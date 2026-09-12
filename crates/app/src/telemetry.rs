//! Logging: a tracing subscriber whose filter can be changed while the server runs, and
//! which keeps the last lines it wrote so the app can show them and follow new ones.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, reload};

/// How many lines the buffer keeps; older ones fall off the end.
pub const LOG_CAPACITY: usize = 5000;
/// How many lines a slow follower may fall behind before it skips ahead.
const FEED_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::TRACE => LogLevel::Trace,
            Level::DEBUG => LogLevel::Debug,
            Level::INFO => LogLevel::Info,
            Level::WARN => LogLevel::Warn,
            Level::ERROR => LogLevel::Error,
        }
    }
}

impl LogLevel {
    pub const ALL: [LogLevel; 5] = [
        LogLevel::Trace,
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        LogLevel::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(text.trim()))
            .ok_or_else(|| format!("{text:?} is not a log level"))
    }
}

/// One line the subscriber wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    /// Rises by one per line for the life of the process.
    pub id: u64,
    pub at: Timestamp,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

/// Which lines to read: at `level` or above, whose target contains `target`, and whose
/// message or fields contain `q`; the `limit` newest ones before line `before`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    pub level: Option<LogLevel>,
    pub target: Option<String>,
    pub q: Option<String>,
    pub before: Option<u64>,
    pub limit: usize,
}

impl LogFilter {
    pub fn matches(&self, line: &LogLine) -> bool {
        if self.level.is_some_and(|level| line.level < level) {
            return false;
        }
        if let Some(target) = &self.target
            && !contains_ignoring_case(&line.target, target)
        {
            return false;
        }
        if let Some(q) = &self.q
            && !contains_ignoring_case(&line.message, q)
            && !line
                .fields
                .iter()
                .any(|(k, v)| contains_ignoring_case(k, q) || contains_ignoring_case(v, q))
        {
            return false;
        }
        true
    }
}

fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.trim();
    needle.is_empty() || haystack.to_lowercase().contains(&needle.to_lowercase())
}

struct Buffered {
    lines: Mutex<VecDeque<Arc<LogLine>>>,
    next_id: AtomicU64,
    feed: broadcast::Sender<Arc<LogLine>>,
}

/// The last [`LOG_CAPACITY`] lines the subscriber wrote, and a feed of each new one.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Buffered>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        let (feed, _) = broadcast::channel(FEED_CAPACITY);
        Self {
            inner: Arc::new(Buffered {
                lines: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
                next_id: AtomicU64::new(1),
                feed,
            }),
        }
    }

    /// Keeps a line and hands it to every follower.
    pub fn record(
        &self,
        level: LogLevel,
        target: &str,
        message: String,
        fields: BTreeMap<String, String>,
    ) -> Arc<LogLine> {
        let line = Arc::new(LogLine {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            at: Timestamp::now(),
            level,
            target: target.to_string(),
            message,
            fields,
        });
        {
            let mut lines = self.inner.lines.lock().unwrap_or_else(|e| e.into_inner());
            if lines.len() == LOG_CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line.clone());
        }
        let _ = self.inner.feed.send(line.clone());
        line
    }

    /// The lines `filter` asks for, newest first.
    pub fn query(&self, filter: &LogFilter) -> Vec<Arc<LogLine>> {
        let lines = self.inner.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines
            .iter()
            .rev()
            .filter(|line| filter.before.is_none_or(|before| line.id < before))
            .filter(|line| filter.matches(line))
            .take(filter.limit.max(1))
            .cloned()
            .collect()
    }

    /// New lines as they are written.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<LogLine>> {
        self.inner.feed.subscribe()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The id of the oldest line kept, when there is one.
    pub fn oldest_id(&self) -> Option<u64> {
        self.inner
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .front()
            .map(|line| line.id)
    }
}

/// The message and fields of one event, as text.
#[derive(Default)]
struct Fields {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, value.to_string());
    }
}

/// Copies every event that passes the filter into the buffer.
struct Capture {
    buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        self.buffer.record(
            (*meta.level()).into(),
            meta.target(),
            fields.message,
            fields.fields,
        );
    }
}

/// Changes the filter of the running subscriber, and reads what it wrote.
#[derive(Clone)]
pub struct LogHandle {
    handle: reload::Handle<EnvFilter, Registry>,
    pub buffer: LogBuffer,
}

impl LogHandle {
    /// Applies `filter`, a tracing directive such as `info` or
    /// `info,discoclip_engine=debug`.
    pub fn set(&self, filter: &str) -> Result<(), String> {
        let filter = EnvFilter::try_new(filter).map_err(|e| e.to_string())?;
        self.handle.reload(filter).map_err(|e| e.to_string())
    }
}

/// Whether `filter` is a tracing directive the subscriber takes.
pub fn check_filter(filter: &str) -> Result<(), String> {
    EnvFilter::try_new(filter)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A filter handle that no subscriber reads, with a buffer nothing writes to but the
/// tests, for tests of the parts that change and read them. The layer behind the handle
/// is kept for the life of the process, as the global subscriber keeps the real one.
pub fn detached(filter: &str) -> LogHandle {
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let (layer, handle): (reload::Layer<EnvFilter, Registry>, _) = reload::Layer::new(filter);
    std::mem::forget(layer);
    LogHandle {
        handle,
        buffer: LogBuffer::new(),
    }
}

/// Installs the global tracing subscriber. `RUST_LOG` overrides the configured filter at
/// startup; a filter set from the app afterwards replaces both. Every line written is
/// also kept in the handle's buffer.
pub fn init(filter: &str) -> LogHandle {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let (layer, handle) = reload::Layer::new(filter);
    let buffer = LogBuffer::new();
    tracing_subscriber::registry()
        .with(layer)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(Capture {
            buffer: buffer.clone(),
        })
        .init();
    LogHandle { handle, buffer }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(buffer: &LogBuffer, level: LogLevel, target: &str, message: &str) -> u64 {
        buffer
            .record(level, target, message.into(), BTreeMap::new())
            .id
    }

    #[test]
    fn the_buffer_keeps_the_newest_lines_and_filters_them() {
        let buffer = LogBuffer::new();
        let mut feed = buffer.subscribe();
        let first = line(
            &buffer,
            LogLevel::Info,
            "discoclip::web",
            "web app listening",
        );
        line(
            &buffer,
            LogLevel::Debug,
            "discoclip_engine::http",
            "retrying",
        );
        let warned = buffer.record(
            LogLevel::Warn,
            "discoclip::bots",
            "bot disconnected".into(),
            BTreeMap::from([("application".to_string(), "Clipper".to_string())]),
        );
        line(&buffer, LogLevel::Error, "discoclip::web", "could not bind");
        assert_eq!(buffer.len(), 4);
        assert_eq!(buffer.oldest_id(), Some(first));
        assert_eq!(feed.try_recv().unwrap().id, first);

        let all = buffer.query(&LogFilter {
            limit: 10,
            ..LogFilter::default()
        });
        assert_eq!(all.len(), 4);
        assert!(
            all.windows(2).all(|pair| pair[0].id > pair[1].id),
            "newest first"
        );

        let warnings = buffer.query(&LogFilter {
            level: Some(LogLevel::Warn),
            limit: 10,
            ..LogFilter::default()
        });
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|l| l.level >= LogLevel::Warn));

        let web = buffer.query(&LogFilter {
            target: Some("::WEB".into()),
            limit: 10,
            ..LogFilter::default()
        });
        assert_eq!(web.len(), 2);

        let clipper = buffer.query(&LogFilter {
            q: Some("clipper".into()),
            limit: 10,
            ..LogFilter::default()
        });
        assert_eq!(clipper.len(), 1);
        assert_eq!(clipper[0].id, warned.id);

        let older = buffer.query(&LogFilter {
            before: Some(warned.id),
            limit: 1,
            ..LogFilter::default()
        });
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].message, "retrying");

        for i in 0..LOG_CAPACITY {
            line(&buffer, LogLevel::Trace, "t", &i.to_string());
        }
        assert_eq!(buffer.len(), LOG_CAPACITY);
        assert!(buffer.oldest_id().unwrap() > first);
        assert!(
            "WARN"
                .parse::<LogLevel>()
                .is_ok_and(|l| l == LogLevel::Warn)
        );
        assert!("loud".parse::<LogLevel>().is_err());
    }

    #[test]
    fn the_capture_layer_records_events_with_their_fields() {
        let buffer = LogBuffer::new();
        let subscriber = tracing_subscriber::registry().with(Capture {
            buffer: buffer.clone(),
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(job = "abc", size = 5, "link submitted");
            tracing::warn!(error = %std::io::Error::other("boom"), "publishing failed");
        });
        let lines = buffer.query(&LogFilter {
            limit: 10,
            ..LogFilter::default()
        });
        assert_eq!(lines.len(), 2);
        let submitted = &lines[1];
        assert_eq!(submitted.level, LogLevel::Info);
        assert_eq!(submitted.message, "link submitted");
        assert_eq!(submitted.fields["job"], "abc");
        assert_eq!(submitted.fields["size"], "5");
        assert!(submitted.target.starts_with("discoclip::telemetry"));
        assert_eq!(lines[0].fields["error"], "boom");
    }
}
