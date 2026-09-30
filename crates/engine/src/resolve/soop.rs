//! SOOP (AfreecaTV) recordings, clips, lives and stations, through the APIs its players
//! call. A recording's files come from the station video API, each an HLS playlist in
//! three qualities; a recording in several files is a playlist of its parts. A catch story
//! lists the short clips cut from a broadcast. A live comes from the player API as a
//! broadcast number and an access token, and its playlist from the CDN the stream manager
//! assigns. A station's recordings are paged through the channel API. A stored session
//! unlocks the recordings and lives the site keeps for logged-in adults and subscribers.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{Offset, TimeZone};
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, Variant, clean_title, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "soop";

const VOD_API: &str = "https://api.m.sooplive.com/station/video/a/view";
const CATCH_API: &str = "https://api.m.sooplive.com/catchstory/a/view";
const LIVE_API: &str = "https://live.sooplive.com/afreeca/player_live_api.php";
const PRIVATE_AUTH: &str = "https://live.sooplive.com/api/private_auth.php";
const STREAM_MANAGER: &str = "https://livestream-manager.sooplive.com";
const STATION_STATUS: &str = "https://st.sooplive.com/api/get_station_status.php";
const CHANNEL_API: &str = "https://chapi.sooplive.com/api";
const PRIVATE_INFO: &str = "https://afevent2.sooplive.com/api/get_private_info.php";
const VOD_SITE: &str = "https://vod.sooplive.com";
const PLAY_SITE: &str = "https://play.sooplive.com";
/// How many recordings a station listing is read up to.
const PER_PAGE: usize = 60;
/// The CDNs the stream manager assigns a live from, tried after the one the player API
/// names.
const WORKING_CDNS: &[&str] = &[
    "gcp_cdn",
    "gs_cdn_mobile_web",
    "gs_cdn_pc_web",
    "lg_cdn_pc_web",
    "lg_cdn_mobile_web",
    "azure_cdn",
    "aws_cf",
];
/// CDNs the player API names that serve nothing to a browser.
const BAD_CDNS: &[&str] = &[
    "gs_cdn",
    "gs_cdn_chromecast",
    "lg_cdn_chromecast",
    "gs_cdn_pc_app",
    "kt_cdn",
];
const HOSTS: [&str; 3] = ["sooplive.com", "sooplive.co.kr", "afreecatv.com"];

static RE_BJ: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_]{2,}$").unwrap());
static RE_KIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]+$").unwrap());
/// First path segments of a station page that are pages of the site, not streamers.
const RESERVED: &[&str] = &[
    "login",
    "signup",
    "api",
    "app",
    "search",
    "directory",
    "live",
    "vod",
    "station",
    "my",
    "notice",
    "event",
    "shop",
    "esports",
    "player",
    "embed",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A recording or clip on the VOD player, `part` picking one file of a recording in
    /// several.
    Vod {
        no: u64,
        part: Option<usize>,
    },
    /// The short clips cut from a broadcast.
    CatchStory {
        no: u64,
    },
    Live {
        bj: String,
        broadcast: Option<u64>,
    },
    /// A station's recordings of one kind: `all`, `review`, `clip`, `normal`, `catch`.
    Station {
        bj: String,
        kind: String,
    },
}

/// The part of the host before the platform's domain: `vod`, `play`, `www`, `bj`...
fn subdomain(host: &str) -> Option<String> {
    for suffix in HOSTS {
        if host == suffix {
            return Some(String::new());
        }
        if let Some(prefix) = host.strip_suffix(&format!(".{suffix}")) {
            return Some(prefix.to_string());
        }
    }
    None
}

/// `#part-2`: which file of a recording in several a link picks out.
fn part_of(url: &Url) -> Option<usize> {
    url.fragment()?
        .strip_prefix("part-")?
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let sub = subdomain(&host)?;
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let number = |s: &str| s.parse::<u64>().ok();
    let streamer = |s: &str| (RE_BJ.is_match(s) && !RESERVED.contains(&s)).then(|| s.to_string());
    match (sub.as_str(), segments.as_slice()) {
        ("vod", [player, no]) if player.eq_ignore_ascii_case("player") => Some(Link::Vod {
            no: number(no)?,
            part: part_of(url),
        }),
        ("vod", [player, station, no])
            if player.eq_ignore_ascii_case("player") && station.eq_ignore_ascii_case("station") =>
        {
            Some(Link::Vod {
                no: number(no)?,
                part: part_of(url),
            })
        }
        ("vod", [player, no, "catchstory"]) if player.eq_ignore_ascii_case("player") => {
            Some(Link::CatchStory { no: number(no)? })
        }
        ("play", [bj]) => Some(Link::Live {
            bj: streamer(bj)?,
            broadcast: None,
        }),
        ("play", [bj, broadcast]) => Some(Link::Live {
            bj: streamer(bj)?,
            broadcast: Some(number(broadcast)?),
        }),
        ("" | "www" | "m", ["station", bj] | ["station", bj, "vod"]) => Some(Link::Station {
            bj: streamer(bj)?,
            kind: "all".to_string(),
        }),
        ("" | "www" | "m", ["station", bj, "vod", kind]) if RE_KIND.is_match(kind) => {
            Some(Link::Station {
                bj: streamer(bj)?,
                kind: kind.to_string(),
            })
        }
        ("bj" | "ch", [bj]) => Some(Link::Station {
            bj: streamer(bj)?,
            kind: "all".to_string(),
        }),
        _ => None,
    }
}

/// `2026-09-25 02:20:01`: the APIs write Korean local time without a zone.
pub fn parse_kst(text: &str) -> Option<Timestamp> {
    let local = DateTime::strptime("%Y-%m-%d %H:%M:%S", text.trim()).ok()?;
    local
        .to_zoned(TimeZone::fixed(Offset::constant(9)))
        .ok()
        .map(|zoned| zoned.timestamp())
}

/// A thumbnail link, which the APIs write with or without a scheme.
fn image_url(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with("//") {
        return Url::parse(&format!("https:{text}")).ok();
    }
    Url::parse(text).ok()
}

fn vod_page(no: u64) -> Url {
    Url::parse(&format!("{VOD_SITE}/player/{no}")).expect("valid")
}

fn station_page(bj: &str) -> Option<Url> {
    Url::parse(&format!("https://www.sooplive.com/station/{bj}")).ok()
}

/// Whether a station API answer's `code` refuses the recording, and how.
fn vod_refusal(data: &Value, origin: &Url) -> Option<ResolveError> {
    let code = util::int(&data["code"])?;
    Some(match code {
        -6221 => ResolveError::NotFound(origin.clone()),
        -6205 => ResolveError::unavailable(origin, "the recording is private"),
        code if code < 0 => ResolveError::unavailable(
            origin,
            util::text(&data["message"])
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| format!("the station API answered {code}")),
        ),
        _ => return None,
    })
}

pub struct SoopResolver {
    http: Http,
}

impl SoopResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A recording's record from the station video API.
    async fn vod_info(&self, no: u64, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(VOD_API).expect("valid");
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", vod_page(no).as_str())
            .header("origin", VOD_SITE)
            .form(&[("nTitleNo", no.to_string().as_str()), ("nApiLevel", "10")])
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("station API JSON: {e}")))?;
        let data = &answer["data"];
        if let Some(error) = vod_refusal(data, origin) {
            return Err(error);
        }
        if !data.is_object() {
            return Err(ResolveError::malformed(
                origin,
                "the station API answered without data",
            ));
        }
        Ok(data.clone())
    }

    /// Asks for the CloudFront cookies a subscriber's recording is served with. They land
    /// in the platform's jar, which every request for the playlist and its segments carries.
    async fn private_auth(
        &self,
        playlist: &Url,
        bj: &str,
        no: u64,
        origin: &Url,
    ) -> Result<(), ResolveError> {
        let response = self
            .http
            .post(Url::parse(PRIVATE_AUTH).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", vod_page(no).as_str())
            .header("origin", VOD_SITE)
            .form(&[
                ("type", "vod"),
                ("strm_id", bj),
                ("title_no", no.to_string().as_str()),
                ("url", playlist.as_str()),
            ])
            .send()
            .await?;
        if !response.is_success() {
            return Err(ResolveError::login_required(
                origin,
                PLATFORM,
                format!(
                    "the recording is for subscribers and the CDN refused the session (HTTP {})",
                    response.status
                ),
            ));
        }
        Ok(())
    }

    /// The variants of one file of a recording: its HLS renditions, or the file itself.
    async fn file_variants(&self, file: &Url, origin: &Url) -> Result<Vec<Variant>, ResolveError> {
        let headers = vec![("referer".to_string(), format!("{VOD_SITE}/"))];
        if file.path().ends_with(".m3u8") {
            let expanded = hls::expand(&self.http, file, PLATFORM, BROWSER_UA, &headers)
                .await
                .map_err(|e| e.at(origin))?;
            return Ok(expanded
                .variants
                .into_iter()
                .map(|mut v| {
                    v.format_id = Some(match &v.label {
                        Some(label) => format!("hls-{label}"),
                        None => "hls".to_string(),
                    });
                    v
                })
                .collect());
        }
        let mut v = Variant::file(file.clone());
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.headers = headers;
        v.format_id = Some("http".to_string());
        Ok(vec![v])
    }

    async fn resolve_vod(
        &self,
        no: u64,
        part: Option<usize>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let data = self.vod_info(no, url).await?;
        let files: Vec<(&Value, Url)> = data["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| Some((file, util::url_of(&file["file"], None)?)))
            .collect();
        let subscribers_only = util::text(&data["sub_upload_type"]).is_some_and(|t| !t.is_empty());
        if files.is_empty() {
            if util::text(&data["adult_status"]).as_deref() == Some("notLogin") {
                return Err(ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the recording is for logged-in adults",
                ));
            }
            if subscribers_only {
                return Err(ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the recording is for subscribers",
                ));
            }
            return Err(ResolveError::unavailable(
                url,
                "the recording lists no files",
            ));
        }
        let title = data["title"].as_str().and_then(clean_title);
        let bj = util::text(&data["bj_id"]).filter(|b| !b.is_empty());
        if files.len() > 1 && part.is_none() {
            let entries = files
                .iter()
                .enumerate()
                .map(|(index, (file, _))| {
                    let mut entry_url = vod_page(no);
                    entry_url.set_fragment(Some(&format!("part-{}", index + 1)));
                    PlaylistEntry {
                        url: entry_url,
                        title: Some(match &title {
                            Some(title) => format!("{title} (part {})", index + 1),
                            None => format!("Part {}", index + 1),
                        }),
                        duration: util::millis(&file["duration"]),
                    }
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(no.to_string()),
                title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let index = part.unwrap_or(1).clamp(1, files.len());
        let (file, file_url) = &files[index - 1];
        if subscribers_only {
            let bj = bj.clone().ok_or_else(|| {
                ResolveError::malformed(url, "a subscribers' recording without a streamer id")
            })?;
            self.private_auth(file_url, &bj, no, url).await?;
        }
        let variants = self.file_variants(file_url, url).await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(
            match util::text(&file["file_info_key"]).filter(|k| !k.is_empty()) {
                Some(key) => key,
                None if files.len() > 1 => format!("{no}-{index}"),
                None => no.to_string(),
            },
        );
        resolved.title = match (&title, files.len() > 1) {
            (Some(title), true) => Some(format!("{title} (part {index})")),
            (title, _) => title.clone(),
        };
        resolved.uploader = data["writer_nick"].as_str().and_then(clean_title);
        resolved.uploader_url = bj.as_deref().and_then(station_page);
        resolved.uploaded_at = util::text(&file["file_start"])
            .and_then(|t| parse_kst(&t))
            .or_else(|| util::text(&data["write_tm"]).and_then(|t| parse_kst(&t)));
        resolved.duration = util::millis(&file["duration"])
            .or_else(|| util::millis(&data["total_file_duration"]))
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::text(&data["thumb"]).and_then(|t| image_url(&t));
        resolved.webpage_url = Some(vod_page(no));
        resolved.age_limit = util::int(&data["grade"]).filter(|g| *g >= 19).map(|_| 19);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_catch_story(&self, no: u64, url: &Url) -> Result<Resolution, ResolveError> {
        let api = util::with_query(
            &Url::parse(CATCH_API).expect("valid"),
            &[("aStoryListIdx", ""), ("nStoryIdx", &no.to_string())],
        );
        let referer = format!("{VOD_SITE}/player/{no}/catchstory");
        let headers = vec![("referer".to_string(), referer)];
        let fetched = super::fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE)
            .await
            .map_err(|e| e.at(url))?;
        let answer = fetched.json(url)?;
        let stories: Vec<&Value> = answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|story| util::text(&story["story_type"]).as_deref() == Some("catch"))
            .collect();
        let entries: Vec<PlaylistEntry> = stories
            .iter()
            .flat_map(|story| story["catch_list"].as_array().into_iter().flatten())
            .filter_map(|catch| {
                let file = catch["files"].as_array()?.first()?;
                let title_no =
                    util::uint(&file["title_no"]).or_else(|| util::uint(&catch["title_no"]))?;
                Some(PlaylistEntry {
                    url: vod_page(title_no),
                    title: catch["title"].as_str().and_then(clean_title),
                    duration: util::millis(&file["duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let title = stories
            .first()
            .and_then(|story| story["copyrighter_nick"].as_str())
            .and_then(clean_title)
            .map(|nick| format!("{nick} catch story"));
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!("catch-{no}")),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }

    /// The `CHANNEL` of a player API answer.
    async fn live_api(&self, form: &[(&str, &str)], origin: &Url) -> Result<Value, ResolveError> {
        let response = self
            .http
            .post(Url::parse(LIVE_API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", &format!("{PLAY_SITE}/"))
            .header("origin", PLAY_SITE)
            .form(form)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("player API JSON: {e}")))?;
        let channel = &answer["CHANNEL"];
        if !channel.is_object() {
            return Err(ResolveError::malformed(
                origin,
                "the player API answered without a channel",
            ));
        }
        Ok(channel.clone())
    }

    /// The station's public record: nick, title and when the broadcast began.
    async fn station_status(&self, bj: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = util::with_query(
            &Url::parse(STATION_STATUS).expect("valid"),
            &[("szBjId", bj)],
        );
        let fetched = super::fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await
            .map_err(|e| e.at(origin))?;
        let answer = fetched.json(origin)?;
        if util::int(&answer["RESULT"]) != Some(1) {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(answer["DATA"].clone())
    }

    async fn resolve_live(
        &self,
        bj: &str,
        broadcast: Option<u64>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let channel = self.live_api(&[("bid", bj)], url).await?;
        let result = util::int(&channel["RESULT"]).unwrap_or(0);
        let bj_id = util::text(&channel["BJID"])
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| bj.to_string());
        let broadcast_no = util::uint(&channel["BNO"]).or(broadcast);
        let Some(broadcast_no) = broadcast_no else {
            return Err(match result {
                0 => ResolveError::unavailable(url, format!("{bj} is not streaming right now")),
                -6 => ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the broadcast is for logged-in viewers: adult-flagged or subscribers only",
                ),
                other => ResolveError::unavailable(
                    url,
                    format!("the player API answered {other} for {bj}"),
                ),
            });
        };
        let password = util::query_param(url, "pwd").filter(|p| !p.is_empty());
        if util::text(&channel["BPWD"]).as_deref() == Some("Y") && password.is_none() {
            return Err(ResolveError::unavailable(
                url,
                "the broadcast is password protected. Add ?pwd= to the link.",
            ));
        }
        let bno = broadcast_no.to_string();
        let mut form = vec![
            ("bno", bno.as_str()),
            ("stream_type", "common"),
            ("type", "aid"),
            ("quality", "master"),
        ];
        if let Some(password) = &password {
            form.push(("pwd", password.as_str()));
        }
        let token = self.live_api(&form, url).await?;
        let Some(aid) = util::text(&token["AID"]).filter(|a| !a.is_empty()) else {
            return Err(match util::int(&token["RESULT"]).unwrap_or(0) {
                0 => ResolveError::unavailable(url, "the broadcast has ended"),
                -6 => ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the broadcast is for logged-in viewers: adult-flagged or subscribers only",
                ),
                other => ResolveError::unavailable(
                    url,
                    format!("the player API answered {other} for the stream token"),
                ),
            });
        };
        let manager = util::url_of(&channel["RMD"], None)
            .unwrap_or_else(|| Url::parse(STREAM_MANAGER).expect("valid"));
        let mut cdns: Vec<String> = Vec::new();
        for cdn in util::text(&channel["CDN"])
            .into_iter()
            .chain(WORKING_CDNS.iter().map(|c| c.to_string()))
        {
            if !BAD_CDNS.contains(&cdn.as_str()) && !cdns.contains(&cdn) {
                cdns.push(cdn);
            }
        }
        let headers = vec![("referer".to_string(), format!("{PLAY_SITE}/"))];
        let query = vec![("aid".to_string(), aid.clone())];
        let mut expanded = None;
        let mut failure = None;
        for cdn in &cdns {
            let assign = util::with_query(
                &manager
                    .join("broad_stream_assign.html")
                    .map_err(|e| ResolveError::malformed(url, format!("stream manager: {e}")))?,
                &[
                    ("return_type", cdn.as_str()),
                    ("broad_key", &format!("{bno}-common-master-hls")),
                ],
            );
            let view_url = match super::fetch_ok(
                &self.http, &assign, PLATFORM, BROWSER_UA, &headers, MAX_PAGE,
            )
            .await
            .and_then(|fetched| fetched.json(url))
            .map(|answer| util::url_of(&answer["view_url"], None))
            {
                Ok(Some(view_url)) => view_url,
                Ok(None) => {
                    failure = Some(ResolveError::unavailable(
                        url,
                        format!("the stream manager assigned no playlist on {cdn}"),
                    ));
                    continue;
                }
                Err(error) => {
                    failure = Some(error.at(url));
                    continue;
                }
            };
            let playlist = util::with_query(&view_url, &[("aid", aid.as_str())]);
            match hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &headers).await {
                Ok(found) if !found.variants.is_empty() => {
                    expanded = Some(found);
                    break;
                }
                Ok(_) => {
                    failure = Some(ResolveError::unavailable(
                        url,
                        format!("the playlist on {cdn} lists no streams"),
                    ));
                }
                Err(error) => {
                    tracing::debug!(
                        platform = PLATFORM,
                        cdn,
                        "SOOP CDN refused the stream: {error}"
                    );
                    failure = Some(error.at(url));
                }
            }
        }
        let Some(expanded) = expanded else {
            return Err(failure
                .unwrap_or_else(|| ResolveError::unavailable(url, "no CDN served the stream")));
        };
        let station = self.station_status(&bj_id, url).await;
        let station = match station {
            Ok(station) => station,
            Err(error) => {
                tracing::debug!(
                    platform = PLATFORM,
                    bj = bj_id,
                    "station status not read: {error}"
                );
                Value::Null
            }
        };
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(bno.clone());
        resolved.title = channel["TITLE"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| station["station_title"].as_str().and_then(clean_title));
        resolved.uploader = channel["BJNICK"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| station["user_nick"].as_str().and_then(clean_title));
        resolved.uploader_url = station_page(&bj_id);
        resolved.uploaded_at = util::text(&station["broad_start"]).and_then(|t| parse_kst(&t));
        resolved.thumbnail = Url::parse(&format!("https://liveimg.sooplive.com/m/{bno}")).ok();
        resolved.webpage_url = Url::parse(&format!("{PLAY_SITE}/{bj_id}/{bno}")).ok();
        resolved.live = true;
        resolved.age_limit = util::int(&channel["GRADE"])
            .filter(|g| *g >= 19)
            .map(|_| 19);
        resolved.subtitles = expanded.subtitles;
        resolved.variants = expanded
            .variants
            .into_iter()
            .map(|mut v| {
                v.live = true;
                v.query = query.clone();
                v.format_id = Some(match &v.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                v
            })
            .collect();
        Ok(Resolution::from(resolved))
    }

    async fn resolve_station(
        &self,
        bj: &str,
        kind: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let station = self.station_status(bj, url).await?;
        let nick = station["user_nick"]
            .as_str()
            .and_then(clean_title)
            .unwrap_or_else(|| bj.to_string());
        let api = util::with_query(
            &Url::parse(&format!("{CHANNEL_API}/{bj}/vods/{kind}"))
                .map_err(|e| ResolveError::malformed(url, format!("station listing: {e}")))?,
            &[
                ("page", "1"),
                ("per_page", &PER_PAGE.to_string()),
                ("orderby", "reg_date"),
            ],
        );
        let headers = vec![(
            "referer".to_string(),
            "https://www.sooplive.com/".to_string(),
        )];
        let fetched = super::fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE)
            .await
            .map_err(|e| e.at(url))?;
        let listing = fetched.json(url)?;
        let entries: Vec<PlaylistEntry> = listing["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let title_no = util::uint(&item["title_no"])?;
                Some(PlaylistEntry {
                    url: vod_page(title_no),
                    title: item["title_name"].as_str().and_then(clean_title),
                    duration: util::millis(&item["ucc"]["total_file_duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                format!("{nick} has no {kind} recordings"),
            ));
        }
        let total = util::uint(&listing["meta"]["total"])
            .map(|n| n as usize)
            .filter(|n| *n >= entries.len())
            .unwrap_or(entries.len());
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!("{bj}-{kind}")),
            title: Some(format!("{nick} - {kind}")),
            total: Some(total),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for SoopResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "SOOP",
            hosts: &["sooplive.com", "sooplive.co.kr", "afreecatv.com"],
            features: &["recordings", "clips", "catch stories", "live", "stations"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Live, Tag::Video],
            session: SessionSupport::Optional,
            examples: &[
                "https://vod.sooplive.com/player/20515605",
                "https://vod.sooplive.co.kr/PLAYER/STATION/20515605",
                "https://vod.sooplive.com/player/103247/catchstory",
                "https://www.sooplive.com/station/khm11903/vod",
                "https://bj.afreecatv.com/khm11903",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Vod { no, part } => self.resolve_vod(no, part, url).await,
            Link::CatchStory { no } => self.resolve_catch_story(no, url).await,
            Link::Live { bj, broadcast } => self.resolve_live(&bj, broadcast, url).await,
            Link::Station { bj, kind } => self.resolve_station(&bj, &kind, url).await,
        }
    }

    /// Whether the stored cookies log in, asked of the endpoint the site's own scripts ask.
    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.http.jar(PLATFORM).is_empty() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse("https://www.sooplive.com/").expect("valid");
        let api = util::with_query(
            &Url::parse(PRIVATE_INFO).expect("valid"),
            &[("_", &Timestamp::now().as_millisecond().to_string())],
        );
        let headers = vec![("referer".to_string(), origin.to_string())];
        let fetched =
            super::fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        let answer = fetched.json(&origin)?;
        let channel = &answer["CHANNEL"];
        if util::int(&channel["IS_LOGIN"]) != Some(1) {
            return Ok(SessionCheck::LoggedOut);
        }
        let account = util::text(&channel["LOGIN_ID"])
            .filter(|id| !id.is_empty())
            .or_else(|| util::text(&channel["LOGIN_NICK"]).filter(|n| !n.is_empty()));
        Ok(match account {
            Some(account) => SessionCheck::LoggedIn { account },
            None => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::Cookie;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::resolve::VariantKind;
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
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
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("GET", url, status, content_type, body)
    }

    fn post(url: &str, body: String) -> Exchange {
        exchange("POST", url, 200, "application/json", body)
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-STREAM-INF:NAME=\"hd\",BANDWIDTH=1000000,RESOLUTION=960x540,CODECS=\"avc1.640020,mp4a.40.2\"\n../43377085_960x540_1000k.mp4/manifest.m3u8?cv=v1\n#EXT-X-STREAM-INF:NAME=\"original\",BANDWIDTH=8015000,RESOLUTION=1920x1080,CODECS=\"avc1.640032,mp4a.40.2\"\n../43377085.mp4/manifest.m3u8?cv=v1\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let vod = |no: u64, part: Option<usize>| Some(Link::Vod { no, part });
        assert_eq!(
            link("https://vod.sooplive.com/player/20515605"),
            vod(20515605, None)
        );
        assert_eq!(
            link("https://vod.sooplive.co.kr/player/20515605/"),
            vod(20515605, None)
        );
        assert_eq!(
            link("https://vod.afreecatv.com/player/20515605#part-2"),
            vod(20515605, Some(2))
        );
        assert_eq!(
            link("https://vod.sooplive.com/PLAYER/STATION/20515605"),
            vod(20515605, None)
        );
        assert_eq!(
            link("https://vod.sooplive.com/player/103247/catchstory"),
            Some(Link::CatchStory { no: 103247 })
        );
        assert_eq!(
            link("https://play.sooplive.com/khm11903"),
            Some(Link::Live {
                bj: "khm11903".into(),
                broadcast: None
            })
        );
        assert_eq!(
            link("https://play.afreecatv.com/pyh3646/237852185"),
            Some(Link::Live {
                bj: "pyh3646".into(),
                broadcast: Some(237852185)
            })
        );
        let station = |bj: &str, kind: &str| {
            Some(Link::Station {
                bj: bj.into(),
                kind: kind.into(),
            })
        };
        assert_eq!(
            link("https://www.sooplive.com/station/khm11903"),
            station("khm11903", "all")
        );
        assert_eq!(
            link("https://www.sooplive.co.kr/station/khm11903/vod"),
            station("khm11903", "all")
        );
        assert_eq!(
            link("https://www.sooplive.com/station/devil0108/vod/review"),
            station("devil0108", "review")
        );
        assert_eq!(
            link("https://bj.afreecatv.com/khm11903"),
            station("khm11903", "all")
        );
        assert_eq!(
            link("https://ch.sooplive.co.kr/khm11903"),
            station("khm11903", "all")
        );
        assert_eq!(link("https://play.sooplive.com/"), None);
        assert_eq!(link("https://play.sooplive.com/login"), None);
        assert_eq!(
            link("https://www.sooplive.com/station/khm11903/posts"),
            None
        );
        assert_eq!(link("https://vod.sooplive.com/player/abc"), None);
        assert_eq!(link("https://www.sooplive.com/"), None);
        assert_eq!(link("https://example.com/player/20515605"), None);
    }

    #[test]
    fn korean_local_times_are_read() {
        assert_eq!(
            parse_kst("2017-04-11 16:57:45"),
            Some("2017-04-11T07:57:45Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(parse_kst("yesterday"), None);
        assert_eq!(
            image_url("//iflv14.sooplive.com/clip/a_r.jpg")
                .unwrap()
                .as_str(),
            "https://iflv14.sooplive.com/clip/a_r.jpg"
        );
    }

    fn vod(files: Vec<Value>, extra: Value) -> String {
        let mut data = json!({
            "title": "혼자사는여자집", "writer_nick": "♥이슬이", "bj_id": "dasl8121",
            "total_file_duration": 213000, "thumb": "//videoimg.sooplive.com/thumb.jpg",
            "adult_status": "pass", "sub_upload_type": "", "grade": 0, "write_tm": "2017-04-11 17:03:03",
            "files": files
        });
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                data[key] = value.clone();
            }
        }
        json!({"result": 1, "data": data}).to_string()
    }

    fn file(key: &str, playlist: &str, start: &str, duration: u64) -> Value {
        json!({"file": playlist, "duration": duration, "file_start": start, "file_info_key": key, "file_type": "NORMAL", "quality": ["Original Quality", "540p"]})
    }

    #[tokio::test]
    async fn recordings_resolve_their_renditions_and_parts_make_a_playlist() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            VOD_API,
            vod(
                vec![file(
                    "BE689A0E_190960999_1_2_A",
                    "https://vod-normal-global-cdn-z02.sooplive.com/spkt/highlight/20170411/999/43377085.smil/manifest.m3u8?rp=o00&ut=0",
                    "2017-04-11 16:57:45",
                    213000,
                )],
                json!({}),
            ),
        ));
        fixture.exchanges.push(get(
            "https://vod-normal-global-cdn-z02.sooplive.com/spkt/highlight/20170411/999/43377085.smil/manifest.m3u8?rp=o00&ut=0",
            200,
            "application/x-mpegURL",
            MASTER.into(),
        ));
        for name in ["43377085_960x540_1000k.mp4", "43377085.mp4"] {
            fixture.exchanges.push(get(
                &format!("https://vod-normal-global-cdn-z02.sooplive.com/spkt/highlight/20170411/999/{name}/manifest.m3u8?cv=v1"),
                200,
                "application/x-mpegURL",
                MEDIA.into(),
            ));
        }
        let parts = vod(
            vec![
                file(
                    "20260922_328A2DAA_297314125_13",
                    "https://cdn.sooplive.com/a/manifest.m3u8",
                    "2026-09-23 12:38:11",
                    18002000,
                ),
                file(
                    "20260922_D2A475E9_297314125_14",
                    "https://cdn.sooplive.com/b/manifest.m3u8",
                    "2026-09-23 17:38:14",
                    7946000,
                ),
            ],
            json!({"title": "봉준 8시 기상", "writer_nick": "봉준", "bj_id": "khm11903", "total_file_duration": 25948000}),
        );
        fixture.exchanges.push(post(VOD_API, parts.clone()));
        fixture.exchanges.push(post(VOD_API, parts));
        fixture.exchanges.push(get(
            "https://cdn.sooplive.com/b/manifest.m3u8",
            200,
            "application/x-mpegURL",
            "#EXTM3U\n#EXT-X-STREAM-INF:NAME=\"hd4k\",BANDWIDTH=4000000,RESOLUTION=1280x720\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.sooplive.com/b/720.m3u8",
            200,
            "application/x-mpegURL",
            MEDIA.into(),
        ));
        let resolver = SoopResolver::new(Http::replay(fixture));
        let url = Url::parse("https://vod.sooplive.com/player/20515605").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("BE689A0E_190960999_1_2_A"));
        assert_eq!(resolved.title.as_deref(), Some("혼자사는여자집"));
        assert_eq!(resolved.uploader.as_deref(), Some("♥이슬이"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.sooplive.com/station/dasl8121"
        );
        assert_eq!(
            resolved.uploaded_at,
            Some("2017-04-11T07:57:45Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(213)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://videoimg.sooplive.com/thumb.jpg"
        );
        assert_eq!(resolved.age_limit, None);
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert_eq!(resolved.variants[0].height, Some(540));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-540p"));
        assert_eq!(resolved.variants[1].height, Some(1080));
        assert_eq!(resolved.variants[1].bitrate, Some(8_015_000));
        assert_eq!(
            resolved.variants[1].headers,
            vec![(
                "referer".to_string(),
                "https://vod.sooplive.com/".to_string()
            )]
        );

        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://vod.sooplive.com/player/207960709").unwrap())
            .await
            .unwrap()
        else {
            panic!("a recording in parts is a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("봉준 8시 기상"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://vod.sooplive.com/player/207960709#part-2"
        );
        assert_eq!(
            playlist.entries[1].title.as_deref(),
            Some("봉준 8시 기상 (part 2)")
        );
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs(7946))
        );
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.id.as_deref(), Some("20260922_D2A475E9_297314125_14"));
        assert_eq!(second.title.as_deref(), Some("봉준 8시 기상 (part 2)"));
        assert_eq!(second.duration, Some(Duration::from_secs(7946)));
        assert_eq!(second.variants.len(), 1);
        assert_eq!(second.variants[0].height, Some(720));
    }

    #[tokio::test]
    async fn locked_and_missing_recordings_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            VOD_API,
            vod(vec![], json!({"adult_status": "notLogin", "grade": 19})),
        ));
        fixture.exchanges.push(post(
            VOD_API,
            vod(vec![], json!({"sub_upload_type": "all_board"})),
        ));
        fixture.exchanges.push(post(
            VOD_API,
            json!({"result": -1, "data": {"code": -6221, "message": "This VOD does not exist."}})
                .to_string(),
        ));
        fixture.exchanges.push(post(
            VOD_API,
            json!({"result": -1, "data": {"code": -6205, "message": "private"}}).to_string(),
        ));
        let resolver = SoopResolver::new(Http::replay(fixture));
        let url = Url::parse("https://vod.sooplive.com/player/191612613").unwrap();
        let adult = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&adult, ResolveError::LoginRequired { platform, reason, .. } if *platform == PLATFORM && reason.contains("adults")),
            "{adult}"
        );
        let subscribers = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&subscribers, ResolveError::LoginRequired { reason, .. } if reason.contains("subscribers")),
            "{subscribers}"
        );
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let private = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&private, ResolveError::Unavailable { reason, .. } if reason == "the recording is private"),
            "{private}"
        );
    }

    #[tokio::test]
    async fn catch_stories_list_their_clips() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.m.sooplive.com/catchstory/a/view?aStoryListIdx=&nStoryIdx=103247",
            200,
            "application/json",
            json!({"result": 1, "data": [
                {"story_type": "catch", "story_idx": 103247, "copyrighter_nick": "요나ㆍ", "catch_list": [
                    {"title": "[캐치]완전 큰 꼬르륵", "writer_nick": "별을쫓아이젠", "files": [{"title_no": 123027017, "duration": 16000, "file": "https://cdn/a.m3u8"}]},
                    {"title": "[캐치]두 번째", "writer_nick": "someone", "files": [{"title_no": 123027018, "duration": 21000, "file": "https://cdn/b.m3u8"}]}
                ]},
                {"story_type": "ad", "catch_list": []}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.m.sooplive.com/catchstory/a/view?aStoryListIdx=&nStoryIdx=1",
            200,
            "application/json",
            json!({"result": 1, "data": []}).to_string(),
        ));
        let resolver = SoopResolver::new(Http::replay(fixture));
        let Resolution::Playlist(story) = resolver
            .resolve(&Url::parse("https://vod.sooplive.com/player/103247/catchstory").unwrap())
            .await
            .unwrap()
        else {
            panic!("a catch story is a playlist");
        };
        assert_eq!(story.id.as_deref(), Some("catch-103247"));
        assert_eq!(story.title.as_deref(), Some("요나ㆍ catch story"));
        assert_eq!(story.entries.len(), 2);
        assert_eq!(
            story.entries[0].url.as_str(),
            "https://vod.sooplive.com/player/123027017"
        );
        assert_eq!(
            story.entries[0].title.as_deref(),
            Some("[캐치]완전 큰 꼬르륵")
        );
        assert_eq!(story.entries[1].duration, Some(Duration::from_secs(21)));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://vod.sooplive.com/player/1/catchstory").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn lives_resolve_through_the_token_and_cdn_or_say_why_not() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            LIVE_API,
            json!({"CHANNEL": {"RESULT": 1, "BJID": "khm11903", "BNO": "297314125", "TITLE": "봉준 종수 레전드", "BJNICK": "봉준", "BPWD": "N", "CDN": "gs_cdn", "RMD": "https://livestream-manager.sooplive.com", "GRADE": "0"}}).to_string(),
        ));
        fixture.exchanges.push(post(
            LIVE_API,
            json!({"CHANNEL": {"RESULT": 1, "AID": ".A32.token"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://livestream-manager.sooplive.com/broad_stream_assign.html?return_type=gcp_cdn&broad_key=297314125-common-master-hls",
            200,
            "application/json",
            json!({"result": "1", "view_url": "https://live-global-cdn-v02.sooplive.com/live-stmc-28/auth_master_playlist.m3u8", "stream_status": "create start"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://live-global-cdn-v02.sooplive.com/live-stmc-28/auth_master_playlist.m3u8?aid=.A32.token",
            200,
            "application/vnd.apple.mpegURL",
            "#EXTM3U\n#EXT-X-STREAM-INF:NAME=original,BANDWIDTH=8000000,RESOLUTION=1920x1080\nauth_playlist.m3u8?aid=.A32.token1\n#EXT-X-STREAM-INF:NAME=hd4k,BANDWIDTH=1382400,RESOLUTION=1280x720\nauth_playlist.m3u8?aid=.A32.token2\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://live-global-cdn-v02.sooplive.com/live-stmc-28/auth_playlist.m3u8?aid=.A32.token1",
            200,
            "application/vnd.apple.mpegURL",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://st.sooplive.com/api/get_station_status.php?szBjId=khm11903",
            200,
            "application/json",
            json!({"RESULT": 1, "DATA": {"user_id": "khm11903", "user_nick": "봉준", "station_title": "스타1 전프로게이머", "broad_start": "2026-09-22 19:59:31"}}).to_string(),
        ));
        fixture.exchanges.push(post(
            LIVE_API,
            json!({"CHANNEL": {"RESULT": 0, "geo_cc": "US"}}).to_string(),
        ));
        fixture.exchanges.push(post(
            LIVE_API,
            json!({"CHANNEL": {"RESULT": -6, "TITLE": "잔치국수"}}).to_string(),
        ));
        fixture.exchanges.push(post(
            LIVE_API,
            json!({"CHANNEL": {"RESULT": 1, "BJID": "locked", "BNO": "5", "BPWD": "Y"}})
                .to_string(),
        ));
        let resolver = SoopResolver::new(Http::replay(fixture));
        let url = Url::parse("https://play.sooplive.com/khm11903").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.id.as_deref(), Some("297314125"));
        assert_eq!(resolved.title.as_deref(), Some("봉준 종수 레전드"));
        assert_eq!(resolved.uploader.as_deref(), Some("봉준"));
        assert_eq!(
            resolved.uploaded_at,
            Some("2026-09-22T10:59:31Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://play.sooplive.com/khm11903/297314125"
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://liveimg.sooplive.com/m/297314125"
        );
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.live));
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-1080p"));
        assert_eq!(
            resolved.variants[0].query,
            vec![("aid".to_string(), ".A32.token".to_string())]
        );
        let offline = resolver
            .resolve(&Url::parse("https://play.sooplive.com/sleeper").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&offline, ResolveError::Unavailable { reason, .. } if reason == "sleeper is not streaming right now"),
            "{offline}"
        );
        let adult = resolver
            .resolve(&Url::parse("https://play.sooplive.com/smmms2002").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&adult, ResolveError::LoginRequired { platform, .. } if *platform == PLATFORM),
            "{adult}"
        );
        let locked = resolver
            .resolve(&Url::parse("https://play.sooplive.com/locked").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&locked, ResolveError::Unavailable { reason, .. } if reason.contains("password")),
            "{locked}"
        );
    }

    #[tokio::test]
    async fn stations_list_their_recordings_and_sessions_are_checked() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://st.sooplive.com/api/get_station_status.php?szBjId=khm11903",
            200,
            "application/json",
            json!({"RESULT": 1, "DATA": {"user_id": "khm11903", "user_nick": "봉준"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://chapi.sooplive.com/api/khm11903/vods/all?page=1&per_page=60&orderby=reg_date",
            200,
            "application/json",
            json!({"data": [
                {"title_no": 208023729, "title_name": "[클립]종수 슈퍼마리오 26 스테이지", "ucc": {"total_file_duration": 113013}},
                {"title_no": 207960709, "title_name": "봉준 8시 기상", "ucc": {"total_file_duration": 84713536}},
                {"title_name": "no number"}
            ], "meta": {"total": 10000, "per_page": 20}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://st.sooplive.com/api/get_station_status.php?szBjId=nosuchbjzzz123",
            200,
            "application/json",
            json!({"RESULT": 0}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://afevent2.sooplive.com/api/get_private_info.php",
            200,
            "application/json",
            json!({"CHANNEL": {"IS_LOGIN": 1, "LOGIN_ID": "nick", "LOGIN_NICK": "닉", "COUNTRY_CODE": "US"}}).to_string(),
        ));
        let http = Http::replay(fixture);
        let resolver = SoopResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut,
            "no cookies, no request"
        );
        let Resolution::Playlist(station) = resolver
            .resolve(&Url::parse("https://www.sooplive.com/station/khm11903/vod").unwrap())
            .await
            .unwrap()
        else {
            panic!("a station is a playlist");
        };
        assert_eq!(station.id.as_deref(), Some("khm11903-all"));
        assert_eq!(station.title.as_deref(), Some("봉준 - all"));
        assert_eq!(station.total, Some(10000));
        assert_eq!(station.entries.len(), 2);
        assert_eq!(
            station.entries[0].url.as_str(),
            "https://vod.sooplive.com/player/208023729"
        );
        assert_eq!(
            station.entries[0].duration,
            Some(Duration::from_millis(113013))
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://bj.afreecatv.com/nosuchbjzzz123").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("PdboxTicket", "ticket", "sooplive.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "nick".into()
            }
        );
    }

    /// Every example link resolves live: the recordings and the live to media with
    /// variants, the catch story and stations to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = SoopResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
                    assert!(playable > 0, "{link}: no playable variant");
                    println!(
                        "{link}: {:?} {playable} variants, live={} {:?}",
                        resolved.media, resolved.live, resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {} entries (total {:?}) {:?}",
                        playlist.entries.len(),
                        playlist.total,
                        playlist.title
                    );
                }
            }
        }
    }
}
