//! Logging: a tracing subscriber whose filter can be changed while the server runs.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, reload};

/// Changes the filter of the running subscriber.
#[derive(Clone)]
pub struct LogHandle {
    handle: reload::Handle<EnvFilter, Registry>,
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
    EnvFilter::try_new(filter).map(|_| ()).map_err(|e| e.to_string())
}

/// Installs the global tracing subscriber. `RUST_LOG` overrides the configured filter at
/// startup; a filter set from the app afterwards replaces both.
pub fn init(filter: &str) -> LogHandle {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let (layer, handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(layer)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
    LogHandle { handle }
}
