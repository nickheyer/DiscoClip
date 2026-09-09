use discoclip_engine::{JobId, JobStatus, SourceId};
use jiff::Timestamp;
use serde::Serialize;
use url::Url;

#[derive(Debug, Clone, Serialize)]
pub struct JobSummary {
    pub id: JobId,
    pub url: Url,
    pub source: SourceId,
    pub status: JobStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub version: &'static str,
}
