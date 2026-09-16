//! What most resolvers need beside HTTP and HTML: the number, date, duration and count
//! formats platforms write, JavaScript object literals turned into JSON, HTML text
//! cleaned, XML read, and the hashes, ciphers and encodings platform APIs sign with.

use std::time::Duration;

use aes::Aes128;
use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::{BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use jiff::civil::DateTime;
use jiff::fmt::strtime::BrokenDownTime;
use jiff::tz::Offset;
use jiff::{Timestamp, ToSpan};
use md5::{Digest as _, Md5};
use regex::Regex;
use serde_json::Value;
use sha2::Sha256;
use url::Url;

use super::page::balanced_end;

// ---------------------------------------------------------------------------------------
// Numbers and strings in JSON
// ---------------------------------------------------------------------------------------

/// An integer, whether the platform wrote it as a number or as a numeric string.
pub fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>()
                .ok()
                .or_else(|| s.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f as i64))
        }
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// [`int`], as an unsigned number when it is not negative.
pub fn uint(value: &Value) -> Option<u64> {
    int(value).and_then(|n| u64::try_from(n).ok())
}

/// [`uint`] within `u32`.
pub fn u32_of(value: &Value) -> Option<u32> {
    uint(value).and_then(|n| u32::try_from(n).ok())
}

/// A floating point number, whether written as a number or a numeric string.
pub fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// A non-empty trimmed string, or a number written out.
pub fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A boolean, taking `"true"`, `"1"`, `1` and their opposites too.
pub fn boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => Some(true),
            "false" | "0" | "no" | "n" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A duration in seconds, whether a number, a numeric string, or text such as `1:02:03`.
pub fn seconds(value: &Value) -> Option<Duration> {
    match value {
        Value::Number(_) => float(value)
            .filter(|f| *f >= 0.0)
            .map(Duration::from_secs_f64),
        Value::String(s) => parse_duration(s),
        _ => None,
    }
}

/// A duration in milliseconds, as a number or a numeric string.
pub fn millis(value: &Value) -> Option<Duration> {
    float(value)
        .filter(|f| *f >= 0.0)
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
}

/// A Unix time, in seconds or milliseconds (anything past the year 5138 in seconds is
/// taken as milliseconds), as a number or a numeric string.
pub fn epoch(value: &Value) -> Option<Timestamp> {
    let n = float(value)?;
    if n <= 0.0 {
        return None;
    }
    let seconds = if n >= 100_000_000_000.0 { n / 1000.0 } else { n };
    Timestamp::from_millisecond((seconds * 1000.0) as i64).ok()
}

/// A time written in any of the formats [`parse_timestamp`] reads, or as a Unix time.
pub fn time(value: &Value) -> Option<Timestamp> {
    match value {
        Value::Number(_) => epoch(value),
        Value::String(s) => parse_timestamp(s).or_else(|| epoch(value)),
        _ => None,
    }
}

/// A link, joined to `base` when relative; `//host/path` links get `https`.
pub fn url_of(value: &Value, base: Option<&Url>) -> Option<Url> {
    join_url(base, value.as_str()?)
}

/// `href` against `base`: absolute links as they are, scheme-relative ones on `https`,
/// relative ones joined; only `http` and `https` come back.
pub fn join_url(base: Option<&Url>, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    let parsed = if let Some(rest) = href.strip_prefix("//") {
        let scheme = base.map(Url::scheme).unwrap_or("https");
        Url::parse(&format!("{scheme}://{rest}")).ok()?
    } else {
        match Url::parse(href) {
            Ok(url) => url,
            Err(url::ParseError::RelativeUrlWithoutBase) => base?.join(href).ok()?,
            Err(_) => return None,
        }
    };
    matches!(parsed.scheme(), "http" | "https").then_some(parsed)
}

/// `url` without its last path segment, query or fragment: where relative links resolve.
pub fn base_url(url: &Url) -> Url {
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    if let Ok(mut segments) = base.path_segments_mut() {
        segments.pop();
        segments.push("");
    }
    base
}

/// The first value of query parameter `key` on `url`.
pub fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

/// `url` with `pairs` set in its query, replacing any of the same names.
pub fn with_query(url: &Url, pairs: &[(&str, &str)]) -> Url {
    let mut out = url.clone();
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !pairs.iter().any(|(name, _)| *name == k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    out.set_query(None);
    {
        let mut query = out.query_pairs_mut();
        for (k, v) in &kept {
            query.append_pair(k, v);
        }
        for (k, v) in pairs {
            query.append_pair(k, v);
        }
    }
    if out.query() == Some("") {
        out.set_query(None);
    }
    out
}

/// The first value under `key` anywhere in `value`: the shallowest match, and among
/// matches at one depth the first in key order.
pub fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut level: Vec<&'a Value> = vec![value];
    while !level.is_empty() {
        let mut next: Vec<&'a Value> = Vec::new();
        for item in level {
            match item {
                Value::Object(map) => {
                    if let Some(found) = map.get(key) {
                        return Some(found);
                    }
                    next.extend(map.values());
                }
                Value::Array(items) => next.extend(items.iter()),
                _ => {}
            }
        }
        level = next;
    }
    None
}

/// Every value under `key` anywhere in `value`, in document order.
pub fn find_keys<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut found = Vec::new();
    fn walk<'a>(value: &'a Value, key: &str, found: &mut Vec<&'a Value>) {
        match value {
            Value::Object(map) => {
                for (k, child) in map {
                    if k == key {
                        found.push(child);
                    }
                    walk(child, key, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|child| walk(child, key, found)),
            _ => {}
        }
    }
    walk(value, key, &mut found);
    found
}

/// The first object anywhere in `value` for which `accept` holds.
pub fn find_object<'a>(value: &'a Value, accept: &dyn Fn(&Value) -> bool) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if accept(value) {
                return Some(value);
            }
            map.values().find_map(|child| find_object(child, accept))
        }
        Value::Array(items) => items.iter().find_map(|child| find_object(child, accept)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Durations, counts, resolutions, ratings
// ---------------------------------------------------------------------------------------

/// Durations as platforms write them: `1:02:03`, `62:03`, `1:02:03.5`, `1d 2:03:04`,
/// `PT1H2M3S`, `1h 2m 3s`, `2 hours 5 minutes`, `90 min`, `1.5 hours`, `45 sec`, `3723`,
/// `3723.5`, with an optional trailing `Z`. Nothing negative.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.strip_suffix('Z').unwrap_or(s).trim();
    if let Some(total) = clock_duration(s)
        .or_else(|| unit_duration(s))
        .or_else(|| loose_duration(s))
    {
        return (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total));
    }
    None
}

/// `[[d:]h:]m:s[.ms]` and plain seconds; a last `:NNN` of three or more digits is a
/// fraction, as `1:02:03:500` names milliseconds.
fn clock_duration(s: &str) -> Option<f64> {
    let mut parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut fraction: Option<String> = None;
    let last = *parts.last()?;
    if let Some((whole, frac)) = last.split_once('.') {
        if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        fraction = Some(frac.to_string());
        let index = parts.len() - 1;
        parts[index] = whole;
    } else if parts.len() >= 2 && last.len() > 2 && last.chars().all(|c| c.is_ascii_digit()) {
        fraction = Some(last.to_string());
        parts.pop();
    }
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let factors: &[f64] = match parts.len() {
        1 => &[1.0],
        2 => &[60.0, 1.0],
        3 => &[3600.0, 60.0, 1.0],
        _ => &[86_400.0, 3600.0, 60.0, 1.0],
    };
    let mut total = 0f64;
    for (part, factor) in parts.iter().zip(factors) {
        total += part.parse::<f64>().ok()? * factor;
    }
    if let Some(fraction) = fraction {
        total += format!("0.{fraction}").parse::<f64>().ok()?;
    }
    Some(total)
}

/// `P1DT2H3M4S`, `1h2m3s`, `2 hours, 3 minutes`, `45 sec`.
fn unit_duration(s: &str) -> Option<f64> {
    let lower = s.to_ascii_lowercase();
    let mut rest = lower.trim_start_matches('p').trim();
    let mut total = 0f64;
    let mut matched = false;
    let units: &[(&[&str], f64)] = &[
        (&["years", "year", "yrs", "yr", "y"], 365.0 * 86_400.0),
        (&["months", "month", "mo"], 30.0 * 86_400.0),
        (&["weeks", "week", "wks", "wk", "w"], 7.0 * 86_400.0),
        (&["days", "day", "d"], 86_400.0),
        (&["hours", "hour", "hrs", "hr", "h"], 3600.0),
        (&["minutes", "minute", "mins", "min", "m"], 60.0),
        (&["seconds", "second", "secs", "sec", "s"], 1.0),
        (&["milliseconds", "millisecond", "ms"], 0.001),
    ];
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c == 't' || c.is_whitespace());
        if rest.is_empty() {
            break;
        }
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if digits.is_empty() {
            return None;
        }
        let number: f64 = digits.parse().ok()?;
        rest = rest[digits.len()..].trim_start();
        let mut unit_found = None;
        'units: for (names, factor) in units {
            for name in *names {
                if let Some(after) = rest.strip_prefix(name) {
                    // A unit ends where a letter does not follow; ISO 8601's `T` between
                    // the date and time parts (`P1DT2H`) counts as an end too.
                    let boundary = match after.chars().next() {
                        None => true,
                        Some(c) if !c.is_ascii_alphabetic() => true,
                        Some('t') => after[1..].chars().next().is_none_or(|d| d.is_ascii_digit()),
                        Some(_) => false,
                    };
                    if boundary {
                        unit_found = Some((*factor, after));
                        break 'units;
                    }
                }
            }
        }
        let (factor, after) = unit_found?;
        total += number * factor;
        matched = true;
        rest = after.trim_start_matches(|c: char| c == '.' || c == ',');
    }
    matched.then_some(total)
}

/// `1.5 hours`, `90 mins.`: one decimal number of hours or minutes.
fn loose_duration(s: &str) -> Option<f64> {
    let lower = s.to_ascii_lowercase();
    let (number, unit) = lower.split_once(char::is_whitespace)?;
    let n: f64 = number.trim().parse().ok()?;
    let unit = unit.trim().trim_end_matches('.');
    match unit {
        "hour" | "hours" => Some(n * 3600.0),
        "min" | "mins" | "minute" | "minutes" => Some(n * 60.0),
        _ => None,
    }
}

/// View, like and follower counts as pages print them: `1,234`, `1.2K`, `3M`, `1.5 B`,
/// `12 views`, `viewed 3.4k times`.
pub fn parse_count(text: &str) -> Option<u64> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    // Drop a leading word such as `Views:` or `viewed`.
    let s = match s.find(|c: char| c.is_ascii_digit()) {
        Some(index) => &s[index..],
        None => return None,
    };
    let number: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    let rest = s[number.len()..].trim_start();
    let unit = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>();
    let multiplier: Option<f64> = match unit.as_str() {
        "k" | "K" => Some(1e3),
        "m" | "M" | "kk" | "KK" => Some(1e6),
        "b" | "B" => Some(1e9),
        _ => None,
    };
    if let Some(multiplier) = multiplier {
        let n: f64 = number.replace(',', ".").parse().ok()?;
        return Some((n * multiplier).round() as u64);
    }
    number.replace([',', '.'], "").parse().ok()
}

/// `18`, `18+`, `PG-13`, `TV-MA`, `TV14`: the age a rating names.
pub fn parse_age_limit(text: &str) -> Option<u8> {
    let s = text.trim();
    let digits = s.strip_suffix('+').unwrap_or(s);
    if !digits.is_empty() && digits.len() <= 2 && digits.chars().all(|c| c.is_ascii_digit()) {
        return digits.parse().ok();
    }
    let upper = s.to_ascii_uppercase();
    match upper.as_str() {
        "G" => return Some(0),
        "PG" => return Some(10),
        "PG-13" => return Some(13),
        "R" => return Some(16),
        "NC" | "NC-17" => return Some(18),
        _ => {}
    }
    let tv = upper
        .strip_prefix("TV")
        .map(|rest| rest.trim_start_matches(['-', '_']))?;
    match tv {
        "Y" | "G" | "PG" => Some(0),
        "Y7" => Some(7),
        "14" => Some(14),
        "MA" => Some(17),
        _ => None,
    }
}

/// `1280x720`, `1280 × 720`, `720p`, `1080p60`, `4K`: `(width, height, fps)`.
pub fn parse_resolution(text: &str) -> (Option<u32>, Option<u32>, Option<f64>) {
    static BY_SIZE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?:^|[^a-zA-Z0-9])(\d{2,5})\s*[xX×,]\s*(\d{2,5})(?:[^a-zA-Z0-9]|$)").unwrap()
    });
    static BY_HEIGHT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?:^|[^a-zA-Z0-9])(\d{3,4})[pPiI](\d{2,3})?(?:[^a-zA-Z0-9]|$)").unwrap()
    });
    static BY_K: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\b([48])[kK](\d{2,3})?\b").unwrap());
    if let Some(caps) = BY_SIZE.captures(text) {
        return (caps[1].parse().ok(), caps[2].parse().ok(), None);
    }
    if let Some(caps) = BY_HEIGHT.captures(text) {
        let fps = caps.get(2).and_then(|m| m.as_str().parse::<f64>().ok());
        return (None, caps[1].parse().ok(), fps);
    }
    if let Some(caps) = BY_K.captures(text) {
        let fps = caps.get(2).and_then(|m| m.as_str().parse::<f64>().ok());
        return (None, caps[1].parse::<u32>().ok().map(|k| k * 540), fps);
    }
    (None, None, None)
}

/// The height a label such as `720p`, `hd1080`, `1080p60` or `4K` names.
pub fn height_of(text: &str) -> Option<u32> {
    parse_resolution(text).1.or_else(|| {
        static HD: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"(?i)(?:hd|sd|res)?_?(\d{3,4})(?:[^0-9]|$)").unwrap());
        HD.captures(text).and_then(|c| c[1].parse().ok())
    })
}

/// Bits per second from `2500 kbps`, `2.5 Mbps`, `2500k`, `800000`.
pub fn parse_bitrate(text: &str) -> Option<u64> {
    let s = text.trim().to_ascii_lowercase();
    let number: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let n: f64 = number.parse().ok()?;
    let unit = s[number.len()..].trim();
    let factor = if unit.starts_with('m') {
        1e6
    } else if unit.starts_with('k') {
        1e3
    } else {
        1.0
    };
    Some((n * factor).round() as u64)
}

/// A ranking over `order`, lowest first: `qualities(&["sd", "hd"])("hd") == 1`, unknown
/// names rank below every known one.
pub fn qualities<'a>(order: &'a [&'a str]) -> impl Fn(&str) -> i64 + 'a {
    move |name: &str| {
        order
            .iter()
            .position(|o| o.eq_ignore_ascii_case(name))
            .map_or(-1, |p| p as i64)
    }
}

// ---------------------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------------------

const TIMEZONE_NAMES: &[(&str, i32)] = &[
    ("UT", 0),
    ("UTC", 0),
    ("GMT", 0),
    ("Z", 0),
    ("AST", -4),
    ("ADT", -3),
    ("EST", -5),
    ("EDT", -4),
    ("CST", -6),
    ("CDT", -5),
    ("MST", -7),
    ("MDT", -6),
    ("PST", -8),
    ("PDT", -7),
];

const DATE_FORMATS: &[&str] = &[
    "%d %B %Y",
    "%d %b %Y",
    "%B %d %Y",
    "%b %d %Y",
    "%d %B %Y %H:%M:%S",
    "%d %b %Y %H:%M:%S",
    "%d %B %Y %H:%M",
    "%d %b %Y %H:%M",
    "%B %d %Y %H:%M:%S",
    "%b %d %Y %H:%M:%S",
    "%B %d %Y %H:%M",
    "%b %d %Y %H:%M",
    "%Y %m %d",
    "%Y-%m-%d",
    "%Y.%m.%d.",
    "%Y.%m.%d",
    "%Y/%m/%d",
    "%Y/%m/%d %H:%M",
    "%Y/%m/%d %H:%M:%S",
    "%Y-%m-%d %H:%M",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%d %H:%M:%S.%f",
    "%Y-%m-%d %H:%M:%S:%f",
    "%d.%m.%Y %H:%M",
    "%d.%m.%Y %H.%M",
    "%d.%m.%Y %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%dT%H:%M:%S.%f",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%dT%H",
    "%Y-%m",
    "%Y",
];

const DATE_FORMATS_DAY_FIRST: &[&str] = &[
    "%d-%m-%Y",
    "%d.%m.%Y",
    "%d.%m.%y",
    "%d/%m/%Y",
    "%d/%m/%y",
    "%d/%m/%Y %H:%M:%S",
    "%d/%m/%Y %H:%M",
    "%d-%m-%Y %H:%M",
    "%d-%m-%Y %H:%M:%S",
    "%H:%M %d/%m/%Y",
];

const DATE_FORMATS_MONTH_FIRST: &[&str] = &[
    "%m-%d-%Y",
    "%m.%d.%Y",
    "%m/%d/%Y",
    "%m/%d/%y",
    "%m/%d/%Y %H:%M:%S",
    "%m/%d/%Y %H:%M",
];

/// A time written the way platforms write them, as [`unified_timestamp`] in yt-dlp reads
/// them: ISO 8601 and RFC 3339 with or without a zone, RFC 2822, `2 January 2026`,
/// `Jan 2, 2026 10:30 PM EST`, `2026-01-02 10:30:00`, `20260102`, `02.01.2026`, and a
/// Unix time in seconds or milliseconds. Day-first for ambiguous `02/01/2026`.
pub fn parse_timestamp(text: &str) -> Option<Timestamp> {
    parse_timestamp_with(text, true)
}

/// [`parse_timestamp`] reading ambiguous `01/02/2026` as month first.
pub fn parse_timestamp_month_first(text: &str) -> Option<Timestamp> {
    parse_timestamp_with(text, false)
}

fn parse_timestamp_with(text: &str, day_first: bool) -> Option<Timestamp> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return match raw.len() {
            8 => civil("%Y%m%d", &format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..8])),
            12 => civil(
                "%Y%m%d%H%M",
                &format!("{}-{}-{} {}:{}", &raw[..4], &raw[4..6], &raw[6..8], &raw[8..10], &raw[10..12]),
            ),
            14 => civil(
                "%Y%m%d%H%M%S",
                &format!(
                    "{}-{}-{} {}:{}:{}",
                    &raw[..4],
                    &raw[4..6],
                    &raw[6..8],
                    &raw[8..10],
                    &raw[10..12],
                    &raw[12..14]
                ),
            ),
            _ => epoch(&Value::String(raw.to_string())),
        };
    }
    if let Ok(ts) = raw.parse::<Timestamp>() {
        return Some(ts);
    }
    let mut s = strip_weekday(raw);
    let pm = {
        static AMPM: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"(?i)\b(AM|PM)\b").unwrap());
        let meridiem = AMPM.captures(&s).map(|c| c[1].to_ascii_uppercase());
        s = AMPM.replace_all(&s, "").to_string();
        meridiem
    };
    let (offset, without_zone) = extract_timezone(&s);
    s = collapse_space(&without_zone);
    // Nanoseconds beyond what a fraction can carry are cut.
    let mut candidate = s.clone();
    strip_ordinals(&mut candidate);
    let formats = DATE_FORMATS
        .iter()
        .chain(if day_first { DATE_FORMATS_DAY_FIRST } else { DATE_FORMATS_MONTH_FIRST }.iter());
    for format in formats {
        if let Some(mut dt) = strptime(format, &candidate) {
            if let Some(meridiem) = &pm {
                let hour = dt.hour();
                let adjusted = match (meridiem.as_str(), hour) {
                    ("PM", h) if h < 12 => Some(h + 12),
                    ("AM", 12) => Some(0),
                    _ => None,
                };
                if let Some(h) = adjusted {
                    dt = dt.with().hour(h).build().ok()?;
                }
            }
            return offset.unwrap_or(Offset::UTC).to_timestamp(dt).ok();
        }
    }
    None
}

/// A civil date-time in UTC read with `format`; the numeric shapes go through here.
fn civil(_shape: &str, normalized: &str) -> Option<Timestamp> {
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d"] {
        if let Some(dt) = strptime(format, normalized) {
            return Offset::UTC.to_timestamp(dt).ok();
        }
    }
    None
}

fn strptime(format: &str, input: &str) -> Option<DateTime> {
    let broken = BrokenDownTime::parse(format, input).ok()?;
    match broken.to_datetime() {
        Ok(dt) => Some(dt),
        Err(_) => {
            // A date without a time of day.
            let date = broken.to_date().ok()?;
            Some(date.to_datetime(jiff::civil::Time::midnight()))
        }
    }
}

fn strip_weekday(s: &str) -> String {
    static WEEKDAY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)[,|]|\b(?:mon|tues?|wed(?:nes)?|thu(?:rs)?|fri|sat(?:ur)?|sun)(?:day)?\b\.?")
            .unwrap()
    });
    let replaced = WEEKDAY.replace_all(s, " ");
    let replaced = replaced.replace(" at ", " ");
    collapse_space(&replaced)
}

fn collapse_space(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `2nd` → `2`, `21st` → `21`, before a month or a year.
fn strip_ordinals(s: &mut String) {
    static ORDINAL: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\b(\d{1,2})(?:st|nd|rd|th)\b").unwrap());
    *s = ORDINAL.replace_all(s, "$1").to_string();
}

/// The zone at the end of a date, as an offset, and the date without it.
fn extract_timezone(s: &str) -> (Option<Offset>, String) {
    static NUMERIC: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?:\d{4}|\d\d:\d\d(?:\.\d+)?|\d\d:\d\d:\d\d(?:\.\d+)?)\s?(Z|[+-]\d{2}:?\d{2})$").unwrap()
    });
    static NAMED: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\d{1,2}:\d{1,2}(?:\.\d+)?(\s*[A-Z]+)$").unwrap());
    let trimmed = s.trim();
    if trimmed.len() >= 8
        && let Some(caps) = NUMERIC.captures(trimmed)
    {
        let zone = caps.get(1).unwrap();
        let rest = trimmed[..zone.start()].trim_end().to_string();
        if zone.as_str() == "Z" {
            return (Some(Offset::UTC), rest);
        }
        let z = zone.as_str();
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let digits: String = z[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        let hours: i32 = digits[..2].parse().unwrap_or(0);
        let minutes: i32 = digits[2..4].parse().unwrap_or(0);
        let offset = Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok();
        return (offset, rest);
    }
    if let Some(caps) = NAMED.captures(trimmed) {
        let zone = caps.get(1).unwrap();
        let name = zone.as_str().trim();
        if let Some((_, hours)) = TIMEZONE_NAMES.iter().find(|(n, _)| *n == name) {
            let rest = trimmed[..zone.start()].trim_end().to_string();
            return (Offset::from_hours(*hours as i8).ok(), rest);
        }
        // An unknown zone name is dropped so the time still reads.
        let rest = trimmed[..zone.start()].trim_end().to_string();
        return (None, rest);
    }
    (None, trimmed.to_string())
}

/// `2026-01-02` as a timestamp at midnight UTC, from a date alone.
pub fn parse_date(text: &str) -> Option<Timestamp> {
    parse_timestamp(text)
}

/// A timestamp `secs` in the future, for `expires_in` answers.
pub fn from_now(secs: i64) -> Timestamp {
    Timestamp::now()
        .checked_add(secs.seconds())
        .unwrap_or_else(|_| Timestamp::now())
}

// ---------------------------------------------------------------------------------------
// JavaScript object literals
// ---------------------------------------------------------------------------------------

/// Turns a JavaScript object or array literal into JSON, as yt-dlp's `js_to_json`:
/// single, double and template quoted strings, unquoted and numeric keys, hex and octal
/// numbers, `undefined` and `void 0` as null, trailing commas, comments, `!0`/`!1`,
/// `new Date("…")`, `Array(…)`, `parseInt("…", 10)`, and bare identifiers as strings.
pub fn js_to_json(code: &str) -> String {
    let code = pre_transform(code);
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Comments.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i = code[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i = code[i..].find('\n').map_or(bytes.len(), |e| i + e + 1);
            continue;
        }
        // Strings.
        if c == b'"' || c == b'\'' || c == b'`' {
            let end = string_end(&code, i);
            let body = &code[i + 1..end.min(code.len()).saturating_sub(1).max(i + 1)];
            out.push('"');
            out.push_str(&escape_js_string(body, c == b'`'));
            out.push('"');
            i = end;
            continue;
        }
        // Trailing commas before a closing bracket.
        if c == b',' {
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j].is_ascii_whitespace() {
                    j += 1;
                } else if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                    j = code[j + 2..].find("*/").map_or(bytes.len(), |e| j + 2 + e + 2);
                } else if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                    j = code[j..].find('\n').map_or(bytes.len(), |e| j + e + 1);
                } else {
                    break;
                }
            }
            if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'}') {
                i += 1;
                continue;
            }
            out.push(',');
            i += 1;
            continue;
        }
        if c == b'!' {
            i += 1;
            continue;
        }
        // Numbers and identifiers.
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c.is_ascii_digit() {
            let start = i;
            let mut j = i;
            let is_number = c.is_ascii_digit();
            if is_number {
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'.') {
                    j += 1;
                }
            } else {
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'_' | b'$' | b'.'))
                {
                    j += 1;
                }
            }
            let word = &code[start..j];
            let followed_by_colon = {
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                k < bytes.len() && bytes[k] == b':'
            };
            let preceded_by_dot = start > 0 && bytes[start - 1] == b'.';
            if is_number {
                if let Some(n) = js_integer(word) {
                    if followed_by_colon {
                        out.push_str(&format!("\"{n}\""));
                    } else {
                        out.push_str(&n.to_string());
                    }
                } else if followed_by_colon {
                    out.push_str(&format!("\"{word}\""));
                } else {
                    out.push_str(word);
                }
            } else if preceded_by_dot && !followed_by_colon {
                out.push_str(word);
            } else {
                match word {
                    "true" | "false" | "null" => out.push_str(word),
                    "undefined" | "NaN" | "Infinity" => out.push_str("null"),
                    "void" => {
                        // `void 0`
                        let mut k = j;
                        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        while k < bytes.len() && bytes[k].is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                        out.push_str("null");
                    }
                    _ => {
                        out.push('"');
                        out.push_str(word);
                        out.push('"');
                    }
                }
            }
            i = j;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// The transformations yt-dlp applies before tokenising.
fn pre_transform(code: &str) -> String {
    static ARRAY: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?:new\s+)?Array\(([^()]*)\)").unwrap());
    static DATE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"new\s+Date\(\s*("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')\s*\)"#).unwrap()
    });
    static PARSE_INT: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"parseInt\([^\d]+(\d+)[^\d]+\)").unwrap());
    static NEW: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"new\s+\w+\(([^()]*)\)").unwrap());
    let code = ARRAY.replace_all(code, "[$1]");
    let code = DATE.replace_all(&code, "$1");
    let code = PARSE_INT.replace_all(&code, "$1");
    let code = NEW.replace_all(&code, |caps: &regex::Captures| {
        serde_json::to_string(&caps[0]).unwrap_or_default()
    });
    code.into_owned()
}

/// The index just past the string literal that starts at `start`.
fn string_end(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// A JavaScript string body as a JSON string body.
fn escape_js_string(body: &str, template: bool) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('x') => out.push_str("\\u00"),
                Some('\n') => {}
                Some(e @ ('"' | '\\' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                    out.push('\\');
                    out.push(e);
                }
                Some('/') => out.push('/'),
                Some(other) => out.push(other),
                None => {}
            },
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if template && chars.peek() == Some(&'{') => {
                chars.next();
                let mut expr = String::new();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    expr.push(inner);
                }
                let evaluated = js_to_json(&expr);
                match serde_json::from_str::<Value>(&evaluated) {
                    Ok(Value::String(s)) => out.push_str(&escape_js_string(&s, false)),
                    Ok(other) => out.push_str(&other.to_string()),
                    Err(_) => out.push_str(&evaluated),
                }
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Hex and leading-zero octal integers as JavaScript reads them.
fn js_integer(word: &str) -> Option<i64> {
    if let Some(hex) = word.strip_prefix("0x").or_else(|| word.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok();
    }
    if word.len() > 1 && word.starts_with('0') && word.chars().all(|c| ('0'..='7').contains(&c)) {
        return i64::from_str_radix(word.trim_start_matches('0'), 8).ok().or(Some(0));
    }
    None
}

/// The byte length of the balanced `{…}` or `[…]` that JavaScript `text` begins with,
/// with single, double and template quoted strings and comments respected.
pub fn balanced_js_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let open = *bytes.first()?;
    let close = match open {
        b'{' => b'}',
        b'[' => b']',
        b'(' => b')',
        _ => return None,
    };
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' | b'`' => {
                i = string_end(text, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = text[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i = text[i..].find('\n').map_or(bytes.len(), |e| i + e + 1);
                continue;
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return (b == close).then_some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The JavaScript object or array literal that follows `prefix` in `text`, as JSON.
pub fn js_object_after(text: &str, prefix: &str) -> Option<Value> {
    let index = text.find(prefix)?;
    let after = text[index + prefix.len()..].trim_start();
    let end = balanced_js_end(after)?;
    parse_js(&after[..end])
}

/// A JavaScript object or array literal as JSON, taking plain JSON as it is.
pub fn parse_js(code: &str) -> Option<Value> {
    let code = code.trim();
    if let Ok(value) = serde_json::from_str::<Value>(code) {
        return Some(value);
    }
    serde_json::from_str(&js_to_json(code)).ok()
}

/// The JSON value that follows `prefix`, whichever of `prefixes` is found first in the
/// text; for pages that name their state under several variables.
pub fn json_after_any(text: &str, prefixes: &[&str]) -> Option<Value> {
    prefixes
        .iter()
        .filter_map(|prefix| text.find(prefix).map(|at| (at, *prefix)))
        .min_by_key(|(at, _)| *at)
        .and_then(|(at, prefix)| {
            let after = text[at + prefix.len()..].trim_start();
            let end = balanced_end(after).or_else(|| balanced_js_end(after))?;
            parse_js(&after[..end])
        })
}

/// The text captured by the first group of `re` in `text`.
pub fn search(re: &Regex, text: &str) -> Option<String> {
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// The text captured by the first group of the first of `patterns` that matches.
pub fn search_any(patterns: &[&Regex], text: &str) -> Option<String> {
    patterns.iter().find_map(|re| search(re, text))
}

// ---------------------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------------------

/// `&amp;`, `&#39;`, `&#x27;` and the common named entities, decoded.
pub fn html_unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[1..].find(';').map(|e| e + 1) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        if entity.len() > 32 || entity.contains(char::is_whitespace) {
            out.push('&');
            rest = &rest[1..];
            continue;
        }
        let decoded: Option<String> = if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(String::from)
        } else if let Some(dec) = entity.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32).map(String::from)
        } else {
            named_entity(entity).map(String::from)
        };
        match decoded {
            Some(s) => {
                out.push_str(&s);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "middot" => "·",
        "bull" => "•",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "deg" => "°",
        "times" => "×",
        "eacute" => "é",
        "egrave" => "è",
        "agrave" => "à",
        "aacute" => "á",
        "ccedil" => "ç",
        "ouml" => "ö",
        "auml" => "ä",
        "uuml" => "ü",
        "Ouml" => "Ö",
        "Auml" => "Ä",
        "Uuml" => "Ü",
        "szlig" => "ß",
        "ntilde" => "ñ",
        "iacute" => "í",
        "oacute" => "ó",
        "uacute" => "ú",
        _ => return None,
    })
}

/// The text of an HTML fragment: tags removed, entities decoded, whitespace collapsed.
pub fn clean_html(html: &str) -> String {
    let with_breaks = {
        static BREAKS: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"(?i)<\s*(?:br|/p|/div|/li|/h\d)\s*/?>").unwrap());
        BREAKS.replace_all(html, "\n")
    };
    let stripped = {
        static TAGS: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
        TAGS.replace_all(&with_breaks, "")
    };
    let decoded = html_unescape(&stripped);
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The attributes of one HTML start tag, lower-cased names, entities decoded.
pub fn extract_attributes(tag: &str) -> Vec<(String, String)> {
    static ATTR: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?s)([a-zA-Z_:][-a-zA-Z0-9_:.]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#)
            .unwrap()
    });
    let inner = tag
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/');
    let body = match inner.find(char::is_whitespace) {
        Some(at) => &inner[at..],
        None => return Vec::new(),
    };
    ATTR.captures_iter(body)
        .map(|caps| {
            let value = caps
                .get(2)
                .or_else(|| caps.get(3))
                .or_else(|| caps.get(4))
                .map(|m| html_unescape(m.as_str()))
                .unwrap_or_default();
            (caps[1].to_ascii_lowercase(), value)
        })
        .collect()
}

/// The value of `name` among a tag's attributes.
pub fn attribute(tag: &str, name: &str) -> Option<String> {
    extract_attributes(tag)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// Every `<tag …>` start tag in `html` whose attributes satisfy `accept`, as raw tag text.
pub fn tags_where(html: &str, tag: &str, accept: &dyn Fn(&[(String, String)]) -> bool) -> Vec<String> {
    let re = Regex::new(&format!(r"(?is)<{}(?:\s[^>]*)?>", regex::escape(tag))).expect("valid");
    re.find_iter(html)
        .map(|m| m.as_str().to_string())
        .filter(|t| accept(&extract_attributes(t)))
        .collect()
}

/// The inner HTML of the first `<tag …>` whose attribute `attr` is `value`.
pub fn element_by_attribute(html: &str, tag: &str, attr: &str, value: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?is)<{tag}\b[^>]*\b{attr}\s*=\s*["']?{value}["'\s>][^>]*>(.*?)</{tag}\s*>"#,
        tag = regex::escape(tag),
        attr = regex::escape(attr),
        value = regex::escape(value)
    ))
    .ok()?;
    search(&re, html)
}

/// The inner HTML of the first element whose `id` is `id`.
pub fn element_by_id(html: &str, id: &str) -> Option<String> {
    static ANY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?is)<([a-zA-Z][a-zA-Z0-9]*)\b[^>]*\bid\s*=\s*["']?([^"'\s>]+)["'\s>]"#).unwrap()
    });
    for caps in ANY.captures_iter(html) {
        if &caps[2] == id {
            return element_by_attribute(html, &caps[1], "id", id);
        }
    }
    None
}

/// The inner HTML of the first element carrying class `class`.
pub fn element_by_class(html: &str, class: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?is)<([a-zA-Z][a-zA-Z0-9]*)\b[^>]*\bclass\s*=\s*["'][^"']*\b{}\b[^"']*["'][^>]*>"#,
        regex::escape(class)
    ))
    .ok()?;
    let caps = re.captures(html)?;
    let tag = caps[1].to_string();
    let start = caps.get(0)?.end();
    let close = format!("</{tag}");
    let end = html[start..].find(&close)? + start;
    Some(html[start..end].to_string())
}

// ---------------------------------------------------------------------------------------
// XML
// ---------------------------------------------------------------------------------------

/// Parses XML leniently: a byte order mark or leading whitespace is fine.
pub fn xml(text: &str) -> Option<roxmltree::Document<'_>> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    roxmltree::Document::parse(trimmed).ok()
}

/// The text of the first child of `node` named `name`, trimmed.
pub fn xml_child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The first descendant of `node` named `name`.
pub fn xml_find<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

/// Every descendant of `node` named `name`.
pub fn xml_find_all<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Vec<roxmltree::Node<'a, 'a>> {
    node.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == name)
        .collect()
}

// ---------------------------------------------------------------------------------------
// Hashes, ciphers, encodings
// ---------------------------------------------------------------------------------------

pub fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

pub fn sha1_hex(data: &[u8]) -> String {
    sha1_smol::Sha1::from(data).digest().to_string()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

/// HMAC-SHA256 over `message` with `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key_block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner_hash);
    outer.finalize().into()
}

/// HMAC-SHA1 over `message` with `key`.
pub fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..20].copy_from_slice(&sha1_smol::Sha1::from(key).digest().bytes());
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = sha1_smol::Sha1::new();
    inner.update(&key_block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(message);
    let inner_hash = inner.digest().bytes();
    let mut outer = sha1_smol::Sha1::new();
    outer.update(&key_block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(&inner_hash);
    outer.digest().bytes()
}

/// HMAC-MD5 over `message` with `key`.
pub fn hmac_md5(key: &[u8], message: &[u8]) -> [u8; 16] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..16].copy_from_slice(&Md5::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Md5::new();
    inner.update(key_block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Md5::new();
    outer.update(key_block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner_hash);
    outer.finalize().into()
}

/// AES-128-CBC with PKCS#7 padding removed.
pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(key, iv).ok()?;
    decryptor.decrypt_padded_vec::<Pkcs7>(data).ok()
}

/// AES-128-CBC without padding removal.
pub fn aes_cbc_decrypt_raw(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(key, iv).ok()?;
    decryptor.decrypt_padded_vec::<NoPadding>(data).ok()
}

/// AES-128-ECB, block by block, no padding.
pub fn aes_ecb_decrypt(key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new_from_slice(key).ok()?;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = aes::Block::default();
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    Some(out)
}

/// PKCS#7 padding removed from `data`, when it is well formed.
pub fn unpad_pkcs7(data: &[u8]) -> Option<Vec<u8>> {
    let last = *data.last()? as usize;
    if last == 0 || last > 16 || last > data.len() {
        return None;
    }
    if !data[data.len() - last..].iter().all(|b| *b as usize == last) {
        return None;
    }
    Some(data[..data.len() - last].to_vec())
}

pub fn b64_encode(data: &[u8]) -> String {
    STANDARD.encode(data)
}

pub fn b64_encode_url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Base64 in any alphabet, padded or not, whitespace ignored.
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let cleaned = cleaned.trim_end_matches('=');
    let url_alphabet = cleaned.contains(['-', '_']);
    if url_alphabet {
        URL_SAFE_NO_PAD
            .decode(cleaned)
            .ok()
            .or_else(|| URL_SAFE.decode(text.trim()).ok())
    } else {
        STANDARD_NO_PAD
            .decode(cleaned)
            .ok()
            .or_else(|| STANDARD.decode(text.trim()).ok())
    }
}

/// `len` random lower-case letters and digits.
pub fn random_alphanumeric(len: usize) -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// `len` random hex digits.
pub fn random_hex(len: usize) -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// A random UUID v4.
pub fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Percent-encodes everything but unreserved characters, as `encodeURIComponent`.
pub fn url_encode(text: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(text, SET).to_string()
}

pub fn url_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// `a=1&b=2` pairs, decoded.
pub fn parse_query(text: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(text.trim_start_matches('?').as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// `pairs` as `a=1&b=2`, encoded.
pub fn encode_query(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_come_from_numbers_and_strings() {
        assert_eq!(int(&json!(42)), Some(42));
        assert_eq!(int(&json!("42")), Some(42));
        assert_eq!(int(&json!(" 42.9 ")), Some(42));
        assert_eq!(int(&json!(true)), Some(1));
        assert_eq!(int(&json!("x")), None);
        assert_eq!(uint(&json!(-1)), None);
        assert_eq!(float(&json!("1.5")), Some(1.5));
        assert_eq!(text(&json!("  hi ")), Some("hi".into()));
        assert_eq!(text(&json!("  ")), None);
        assert_eq!(text(&json!(7)), Some("7".into()));
        assert_eq!(boolean(&json!("true")), Some(true));
        assert_eq!(boolean(&json!(0)), Some(false));
        assert_eq!(seconds(&json!(90.5)), Some(Duration::from_secs_f64(90.5)));
        assert_eq!(seconds(&json!("1:30")), Some(Duration::from_secs(90)));
        assert_eq!(millis(&json!("1500")), Some(Duration::from_millis(1500)));
        assert_eq!(epoch(&json!(1_700_000_000)).unwrap().as_second(), 1_700_000_000);
        assert_eq!(epoch(&json!("1700000000123")).unwrap().as_millisecond(), 1_700_000_000_123);
        assert_eq!(time(&json!("2024-01-02T03:04:05Z")).unwrap().as_second(), 1_704_164_645);
        assert_eq!(time(&json!(1_704_164_645)).unwrap().as_second(), 1_704_164_645);
    }

    #[test]
    fn links_join_and_queries_read() {
        let base = Url::parse("https://site.test/dir/page.html?x=1").unwrap();
        assert_eq!(
            url_of(&json!("//cdn.test/v.mp4"), Some(&base)).unwrap().as_str(),
            "https://cdn.test/v.mp4"
        );
        assert_eq!(
            join_url(Some(&base), "../v.mp4").unwrap().as_str(),
            "https://site.test/v.mp4"
        );
        assert_eq!(join_url(None, "ftp://x/y"), None);
        assert_eq!(join_url(None, "relative"), None);
        assert_eq!(base_url(&base).as_str(), "https://site.test/dir/");
        assert_eq!(query_param(&base, "x").as_deref(), Some("1"));
        assert_eq!(query_param(&base, "y"), None);
        let changed = with_query(&base, &[("x", "2"), ("y", "a b")]);
        assert_eq!(changed.query(), Some("x=2&y=a+b"));
        let doc = json!({"a": {"b": [{"c": 1}, {"c": 2}]}, "c": 3});
        assert_eq!(find_key(&doc, "c"), Some(&json!(3)));
        assert_eq!(find_key(&doc["a"], "c"), Some(&json!(1)));
        assert_eq!(find_keys(&doc, "c").len(), 3);
        assert_eq!(
            find_object(&doc, &|v| v.get("c") == Some(&json!(2))),
            Some(&json!({"c": 2}))
        );
    }

    #[test]
    fn durations_in_every_shape() {
        let secs = |s: &str| parse_duration(s).map(|d| d.as_secs_f64());
        assert_eq!(secs("1:02:03"), Some(3723.0));
        assert_eq!(secs("62:03"), Some(3723.0));
        assert_eq!(secs("1:02:03.5"), Some(3723.5));
        assert_eq!(secs("1:02:03:500"), Some(3723.5));
        assert_eq!(secs("1:01:02:03"), Some(90123.0));
        assert_eq!(secs("3723"), Some(3723.0));
        assert_eq!(secs("3723.5"), Some(3723.5));
        assert_eq!(secs("PT1H2M3S"), Some(3723.0));
        assert_eq!(secs("PT1H"), Some(3600.0));
        assert_eq!(secs("P1DT2H"), Some(93600.0));
        assert_eq!(secs("1h2m3s"), Some(3723.0));
        assert_eq!(secs("1 hour 2 minutes 3 seconds"), Some(3723.0));
        assert_eq!(secs("2 hours, 5 mins"), Some(7500.0));
        assert_eq!(secs("90 min"), Some(5400.0));
        assert_eq!(secs("1.5 hours"), Some(5400.0));
        assert_eq!(secs("45 sec"), Some(45.0));
        assert_eq!(secs("45s"), Some(45.0));
        assert_eq!(secs("2m30s"), Some(150.0));
        assert_eq!(secs("3 days"), Some(259200.0));
        assert_eq!(secs("00:00:35Z"), Some(35.0));
        assert_eq!(secs(""), None);
        assert_eq!(secs("soon"), None);
        assert_eq!(secs("1:xx"), None);
    }

    #[test]
    fn counts_ratings_resolutions_and_bitrates() {
        assert_eq!(parse_count("1,234"), Some(1234));
        assert_eq!(parse_count("1.2K"), Some(1200));
        assert_eq!(parse_count("3M"), Some(3_000_000));
        assert_eq!(parse_count("1.5 B"), Some(1_500_000_000));
        assert_eq!(parse_count("12 views"), Some(12));
        assert_eq!(parse_count("Views: 3.4k"), Some(3400));
        assert_eq!(parse_count("none"), None);
        assert_eq!(parse_age_limit("18"), Some(18));
        assert_eq!(parse_age_limit("18+"), Some(18));
        assert_eq!(parse_age_limit("PG-13"), Some(13));
        assert_eq!(parse_age_limit("TV-MA"), Some(17));
        assert_eq!(parse_age_limit("tv14"), Some(14));
        assert_eq!(parse_age_limit("weird"), None);
        assert_eq!(parse_resolution("1280x720"), (Some(1280), Some(720), None));
        assert_eq!(parse_resolution("res 1920 × 1080 hd"), (Some(1920), Some(1080), None));
        assert_eq!(parse_resolution("1080p60"), (None, Some(1080), Some(60.0)));
        assert_eq!(parse_resolution("4K"), (None, Some(2160), None));
        assert_eq!(parse_resolution("hd"), (None, None, None));
        assert_eq!(height_of("hd1080"), Some(1080));
        assert_eq!(height_of("720p"), Some(720));
        assert_eq!(parse_bitrate("2500 kbps"), Some(2_500_000));
        assert_eq!(parse_bitrate("2.5 Mbps"), Some(2_500_000));
        assert_eq!(parse_bitrate("800000"), Some(800_000));
        let rank = qualities(&["sd", "hd", "fhd"]);
        assert!(rank("FHD") > rank("hd"));
        assert_eq!(rank("x"), -1);
    }

    #[test]
    fn dates_in_every_shape() {
        let at = |s: &str| parse_timestamp(s).map(|t| t.to_string());
        assert_eq!(at("2024-01-02T03:04:05Z").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("2024-01-02T03:04:05.123+02:00").as_deref(), Some("2024-01-02T01:04:05.123Z"));
        assert_eq!(at("2024-01-02T03:04:05").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("2024-01-02 03:04:05").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("2024-01-02 03:04").as_deref(), Some("2024-01-02T03:04:00Z"));
        assert_eq!(at("2024-01-02").as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(at("20240102").as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(at("20240102030405").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("2 January 2024").as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(at("January 2, 2024").as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(at("Jan 2nd, 2024 10:30 PM EST").as_deref(), Some("2024-01-03T03:30:00Z"));
        assert_eq!(at("Tue, 02 Jan 2024 03:04:05 GMT").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("Tue, 02 Jan 2024 03:04:05 +0100").as_deref(), Some("2024-01-02T02:04:05Z"));
        assert_eq!(at("02.01.2024").as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(at("02/01/2024 03:04:05").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(
            parse_timestamp_month_first("01/02/2024").map(|t| t.to_string()).as_deref(),
            Some("2024-01-02T00:00:00Z")
        );
        assert_eq!(at("1704164645").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(at("2024-01-02T03:04:05.123456789Z").as_deref(), Some("2024-01-02T03:04:05.123456789Z"));
        assert_eq!(at("12:30 AM 2024-01-02"), None);
        assert_eq!(at("someday"), None);
        assert_eq!(at(""), None);
    }

    #[test]
    fn javascript_literals_become_json() {
        let v = parse_js(r#"{a: 'x', "b": 0x10, c: [1, 2,], d: undefined, e: !0, 'f': true, g: void 0, // note
            h: `t${1}`, i: 010, 3: "k", j: "q\"q", k: 'it\'s', l: window.foo, }"#)
        .unwrap();
        assert_eq!(v["a"], "x");
        assert_eq!(v["b"], 16);
        assert_eq!(v["c"], json!([1, 2]));
        assert!(v["d"].is_null());
        assert_eq!(v["e"], 0);
        assert_eq!(v["f"], true);
        assert!(v["g"].is_null());
        assert_eq!(v["h"], "t1");
        assert_eq!(v["i"], 8);
        assert_eq!(v["3"], "k");
        assert_eq!(v["j"], "q\"q");
        assert_eq!(v["k"], "it's");
        assert_eq!(v["l"], "window.foo");
        assert_eq!(parse_js(r#"[1, 2.5, -3, 1e3, "x"]"#).unwrap(), json!([1, 2.5, -3, 1000.0, "x"]));
        assert_eq!(
            parse_js(r#"{d: new Date("2024-01-02"), a: Array(1, 2), n: parseInt("12")}"#).unwrap(),
            json!({"d": "2024-01-02", "a": [1, 2], "n": 12})
        );
        assert_eq!(js_to_json("{/* c */ a: 1 /* d */}"), "{ a: 1 }".replace("a", "\"a\""));
        let page = r#"<script>window.__DATA__ = {items: [{id: 'a'}]}; var x = 1;</script>"#;
        assert_eq!(js_object_after(page, "window.__DATA__ =").unwrap()["items"][0]["id"], "a");
        assert_eq!(json_after_any(page, &["nothing", "window.__DATA__ ="]).unwrap()["items"][0]["id"], "a");
        assert_eq!(balanced_js_end("{a: '}', b: \"]\"} tail"), Some(16));
    }

    #[test]
    fn html_is_cleaned_and_attributes_read() {
        assert_eq!(html_unescape("a &amp; b &lt;c&gt; &#39;d&#x27; &nbsp;&bogus; &"), "a & b <c> 'd' \u{a0}&bogus; &");
        assert_eq!(clean_html("<p>One&nbsp;two</p><p>Three <b>four</b><br>five</p>"), "One two\nThree four\nfive");
        let tag = r#"<video id="v1" data-src='a.mp4' controls width=640 title="a &amp; b">"#;
        let attrs = extract_attributes(tag);
        assert_eq!(attribute(tag, "id").as_deref(), Some("v1"));
        assert_eq!(attribute(tag, "data-src").as_deref(), Some("a.mp4"));
        assert_eq!(attribute(tag, "width").as_deref(), Some("640"));
        assert_eq!(attribute(tag, "title").as_deref(), Some("a & b"));
        assert!(attrs.iter().any(|(k, v)| k == "controls" && v.is_empty()));
        let html = r#"<div id="x"><span class="a b">hi</span></div><div class="c">yo</div>"#;
        assert_eq!(element_by_id(html, "x").as_deref(), Some(r#"<span class="a b">hi</span>"#));
        assert_eq!(element_by_class(html, "b").as_deref(), Some("hi"));
        assert_eq!(element_by_class(html, "c").as_deref(), Some("yo"));
        assert_eq!(tags_where(html, "div", &|a| a.iter().any(|(k, _)| k == "class")).len(), 1);
    }

    #[test]
    fn xml_reads_children_and_descendants() {
        let doc = xml("\u{feff}<root><a><b>x</b></a><c>y</c><a><b>z</b></a></root>").unwrap();
        let root = doc.root_element();
        assert_eq!(xml_child_text(root, "c").as_deref(), Some("y"));
        assert_eq!(xml_child_text(root, "b"), None);
        assert_eq!(xml_find(root, "b").and_then(|n| n.text()), Some("x"));
        assert_eq!(xml_find_all(root, "b").len(), 2);
        assert!(xml("<unclosed").is_none());
    }

    #[test]
    fn hashes_ciphers_and_encodings() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex::encode(hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog")),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        assert_eq!(
            hex::encode(hmac_sha1(b"key", b"The quick brown fox jumps over the lazy dog")),
            "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9"
        );
        assert_eq!(
            hex::encode(hmac_md5(b"key", b"The quick brown fox jumps over the lazy dog")),
            "80070713463e7749b90c2dc24911e275"
        );
        assert_eq!(b64_decode("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(b64_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(b64_decode("aG Vs\nbG8="), Some(b"hello".to_vec()));
        assert_eq!(b64_decode("-_-_"), Some(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_encode_url(&[0xfb, 0xff, 0xbf]), "-_-_");
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let plain = b"0123456789abcdef";
        let mut padded = plain.to_vec();
        padded.extend([16u8; 16]);
        // Encrypt with ECB block by block to test the decryptors against a known plaintext.
        use aes::cipher::BlockCipherEncrypt;
        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut block = aes::Block::default();
        block.copy_from_slice(plain);
        cipher.encrypt_block(&mut block);
        let ecb = block.to_vec();
        assert_eq!(aes_ecb_decrypt(&key, &ecb), Some(plain.to_vec()));
        // CBC with a zero IV equals ECB for the first block.
        assert_eq!(aes_cbc_decrypt_raw(&key, &iv, &ecb), Some(plain.to_vec()));
        assert_eq!(unpad_pkcs7(&padded), Some(plain.to_vec()));
        assert_eq!(unpad_pkcs7(b"abc\x05"), None);
        assert_eq!(random_alphanumeric(8).len(), 8);
        assert_eq!(random_hex(4).len(), 4);
        assert_eq!(random_uuid().len(), 36);
        assert_eq!(url_encode("a b&c/d~"), "a%20b%26c%2Fd~");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(parse_query("?a=1&b=x+y"), vec![("a".to_string(), "1".to_string()), ("b".to_string(), "x y".to_string())]);
        assert_eq!(encode_query(&[("a", "1"), ("b", "x y")]), "a=1&b=x+y");
    }
}

/// A podcast link with its tracking prefixes removed: `dts.podtrac.com/redirect.mp3/…`,
/// `chtbl.com/track/…/…`, `pdst.fm/e/…` and `www.podtrac.com/pts/redirect.mp3/…` wrap
/// the real link, sometimes several deep.
pub fn clean_podcast_url(url: Url) -> Url {
    static RE_WRAPPERS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^https?://(?:dts\.podtrac\.com/redirect\.[a-z0-9]+/|chtbl\.com/track/[^/]+/|pdst\.fm/e/|www\.podtrac\.com/pts/redirect\.[a-z0-9]+/)(?:https?://)?")
            .unwrap()
    });
    let mut stripped = url.to_string();
    loop {
        let next = RE_WRAPPERS.replace(&stripped, "https://").into_owned();
        if next == stripped {
            break;
        }
        stripped = next;
    }
    Url::parse(&stripped).unwrap_or(url)
}

/// ISO 639-2 three-letter language codes and their ISO 639-1 two-letter forms.
const ISO639_CODES: &[(&str, &str)] = &[
    ("aa", "aar"), ("ab", "abk"), ("ae", "ave"), ("af", "afr"), ("ak", "aka"), ("am", "amh"),
    ("an", "arg"), ("ar", "ara"), ("as", "asm"), ("av", "ava"), ("ay", "aym"), ("az", "aze"),
    ("ba", "bak"), ("be", "bel"), ("bg", "bul"), ("bh", "bih"), ("bi", "bis"), ("bm", "bam"),
    ("bn", "ben"), ("bo", "bod"), ("br", "bre"), ("bs", "bos"), ("ca", "cat"), ("ce", "che"),
    ("ch", "cha"), ("co", "cos"), ("cr", "cre"), ("cs", "ces"), ("cu", "chu"), ("cv", "chv"),
    ("cy", "cym"), ("da", "dan"), ("de", "deu"), ("dv", "div"), ("dz", "dzo"), ("ee", "ewe"),
    ("el", "ell"), ("en", "eng"), ("eo", "epo"), ("es", "spa"), ("et", "est"), ("eu", "eus"),
    ("fa", "fas"), ("ff", "ful"), ("fi", "fin"), ("fj", "fij"), ("fo", "fao"), ("fr", "fra"),
    ("fy", "fry"), ("ga", "gle"), ("gd", "gla"), ("gl", "glg"), ("gn", "grn"), ("gu", "guj"),
    ("gv", "glv"), ("ha", "hau"), ("he", "heb"), ("hi", "hin"), ("ho", "hmo"), ("hr", "hrv"),
    ("ht", "hat"), ("hu", "hun"), ("hy", "hye"), ("hz", "her"), ("ia", "ina"), ("id", "ind"),
    ("ie", "ile"), ("ig", "ibo"), ("ii", "iii"), ("ik", "ipk"), ("in", "ind"), ("io", "ido"),
    ("is", "isl"), ("it", "ita"), ("iu", "iku"), ("iw", "heb"), ("ja", "jpn"), ("ji", "yid"),
    ("jv", "jav"), ("ka", "kat"), ("kg", "kon"), ("ki", "kik"), ("kj", "kua"), ("kk", "kaz"),
    ("kl", "kal"), ("km", "khm"), ("kn", "kan"), ("ko", "kor"), ("kr", "kau"), ("ks", "kas"),
    ("ku", "kur"), ("kv", "kom"), ("kw", "cor"), ("ky", "kir"), ("la", "lat"), ("lb", "ltz"),
    ("lg", "lug"), ("li", "lim"), ("ln", "lin"), ("lo", "lao"), ("lt", "lit"), ("lu", "lub"),
    ("lv", "lav"), ("mg", "mlg"), ("mh", "mah"), ("mi", "mri"), ("mk", "mkd"), ("ml", "mal"),
    ("mn", "mon"), ("mr", "mar"), ("ms", "msa"), ("mt", "mlt"), ("my", "mya"), ("na", "nau"),
    ("nb", "nob"), ("nd", "nde"), ("ne", "nep"), ("ng", "ndo"), ("nl", "nld"), ("nn", "nno"),
    ("no", "nor"), ("nr", "nbl"), ("nv", "nav"), ("ny", "nya"), ("oc", "oci"), ("oj", "oji"),
    ("om", "orm"), ("or", "ori"), ("os", "oss"), ("pa", "pan"), ("pe", "per"), ("pi", "pli"),
    ("pl", "pol"), ("ps", "pus"), ("pt", "por"), ("qu", "que"), ("rm", "roh"), ("rn", "run"),
    ("ro", "ron"), ("ru", "rus"), ("rw", "kin"), ("sa", "san"), ("sc", "srd"), ("sd", "snd"),
    ("se", "sme"), ("sg", "sag"), ("si", "sin"), ("sk", "slk"), ("sl", "slv"), ("sm", "smo"),
    ("sn", "sna"), ("so", "som"), ("sq", "sqi"), ("sr", "srp"), ("ss", "ssw"), ("st", "sot"),
    ("su", "sun"), ("sv", "swe"), ("sw", "swa"), ("ta", "tam"), ("te", "tel"), ("tg", "tgk"),
    ("th", "tha"), ("ti", "tir"), ("tk", "tuk"), ("tl", "tgl"), ("tn", "tsn"), ("to", "ton"),
    ("tr", "tur"), ("ts", "tso"), ("tt", "tat"), ("tw", "twi"), ("ty", "tah"), ("ug", "uig"),
    ("uk", "ukr"), ("ur", "urd"), ("uz", "uzb"), ("ve", "ven"), ("vi", "vie"), ("vo", "vol"),
    ("wa", "wln"), ("wo", "wol"), ("xh", "xho"), ("yi", "yid"), ("yo", "yor"), ("za", "zha"),
    ("zh", "zho"), ("zu", "zul"),
];

/// The two-letter ISO 639-1 code of a language named by its three-letter ISO 639-2
/// code, or the code itself when it is already two letters; `None` for anything else.
pub fn iso639_short(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        return ISO639_CODES.iter().find(|(_, short)| *short == code).map(|(_, short)| *short);
    }
    ISO639_CODES
        .iter()
        .find(|(long, _)| *long == code)
        .map(|(_, short)| *short)
}

/// English names of languages and their ISO 639-1 codes, for players that label
/// subtitle tracks by name.
const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("afrikaans", "af"), ("arabic", "ar"), ("basque", "eu"), ("bengali", "bn"),
    ("bulgarian", "bg"), ("catalan", "ca"), ("chinese", "zh"), ("croatian", "hr"),
    ("czech", "cs"), ("danish", "da"), ("dutch", "nl"), ("english", "en"),
    ("esperanto", "eo"), ("estonian", "et"), ("filipino", "tl"), ("finnish", "fi"),
    ("french", "fr"), ("galician", "gl"), ("german", "de"), ("greek", "el"),
    ("hebrew", "he"), ("hindi", "hi"), ("hungarian", "hu"), ("icelandic", "is"),
    ("indonesian", "id"), ("irish", "ga"), ("italian", "it"), ("japanese", "ja"),
    ("korean", "ko"), ("latin", "la"), ("latvian", "lv"), ("lithuanian", "lt"),
    ("malay", "ms"), ("norwegian", "no"), ("persian", "fa"), ("polish", "pl"),
    ("portuguese", "pt"), ("romanian", "ro"), ("russian", "ru"), ("serbian", "sr"),
    ("slovak", "sk"), ("slovenian", "sl"), ("spanish", "es"), ("swahili", "sw"),
    ("swedish", "sv"), ("tagalog", "tl"), ("tamil", "ta"), ("thai", "th"),
    ("turkish", "tr"), ("ukrainian", "uk"), ("urdu", "ur"), ("vietnamese", "vi"),
    ("welsh", "cy"),
];

/// The ISO 639-1 code a subtitle label names: a two- or three-letter code as it is
/// (`en`, `eng`, `en-US`), or the English name of the language (`English`, `Spanish
/// (Latin America)`).
pub fn language_code(label: &str) -> Option<String> {
    let label = label.trim();
    let head = label
        .split(|c: char| c == '-' || c == '_' || c == ' ' || c == '(')
        .next()
        .unwrap_or(label);
    if let Some(code) = iso639_short(head) {
        return Some(code.to_string());
    }
    let lower = label.to_ascii_lowercase();
    LANGUAGE_NAMES
        .iter()
        .find(|(name, _)| lower.starts_with(name))
        .map(|(_, code)| code.to_string())
}
