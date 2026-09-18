//! Links posted instead of uploads: where a job's media can be watched on the web, for
//! destinations that cannot take the file well. The app knows its front ends; the bot
//! only asks, once the job is resolved, and posts the page when the engine decided on a
//! link.

use discoclip_engine::job::Job;
use discoclip_engine::publish::Fallback;
use url::Url;

/// A page that plays a job's media, and the bounds under which linking beats uploading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLink {
    /// The page to post; the page carries what Discord needs to play the media inline.
    pub page: Url,
    pub fallback: Fallback,
}

/// Where a running bot finds the page a job's media would be linked from, as front ends
/// are edited while it runs.
pub trait LinkTargets: Send + Sync {
    /// The page for `job`'s media, when a front end shows the job's origin and platform
    /// and posts links; `None` when uploads are the only way.
    fn link_for(&self, job: &Job) -> Option<MediaLink>;
}

/// No front ends: every destination uploads.
pub struct NoLinks;

impl LinkTargets for NoLinks {
    fn link_for(&self, _: &Job) -> Option<MediaLink> {
        None
    }
}
