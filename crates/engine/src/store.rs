use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::job::{Job, JobId, SourceId, StatusKind};

pub mod migrate;
pub mod sqlite;

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn insert(&self, job: &Job) -> Result<(), StoreError>;
    async fn update(&self, job: &Job) -> Result<(), StoreError>;
    async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError>;
    async fn list(&self, filter: &JobFilter) -> Result<Vec<Job>, StoreError>;
    async fn count(&self, filter: &JobFilter) -> Result<u64, StoreError>;
    async fn list_active(&self) -> Result<Vec<Job>, StoreError>;
    /// The jobs a playlist job expanded into, oldest first.
    async fn children(&self, parent: JobId) -> Result<Vec<Job>, StoreError>;
    /// Removes a job's record. Whether there was one.
    async fn delete(&self, id: JobId) -> Result<bool, StoreError>;
    /// Removes finished jobs of `kinds` created before `before`. The ids removed.
    async fn purge(
        &self,
        before: Timestamp,
        kinds: &[StatusKind],
    ) -> Result<Vec<JobId>, StoreError>;
    async fn stats(&self) -> Result<Stats, StoreError>;
    /// Counts of jobs created since `since`.
    async fn stats_since(&self, since: Timestamp) -> Result<Stats, StoreError>;
    /// Done and failed counts per resolver, over every job.
    async fn resolver_stats(&self) -> Result<Vec<ResolverStats>, StoreError>;
    /// The resolvers of the jobs a filter admits, each once, in id order
    async fn resolvers(&self, filter: &JobFilter) -> Result<Vec<String>, StoreError>;
    /// Archived done jobs whose link key or platform media key matches, newest first
    async fn find_finished_by_url(
        &self,
        url_key: &str,
        media_key: Option<&str>,
    ) -> Result<Vec<Job>, StoreError>;
    /// Archived done jobs whose source bytes hashed the same, newest first
    async fn find_finished_by_hash(&self, media_hash: &str) -> Result<Vec<Job>, StoreError>;
}

/// How jobs are ordered in a listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    Newest,
    Oldest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JobFilter {
    pub source: Option<SourceId>,
    pub status: Option<StatusKind>,
    /// Jobs created before this moment: the cursor for the next page.
    pub before: Option<Timestamp>,
    pub after: Option<Timestamp>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Matched against the link, the title and the submitter, as a substring.
    pub q: Option<String>,
    pub resolver: Option<String>,
    /// Only jobs of these resolvers. Empty means any.
    pub resolvers: Vec<String>,
    pub parent: Option<JobId>,
    /// Leave out jobs expanded from playlists.
    pub top_level: bool,
    /// Only jobs whose policy publishes them on this content view
    pub view: Option<String>,
    pub media: Option<crate::media::MediaKind>,
    /// Only jobs with an output, done or being published or archived, or running with a live recording
    pub with_output: bool,
    /// Only the newest job of each media, by link or platform media id and clip range, among those admitted
    pub one_per_media: bool,
    pub order: Order,
}

impl JobFilter {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 500;

    pub fn effective_limit(&self) -> usize {
        self.limit
            .unwrap_or(Self::DEFAULT_LIMIT)
            .clamp(1, Self::MAX_LIMIT)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub queued: u64,
    pub running: u64,
    pub done: u64,
    pub failed: u64,
    pub cancelled: u64,
}

impl Stats {
    pub fn total(&self) -> u64 {
        self.queued + self.running + self.done + self.failed + self.cancelled
    }

    pub fn set(&mut self, kind: StatusKind, count: u64) {
        match kind {
            StatusKind::Queued => self.queued = count,
            StatusKind::Running => self.running = count,
            StatusKind::Done => self.done = count,
            StatusKind::Failed => self.failed = count,
            StatusKind::Cancelled => self.cancelled = count,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolverStats {
    pub resolver: String,
    pub done: u64,
    pub failed: u64,
    pub last_done_at: Option<Timestamp>,
    pub last_failed_at: Option<Timestamp>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("database schema: {0}")]
    Schema(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
