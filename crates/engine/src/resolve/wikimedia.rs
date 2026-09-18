//! Wikimedia Commons and Wikipedia file pages, through the MediaWiki API's video info: the
//! original upload and every transcode the wiki made of it, with the file's description,
//! author and upload time. A direct link to an upload resolves to the same file page.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, essence, fetch, parse_codecs,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "wikimedia";
const COMMONS: &str = "commons.wikimedia.org";

/// A file page: the wiki whose API describes it, and the file's title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub wiki: String,
    pub title: String,
}

fn wiki_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    if host == COMMONS || host == "commons.m.wikimedia.org" {
        return Some(COMMONS.to_string());
    }
    if host == "upload.wikimedia.org" {
        return Some(COMMONS.to_string());
    }
    let rest = host.strip_suffix(".wikipedia.org")?;
    let language = rest.strip_suffix(".m").unwrap_or(rest);
    (!language.is_empty() && !language.contains('.')).then(|| format!("{language}.wikipedia.org"))
}

fn decode(segment: &str) -> String {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

fn file_title(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let bare = name
        .strip_prefix("File:")
        .or_else(|| name.strip_prefix("Image:"))
        .unwrap_or(name);
    Some(format!("File:{}", bare.replace('_', " ")))
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let wiki = wiki_host(&host)?;
    let decoded: Vec<String> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(decode)
        .collect();
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();
    if host == "upload.wikimedia.org" {
        // /wikipedia/commons/c/c0/Name.webm and
        // /wikipedia/commons/transcoded/c/c0/Name.webm/Name.webm.480p.vp9.webm
        let name = match segments.as_slice() {
            [_, _, "transcoded", _, _, name, ..] => name,
            [_, _, _, _, name] => name,
            _ => return None,
        };
        if !name.contains('.') {
            return None;
        }
        return Some(Link {
            wiki,
            title: file_title(name)?,
        });
    }
    match segments.as_slice() {
        ["wiki", title] => Some(Link {
            wiki,
            title: file_title(title)
                .filter(|_| title.starts_with("File:") || title.starts_with("Image:"))?,
        }),
        ["w", "index.php"] => {
            let title = url
                .query_pairs()
                .find(|(k, _)| k == "title")?
                .1
                .into_owned();
            (title.starts_with("File:") || title.starts_with("Image:")).then(|| Link {
                wiki,
                title: file_title(&title).expect("non-empty"),
            })
        }
        _ => None,
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn codecs_of(mime_type: &str) -> (Option<VideoCodec>, Option<AudioCodec>) {
    let codecs = mime_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("codecs="))
        .map(|c| c.trim_matches('"'));
    let (mut video, mut audio) = parse_codecs(codecs);
    if let Some(codecs) = codecs {
        let lower = codecs.to_ascii_lowercase();
        if video.is_none() && lower.contains("theora") {
            video = Some(VideoCodec::Other("theora".into()));
        }
        if video.is_none() && lower.contains("mp4v") {
            video = Some(VideoCodec::Other("mpeg4".into()));
        }
        if audio.is_none() && lower.contains("vorbis") {
            audio = Some(AudioCodec::Vorbis);
        }
    }
    (video, audio)
}

/// The variants a file's video info lists: the original upload and every transcode.
pub fn variants_of(info: &Value) -> Vec<Variant> {
    let duration = info["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let original_url = info["url"].as_str().and_then(|u| Url::parse(u).ok());
    let mut variants = Vec::new();
    for derivative in info["derivatives"].as_array().into_iter().flatten() {
        let Some(url) = derivative["src"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mime_type = derivative["type"].as_str().unwrap_or_default();
        let mime = essence(Some(mime_type));
        if mime.starts_with("audio/") {
            continue;
        }
        let mut v = Variant::new(url.clone(), VariantKind::File);
        v.container = Container::from_mime(&mime).or_else(|| match mime.as_str() {
            "video/ogg" => Some(Container::Other("ogv".into())),
            _ => None,
        });
        let (video, audio) = codecs_of(mime_type);
        v.video = video;
        v.audio = audio;
        v.width = derivative["width"].as_u64().map(|w| w as u32);
        v.height = derivative["height"].as_u64().map(|h| h as u32);
        v.bitrate = derivative["bandwidth"].as_u64().filter(|b| *b > 0);
        v.duration = duration;
        let key = derivative["transcodekey"].as_str();
        v.format_id = Some(key.unwrap_or("original").to_string());
        v.label = Some(match (key, v.height) {
            (None, _) => "original".to_string(),
            (Some(_), Some(h)) => format!("{h}p"),
            (Some(key), None) => key.to_string(),
        });
        if key.is_none()
            || original_url
                .as_ref()
                .is_some_and(|o| o.path() == url.path())
        {
            v.size = info["size"].as_u64();
        }
        variants.push(v);
    }
    if variants.is_empty()
        && let Some(url) = original_url
    {
        let mime_type = info["mime"].as_str().unwrap_or_default();
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Container::from_mime(mime_type);
        v.width = info["width"].as_u64().map(|w| w as u32);
        v.height = info["height"].as_u64().map(|h| h as u32);
        v.size = info["size"].as_u64();
        v.duration = duration;
        v.label = Some("original".into());
        variants.push(v);
    }
    variants
}

pub struct WikimediaResolver {
    http: Http,
}

impl WikimediaResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for WikimediaResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Wikimedia Commons",
            hosts: &[
                "commons.wikimedia.org",
                "wikipedia.org",
                "upload.wikimedia.org",
            ],
            features: &[
                "file pages",
                "wikipedia file pages",
                "direct uploads",
                "transcodes",
            ],
            formats: &["webm", "mp4", "mov", "ogv"],
            session: SessionSupport::None,
            examples: &[
                "https://commons.wikimedia.org/wiki/File:Big_Buck_Bunny_4K.webm",
                "https://upload.wikimedia.org/wikipedia/commons/c/c0/Big_Buck_Bunny_4K.webm",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut api = Url::parse(&format!("https://{}/w/api.php", link.wiki)).expect("valid");
        api.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("titles", &link.title)
            .append_pair("prop", "videoinfo")
            .append_pair(
                "viprop",
                "url|size|mime|derivatives|extmetadata|user|timestamp|canonicaltitle",
            )
            .append_pair("viurlwidth", "640")
            .append_pair("format", "json")
            .append_pair("formatversion", "2");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the wiki's API answered HTTP {status}"),
                ));
            }
        }
        let answer = fetched.json(url)?;
        let page = answer["query"]["pages"]
            .as_array()
            .and_then(|pages| pages.first())
            .ok_or_else(|| ResolveError::malformed(url, "the API listed no page"))?;
        if page["missing"].as_bool() == Some(true) || page["invalid"].as_bool() == Some(true) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let info = page["videoinfo"]
            .as_array()
            .and_then(|infos| infos.first())
            .ok_or_else(|| ResolveError::malformed(url, "the page carries no file info"))?;
        let mime = essence(info["mime"].as_str());
        if !mime.starts_with("video/") && !mime.starts_with("application/ogg") {
            return Err(ResolveError::unavailable(
                url,
                format!(
                    "the file is {}, not a video",
                    if mime.is_empty() {
                        "of an unknown type"
                    } else {
                        mime.as_str()
                    }
                ),
            ));
        }
        let variants = variants_of(info);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let meta = &info["extmetadata"];
        let title_of = |key: &str| {
            meta[key]["value"]
                .as_str()
                .map(strip_tags)
                .and_then(|t| clean_title(&t))
        };
        let canonical = info["canonicaltitle"]
            .as_str()
            .or(page["title"].as_str())
            .unwrap_or(&link.title);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = page["pageid"].as_u64().map(|id| id.to_string());
        resolved.title = title_of("ObjectName").or_else(|| {
            let bare = canonical.strip_prefix("File:").unwrap_or(canonical);
            clean_title(bare.rsplit_once('.').map_or(bare, |(s, _)| s))
        });
        resolved.description = title_of("ImageDescription");
        resolved.uploader =
            title_of("Artist").or_else(|| info["user"].as_str().and_then(clean_title));
        resolved.uploaded_at = info["timestamp"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = info["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.thumbnail = info["thumburl"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = info["descriptionurl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| Some(url.clone()));
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const API: &str = "https://commons.wikimedia.org/w/api.php";
    const INFO: &str = r#"{"batchcomplete":true,"query":{"normalized":[{"from":"File:Big_Buck_Bunny_4K.webm","to":"File:Big Buck Bunny 4K.webm"}],"pages":[{"pageid":40108642,"ns":6,"title":"File:Big Buck Bunny 4K.webm","imagerepository":"local","videoinfo":[{"timestamp":"2015-05-10T18:23:36Z","user":"Matanya","size":2964839055,"width":4000,"height":2250,"duration":634.553,"canonicaltitle":"File:Big Buck Bunny 4K.webm","thumburl":"https://upload.wikimedia.org/wikipedia/commons/thumb/c/c0/Big_Buck_Bunny_4K.webm/640px--Big_Buck_Bunny_4K.webm.jpg","url":"https://upload.wikimedia.org/wikipedia/commons/c/c0/Big_Buck_Bunny_4K.webm","descriptionurl":"https://commons.wikimedia.org/wiki/File:Big_Buck_Bunny_4K.webm","extmetadata":{"ObjectName":{"value":"Big Buck Bunny 4K"},"ImageDescription":{"value":"<i><a href=\"//commons.wikimedia.org/wiki/Big_Buck_Bunny\">Big Buck Bunny</a></i> short film by the Blender Foundation."},"Artist":{"value":"Blender Foundation"}},"mime":"video/webm","derivatives":[{"src":"https://upload.wikimedia.org/wikipedia/commons/c/c0/Big_Buck_Bunny_4K.webm","type":"video/webm; codecs=\"vp8, vorbis\"","width":4000,"height":2250,"bandwidth":37378615},{"src":"https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.240p.vp9.webm","type":"video/webm; codecs=\"vp9, opus\"","transcodekey":"240p.vp9.webm","width":426,"height":240,"bandwidth":383864},{"src":"https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.360p.mpeg4.mov","type":"video/quicktime; codecs=\"mp4v\"","transcodekey":"360p.mpeg4.mov","width":640,"height":360,"bandwidth":1636936},{"src":"https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.1080p.vp9.webm","type":"video/webm; codecs=\"vp9, opus\"","transcodekey":"1080p.vp9.webm","width":1920,"height":1080,"bandwidth":3788064}]}]}]}}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://commons.wikimedia.org/wiki/File:Big_Buck_Bunny_4K.webm"),
            Some(Link {
                wiki: COMMONS.into(),
                title: "File:Big Buck Bunny 4K.webm".into()
            })
        );
        assert_eq!(
            link("https://en.wikipedia.org/wiki/File:Some_video.ogv")
                .unwrap()
                .wiki,
            "en.wikipedia.org"
        );
        assert_eq!(
            link("https://en.m.wikipedia.org/w/index.php?title=File:Some_video.ogv&x=1")
                .unwrap()
                .title,
            "File:Some video.ogv"
        );
        assert_eq!(
            link("https://upload.wikimedia.org/wikipedia/commons/transcoded/c/c0/Big_Buck_Bunny_4K.webm/Big_Buck_Bunny_4K.webm.480p.vp9.webm").unwrap().title,
            "File:Big Buck Bunny 4K.webm"
        );
        assert_eq!(link("https://commons.wikimedia.org/wiki/Main_Page"), None);
        assert_eq!(link("https://en.wikipedia.org/wiki/Video"), None);
    }

    #[tokio::test]
    async fn file_pages_resolve_with_their_transcodes() {
        let mut fixture = Fixture::new("wikimedia", None);
        fixture.exchanges.push(get(API, INFO));
        let resolver = WikimediaResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://commons.wikimedia.org/wiki/File:Big_Buck_Bunny_4K.webm").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Big Buck Bunny 4K"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Big Buck Bunny short film by the Blender Foundation.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Blender Foundation"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(634.553)));
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.variants.len(), 4);
        let original = &resolved.variants[0];
        assert_eq!(original.label.as_deref(), Some("original"));
        assert_eq!(original.video, Some(VideoCodec::Vp8));
        assert_eq!(original.audio, Some(AudioCodec::Vorbis));
        assert_eq!(original.size, Some(2964839055));
        assert_eq!(original.height, Some(2250));
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(1080))
            .unwrap();
        assert_eq!(hd.video, Some(VideoCodec::Vp9));
        assert_eq!(hd.audio, Some(AudioCodec::Opus));
        assert_eq!(hd.bitrate, Some(3788064));
        assert_eq!(hd.container, Some(Container::Webm));
        let mov = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(360))
            .unwrap();
        assert_eq!(mov.container, Some(Container::Mov));
        assert_eq!(mov.video, Some(VideoCodec::Other("mpeg4".into())));
    }

    #[tokio::test]
    async fn missing_pages_and_still_images_say_so() {
        let mut fixture = Fixture::new("wikimedia", None);
        fixture.exchanges.push(get(
            API,
            r#"{"query":{"pages":[{"ns":6,"title":"File:Nope.webm","missing":true}]}}"#,
        ));
        fixture.exchanges.push(get(
            API,
            r#"{"query":{"pages":[{"pageid":1,"ns":6,"title":"File:Cat.jpg","videoinfo":[{"mime":"image/jpeg","url":"https://upload.wikimedia.org/x/Cat.jpg","size":5}]}]}}"#,
        ));
        let resolver = WikimediaResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://commons.wikimedia.org/wiki/File:Nope.webm").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://commons.wikimedia.org/wiki/File:Cat.jpg").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not a video")),
            "{error}"
        );
    }
}
