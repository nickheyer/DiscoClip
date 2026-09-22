//! Mux Video playback ids, from the HLS master the player reads, expanded to its
//! renditions, with every static MP4 rendition the asset offers found by asking the
//! CDN for its first byte, and the asset's thumbnail. Stream, player and image links
//! name the playback id. A signed playback id carries its token through to the
//! manifests and files.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, clean_title, fetch, hls, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "mux";
const STREAM: &str = "https://stream.mux.com/";
const PLAYER: &str = "https://player.mux.com/";
const IMAGE: &str = "https://image.mux.com/";
/// The static rendition names Mux serves: the newer `highest`, the older three, and one
/// per height the asset was encoded at.
const RENDITIONS: [&str; 4] = ["highest", "high", "medium", "low"];
const HEIGHTS: [u32; 7] = [2160, 1440, 1080, 720, 480, 360, 270];

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{10,}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub id: String,
    /// The signed playback token of a signed playback id.
    pub token: Option<String>,
    /// The title a player link carries in its metadata.
    pub title: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "stream.mux.com" | "player.mux.com" | "image.mux.com"
    ) {
        return None;
    }
    let first = url.path_segments()?.find(|s| !s.is_empty())?;
    let id = first.split('.').next().unwrap_or(first);
    if !RE_ID.is_match(id) {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.trim().is_empty())
    };
    Some(Link {
        id: id.to_string(),
        token: query("token"),
        title: query("metadata-video-title")
            .or_else(|| query("video-title"))
            .and_then(|t| clean_title(&t)),
    })
}

impl Link {
    fn signed(&self, url: &str) -> Url {
        let mut url = Url::parse(url).expect("valid");
        if let Some(token) = &self.token {
            url.query_pairs_mut().append_pair("token", token);
        }
        url
    }

    /// The HLS master.
    pub fn master(&self) -> Url {
        self.signed(&format!("{STREAM}{}.m3u8", self.id))
    }

    /// A static rendition file.
    pub fn rendition(&self, name: &str) -> Url {
        self.signed(&format!("{STREAM}{}/{name}", self.id))
    }

    pub fn player(&self) -> Url {
        let mut url = self.signed(&format!("{PLAYER}{}", self.id));
        if let Some(title) = &self.title {
            url.query_pairs_mut()
                .append_pair("metadata-video-title", title);
        }
        url
    }
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

/// Players a page embeds through the `<mux-player>` and `<mux-video>` elements, as
/// links this resolver takes.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    for element in page
        .document()
        .select(&selector("mux-player[playback-id], mux-video[playback-id]"))
    {
        let Some(id) = element
            .value()
            .attr("playback-id")
            .map(str::trim)
            .filter(|id| RE_ID.is_match(id))
        else {
            continue;
        };
        let link = Link {
            id: id.to_string(),
            token: element
                .value()
                .attr("playback-token")
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from),
            title: element
                .value()
                .attr("metadata-video-title")
                .and_then(clean_title),
        };
        let player = link.player();
        if !found.contains(&player) {
            found.push(player);
        }
    }
    found
}

pub struct MuxResolver {
    http: Http,
}

impl MuxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for MuxResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Mux",
            hosts: &["stream.mux.com", "player.mux.com", "image.mux.com"],
            features: &[
                "playback ids",
                "player embeds",
                "signed playback",
                "static renditions",
                "live",
            ],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://stream.mux.com/DS00Spx1CV902MCtPj5WknGlR102V5HFkDe.m3u8",
                "https://player.mux.com/DS00Spx1CV902MCtPj5WknGlR102V5HFkDe",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let master = link.master();
        let fetched = fetch(&self.http, &master, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            400 | 401 | 403 => {
                let reason = if link.token.is_some() {
                    "the playback token does not sign this playback id"
                } else {
                    "the playback id needs a signed token"
                };
                return Err(ResolveError::unavailable(url, reason));
            }
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the CDN answered HTTP {status}"),
                ));
            }
        }
        let expanded = hls::expand_playlist(
            &self.http,
            &master,
            &fetched.url,
            &fetched.body,
            PLATFORM,
            BROWSER_UA,
            &[],
        )
        .await?;
        let mut names: Vec<String> = RENDITIONS
            .iter()
            .map(|name| format!("{name}.mp4"))
            .collect();
        for height in HEIGHTS {
            if expanded.variants.iter().any(|v| v.height == Some(height)) {
                names.push(format!("{height}p.mp4"));
            }
        }
        names.push("audio.m4a".to_string());
        let probes = futures::future::join_all(names.iter().map(|name| {
            let target = link.rendition(name);
            let http = &self.http;
            async move {
                probe_file(http, &target, PLATFORM, BROWSER_UA, &[])
                    .await
                    .map(|probed| (target, probed))
            }
        }))
        .await;
        let mut variants = expanded.variants;
        for probe in probes {
            let (target, probed) = probe?;
            if !probed.status.is_success() {
                continue;
            }
            let name = target
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or_default()
                .to_string();
            let stem = name.split('.').next().unwrap_or(&name).to_string();
            let audio = name.ends_with(".m4a");
            let mut v = Variant::new(target, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.audio_only = audio;
            v.video = (!audio).then_some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.size = probed.size;
            v.height = stem.strip_suffix('p').and_then(|h| h.parse().ok());
            v.duration = expanded.duration;
            v.live = expanded.live;
            v.label = Some(stem.clone());
            v.format_id = Some(stem);
            variants.push(v);
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(link.id.clone());
        resolved.title = link.title.clone();
        resolved.duration = expanded.duration;
        resolved.live = expanded.live;
        resolved.thumbnail = link
            .token
            .is_none()
            .then(|| Url::parse(&format!("{IMAGE}{}/thumbnail.jpg", link.id)).expect("valid"));
        resolved.webpage_url = Some(link.player());
        resolved.subtitles = expanded.subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("mux_fixture.json")).unwrap()
    }

    const ID: &str = "DS00Spx1CV902MCtPj5WknGlR102V5HFkDe";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let plain = Some(Link {
            id: ID.into(),
            token: None,
            title: None,
        });
        assert_eq!(link(&format!("https://stream.mux.com/{ID}.m3u8")), plain);
        assert_eq!(
            link(&format!("https://stream.mux.com/{ID}/high.mp4")),
            plain
        );
        assert_eq!(link(&format!("https://player.mux.com/{ID}")), plain);
        assert_eq!(
            link(&format!("https://image.mux.com/{ID}/thumbnail.jpg?time=5")),
            plain
        );
        assert_eq!(
            link(&format!(
                "https://player.mux.com/{ID}?metadata-video-title=Big+Buck+Bunny&metadata-viewer-user-id=1"
            )),
            Some(Link {
                id: ID.into(),
                token: None,
                title: Some("Big Buck Bunny".into())
            })
        );
        let signed = link(&format!(
            "https://stream.mux.com/{ID}.m3u8?token=eyJ.abc.def"
        ))
        .unwrap();
        assert_eq!(signed.token.as_deref(), Some("eyJ.abc.def"));
        assert_eq!(
            signed.master().as_str(),
            format!("https://stream.mux.com/{ID}.m3u8?token=eyJ.abc.def")
        );
        assert_eq!(
            signed.rendition("high.mp4").as_str(),
            format!("https://stream.mux.com/{ID}/high.mp4?token=eyJ.abc.def")
        );
        assert_eq!(link("https://stream.mux.com/"), None);
        assert_eq!(link("https://www.mux.com/pricing"), None);
        assert_eq!(
            link("https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"),
            None
        );
    }

    #[test]
    fn embedded_players_are_found_in_pages() {
        let html = format!(
            r#"<html><body>
            <mux-player playback-id="{ID}" metadata-video-title="Demo" stream-type="on-demand"></mux-player>
            <mux-video playback-id="AbCdEfGhIjKlMnOp01" playback-token="tok.en.x" controls></mux-video>
            </body></html>"#
        );
        let page = Page::parse(&html, &Url::parse("https://site.test/page").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            found,
            vec![
                format!("https://player.mux.com/{ID}?metadata-video-title=Demo"),
                "https://player.mux.com/AbCdEfGhIjKlMnOp01?token=tok.en.x".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn playback_ids_resolve_with_renditions_and_static_files() {
        let resolver = MuxResolver::new(Http::replay(fixture()));
        let resolved = resolver
            .resolve(
                &Url::parse(&format!(
                    "https://player.mux.com/{ID}?metadata-video-title=Big%20Buck%20Bunny"
                ))
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(resolved.title.as_deref(), Some("Big Buck Bunny"));
        assert!(
            resolved.duration.unwrap().as_secs() > 60,
            "{:?}",
            resolved.duration
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://image.mux.com/DS00Spx1CV902MCtPj5WknGlR102V5HFkDe/thumbnail.jpg")
        );
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(hls.len(), 5);
        assert!(
            hls.iter()
                .any(|v| v.height == Some(1080) && v.video == Some(VideoCodec::H264))
        );
        let files: Vec<(&str, Option<u64>)> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .map(|v| (v.format_id.as_deref().unwrap(), v.size))
            .collect();
        assert_eq!(
            files,
            vec![
                ("high", Some(72619954)),
                ("medium", Some(24973272)),
                ("low", Some(14107285))
            ]
        );
        assert!(
            resolved
                .variants
                .iter()
                .filter(|v| v.kind == VariantKind::File)
                .all(|v| v.container == Some(Container::Mp4) && !v.audio_only)
        );
        let error = resolver
            .resolve(
                &Url::parse("https://stream.mux.com/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.m3u8")
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }
}
