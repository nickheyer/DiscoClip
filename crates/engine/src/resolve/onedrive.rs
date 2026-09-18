//! OneDrive shared files and folders, through the share API the web app calls with the
//! anonymous token it asks for first: a file of any kind by its download link, with its
//! video, audio or image facet when the item carries one, and a folder's files as a
//! playlist, each entry naming its item within the share.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "onedrive";
const TOKEN_API: &str = "https://api-badgerp.svc.ms/v1.0/token";
/// The web app's own id, which the token service issues anonymous tokens to.
const APP_ID: &str = "5cbed6ac-a083-4e14-b191-b4ba07653de2";
const SHARES_API: &str = "https://my.microsoftpersonalcontent.com/_api/v2.0/shares/";
/// Tokens last a week; one is asked for again well before that.
const TOKEN_LIFETIME: Duration = Duration::from_secs(5 * 24 * 60 * 60);

/// A share link, and the item within it a playlist entry names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub share: Url,
    pub item: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let known = host == "1drv.ms"
        || host == "onedrive.live.com"
        || host == "photos.onedrive.com"
        || host.ends_with("-my.sharepoint.com");
    if !known {
        return None;
    }
    let item = url
        .fragment()
        .and_then(|f| f.strip_prefix("item="))
        .filter(|i| !i.is_empty())
        .map(String::from);
    let mut share = url.clone();
    share.set_fragment(None);
    let has_path = share
        .path_segments()
        .is_some_and(|mut s| s.any(|p| !p.is_empty()));
    let has_id = share
        .query_pairs()
        .any(|(k, _)| matches!(k.as_ref(), "id" | "resid" | "cid" | "redeem"));
    (has_path || has_id).then_some(Link { share, item })
}

/// The share id the API takes: `u!` and the link, base64url encoded.
pub fn share_id(share: &Url) -> String {
    format!("u!{}", URL_SAFE_NO_PAD.encode(share.as_str()))
}

/// The format a drive item is in: what its type names, else what its name does.
fn container_of(item: &Value) -> Option<Container> {
    let mime = essence(item["file"]["mimeType"].as_str());
    Container::from_mime(&mime).or_else(|| item["name"].as_str().and_then(Container::from_name))
}

/// What a drive item is: what its facet says, then what its type says, then what its
/// name's extension does.
pub fn kind_of(item: &Value) -> MediaKind {
    if item["video"].is_object() {
        return MediaKind::Video;
    }
    if item["audio"].is_object() {
        return MediaKind::Audio;
    }
    if item["image"].is_object() || item["photo"].is_object() {
        return MediaKind::Image;
    }
    let mime = essence(item["file"]["mimeType"].as_str());
    match MediaKind::from_mime(&mime) {
        MediaKind::File => container_of(item).map_or(MediaKind::File, |c| c.kind()),
        kind => kind,
    }
}

/// The codec an audio file's format implies.
fn audio_codec_of(container: Option<&Container>) -> Option<AudioCodec> {
    match container {
        Some(Container::Mp3) => Some(AudioCodec::Mp3),
        Some(Container::M4a) => Some(AudioCodec::Aac),
        Some(Container::Ogg) => Some(AudioCodec::Vorbis),
        Some(Container::Opus) => Some(AudioCodec::Opus),
        Some(Container::Flac) => Some(AudioCodec::Flac),
        _ => None,
    }
}

/// A file variant for a drive item with a download link.
pub fn variant_of(item: &Value) -> Option<Variant> {
    let url = item["@content.downloadUrl"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())?;
    let mut v = Variant::new(url, VariantKind::File);
    v.container = container_of(item);
    v.size = item["size"].as_u64();
    match kind_of(item) {
        MediaKind::Video => {
            let video = &item["video"];
            v.width = video["width"].as_u64().map(|w| w as u32);
            v.height = video["height"].as_u64().map(|h| h as u32);
            v.fps = video["frameRate"].as_f64().filter(|f| *f > 0.0);
            v.bitrate = video["bitRate"].as_u64().filter(|b| *b > 0);
            v.duration = video["duration"]
                .as_u64()
                .filter(|d| *d > 0)
                .map(Duration::from_millis);
            if video["fourCC"]
                .as_str()
                .is_some_and(|c| c.eq_ignore_ascii_case("H264"))
            {
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
            }
        }
        MediaKind::Audio => {
            let audio = &item["audio"];
            v.audio_only = true;
            v.audio = audio_codec_of(v.container.as_ref());
            v.bitrate = audio["bitrate"].as_u64().filter(|b| *b > 0);
            v.duration = audio["duration"]
                .as_u64()
                .filter(|d| *d > 0)
                .map(Duration::from_millis);
        }
        MediaKind::Image => {
            let image = &item["image"];
            v.width = image["width"].as_u64().map(|w| w as u32);
            v.height = image["height"].as_u64().map(|h| h as u32);
        }
        MediaKind::File => {}
    }
    v.format_id = Some("original".into());
    Some(v)
}

fn resolved_of(item: &Value, share: &Url) -> Result<Resolved, ResolveError> {
    let variant = variant_of(item)
        .ok_or_else(|| ResolveError::unavailable(share, "the item carries no download link"))?;
    let name = item["name"].as_str().unwrap_or_default();
    let mut resolved = Resolved::of(PLATFORM, kind_of(item));
    resolved.id = item["id"].as_str().map(String::from);
    resolved.title = clean_title(name.rsplit_once('.').map_or(name, |(s, _)| s));
    resolved.description = item["description"].as_str().and_then(clean_title);
    resolved.uploader = item["createdBy"]["user"]["displayName"]
        .as_str()
        .and_then(clean_title);
    resolved.thumbnail = item["thumbnails"]
        .as_array()
        .and_then(|sets| sets.first())
        .and_then(|set| {
            set["large"]["url"]
                .as_str()
                .or(set["medium"]["url"].as_str())
        })
        .and_then(|u| Url::parse(u).ok());
    resolved.uploaded_at = item["lastModifiedDateTime"]
        .as_str()
        .or(item["createdDateTime"].as_str())
        .and_then(|t| t.parse::<Timestamp>().ok());
    resolved.duration = variant.duration;
    resolved.webpage_url = Some(share.clone());
    resolved.variants = vec![variant];
    Ok(resolved)
}

pub struct OnedriveResolver {
    http: Http,
    token: RwLock<Option<(String, Instant)>>,
}

impl OnedriveResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            token: RwLock::new(None),
        }
    }

    /// An anonymous token for the share API, kept until it nears its end.
    async fn token(&self, origin: &Url, fresh: bool) -> Result<String, ResolveError> {
        if !fresh
            && let Some((token, issued)) =
                self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
            && issued.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token);
        }
        let response = self
            .http
            .post(Url::parse(TOKEN_API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .json(&json!({"appId": APP_ID}))
            .send()
            .await?;
        if !response.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the token service answered HTTP {}", response.status),
            ));
        }
        let answer: Value = response.json(MAX_PAGE).await?;
        let token = answer["token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the token service issued no token"))?
            .to_string();
        *self.token.write().unwrap_or_else(|e| e.into_inner()) =
            Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// GETs `path` under the share, asking for a fresh token once when the one held is
    /// refused.
    async fn api(&self, share: &Url, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{SHARES_API}{}/{path}", share_id(share))).expect("valid");
        let mut fresh = false;
        loop {
            let token = self.token(origin, fresh).await?;
            // The share is redeemed for the token on its first use; without asking for
            // that the API refuses the link.
            let headers = [
                ("authorization".to_string(), format!("Badger {token}")),
                ("accept".to_string(), "application/json".to_string()),
                ("prefer".to_string(), "autoredeem".to_string()),
            ];
            let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
            match fetched.status.as_u16() {
                200..=299 => return fetched.json(origin),
                401 if !fresh => {
                    fresh = true;
                    continue;
                }
                401 | 403 => {
                    return Err(ResolveError::unavailable(
                        origin,
                        "the link is not shared with everyone",
                    ));
                }
                404 => return Err(ResolveError::NotFound(origin.clone())),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                status => {
                    let detail = fetched
                        .json(origin)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(String::from))
                        .unwrap_or_else(|| format!("HTTP {status}"));
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the share API refused the link: {detail}"),
                    ));
                }
            }
        }
    }

    async fn children(&self, share: &Url, origin: &Url) -> Result<Vec<Value>, ResolveError> {
        let mut path = "driveitem/children".to_string();
        let mut all = Vec::new();
        for _ in 0..50 {
            let page = self.api(share, &path, origin).await?;
            all.extend(page["value"].as_array().cloned().unwrap_or_default());
            match page["@odata.nextLink"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
            {
                Some(next) => {
                    let base = format!("{SHARES_API}{}/", share_id(share));
                    match next.as_str().strip_prefix(&base) {
                        Some(rest) => path = rest.to_string(),
                        None => break,
                    }
                }
                None => break,
            }
        }
        Ok(all)
    }
}

#[async_trait]
impl Resolver for OnedriveResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "OneDrive",
            hosts: &["1drv.ms", "onedrive.live.com", "photos.onedrive.com"],
            features: &[
                "shared files",
                "shared folders",
                "short links",
                "audio",
                "images",
                "documents",
            ],
            formats: &[
                "mp4", "mov", "webm", "mkv", "mp3", "m4a", "flac", "wav", "jpg", "png", "heic",
                "pdf", "any file",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            examples: &[
                "https://1drv.ms/v/c/49e18460ed20d89c/IQA0-4CgNI50R5PRqh3dLEGoAXDFVRoySH969JO0uU1c3mQ?e=35jiOO",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        if let Some(wanted) = &link.item {
            let child = self
                .children(&link.share, url)
                .await?
                .into_iter()
                .find(|c| c["id"].as_str() == Some(wanted))
                .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
            let mut resolved = resolved_of(&child, &link.share)?;
            resolved.webpage_url = Some(url.clone());
            return Ok(Resolution::from(resolved));
        }
        let item = self.api(&link.share, "driveitem", url).await?;
        if item["folder"].is_object() || item["bundle"].is_object() {
            let children = self.children(&link.share, url).await?;
            let entries: Vec<PlaylistEntry> = children
                .iter()
                .filter(|c| c["file"].is_object())
                .filter_map(|c| {
                    let id = c["id"].as_str()?;
                    let mut entry = link.share.clone();
                    entry.set_fragment(Some(&format!("item={id}")));
                    let name = c["name"].as_str().unwrap_or_default();
                    Some(PlaylistEntry {
                        url: entry,
                        title: clean_title(name.rsplit_once('.').map_or(name, |(s, _)| s)),
                        duration: c["video"]["duration"]
                            .as_u64()
                            .or(c["audio"]["duration"].as_u64())
                            .filter(|d| *d > 0)
                            .map(Duration::from_millis),
                    })
                })
                .collect();
            if entries.is_empty() {
                return Err(ResolveError::unavailable(url, "the folder holds no files"));
            }
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: item["id"].as_str().map(String::from),
                title: item["name"].as_str().and_then(clean_title),
                total: Some(entries.len()),
                entries,
            }));
        }
        if !item["file"].is_object() {
            return Err(ResolveError::unavailable(url, "the link names no file"));
        }
        Ok(Resolution::from(resolved_of(&item, &link.share)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn exchange(method: &str, url: &str, status: u16, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const SHARE: &str = "https://1drv.ms/v/c/49e18460ed20d89c/IQA0-4CgNI50R5PRqh3dLEGoAXDFVRoySH969JO0uU1c3mQ?e=35jiOO";
    const FOLDER: &str = "https://1drv.ms/f/c/0dbf39bbc36d89cf/EjbaXOC-7Q9JtIm685kEMgkBcOOiVQKSWUszH8fUWcvDNQ?e=Mh8LWI";
    const TOKEN: &str = r#"{"authScheme":"badger","token":"eyJhbGciOi.token.value"}"#;
    const VIDEO: &str = r#"{"@content.downloadUrl":"https://my.microsoftpersonalcontent.com/personal/49e18460ed20d89c/_layouts/15/download.aspx?UniqueId=a080fb34&tempauth=t","id":"49E18460ED20D89C!sa080fb348e34477493d1aa1ddd2c41a8","name":"Screenbox playback bug.mp4","size":132636591,"webUrl":"https://onedrive.live.com?cid=49E18460ED20D89C&id=49E18460ED20D89C!sa080","file":{"fileExtension":".mp4","mimeType":"video/mp4"},"video":{"audioChannels":1,"bitRate":426340,"duration":1996240,"fourCC":"H264","frameRate":25.0,"height":780,"width":936},"createdDateTime":"2025-12-22T23:42:09Z","lastModifiedDateTime":"2025-12-20T12:36:42Z","createdBy":{"user":{"displayName":"Armin Osaj","id":"49E18460ED20D89C"}}}"#;

    fn api(share: &str, path: &str) -> String {
        format!(
            "{SHARES_API}{}/{path}",
            share_id(&Url::parse(share).unwrap())
        )
    }

    #[test]
    fn links_are_read_and_encoded() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let parsed = link(SHARE).unwrap();
        assert_eq!(parsed.share.as_str(), SHARE);
        assert!(parsed.item.is_none());
        let with_item = link(&format!("{FOLDER}#item=ABC!123")).unwrap();
        assert_eq!(with_item.item.as_deref(), Some("ABC!123"));
        assert_eq!(with_item.share.as_str(), FOLDER);
        assert!(
            link("https://onedrive.live.com/?cid=49E18460ED20D89C&id=49E18460ED20D89C!322970")
                .is_some()
        );
        assert!(
            link("https://contoso-my.sharepoint.com/:v:/g/personal/someone/EaBc?e=1").is_some()
        );
        assert_eq!(link("https://onedrive.live.com/"), None);
        assert_eq!(link("https://1drv.ms/"), None);
        assert_eq!(
            share_id(
                &Url::parse("https://1drv.ms/u/s!Atj71Lw5QEdsrQnTRHMj-fjGc49N?e=hOB5gO").unwrap()
            ),
            "u!aHR0cHM6Ly8xZHJ2Lm1zL3UvcyFBdGo3MUx3NVFFZHNyUW5UUkhNai1makdjNDlOP2U9aE9CNWdP"
        );
    }

    #[tokio::test]
    async fn shared_videos_resolve_with_their_download_link() {
        let mut fixture = Fixture::new("onedrive", None);
        fixture
            .exchanges
            .push(exchange("POST", TOKEN_API, 200, TOKEN));
        fixture
            .exchanges
            .push(exchange("GET", &api(SHARE, "driveitem"), 200, VIDEO));
        let resolver = OnedriveResolver::new(Http::replay(fixture));
        let url = Url::parse(SHARE).unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Screenbox playback bug"));
        assert_eq!(resolved.uploader.as_deref(), Some("Armin Osaj"));
        assert_eq!(resolved.duration, Some(Duration::from_millis(1996240)));
        assert_eq!(resolved.variants.len(), 1);
        let v = &resolved.variants[0];
        assert!(v.url.as_str().contains("download.aspx"));
        assert_eq!(v.size, Some(132636591));
        assert_eq!((v.width, v.height), (Some(936), Some(780)));
        assert_eq!(v.fps, Some(25.0));
        assert_eq!(v.video, Some(VideoCodec::H264));
        assert_eq!(v.container, Some(Container::Mp4));
    }

    const PHOTO: &str = r#"{"@content.downloadUrl":"https://my.microsoftpersonalcontent.com/personal/49e18460ed20d89c/_layouts/15/download.aspx?UniqueId=photo1&tempauth=t","id":"49E18460ED20D89C!photo1","name":"Sunset over the bay.HEIC","size":2048000,"file":{"fileExtension":".heic","mimeType":"image/heic"},"image":{"height":3024,"width":4032},"photo":{"takenDateTime":"2025-06-01T19:12:00Z"},"thumbnails":[{"large":{"url":"https://thumbs.example/large.jpg","width":800,"height":600}}],"createdDateTime":"2025-06-02T09:00:00Z","createdBy":{"user":{"displayName":"Armin Osaj"}}}"#;
    const SONG: &str = r#"{"@content.downloadUrl":"https://my.microsoftpersonalcontent.com/personal/49e18460ed20d89c/_layouts/15/download.aspx?UniqueId=song1&tempauth=t","id":"49E18460ED20D89C!song1","name":"Demo take 3.mp3","size":4800000,"file":{"fileExtension":".mp3","mimeType":"audio/mpeg"},"audio":{"album":"Demos","bitrate":192000,"duration":200000,"title":"Demo take 3"},"createdDateTime":"2025-06-02T09:00:00Z"}"#;
    const PAPER: &str = r#"{"@content.downloadUrl":"https://my.microsoftpersonalcontent.com/personal/49e18460ed20d89c/_layouts/15/download.aspx?UniqueId=paper1&tempauth=t","id":"49E18460ED20D89C!paper1","name":"Thesis final.pdf","size":900000,"file":{"fileExtension":".pdf","mimeType":"application/pdf"},"createdDateTime":"2025-06-02T09:00:00Z"}"#;

    #[tokio::test]
    async fn images_audio_and_documents_resolve_as_what_they_are() {
        let mut fixture = Fixture::new("onedrive", None);
        fixture
            .exchanges
            .push(exchange("POST", TOKEN_API, 200, TOKEN));
        let photo = "https://1drv.ms/i/c/49e18460ed20d89c/photo1";
        let song = "https://1drv.ms/u/c/49e18460ed20d89c/song1";
        let paper = "https://1drv.ms/b/c/49e18460ed20d89c/paper1";
        fixture
            .exchanges
            .push(exchange("GET", &api(photo, "driveitem"), 200, PHOTO));
        fixture
            .exchanges
            .push(exchange("GET", &api(song, "driveitem"), 200, SONG));
        fixture
            .exchanges
            .push(exchange("GET", &api(paper, "driveitem"), 200, PAPER));
        let resolver = OnedriveResolver::new(Http::replay(fixture));

        let image = resolver
            .resolve(&Url::parse(photo).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.title.as_deref(), Some("Sunset over the bay"));
        assert_eq!(image.duration, None);
        assert_eq!(
            image.thumbnail.as_ref().map(|t| t.as_str()),
            Some("https://thumbs.example/large.jpg")
        );
        let v = &image.variants[0];
        assert_eq!(v.container, Some(Container::Other("heic".into())));
        assert_eq!((v.width, v.height), (Some(4032), Some(3024)));
        assert_eq!(v.size, Some(2048000));
        assert!(!v.audio_only && v.video.is_none());

        let audio = resolver
            .resolve(&Url::parse(song).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("Demo take 3"));
        assert_eq!(audio.duration, Some(Duration::from_millis(200000)));
        let v = &audio.variants[0];
        assert!(v.audio_only);
        assert_eq!(v.container, Some(Container::Mp3));
        assert_eq!(v.audio, Some(AudioCodec::Mp3));
        assert_eq!(v.bitrate, Some(192000));
        assert_eq!(v.size, Some(4800000));

        let file = resolver
            .resolve(&Url::parse(paper).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(file.media, MediaKind::File);
        assert_eq!(file.title.as_deref(), Some("Thesis final"));
        let v = &file.variants[0];
        assert_eq!(v.container, Some(Container::Other("pdf".into())));
        assert_eq!(v.size, Some(900000));
        assert!(v.audio.is_none() && v.video.is_none() && v.width.is_none());
    }

    #[tokio::test]
    async fn folders_list_their_files_and_entries_resolve_within_the_share() {
        let folder = r#"{"id":"F1","name":"Clips","folder":{"childCount":3},"size":10}"#;
        let children = format!(
            r#"{{"value":[{VIDEO},{{"id":"N1","name":"notes.txt","size":3,"file":{{"mimeType":"text/plain"}},"@content.downloadUrl":"https://my.microsoftpersonalcontent.com/dl/notes"}},{{"id":"SUB","name":"more","folder":{{"childCount":0}}}}]}}"#
        );
        let mut fixture = Fixture::new("onedrive", None);
        fixture
            .exchanges
            .push(exchange("POST", TOKEN_API, 200, TOKEN));
        fixture
            .exchanges
            .push(exchange("GET", &api(FOLDER, "driveitem"), 200, folder));
        fixture.exchanges.push(exchange(
            "GET",
            &api(FOLDER, "driveitem/children"),
            200,
            &children,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            &api(FOLDER, "driveitem/children"),
            200,
            &children,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            &api("https://1drv.ms/v/c/x/gone", "driveitem"),
            404,
            r#"{"error":{"code":"itemNotFound"}}"#,
        ));
        let resolver = OnedriveResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse(FOLDER).unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Clips"));
        assert_eq!(playlist.entries.len(), 2);
        let entry = &playlist.entries[0];
        assert_eq!(
            entry.url.fragment(),
            Some("item=49E18460ED20D89C!sa080fb348e34477493d1aa1ddd2c41a8")
        );
        assert_eq!(entry.title.as_deref(), Some("Screenbox playback bug"));
        assert_eq!(playlist.entries[1].title.as_deref(), Some("notes"));
        let resolved = resolver.resolve(&entry.url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants[0].size, Some(132636591));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://1drv.ms/v/c/x/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
