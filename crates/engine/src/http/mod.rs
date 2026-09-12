//! HTTP for resolvers and downloaders: one shared [`Http`] with per-platform cookie jars
//! (the logged-in sessions the app manages) over the consent cookies each resolver seeds,
//! proxies, per-host rate limits, retries, redirects followed by hand so cookies and short
//! links behave, request counts for the metrics page, and a transport that tests swap for
//! recorded fixtures.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use jiff::Timestamp;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::Url;

pub mod cookies;
pub mod ratelimit;
pub mod retry;
pub mod transport;

pub use cookies::{Cookie, CookieParseError, Jar};
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
pub use ratelimit::{HostLimiter, Rate, RateLimits};
pub use retry::RetryPolicy;
pub use transport::{
    BodyStream, Exchange, Fixture, LiveTransport, RecordedBody, RecordedRequest, RecordedResponse,
    RecordingTransport, ReplayTransport, Transport, TransportRequest, TransportResponse,
};

pub const APP_UA: &str = concat!("DiscoClip/", env!("CARGO_PKG_VERSION"));
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
pub const ANDROID_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
pub const EMBED_BOT_UA: &str = "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)";

/// The jar used by requests that name no platform: cookies imported for arbitrary sites.
pub const WEB_PLATFORM: &str = "web";
pub const DEFAULT_BODY_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("could not connect to {url}: {message}")]
    Connect { url: String, message: String },
    #[error("request to {url} timed out")]
    Timeout { url: String },
    #[error("request to {url} failed: {message}")]
    Transport { url: String, message: String },
    #[error("body from {url} is longer than {limit} bytes")]
    BodyTooLarge { url: String, limit: usize },
    #[error("too many redirects from {0}")]
    TooManyRedirects(String),
    #[error("{url} redirects to {location}, which is not a URL")]
    BadRedirect { url: String, location: String },
    #[error("no recorded exchange for {method} {url}")]
    NoFixture { method: String, url: String },
    #[error("{0} is not a usable proxy URL")]
    Proxy(String),
    #[error("http client: {0}")]
    Build(String),
    #[error("{0} is not a valid HTTP header")]
    Header(String),
    #[error("{url} answered HTTP {status}")]
    Status { url: String, status: StatusCode },
    #[error("{url} sent a body that is not the JSON expected: {message}")]
    Json { url: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl HttpError {
    pub fn from_reqwest(url: &Url, error: reqwest::Error) -> Self {
        let url = url.to_string();
        if error.is_timeout() {
            HttpError::Timeout { url }
        } else if error.is_connect() {
            HttpError::Connect {
                url,
                message: error.to_string(),
            }
        } else {
            HttpError::Transport {
                url,
                message: error.to_string(),
            }
        }
    }

    /// Whether asking again might work.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            HttpError::Connect { .. } | HttpError::Timeout { .. } | HttpError::Transport { .. }
        )
    }
}

/// Proxies by platform and by host suffix, over a default; `bypass` hosts go direct.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Proxies {
    pub default: Option<Url>,
    pub platforms: HashMap<String, Url>,
    pub hosts: HashMap<String, Url>,
    pub bypass: Vec<String>,
}

fn host_matches(host: &str, suffix: &str) -> bool {
    let suffix = suffix.trim_start_matches('.').to_ascii_lowercase();
    !suffix.is_empty() && (host == suffix || host.ends_with(&format!(".{suffix}")))
}

impl Proxies {
    pub fn for_request(&self, platform: Option<&str>, host: &str) -> Option<Url> {
        let host = host.to_ascii_lowercase();
        if self.bypass.iter().any(|b| host_matches(&host, b)) {
            return None;
        }
        if let Some(url) = platform.and_then(|p| self.platforms.get(p)) {
            return Some(url.clone());
        }
        if let Some((_, url)) = self
            .hosts
            .iter()
            .filter(|(suffix, _)| host_matches(&host, suffix))
            .max_by_key(|(suffix, _)| suffix.len())
        {
            return Some(url.clone());
        }
        self.default.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// Sent when a request names no other user agent.
    pub user_agent: String,
    pub connect_timeout_secs: u64,
    /// Bound on a whole page or API request; media downloads have none.
    pub request_timeout_secs: u64,
    /// Longest pause between two chunks of a body.
    pub read_timeout_secs: u64,
    pub max_redirects: u32,
    pub retry: RetryPolicy,
    pub rate_limits: RateLimits,
    pub proxies: Proxies,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: APP_UA.to_string(),
            connect_timeout_secs: 15,
            request_timeout_secs: 90,
            read_timeout_secs: 60,
            max_redirects: 10,
            retry: RetryPolicy::default(),
            rate_limits: RateLimits::default(),
            proxies: Proxies::default(),
        }
    }
}

/// Request counts since start, by host and status, for the metrics endpoint.
#[derive(Default)]
pub struct Stats {
    by_host: Mutex<HashMap<(String, u16), u64>>,
    retries: AtomicU64,
    rate_limit_waits: AtomicU64,
    bytes_received: AtomicU64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatsSnapshot {
    /// `(host, status, count)`, by host then status.
    pub requests: Vec<(String, u16, u64)>,
    pub retries: u64,
    pub rate_limit_waits: u64,
    pub bytes_received: u64,
}

impl Stats {
    fn record(&self, host: &str, status: StatusCode) {
        *self
            .by_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((host.to_string(), status.as_u16()))
            .or_default() += 1;
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let mut requests: Vec<(String, u16, u64)> = self
            .by_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|((host, status), count)| (host.clone(), *status, *count))
            .collect();
        requests.sort();
        StatsSnapshot {
            requests,
            retries: self.retries.load(Ordering::Relaxed),
            rate_limit_waits: self.rate_limit_waits.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
        }
    }
}

struct Inner {
    transport: Arc<dyn Transport>,
    config: RwLock<HttpConfig>,
    jars: RwLock<HashMap<String, Jar>>,
    /// Cookies each platform's resolver sends to get past consent and age gates without
    /// an account; a jar cookie of the same name and domain replaces one.
    seeds: RwLock<HashMap<String, Vec<Cookie>>>,
    limiter: HostLimiter,
    stats: Stats,
}

#[derive(Clone)]
pub struct Http {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("transport", &self.inner.transport.name())
            .finish_non_exhaustive()
    }
}

impl Http {
    /// Talks to the network.
    pub fn new(config: HttpConfig) -> Self {
        let transport = LiveTransport::new(
            Duration::from_secs(config.connect_timeout_secs),
            Duration::from_secs(config.read_timeout_secs),
        );
        Self::with_transport(Arc::new(transport), config)
    }

    pub fn with_transport(transport: Arc<dyn Transport>, config: HttpConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                limiter: HostLimiter::new(config.rate_limits.clone()),
                config: RwLock::new(config),
                jars: RwLock::new(HashMap::new()),
                seeds: RwLock::new(HashMap::new()),
                stats: Stats::default(),
            }),
        }
    }

    /// Answers from a recorded fixture, at once and without retries.
    pub fn replay(fixture: Fixture) -> Self {
        Self::with_transport(Arc::new(ReplayTransport::new(fixture)), Self::test_config())
    }

    /// Talks to the network and records what passed into a fixture at `path`.
    pub fn recording(config: HttpConfig, path: PathBuf, fixture: Fixture) -> Self {
        let live = LiveTransport::new(
            Duration::from_secs(config.connect_timeout_secs),
            Duration::from_secs(config.read_timeout_secs),
        );
        Self::with_transport(
            Arc::new(RecordingTransport::new(Arc::new(live), path, fixture)),
            config,
        )
    }

    fn test_config() -> HttpConfig {
        HttpConfig {
            retry: RetryPolicy::NONE,
            rate_limits: RateLimits {
                default: Rate::UNLIMITED,
                hosts: HashMap::new(),
            },
            ..HttpConfig::default()
        }
    }

    pub fn transport_name(&self) -> &'static str {
        self.inner.transport.name()
    }

    pub fn config(&self) -> HttpConfig {
        self.inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Applies new rate limits, proxies, retries and timeouts to requests from now on.
    pub fn configure(&self, config: HttpConfig) {
        self.inner.limiter.set(config.rate_limits.clone());
        self.inner.transport.configure(
            Duration::from_secs(config.connect_timeout_secs),
            Duration::from_secs(config.read_timeout_secs),
        );
        *self.inner.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.inner.stats.snapshot()
    }

    /// A copy of `platform`'s cookies.
    pub fn jar(&self, platform: &str) -> Jar {
        self.inner
            .jars
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(platform)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_jar(&self, platform: &str, jar: Jar) {
        let mut jars = self.inner.jars.write().unwrap_or_else(|e| e.into_inner());
        if jar.is_empty() {
            jars.remove(platform);
        } else {
            jars.insert(platform.to_string(), jar);
        }
    }

    pub fn clear_jar(&self, platform: &str) {
        self.inner
            .jars
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(platform);
    }

    /// Edits `platform`'s cookies in place.
    pub fn with_jar<R>(&self, platform: &str, edit: impl FnOnce(&mut Jar) -> R) -> R {
        let mut jars = self.inner.jars.write().unwrap_or_else(|e| e.into_inner());
        let jar = jars.entry(platform.to_string()).or_default();
        let result = edit(jar);
        if jar.is_empty() {
            jars.remove(platform);
        }
        result
    }

    /// Sets the cookies `platform`'s requests carry besides its jar: what gets past the
    /// platform's consent and age gates without an account.
    pub fn seed_cookies(&self, platform: &str, cookies: Vec<Cookie>) {
        let mut seeds = self.inner.seeds.write().unwrap_or_else(|e| e.into_inner());
        if cookies.is_empty() {
            seeds.remove(platform);
        } else {
            seeds.insert(platform.to_string(), cookies);
        }
    }

    /// The consent cookies seeded for `platform`.
    pub fn seeds(&self, platform: &str) -> Vec<Cookie> {
        self.inner
            .seeds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(platform)
            .cloned()
            .unwrap_or_default()
    }

    pub fn platforms_with_cookies(&self) -> Vec<String> {
        let mut platforms: Vec<String> = self
            .inner
            .jars
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        platforms.sort();
        platforms
    }

    pub fn request(&self, method: Method, url: Url) -> RequestBuilder {
        RequestBuilder {
            http: self.clone(),
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            platform: None,
            follow_redirects: true,
            timeout: Timeout::Default,
            cookies: true,
            retry: true,
            rate_limit: true,
            error: None,
        }
    }

    pub fn get(&self, url: Url) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn head(&self, url: Url) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    pub fn post(&self, url: Url) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Where `url` ends up after its redirects, without reading any body: unwraps short
    /// links such as `t.co`, `bit.ly` and `youtu.be`.
    pub async fn unwrap_redirects(
        &self,
        url: &Url,
        platform: Option<&str>,
        user_agent: &str,
    ) -> Result<Url, HttpError> {
        let mut builder = self.get(url.clone()).user_agent(user_agent);
        if let Some(platform) = platform {
            builder = builder.platform(platform);
        }
        let response = builder.send().await?;
        Ok(response.url)
    }

    fn cookie_header(&self, platform: Option<&str>, url: &Url) -> Option<String> {
        let platform = platform.unwrap_or(WEB_PLATFORM);
        let seeds = self.inner.seeds.read().unwrap_or_else(|e| e.into_inner());
        let jars = self.inner.jars.read().unwrap_or_else(|e| e.into_inner());
        match (seeds.get(platform), jars.get(platform)) {
            (None, None) => None,
            (None, Some(jar)) => jar.header_for(url, Timestamp::now()),
            (Some(seeds), jar) => {
                // The jar's cookies replace seeds of the same name, domain and path.
                let mut merged = Jar::from_cookies(seeds.clone());
                for cookie in jar.map(|j| j.cookies()).unwrap_or_default() {
                    merged.insert(cookie.clone());
                }
                merged.header_for(url, Timestamp::now())
            }
        }
    }

    fn store_cookies(&self, platform: Option<&str>, url: &Url, headers: &HeaderMap) {
        let set_cookies: Vec<&str> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        if set_cookies.is_empty() {
            return;
        }
        let mut jars = self.inner.jars.write().unwrap_or_else(|e| e.into_inner());
        let jar = jars
            .entry(platform.unwrap_or(WEB_PLATFORM).to_string())
            .or_default();
        for value in set_cookies {
            jar.store_set_cookie(value, url);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Timeout {
    /// The configured request timeout.
    Default,
    None,
    Some(Duration),
}

/// One request being put together; `send` runs it.
pub struct RequestBuilder {
    http: Http,
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Bytes>,
    platform: Option<String>,
    follow_redirects: bool,
    timeout: Timeout,
    cookies: bool,
    retry: bool,
    rate_limit: bool,
    error: Option<HttpError>,
}

impl RequestBuilder {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                self.headers.insert(name, value);
            }
            _ => self.error = Some(HttpError::Header(format!("{name}: {value}"))),
        }
        self
    }

    pub fn headers(mut self, headers: &[(String, String)]) -> Self {
        for (name, value) in headers {
            self = self.header(name, value);
        }
        self
    }

    pub fn header_map(mut self, headers: HeaderMap) -> Self {
        self.headers.extend(headers);
        self
    }

    pub fn user_agent(self, agent: &str) -> Self {
        self.header("user-agent", agent)
    }

    /// Whose cookie jar and proxy the request uses.
    pub fn platform(mut self, platform: &str) -> Self {
        self.platform = Some(platform.to_string());
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn json<T: Serialize>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => {
                self.body = Some(Bytes::from(bytes));
                self.header("content-type", "application/json")
            }
            Err(error) => {
                self.error = Some(HttpError::Json {
                    url: self.url.to_string(),
                    message: error.to_string(),
                });
                self
            }
        }
    }

    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        let encoded: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        self.body = Some(Bytes::from(encoded));
        self.header("content-type", "application/x-www-form-urlencoded")
    }

    /// Whether 3xx answers are followed; off, the redirect itself is returned.
    pub fn follow_redirects(mut self, follow: bool) -> Self {
        self.follow_redirects = follow;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Timeout::Some(timeout);
        self
    }

    /// No bound on the whole exchange: for media bodies, which take as long as they take.
    pub fn no_timeout(mut self) -> Self {
        self.timeout = Timeout::None;
        self
    }

    /// A media request: no total timeout, no compressed transfer, so lengths and ranges hold.
    pub fn media(self) -> Self {
        self.no_timeout().header("accept-encoding", "identity")
    }

    pub fn no_cookies(mut self) -> Self {
        self.cookies = false;
        self
    }

    pub fn no_retry(mut self) -> Self {
        self.retry = false;
        self
    }

    pub fn no_rate_limit(mut self) -> Self {
        self.rate_limit = false;
        self
    }

    pub async fn send(self) -> Result<Response, HttpError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let http = self.http.clone();
        let config = http.config();
        let requested = self.url.clone();
        let mut url = self.url.clone();
        let mut method = self.method.clone();
        let mut body = self.body.clone();
        let timeout = match self.timeout {
            Timeout::Default => Some(Duration::from_secs(config.request_timeout_secs)),
            Timeout::None => None,
            Timeout::Some(d) => Some(d),
        };
        let platform = self.platform.as_deref();
        let attempts = if self.retry {
            config.retry.attempts.max(1)
        } else {
            1
        };
        for _hop in 0..=config.max_redirects {
            let host = url.host_str().unwrap_or("").to_string();
            let mut headers = self.headers.clone();
            if !headers.contains_key(header::USER_AGENT) {
                headers.insert(
                    header::USER_AGENT,
                    HeaderValue::from_str(&config.user_agent)
                        .map_err(|_| HttpError::Header(config.user_agent.clone()))?,
                );
            }
            if url.host_str() != requested.host_str() {
                headers.remove(header::AUTHORIZATION);
            }
            if self.cookies
                && !headers.contains_key(header::COOKIE)
                && let Some(cookie) = http.cookie_header(platform, &url)
                && let Ok(value) = HeaderValue::from_str(&cookie)
            {
                headers.insert(header::COOKIE, value);
            }
            let proxy = config.proxies.for_request(platform, &host);
            let mut attempt = 0u32;
            let response = loop {
                attempt += 1;
                if self.rate_limit {
                    let wait = http.inner.limiter.reserve(&host, std::time::Instant::now());
                    if !wait.is_zero() {
                        http.inner
                            .stats
                            .rate_limit_waits
                            .fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(wait).await;
                    }
                }
                let request = TransportRequest {
                    method: method.clone(),
                    url: url.clone(),
                    headers: headers.clone(),
                    body: body.clone(),
                    timeout,
                    proxy: proxy.clone(),
                };
                match http.inner.transport.send(request).await {
                    Ok(response) => {
                        http.inner.stats.record(&host, response.status);
                        if self.cookies {
                            http.store_cookies(platform, &url, &response.headers);
                        }
                        if attempt < attempts && RetryPolicy::retries_status(response.status) {
                            let delay = config.retry.delay(attempt, Some(&response.headers));
                            tracing::debug!(%url, status = %response.status, ?delay, "retrying");
                            http.inner.stats.retries.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        break response;
                    }
                    Err(error) if attempt < attempts && error.is_retryable() => {
                        let delay = config.retry.delay(attempt, None);
                        tracing::debug!(%url, %error, ?delay, "retrying");
                        http.inner.stats.retries.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(delay).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let redirect = self.follow_redirects
                && matches!(response.status.as_u16(), 301 | 302 | 303 | 307 | 308);
            if redirect {
                let Some(location) = response
                    .headers
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                else {
                    return Ok(Response::new(response, requested, url));
                };
                let next = url.join(location).map_err(|_| HttpError::BadRedirect {
                    url: url.to_string(),
                    location: location.to_string(),
                })?;
                if !matches!(next.scheme(), "http" | "https") {
                    return Ok(Response::new(response, requested, url));
                }
                let keep_method = matches!(response.status.as_u16(), 307 | 308)
                    || method == Method::GET
                    || method == Method::HEAD;
                if !keep_method {
                    method = Method::GET;
                    body = None;
                }
                tracing::trace!(from = %url, to = %next, "redirect");
                url = next;
                continue;
            }
            let stats_bytes = Arc::clone(&http.inner);
            let mut result = Response::new(response, requested, url);
            let inner = result.body;
            result.body = Box::pin(inner.inspect(move |chunk| {
                if let Ok(chunk) = chunk {
                    stats_bytes
                        .stats
                        .bytes_received
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
            }));
            return Ok(result);
        }
        Err(HttpError::TooManyRedirects(requested.to_string()))
    }
}

/// An answer: headers now, the body as it is read.
pub struct Response {
    pub status: StatusCode,
    /// Where the answer came from, after redirects.
    pub url: Url,
    /// What was asked for.
    pub requested: Url,
    pub headers: HeaderMap,
    body: BodyStream,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("url", &self.url.as_str())
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl Response {
    fn new(response: TransportResponse, requested: Url, url: Url) -> Self {
        Self {
            status: response.status,
            url,
            requested,
            headers: response.headers,
            body: response.body,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.parse().ok())
    }

    /// Fails unless the status is 2xx.
    pub fn error_for_status(&self) -> Result<(), HttpError> {
        if self.status.is_success() {
            Ok(())
        } else {
            Err(HttpError::Status {
                url: self.requested.to_string(),
                status: self.status,
            })
        }
    }

    /// The body, failing once it is longer than `limit`.
    pub async fn bytes(self, limit: usize) -> Result<Bytes, HttpError> {
        let url = self.url.to_string();
        let (bytes, truncated) = self.bytes_up_to(limit).await?;
        if truncated {
            return Err(HttpError::BodyTooLarge { url, limit });
        }
        Ok(bytes)
    }

    /// At most `limit` bytes of the body, and whether there was more.
    pub async fn bytes_up_to(self, limit: usize) -> Result<(Bytes, bool), HttpError> {
        let mut body = Vec::new();
        let mut stream = self.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let room = limit.saturating_sub(body.len());
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                return Ok((Bytes::from(body), true));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((Bytes::from(body), false))
    }

    /// The body as text, lossily, cut at `limit` bytes.
    pub async fn text(self, limit: usize) -> Result<String, HttpError> {
        let (bytes, _) = self.bytes_up_to(limit).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn json<T: DeserializeOwned>(self, limit: usize) -> Result<T, HttpError> {
        let url = self.url.to_string();
        let bytes = self.bytes(limit).await?;
        serde_json::from_slice(&bytes).map_err(|e| HttpError::Json {
            url,
            message: e.to_string(),
        })
    }

    pub fn into_stream(self) -> BodyStream {
        self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A status, headers and body to answer with.
    type Reply = (u16, Vec<(String, String)>, &'static str);

    struct Scripted {
        responses: Mutex<Vec<Reply>>,
        seen: Mutex<Vec<TransportRequest>>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Transport for Scripted {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let url = request.url.clone();
            let mut responses = self.responses.lock().unwrap();
            let (status, headers, body) = if responses.is_empty() {
                (200, Vec::new(), "done")
            } else {
                responses.remove(0)
            };
            self.seen.lock().unwrap().push(request);
            let mut map = HeaderMap::new();
            for (name, value) in headers {
                map.append(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(&value).unwrap(),
                );
            }
            Ok(TransportResponse {
                status: StatusCode::from_u16(status).unwrap(),
                url,
                headers: map,
                body: Box::pin(futures::stream::once(async move { Ok(Bytes::from(body)) })),
            })
        }

        fn name(&self) -> &'static str {
            "scripted"
        }
    }

    fn scripted(responses: Vec<Reply>) -> (Http, Arc<Scripted>) {
        let transport = Arc::new(Scripted {
            responses: Mutex::new(responses),
            seen: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut config = HttpConfig {
            retry: RetryPolicy {
                attempts: 3,
                base_ms: 1,
                max_ms: 2,
            },
            ..Http::test_config()
        };
        config.user_agent = "test-agent".into();
        (Http::with_transport(transport.clone(), config), transport)
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.into(), value.into())
    }

    #[tokio::test]
    async fn redirects_are_followed_with_cookies_and_method_rules() {
        let (http, transport) = scripted(vec![
            (
                302,
                vec![pair("location", "/next"), pair("set-cookie", "a=1; Path=/")],
                "",
            ),
            (303, vec![pair("location", "https://other.test/final")], ""),
            (200, vec![], "landed"),
        ]);
        let response = http
            .post(Url::parse("https://a.test/start").unwrap())
            .platform("p")
            .header("authorization", "Bearer x")
            .body("payload")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.url.as_str(), "https://other.test/final");
        assert_eq!(response.requested.as_str(), "https://a.test/start");
        assert_eq!(response.text(1024).await.unwrap(), "landed");
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].method, Method::POST);
        assert_eq!(seen[0].headers["user-agent"], "test-agent");
        // 302 on POST becomes GET without body; the cookie set on hop one is sent on hop two.
        assert_eq!(seen[1].method, Method::GET);
        assert!(seen[1].body.is_none());
        assert_eq!(seen[1].headers["cookie"], "a=1");
        assert_eq!(seen[1].headers["authorization"], "Bearer x");
        // Another host: no cookie, no credentials.
        assert!(!seen[2].headers.contains_key("cookie"));
        assert!(!seen[2].headers.contains_key("authorization"));
        assert_eq!(http.jar("p").get("a").unwrap().value, "1");
    }

    #[tokio::test]
    async fn redirects_can_be_left_alone_and_are_bounded() {
        let (http, _) = scripted(vec![(301, vec![pair("location", "/x")], "")]);
        let response = http
            .get(Url::parse("https://a.test/").unwrap())
            .follow_redirects(false)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::MOVED_PERMANENTLY);
        assert_eq!(response.header("location"), Some("/x"));

        let loops: Vec<_> = (0..20)
            .map(|_| (302, vec![pair("location", "/again")], ""))
            .collect();
        let (http, _) = scripted(loops);
        let error = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap_err();
        assert!(matches!(error, HttpError::TooManyRedirects(_)));
    }

    #[tokio::test]
    async fn retries_on_server_errors_then_gives_up() {
        let (http, transport) = scripted(vec![
            (503, vec![pair("retry-after", "0")], ""),
            (500, vec![], ""),
            (200, vec![], "ok"),
        ]);
        let response = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(transport.calls.load(Ordering::Relaxed), 3);
        assert_eq!(http.stats().retries, 2);

        let (http, transport) = scripted(vec![
            (503, vec![], ""),
            (503, vec![], ""),
            (503, vec![], ""),
        ]);
        let response = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(transport.calls.load(Ordering::Relaxed), 3);
        assert!(response.error_for_status().is_err());

        let (http, transport) = scripted(vec![(404, vec![], "")]);
        let response = http
            .get(Url::parse("https://a.test/").unwrap())
            .no_retry()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn bodies_are_bounded_and_decoded() {
        let (http, _) = scripted(vec![(200, vec![], "{\"n\": 5}")]);
        let value: serde_json::Value = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap()
            .json(1024)
            .await
            .unwrap();
        assert_eq!(value["n"], 5);
        let (http, _) = scripted(vec![(200, vec![], "0123456789")]);
        let error = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap()
            .bytes(4)
            .await
            .unwrap_err();
        assert!(matches!(error, HttpError::BodyTooLarge { limit: 4, .. }));
        let (http, _) = scripted(vec![(200, vec![], "0123456789")]);
        let (bytes, truncated) = http
            .get(Url::parse("https://a.test/").unwrap())
            .send()
            .await
            .unwrap()
            .bytes_up_to(4)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"0123");
        assert!(truncated);
        assert_eq!(http.stats().bytes_received, 10);
        assert_eq!(http.stats().requests, vec![("a.test".to_string(), 200, 1)]);
    }

    #[tokio::test]
    async fn jars_are_per_platform_and_editable() {
        let (http, transport) = scripted(vec![(200, vec![], ""), (200, vec![], "")]);
        http.with_jar("youtube", |jar| {
            jar.insert(Cookie::new("CONSENT", "YES", ".youtube.com"));
        });
        http.set_jar(
            WEB_PLATFORM,
            Jar::from_cookies(vec![Cookie::new("w", "1", "site.test")]),
        );
        assert_eq!(http.platforms_with_cookies(), vec!["web", "youtube"]);
        http.get(Url::parse("https://www.youtube.com/watch").unwrap())
            .platform("youtube")
            .send()
            .await
            .unwrap();
        http.get(Url::parse("https://site.test/page").unwrap())
            .send()
            .await
            .unwrap();
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen[0].headers["cookie"], "CONSENT=YES");
        assert_eq!(seen[1].headers["cookie"], "w=1");
        drop(seen);
        http.clear_jar("youtube");
        assert_eq!(http.platforms_with_cookies(), vec!["web"]);
        http.set_jar("web", Jar::new());
        assert!(http.platforms_with_cookies().is_empty());
    }

    #[tokio::test]
    async fn seeded_consent_cookies_ride_along_until_the_jar_says_otherwise() {
        let (http, transport) = scripted(vec![(200, vec![], ""), (200, vec![], "")]);
        http.seed_cookies("reddit", vec![Cookie::new("over18", "1", "reddit.com")]);
        assert_eq!(http.seeds("reddit").len(), 1);
        http.get(Url::parse("https://old.reddit.com/r/x.json").unwrap())
            .platform("reddit")
            .send()
            .await
            .unwrap();
        http.with_jar("reddit", |jar| {
            jar.insert(Cookie::new("over18", "0", "reddit.com"));
            jar.insert(Cookie::new("reddit_session", "abc", "reddit.com"));
        });
        http.get(Url::parse("https://old.reddit.com/r/x.json").unwrap())
            .platform("reddit")
            .send()
            .await
            .unwrap();
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen[0].headers["cookie"], "over18=1");
        let second = seen[1].headers["cookie"].to_str().unwrap();
        assert!(second.contains("over18=0"), "{second}");
        assert!(second.contains("reddit_session=abc"), "{second}");
        assert!(!second.contains("over18=1"), "{second}");
        drop(seen);
        http.seed_cookies("reddit", Vec::new());
        assert!(http.seeds("reddit").is_empty());
    }

    #[test]
    fn proxies_pick_platform_then_host_then_default() {
        let proxies = Proxies {
            default: Some(Url::parse("socks5://default:1080").unwrap()),
            platforms: HashMap::from([(
                "tiktok".to_string(),
                Url::parse("http://tiktok-proxy:3128").unwrap(),
            )]),
            hosts: HashMap::from([
                (
                    "youtube.com".to_string(),
                    Url::parse("http://yt:3128").unwrap(),
                ),
                (
                    "m.youtube.com".to_string(),
                    Url::parse("http://myt:3128").unwrap(),
                ),
            ]),
            bypass: vec!["localhost".into(), "internal.test".into()],
        };
        let pick = |platform: Option<&str>, host: &str| {
            proxies.for_request(platform, host).map(|u| u.to_string())
        };
        assert_eq!(
            pick(Some("tiktok"), "www.tiktok.com").unwrap(),
            "http://tiktok-proxy:3128/"
        );
        assert_eq!(pick(None, "www.youtube.com").unwrap(), "http://yt:3128/");
        assert_eq!(pick(None, "m.youtube.com").unwrap(), "http://myt:3128/");
        assert_eq!(pick(None, "example.com").unwrap(), "socks5://default:1080");
        assert!(pick(None, "api.internal.test").is_none());
        assert!(pick(Some("tiktok"), "localhost").is_none());
    }

    #[tokio::test]
    async fn invalid_headers_fail_at_send() {
        let (http, _) = scripted(vec![]);
        let error = http
            .get(Url::parse("https://a.test/").unwrap())
            .header("bad header", "x")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(error, HttpError::Header(_)));
    }
}
