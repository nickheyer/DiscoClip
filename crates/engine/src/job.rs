use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::Limits;
use url::Url;
use uuid::Uuid;

use crate::archive::ArchiveEntry;
use crate::download::LocalSubtitle;
use crate::media::LocalFile;
use crate::publish::Published;
use crate::resolve::{ClipRange, Resolved};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for JobId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

impl SourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub source: SourceId,
    pub reference: String,
    pub url: Option<Url>,
}

/// Limits tighter than the engine's own, for one request; `None` leaves the engine's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestLimits {
    pub max_source_bytes: Option<u64>,
    pub max_duration_secs: Option<u64>,
    pub max_height: Option<u32>,
}

impl RequestLimits {
    /// The engine's `limits` tightened by these.
    pub fn applied_to(&self, limits: &Limits) -> Limits {
        Limits {
            max_source_bytes: self
                .max_source_bytes
                .map_or(limits.max_source_bytes, |mine| {
                    mine.min(limits.max_source_bytes)
                }),
            max_duration_secs: match (self.max_duration_secs, limits.max_duration_secs) {
                (Some(mine), Some(theirs)) => Some(mine.min(theirs)),
                (mine, theirs) => mine.or(theirs),
            },
            max_height: self
                .max_height
                .map_or(limits.max_height, |mine| mine.min(limits.max_height)),
        }
    }
}

/// What to do with subtitles the source offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    /// Fetched and kept beside the output, never rendered.
    #[default]
    Keep,
    /// Rendered into the picture.
    Burn,
    /// Ignored.
    Skip,
}

/// Choices made for one request beyond its limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestOptions {
    /// The portion of the media wanted; overrides what the link itself names.
    pub clip: Option<ClipRange>,
    pub subtitles: SubtitleMode,
    /// The subtitle language preferred when several are offered.
    pub subtitle_language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub origin: Origin,
    pub url: Url,
    /// Where the publisher for the origin's source should post, in that source's terms,
    /// when not back at the origin.
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub limits: RequestLimits,
    #[serde(default)]
    pub options: RequestOptions,
    /// The playlist job this one was expanded from.
    #[serde(default)]
    pub parent: Option<JobId>,
    /// The job this one repeats.
    #[serde(default)]
    pub retry_of: Option<JobId>,
    /// Who submitted the link, as the source names them.
    #[serde(default)]
    pub submitted_by: Option<String>,
}

impl Request {
    pub fn new(origin: Origin, url: Url) -> Self {
        Self {
            origin,
            url,
            destination: None,
            limits: RequestLimits::default(),
            options: RequestOptions::default(),
            parent: None,
            retry_of: None,
            submitted_by: None,
        }
    }
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

impl Stage {
    pub const ALL: [Stage; 5] = [
        Stage::Resolve,
        Stage::Download,
        Stage::Transcode,
        Stage::Publish,
        Stage::Archive,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Resolve => "resolve",
            Stage::Download => "download",
            Stage::Transcode => "transcode",
            Stage::Publish => "publish",
            Stage::Archive => "archive",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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

impl JobStatus {
    pub fn kind(&self) -> StatusKind {
        match self {
            JobStatus::Queued => StatusKind::Queued,
            JobStatus::Running { .. } => StatusKind::Running,
            JobStatus::Done => StatusKind::Done,
            JobStatus::Failed { .. } => StatusKind::Failed,
            JobStatus::Cancelled => StatusKind::Cancelled,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl StatusKind {
    pub const ALL: [StatusKind; 5] = [
        StatusKind::Queued,
        StatusKind::Running,
        StatusKind::Done,
        StatusKind::Failed,
        StatusKind::Cancelled,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            StatusKind::Queued => "queued",
            StatusKind::Running => "running",
            StatusKind::Done => "done",
            StatusKind::Failed => "failed",
            StatusKind::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StatusKind::Done | StatusKind::Failed | StatusKind::Cancelled
        )
    }
}

impl fmt::Display for StatusKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for StatusKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StatusKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown status {s}"))
    }
}

/// When a stage ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: Stage,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

impl StageTiming {
    pub fn duration(&self) -> Option<jiff::SignedDuration> {
        self.ended_at.map(|end| end.duration_since(self.started_at))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Artifacts {
    pub resolved: Option<Resolved>,
    pub source: Option<LocalFile>,
    pub output: Option<LocalFile>,
    pub published: Option<Published>,
    pub archived: Option<ArchiveEntry>,
    pub subtitles: Vec<LocalSubtitle>,
    /// The jobs a playlist link expanded into.
    pub children: Vec<JobId>,
    pub timings: Vec<StageTiming>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: Timestamp,
    pub stage: Option<Stage>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub request: Request,
    pub status: JobStatus,
    #[serde(default)]
    pub artifacts: Artifacts,
    pub log: Vec<LogEntry>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    #[serde(default)]
    pub finished_at: Option<Timestamp>,
}

impl Job {
    pub fn new(request: Request) -> Self {
        let now = Timestamp::now();
        Self {
            id: JobId::new(),
            request,
            status: JobStatus::Queued,
            artifacts: Artifacts::default(),
            log: Vec::new(),
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }
    }

    /// The media title, when resolved.
    pub fn title(&self) -> Option<&str> {
        self.artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref())
    }

    pub fn resolver(&self) -> Option<&str> {
        self.artifacts
            .resolved
            .as_ref()
            .map(|r| r.resolver.as_str())
    }

    /// Marks `stage` as begun, closing the one before.
    pub fn start_stage(&mut self, stage: Stage, now: Timestamp) {
        for timing in &mut self.artifacts.timings {
            if timing.ended_at.is_none() {
                timing.ended_at = Some(now);
            }
        }
        self.artifacts.timings.push(StageTiming {
            stage,
            started_at: now,
            ended_at: None,
        });
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
    }

    pub fn end_stages(&mut self, now: Timestamp) {
        for timing in &mut self.artifacts.timings {
            if timing.ended_at.is_none() {
                timing.ended_at = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn request_limits_only_tighten() {
        let engine = Limits {
            max_source_bytes: 100,
            max_duration_secs: Some(60),
            max_height: 720,
        };
        assert_eq!(RequestLimits::default().applied_to(&engine), engine);
        let tighter = RequestLimits {
            max_source_bytes: Some(10),
            max_duration_secs: Some(5),
            max_height: Some(480),
        }
        .applied_to(&engine);
        assert_eq!(tighter.max_source_bytes, 10);
        assert_eq!(tighter.max_duration_secs, Some(5));
        assert_eq!(tighter.max_height, 480);
        let looser = RequestLimits {
            max_source_bytes: Some(1000),
            max_duration_secs: Some(600),
            max_height: Some(2160),
        }
        .applied_to(&engine);
        assert_eq!(looser, engine);
        let unlimited_engine = Limits {
            max_duration_secs: None,
            ..engine
        };
        assert_eq!(
            RequestLimits {
                max_source_bytes: None,
                max_duration_secs: Some(5),
                max_height: None,
            }
            .applied_to(&unlimited_engine)
            .max_duration_secs,
            Some(5)
        );
    }

    #[test]
    fn stage_timings_open_and_close() {
        let origin = Origin {
            source: SourceId::new("local"),
            reference: "x".into(),
            url: None,
        };
        let mut job = Job::new(Request::new(
            origin,
            Url::parse("https://a.test/v").unwrap(),
        ));
        let t0 = Timestamp::UNIX_EPOCH;
        job.start_stage(Stage::Resolve, t0);
        assert_eq!(job.started_at, Some(t0));
        let t1 = t0 + jiff::SignedDuration::from_secs(2);
        job.start_stage(Stage::Download, t1);
        assert_eq!(job.artifacts.timings[0].ended_at, Some(t1));
        assert_eq!(
            job.artifacts.timings[0].duration(),
            Some(jiff::SignedDuration::from_secs(2))
        );
        assert!(job.artifacts.timings[1].ended_at.is_none());
        job.end_stages(t1);
        assert!(job.artifacts.timings.iter().all(|t| t.ended_at.is_some()));
    }
}
