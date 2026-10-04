//! Links posted instead of uploads: where a job's media can be watched on the web. The
//! app knows its front ends. The bot only asks, once the job is resolved, and posts the
//! page when the engine decided on a link.

use discoclip_engine::job::Job;
use url::Url;

/// A page that plays a job's media
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLink {
    /// The page to post, which carries what Discord needs to play the media inline
    pub page: Url,
    /// The front end that serves it, as the app names it
    pub view: String,
}

/// Why no page can be linked for a job
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("the profile names no content view")]
    NoView,
    #[error("view {0} does not exist")]
    Unknown(String),
    #[error("view {0} is turned off")]
    Disabled(String),
    #[error("no public address is known yet")]
    NoPublicUrl,
}

/// Where a running bot finds the page a job's media would be linked from, as front ends
/// are edited while it runs.
pub trait LinkTargets: Send + Sync {
    /// The page for `job`'s media on the content view its policy names
    fn link_for(&self, job: &Job) -> Result<MediaLink, LinkError>;
}

/// The addresses the app is reached at, so the watcher leaves links to its own pages alone
pub trait OwnLinks: Send + Sync {
    /// Whether `url` is under an address the app is reached at
    fn is_own(&self, url: &Url) -> bool;
}

/// No front ends: every destination uploads.
pub struct NoLinks;

impl LinkTargets for NoLinks {
    fn link_for(&self, _: &Job) -> Result<MediaLink, LinkError> {
        Err(LinkError::NoView)
    }
}
