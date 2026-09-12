//! What a `player` answer says: whether the video plays, what it is, the formats it comes
//! in as variants, and its captions.

use std::sync::LazyLock;
use std::time::Duration;

use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::innertube::Client;
use super::player::Player;
use crate::media::Container;
use crate::resolve::{
    Resolved, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, essence,
};

/// What the API says about playing the video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playability {
    Ok,
    /// Signing in to confirm one's age is asked.
    AgeGated(String),
    /// Signing in is asked for another reason: a private or members-only video.
    LoginRequired(String),
    /// A premiere or stream yet to start, at the time when it is known.
    Upcoming(Option<Timestamp>),
    Unavailable(String),
}

pub fn playability(response: &Value) -> Playability {
    let status = response
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reason = response
        .pointer("/playabilityStatus/reason")
        .and_then(Value::as_str)
        .or_else(|| {
            response
                .pointer("/playabilityStatus/messages/0")
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_string();
    match status {
        "OK" => Playability::Ok,
        "AGE_VERIFICATION_REQUIRED" | "AGE_CHECK_REQUIRED" => Playability::AgeGated(reason),
        "LOGIN_REQUIRED" => {
            let lower = reason.to_lowercase();
            if lower.contains("age") || lower.contains("inappropriate") {
                Playability::AgeGated(reason)
            } else {
                Playability::LoginRequired(reason)
            }
        }
        "LIVE_STREAM_OFFLINE" => Playability::Upcoming(
            response
                .pointer("/playabilityStatus/liveStreamability/liveStreamabilityRenderer/offlineSlate/liveStreamOfflineSlateRenderer/scheduledStartTime")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|s| Timestamp::from_second(s).ok()),
        ),
        "" => Playability::Unavailable("the API answered without a playability status".into()),
        other => Playability::Unavailable(if reason.is_empty() {
            other.to_string()
        } else {
            reason
        }),
    }
}

fn parse_date(text: &str) -> Option<Timestamp> {
    if let Ok(timestamp) = text.parse::<Timestamp>() {
        return Some(timestamp);
    }
    text.parse::<jiff::civil::Date>()
        .ok()
        .and_then(|date| date.to_zoned(jiff::tz::TimeZone::UTC).ok())
        .map(|zoned| zoned.timestamp())
}

/// The video's details, into `resolved`.
pub fn details(response: &Value, resolved: &mut Resolved) {
    let details = &response["videoDetails"];
    resolved.id = details["videoId"].as_str().map(String::from);
    resolved.title = details["title"].as_str().and_then(clean_title);
    resolved.description = details["shortDescription"]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    resolved.uploader = details["author"].as_str().and_then(clean_title);
    resolved.uploader_url = details["channelId"]
        .as_str()
        .and_then(|c| Url::parse(&format!("https://www.youtube.com/channel/{c}")).ok());
    resolved.duration = details["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs);
    resolved.live = details["isLive"].as_bool().unwrap_or(false);
    resolved.thumbnail = details["thumbnail"]["thumbnails"]
        .as_array()
        .and_then(|list| list.iter().max_by_key(|t| t["width"].as_u64().unwrap_or(0)))
        .and_then(|t| t["url"].as_str())
        .and_then(|u| Url::parse(u).ok());
    let micro = &response["microformat"]["playerMicroformatRenderer"];
    resolved.uploaded_at = micro["publishDate"]
        .as_str()
        .or_else(|| micro["uploadDate"].as_str())
        .and_then(parse_date);
    if let Some(id) = &resolved.id {
        resolved.webpage_url = Url::parse(&format!("https://www.youtube.com/watch?v={id}")).ok();
    }
}

static RE_CODECS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"codecs="([^"]+)""#).unwrap());

/// Every format of the answer as a variant, its URL unlocked with `player` where the app
/// needs it; with why each format that could not be used was left out.
pub async fn variants(
    response: &Value,
    client: &Client,
    player: Option<&Player>,
) -> (Vec<Variant>, Vec<String>) {
    let streaming = &response["streamingData"];
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let listed = [("formats", false), ("adaptiveFormats", true)];
    for (key, adaptive) in listed {
        for format in streaming[key].as_array().into_iter().flatten() {
            let itag = format["itag"].as_u64().unwrap_or(0);
            if format["type"].as_str() == Some("FORMAT_STREAM_TYPE_OTF") {
                problems.push(format!(
                    "itag {itag} is served in a segmented form this app does not fetch"
                ));
                continue;
            }
            let mime = format["mimeType"].as_str().unwrap_or("");
            let kind = essence(Some(mime));
            let cipher = format["signatureCipher"]
                .as_str()
                .or_else(|| format["cipher"].as_str());
            let url = match (format["url"].as_str(), cipher, player) {
                (_, Some(cipher), Some(player)) => match player.unlock(None, Some(cipher)).await {
                    Ok(url) => url,
                    Err(error) => {
                        problems.push(format!("itag {itag}: {error}"));
                        continue;
                    }
                },
                (_, Some(_), None) => {
                    problems.push(format!(
                        "itag {itag} carries a signature cipher the {} app cannot decipher",
                        client.id
                    ));
                    continue;
                }
                (Some(url), None, Some(player)) => match player.unlock(Some(url), None).await {
                    Ok(url) => url,
                    Err(error) => {
                        problems.push(format!("itag {itag}: {error}"));
                        continue;
                    }
                },
                (Some(url), None, None) => match Url::parse(url) {
                    Ok(url) => url,
                    Err(error) => {
                        problems.push(format!("itag {itag}: {error}"));
                        continue;
                    }
                },
                (None, None, _) => {
                    problems.push(format!("itag {itag} has no URL"));
                    continue;
                }
            };
            let mut variant = Variant::new(url, VariantKind::File);
            variant.container = match kind.as_str() {
                "video/mp4" | "audio/mp4" => Some(Container::Mp4),
                "video/webm" | "audio/webm" => Some(Container::Webm),
                "video/3gpp" => Some(Container::Other("3gp".into())),
                _ => None,
            };
            if let Some(codecs) = RE_CODECS.captures(mime).map(|c| c[1].to_string()) {
                variant = variant.with_codecs(&codecs);
            }
            variant.width = format["width"].as_u64().map(|w| w as u32);
            variant.height = format["height"].as_u64().map(|h| h as u32);
            variant.fps = format["fps"].as_f64();
            variant.bitrate = format["bitrate"]
                .as_u64()
                .or_else(|| format["averageBitrate"].as_u64());
            variant.size = format["contentLength"]
                .as_str()
                .and_then(|s| s.parse().ok());
            variant.duration = format["approxDurationMs"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_millis);
            variant.format_id = Some(itag.to_string());
            variant.label = format["qualityLabel"]
                .as_str()
                .or_else(|| format["audioQuality"].as_str())
                .map(String::from);
            variant.language = format["audioTrack"]["id"]
                .as_str()
                .and_then(|id| id.split('.').next())
                .map(String::from);
            variant.video_only = adaptive && kind.starts_with("video/");
            variant.audio_only = kind.starts_with("audio/");
            variant.headers = vec![("user-agent".to_string(), client.user_agent.to_string())];
            variant.drm = format["drmFamilies"]
                .as_array()
                .and_then(|f| f.first())
                .and_then(Value::as_str)
                .map(|s| s.to_lowercase());
            out.push(variant);
        }
    }
    (out, problems)
}

/// The caption tracks as subtitles, in the `json3` timed text form.
pub fn captions(response: &Value) -> Vec<SubtitleTrack> {
    response
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|track| {
            let mut url = Url::parse(track["baseUrl"].as_str()?).ok()?;
            url.query_pairs_mut().append_pair("fmt", "json3");
            Some(SubtitleTrack {
                url,
                language: track["languageCode"].as_str().unwrap_or("und").to_string(),
                name: track["name"]["simpleText"]
                    .as_str()
                    .or_else(|| track["name"]["runs"][0]["text"].as_str())
                    .map(String::from),
                format: SubtitleFormat::Json3,
                auto: track["kind"].as_str() == Some("asr"),
                headers: Vec::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn playability_is_read_with_its_reasons() {
        assert_eq!(
            playability(&json!({"playabilityStatus": {"status": "OK"}})),
            Playability::Ok
        );
        assert_eq!(
            playability(
                &json!({"playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm your age"}})
            ),
            Playability::AgeGated("Sign in to confirm your age".into())
        );
        assert_eq!(
            playability(
                &json!({"playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "This is a private video"}})
            ),
            Playability::LoginRequired("This is a private video".into())
        );
        assert_eq!(
            playability(
                &json!({"playabilityStatus": {"status": "LIVE_STREAM_OFFLINE", "liveStreamability": {"liveStreamabilityRenderer": {"offlineSlate": {"liveStreamOfflineSlateRenderer": {"scheduledStartTime": "1700000000"}}}}}})
            ),
            Playability::Upcoming(Some(Timestamp::from_second(1_700_000_000).unwrap()))
        );
        assert_eq!(
            playability(
                &json!({"playabilityStatus": {"status": "UNPLAYABLE", "reason": "Video unavailable"}})
            ),
            Playability::Unavailable("Video unavailable".into())
        );
        assert!(matches!(
            playability(&json!({})),
            Playability::Unavailable(_)
        ));
    }

    #[test]
    fn details_and_captions_are_read() {
        let response = json!({
            "videoDetails": {
                "videoId": "jNQXAC9IVRw", "title": "  Me at the zoo ", "shortDescription": "The first video",
                "author": "jawed", "channelId": "UC4QobU6STFB0P71PMvOGN5A", "lengthSeconds": "19",
                "isLive": false,
                "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/small.jpg", "width": 120}, {"url": "https://i.ytimg.com/big.jpg", "width": 1280}]}
            },
            "microformat": {"playerMicroformatRenderer": {"publishDate": "2005-04-23"}},
            "captions": {"playerCaptionsTracklistRenderer": {"captionTracks": [
                {"baseUrl": "https://www.youtube.com/api/timedtext?v=jNQXAC9IVRw&lang=en", "languageCode": "en", "name": {"simpleText": "English"}, "kind": "asr"},
                {"baseUrl": "https://www.youtube.com/api/timedtext?v=jNQXAC9IVRw&lang=de", "languageCode": "de", "name": {"runs": [{"text": "German"}]}}
            ]}}
        });
        let mut resolved = Resolved::new("youtube");
        details(&response, &mut resolved);
        assert_eq!(resolved.title.as_deref(), Some("Me at the zoo"));
        assert_eq!(resolved.uploader.as_deref(), Some("jawed"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(19)));
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            "https://i.ytimg.com/big.jpg"
        );
        assert_eq!(
            resolved.webpage_url.unwrap().as_str(),
            "https://www.youtube.com/watch?v=jNQXAC9IVRw"
        );
        assert_eq!(
            resolved.uploaded_at,
            Some("2005-04-23T00:00:00Z".parse().unwrap())
        );
        let subtitles = captions(&response);
        assert_eq!(subtitles.len(), 2);
        assert!(subtitles[0].auto);
        assert!(subtitles[0].url.as_str().ends_with("&fmt=json3"));
        assert_eq!(subtitles[1].name.as_deref(), Some("German"));
        assert!(!subtitles[1].auto);
    }
}
