//! GIPHY GIFs, stickers and clips, from the data the page renders: a GIF's MP4 renditions,
//! a clip's MP4s by height with its sound, and direct media links by their id.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::{Page, leading_json};
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, essence, fetch, navigation_headers, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "giphy";
const SITE: &str = "https://giphy.com/";
const MEDIA: &str = "https://media.giphy.com/media/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{8,32}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A GIF or sticker page, by id.
    Gif(String),
    /// A clip, a video with sound, by id.
    Clip(String),
    /// A `gph.is` short link.
    Short(Url),
}

/// The id at the end of a `some-words-ID` slug.
fn id_of(slug: &str) -> Option<String> {
    let id = slug.rsplit('-').next().unwrap_or(slug);
    RE_ID.is_match(id).then(|| id.to_string())
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if host == "gph.is" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    if host == "giphy.com" || host == "www.giphy.com" {
        return match segments.as_slice() {
            ["gifs" | "stickers", slug, ..] => id_of(slug).map(Link::Gif),
            ["clips", slug, ..] => id_of(slug).map(Link::Clip),
            ["embed", id, ..] => id_of(id).map(Link::Gif),
            _ => None,
        };
    }
    if host == "i.giphy.com" {
        // /ID.mp4, /ID.gif, /media/ID/giphy.mp4
        return match segments.as_slice() {
            ["media", id, ..] => id_of(id).map(Link::Gif),
            [name] => id_of(name.rsplit_once('.').map_or(name, |(s, _)| s)).map(Link::Gif),
            _ => None,
        };
    }
    if host == "media.giphy.com" || (host.starts_with("media") && host.ends_with(".giphy.com")) {
        // /media/ID/giphy.mp4 and /media/v1.TOKEN/ID/giphy.mp4
        return match segments.as_slice() {
            ["media", token, id, _] if token.starts_with("v1.") => id_of(id).map(Link::Gif),
            ["media", id, ..] => id_of(id).map(Link::Gif),
            _ => None,
        };
    }
    None
}

/// The GIF's record as the page inlines it, escaped inside the app's data.
pub fn gif_json_in(html: &str) -> Option<Value> {
    let unescaped = html.replace("\\\"", "\"");
    let at = unescaped.find("\"gif\":{")?;
    leading_json(&unescaped[at + "\"gif\":".len()..]).map(|(v, _)| v)
}

fn parse_date(text: &str) -> Option<Timestamp> {
    DateTime::strptime("%Y-%m-%d %H:%M:%S", text.trim())
        .ok()
        .and_then(|dt| dt.to_zoned(TimeZone::UTC).ok())
        .map(|z| z.timestamp())
        .filter(|t| t.as_second() > 0)
}

fn dimension(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .map(|n| n as u32)
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| *n > 0)
}

/// The MP4 renditions the record names: a clip's by height, a GIF's original and HD ones.
pub fn variants_of(gif: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    let duration = gif["video"]["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    if let Some(assets) = gif["video"]["assets"].as_object() {
        for (key, asset) in assets {
            let Some(url) = asset["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let mut v = Variant::new(url.clone(), VariantKind::File);
            let extension = url
                .path()
                .rsplit('.')
                .next()
                .unwrap_or("mp4")
                .to_ascii_lowercase();
            v.container = Container::from_extension(&extension).or(Some(Container::Mp4));
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.width = dimension(&asset["width"]);
            v.height = dimension(&asset["height"]);
            v.duration = duration;
            v.format_id = Some(key.clone());
            v.label = Some(key.clone());
            variants.push(v);
        }
    }
    if !variants.is_empty() {
        return variants;
    }
    for (key, image) in [
        ("original", &gif["images"]["original"]),
        ("hd", &gif["images"]["hd"]),
    ] {
        let Some(url) = image["mp4"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.width = dimension(&image["width"]);
        v.height = dimension(&image["height"]);
        v.size = image["mp4_size"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| image["mp4_size"].as_u64());
        v.format_id = Some(key.to_string());
        v.label = Some(key.to_string());
        variants.push(v);
    }
    if variants.is_empty()
        && let Some(url) = gif["images"]["original"]["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Gif);
        v.width = dimension(&gif["images"]["original"]["width"]);
        v.height = dimension(&gif["images"]["original"]["height"]);
        v.format_id = Some("gif".into());
        variants.push(v);
    }
    variants
}

pub struct GiphyResolver {
    http: Http,
}

impl GiphyResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The GIF's page, whose data names every rendition; the media file itself when the
    /// page has none, as an embed of a removed GIF still serves.
    async fn page_media(
        &self,
        id: &str,
        clip: bool,
        origin: &Url,
    ) -> Result<Resolved, ResolveError> {
        let page_url = Url::parse(&format!(
            "{SITE}{}/{id}",
            if clip { "clips" } else { "gifs" }
        ))
        .expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.webpage_url = Some(fetched.url.clone());
        if let Some(gif) = gif_json_in(&html).filter(|g| g["id"].as_str() == Some(id)) {
            resolved.title = gif["title"].as_str().and_then(clean_title);
            resolved.description = gif["video"]["description"]
                .as_str()
                .or(gif["alt_text"].as_str())
                .and_then(clean_title);
            resolved.uploader = gif["user"]["display_name"]
                .as_str()
                .or(gif["username"].as_str())
                .and_then(clean_title);
            resolved.uploader_url = gif["user"]["profile_url"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
                .or_else(|| {
                    gif["username"]
                        .as_str()
                        .filter(|u| !u.is_empty())
                        .and_then(|u| Url::parse(&format!("{SITE}{u}")).ok())
                });
            resolved.uploaded_at = gif["import_datetime"]
                .as_str()
                .and_then(parse_date)
                .or_else(|| gif["create_datetime"].as_str().and_then(parse_date));
            resolved.duration = gif["video"]["duration"]
                .as_f64()
                .filter(|d| *d > 0.0)
                .map(Duration::from_secs_f64);
            resolved.thumbnail = gif["images"]["original"]["url"]
                .as_str()
                .or(gif["images"]["480w_still"]["url"].as_str())
                .and_then(|u| Url::parse(u).ok());
            resolved.age_limit = match gif["rating"].as_str() {
                Some("r") => Some(17),
                Some("pg-13") => Some(13),
                _ => None,
            };
            resolved.variants = variants_of(&gif);
            if !resolved.variants.is_empty() {
                return Ok(resolved);
            }
        }
        let (title, thumbnail) = {
            let page = Page::parse(&html, &fetched.url);
            (
                page.title()
                    .map(|t| {
                        t.trim_end_matches(" - Find & Share on GIPHY")
                            .trim()
                            .to_string()
                    })
                    .and_then(|t| clean_title(&t)),
                page.meta("og:image").and_then(|u| Url::parse(&u).ok()),
            )
        };
        resolved.title = title;
        resolved.thumbnail = thumbnail;
        resolved.variants = vec![self.media_variant(id, origin).await?];
        Ok(resolved)
    }

    /// The MP4 the CDN serves for an id.
    async fn media_variant(&self, id: &str, origin: &Url) -> Result<Variant, ResolveError> {
        let url = Url::parse(&format!("{MEDIA}{id}/giphy.mp4")).expect("valid");
        let probed = probe_file(&self.http, &url, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 | 403 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the media host answered HTTP {status}"),
                ));
            }
        }
        if !essence(probed.content_type.as_deref()).starts_with("video/") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.size = probed.size;
        v.format_id = Some("original".into());
        Ok(v)
    }
}

#[async_trait]
impl Resolver for GiphyResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "GIPHY",
            hosts: &["giphy.com", "media.giphy.com", "i.giphy.com", "gph.is"],
            features: &[
                "gifs",
                "stickers",
                "clips",
                "embeds",
                "media links",
                "short links",
            ],
            formats: &["mp4", "mov", "gif"],
            session: SessionSupport::None,
            examples: &[
                "https://giphy.com/gifs/l0ExbnGIX9sMFS7PG",
                "https://giphy.com/clips/GHuZnOveABj2uSWgd0",
                "https://media.giphy.com/media/l0ExbnGIX9sMFS7PG/giphy.mp4",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Gif(id) => Ok(Resolution::from(self.page_media(&id, false, url).await?)),
            Link::Clip(id) => Ok(Resolution::from(self.page_media(&id, true, url).await?)),
            Link::Short(short) => {
                let landed = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                if landed == short || parse_link(&landed).is_none() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Err(ResolveError::Redirect(landed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(
        url: &str,
        status: u16,
        content_type: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
        let mut all = vec![("content-type".to_string(), content_type.to_string())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    /// The page's data as the app escapes it inside a script string.
    fn page_with(gif: &str) -> String {
        let escaped = gif.replace('"', "\\\"");
        format!(
            r#"<html><head><title>Salutes Jack Black GIF - Find &amp; Share on GIPHY</title><meta property="og:image" content="https://media4.giphy.com/media/x/giphy.gif"/></head><body><script>self.__next_f.push([1,"[\"$\",\"$L29\",null,{{\"gif\":{escaped},\"other\":1}}]"])</script></body></html>"#
        )
    }

    const GIF: &str = r#"{"type":"gif","id":"l0ExbnGIX9sMFS7PG","url":"https://giphy.com/gifs/l0ExbnGIX9sMFS7PG","slug":"l0ExbnGIX9sMFS7PG","username":"","title":"Salutes Jack Black GIF","rating":"g","is_sticker":false,"import_datetime":"2017-01-25 23:39:02","create_datetime":"2017-01-25 23:39:09","images":{"original":{"height":264,"width":480,"size":"661302","url":"https://media4.giphy.com/media/v1.T/l0ExbnGIX9sMFS7PG/giphy.gif","mp4_size":"150315","mp4":"https://media4.giphy.com/media/v1.T/l0ExbnGIX9sMFS7PG/giphy.mp4","webp":"https://media4.giphy.com/media/v1.T/l0ExbnGIX9sMFS7PG/giphy.webp","frames":"23"},"downsized":{"height":264,"width":480,"size":"661302","url":"https://media4.giphy.com/media/v1.T/l0ExbnGIX9sMFS7PG/giphy.gif"}}}"#;
    const CLIP: &str = r#"{"type":"video","id":"GHuZnOveABj2uSWgd0","url":"https://giphy.com/clips/GHuZnOveABj2uSWgd0","username":"amandabonaiuto","title":"Over Crowded","rating":"g","import_datetime":"2021-03-20 16:30:24","user":{"username":"amandabonaiuto","display_name":"Amanda Bonaiuto","profile_url":"https://giphy.com/amandabonaiuto/"},"images":{"original":{"height":270,"width":480,"size":"2502111","url":"https://media4.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/giphy.gif","mp4":"https://media4.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/giphy.mp4"}},"video":{"description":"Animation: Amanda Bonaiuto\nSound: Daniel Crook","hls_manifest_url":"","dash_manifest_url":"","assets":{"source":{"url":"https://media4.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/source.mov","height":"1080","width":"1920"},"360p":{"url":"https://media0.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/giphy360p.mp4","height":"360","width":"640"},"720p":{"url":"https://media1.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/giphy720p.mp4","height":"720","width":"1280"},"480p":{"url":"https://media4.giphy.com/media/v1.T/GHuZnOveABj2uSWgd0/giphy480p.mp4","height":"480","width":"854"}},"duration":3.1}}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://giphy.com/gifs/kiss-valentines-day-looney-tunes-5GdhgaBpA3oCA"),
            Some(Link::Gif("5GdhgaBpA3oCA".into()))
        );
        assert_eq!(
            link("https://giphy.com/gifs/l0ExbnGIX9sMFS7PG"),
            Some(Link::Gif("l0ExbnGIX9sMFS7PG".into()))
        );
        assert_eq!(
            link("https://giphy.com/clips/NovaSound-nova-sound-LG5EtlRwv46uESZNRl"),
            Some(Link::Clip("LG5EtlRwv46uESZNRl".into()))
        );
        assert_eq!(
            link("https://giphy.com/embed/l0ExbnGIX9sMFS7PG"),
            Some(Link::Gif("l0ExbnGIX9sMFS7PG".into()))
        );
        assert_eq!(
            link("https://media.giphy.com/media/l0ExbnGIX9sMFS7PG/giphy.mp4"),
            Some(Link::Gif("l0ExbnGIX9sMFS7PG".into()))
        );
        assert_eq!(
            link("https://media4.giphy.com/media/v1.Y2lkPTc5/l0ExbnGIX9sMFS7PG/giphy.gif"),
            Some(Link::Gif("l0ExbnGIX9sMFS7PG".into()))
        );
        assert_eq!(
            link("https://i.giphy.com/l0ExbnGIX9sMFS7PG.mp4"),
            Some(Link::Gif("l0ExbnGIX9sMFS7PG".into()))
        );
        assert!(matches!(
            link("https://gph.is/2k5cNmO"),
            Some(Link::Short(_))
        ));
        assert_eq!(link("https://giphy.com/explore/cats"), None);
        assert_eq!(link("https://giphy.com/"), None);
    }

    #[tokio::test]
    async fn gifs_resolve_to_their_mp4_renditions() {
        let mut fixture = Fixture::new("giphy", None);
        fixture.exchanges.push(get(
            "https://giphy.com/gifs/l0ExbnGIX9sMFS7PG",
            200,
            "text/html",
            &page_with(GIF),
            &[],
        ));
        let resolver = GiphyResolver::new(Http::replay(fixture));
        let url = Url::parse("https://media.giphy.com/media/l0ExbnGIX9sMFS7PG/giphy.mp4").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Salutes Jack Black GIF"));
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2017-01-25T23:39:02Z"
        );
        assert_eq!(resolved.variants.len(), 1);
        let original = &resolved.variants[0];
        assert_eq!(
            original.url.as_str(),
            "https://media4.giphy.com/media/v1.T/l0ExbnGIX9sMFS7PG/giphy.mp4"
        );
        assert_eq!(original.size, Some(150315));
        assert_eq!((original.width, original.height), (Some(480), Some(264)));
        assert!(original.audio.is_none());
    }

    #[tokio::test]
    async fn clips_carry_sound_in_several_sizes() {
        let mut fixture = Fixture::new("giphy", None);
        fixture.exchanges.push(get(
            "https://giphy.com/clips/GHuZnOveABj2uSWgd0",
            200,
            "text/html",
            &page_with(CLIP),
            &[],
        ));
        let resolver = GiphyResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://giphy.com/clips/some-slug-GHuZnOveABj2uSWgd0").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Over Crowded"));
        assert_eq!(resolved.uploader.as_deref(), Some("Amanda Bonaiuto"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(3.1)));
        assert_eq!(resolved.variants.len(), 4);
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(720))
            .unwrap();
        assert_eq!(hd.audio, Some(AudioCodec::Aac));
        assert_eq!(hd.container, Some(Container::Mp4));
        let source = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("source"))
            .unwrap();
        assert_eq!(source.container, Some(Container::Mov));
        assert_eq!(source.height, Some(1080));
    }

    #[tokio::test]
    async fn a_page_without_data_falls_back_to_the_media_file_and_short_links_unwrap() {
        let mut fixture = Fixture::new("giphy", None);
        fixture.exchanges.push(get(
            "https://giphy.com/gifs/abcdefgh1234",
            200,
            "text/html",
            "<html><head><title>Cat GIF - Find &amp; Share on GIPHY</title></head></html>",
            &[],
        ));
        fixture.exchanges.push(get(
            "https://media.giphy.com/media/abcdefgh1234/giphy.mp4",
            206,
            "video/mp4",
            "",
            &[("content-range", "bytes 0-0/4242")],
        ));
        fixture.exchanges.push(get(
            "https://gph.is/2k5cNmO",
            301,
            "text/html",
            "",
            &[("location", "https://giphy.com/gifs/l0ExbnGIX9sMFS7PG")],
        ));
        fixture.exchanges.push(get(
            "https://giphy.com/gifs/l0ExbnGIX9sMFS7PG",
            200,
            "text/html",
            "",
            &[],
        ));
        fixture.exchanges.push(get(
            "https://giphy.com/gifs/gone12345678",
            404,
            "text/html",
            "",
            &[],
        ));
        let resolver = GiphyResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://giphy.com/gifs/abcdefgh1234").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Cat GIF"));
        assert_eq!(resolved.variants[0].size, Some(4242));
        let error = resolver
            .resolve(&Url::parse("https://gph.is/2k5cNmO").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://giphy.com/gifs/l0ExbnGIX9sMFS7PG"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://giphy.com/gifs/gone12345678").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
