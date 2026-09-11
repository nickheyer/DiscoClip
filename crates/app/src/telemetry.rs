use tracing_subscriber::EnvFilter;

/// Installs the global tracing subscriber. `RUST_LOG` overrides the configured filter.
pub fn init(filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
