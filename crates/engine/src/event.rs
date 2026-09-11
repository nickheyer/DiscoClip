use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::job::{JobId, JobStatus, LogEntry, Request, Stage};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
}

impl Progress {
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|t| *t > 0)
            .map(|t| (self.done as f64 / t as f64).min(1.0))
    }
}

pub type ProgressSender = watch::Sender<Progress>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineEvent {
    pub job: JobId,
    pub at: Timestamp,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventKind {
    Submitted {
        request: Box<Request>,
    },
    Status {
        status: JobStatus,
    },
    Progress {
        stage: Stage,
        progress: Progress,
    },
    Log {
        entry: LogEntry,
    },
    /// A playlist link expanded into these jobs.
    Children {
        ids: Vec<JobId>,
    },
    /// The job's record was removed.
    Deleted,
}
