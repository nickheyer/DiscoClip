//! Embedded web UI and JSON/SSE API over the engine.

pub mod api;
pub mod assets;
pub mod config;
pub mod server;

pub use config::UiConfig;
pub use server::UiServer;
