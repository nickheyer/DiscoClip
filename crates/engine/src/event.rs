use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::job::{JobId, JobStatus, LogEntry, Request, Stage};
use crate::media::LocalFile;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes on disk so far, for a capture whose file grows as the stream goes on.
    #[serde(default)]
    pub bytes: Option<u64>,
}

impl Progress {
    /// `done` of `total`, with no byte count.
    pub fn of(done: u64, total: Option<u64>) -> Self {
        Self {
            done,
            total,
            bytes: None,
        }
    }

    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|t| *t > 0)
            .map(|t| (self.done as f64 / t as f64).min(1.0))
    }
}

pub type ProgressSender = watch::Sender<Progress>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineEvent {
    pub job: JobId,
    pub at: Timestamp,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A live capture began: the recording it writes from its first byte, playable while
    /// it grows.
    Recording {
        file: Box<LocalFile>,
    },
    /// A person asked the live capture to stop. The recording as it stands becomes the
    /// source and the job goes on.
    Stop,
    /// The job's record was removed.
    Deleted,
}
