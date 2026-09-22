//! Resolve Kaltura entries through a widget session. Include ready file renditions, HLS,
//! captions and metadata.
//!
//! Support entry and reference IDs. Report Widevine-protected variants as DRM.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind, clean_title,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "kaltura";
const API: &str = "https://cdnapisec.kaltura.com/api_v3/service/multirequest";
const CAPTION_SERVE: &str = "https://cdnapisec.kaltura.com/api_v3/service/caption_captionasset/action/serve/captionAssetId/";
const ENTRY_FIELDS: &str = "id,name,description,createdAt,dataUrl,duration,msDuration,thumbnailUrl,userId,referenceId,mediaType,type";

static RE_PARTNER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+$").unwrap());
static RE_ENTRY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d_[a-z0-9]{8}$").unwrap());
static RE_FLVCLIPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/flvclipper/.*").unwrap());
static RE_KWIDGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)kWidget\.(?:thumb)?[Ee]mbed\(\s*\{(.*?)\}\s*\)").unwrap());
static RE_WID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bwid\W+_?(\d+)").unwrap());
static RE_ENTRY_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"entry_?[iI]d\W{1,4}(\d_[a-z0-9]{8})").unwrap());
static RE_KALTURA_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:https?:)?//(?:[\w-]+\.)*kaltura\.com(?::\d+)?/[^\s"'<>\\]+"#).unwrap()
});

/// How a link names the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Id(String),
    Reference(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub partner: String,
    pub entry: Entry,
}

/// The play manifest link of an entry, which every player of the partner resolves.
pub fn manifest_url(partner: &str, entry: &str) -> Url {
    Url::parse(&format!(
        "https://cdnapisec.kaltura.com/p/{partner}/sp/{partner}00/playManifest/entryId/{entry}/format/url/protocol/https"
    ))
    .expect("valid")
}

/// The partner, entry id and reference id a kaltura.com link carries, in its path
/// variables (`wid/_P`, `p/P`, `partner_id/P`, `entry_id/E`) and its query.
fn parts_of(url: &Url) -> Option<(Option<String>, Option<String>, Option<String>)> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "kaltura.com" || host.ends_with(".kaltura.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let (mut wid, mut partner_id, mut p, mut entry, mut reference) = (None, None, None, None, None);
    for pair in segments.windows(2) {
        let value = pair[1].to_string();
        match pair[0] {
            "wid" => wid = Some(value.trim_start_matches('_').to_string()),
            "partner_id" => partner_id = Some(value),
            "p" => p = Some(value),
            "entry_id" | "entryId" => entry = Some(value),
            _ => {}
        }
    }
    for (key, value) in url.query_pairs() {
        let value = value.into_owned();
        match key.as_ref() {
            "wid" => wid = Some(value.trim_start_matches('_').to_string()),
            "partner_id" => partner_id = Some(value),
            "p" => p = Some(value),
            "entry_id" | "entryId" | "flashvars[entry_id]" | "flashvars[entryId]" => {
                entry = Some(value)
            }
            "flashvars[referenceId]" | "referenceId" => reference = Some(value),
            _ => {}
        }
    }
    let partner = wid
        .or(partner_id)
        .or(p)
        .filter(|partner| RE_PARTNER.is_match(partner));
    Some((
        partner,
        entry.filter(|e| RE_ENTRY.is_match(e)),
        reference.filter(|r| !r.trim().is_empty()),
    ))
}

pub fn parse_link(url: &Url) -> Option<Link> {
    let (partner, entry, reference) = parts_of(url)?;
    let partner = partner?;
    let entry = match (entry, reference) {
        (Some(id), _) => Entry::Id(id),
        (None, Some(reference)) => Entry::Reference(reference),
        (None, None) => return None,
    };
    Some(Link { partner, entry })
}

/// Players a page embeds through `kWidget.embed`, a player script or an iframe of
/// kaltura.com, as manifest links this resolver takes.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let html = page.html();
    let mut found: Vec<Url> = Vec::new();
    let mut push = |partner: &str, entry: &str| {
        let url = manifest_url(partner, entry);
        if !found.contains(&url) {
            found.push(url);
        }
    };
    for captures in RE_KWIDGET.captures_iter(html) {
        let block = &captures[1];
        if let (Some(wid), Some(entry)) = (RE_WID.captures(block), RE_ENTRY_KEY.captures(block)) {
            push(&wid[1], &entry[1]);
        }
    }
    for found_url in RE_KALTURA_URL.find_iter(html) {
        let raw = found_url.as_str();
        let full = if raw.starts_with("//") {
            format!("https:{raw}")
        } else {
            raw.to_string()
        };
        let Ok(url) = Url::parse(&full) else {
            continue;
        };
        let Some((Some(partner), entry, _)) = parts_of(&url) else {
            continue;
        };
        let entry = entry.or_else(|| {
            RE_ENTRY_KEY
                .captures(&html[found_url.end()..])
                .map(|c| c[1].to_string())
        });
        if let Some(entry) = entry {
            push(&partner, &entry);
        }
    }
    found
}

/// The API's exception in a multirequest answer item: its code and message.
fn exception(item: &Value) -> Option<(String, String)> {
    (item["objectType"].as_str() == Some("KalturaAPIException")).then(|| {
        (
            item["code"].as_str().unwrap_or_default().to_string(),
            item["message"].as_str().unwrap_or_default().to_string(),
        )
    })
}

fn exception_error(code: &str, message: &str, origin: &Url) -> ResolveError {
    match code {
        "ENTRY_ID_NOT_FOUND" | "INVALID_ENTRY_ID" => ResolveError::NotFound(origin.clone()),
        _ => ResolveError::unavailable(origin, format!("Kaltura said: {message} ({code})")),
    }
}

fn session(partner: &str) -> Value {
    json!({
        "expiry": 86400,
        "service": "session",
        "action": "startWidgetSession",
        "widgetId": format!("_{partner}"),
    })
}

fn list_entries(filter: Value, fields: &str) -> Value {
    json!({
        "action": "list",
        "filter": filter,
        "service": "baseentry",
        "ks": "{1:result:ks}",
        "responseProfile": {"type": 1, "fields": fields},
    })
}

fn video_codec(name: &str) -> Option<VideoCodec> {
    match name.to_ascii_lowercase().as_str() {
        "avc1" | "h264" | "avc" => Some(VideoCodec::H264),
        "hvc1" | "hev1" | "hevc" | "h265" => Some(VideoCodec::H265),
        "v_vp8" | "vp8" => Some(VideoCodec::Vp8),
        "v_vp9" | "vp9" => Some(VideoCodec::Vp9),
        "av01" | "av1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

fn duration_of(entry: &Value) -> Option<Duration> {
    entry["msDuration"]
        .as_u64()
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .or_else(|| {
            entry["duration"]
                .as_f64()
                .filter(|s| *s > 0.0)
                .map(Duration::from_secs_f64)
        })
}

/// A live stream entry: the live type, or a live media type.
fn is_live(entry: &Value) -> bool {
    entry["type"].as_i64() == Some(7) || matches!(entry["mediaType"].as_i64(), Some(201..=204))
}

/// The entry's ready flavors as files, each behind the widget session, and its HLS
/// manifest.
pub fn variants_of(entry: &Value, flavors: &[Value], ks: &str) -> Vec<Variant> {
    let duration = duration_of(entry);
    let live = is_live(entry);
    let Some(data_url) = entry["dataUrl"].as_str() else {
        return Vec::new();
    };
    let data_url = RE_FLVCLIPPER.replace(data_url, "/serveFlavor").into_owned();
    let mut variants = Vec::new();
    for flavor in flavors {
        if flavor["status"].as_i64() != Some(2) {
            continue;
        }
        let Some(id) = flavor["id"].as_str() else {
            continue;
        };
        let container_name = flavor["containerFormat"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut ext = flavor["fileExt"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext == "chun" {
            continue;
        }
        if ext.is_empty() {
            ext = if container_name == "qt" { "mov" } else { "mp4" }.to_string();
        }
        let Ok(file_url) = Url::parse(&format!("{data_url}/flavorId/{id}/ks/{ks}")) else {
            continue;
        };
        let mut v = Variant::new(file_url, VariantKind::File);
        v.container =
            Container::from_extension(&ext).or_else(|| Some(Container::Other(ext.clone())));
        let codec = flavor["videoCodecId"].as_str().filter(|c| !c.is_empty());
        let frame_rate = flavor["frameRate"].as_f64().unwrap_or(0.0);
        let audio_only =
            (codec.is_none() && frame_rate == 0.0) || entry["mediaType"].as_i64() == Some(5);
        v.audio_only = audio_only;
        if !audio_only {
            v.video = codec.and_then(video_codec);
            v.width = flavor["width"]
                .as_u64()
                .filter(|w| *w > 0)
                .map(|w| w as u32);
            v.height = flavor["height"]
                .as_u64()
                .filter(|h| *h > 0)
                .map(|h| h as u32);
        }
        v.audio = match v.container {
            Some(Container::Mp4) => Some(AudioCodec::Aac),
            Some(Container::Webm) => Some(AudioCodec::Vorbis),
            _ => None,
        };
        v.bitrate = flavor["bitrate"]
            .as_u64()
            .filter(|b| *b > 0)
            .map(|kbps| kbps * 1000);
        v.size = flavor["size"]
            .as_u64()
            .filter(|s| *s > 0)
            .map(|kb| kb * 1024);
        v.fps = Some(frame_rate).filter(|f| *f > 0.0);
        v.duration = duration;
        v.live = live;
        v.label = v.height.map(|h| format!("{h}p"));
        v.format_id = Some(format!(
            "{}-{ext}",
            flavor["flavorParamsId"].as_u64().unwrap_or(0)
        ));
        if ext == "wvm" {
            v.drm = Some("widevine".into());
        }
        variants.push(v);
    }
    if data_url.contains("/playManifest/") {
        let manifest = data_url.replace("format/url", "format/applehttp");
        if let Ok(manifest_url) = Url::parse(&format!("{manifest}/ks/{ks}")) {
            let mut v = Variant::new(manifest_url, VariantKind::Hls);
            v.duration = duration;
            v.live = live;
            v.format_id = Some("hls".into());
            variants.push(v);
        }
    }
    variants
}

/// The entry's ready caption assets, served by the API.
pub fn subtitles_of(captions: &Value) -> Vec<SubtitleTrack> {
    captions["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|caption| caption["status"].as_i64() == Some(2))
        .filter_map(|caption| {
            let id = caption["id"].as_str().filter(|id| !id.is_empty())?;
            let ext = caption["fileExt"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let format = match (caption["format"].as_i64(), ext.as_str()) {
                (Some(1), _) | (_, "srt") => SubtitleFormat::Srt,
                (Some(3), _) | (_, "vtt") => SubtitleFormat::Vtt,
                _ => SubtitleFormat::Ttml,
            };
            Some(SubtitleTrack {
                url: Url::parse(&format!("{CAPTION_SERVE}{id}")).ok()?,
                language: caption["languageCode"]
                    .as_str()
                    .or(caption["language"].as_str())
                    .unwrap_or("und")
                    .to_string(),
                name: caption["label"].as_str().and_then(clean_title),
                format,
                auto: false,
                headers: Vec::new(),
            })
        })
        .collect()
}

pub struct KalturaResolver {
    http: Http,
}

impl KalturaResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// POSTs `actions` as one multirequest for `partner`, returning the answer to each.
    async fn multirequest(
        &self,
        partner: &str,
        actions: Vec<Value>,
        origin: &Url,
    ) -> Result<Vec<Value>, ResolveError> {
        let mut body = json!({
            "apiVersion": "3.3.0",
            "clientTag": "html5:v3.1.0",
            "format": 1,
            "ks": "",
            "partnerId": partner,
        });
        let object = body.as_object_mut().expect("an object");
        for (index, action) in actions.into_iter().enumerate() {
            object.insert((index + 1).to_string(), action);
        }
        let response = self
            .http
            .post(Url::parse(API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .json(&body)
            .send()
            .await?;
        let status = response.status;
        let (bytes, _) = response.bytes_up_to(MAX_PAGE).await?;
        if !status.is_success() {
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                code => ResolveError::unavailable(origin, format!("the API answered HTTP {code}")),
            });
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ResolveError::malformed(origin, format!("JSON: {e}")))?;
        value.as_array().cloned().ok_or_else(|| {
            ResolveError::malformed(origin, "the API did not answer a multirequest list")
        })
    }

    /// The entry id a reference id names.
    async fn entry_by_reference(
        &self,
        partner: &str,
        reference: &str,
        origin: &Url,
    ) -> Result<String, ResolveError> {
        let answer = self
            .multirequest(
                partner,
                vec![
                    session(partner),
                    list_entries(json!({"referenceIdEqual": reference}), "id"),
                ],
                origin,
            )
            .await?;
        for item in answer.iter().take(2) {
            if let Some((code, message)) = exception(item) {
                return Err(exception_error(&code, &message, origin));
            }
        }
        answer
            .get(1)
            .and_then(|list| list["objects"][0]["id"].as_str())
            .map(String::from)
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }
}

#[async_trait]
impl Resolver for KalturaResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Kaltura",
            hosts: &["kaltura.com"],
            features: &[
                "entries",
                "player embeds",
                "reference ids",
                "manifests",
                "live",
                "captions",
                "drm reported",
            ],
            formats: &["mp4", "webm", "flv", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://cdnapisec.kaltura.com/p/243342/sp/24334200/playManifest/entryId/1_sf5ovm7u/format/url/protocol/https",
                "https://www.kaltura.com/index.php/extwidget/preview/partner_id/1770401/uiconf_id/37307382/entry_id/0_58u8kme7/embed/iframe",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let entry_id = match &link.entry {
            Entry::Id(id) => id.clone(),
            Entry::Reference(reference) => {
                self.entry_by_reference(&link.partner, reference, url)
                    .await?
            }
        };
        let answer = self
            .multirequest(
                &link.partner,
                vec![
                    session(&link.partner),
                    list_entries(json!({"redirectFromEntryId": entry_id}), ENTRY_FIELDS),
                    json!({
                        "action": "getbyentryid",
                        "entryId": entry_id,
                        "service": "flavorAsset",
                        "ks": "{1:result:ks}",
                    }),
                    json!({
                        "action": "list",
                        "filter:entryIdEqual": entry_id,
                        "service": "caption_captionasset",
                        "ks": "{1:result:ks}",
                    }),
                ],
                url,
            )
            .await?;
        for item in answer.iter().take(3) {
            if let Some((code, message)) = exception(item) {
                return Err(exception_error(&code, &message, url));
            }
        }
        let ks = answer
            .first()
            .and_then(|item| item["ks"].as_str())
            .ok_or_else(|| ResolveError::malformed(url, "the widget session carries no ks"))?;
        let entry = answer
            .get(1)
            .map(|list| &list["objects"][0])
            .filter(|entry| entry.is_object())
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let flavors: Vec<Value> = answer
            .get(2)
            .and_then(|item| item.as_array())
            .cloned()
            .unwrap_or_default();
        let subtitles = match answer.get(3) {
            Some(captions) => match exception(captions) {
                Some((code, message)) => {
                    tracing::warn!(entry = %entry_id, %code, %message, "kaltura would not list the captions");
                    Vec::new()
                }
                None => subtitles_of(captions),
            },
            None => Vec::new(),
        };
        let variants = variants_of(entry, &flavors, ks);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the entry has no ready flavors",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = entry["id"].as_str().map(String::from);
        resolved.title = entry["name"].as_str().and_then(clean_title);
        resolved.description = entry["description"].as_str().and_then(clean_title);
        resolved.uploaded_at = entry["createdAt"]
            .as_i64()
            .filter(|t| *t > 0)
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = duration_of(entry);
        resolved.live = is_live(entry);
        resolved.thumbnail = entry["thumbnailUrl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(url.clone());
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("kaltura_fixture.json")).unwrap()
    }

    fn id_link(partner: &str, entry: &str) -> Option<Link> {
        Some(Link {
            partner: partner.into(),
            entry: Entry::Id(entry.into()),
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(
                "http://www.kaltura.com/index.php/kwidget/cache_st/1319644/wid/_243342/uiconf_id/20540612/entry_id/1_1jc2y3e4"
            ),
            id_link("243342", "1_1jc2y3e4")
        );
        assert_eq!(
            link(
                "https://www.kaltura.com/index.php/extwidget/preview/partner_id/1770401/uiconf_id/37307382/entry_id/0_58u8kme7/embed/iframe?&flashvars[streamerType]=auto"
            ),
            id_link("1770401", "0_58u8kme7")
        );
        assert_eq!(
            link(
                "https://cdnapisec.kaltura.com/html5/html5lib/v2.30.2/mwEmbedFrame.php/p/1337/uiconf_id/20540612/entry_id/1_sf5ovm7u?wid=_243342"
            ),
            id_link("243342", "1_sf5ovm7u")
        );
        assert_eq!(
            link(
                "https://cdnapisec.kaltura.com/p/811441/sp/81144100/embedIframeJs/uiconf_id/57733292/partner_id/811441?iframeembed=true&playerId=kaltura_player&entry_id=1_klnhtzpl"
            ),
            id_link("811441", "1_klnhtzpl")
        );
        assert_eq!(
            link(
                "https://cdnapisec.kaltura.com/p/811441/embedPlaykitJs/uiconf_id/57733292?iframeembed=true&entry_id=1_klnhtzpl"
            ),
            id_link("811441", "1_klnhtzpl")
        );
        assert_eq!(
            link(
                "https://cdnapisec.kaltura.com/p/243342/sp/24334200/playManifest/entryId/1_sf5ovm7u/format/url/protocol/https"
            ),
            id_link("243342", "1_sf5ovm7u")
        );
        assert_eq!(
            link(
                "https://cdnapisec.kaltura.com/p/243342/sp/24334200/embedIframeJs/uiconf_id/1/partner_id/243342?flashvars[referenceId]=my-ref"
            ),
            Some(Link {
                partner: "243342".into(),
                entry: Entry::Reference("my-ref".into())
            })
        );
        assert_eq!(
            link("https://cdnapisec.kaltura.com/p/811441/embedPlaykitJs/uiconf_id/57733292"),
            None
        );
        assert_eq!(link("https://corp.kaltura.com/"), None);
        assert_eq!(
            link(
                "https://www.kaltura.com/index.php/extwidget/preview/partner_id/abc/entry_id/1_sf5ovm7u"
            ),
            None
        );
        assert_eq!(
            manifest_url("243342", "1_sf5ovm7u").as_str(),
            "https://cdnapisec.kaltura.com/p/243342/sp/24334200/playManifest/entryId/1_sf5ovm7u/format/url/protocol/https"
        );
    }

    #[test]
    fn embedded_players_are_found_in_pages() {
        let html = r#"<html><body>
            <script>kWidget.embed({"targetId": "kaltura_player", "wid": "_243342", "uiconf_id": 20540612, "entry_id": "1_sf5ovm7u"});</script>
            <script src="https://cdnapisec.kaltura.com/p/811441/embedPlaykitJs/uiconf_id/57733292"></script>
            <script>var player = KalturaPlayer.setup({targetId: "p", provider: {partnerId: 811441}}); player.loadMedia({entryId: '1_klnhtzpl'});</script>
            <iframe src="https://www.kaltura.com/index.php/extwidget/preview/partner_id/1770401/uiconf_id/37307382/entry_id/0_58u8kme7/embed/iframe"></iframe>
            </body></html>"#;
        let page = Page::parse(html, &Url::parse("https://site.test/page").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            found,
            vec![
                manifest_url("243342", "1_sf5ovm7u").to_string(),
                manifest_url("811441", "1_klnhtzpl").to_string(),
                manifest_url("1770401", "0_58u8kme7").to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn entries_resolve_with_their_flavors_and_manifest() {
        let resolver = KalturaResolver::new(Http::replay(fixture()));
        let url = Url::parse("https://cdnapisec.kaltura.com/html5/html5lib/v2.30.2/mwEmbedFrame.php/p/243342/uiconf_id/20540612/entry_id/1_sf5ovm7u?wid=_243342").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("1_sf5ovm7u"));
        assert_eq!(resolved.title.as_deref(), Some("Kaltura Player ToolKit"));
        assert!(
            resolved
                .description
                .as_deref()
                .unwrap()
                .starts_with("The Kaltura player toolkit")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(114)));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1379965720);
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .unwrap()
                .as_str()
                .contains("/thumbnail/entry_id/1_sf5ovm7u")
        );
        assert!(!resolved.live);
        assert_eq!(
            resolved.variants.len(),
            10,
            "{:?}",
            resolved
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        let original = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("0-mp4"))
            .unwrap();
        assert_eq!((original.width, original.height), (Some(1280), Some(720)));
        assert_eq!(original.size, Some(83660 * 1024));
        assert_eq!(original.bitrate, Some(5998000));
        assert_eq!(original.fps, Some(23.976));
        assert_eq!(original.video, Some(VideoCodec::H264));
        assert_eq!(original.container, Some(Container::Mp4));
        assert_eq!(
            original.url.as_str(),
            "https://cdnapisec.kaltura.com/p/243342/sp/24334200/playManifest/entryId/1_sf5ovm7u/format/url/protocol/https/flavorId/1_4b5u78rl/ks/N2I1NGY4MmY1ZDUwODdlOTI5YmZiODFhYjQzNTI4ODQzNWY1YjNiYnwyNDMzNDI7MjQzMzQyOzE3ODkzNzg5ODE7MDsxNzg5MjkyNTgxLjI2MDY7MDt2aWV3Oiosd2lkZ2V0OjE7Ow=="
        );
        let webm = resolved
            .variants
            .iter()
            .find(|v| v.container == Some(Container::Webm))
            .unwrap();
        assert_eq!(webm.video, Some(VideoCodec::Vp8));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.container == Some(Container::Flv))
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.container == Some(Container::Other("3gp".into())))
        );
        let manifest = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::Hls)
            .unwrap();
        assert!(
            manifest
                .url
                .as_str()
                .contains("/format/applehttp/protocol/https/ks/"),
            "{}",
            manifest.url
        );
        assert!(resolved.subtitles.is_empty());
        let error = resolver
            .resolve(&Url::parse("http://www.kaltura.com/index.php/kwidget/cache_st/1319644/wid/_243342/uiconf_id/20540612/entry_id/1_1jc2y3e4").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
        let error = resolver
            .resolve(&Url::parse("https://cdnapisec.kaltura.com/p/1091/sp/109100/playManifest/entryId/1_p0ktkrap/format/url/protocol/https").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("blocked")),
            "{error}"
        );
    }

    #[test]
    fn caption_assets_become_tracks_and_locked_flavors_carry_their_system() {
        let captions: Value = serde_json::from_str(
            r#"{"objects":[{"id":"1_abcdefgh","entryId":"1_sf5ovm7u","language":"English","languageCode":"en","label":"English","format":"3","fileExt":"vtt","status":2,"isDefault":true,"objectType":"KalturaCaptionAsset"},{"id":"1_ijklmnop","language":"French","languageCode":"fr","label":"Français","format":"1","fileExt":"srt","status":2,"objectType":"KalturaCaptionAsset"},{"id":"1_notready","languageCode":"de","format":"2","status":1,"objectType":"KalturaCaptionAsset"}],"totalCount":3,"objectType":"KalturaCaptionAssetListResponse"}"#,
        )
        .unwrap();
        let tracks = subtitles_of(&captions);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].language, "en");
        assert_eq!(tracks[0].format, SubtitleFormat::Vtt);
        assert_eq!(
            tracks[0].url.as_str(),
            "https://cdnapisec.kaltura.com/api_v3/service/caption_captionasset/action/serve/captionAssetId/1_abcdefgh"
        );
        assert_eq!(tracks[1].format, SubtitleFormat::Srt);
        assert_eq!(tracks[1].name.as_deref(), Some("Français"));
        let entry: Value = serde_json::from_str(r#"{"id":"1_x","dataUrl":"https://cdnapisec.kaltura.com/p/1/sp/100/playManifest/entryId/1_x/format/url/protocol/https","duration":10}"#).unwrap();
        let flavors: Vec<Value> = serde_json::from_str(r#"[{"id":"1_locked","fileExt":"wvm","status":2,"width":1280,"height":720,"bitrate":1000,"videoCodecId":"avc1","frameRate":25},{"id":"1_partial","fileExt":"chun","status":2},{"id":"1_converting","fileExt":"mp4","status":1}]"#).unwrap();
        let variants = variants_of(&entry, &flavors, "ks1");
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].drm.as_deref(), Some("widevine"));
        assert_eq!(variants[1].kind, VariantKind::Hls);
    }
}
