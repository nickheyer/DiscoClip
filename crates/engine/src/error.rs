use crate::archive::ArchiveError;
use crate::download::DownloadError;
use crate::job::Stage;
use crate::plan::SelectError;
use crate::publish::PublishError;
use crate::resolve::ResolveError;
use crate::store::StoreError;
use crate::transcode::TranscodeError;

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Transcode(#[from] TranscodeError),
    #[error(transparent)]
    Publish(#[from] PublishError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Rejected(String),
}

impl StageError {
    pub fn stage(&self, current: Stage) -> Stage {
        match self {
            StageError::Resolve(_) | StageError::Select(_) => Stage::Resolve,
            StageError::Download(_) => Stage::Download,
            StageError::Transcode(_) => Stage::Transcode,
            StageError::Publish(_) => Stage::Publish,
            StageError::Archive(_) => Stage::Archive,
            StageError::Store(_) | StageError::Rejected(_) => current,
        }
    }
}
