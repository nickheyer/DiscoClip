use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::job::{JobId, JobStatus, Request, Stage};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
}

pub type ProgressSender = watch::Sender<Progress>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineEvent {
    pub job: JobId,
    pub at: Timestamp,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventKind {
    Submitted { request: Request },
    Status { status: JobStatus },
    Progress { stage: Stage, progress: Progress },
}
