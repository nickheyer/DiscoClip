use crate::archive::ArchiveError;
use crate::download::DownloadError;
use crate::publish::PublishError;
use crate::resolve::ResolveError;
use crate::transcode::TranscodeError;

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Transcode(#[from] TranscodeError),
    #[error(transparent)]
    Publish(#[from] PublishError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
}
