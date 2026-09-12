//! Playlists and channels: their videos listed through the `browse` endpoint, page after
//! page, and a channel's page turned into its uploads playlist.

use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

use super::PLATFORM;
use super::innertube::{InnerTube, WEB};
use crate::resolve::{Playlist, PlaylistEntry, ResolveError, clean_title};

/// How many pages of a playlist are read; the engine caps entries below that anyway.
const MAX_PAGES: usize = 10;

/// Every value under `key` anywhere in `value`, depth first.
pub fn walk<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                }
                walk(v, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, key, out);
            }
        }
        _ => {}
    }
}

fn text(value: &Value) -> Option<String> {
    value["simpleText"]
        .as_str()
        .map(String::from)
        .or_else(|| {
            value["runs"].as_array().map(|runs| {
                runs.iter()
                    .filter_map(|r| r["text"].as_str())
                    .collect::<String>()
            })
        })
        .and_then(|t| clean_title(&t))
}

fn entries_of(page: &Value, entries: &mut Vec<PlaylistEntry>) {
    let mut found = Vec::new();
    walk(page, "playlistVideoRenderer", &mut found);
    for video in found {
        if video["isPlayable"].as_bool() == Some(false) {
            continue;
        }
        let Some(id) = video["videoId"].as_str() else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("https://www.youtube.com/watch?v={id}")) else {
            continue;
        };
        if entries.iter().any(|e| e.url == url) {
            continue;
        }
        entries.push(PlaylistEntry {
            url,
            title: text(&video["title"]),
            duration: video["lengthSeconds"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .map(Duration::from_secs),
        });
    }
}

fn continuation_of(page: &Value) -> Option<String> {
    let mut found = Vec::new();
    walk(page, "continuationCommand", &mut found);
    found
        .into_iter()
        .find_map(|c| c["token"].as_str().map(String::from))
}

fn title_of(page: &Value) -> Option<String> {
    for key in [
        "playlistHeaderRenderer",
        "playlistMetadataRenderer",
        "pageHeaderRenderer",
    ] {
        let mut found = Vec::new();
        walk(page, key, &mut found);
        for header in found {
            if let Some(title) = header["title"]
                .as_str()
                .and_then(clean_title)
                .or_else(|| text(&header["title"]))
                .or_else(|| header["pageTitle"].as_str().and_then(clean_title))
            {
                return Some(title);
            }
        }
    }
    None
}

/// The count a header states, such as `1,234 videos`.
fn total_of(page: &Value) -> Option<usize> {
    let mut found = Vec::new();
    walk(page, "numVideosText", &mut found);
    walk(page, "videoCountText", &mut found);
    found.into_iter().find_map(|count| {
        let digits: String = text(count)?
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    })
}

/// The playlist `list_id`, its entries read page after page.
pub async fn resolve_playlist(
    tube: &InnerTube,
    list_id: &str,
    origin: &Url,
) -> Result<Playlist, ResolveError> {
    let mut page = tube
        .call(
            "browse",
            &WEB,
            json!({ "browseId": format!("VL{list_id}") }),
            origin,
        )
        .await?;
    let title = title_of(&page);
    let total = total_of(&page);
    let mut entries = Vec::new();
    entries_of(&page, &mut entries);
    let mut pages = 1;
    while let Some(token) = continuation_of(&page) {
        if pages >= MAX_PAGES {
            break;
        }
        page = tube
            .call("browse", &WEB, json!({ "continuation": token }), origin)
            .await?;
        let before = entries.len();
        entries_of(&page, &mut entries);
        pages += 1;
        if entries.len() == before {
            break;
        }
    }
    if entries.is_empty() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    Ok(Playlist {
        resolver: PLATFORM.to_string(),
        id: Some(list_id.to_string()),
        title,
        total: total.filter(|t| *t > entries.len()),
        entries,
    })
}

/// The uploads playlist of the channel at `url`: `UU` and the channel id's tail.
pub async fn channel_playlist(tube: &InnerTube, url: &Url) -> Result<String, ResolveError> {
    let mut segments = url.path_segments().into_iter().flatten();
    let channel_id = match segments.next() {
        Some("channel") => segments.next().map(String::from),
        _ => None,
    };
    let channel_id = match channel_id {
        Some(id) if id.starts_with("UC") => id,
        _ => {
            let answer = tube
                .call(
                    "navigation/resolve_url",
                    &WEB,
                    json!({ "url": url.as_str() }),
                    url,
                )
                .await?;
            let mut found = Vec::new();
            walk(&answer, "browseId", &mut found);
            found
                .into_iter()
                .filter_map(Value::as_str)
                .find(|id| id.starts_with("UC"))
                .map(String::from)
                .ok_or_else(|| ResolveError::NotFound(url.clone()))?
        }
    };
    Ok(format!("UU{}", &channel_id[2..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_read_for_videos_titles_counts_and_continuations() {
        let page = json!({
            "header": {"playlistHeaderRenderer": {"title": {"simpleText": "Best of"}, "numVideosText": {"runs": [{"text": "1,234"}, {"text": " videos"}]}}},
            "contents": [
                {"playlistVideoRenderer": {"videoId": "aaaaaaaaaaa", "title": {"runs": [{"text": "One"}]}, "lengthSeconds": "61", "isPlayable": true}},
                {"playlistVideoRenderer": {"videoId": "bbbbbbbbbbb", "title": {"simpleText": "[Private video]"}, "isPlayable": false}},
                {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "NEXT"}}}}
            ]
        });
        let mut entries = Vec::new();
        entries_of(&page, &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title.as_deref(), Some("One"));
        assert_eq!(entries[0].duration, Some(Duration::from_secs(61)));
        assert_eq!(title_of(&page).as_deref(), Some("Best of"));
        assert_eq!(total_of(&page), Some(1234));
        assert_eq!(continuation_of(&page).as_deref(), Some("NEXT"));
    }
}
