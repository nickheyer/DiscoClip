//! Source-agnostic media pipeline: resolve, download, transcode, publish, archive.

pub mod archive;
pub mod config;
pub mod detect;
pub mod download;
pub mod engine;
pub mod error;
pub mod event;
pub mod ffmpeg;
pub mod http;
pub mod job;
pub mod js;
pub mod media;
mod pipeline;
pub mod plan;
pub mod publish;
pub mod resolve;
pub mod store;
pub mod transcode;

pub use reqwest;
pub use rusqlite;

pub use config::{EngineConfig, Limits, LiveConfig, PlaylistConfig, RetentionConfig};
pub use engine::{
    CancelError, DeleteError, Engine, EngineBuilder, EngineError, EngineHandle, PlatformSession,
    RetryError, SubmitError, Utilisation,
};
pub use event::{EngineEvent, EventKind, Progress};
pub use http::{Http, HttpConfig};
pub use job::{
    Job, JobId, JobStatus, LogEntry, Origin, Request, RequestLimits, RequestOptions, SourceId,
    Stage, StatusKind, SubtitleMode,
};
pub use resolve::{Platform, Resolution, Resolved, SessionCheck, SessionSupport};
pub use store::{JobFilter, JobStore, Order, ResolverStats, Stats, StoreError};
