//! Source-agnostic media pipeline: resolve, download, transcode, publish, archive.

pub mod archive;
pub mod config;
pub mod detect;
pub mod download;
pub mod engine;
pub mod error;
pub mod event;
pub mod job;
pub mod media;
pub mod publish;
pub mod resolve;
pub mod store;
pub mod transcode;

pub use config::EngineConfig;
pub use engine::{Engine, EngineHandle, SubmitError};
pub use event::{EngineEvent, EventKind, Progress};
pub use job::{Job, JobId, JobStatus, Origin, Request, SourceId, Stage};
