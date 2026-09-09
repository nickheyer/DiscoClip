use async_trait::async_trait;
use jiff::Timestamp;

use crate::job::{Job, JobId, SourceId};

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn insert(&self, job: &Job) -> Result<(), StoreError>;
    async fn update(&self, job: &Job) -> Result<(), StoreError>;
    async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError>;
    async fn list(&self, filter: &JobFilter) -> Result<Vec<Job>, StoreError>;
}

#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub source: Option<SourceId>,
    pub before: Option<Timestamp>,
    pub limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("job {0:?} not found")]
    NotFound(JobId),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
