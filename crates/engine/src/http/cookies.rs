//! Cookie jars per platform: RFC 6265 matching, `Set-Cookie` parsing, and the Netscape
//! `cookies.txt` format browsers and yt-dlp exchange.

use jiff::Timestamp;
use jiff::civil::DateTime;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Lower-cased, without a leading dot.
    pub domain: String,
    /// Set without a `Domain` attribute: sent to that exact host only.
    #[serde(default)]
    pub host_only: bool,
    #[serde(default = "root")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// `None` for a session cookie.
    #[serde(default)]
    pub expires: Option<Timestamp>,
}

fn root() -> String {
    "/".into()
}

#[derive(Debug, thiserror::Error)]
pub enum CookieParseError {
    #[error("line {line}: expected 7 tab separated fields, found {found}")]
    Fields { line: usize, found: usize },
    #[error("line {0}: {1}")]
    Value(usize, String),
}

impl Cookie {
    pub fn new(
        name: impl Into<String>,
        value: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: normalize_domain(&domain.into()),
            host_only: false,
            path: root(),
            secure: false,
            http_only: false,
            expires: None,
        }
    }

    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }

    /// Whether this cookie is sent with a request to `url`.
    pub fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        if self.secure && url.scheme() != "https" && !is_localhost(host) {
            return false;
        }
        domain_matches(&self.domain, self.host_only, &host.to_ascii_lowercase())
            && path_matches(&self.path, url.path())
    }

    /// Parses a `Set-Cookie` header value received from `request`, applying the default
    /// domain and path rules of RFC 6265 §5.3.
    pub fn parse_set_cookie(header: &str, request: &Url) -> Option<Cookie> {
        let mut parts = header.split(';');
        let pair = parts.next()?.trim();
        let (name, value) = pair.split_once('=')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let request_host = request.host_str()?.to_ascii_lowercase();
        let mut cookie = Cookie {
            name: name.to_string(),
            value: value.trim().trim_matches('"').to_string(),
            domain: request_host.clone(),
            host_only: true,
            path: default_path(request),
            secure: false,
            http_only: false,
            expires: None,
        };
        let mut max_age: Option<i64> = None;
        let mut expires: Option<Timestamp> = None;
        for attribute in parts {
            let attribute = attribute.trim();
            let (key, val) = match attribute.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
                None => (attribute.to_ascii_lowercase(), ""),
            };
            match key.as_str() {
                "domain" => {
                    let domain = normalize_domain(val);
                    if domain.is_empty() {
                        continue;
                    }
                    // A cookie may only be set for the request host or a parent domain.
                    if !domain_matches(&domain, false, &request_host) {
                        return None;
                    }
                    cookie.domain = domain;
                    cookie.host_only = false;
                }
                "path" if val.starts_with('/') => cookie.path = val.to_string(),
                "secure" => cookie.secure = true,
                "httponly" => cookie.http_only = true,
                "max-age" => max_age = val.parse().ok(),
                "expires" => expires = parse_http_date(val),
                _ => {}
            }
        }
        cookie.expires = match max_age {
            Some(secs) => Some(Timestamp::now() + jiff::SignedDuration::from_secs(secs)),
            None => expires,
        };
        Some(cookie)
    }
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

pub fn normalize_domain(domain: &str) -> String {
    domain.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// RFC 6265 §5.1.3: the host is the domain, or a subdomain of a non host-only cookie.
pub fn domain_matches(cookie_domain: &str, host_only: bool, host: &str) -> bool {
    if host == cookie_domain {
        return true;
    }
    if host_only {
        return false;
    }
    host.ends_with(cookie_domain)
        && host.len() > cookie_domain.len()
        && host.as_bytes()[host.len() - cookie_domain.len() - 1] == b'.'
        && host.parse::<std::net::IpAddr>().is_err()
}

/// RFC 6265 §5.1.4.
pub fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if cookie_path == request_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

fn default_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return root();
    }
    match path.rfind('/') {
        Some(0) | None => root(),
        Some(index) => path[..index].to_string(),
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Parses the dates cookies and HTTP headers carry: RFC 1123 (`Sun, 06 Nov 1994 08:49:37
/// GMT`), RFC 850 (`Sunday, 06-Nov-94 08:49:37 GMT`), asctime, and the dashed variants
/// cookies use, tolerantly as RFC 6265 §5.1.1 asks.
pub fn parse_http_date(text: &str) -> Option<Timestamp> {
    let cleaned = text.replace(['-', ','], " ");
    let mut time: Option<(i8, i8, i8)> = None;
    let mut day: Option<i8> = None;
    let mut month: Option<i8> = None;
    let mut year: Option<i16> = None;
    for token in cleaned.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if time.is_none() && token.contains(':') {
            let mut parts = token.split(':');
            let h = parts.next()?.parse().ok()?;
            let m = parts.next()?.parse().ok()?;
            let s = parts.next().unwrap_or("0").parse().ok()?;
            time = Some((h, m, s));
            continue;
        }
        if month.is_none()
            && let Some(index) = MONTHS.iter().position(|m| lower.starts_with(m))
        {
            month = Some(index as i8 + 1);
            continue;
        }
        if let Ok(number) = token.parse::<i64>() {
            if day.is_none() && (1..=31).contains(&number) && number <= 31 && token.len() <= 2 {
                day = Some(number as i8);
            } else if year.is_none() {
                let mut y = number;
                if token.len() <= 2 {
                    y += if y >= 70 { 1900 } else { 2000 };
                }
                year = Some(y as i16);
            }
        }
    }
    let (hour, minute, second) = time?;
    let datetime = DateTime::new(year?, month?, day?, hour, minute, second, 0).ok()?;
    datetime
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

/// One platform's cookies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jar {
    cookies: Vec<Cookie>,
}

impl Jar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_cookies(cookies: Vec<Cookie>) -> Self {
        let mut jar = Self::new();
        for cookie in cookies {
            jar.insert(cookie);
        }
        jar
    }

    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    pub fn into_cookies(self) -> Vec<Cookie> {
        self.cookies
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// The first cookie named `name`, on any domain.
    pub fn get(&self, name: &str) -> Option<&Cookie> {
        self.cookies.iter().find(|c| c.name == name)
    }

    /// Adds `cookie`, replacing one with the same name, domain and path.
    pub fn insert(&mut self, cookie: Cookie) {
        if let Some(existing) = self
            .cookies
            .iter_mut()
            .find(|c| c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        {
            *existing = cookie;
        } else {
            self.cookies.push(cookie);
        }
    }

    pub fn remove(&mut self, name: &str, domain: &str) -> bool {
        let before = self.cookies.len();
        let domain = normalize_domain(domain);
        self.cookies
            .retain(|c| !(c.name == name && c.domain == domain));
        self.cookies.len() != before
    }

    /// Records a `Set-Cookie` header received from `url`; an expired one removes the cookie.
    pub fn store_set_cookie(&mut self, header: &str, url: &Url) {
        if let Some(cookie) = Cookie::parse_set_cookie(header, url) {
            if cookie.is_expired(Timestamp::now()) {
                self.cookies.retain(|c| {
                    !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
                });
            } else {
                self.insert(cookie);
            }
        }
    }

    pub fn remove_expired(&mut self, now: Timestamp) {
        self.cookies.retain(|c| !c.is_expired(now));
    }

    /// The `Cookie` request header for `url`, longest paths first as RFC 6265 §5.4 asks.
    pub fn header_for(&self, url: &Url, now: Timestamp) -> Option<String> {
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| !c.is_expired(now) && c.matches(url))
            .collect();
        if matching.is_empty() {
            return None;
        }
        matching.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Some(
            matching
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Reads the Netscape `cookies.txt` format, including the `#HttpOnly_` prefix.
    pub fn import_netscape(text: &str) -> Result<Jar, CookieParseError> {
        let mut jar = Jar::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim_end_matches('\r');
            let (http_only, content) = match trimmed.strip_prefix("#HttpOnly_") {
                Some(rest) => (true, rest),
                None => (false, trimmed),
            };
            if content.trim().is_empty() || content.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = content.split('\t').collect();
            if fields.len() < 6 || fields.len() > 7 {
                return Err(CookieParseError::Fields {
                    line,
                    found: fields.len(),
                });
            }
            let domain_field = fields[0];
            let include_subdomains = fields[1].eq_ignore_ascii_case("TRUE");
            let path = if fields[2].starts_with('/') {
                fields[2].to_string()
            } else {
                root()
            };
            let secure = fields[3].eq_ignore_ascii_case("TRUE");
            let expires: i64 = fields[4]
                .trim()
                .parse()
                .map_err(|e| CookieParseError::Value(line, format!("expiry: {e}")))?;
            let name = fields[5].to_string();
            let value = fields.get(6).map(|v| v.to_string()).unwrap_or_default();
            if name.is_empty() {
                return Err(CookieParseError::Value(line, "empty cookie name".into()));
            }
            jar.insert(Cookie {
                name,
                value,
                domain: normalize_domain(domain_field),
                host_only: !include_subdomains && !domain_field.starts_with('.'),
                path,
                secure,
                http_only,
                expires: (expires > 0)
                    .then(|| Timestamp::from_second(expires).ok())
                    .flatten(),
            });
        }
        Ok(jar)
    }

    pub fn export_netscape(&self) -> String {
        let mut out = String::from(
            "# Netscape HTTP Cookie File\n# https://curl.se/docs/http-cookies.html\n\n",
        );
        for c in &self.cookies {
            let prefix = if c.http_only { "#HttpOnly_" } else { "" };
            let domain = if c.host_only {
                c.domain.clone()
            } else {
                format!(".{}", c.domain)
            };
            out.push_str(&format!(
                "{prefix}{domain}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                if c.host_only { "FALSE" } else { "TRUE" },
                c.path,
                if c.secure { "TRUE" } else { "FALSE" },
                c.expires.map(|t| t.as_second()).unwrap_or(0),
                c.name,
                c.value
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn set_cookie_defaults_and_attributes() {
        let request = url("https://www.example.com/videos/watch?v=1");
        let cookie =
            Cookie::parse_set_cookie("sid=abc; Path=/; Secure; HttpOnly", &request).unwrap();
        assert_eq!(cookie.name, "sid");
        assert_eq!(cookie.value, "abc");
        assert_eq!(cookie.domain, "www.example.com");
        assert!(cookie.host_only);
        assert_eq!(cookie.path, "/");
        assert!(cookie.secure && cookie.http_only);
        assert!(cookie.expires.is_none());

        let cookie = Cookie::parse_set_cookie("a=b", &request).unwrap();
        assert_eq!(cookie.path, "/videos");
        let cookie =
            Cookie::parse_set_cookie("a=b; Domain=.example.com; Max-Age=60", &request).unwrap();
        assert_eq!(cookie.domain, "example.com");
        assert!(!cookie.host_only);
        assert!(cookie.expires.unwrap() > Timestamp::now());
        assert!(Cookie::parse_set_cookie("a=b; Domain=other.com", &request).is_none());
        assert!(Cookie::parse_set_cookie("=b", &request).is_none());
        let cookie = Cookie::parse_set_cookie(
            "CONSENT=YES+1; Expires=Sun, 10 Jan 2038 00:00:00 GMT; Domain=example.com",
            &request,
        )
        .unwrap();
        assert_eq!(cookie.expires.unwrap().as_second(), 2146694400);
    }

    #[test]
    fn matching_follows_domain_path_and_secure_rules() {
        let mut secure = Cookie::new("s", "1", ".example.com");
        secure.secure = true;
        assert!(secure.matches(&url("https://a.example.com/x")));
        assert!(!secure.matches(&url("http://a.example.com/x")));
        assert!(!secure.matches(&url("https://notexample.com/x")));
        let mut host_only = Cookie::new("h", "1", "www.example.com");
        host_only.host_only = true;
        assert!(host_only.matches(&url("http://www.example.com/")));
        assert!(!host_only.matches(&url("http://sub.www.example.com/")));
        let mut pathed = Cookie::new("p", "1", "example.com");
        pathed.path = "/api".into();
        assert!(pathed.matches(&url("http://example.com/api")));
        assert!(pathed.matches(&url("http://example.com/api/v1")));
        assert!(!pathed.matches(&url("http://example.com/apis")));
        assert!(!pathed.matches(&url("http://example.com/")));
    }

    #[test]
    fn jar_builds_request_headers_and_replaces_cookies() {
        let mut jar = Jar::new();
        let page = url("https://www.example.com/a/b");
        jar.store_set_cookie("x=1; Path=/", &page);
        jar.store_set_cookie("y=2", &page);
        jar.store_set_cookie("x=3; Path=/", &page);
        assert_eq!(jar.len(), 2);
        assert_eq!(jar.header_for(&page, Timestamp::now()).unwrap(), "y=2; x=3");
        assert!(
            jar.header_for(&url("https://other.com/"), Timestamp::now())
                .is_none()
        );
        jar.store_set_cookie("x=; Path=/; Max-Age=0", &page);
        assert_eq!(jar.len(), 1);
        assert!(jar.get("y").is_some());
        assert!(jar.remove("y", "www.example.com"));
        assert!(jar.is_empty());
    }

    #[test]
    fn netscape_round_trips() {
        let text = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t2147483647\tCONSENT\tYES+cb\n\
            #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1893456000\tSID\tabc.def\n\
            www.example.com\tFALSE\t/path\tFALSE\t0\tsession\tvalue\n";
        let jar = Jar::import_netscape(text).unwrap();
        assert_eq!(jar.len(), 3);
        let sid = jar.get("SID").unwrap();
        assert!(sid.http_only && sid.secure && !sid.host_only);
        assert_eq!(sid.domain, "youtube.com");
        let session = jar.get("session").unwrap();
        assert!(session.host_only && session.expires.is_none());
        assert_eq!(session.path, "/path");
        let exported = jar.export_netscape();
        let again = Jar::import_netscape(&exported).unwrap();
        assert_eq!(again, jar);
        assert!(matches!(
            Jar::import_netscape("a\tb\n"),
            Err(CookieParseError::Fields { line: 1, found: 2 })
        ));
        assert!(Jar::import_netscape("").unwrap().is_empty());
    }

    #[test]
    fn http_dates_in_every_common_shape() {
        let expected = 784111777;
        for text in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "06 Nov 1994 08:49:37",
        ] {
            assert_eq!(
                parse_http_date(text).map(|t| t.as_second()),
                Some(expected),
                "{text}"
            );
        }
        assert!(parse_http_date("never").is_none());
    }
}
