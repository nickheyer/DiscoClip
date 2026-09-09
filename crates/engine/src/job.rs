use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::archive::ArchiveEntry;
use crate::media::LocalFile;
use crate::publish::Published;
use crate::resolve::Resolved;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub source: SourceId,
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub origin: Origin,
    pub url: Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Resolve,
    Download,
    Transcode,
    Publish,
    Archive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum JobStatus {
    Queued,
    Running { stage: Stage },
    Done,
    Failed { stage: Stage, message: String },
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Artifacts {
    pub resolved: Option<Resolved>,
    pub source: Option<LocalFile>,
    pub output: Option<LocalFile>,
    pub published: Option<Published>,
    pub archived: Option<ArchiveEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub request: Request,
    pub status: JobStatus,
    pub artifacts: Artifacts,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
