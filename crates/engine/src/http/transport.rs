//! Where requests actually go: the network, a recorded fixture, or the network while a
//! fixture is being recorded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use super::HttpError;

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, HttpError>> + Send>>;

/// One request as the transport sees it: cookies, retries and redirects are already decided.
pub struct TransportRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    /// Bound on the whole exchange, body included; `None` for media downloads.
    pub timeout: Option<Duration>,
    pub proxy: Option<Url>,
    /// Sent as a browser would: with Chrome's TLS and HTTP/2 fingerprint and its default
    /// headers, for hosts that refuse any other client.
    pub impersonate: bool,
}

pub struct TransportResponse {
    pub status: StatusCode,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: BodyStream,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError>;
    fn name(&self) -> &'static str;
    /// Takes new connection and read timeouts for the requests that follow.
    fn configure(&self, _connect_timeout: Duration, _read_timeout: Duration) {}
}

/// The browser impersonated requests pass for.
const EMULATION: wreq_util::Profile = wreq_util::Emulation::Chrome142;

/// The network, through reqwest, one client per proxy; impersonated requests go through
/// wreq, which speaks TLS and HTTP/2 the way Chrome does, one client per proxy as well.
pub struct LiveTransport {
    timeouts: Mutex<(Duration, Duration)>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    browsers: Mutex<HashMap<String, wreq::Client>>,
}

impl LiveTransport {
    pub fn new(connect_timeout: Duration, read_timeout: Duration) -> Self {
        Self {
            timeouts: Mutex::new((connect_timeout, read_timeout)),
            clients: Mutex::new(HashMap::new()),
            browsers: Mutex::new(HashMap::new()),
        }
    }

    fn browser(&self, proxy: Option<&Url>) -> Result<wreq::Client, HttpError> {
        let key = proxy.map(|p| p.to_string()).unwrap_or_default();
        let (connect_timeout, read_timeout) = self.timeouts();
        let mut browsers = self.browsers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = browsers.get(&key) {
            return Ok(client.clone());
        }
        let mut builder = wreq::Client::builder()
            .emulation(EMULATION)
            .redirect(wreq::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout);
        if let Some(proxy) = proxy {
            builder = builder.proxy(
                wreq::Proxy::all(proxy.as_str())
                    .map_err(|_| HttpError::Proxy(proxy.to_string()))?,
            );
        }
        let client = builder
            .build()
            .map_err(|e| HttpError::Build(e.to_string()))?;
        browsers.insert(key, client.clone());
        Ok(client)
    }

    async fn send_as_browser(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, HttpError> {
        let client = self.browser(request.proxy.as_ref())?;
        let url = request.url.clone();
        let mut builder = client
            .request(request.method, request.url.as_str())
            .headers(request.headers);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| HttpError::from_wreq(&url, e))?;
        let final_url = Url::parse(&response.uri().to_string()).unwrap_or_else(|_| url.clone());
        let stream_url = final_url.clone();
        Ok(TransportResponse {
            status: response.status(),
            url: final_url,
            headers: response.headers().clone(),
            body: Box::pin(
                response
                    .bytes_stream()
                    .map_err(move |e| HttpError::from_wreq(&stream_url, e)),
            ),
        })
    }

    pub fn timeouts(&self) -> (Duration, Duration) {
        *self.timeouts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn client(&self, proxy: Option<&Url>) -> Result<reqwest::Client, HttpError> {
        let key = proxy.map(|p| p.to_string()).unwrap_or_default();
        let (connect_timeout, read_timeout) = self.timeouts();
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout);
        if let Some(proxy) = proxy {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.as_str())
                    .map_err(|_| HttpError::Proxy(proxy.to_string()))?,
            );
        }
        let client = builder
            .build()
            .map_err(|e| HttpError::Build(e.to_string()))?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

#[async_trait]
impl Transport for LiveTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
        if request.impersonate {
            return self.send_as_browser(request).await;
        }
        let client = self.client(request.proxy.as_ref())?;
        let url = request.url.clone();
        let mut builder = client
            .request(request.method, request.url)
            .headers(request.headers);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| HttpError::from_reqwest(&url, e))?;
        let final_url = response.url().clone();
        let stream_url = final_url.clone();
        Ok(TransportResponse {
            status: response.status(),
            url: final_url,
            headers: response.headers().clone(),
            body: Box::pin(
                response
                    .bytes_stream()
                    .map_err(move |e| HttpError::from_reqwest(&stream_url, e)),
            ),
        })
    }

    fn name(&self) -> &'static str {
        "live"
    }

    /// Clients built with the old timeouts are dropped; the next request builds new ones.
    fn configure(&self, connect_timeout: Duration, read_timeout: Duration) {
        let mut timeouts = self.timeouts.lock().unwrap_or_else(|e| e.into_inner());
        if *timeouts == (connect_timeout, read_timeout) {
            return;
        }
        *timeouts = (connect_timeout, read_timeout);
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.browsers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// A recorded set of exchanges a resolver made for one URL, replayed in tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub recorded_at: Timestamp,
    /// The URL the recording resolved, so the test knows what to ask for.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub exchanges: Vec<Exchange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<RecordedBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: RecordedBody,
    /// The body was longer than the recorder keeps.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedBody {
    #[default]
    Empty,
    Text(String),
    Base64(String),
}

impl RecordedBody {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return RecordedBody::Empty;
        }
        match std::str::from_utf8(bytes) {
            Ok(text) if !text.bytes().any(|b| b < 0x09 || (b > 0x0d && b < 0x20)) => {
                RecordedBody::Text(text.to_string())
            }
            _ => RecordedBody::Base64(STANDARD.encode(bytes)),
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        match self {
            RecordedBody::Empty => Bytes::new(),
            RecordedBody::Text(text) => Bytes::from(text.clone()),
            RecordedBody::Base64(encoded) => {
                Bytes::from(STANDARD.decode(encoded).unwrap_or_default())
            }
        }
    }
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn header_map(pairs: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.append(name, value);
        }
    }
    map
}

impl Fixture {
    pub fn new(name: impl Into<String>, url: Option<&Url>) -> Self {
        Self {
            name: name.into(),
            recorded_at: Timestamp::now(),
            url: url.map(|u| u.to_string()),
            notes: None,
            exchanges: Vec::new(),
        }
    }

    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

/// `scheme://host/path` with the query sorted, so parameter order does not matter.
fn normalized(url: &str) -> String {
    match Url::parse(url) {
        Ok(parsed) => {
            let mut pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
            pairs.sort();
            let query = pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            format!(
                "{}://{}{}?{query}",
                parsed.scheme(),
                parsed.host_str().unwrap_or(""),
                parsed.path()
            )
        }
        Err(_) => url.to_string(),
    }
}

fn host_path(url: &str) -> String {
    match Url::parse(url) {
        Ok(parsed) => format!("{}{}", parsed.host_str().unwrap_or(""), parsed.path()),
        Err(_) => url.to_string(),
    }
}

/// Serves recorded exchanges: the same request, in the recorded order where it repeats.
pub struct ReplayTransport {
    fixture: Fixture,
    used: Mutex<Vec<bool>>,
}

impl ReplayTransport {
    pub fn new(fixture: Fixture) -> Self {
        let used = vec![false; fixture.exchanges.len()];
        Self {
            fixture,
            used: Mutex::new(used),
        }
    }

    pub fn fixture(&self) -> &Fixture {
        &self.fixture
    }

    /// Exchanges no request has asked for.
    pub fn unused(&self) -> Vec<&Exchange> {
        let used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        self.fixture
            .exchanges
            .iter()
            .zip(used.iter())
            .filter(|(_, used)| !**used)
            .map(|(exchange, _)| exchange)
            .collect()
    }

    fn pick(&self, method: &Method, url: &Url) -> Option<usize> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        let wanted = url.to_string();
        let wanted_normalized = normalized(&wanted);
        let wanted_path = host_path(&wanted);
        let candidates = |strict: bool, allow_used: bool| -> Option<usize> {
            self.fixture
                .exchanges
                .iter()
                .enumerate()
                .filter(|(index, exchange)| {
                    exchange
                        .request
                        .method
                        .eq_ignore_ascii_case(method.as_str())
                        && (allow_used || !used[*index])
                        && if strict {
                            exchange.request.url == wanted
                                || normalized(&exchange.request.url) == wanted_normalized
                        } else {
                            host_path(&exchange.request.url) == wanted_path
                        }
                })
                .map(|(index, _)| index)
                .next()
        };
        let index = candidates(true, false)
            .or_else(|| candidates(true, true))
            .or_else(|| candidates(false, false))
            .or_else(|| candidates(false, true))?;
        used[index] = true;
        Some(index)
    }
}

#[async_trait]
impl Transport for ReplayTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
        let index =
            self.pick(&request.method, &request.url)
                .ok_or_else(|| HttpError::NoFixture {
                    method: request.method.to_string(),
                    url: request.url.to_string(),
                })?;
        let recorded = &self.fixture.exchanges[index].response;
        let url = Url::parse(&recorded.response_url_or(&request.url))
            .unwrap_or_else(|_| request.url.clone());
        let bytes = recorded.body.to_bytes();
        Ok(TransportResponse {
            status: StatusCode::from_u16(recorded.status).unwrap_or(StatusCode::OK),
            url,
            headers: header_map(&recorded.headers),
            body: Box::pin(futures::stream::once(async move { Ok(bytes) })),
        })
    }

    fn name(&self) -> &'static str {
        "replay"
    }
}

impl RecordedResponse {
    fn response_url_or(&self, fallback: &Url) -> String {
        if self.url.is_empty() {
            fallback.to_string()
        } else {
            self.url.clone()
        }
    }
}

const RECORD_BODY_LIMIT: usize = 8 * 1024 * 1024;
const PASSTHROUGH_LIMIT: usize = 512 * 1024 * 1024;
const REDACTED: &str = "<redacted>";

/// Forwards to another transport and writes what passed to a fixture file. Cookies and
/// credentials are redacted; bodies longer than eight megabytes are cut, and marked so.
pub struct RecordingTransport {
    inner: Arc<dyn Transport>,
    path: PathBuf,
    fixture: Mutex<Fixture>,
}

impl RecordingTransport {
    pub fn new(inner: Arc<dyn Transport>, path: PathBuf, fixture: Fixture) -> Self {
        Self {
            inner,
            path,
            fixture: Mutex::new(fixture),
        }
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let fixture = self.fixture.lock().unwrap_or_else(|e| e.into_inner());
        fixture.save(&self.path)
    }

    pub fn exchanges(&self) -> usize {
        self.fixture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exchanges
            .len()
    }
}

fn is_secret_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cookie"
            | "authorization"
            | "set-cookie"
            | "x-csrf-token"
            | "x-csrftoken"
            | "x-guest-token"
    )
}

fn redacted(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .map(|(name, value)| {
            if is_secret_header(&name) {
                (name, REDACTED.to_string())
            } else {
                (name, value)
            }
        })
        .collect()
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
        let recorded_request = RecordedRequest {
            method: request.method.to_string(),
            url: request.url.to_string(),
            headers: redacted(header_pairs(&request.headers)),
            body: request.body.as_ref().map(|b| RecordedBody::from_bytes(b)),
        };
        let response = self.inner.send(request).await?;
        let mut body = Vec::new();
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > PASSTHROUGH_LIMIT {
                return Err(HttpError::BodyTooLarge {
                    url: response.url.to_string(),
                    limit: PASSTHROUGH_LIMIT,
                });
            }
            body.extend_from_slice(&chunk);
        }
        let truncated = body.len() > RECORD_BODY_LIMIT;
        let kept = &body[..body.len().min(RECORD_BODY_LIMIT)];
        let recorded_response = RecordedResponse {
            status: response.status.as_u16(),
            url: response.url.to_string(),
            headers: redacted(header_pairs(&response.headers)),
            body: RecordedBody::from_bytes(kept),
            truncated,
        };
        self.fixture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exchanges
            .push(Exchange {
                request: recorded_request,
                response: recorded_response,
            });
        let bytes = Bytes::from(body);
        Ok(TransportResponse {
            status: response.status,
            url: response.url,
            headers: response.headers,
            body: Box::pin(futures::stream::once(async move { Ok(bytes) })),
        })
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

impl Drop for RecordingTransport {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            tracing::error!("could not save fixture {}: {error}", self.path.display());
        }
    }
}

/// A host for tests: answers by URL from a table, in bodies of a thousand bytes at a time,
/// serving byte ranges, breaking transfers where scripted, and changing an answer from one
/// request to the next.
#[cfg(test)]
pub mod site {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, StatusCode};

    use super::{Transport, TransportRequest, TransportResponse};
    use crate::http::HttpError;

    /// One answer: what the host says for a URL until the next answer in its series.
    #[derive(Debug, Clone)]
    pub struct Reply {
        pub status: u16,
        pub content_type: String,
        pub body: Vec<u8>,
        pub etag: Option<String>,
    }

    impl Reply {
        pub fn new(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                content_type: content_type.to_string(),
                body: body.into(),
                etag: None,
            }
        }

        pub fn status(mut self, status: u16) -> Self {
            self.status = status;
            self
        }

        pub fn etag(mut self, etag: &str) -> Self {
            self.etag = Some(etag.to_string());
            self
        }
    }

    /// A request as the host saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Seen {
        pub url: String,
        pub range: Option<String>,
        pub if_range: Option<String>,
    }

    #[derive(Default)]
    pub struct Site {
        /// Each URL's answers in order; the last one repeats.
        replies: Mutex<HashMap<String, Vec<Reply>>>,
        hits: Mutex<HashMap<String, usize>>,
        /// Whether ranged requests are honoured.
        ranges: bool,
        /// By URL and the byte a request starts at, how many bytes arrive before the
        /// connection drops; each entry once.
        breaks: Mutex<Vec<(String, u64, usize)>>,
        seen: Mutex<Vec<Seen>>,
    }

    impl Site {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                ranges: true,
                ..Self::default()
            })
        }

        /// A host that answers every request with the whole file.
        pub fn without_ranges() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn put(&self, url: &str, reply: Reply) {
            self.replies
                .lock()
                .unwrap()
                .insert(url.to_string(), vec![reply]);
        }

        /// Answers `url` with each reply in turn, repeating the last.
        pub fn put_series(&self, url: &str, replies: Vec<Reply>) {
            assert!(!replies.is_empty());
            self.replies
                .lock()
                .unwrap()
                .insert(url.to_string(), replies);
        }

        pub fn put_text(&self, url: &str, content_type: &str, text: &str) {
            self.put(url, Reply::new(content_type, text.as_bytes().to_vec()));
        }

        pub fn put_bytes(&self, url: &str, content_type: &str, bytes: &[u8]) {
            self.put(url, Reply::new(content_type, bytes.to_vec()));
        }

        pub fn remove(&self, url: &str) {
            self.replies.lock().unwrap().remove(url);
        }

        pub fn break_at(&self, url: &str, start: u64, after: usize) {
            self.breaks
                .lock()
                .unwrap()
                .push((url.to_string(), start, after));
        }

        pub fn hits(&self, url: &str) -> usize {
            self.hits.lock().unwrap().get(url).copied().unwrap_or(0)
        }

        pub fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// The Range headers of every request for `url`, in order.
        pub fn ranges_seen(&self, url: &str) -> Vec<String> {
            self.seen()
                .into_iter()
                .filter(|s| s.url == url)
                .filter_map(|s| s.range)
                .collect()
        }

        fn reply_for(&self, url: &str) -> Option<Reply> {
            let mut hits = self.hits.lock().unwrap();
            let hit = hits.entry(url.to_string()).or_insert(0);
            let replies = self.replies.lock().unwrap();
            let series = replies.get(url)?;
            let reply = series[(*hit).min(series.len() - 1)].clone();
            *hit += 1;
            Some(reply)
        }
    }

    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    #[async_trait]
    impl Transport for Site {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
            let url = request.url.to_string();
            let range = header(&request.headers, "range");
            let if_range = header(&request.headers, "if-range");
            self.seen.lock().unwrap().push(Seen {
                url: url.clone(),
                range: range.clone(),
                if_range: if_range.clone(),
            });
            let Some(reply) = self.reply_for(&url) else {
                let mut headers = HeaderMap::new();
                headers.insert("content-length", HeaderValue::from_static("0"));
                return Ok(TransportResponse {
                    status: StatusCode::NOT_FOUND,
                    url: request.url,
                    headers,
                    body: Box::pin(futures::stream::empty()),
                });
            };
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                HeaderValue::from_str(&reply.content_type).unwrap(),
            );
            if let Some(etag) = &reply.etag {
                headers.insert("etag", HeaderValue::from_str(etag).unwrap());
            }
            let asked = range
                .as_deref()
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|r| r.split_once('-'))
                .map(|(start, end)| (start.parse::<u64>().unwrap(), end.parse::<u64>().ok()));
            let valid = match (&if_range, &reply.etag) {
                (Some(asked), Some(etag)) => asked == etag,
                (Some(_), None) => false,
                (None, _) => true,
            };
            let body = reply.body;
            let (status, start, slice): (u16, u64, Vec<u8>) = match asked {
                Some((start, end)) if self.ranges && valid && reply.status == 200 => {
                    let len = body.len() as u64;
                    if start >= len {
                        headers.insert(
                            "content-range",
                            HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                        );
                        (416, start, Vec::new())
                    } else {
                        let end = end.unwrap_or(len - 1).min(len - 1);
                        headers.insert(
                            "content-range",
                            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).unwrap(),
                        );
                        (206, start, body[start as usize..=end as usize].to_vec())
                    }
                }
                _ => (reply.status, 0, body),
            };
            headers.insert(
                "content-length",
                HeaderValue::from_str(&slice.len().to_string()).unwrap(),
            );
            let cut = {
                let mut breaks = self.breaks.lock().unwrap();
                breaks
                    .iter()
                    .position(|(u, at, _)| *u == url && *at == start)
                    .map(|index| breaks.remove(index).2)
            };
            let mut items: Vec<Result<Bytes, HttpError>> = Vec::new();
            let mut sent = 0usize;
            for piece in slice.chunks(1000) {
                if let Some(cut) = cut
                    && sent + piece.len() > cut
                {
                    items.push(Ok(Bytes::copy_from_slice(&piece[..cut - sent])));
                    items.push(Err(HttpError::Transport {
                        url: url.clone(),
                        message: "connection reset".into(),
                    }));
                    break;
                }
                items.push(Ok(Bytes::copy_from_slice(piece)));
                sent += piece.len();
            }
            Ok(TransportResponse {
                status: StatusCode::from_u16(status).unwrap(),
                url: request.url,
                headers,
                body: Box::pin(futures::stream::iter(items)),
            })
        }

        fn name(&self) -> &'static str {
            "site"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                headers: vec![("content-type".into(), "text/plain".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    async fn ask(transport: &ReplayTransport, method: Method, url: &str) -> (u16, String) {
        let response = transport
            .send(TransportRequest {
                method,
                url: Url::parse(url).unwrap(),
                headers: HeaderMap::new(),
                body: None,
                timeout: None,
                proxy: None,
                impersonate: false,
            })
            .await
            .unwrap();
        let mut body = Vec::new();
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.unwrap());
        }
        (response.status.as_u16(), String::from_utf8(body).unwrap())
    }

    #[tokio::test]
    async fn replay_matches_exact_then_loose_and_in_order() {
        let mut fixture = Fixture::new("test", None);
        fixture.exchanges = vec![
            exchange("GET", "https://a.test/page?b=2&a=1", 200, "first"),
            exchange("GET", "https://a.test/page?b=2&a=1", 200, "second"),
            exchange("GET", "https://a.test/other?x=1", 404, "gone"),
            exchange("POST", "https://a.test/api", 200, "posted"),
        ];
        let transport = ReplayTransport::new(fixture);
        assert_eq!(
            ask(&transport, Method::GET, "https://a.test/page?a=1&b=2").await,
            (200, "first".into())
        );
        assert_eq!(
            ask(&transport, Method::GET, "https://a.test/page?a=1&b=2").await,
            (200, "second".into())
        );
        // Exhausted exact matches are reused rather than failing.
        assert_eq!(
            ask(&transport, Method::GET, "https://a.test/page?a=1&b=2")
                .await
                .1,
            "first"
        );
        // A different query on the same path falls back to the path match.
        assert_eq!(
            ask(&transport, Method::GET, "https://a.test/other?x=9").await,
            (404, "gone".into())
        );
        assert_eq!(
            ask(&transport, Method::POST, "https://a.test/api").await.1,
            "posted"
        );
        let missing = transport
            .send(TransportRequest {
                method: Method::GET,
                url: Url::parse("https://b.test/").unwrap(),
                headers: HeaderMap::new(),
                body: None,
                timeout: None,
                proxy: None,
                impersonate: false,
            })
            .await;
        assert!(matches!(missing, Err(HttpError::NoFixture { .. })));
        assert!(transport.unused().is_empty());
    }

    #[test]
    fn bodies_and_fixtures_round_trip_through_json() {
        let text = RecordedBody::from_bytes(b"{\"a\":1}\n");
        assert!(matches!(text, RecordedBody::Text(_)));
        let binary = RecordedBody::from_bytes(&[0, 159, 146, 150]);
        assert!(matches!(binary, RecordedBody::Base64(_)));
        assert_eq!(binary.to_bytes().as_ref(), &[0, 159, 146, 150]);
        assert!(matches!(RecordedBody::from_bytes(b""), RecordedBody::Empty));
        let mut fixture = Fixture::new("x", Some(&Url::parse("https://a.test/v").unwrap()));
        fixture
            .exchanges
            .push(exchange("GET", "https://a.test/v", 200, "hi"));
        let json = serde_json::to_string(&fixture).unwrap();
        let back = Fixture::parse(&json).unwrap();
        assert_eq!(back.exchanges.len(), 1);
        assert_eq!(back.url.as_deref(), Some("https://a.test/v"));
        let dir = std::env::temp_dir().join(format!("discoclip-fixture-{}", uuid::Uuid::now_v7()));
        let path = dir.join("nested").join("x.json");
        fixture.save(&path).unwrap();
        assert_eq!(Fixture::load(&path).unwrap().name, "x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn secrets_are_redacted() {
        let pairs = redacted(vec![
            ("Cookie".into(), "sid=1".into()),
            ("accept".into(), "*/*".into()),
            ("Authorization".into(), "Bearer x".into()),
        ]);
        assert_eq!(pairs[0].1, REDACTED);
        assert_eq!(pairs[1].1, "*/*");
        assert_eq!(pairs[2].1, REDACTED);
    }
}
