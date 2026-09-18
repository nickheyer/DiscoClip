//! Turning a link into media: each platform has a resolver that knows its pages and APIs,
//! and every resolver returns the same shape, which the engine picks a variant from.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::http::{Cookie, Http, HttpError, Response, StatusCode};
use crate::media::{AudioCodec, Container, VideoCodec};

pub use page::Page;

pub mod abc;
pub mod abcotvs;
pub mod acast;
pub mod acfun;
pub mod adobetv;
pub mod aljazeera;
pub mod allocine;
pub mod amp;
pub mod aparat;
pub mod applepodcasts;
pub mod archive_org;
pub mod ard;
pub mod audioboom;
pub mod baidu;
pub mod banbye;
pub mod bandcamp;
pub mod bilibili;
pub mod bitchute;
pub mod blerp;
pub mod blogger;
pub mod bloomberg;
pub mod bluesky;
pub mod bongacams;
pub mod box_;
pub mod brightcove;
pub mod bundesliga;
pub mod bundestag;
pub mod bunny;
pub mod businessinsider;
pub mod buzzfeed;
pub mod byutv;
pub mod catbox;
pub mod ccc;
pub mod chaturbate;
pub mod cloudflare_stream;
pub mod coub;
pub mod cpac;
pub mod dailymail;
pub mod dailymotion;
pub mod dash;
pub mod daystar;
pub mod dbtv;
pub mod dctp;
pub mod democracynow;
pub mod discord;
pub mod douyin;
pub mod dplay;
pub mod dropbox;
pub mod drtv;
pub mod dumpert;
pub mod dw;
pub mod facebook;
pub mod firsttv;
pub mod floatplane;
pub mod geo;
pub mod giphy;
pub mod google_drive;
pub mod hls;
pub mod ifunny;
pub mod imgur;
pub mod instagram;
pub mod ism;
pub mod jixie;
pub mod jwplayer;
pub mod kaltura;
pub mod kick;
pub mod kuaishou;
pub mod linkedin;
pub mod loom;
pub mod manifests;
pub mod mastodon;
pub mod medialaan;
pub mod mega;
pub mod mux;
pub mod newgrounds;
pub mod nexx;
pub mod niconico;
pub mod ninegag;
pub mod odysee;
pub mod onedrive;
pub mod onenewsnz;
pub mod page;
pub mod periscope;
pub mod pinterest;
pub mod reddit;
pub mod redgifs;
pub mod rumble;
pub mod seventeenlive;
pub mod smil;
pub mod snapchat;
pub mod streamable;
pub mod telegram;
pub mod tenor;
pub mod threads;
pub mod tiktok;
pub mod tumblr;
pub mod twentymin;
pub mod twitch;
pub mod twitter;
pub mod uplynk;
pub mod ustream;
pub mod util;
pub mod vidyard;
pub mod vimeo;
pub mod vk;
pub mod web;
pub mod weibo;
pub mod wikimedia;
pub mod wistia;
pub mod x;
pub mod xiaohongshu;
pub mod youtube;

pub fn builtin_resolvers(http: &Http) -> Vec<Arc<dyn Resolver>> {
    vec![
        Arc::new(youtube::YoutubeResolver::new(http.clone())),
        Arc::new(x::XResolver::new(http.clone())),
        Arc::new(tiktok::TiktokResolver::new(http.clone())),
        Arc::new(instagram::InstagramResolver::new(http.clone())),
        Arc::new(facebook::FacebookResolver::new(http.clone())),
        Arc::new(reddit::RedditResolver::new(http.clone())),
        Arc::new(twitch::TwitchResolver::new(http.clone())),
        Arc::new(kick::KickResolver::new(http.clone())),
        Arc::new(vimeo::VimeoResolver::new(http.clone())),
        Arc::new(dailymotion::DailymotionResolver::new(http.clone())),
        Arc::new(streamable::StreamableResolver::new(http.clone())),
        Arc::new(imgur::ImgurResolver::new(http.clone())),
        Arc::new(redgifs::RedgifsResolver::new(http.clone())),
        Arc::new(bilibili::BilibiliResolver::new(http.clone())),
        Arc::new(niconico::NiconicoResolver::new(http.clone())),
        Arc::new(douyin::DouyinResolver::new(http.clone())),
        Arc::new(kuaishou::KuaishouResolver::new(http.clone())),
        Arc::new(weibo::WeiboResolver::new(http.clone())),
        Arc::new(xiaohongshu::XiaohongshuResolver::new(http.clone())),
        Arc::new(vk::VkResolver::new(http.clone())),
        Arc::new(rumble::RumbleResolver::new(http.clone())),
        Arc::new(odysee::OdyseeResolver::new(http.clone())),
        Arc::new(bluesky::BlueskyResolver::new(http.clone())),
        Arc::new(threads::ThreadsResolver::new(http.clone())),
        Arc::new(tumblr::TumblrResolver::new(http.clone())),
        Arc::new(pinterest::PinterestResolver::new(http.clone())),
        Arc::new(linkedin::LinkedinResolver::new(http.clone())),
        Arc::new(snapchat::SnapchatResolver::new(http.clone())),
        Arc::new(loom::LoomResolver::new(http.clone())),
        Arc::new(telegram::TelegramResolver::new(http.clone())),
        Arc::new(ninegag::NinegagResolver::new(http.clone())),
        Arc::new(ifunny::IfunnyResolver::new(http.clone())),
        Arc::new(newgrounds::NewgroundsResolver::new(http.clone())),
        Arc::new(archive_org::ArchiveOrgResolver::new(http.clone())),
        Arc::new(wikimedia::WikimediaResolver::new(http.clone())),
        Arc::new(coub::CoubResolver::new(http.clone())),
        Arc::new(giphy::GiphyResolver::new(http.clone())),
        Arc::new(tenor::TenorResolver::new(http.clone())),
        Arc::new(catbox::CatboxResolver::new(http.clone())),
        Arc::new(google_drive::GoogleDriveResolver::new(http.clone())),
        Arc::new(dropbox::DropboxResolver::new(http.clone())),
        Arc::new(onedrive::OnedriveResolver::new(http.clone())),
        Arc::new(mega::MegaResolver::new(http.clone())),
        Arc::new(jwplayer::JwplayerResolver::new(http.clone())),
        Arc::new(brightcove::BrightcoveResolver::new(http.clone())),
        Arc::new(wistia::WistiaResolver::new(http.clone())),
        Arc::new(kaltura::KalturaResolver::new(http.clone())),
        Arc::new(vidyard::VidyardResolver::new(http.clone())),
        Arc::new(cloudflare_stream::CloudflareStreamResolver::new(
            http.clone(),
        )),
        Arc::new(mux::MuxResolver::new(http.clone())),
        Arc::new(bunny::BunnyResolver::new(http.clone())),
        Arc::new(onenewsnz::OneNewsNzResolver::new(http.clone())),
        Arc::new(seventeenlive::SeventeenLiveResolver::new(http.clone())),
        Arc::new(firsttv::FirstTvResolver::new(http.clone())),
        Arc::new(twentymin::TwentyMinResolver::new(http.clone())),
        Arc::new(abc::AbcResolver::new(http.clone())),
        Arc::new(abcotvs::AbcotvsResolver::new(http.clone())),
        Arc::new(acast::AcastResolver::new(http.clone())),
        Arc::new(acfun::AcfunResolver::new(http.clone())),
        Arc::new(adobetv::AdobetvResolver::new(http.clone())),
        Arc::new(aljazeera::AljazeeraResolver::new(http.clone())),
        Arc::new(allocine::AllocineResolver::new(http.clone())),
        Arc::new(aparat::AparatResolver::new(http.clone())),
        Arc::new(applepodcasts::ApplePodcastsResolver::new(http.clone())),
        Arc::new(audioboom::AudioboomResolver::new(http.clone())),
        Arc::new(amp::AmpResolver::new(http.clone())),
        Arc::new(ard::ArdResolver::new(http.clone())),
        Arc::new(dplay::DplayResolver::new(http.clone())),
        Arc::new(floatplane::FloatplaneResolver::new(http.clone())),
        Arc::new(jixie::JixieResolver::new(http.clone())),
        Arc::new(medialaan::MedialaanResolver::new(http.clone())),
        Arc::new(nexx::NexxResolver::new(http.clone())),
        Arc::new(periscope::PeriscopeResolver::new(http.clone())),
        Arc::new(uplynk::UplynkResolver::new(http.clone())),
        Arc::new(ustream::UstreamResolver::new(http.clone())),
        Arc::new(baidu::BaiduResolver::new(http.clone())),
        Arc::new(banbye::BanByeResolver::new(http.clone())),
        Arc::new(bandcamp::BandcampResolver::new(http.clone())),
        Arc::new(bitchute::BitchuteResolver::new(http.clone())),
        Arc::new(ccc::CccResolver::new(http.clone())),
        Arc::new(chaturbate::ChaturbateResolver::new(http.clone())),
        Arc::new(cpac::CpacResolver::new(http.clone())),
        Arc::new(dailymail::DailymailResolver::new(http.clone())),
        Arc::new(blerp::BlerpResolver::new(http.clone())),
        Arc::new(blogger::BloggerResolver::new(http.clone())),
        Arc::new(bloomberg::BloombergResolver::new(http.clone())),
        Arc::new(bongacams::BongacamsResolver::new(http.clone())),
        Arc::new(box_::BoxResolver::new(http.clone())),
        Arc::new(bundesliga::BundesligaResolver::new(http.clone())),
        Arc::new(bundestag::BundestagResolver::new(http.clone())),
        Arc::new(businessinsider::BusinessinsiderResolver::new(http.clone())),
        Arc::new(buzzfeed::BuzzfeedResolver::new(http.clone())),
        Arc::new(byutv::ByutvResolver::new(http.clone())),
        Arc::new(daystar::DaystarResolver::new(http.clone())),
        Arc::new(dbtv::DbtvResolver::new(http.clone())),
        Arc::new(dctp::DctpResolver::new(http.clone())),
        Arc::new(democracynow::DemocracynowResolver::new(http.clone())),
        Arc::new(drtv::DrtvResolver::new(http.clone())),
        Arc::new(dumpert::DumpertResolver::new(http.clone())),
        Arc::new(dw::DwResolver::new(http.clone())),
    ]
}

/// The resolvers that come last, whatever else is registered: Mastodon, which matches
/// links on any host by their shape and passes the rest on, and the generic web resolver,
/// which takes whatever is left and hands a page whose player one of `players` knows to
/// that player's resolver.
pub fn tail_resolvers(http: &Http, players: &[Arc<dyn Resolver>]) -> Vec<Arc<dyn Resolver>> {
    vec![
        Arc::new(mastodon::MastodonResolver::new(http.clone())),
        Arc::new(web::WebResolver::new(http.clone(), players.to_vec())),
    ]
}

/// Every resolver this crate carries, in the order links are offered to them:
/// [`builtin_resolvers`], then [`tail_resolvers`].
pub fn standard_resolvers(http: &Http) -> Vec<Arc<dyn Resolver>> {
    let mut list = builtin_resolvers(http);
    let tail = tail_resolvers(http, &list);
    list.extend(tail);
    list
}

const MAX_REDIRECT_HOPS: usize = 5;
/// The most of a page or API answer a resolver reads.
pub const MAX_PAGE: usize = 8 * 1024 * 1024;

/// Whether a platform's links need, or benefit from, stored cookies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSupport {
    /// The platform is read without an account.
    None,
    /// Public links work without one; a session unlocks age-gated, private or higher
    /// quality media.
    Optional,
    /// Nothing resolves without a logged-in session.
    Required,
}

/// What a resolver covers, for the coverage page and the smoke tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Platform {
    pub id: &'static str,
    pub name: &'static str,
    /// Hosts the resolver takes links from, as suffixes.
    pub hosts: &'static [&'static str],
    /// The kinds of links it handles: `videos`, `shorts`, `live`, `playlists`, `clips`...
    pub features: &'static [&'static str],
    /// How the media arrives: `mp4`, `webm`, `hls`, `dash`, `ism`, `rtmp`, `whep`...
    pub formats: &'static [&'static str],
    pub session: SessionSupport,
    /// Public links the scheduled smoke tests resolve.
    pub examples: &'static [&'static str],
}

/// What stored cookies are worth at a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionCheck {
    /// The platform has no sessions to check.
    Unsupported,
    /// The cookies form no session, or none is stored.
    LoggedOut,
    LoggedIn {
        account: String,
    },
}

#[async_trait]
pub trait Resolver: Send + Sync {
    fn id(&self) -> &'static str;
    fn platform(&self) -> Platform;
    fn matches(&self, url: &Url) -> bool;
    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError>;
    /// Cookies that get past the platform's consent and age gates without an account, sent
    /// with every request of the platform's unless its jar holds one of the same name.
    fn consent_cookies(&self) -> Vec<Cookie> {
        Vec::new()
    }
    /// Whether the platform's stored cookies log in, and as whom.
    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        Ok(SessionCheck::Unsupported)
    }
    /// Links to the platform's player embedded in `page`: frames, script tags and the
    /// inline markup the player is loaded with. The generic web resolver asks every
    /// resolver and hands the page on to the one whose player it finds.
    fn embeds_in(&self, _page: &Page) -> Vec<Url> {
        Vec::new()
    }
}

pub struct ResolverRegistry {
    resolvers: Vec<Arc<dyn Resolver>>,
}

impl ResolverRegistry {
    pub fn new(resolvers: Vec<Arc<dyn Resolver>>) -> Self {
        Self { resolvers }
    }

    pub fn resolvers(&self) -> &[Arc<dyn Resolver>] {
        &self.resolvers
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.resolvers.iter().map(|r| r.id()).collect()
    }

    pub fn platforms(&self) -> Vec<Platform> {
        self.resolvers.iter().map(|r| r.platform()).collect()
    }

    pub fn get(&self, id: &str) -> Option<&dyn Resolver> {
        self.resolvers
            .iter()
            .find(|r| r.id() == id)
            .map(|r| r.as_ref())
    }

    pub fn supports(&self, url: &Url) -> bool {
        self.resolvers.iter().any(|r| r.matches(url))
    }

    pub fn find(&self, url: &Url) -> Option<&dyn Resolver> {
        self.resolvers
            .iter()
            .find(|r| r.matches(url))
            .map(|r| r.as_ref())
    }

    /// Resolves with the matching resolvers in order: a resolver that finds the link is
    /// not one of its own after all hands it on to the next one that matches, and a
    /// resolver level redirect (a page that only links to media hosted elsewhere) goes
    /// through the registry again.
    pub async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let mut current = url.clone();
        for _ in 0..MAX_REDIRECT_HOPS {
            let mut outcome = Err(ResolveError::Unsupported(current.clone()));
            for resolver in self.resolvers.iter().filter(|r| r.matches(&current)) {
                outcome = resolver.resolve(&current).await;
                if let Err(ResolveError::Unsupported(declined)) = &outcome
                    && *declined == current
                {
                    tracing::debug!(resolver = resolver.id(), url = %current, "resolver passed the link on");
                    continue;
                }
                break;
            }
            match outcome {
                Err(ResolveError::Redirect(next)) if next != current => {
                    tracing::debug!(from = %current, to = %next, "resolver redirect");
                    current = next;
                }
                other => return other,
            }
        }
        Err(ResolveError::Unavailable {
            url: current,
            reason: "too many redirects between resolvers".into(),
        })
    }
}

/// A link is one piece of media, or a list of links to resolve one by one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    Media(Box<Resolved>),
    Playlist(Playlist),
}

impl Resolution {
    pub fn media(self) -> Option<Resolved> {
        match self {
            Resolution::Media(resolved) => Some(*resolved),
            Resolution::Playlist(_) => None,
        }
    }
}

impl From<Resolved> for Resolution {
    fn from(resolved: Resolved) -> Self {
        Resolution::Media(Box::new(resolved))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub resolver: String,
    pub id: Option<String>,
    pub title: Option<String>,
    pub entries: Vec<PlaylistEntry>,
    /// How many the platform reports, when more than `entries` holds.
    pub total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub url: Url,
    pub title: Option<String>,
    pub duration: Option<Duration>,
}

/// A portion of the media to keep, as YouTube clips and time-stamped links name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipRange {
    pub start: Duration,
    pub end: Option<Duration>,
}

impl ClipRange {
    pub fn length(&self) -> Option<Duration> {
        self.end.map(|end| end.saturating_sub(self.start))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleFormat {
    Vtt,
    Srt,
    Ttml,
    Ass,
    /// YouTube's `json3` timed text.
    Json3,
    /// Bilibili's JSON subtitles: a `body` of cues with `from`, `to` and `content`.
    BilibiliJson,
    /// TikTok's automatic captions: `utterances` with `start_time`, `end_time` (in
    /// milliseconds) and `text`.
    TiktokJson,
    /// A playlist of WebVTT segments.
    HlsVtt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub url: Url,
    pub language: String,
    pub name: Option<String>,
    pub format: SubtitleFormat,
    /// Machine generated.
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub resolver: String,
    /// The platform's own id for the media.
    #[serde(default)]
    pub id: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub uploader_url: Option<Url>,
    #[serde(default)]
    pub uploaded_at: Option<Timestamp>,
    pub duration: Option<Duration>,
    #[serde(default)]
    pub thumbnail: Option<Url>,
    #[serde(default)]
    pub webpage_url: Option<Url>,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub age_limit: Option<u8>,
    #[serde(default)]
    pub clip: Option<ClipRange>,
    #[serde(default)]
    pub subtitles: Vec<SubtitleTrack>,
    pub variants: Vec<Variant>,
}

impl Resolved {
    pub fn new(resolver: &str) -> Self {
        Self {
            resolver: resolver.to_string(),
            id: None,
            title: None,
            description: None,
            uploader: None,
            uploader_url: None,
            uploaded_at: None,
            duration: None,
            thumbnail: None,
            webpage_url: None,
            live: false,
            age_limit: None,
            clip: None,
            subtitles: Vec::new(),
            variants: Vec::new(),
        }
    }

    /// The DRM system every playable variant is locked with, if all are.
    pub fn drm(&self) -> Option<&str> {
        if self.variants.is_empty() || self.variants.iter().any(|v| v.drm.is_none()) {
            return None;
        }
        self.variants.iter().find_map(|v| v.drm.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantKind {
    /// One file over HTTP.
    File,
    Hls,
    Dash,
    /// Microsoft Smooth Streaming.
    Ism,
    Rtmp,
    Rtsp,
    /// WebRTC through a WHEP endpoint.
    Whep,
    /// A page whose player feeds a media source extension; captured in a headless browser.
    Browser,
}

impl VariantKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VariantKind::File => "file",
            VariantKind::Hls => "hls",
            VariantKind::Dash => "dash",
            VariantKind::Ism => "ism",
            VariantKind::Rtmp => "rtmp",
            VariantKind::Rtsp => "rtsp",
            VariantKind::Whep => "whep",
            VariantKind::Browser => "browser",
        }
    }
}

/// How a file's bytes are decrypted as they arrive, for hosts that keep files encrypted
/// end to end and hand out the key in the link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Cipher {
    /// AES-128 in counter mode: the counter block is `nonce` followed by a 64-bit
    /// big-endian block counter that starts at zero.
    Aes128Ctr { key: [u8; 16], nonce: [u8; 8] },
}

/// A request the download repeats while it runs, for hosts that serve a stream only
/// while its player keeps reporting playback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Keepalive {
    /// Bunny Stream's MediaCage ping: every two seconds, `url` with `hash`, `time`,
    /// `paused` and `resolution` in its query, the hash being the MD5 of
    /// `{secret}_{context_id}_{time}_{paused}_{resolution}`.
    BunnyPing {
        url: Url,
        secret: String,
        context_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub url: Url,
    pub kind: VariantKind,
    /// A separate audio stream to mux with a video-only `url`.
    pub audio_url: Option<Url>,
    pub container: Option<Container>,
    pub video: Option<VideoCodec>,
    pub audio: Option<AudioCodec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
    /// Bits per second.
    pub bitrate: Option<u64>,
    pub size: Option<u64>,
    pub duration: Option<Duration>,
    pub headers: Vec<(String, String)>,
    /// Query parameters every request for the variant's playlists, segments and keys
    /// carries, as hosts that sign the manifest link and check the signature on each
    /// segment require.
    #[serde(default)]
    pub query: Vec<(String, String)>,
    /// The platform's name for the format, such as a YouTube itag.
    #[serde(default)]
    pub format_id: Option<String>,
    /// A label such as `1080p60`.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// The RFC 6381 codec string, when known.
    #[serde(default)]
    pub codecs: Option<String>,
    #[serde(default)]
    pub video_only: bool,
    #[serde(default)]
    pub audio_only: bool,
    #[serde(default)]
    pub live: bool,
    /// The DRM system the stream is locked with; such variants cannot be downloaded.
    #[serde(default)]
    pub drm: Option<String>,
    /// How the bytes are decrypted as they are fetched, when the host stores them
    /// encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<Cipher>,
    /// The request the download keeps making while it runs, when the host serves the
    /// stream only to a player that reports playback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive: Option<Keepalive>,
}

impl Variant {
    pub fn new(url: Url, kind: VariantKind) -> Self {
        Self {
            url,
            kind,
            audio_url: None,
            container: None,
            video: None,
            audio: None,
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            size: None,
            duration: None,
            headers: Vec::new(),
            query: Vec::new(),
            format_id: None,
            label: None,
            language: None,
            codecs: None,
            video_only: false,
            audio_only: false,
            live: false,
            drm: None,
            cipher: None,
            keepalive: None,
        }
    }

    pub fn file(url: Url) -> Self {
        Self::new(url, VariantKind::File)
    }

    pub fn hls(url: Url) -> Self {
        Self::new(url, VariantKind::Hls)
    }

    pub fn dash(url: Url) -> Self {
        Self::new(url, VariantKind::Dash)
    }

    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = Some(width);
        self.height = Some(height);
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// `url` with the variant's query parameters added, for the requests the variant's
    /// playlists, segments and keys are fetched with; a parameter the link already
    /// carries is left as it is.
    pub fn signed(&self, url: &Url) -> Url {
        signed_url(url, &self.query)
    }

    /// The variant's own link with `query` added.
    pub fn signed_with(&self, query: &[(String, String)]) -> Url {
        signed_url(&self.url, query)
    }

    /// Fills codec identities from an RFC 6381 codec string.
    pub fn with_codecs(mut self, codecs: &str) -> Self {
        let (video, audio) = parse_codecs(Some(codecs));
        if self.video.is_none() {
            self.video = video;
        }
        if self.audio.is_none() {
            self.audio = audio;
        }
        self.codecs = Some(codecs.to_string());
        self
    }

    pub fn is_playable(&self) -> bool {
        self.drm.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no resolver handles {0}")]
    Unsupported(Url),
    #[error("no video found at {0}")]
    NotFound(Url),
    #[error("{url} is unavailable: {reason}")]
    Unavailable { url: Url, reason: String },
    #[error("unexpected response from {url}: {detail}")]
    Malformed { url: Url, detail: String },
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("media lives at {0}")]
    Redirect(Url),
    #[error("{url} is protected by {system} DRM")]
    Drm { url: Url, system: String },
    #[error("{url} needs a logged-in {platform} session: {reason}")]
    LoginRequired {
        url: Url,
        platform: &'static str,
        reason: Box<str>,
    },
    #[error("{0} is rate limiting requests; try again later")]
    RateLimited(Url),
    #[error("{url} needs a headless browser, which is not available: {reason}")]
    BrowserUnavailable { url: Url, reason: String },
}

impl ResolveError {
    /// Attribute a platform's manifest failure to the link that requested it, keeping
    /// transport diagnostics and redirects at their original destinations.
    fn at(mut self, origin: &Url) -> Self {
        match &mut self {
            Self::NotFound(url)
            | Self::RateLimited(url)
            | Self::Unavailable { url, .. }
            | Self::Malformed { url, .. }
            | Self::Drm { url, .. }
            | Self::LoginRequired { url, .. }
            | Self::BrowserUnavailable { url, .. } => *url = origin.clone(),
            Self::Unsupported(_) | Self::Redirect(_) | Self::Http(_) => {}
        }
        self
    }

    pub fn malformed(url: &Url, detail: impl Into<String>) -> Self {
        Self::Malformed {
            url: url.clone(),
            detail: detail.into(),
        }
    }

    pub fn unavailable(url: &Url, reason: impl Into<String>) -> Self {
        Self::Unavailable {
            url: url.clone(),
            reason: reason.into(),
        }
    }

    pub fn login_required(url: &Url, platform: &'static str, reason: impl Into<String>) -> Self {
        Self::LoginRequired {
            url: url.clone(),
            platform,
            reason: reason.into().into_boxed_str(),
        }
    }

    pub fn drm(url: &Url, system: impl Into<String>) -> Self {
        Self::Drm {
            url: url.clone(),
            system: system.into(),
        }
    }

    /// Whether the failure is the platform's doing rather than a bug: nothing there, taken
    /// down, locked, or busy.
    pub fn is_expected(&self) -> bool {
        matches!(
            self,
            ResolveError::NotFound(_)
                | ResolveError::Unavailable { .. }
                | ResolveError::Drm { .. }
                | ResolveError::LoginRequired { .. }
                | ResolveError::RateLimited(_)
        )
    }
}

/// `url` with `query` added, leaving parameters the link already carries as they are.
pub fn signed_url(url: &Url, query: &[(String, String)]) -> Url {
    if query.is_empty() {
        return url.clone();
    }
    let present: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    let mut signed = url.clone();
    {
        let mut pairs = signed.query_pairs_mut();
        for (name, value) in query {
            if !present.contains(name) {
                pairs.append_pair(name, value);
            }
        }
    }
    signed
}

/// Turns an HTTP status into the resolver error it means for `requested`: nothing there,
/// locked, rate limited, or unavailable.
pub fn check_status(response: &Response, requested: &Url) -> Result<(), ResolveError> {
    status_error(response.status, requested).map_or(Ok(()), Err)
}

pub fn status_error(status: StatusCode, requested: &Url) -> Option<ResolveError> {
    if status.is_success() {
        return None;
    }
    Some(match status.as_u16() {
        404 | 410 => ResolveError::NotFound(requested.clone()),
        429 => ResolveError::RateLimited(requested.clone()),
        401 | 403 => ResolveError::unavailable(requested, format!("HTTP {status}")),
        _ => ResolveError::unavailable(requested, format!("HTTP {status}")),
    })
}

/// A page or API answer read whole.
pub struct Fetched {
    pub url: Url,
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: bytes::Bytes,
}

impl Fetched {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self, requested: &Url) -> Result<serde_json::Value, ResolveError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| ResolveError::malformed(requested, format!("JSON: {e}")))
    }
}

/// The headers a browser sends when a person navigates to a page, beside its user agent.
/// Several platforms serve a data-less shell to any request without them.
pub fn navigation_headers() -> Vec<(String, String)> {
    [
        (
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
        ("accept-language", "en-US,en;q=0.9"),
        ("sec-fetch-dest", "document"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-site", "none"),
        ("upgrade-insecure-requests", "1"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
}

/// GETs `url` as `platform` with `user_agent`, reading at most `limit` bytes.
pub async fn fetch(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .user_agent(user_agent)
        .headers(headers)
        .send()
        .await?;
    let status = response.status;
    let final_url = response.url.clone();
    let content_type = response.content_type().map(str::to_owned);
    let (body, _) = response.bytes_up_to(limit).await?;
    Ok(Fetched {
        url: final_url,
        status,
        content_type,
        body,
    })
}

/// GETs `url` as `platform` the way Chrome would: its TLS and HTTP/2 fingerprint and its
/// own headers, for hosts that refuse any other client. `headers` still win over the
/// browser's.
pub async fn fetch_as_browser(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .impersonate()
        .send()
        .await?;
    let status = response.status;
    let final_url = response.url.clone();
    let content_type = response.content_type().map(str::to_owned);
    let (body, _) = response.bytes_up_to(limit).await?;
    Ok(Fetched {
        url: final_url,
        status,
        content_type,
        body,
    })
}

/// [`fetch_as_browser`], failing unless the answer is 2xx.
pub async fn fetch_ok_as_browser(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let fetched = fetch_as_browser(http, url, platform, headers, limit).await?;
    if let Some(error) = status_error(fetched.status, url) {
        return Err(error);
    }
    Ok(fetched)
}

/// [`fetch`], failing unless the answer is 2xx.
pub async fn fetch_ok(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let fetched = fetch(http, url, platform, user_agent, headers, limit).await?;
    if let Some(error) = status_error(fetched.status, url) {
        return Err(error);
    }
    Ok(fetched)
}

/// What a direct file link answers to a request for its first byte: where it ends up,
/// its content type, and its length from `Content-Range` (or `Content-Length` when the
/// host ignores the range).
pub struct Probed {
    pub url: Url,
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub size: Option<u64>,
    /// The file name the host offers in `Content-Disposition`.
    pub filename: Option<String>,
}

/// GETs the first byte of `url` as `platform`, without reading any body.
pub async fn probe_file(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
) -> Result<Probed, ResolveError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .user_agent(user_agent)
        .headers(headers)
        .header("range", "bytes=0-0")
        .media()
        .send()
        .await?;
    let size = response
        .header("content-range")
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            if response.status.as_u16() == 200 {
                response.content_length().filter(|n| *n > 0)
            } else {
                None
            }
        });
    Ok(Probed {
        url: response.url.clone(),
        status: response.status,
        content_type: response.content_type().map(str::to_owned),
        size,
        filename: response
            .header("content-disposition")
            .and_then(disposition_filename),
    })
}

/// The file name a `Content-Disposition` value carries: the RFC 5987 `filename*` when
/// there is one, else the quoted or bare `filename`.
pub fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename*=") {
            let rest = rest.trim_matches('"');
            let encoded = rest.splitn(3, '\'').nth(2).unwrap_or(rest);
            let decoded = percent_encoding::percent_decode_str(encoded)
                .decode_utf8_lossy()
                .into_owned();
            if !decoded.trim().is_empty() {
                return Some(decoded);
            }
        } else if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"').trim();
            if !name.is_empty() {
                plain = Some(name.to_string());
            }
        }
    }
    plain
}

/// The `type/subtype` of a `Content-Type` value, lower-cased, without parameters.
pub fn essence(content_type: Option<&str>) -> String {
    content_type
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .map(|mime| mime.essence_str().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_hls_type(essence: &str) -> bool {
    matches!(
        essence,
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
    )
}

pub fn is_dash_type(essence: &str) -> bool {
    essence == "application/dash+xml"
}

pub fn is_ism_type(essence: &str) -> bool {
    matches!(
        essence,
        "application/vnd.ms-sstr+xml" | "application/vnd.ms-sstr" | "text/xml" | "application/xml"
    )
}

/// The lower-cased extension of the URL path, when it looks like a file extension.
pub fn path_extension(url: &Url) -> Option<String> {
    Path::new(url.path())
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 5)
        .map(str::to_ascii_lowercase)
}

/// Maps an RFC 6381 codec list such as `avc1.64001f,mp4a.40.2` to codec identities.
pub fn parse_codecs(codecs: Option<&str>) -> (Option<VideoCodec>, Option<AudioCodec>) {
    let mut video: Option<VideoCodec> = None;
    let mut audio: Option<AudioCodec> = None;
    for item in codecs.unwrap_or("").split(',') {
        let item = item.trim().to_ascii_lowercase();
        let tag = item.split('.').next().unwrap_or("");
        let found_video = match tag {
            "avc1" | "avc3" | "h264" => Some(VideoCodec::H264),
            "hvc1" | "hev1" | "hevc" | "h265" | "dvh1" | "dvhe" => Some(VideoCodec::H265),
            "vp09" | "vp9" => Some(VideoCodec::Vp9),
            "vp08" | "vp8" => Some(VideoCodec::Vp8),
            "av01" | "av1" => Some(VideoCodec::Av1),
            _ => None,
        };
        let found_audio = match tag {
            "mp4a" if item.starts_with("mp4a.40.34") || item.starts_with("mp4a.6b") => {
                Some(AudioCodec::Mp3)
            }
            "mp4a" | "aac" => Some(AudioCodec::Aac),
            "opus" => Some(AudioCodec::Opus),
            "vorbis" => Some(AudioCodec::Vorbis),
            "mp3" => Some(AudioCodec::Mp3),
            "ec-3" | "ac-3" | "flac" | "alac" => Some(AudioCodec::Other(item.clone())),
            _ => None,
        };
        if video.is_none() && found_video.is_some() {
            video = found_video;
        }
        if audio.is_none() && found_audio.is_some() {
            audio = found_audio;
        }
    }
    (video, audio)
}

/// Parses a frame rate written as a rational such as `30000/1001` or as a decimal.
pub fn parse_rate(rate: &str) -> Option<f64> {
    let rate = rate.trim();
    match rate.split_once('/') {
        Some((num, den)) => {
            let num: f64 = num.trim().parse().ok()?;
            let den: f64 = den.trim().parse().ok()?;
            (den != 0.0).then(|| num / den)
        }
        None => rate.parse().ok(),
    }
}

pub fn clean_title(raw: &str) -> Option<String> {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(200).collect())
}

/// A `?t=1h2m3s`, `?t=95`, `#t=95` or `?start=95` time-stamp on a link.
pub fn timestamp_hint(url: &Url) -> Option<Duration> {
    let value = url
        .query_pairs()
        .find(|(k, _)| k == "t" || k == "start" || k == "time_continue")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            url.fragment()
                .and_then(|f| f.strip_prefix("t="))
                .map(str::to_string)
        })?;
    parse_time_stamp(&value)
}

/// `95`, `95s`, `1m35s`, `1h2m3s`, `01:35`, `1:02:03`.
pub fn parse_time_stamp(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.contains(':') {
        let mut total = 0f64;
        for part in text.split(':') {
            total = total * 60.0 + part.parse::<f64>().ok()?;
        }
        return (total >= 0.0).then(|| Duration::from_secs_f64(total));
    }
    if let Ok(secs) = text.parse::<f64>() {
        return (secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let mut total = 0f64;
    let mut number = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        total += match ch {
            'h' => value * 3600.0,
            'm' => value * 60.0,
            's' => value,
            _ => return None,
        };
    }
    if !number.is_empty() {
        total += number.parse::<f64>().ok()?;
    }
    Some(Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn essence_drops_parameters_and_case() {
        assert_eq!(essence(Some("Video/MP4; codecs=\"avc1\"")), "video/mp4");
        assert_eq!(essence(Some("text/html;charset=utf-8")), "text/html");
        assert_eq!(essence(None), "");
        assert_eq!(essence(Some("not a type")), "");
    }

    #[test]
    fn path_extension_reads_file_like_paths_only() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            path_extension(&url("https://h/dir/Clip.MP4?x=1")),
            Some("mp4".into())
        );
        assert_eq!(path_extension(&url("https://h/dir/")), None);
        assert_eq!(
            path_extension(&url("https://h/archive.tar.gz")),
            Some("gz".into())
        );
        assert_eq!(path_extension(&url("https://h/a.reallylong")), None);
    }

    #[test]
    fn codec_strings_map_to_identities() {
        assert_eq!(
            parse_codecs(Some("avc1.64001f,mp4a.40.2")),
            (Some(VideoCodec::H264), Some(AudioCodec::Aac))
        );
        assert_eq!(
            parse_codecs(Some("vp09.00.10.08, opus")),
            (Some(VideoCodec::Vp9), Some(AudioCodec::Opus))
        );
        assert_eq!(parse_codecs(Some("mp4a.40.34")).1, Some(AudioCodec::Mp3));
        assert_eq!(parse_codecs(None), (None, None));
        let variant = Variant::file(Url::parse("https://h/v.mp4").unwrap())
            .with_codecs("hvc1.1.6.L93.B0,ec-3");
        assert_eq!(variant.video, Some(VideoCodec::H265));
        assert_eq!(variant.audio, Some(AudioCodec::Other("ec-3".into())));
    }

    #[test]
    fn time_stamps_in_every_common_shape() {
        assert_eq!(parse_time_stamp("95"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("95s"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("1m35s"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_time_stamp("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_time_stamp("01:35"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("x"), None);
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            timestamp_hint(&url("https://youtu.be/x?t=1m35s")),
            Some(Duration::from_secs(95))
        );
        assert_eq!(
            timestamp_hint(&url("https://v.test/clip#t=10")),
            Some(Duration::from_secs(10))
        );
        assert_eq!(timestamp_hint(&url("https://v.test/clip")), None);
    }

    #[test]
    fn disposition_file_names_are_read_in_both_forms() {
        assert_eq!(
            disposition_filename("attachment; filename=\"Big Buck Bunny.mp4\""),
            Some("Big Buck Bunny.mp4".into())
        );
        assert_eq!(
            disposition_filename(
                "attachment; filename=\"x.mp4\"; filename*=UTF-8''youtube-dl%20test%20%27%C3%A4.mp4"
            ),
            Some("youtube-dl test 'ä.mp4".into())
        );
        assert_eq!(
            disposition_filename(
                "attachment;filename*=utf-8''Screenbox%20playback%20bug%2Emp4;filename=\"Screenbox playback bug.mp4\""
            ),
            Some("Screenbox playback bug.mp4".into())
        );
        assert_eq!(disposition_filename("inline"), None);
    }

    #[test]
    fn drm_is_reported_only_when_nothing_plays() {
        let mut resolved = Resolved::new("x");
        assert!(resolved.drm().is_none());
        let mut locked = Variant::dash(Url::parse("https://h/a.mpd").unwrap());
        locked.drm = Some("widevine".into());
        resolved.variants.push(locked.clone());
        assert_eq!(resolved.drm(), Some("widevine"));
        resolved
            .variants
            .push(Variant::file(Url::parse("https://h/a.mp4").unwrap()));
        assert!(resolved.drm().is_none());
        assert!(!locked.is_playable());
    }

    #[test]
    fn statuses_become_resolver_errors() {
        let url = Url::parse("https://h/v").unwrap();
        assert!(status_error(StatusCode::OK, &url).is_none());
        assert!(matches!(
            status_error(StatusCode::NOT_FOUND, &url),
            Some(ResolveError::NotFound(_))
        ));
        assert!(matches!(
            status_error(StatusCode::GONE, &url),
            Some(ResolveError::NotFound(_))
        ));
        assert!(matches!(
            status_error(StatusCode::TOO_MANY_REQUESTS, &url),
            Some(ResolveError::RateLimited(_))
        ));
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, &url),
            Some(ResolveError::Unavailable { .. })
        ));
        assert!(ResolveError::NotFound(url.clone()).is_expected());
        assert!(!ResolveError::Unsupported(url).is_expected());
    }
}
