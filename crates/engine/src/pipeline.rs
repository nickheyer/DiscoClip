use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::WatchStream;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::archive::Archiver;
use crate::config::EngineConfig;
use crate::dedupe;
use crate::deliver::{self, LinkAvailability, Moment, Outcome};
use crate::download::{
    CaptureNotice, DownloadContext, Downloaded, Downloader, LocalSubtitle, SubtitleChoice,
    subtitles,
};
use crate::error::StageError;
use crate::event::{EngineEvent, EventKind, Progress, ProgressSender};
use crate::http::Http;
use crate::job::{
    Delivery, Job, JobId, JobStatus, LogEntry, Request, RequestOptions, SourceId, Stage,
    SubtitleMode,
};
use crate::media::{Container, LocalFile, MediaInfo, MediaKind};
use crate::plan::{self, Plan};
use crate::policy::{DeliveryPolicy, Limits, Policy};
use crate::publish::{self, Constraints, LinkTarget, PublishError, Publisher, QualityFloor};
use crate::resolve::SubtitleFormat;
use crate::resolve::{ClipRange, Resolution, Resolved, ResolverRegistry, Variant};
use crate::store::{JobStore, StoreError};
use crate::text::{count, elapsed};
use crate::transcode::{
    AudioTarget, BurnSource, ImageTarget, StillSource, Target, TranscodeError, Transcoder,
    VideoTarget, byte_budget_bps, clipped_duration,
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct Context {
    /// The engine's settings, replaced whole when they change in the app.
    pub config: Arc<RwLock<EngineConfig>>,
    pub http: Http,
    pub resolvers: Arc<ResolverRegistry>,
    pub downloaders: Vec<Arc<dyn Downloader>>,
    pub transcoder: Arc<dyn Transcoder>,
    pub publishers: HashMap<SourceId, Arc<dyn Publisher>>,
    pub archiver: Option<Arc<dyn Archiver>>,
    pub store: Arc<dyn JobStore>,
    pub events: broadcast::Sender<EngineEvent>,
    /// Where child jobs of a playlist are queued.
    pub submit: mpsc::Sender<JobId>,
}

impl Context {
    /// The settings as they stand now.
    pub fn config(&self) -> EngineConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .cache_dir
            .clone()
    }

    pub fn emit(&self, job: JobId, kind: EventKind) {
        let _ = self.events.send(EngineEvent {
            job,
            at: Timestamp::now(),
            kind,
        });
    }

    pub async fn transition(&self, job: &mut Job, status: JobStatus) -> Result<(), StoreError> {
        let now = Timestamp::now();
        if let JobStatus::Running { stage } = &status {
            job.start_stage(*stage, now);
        }
        if status.is_terminal() {
            job.end_stages(now);
            job.finished_at = Some(now);
        }
        if status == JobStatus::Queued {
            job.finished_at = None;
        }
        job.status = status.clone();
        job.updated_at = now;
        self.store.update(job).await?;
        self.emit(job.id, EventKind::Status { status });
        Ok(())
    }

    pub async fn note(
        &self,
        job: &mut Job,
        stage: Option<Stage>,
        message: impl Into<String>,
    ) -> Result<(), StoreError> {
        let entry = LogEntry {
            at: Timestamp::now(),
            stage,
            message: message.into(),
        };
        tracing::info!(job = %job.id, stage = stage.map(|s| s.as_str()), "{}", entry.message);
        job.log.push(entry.clone());
        job.updated_at = entry.at;
        self.store.update(job).await?;
        self.emit(job.id, EventKind::Log { entry });
        Ok(())
    }

    pub fn job_dir(&self, id: JobId) -> PathBuf {
        self.cache_dir().join("jobs").join(id.to_string())
    }

    /// Forwards progress updates as events, at most one per `PROGRESS_INTERVAL`, always ending
    /// with the latest value once the sender is dropped.
    fn progress(&self, id: JobId, stage: Stage) -> (ProgressSender, tokio::task::JoinHandle<()>) {
        let (tx, rx) = watch::channel(Progress::default());
        let events = self.events.clone();
        let forwarder = tokio::spawn(async move {
            let mut updates =
                std::pin::pin!(WatchStream::from_changes(rx).throttle(PROGRESS_INTERVAL));
            while let Some(progress) = updates.next().await {
                let _ = events.send(EngineEvent {
                    job: id,
                    at: Timestamp::now(),
                    kind: EventKind::Progress { stage, progress },
                });
            }
        });
        (tx, forwarder)
    }
}

enum Interrupt {
    Failed {
        stage: Stage,
        message: String,
    },
    Cancelled,
    Shutdown,
    /// The link was a playlist, expanded into jobs of its own: this job is done.
    Expanded,
}

fn failed(stage: Stage) -> impl Fn(StageError) -> Interrupt {
    move |error| Interrupt::Failed {
        stage: error.stage(stage),
        message: error.to_string(),
    }
}

/// Runs `job` to its end. `cancel` stops it at any point, as a person asked. `stop` ends
/// a live capture early, keeping what was recorded. `interrupt` is the engine stopping
/// with no more time to give: it stops the job wherever it is, except in the publish
/// stage, which runs to its end so a destination never gets the same media twice.
pub(crate) async fn run_job(
    ctx: Arc<Context>,
    mut job: Job,
    cancel: CancellationToken,
    stop: CancellationToken,
    interrupt: CancellationToken,
) {
    let job_dir = ctx.job_dir(job.id);
    let outcome = tokio::select! {
        result = execute(&ctx, &mut job, &job_dir, &stop, &interrupt) => result,
        _ = cancel.cancelled() => Err(Interrupt::Cancelled),
    };
    let status = match outcome {
        Ok(()) | Err(Interrupt::Expanded) => JobStatus::Done,
        Err(Interrupt::Failed { stage, message }) => JobStatus::Failed { stage, message },
        Err(Interrupt::Cancelled) => JobStatus::Cancelled,
        Err(Interrupt::Shutdown) => JobStatus::Queued,
    };
    if let JobStatus::Failed { stage, message } = &status {
        tracing::warn!(job = %job.id, %stage, "job failed: {message}");
    }
    if let Err(error) = ctx.transition(&mut job, status).await {
        tracing::error!(job = %job.id, "could not persist final status: {error}");
    }
    if let JobStatus::Failed { stage, message } = &job.status
        && let Some(publisher) = ctx.publishers.get(&job.request.origin.source)
    {
        let report = publisher.report_failure(&job, *stage, message);
        if tokio::time::timeout(Duration::from_secs(60), report)
            .await
            .is_err()
        {
            tracing::warn!(job = %job.id, "failure report to the destination timed out");
        }
    }
    if job.status == JobStatus::Done {
        tidy_job_dir(&job, &job_dir).await;
    } else if job.status == JobStatus::Queued && job.artifacts.recording.is_some() {
        // A capture interrupted by the engine stopping keeps its recording: the next
        // start carries on from it.
    } else if let Err(error) = tokio::fs::remove_dir_all(&job_dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(job = %job.id, "could not remove {}: {error}", job_dir.display());
    }
}

/// The files a finished job keeps in the cache, for the web app to serve: the output and
/// the subtitles fetched beside it. Everything else, the source above all, goes.
pub(crate) fn kept_files(job: &Job) -> Vec<PathBuf> {
    let mut keep: Vec<PathBuf> = job
        .artifacts
        .output
        .iter()
        .map(|file| file.path.clone())
        .collect();
    keep.extend(job.artifacts.subtitles.iter().map(|s| s.path.clone()));
    keep
}

/// Removes what a finished job left in its directory beyond [`kept_files`]. A directory
/// with nothing to keep goes altogether.
async fn tidy_job_dir(job: &Job, job_dir: &Path) {
    let keep = kept_files(job);
    if keep.is_empty() {
        if let Err(error) = tokio::fs::remove_dir_all(job_dir).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(job = %job.id, "could not remove {}: {error}", job_dir.display());
        }
        return;
    }
    let mut entries = match tokio::fs::read_dir(job_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(job = %job.id, "could not read {}: {error}", job_dir.display());
            return;
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if keep.iter().any(|kept| kept == &path) {
            continue;
        }
        let removed = match entry.file_type().await {
            Ok(kind) if kind.is_dir() => tokio::fs::remove_dir_all(&path).await,
            _ => tokio::fs::remove_file(&path).await,
        };
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(job = %job.id, "could not remove {}: {error}", path.display());
        }
    }
}

/// Queues one job per playlist entry, as children of `job`.
async fn expand_playlist(
    ctx: &Context,
    job: &mut Job,
    playlist: crate::resolve::Playlist,
) -> Result<(), Interrupt> {
    let stage = Stage::Resolve;
    let settings = job.request.policy.intake.playlists.clone();
    if !settings.enabled {
        return Err(failed(stage)(StageError::Rejected(
            "playlist links are turned off".into(),
        )));
    }
    if job.request.parent.is_some() {
        return Err(failed(stage)(StageError::Rejected(
            "a playlist entry led to another playlist".into(),
        )));
    }
    let total = playlist.total.unwrap_or(playlist.entries.len());
    let entries: Vec<_> = playlist
        .entries
        .into_iter()
        .take(settings.max_entries)
        .collect();
    if entries.is_empty() {
        return Err(failed(stage)(StageError::Rejected(format!(
            "{} listed no entries",
            playlist.resolver
        ))));
    }
    let mut ids = Vec::with_capacity(entries.len());
    for entry in &entries {
        let mut request = Request::new(job.request.origin.clone(), entry.url.clone());
        request.destination = job.request.destination.clone();
        request.limits = job.request.limits;
        request.options = job.request.options.clone();
        request.parent = Some(job.id);
        request.submitted_by = job.request.submitted_by.clone();
        request.disabled_platforms = job.request.disabled_platforms.clone();
        request.policy = job.request.policy.clone();
        let child = Job::new(request);
        ctx.store
            .insert(&child)
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        ctx.emit(
            child.id,
            EventKind::Submitted {
                request: Box::new(child.request.clone()),
            },
        );
        ctx.emit(
            child.id,
            EventKind::Status {
                status: JobStatus::Queued,
            },
        );
        ids.push(child.id);
    }
    job.artifacts.children = ids.clone();
    let mut resolved = Resolved::new(&playlist.resolver);
    resolved.id = playlist.id;
    resolved.title = playlist.title;
    job.artifacts.resolved = Some(resolved);
    ctx.note(
        job,
        Some(stage),
        format!(
            "{} returned {}. Queued {}.",
            playlist.resolver,
            count(total, "entry", "entries"),
            count(ids.len(), "job", "jobs")
        ),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    ctx.emit(job.id, EventKind::Children { ids: ids.clone() });
    for id in ids {
        ctx.submit
            .send(id)
            .await
            .map_err(|_| failed(stage)(StageError::Rejected("engine is stopping".into())))?;
    }
    Ok(())
}

/// The seconds of media a job produces: the clip when one is named, else the whole.
fn effective_duration(
    duration: Option<Duration>,
    clip: Option<crate::resolve::ClipRange>,
) -> Option<Duration> {
    let duration = duration?;
    Some(match clip {
        Some(clip) => {
            let start = clip.start.min(duration);
            let end = clip.end.map_or(duration, |e| e.min(duration));
            end.saturating_sub(start)
        }
        None => duration,
    })
}

/// `work` until it is done, or the engine stops with no more time to give.
async fn unless_interrupted<T>(
    interrupt: &CancellationToken,
    work: impl std::future::Future<Output = T>,
) -> Result<T, Interrupt> {
    tokio::select! {
        result = work => Ok(result),
        _ = interrupt.cancelled() => Err(Interrupt::Shutdown),
    }
}

/// How many times a destination may lower its limit before the job gives up
const LIMIT_LOWERINGS: usize = 3;

/// What the resolve stage hands on
struct Located {
    resolved: Resolved,
    variant: Variant,
    clip: Option<ClipRange>,
}

/// The source the rest of the job works from
struct Fetched {
    source: LocalFile,
    subtitles: Vec<LocalSubtitle>,
}

/// What the destination takes, and whether a page is there to link to
struct Destination {
    constraints: Constraints,
    link: LinkAvailability,
    target: Option<LinkTarget>,
}

/// Asks the publisher what its destination takes and which page it would link
async fn destination(publisher: &dyn Publisher, job: &Job) -> Result<Destination, Interrupt> {
    let stage = Stage::Resolve;
    let constraints = publisher
        .constraints(job)
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let (link, target) = match publisher.link_target(job).await {
        Ok(target) => (LinkAvailability::Available, Some(target)),
        Err(PublishError::NoLink(why)) => (LinkAvailability::Unavailable(why), None),
        Err(error) => return Err(failed(stage)(error.into())),
    };
    Ok(Destination {
        constraints,
        link,
        target,
    })
}

/// The recording a job interrupted mid-capture can carry on from: on disk, with bytes in
/// it, and with the link resolved so the rest of the job knows what it is.
async fn resumable(job: &Job) -> Option<LocalFile> {
    let recording = job.artifacts.recording.as_ref()?;
    job.artifacts.resolved.as_ref()?;
    let size = tokio::fs::metadata(&recording.path)
        .await
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .filter(|size| *size > 0)?;
    Some(LocalFile {
        path: recording.path.clone(),
        size,
        info: None,
    })
}

/// Takes in a live capture beginning: the recording it writes goes on the job for the
/// web app to play and serve while it grows, and the destination is told, when it has
/// somewhere to say so.
async fn capture_began(
    ctx: &Context,
    job: &mut Job,
    publisher: Option<&Arc<dyn Publisher>>,
    path: PathBuf,
) -> Result<(), Interrupt> {
    let stage = Stage::Download;
    let file = LocalFile {
        path,
        size: 0,
        info: None,
    };
    job.artifacts.recording = Some(file.clone());
    job.updated_at = Timestamp::now();
    ctx.store
        .update(job)
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    ctx.emit(
        job.id,
        EventKind::Recording {
            file: Box::new(file.clone()),
        },
    );
    ctx.note(
        job,
        Some(stage),
        "Live capture began. The recording plays while it grows.",
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    let Some(publisher) = publisher else {
        return Ok(());
    };
    match publisher.announce(job, &file).await {
        Ok(Some(published)) => {
            let reference = published.reference.clone();
            job.artifacts.announced = Some(published);
            ctx.note(
                job,
                Some(stage),
                format!("Capture announced as {reference}."),
            )
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        }
        Ok(None) => {}
        Err(error) => {
            ctx.note(job, Some(stage), format!("Capture not announced: {error}"))
                .await
                .map_err(|e| failed(stage)(e.into()))?;
        }
    }
    Ok(())
}

/// Resolves the link under the limits and the intake policy, expanding a playlist into jobs
async fn resolve_link(
    ctx: &Context,
    job: &mut Job,
    interrupt: &CancellationToken,
    limits: &Limits,
    policy: &Policy,
) -> Result<Located, Interrupt> {
    let url = job.request.url.clone();
    let stage = Stage::Resolve;
    let resolution = unless_interrupted(
        interrupt,
        ctx.resolvers
            .resolve_without(&url, &job.request.disabled_platforms),
    )
    .await?
    .map_err(|e| failed(stage)(e.into()))?;
    let resolved = match resolution {
        Resolution::Media(resolved) => *resolved,
        Resolution::Playlist(playlist) => {
            expand_playlist(ctx, job, playlist).await?;
            return Err(Interrupt::Expanded);
        }
    };
    let media = resolved.media;
    let clip = job.request.options.clip.or(resolved.clip);
    if let (Some(limit), Some(duration)) = (
        limits.max_duration_secs,
        effective_duration(resolved.duration, clip),
    ) && duration.as_secs() > limit
    {
        return Err(failed(stage)(StageError::Rejected(format!(
            "{media} is {}s long, limit is {limit}s",
            duration.as_secs()
        ))));
    }
    if resolved.live && !policy.intake.live {
        return Err(failed(stage)(StageError::Rejected(
            "live streams are not accepted here".into(),
        )));
    }
    let variant = plan::select_variant(
        &resolved.variants,
        limits,
        &resolved.resolver,
        media,
        &job.request.options.audio_language,
    )
    .map_err(|e| failed(stage)(e.into()))?;
    ctx.note(
        job,
        Some(stage),
        format!(
            "{} resolved {media} with {}. Selected {} {}.",
            resolved.resolver,
            count(resolved.variants.len(), "variant", "variants"),
            variant.kind.as_str(),
            describe_variant(&variant)
        ),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    job.artifacts.resolved = Some(resolved.clone());
    Ok(Located {
        resolved,
        variant,
        clip,
    })
}

/// Fetches the located media, following a live capture as it goes
#[allow(clippy::too_many_arguments)]
async fn download(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    stop: &CancellationToken,
    interrupt: &CancellationToken,
    config: &EngineConfig,
    limits: &Limits,
    located: &Located,
) -> Result<Fetched, Interrupt> {
    let Located {
        resolved,
        variant,
        clip,
    } = located;
    let origin = job.request.origin.clone();
    let stage = Stage::Download;
    ctx.transition(job, JobStatus::Running { stage })
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let downloader = ctx
        .downloaders
        .iter()
        .find(|d| d.handles(variant.kind))
        .cloned()
        .ok_or_else(|| {
            failed(stage)(StageError::Rejected(format!(
                "no downloader handles {} variants",
                variant.kind.as_str()
            )))
        })?;
    tokio::fs::create_dir_all(job_dir)
        .await
        .map_err(|e| failed(stage)(StageError::Download(e.into())))?;
    let (capture, mut notices) = CaptureNotice::channel();
    let context = DownloadContext {
        max_bytes: limits.max_source_bytes,
        max_height: limits.max_height,
        max_live: Duration::from_secs(job.request.limits.capture_secs(&job.request.policy.limits)),
        stop: stop.clone(),
        capture,
        clip: *clip,
        audio_language: job.request.options.audio_language.clone(),
        platform: resolved.resolver.clone(),
        download: config.download.clone(),
        browser: config.browser.clone(),
        subtitles: (job.request.options.subtitles != SubtitleMode::Skip).then(|| SubtitleChoice {
            tracks: subtitles::pick(
                &resolved.subtitles,
                job.request.options.subtitle_language.as_deref(),
            ),
            language: job.request.options.subtitle_language.clone(),
        }),
    };
    let publisher = ctx.publishers.get(&origin.source);
    let (progress, forwarder) = ctx.progress(job.id, stage);
    let downloaded = {
        let download = downloader.download(variant, job_dir, &context, progress);
        tokio::pin!(download);
        let mut listening = true;
        loop {
            tokio::select! {
                result = &mut download => break Ok(result),
                _ = interrupt.cancelled() => break Err(Interrupt::Shutdown),
                changed = notices.changed(), if listening => match changed {
                    Ok(()) => {
                        let path = notices.borrow_and_update().clone();
                        if let Some(path) = path {
                            capture_began(ctx, job, publisher, path).await?;
                        }
                    }
                    Err(_) => listening = false,
                },
            }
        }
    };
    let _ = forwarder.await;
    let downloaded = downloaded?;
    let Downloaded {
        file: source,
        subtitles: mut local_subtitles,
        notes,
    } = downloaded.map_err(|e| failed(stage)(e.into()))?;
    for note in notes {
        ctx.note(job, Some(stage), note)
            .await
            .map_err(|e| failed(stage)(e.into()))?;
    }
    if source.size > limits.max_source_bytes {
        return Err(failed(stage)(StageError::Rejected(format!(
            "source is {} bytes, limit is {}",
            source.size, limits.max_source_bytes
        ))));
    }
    // The tracks the downloader did not fetch along with the media.
    let remaining: Vec<_> = context
        .subtitles
        .as_ref()
        .map(|choice| {
            choice
                .tracks
                .iter()
                .filter(|t| {
                    !local_subtitles
                        .iter()
                        .any(|s| s.url.as_ref() == Some(&t.url))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !remaining.is_empty() {
        match unless_interrupted(
            interrupt,
            subtitles::fetch_all(&ctx.http, &resolved.resolver, &remaining, job_dir),
        )
        .await?
        {
            Ok(fetched) => local_subtitles.extend(fetched),
            Err(error) => {
                ctx.note(job, Some(stage), format!("subtitles not fetched: {error}"))
                    .await
                    .map_err(|e| failed(stage)(e.into()))?;
            }
        }
    }
    job.artifacts.subtitles = local_subtitles.clone();
    ctx.note(
        job,
        Some(stage),
        format!(
            "{} {} bytes to {}{}.",
            if job.artifacts.recording.is_some() {
                "Captured"
            } else {
                "Downloaded"
            },
            source.size,
            source.path.display(),
            if local_subtitles.is_empty() {
                String::new()
            } else {
                format!(
                    " with {}",
                    count(local_subtitles.len(), "subtitle track", "subtitle tracks")
                )
            }
        ),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    Ok(Fetched {
        source,
        subtitles: local_subtitles,
    })
}

/// The source to work from, an earlier job's archived media when the policy allows and it matches
#[allow(clippy::too_many_arguments)]
async fn fetch(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    stop: &CancellationToken,
    interrupt: &CancellationToken,
    config: &EngineConfig,
    limits: &Limits,
    policy: &Policy,
    dest: &Destination,
    located: &Located,
) -> Result<Fetched, Interrupt> {
    let matching = policy.dedupe.matching;
    if policy.dedupe.enabled && matching.by_url() && !located.resolved.live {
        let stage = Stage::Resolve;
        let candidates = ctx
            .store
            .find_finished_by_url(
                &dedupe::url_key(&job.request.url),
                dedupe::media_key(&located.resolved).as_deref(),
            )
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        if let Some((source, _)) = reuse(
            ctx,
            job,
            job_dir,
            candidates,
            dest,
            limits,
            policy,
            Some(located),
            stage,
            true,
        )
        .await?
        {
            return Ok(Fetched {
                source,
                subtitles: Vec::new(),
            });
        }
    }
    let fetched = download(ctx, job, job_dir, stop, interrupt, config, limits, located).await?;
    if !policy.dedupe.enabled || job.artifacts.recording.is_some() {
        return Ok(fetched);
    }
    let stage = Stage::Download;
    let hash = unless_interrupted(interrupt, dedupe::sha256_file(&fetched.source.path))
        .await?
        .map_err(|e| failed(stage)(StageError::Download(e.into())))?;
    job.artifacts.media_hash = Some(hash.clone());
    if matching.by_content() {
        let candidates = ctx
            .store
            .find_finished_by_hash(&hash)
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        if let Some((source, _)) = reuse(
            ctx,
            job,
            job_dir,
            candidates,
            dest,
            limits,
            policy,
            Some(located),
            stage,
            false,
        )
        .await?
        {
            return Ok(Fetched {
                source,
                subtitles: fetched.subtitles,
            });
        }
    }
    Ok(fetched)
}

/// Whether an archived output goes to this destination as it is, at or above the floor
fn fits(
    output: &LocalFile,
    reference: &LocalFile,
    constraints: &Constraints,
    limits: &Limits,
    delivery: &DeliveryPolicy,
) -> bool {
    let Some(info) = output.info.as_ref() else {
        return false;
    };
    let max_height = limits
        .max_height
        .min(constraints.max_height.unwrap_or(u32::MAX));
    let still = (info.kind == MediaKind::Audio && constraints.renders_audio_as_video())
        .then_some(StillSource::Waveform);
    let target = build_target(
        info.kind,
        constraints,
        Some(info),
        max_height,
        None,
        None,
        still,
    );
    let capped = Limits {
        max_height,
        ..limits.clone()
    };
    if !matches!(
        plan::plan(output, Some(info), constraints, &capped, target),
        Ok(Plan::Passthrough)
    ) {
        return false;
    }
    if info.kind != MediaKind::Video {
        return true;
    }
    let reference_info = reference.info.as_ref().unwrap_or(info);
    below_floor(
        output,
        &floor_for(&delivery.floor, reference, reference_info),
    )
    .is_none()
}

/// The archived media of the first candidate with the same options that fits, brought in as this job's source, with the candidate it came from
#[allow(clippy::too_many_arguments)]
async fn reuse(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    candidates: Vec<Job>,
    dest: &Destination,
    limits: &Limits,
    policy: &Policy,
    located: Option<&Located>,
    stage: Stage,
    from_source: bool,
) -> Result<Option<(LocalFile, Job)>, Interrupt> {
    let Some(archiver) = ctx.archiver.as_ref() else {
        return Ok(None);
    };
    let io = |e: std::io::Error| failed(stage)(StageError::Download(e.into()));
    let all_skipped = match (located, stage) {
        (None, _) => "Resolving, download and conversion skipped.",
        (Some(_), Stage::Resolve) => "Download and conversion skipped.",
        _ => "Conversion skipped.",
    };
    let fetch_skipped = if located.is_some() {
        "Download skipped."
    } else {
        "Resolving and download skipped."
    };
    for candidate in candidates {
        if candidate.id == job.id || candidate.request.options != job.request.options {
            continue;
        }
        if let Some(located) = located {
            let candidate_clip = candidate.request.options.clip.or(candidate
                .artifacts
                .resolved
                .as_ref()
                .and_then(|r| r.clip));
            if candidate_clip != located.clip {
                continue;
            }
        }
        let Some(entry) = candidate.artifacts.archived.clone() else {
            continue;
        };
        let files = archiver.locate(&entry).await;
        let title = candidate.title().unwrap_or("untitled").to_string();
        let earlier_post = candidate
            .artifacts
            .published
            .as_ref()
            .and_then(|p| p.url.clone());
        let archived_output = match (files.output, candidate.artifacts.output.as_ref()) {
            (Some(archived), Some(output)) => output.info.clone().map(|info| LocalFile {
                info: Some(info),
                ..archived
            }),
            _ => None,
        };
        if let (Some(archived), Some(output)) = (
            archived_output.as_ref(),
            candidate.artifacts.output.as_ref(),
        ) {
            let reference = candidate.artifacts.source.as_ref().unwrap_or(output);
            if fits(
                archived,
                reference,
                &dest.constraints,
                limits,
                &policy.delivery,
            ) {
                tokio::fs::create_dir_all(job_dir).await.map_err(io)?;
                let ext = archived
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("bin");
                let copy = job_dir.join(format!("reused.{ext}"));
                tokio::fs::copy(&archived.path, &copy).await.map_err(io)?;
                job.artifacts.reused_from = Some(candidate.id);
                job.artifacts.earlier_post = earlier_post;
                if let Some(hash) = candidate.artifacts.media_hash.clone() {
                    job.artifacts.media_hash = Some(hash);
                }
                job.artifacts.archived = Some(entry);
                ctx.note(
                    job,
                    Some(stage),
                    format!(
                        "Same media as job {} ({title}), finished {} earlier: reusing its \
                         archived output ({} bytes). {all_skipped}",
                        candidate.id,
                        elapsed(
                            candidate.finished_at.unwrap_or(candidate.updated_at),
                            Timestamp::now()
                        ),
                        archived.size
                    ),
                )
                .await
                .map_err(|e| failed(stage)(e.into()))?;
                let reused = LocalFile {
                    path: copy,
                    ..archived.clone()
                };
                return Ok(Some((reused, candidate)));
            }
        }
        if !from_source {
            continue;
        }
        if let Some(source) = files.source {
            let info = candidate
                .artifacts
                .source
                .as_ref()
                .and_then(|s| s.info.clone());
            job.artifacts.reused_from = Some(candidate.id);
            job.artifacts.earlier_post = earlier_post;
            if let Some(hash) = candidate.artifacts.media_hash.clone() {
                job.artifacts.media_hash = Some(hash);
            }
            ctx.note(
                job,
                Some(stage),
                format!(
                    "Same media as job {} ({title}): its archived output does not fit here. \
                     Converting from its archived source instead. {fetch_skipped}",
                    candidate.id
                ),
            )
            .await
            .map_err(|e| failed(stage)(e.into()))?;
            return Ok(Some((LocalFile { info, ..source }, candidate)));
        }
        if let Some(archived) = archived_output {
            job.artifacts.reused_from = Some(candidate.id);
            job.artifacts.earlier_post = earlier_post;
            if let Some(hash) = candidate.artifacts.media_hash.clone() {
                job.artifacts.media_hash = Some(hash);
            }
            ctx.note(
                job,
                Some(stage),
                format!(
                    "Same media as job {} ({title}): its archived output does not fit here. \
                     Converting from that output instead. {fetch_skipped}",
                    candidate.id
                ),
            )
            .await
            .map_err(|e| failed(stage)(e.into()))?;
            return Ok(Some((archived, candidate)));
        }
    }
    Ok(None)
}

/// A finished job's resolution and the variant this job would take from it, unless it was live
fn stored_variant(
    candidate: &Job,
    limits: &Limits,
    options: &RequestOptions,
) -> Option<(Resolved, Variant)> {
    let resolved = candidate.artifacts.resolved.clone().filter(|r| !r.live)?;
    let variant = plan::select_variant(
        &resolved.variants,
        limits,
        &resolved.resolver,
        resolved.media,
        &options.audio_language,
    )
    .ok()?;
    Some((resolved, variant))
}

/// The archived output of an earlier job for the same link, in place of resolving it again
async fn reuse_link(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    dest: &Destination,
    limits: &Limits,
    policy: &Policy,
) -> Result<Option<(Located, LocalFile)>, Interrupt> {
    if !policy.dedupe.enabled || !policy.dedupe.matching.by_url() {
        return Ok(None);
    }
    let stage = Stage::Resolve;
    let candidates: Vec<Job> = ctx
        .store
        .find_finished_by_url(&dedupe::url_key(&job.request.url), None)
        .await
        .map_err(|e| failed(stage)(e.into()))?
        .into_iter()
        .filter(|candidate| stored_variant(candidate, limits, &job.request.options).is_some())
        .collect();
    let Some((source, candidate)) = reuse(
        ctx, job, job_dir, candidates, dest, limits, policy, None, stage, true,
    )
    .await?
    else {
        return Ok(None);
    };
    let (resolved, variant) = stored_variant(&candidate, limits, &job.request.options)
        .expect("every candidate offered to reuse has a stored variant");
    job.artifacts.resolved = Some(resolved.clone());
    Ok(Some((
        Located {
            clip: job.request.options.clip.or(resolved.clip),
            resolved,
            variant,
        },
        source,
    )))
}

async fn execute(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    stop: &CancellationToken,
    interrupt: &CancellationToken,
) -> Result<(), Interrupt> {
    let config = ctx.config();
    let policy = job.request.policy.clone();
    let limits = job.request.limits.applied_to(&policy.limits);
    let origin = job.request.origin.clone();
    let publisher = ctx.publishers.get(&origin.source).cloned().ok_or_else(|| {
        failed(Stage::Resolve)(StageError::Rejected(format!(
            "no publisher registered for source {}",
            origin.source
        )))
    })?;

    let dest = destination(publisher.as_ref(), job).await?;
    // A capture the engine's stop interrupted carries on from its recording: what was
    // recorded is the source, and the job resumes at the transcode.
    let (located, prefetched) = match resumable(job).await {
        Some(recording) => {
            let resolved = job
                .artifacts
                .resolved
                .clone()
                .expect("a resumable job is resolved");
            let variant = plan::select_variant(
                &resolved.variants,
                &limits,
                &resolved.resolver,
                resolved.media,
                &job.request.options.audio_language,
            )
            .map_err(|e| failed(Stage::Resolve)(e.into()))?;
            ctx.note(
                job,
                Some(Stage::Download),
                format!(
                    "Carrying on from the recording as it stands: {} bytes.",
                    recording.size
                ),
            )
            .await
            .map_err(|e| failed(Stage::Download)(e.into()))?;
            let subtitles = job.artifacts.subtitles.clone();
            (
                Located {
                    clip: job.request.options.clip.or(resolved.clip),
                    resolved,
                    variant,
                },
                Some(Fetched {
                    source: recording,
                    subtitles,
                }),
            )
        }
        None => {
            let stage = Stage::Resolve;
            ctx.transition(job, JobStatus::Running { stage })
                .await
                .map_err(|e| failed(stage)(e.into()))?;
            match reuse_link(ctx, job, job_dir, &dest, &limits, &policy).await? {
                Some((located, source)) => (
                    located,
                    Some(Fetched {
                        source,
                        subtitles: Vec::new(),
                    }),
                ),
                None => match resolve_link(ctx, job, interrupt, &limits, &policy).await {
                    Ok(located) => (located, None),
                    Err(Interrupt::Expanded) => return Ok(()),
                    Err(other) => return Err(other),
                },
            }
        }
    };
    let fetched = match prefetched {
        Some(fetched) => fetched,
        None => {
            fetch(
                ctx, job, job_dir, stop, interrupt, &config, &limits, &policy, &dest, &located,
            )
            .await?
        }
    };

    // Transcode
    let stage = Stage::Transcode;
    ctx.transition(job, JobStatus::Running { stage })
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let prepared = prepare(
        ctx,
        job,
        job_dir,
        &located,
        fetched,
        &dest.constraints,
        &limits,
    )
    .await?;
    let mut constraints = dest.constraints.clone();
    let mut lowered = 0;
    let published = loop {
        let output = make_output(
            ctx,
            job,
            job_dir,
            &prepared,
            &constraints,
            &limits,
            &policy,
            &dest,
            interrupt,
        )
        .await?;
        job.artifacts.output = Some(output.clone());
        job.artifacts.thumbnail = make_thumbnail(ctx, job, job_dir, &output).await;
        ctx.note(
            job,
            Some(Stage::Transcode),
            format!("Output ready, {} bytes.", output.size),
        )
        .await
        .map_err(|e| failed(Stage::Transcode)(e.into()))?;

        // Publish
        let stage = Stage::Publish;
        ctx.transition(job, JobStatus::Running { stage })
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        match publisher.publish(job, &output).await {
            Ok(published) => break published,
            Err(PublishError::LimitLowered { size, max })
                if lowered < LIMIT_LOWERINGS && job.artifacts.delivery == Delivery::Upload =>
            {
                lowered += 1;
                ctx.note(
                    job,
                    Some(stage),
                    format!(
                        "The destination refused {size} bytes. Its limit is {max} bytes. \
                         Making the output again under it."
                    ),
                )
                .await
                .map_err(|e| failed(stage)(e.into()))?;
                constraints.max_bytes = max;
                job.artifacts.archived = None;
                ctx.transition(
                    job,
                    JobStatus::Running {
                        stage: Stage::Transcode,
                    },
                )
                .await
                .map_err(|e| failed(Stage::Transcode)(e.into()))?;
            }
            Err(error) => return Err(failed(stage)(error.into())),
        }
    };
    let stage = Stage::Publish;
    for note in &published.notes {
        ctx.note(job, Some(stage), note.clone())
            .await
            .map_err(|e| failed(stage)(e.into()))?;
    }
    ctx.note(
        job,
        Some(stage),
        format!("Published as {}.", published.reference),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    job.artifacts.published = Some(published);

    // Archive
    if let (Some(earlier), true) = (job.artifacts.reused_from, job.artifacts.archived.is_some()) {
        ctx.note(
            job,
            Some(Stage::Archive),
            format!(
                "Not archived again. The archive already holds this output from job {earlier}."
            ),
        )
        .await
        .map_err(|e| failed(Stage::Archive)(e.into()))?;
    } else if let Some(archiver) = ctx.archiver.as_ref().filter(|a| a.enabled()) {
        let stage = Stage::Archive;
        ctx.transition(job, JobStatus::Running { stage })
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        let entry = unless_interrupted(interrupt, archiver.archive(job))
            .await?
            .map_err(|e| failed(stage)(e.into()))?;
        ctx.note(
            job,
            Some(stage),
            format!(
                "Archived {}, {} bytes.",
                count(entry.files.len(), "file", "files"),
                entry.bytes
            ),
        )
        .await
        .map_err(|e| failed(stage)(e.into()))?;
        job.artifacts.archived = Some(entry);
    }
    Ok(())
}

/// The source as probed, with everything the output is made with
struct Prepared {
    source: LocalFile,
    info: Option<MediaInfo>,
    kind: MediaKind,
    clip: Option<ClipRange>,
    burn: Option<BurnSource>,
    still: Option<StillSource>,
}

/// Probes the source and readies what the output is made with
async fn prepare(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    located: &Located,
    fetched: Fetched,
    constraints: &Constraints,
    limits: &Limits,
) -> Result<Prepared, Interrupt> {
    let stage = Stage::Transcode;
    let Fetched {
        mut source,
        subtitles: local_subtitles,
    } = fetched;
    let Located {
        resolved,
        variant,
        clip,
    } = located;
    let clip = *clip;
    let media = resolved.media;
    // A file that is not media is never probed. Anything else is, and what the probe
    // finds the file to be outranks what the resolver said it was.
    let info: Option<MediaInfo> = match media {
        MediaKind::File => None,
        _ => Some(
            ctx.transcoder
                .probe(&source.path)
                .await
                .map_err(|e| failed(stage)(e.into()))?,
        ),
    };
    let kind = info.as_ref().map_or(media, |i| i.kind);
    if kind != media {
        ctx.note(
            job,
            Some(stage),
            format!(
                "{} reported {media}. The downloaded file is {kind}.",
                resolved.resolver
            ),
        )
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    }
    if let (Some(limit), Some(duration)) = (
        limits.max_duration_secs,
        effective_duration(info.as_ref().and_then(|i| i.duration), clip),
    ) && duration.as_secs() > limit
    {
        return Err(failed(stage)(StageError::Rejected(format!(
            "{kind} is {}s long, limit is {limit}s",
            duration.as_secs()
        ))));
    }
    // What the platform said about the picture's shape fills in what the file does not.
    let info = info.map(|mut info| {
        if let Some(video) = info.video.as_mut() {
            if video.projection.is_none() {
                video.projection = variant.projection;
            }
            if video.stereo.is_none() {
                video.stereo = variant.stereo;
            }
        }
        info
    });
    source.info = info.clone();
    job.artifacts.source = Some(source.clone());
    let burn = match (job.request.options.subtitles, info.as_ref()) {
        (SubtitleMode::Burn, Some(info)) if kind == MediaKind::Video => {
            match burn_source(
                &local_subtitles,
                info,
                job.request.options.subtitle_language.as_deref(),
                job_dir,
            )
            .await
            {
                Ok(burn) => burn,
                Err(problem) => {
                    ctx.note(job, Some(stage), problem)
                        .await
                        .map_err(|e| failed(stage)(e.into()))?;
                    None
                }
            }
        }
        _ => None,
    };
    if job.request.options.subtitles == SubtitleMode::Burn
        && kind == MediaKind::Video
        && burn.is_none()
    {
        ctx.note(
            job,
            Some(stage),
            "No subtitles to burn in: the source offers none.",
        )
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    }
    // Sound alone becomes a video when the destination asks for one or takes no sound
    // as such. The picture is the cover art, else the platform's thumbnail, else a
    // waveform.
    let still = match info.as_ref() {
        Some(info) if kind == MediaKind::Audio && constraints.renders_audio_as_video() => {
            Some(still_source(ctx, job, job_dir, info, resolved.thumbnail.as_ref()).await)
        }
        _ => None,
    };
    Ok(Prepared {
        source,
        info,
        kind,
        clip,
        burn,
        still,
    })
}

/// The output for the destination, made for the page instead when the delivery decision says so
#[allow(clippy::too_many_arguments)]
async fn make_output(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    prepared: &Prepared,
    constraints: &Constraints,
    limits: &Limits,
    policy: &Policy,
    dest: &Destination,
    interrupt: &CancellationToken,
) -> Result<LocalFile, Interrupt> {
    let stage = Stage::Transcode;
    let purpose = Purpose::Upload {
        delivery: &policy.delivery,
        link: &dest.link,
    };
    let produced = unless_interrupted(
        interrupt,
        produce(ctx, job, job_dir, prepared, constraints, limits, purpose),
    )
    .await?
    .map_err(failed(stage))?;
    match produced {
        Produced::Upload(output) => Ok(output),
        Produced::Skip(reason) => Err(failed(stage)(StageError::Rejected(reason))),
        Produced::Link { output, reason } => {
            job.artifacts.delivery = Delivery::Link;
            job.artifacts.link_reason = Some(reason.clone());
            job.artifacts.link = dest.target.clone();
            ctx.note(job, Some(stage), format!("{reason}. Posting a media link."))
                .await
                .map_err(|e| failed(stage)(e.into()))?;
            if let Some(output) = output {
                return Ok(output);
            }
            let page = constraints.for_link(policy.delivery.link_max_bytes);
            match unless_interrupted(
                interrupt,
                produce(ctx, job, job_dir, prepared, &page, limits, Purpose::Page),
            )
            .await?
            .map_err(failed(stage))?
            {
                Produced::Upload(output)
                | Produced::Link {
                    output: Some(output),
                    ..
                } => Ok(output),
                Produced::Link {
                    output: None,
                    reason,
                }
                | Produced::Skip(reason) => Err(failed(stage)(StageError::Rejected(reason))),
            }
        }
    }
}

/// Who the output is for, which decides whether delivery choices apply
enum Purpose<'a> {
    Upload {
        delivery: &'a DeliveryPolicy,
        link: &'a LinkAvailability,
    },
    /// The page that plays the media, bounded by its own size and nothing else
    Page,
}

/// What the transcode stage made of the source for one destination.
enum Produced {
    /// A file the destination takes as an upload.
    Upload(LocalFile),
    /// The destination gets a link instead: with the output already made for the page
    /// when the upload was tried and came out too reduced, or with none yet when the
    /// upload was never worth trying.
    Link {
        output: Option<LocalFile>,
        reason: String,
    },
    /// Nothing is posted, for this reason
    Skip(String),
}

/// Bits per second of a whole file over its playing time.
fn bitrate_of(file: &LocalFile, duration: Option<Duration>) -> Option<u64> {
    let secs = duration?.as_secs_f64();
    (secs > 0.0).then(|| (file.size as f64 * 8.0 / secs) as u64)
}

/// The floor as it applies to this source: a source already below a bound is not reduced
/// by an output at the source's own level.
fn floor_for(floor: &QualityFloor, source: &LocalFile, info: &MediaInfo) -> QualityFloor {
    let source_height = info.video.as_ref().map_or(u32::MAX, |v| v.height);
    let source_bitrate = bitrate_of(source, info.duration).unwrap_or(u64::MAX);
    QualityFloor {
        min_height: floor.min_height.min(source_height),
        min_bitrate: floor.min_bitrate.min(source_bitrate),
    }
}

/// Whether `output` falls under `floor`.
fn below_floor(output: &LocalFile, floor: &QualityFloor) -> Option<String> {
    let info = output.info.as_ref()?;
    let height = info.video.as_ref().map_or(0, |v| v.height);
    if height < floor.min_height {
        return Some(format!(
            "the upload would be {height}p, under the {}p floor",
            floor.min_height
        ));
    }
    if let Some(bitrate) = bitrate_of(output, info.duration)
        && bitrate < floor.min_bitrate
    {
        return Some(format!(
            "the upload would be {} kb/s, under the {} kb/s floor",
            bitrate / 1000,
            floor.min_bitrate / 1000
        ));
    }
    None
}

/// What the output is made into for `kind` under the destination
fn build_target(
    kind: MediaKind,
    constraints: &Constraints,
    info: Option<&MediaInfo>,
    max_height: u32,
    clip: Option<ClipRange>,
    burn: Option<BurnSource>,
    still: Option<StillSource>,
) -> Target {
    match kind {
        MediaKind::Video => {
            let mut target = VideoTarget::new(
                constraints.preferred_container(),
                constraints.preferred_video(),
                info.and_then(|i| i.audio.as_ref())
                    .map(|_| constraints.preferred_audio()),
                constraints.max_bytes,
                max_height,
            );
            target.max_fps = constraints.max_fps;
            target.clip = clip;
            target.burn = burn;
            Target::Video(target)
        }
        MediaKind::Audio if still.is_some() => {
            let mut target = VideoTarget::new(
                constraints.preferred_container(),
                constraints.preferred_video(),
                Some(constraints.preferred_audio()),
                constraints.max_bytes,
                max_height,
            );
            target.max_fps = constraints.max_fps;
            target.clip = clip;
            target.still = still;
            Target::Video(target)
        }
        MediaKind::Audio => {
            let container = match info.map(|i| &i.container) {
                Some(container) if constraints.accepts_audio_file(container, None) => {
                    container.clone()
                }
                _ => constraints.preferred_audio_container(),
            };
            Target::Audio(AudioTarget {
                codec: publish::audio_codec_for(&container),
                container,
                max_bytes: constraints.max_bytes,
                clip,
            })
        }
        MediaKind::Image => {
            let container = match info.map(|i| &i.container) {
                Some(container) if constraints.accepts_image(container) => container.clone(),
                _ => constraints.preferred_image_container(),
            };
            Target::Image(ImageTarget {
                container,
                max_bytes: constraints.max_bytes,
            })
        }
        MediaKind::File => Target::File {
            max_bytes: constraints.max_bytes,
        },
    }
}

/// Makes the output for `constraints`: the source as it is when it fits, else a
/// transcode. For an upload, the delivery policy decides at each point where the file
/// cannot be uploaded well whether a link is posted or nothing is.
async fn produce(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    prepared: &Prepared,
    constraints: &Constraints,
    limits: &Limits,
    purpose: Purpose<'_>,
) -> Result<Produced, StageError> {
    let stage = Stage::Transcode;
    let Prepared {
        source,
        info,
        kind,
        clip,
        burn,
        still,
    } = prepared;
    let (kind, clip) = (*kind, *clip);
    let info = info.as_ref();
    let decide = |moment: Moment| match &purpose {
        Purpose::Upload { delivery, link } => Some(deliver::decide(delivery, link, moment)),
        Purpose::Page => None,
    };
    match decide(Moment::Start) {
        Some(Outcome::Link { reason }) => {
            return Ok(Produced::Link {
                output: None,
                reason,
            });
        }
        Some(Outcome::Skip { reason }) => return Ok(Produced::Skip(reason)),
        Some(Outcome::Upload) | None => {}
    }
    let max_height = limits
        .max_height
        .min(constraints.max_height.unwrap_or(u32::MAX));
    let target = build_target(
        kind,
        constraints,
        info,
        max_height,
        clip,
        burn.clone(),
        still.clone(),
    );
    // A recording is fragmented, for playing while it grew. What the destination gets is
    // written whole, its index first, even when the streams go in as they are.
    let recording = job
        .artifacts
        .recording
        .as_ref()
        .is_some_and(|r| r.path == source.path);
    let capped = Limits {
        max_height,
        ..limits.clone()
    };
    let plan = match plan::plan(source, info, constraints, &capped, target.clone()) {
        Ok(Plan::Passthrough) if recording => Plan::Transcode(Box::new(target)),
        Ok(plan) => plan,
        Err(TranscodeError::CannotShrink { size, max_bytes }) => {
            return Ok(match decide(Moment::CannotShrink { size, max_bytes }) {
                Some(Outcome::Link { reason }) => Produced::Link {
                    output: Some(source.clone()),
                    reason,
                },
                Some(Outcome::Skip { reason }) => Produced::Skip(reason),
                Some(Outcome::Upload) => {
                    return Err(TranscodeError::CannotShrink { size, max_bytes }.into());
                }
                None => Produced::Skip(format!(
                    "the file is {size} bytes, over the {max_bytes} a page takes"
                )),
            });
        }
        Err(error) => return Err(error.into()),
    };
    // The floor as it applies to this source, when a video is made for an upload.
    let floor = match (&purpose, info) {
        (Purpose::Upload { delivery, .. }, Some(info)) if kind == MediaKind::Video => {
            let floor = floor_for(&delivery.floor, source, info);
            let secs = clipped_duration(clip, info.duration.unwrap_or_default()).as_secs_f64();
            if matches!(plan, Plan::Transcode(_)) && secs > 0.0 {
                let budget = byte_budget_bps(constraints.max_bytes, secs) as u64;
                if budget < floor.min_bitrate {
                    let moment = Moment::BudgetUnderFloor {
                        max_bytes: constraints.max_bytes,
                        budget_bps: budget,
                        secs,
                        floor_bps: floor.min_bitrate,
                    };
                    let reason = moment.reason();
                    match decide(moment) {
                        Some(Outcome::Link { reason }) => {
                            return Ok(Produced::Link {
                                output: None,
                                reason,
                            });
                        }
                        Some(Outcome::Skip { reason }) => return Ok(Produced::Skip(reason)),
                        Some(Outcome::Upload) | None => {
                            ctx.note(
                                job,
                                Some(stage),
                                format!("{reason}. Uploading regardless, as delivery says."),
                            )
                            .await?;
                        }
                    }
                }
            }
            Some(floor)
        }
        _ => None,
    };
    let output = match plan {
        Plan::Passthrough => {
            let subject = if job.artifacts.reused_from.is_some() && job.artifacts.archived.is_some()
            {
                "The reused output"
            } else {
                "The source"
            };
            ctx.note(
                job,
                Some(stage),
                format!(
                    "{subject} already meets the requirements, {}. Publishing it without conversion.",
                    describe_constraints(constraints, kind)
                ),
            )
            .await?;
            source.clone()
        }
        Plan::Transcode(target) => {
            let target = *target;
            ctx.note(
                job,
                Some(stage),
                format!(
                    "Source: {}, {} bytes. Converting to {}{}.",
                    info.map(describe_info).unwrap_or_else(|| kind.to_string()),
                    source.size,
                    describe_target(&target),
                    target
                        .clip()
                        .map(|c| format!(
                            ", keeping {:.1}s to {}",
                            c.start.as_secs_f64(),
                            c.end.map_or("the end".to_string(), |e| format!(
                                "{:.1}s",
                                e.as_secs_f64()
                            ))
                        ))
                        .unwrap_or_default(),
                ),
            )
            .await?;
            let (progress, forwarder) = ctx.progress(job.id, stage);
            let output = ctx
                .transcoder
                .transcode(source, &target, job_dir, progress)
                .await;
            let _ = forwarder.await;
            match output {
                Ok(transcoded) => {
                    for note in transcoded.notes {
                        ctx.note(job, Some(stage), note).await?;
                    }
                    transcoded.file
                }
                Err(TranscodeError::BudgetUnreachable {
                    max_bytes,
                    duration_secs,
                }) => {
                    return Ok(
                        match decide(Moment::BudgetUnreachable {
                            max_bytes,
                            duration_secs,
                        }) {
                            Some(Outcome::Link { reason }) => Produced::Link {
                                output: None,
                                reason,
                            },
                            Some(Outcome::Skip { reason }) => Produced::Skip(reason),
                            Some(Outcome::Upload) => {
                                return Err(TranscodeError::BudgetUnreachable {
                                    max_bytes,
                                    duration_secs,
                                }
                                .into());
                            }
                            None => Produced::Skip(format!(
                                "{duration_secs}s of video cannot be fit under the {max_bytes} \
                             bytes a page takes"
                            )),
                        },
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    if output.size > constraints.max_bytes {
        return match decide(Moment::OverLimit {
            size: output.size,
            max_bytes: constraints.max_bytes,
        }) {
            Some(Outcome::Link { reason }) => Ok(Produced::Link {
                output: None,
                reason,
            }),
            Some(Outcome::Skip { reason }) => Ok(Produced::Skip(reason)),
            Some(Outcome::Upload) => Err(StageError::Rejected(format!(
                "output is {} bytes, destination allows {}",
                output.size, constraints.max_bytes
            ))),
            None => Err(StageError::Rejected(format!(
                "output is {} bytes, the page takes {}",
                output.size, constraints.max_bytes
            ))),
        };
    }
    if let Some(floor) = floor
        && let Some(reason) = below_floor(&output, &floor)
    {
        match decide(Moment::BelowFloor { reason }) {
            Some(Outcome::Link { reason }) => {
                return Ok(Produced::Link {
                    output: None,
                    reason,
                });
            }
            Some(Outcome::Skip { reason }) => return Ok(Produced::Skip(reason)),
            Some(Outcome::Upload) | None => {}
        }
    }
    Ok(Produced::Upload(output))
}

/// The subtitles to render into the picture: the track fetched beside the media in the
/// preferred language when there is one, else a stream inside the file, the preferred
/// language's, else the one marked default, else the first. A TTML file is written out
/// as SubRip first, since ffmpeg draws that and not TTML. The reason when a track was
/// there but could not be prepared.
async fn burn_source(
    local: &[LocalSubtitle],
    info: &MediaInfo,
    language: Option<&str>,
    job_dir: &Path,
) -> Result<Option<BurnSource>, String> {
    let same_language = |candidate: &str| {
        language.is_some_and(|wanted| {
            let wanted = wanted.to_ascii_lowercase();
            let candidate = candidate.to_ascii_lowercase();
            candidate == wanted
                || candidate.starts_with(&format!("{wanted}-"))
                || wanted.starts_with(&format!("{candidate}-"))
        })
    };
    let external = local
        .iter()
        .find(|s| same_language(&s.language))
        .or_else(|| local.first());
    if let Some(track) = external {
        let label = track.name.clone().unwrap_or_else(|| track.language.clone());
        let path = match track.format {
            SubtitleFormat::Ttml => {
                let text = tokio::fs::read_to_string(&track.path)
                    .await
                    .map_err(|e| format!("subtitles not burnt in: {e}"))?;
                let srt = subtitles::ttml_to_srt(&text).ok_or_else(|| {
                    format!(
                        "subtitles not burnt in: {} holds no cues ffmpeg can draw",
                        track.path.display()
                    )
                })?;
                let converted = job_dir.join(format!(
                    "{}.burn.srt",
                    track
                        .path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "subtitles".into())
                ));
                tokio::fs::write(&converted, srt)
                    .await
                    .map_err(|e| format!("subtitles not burnt in: {e}"))?;
                converted
            }
            _ => track.path.clone(),
        };
        return Ok(Some(BurnSource::File { path, label }));
    }
    let embedded = info
        .subtitles
        .iter()
        .enumerate()
        .find(|(_, s)| s.language.as_deref().is_some_and(same_language))
        .or_else(|| info.subtitles.iter().enumerate().find(|(_, s)| s.default))
        .or_else(|| info.subtitles.iter().enumerate().next());
    Ok(embedded.map(|(position, stream)| BurnSource::Embedded {
        index: stream.index,
        position,
        bitmap: stream.bitmap,
        label: stream
            .name
            .clone()
            .or_else(|| stream.language.clone())
            .unwrap_or_else(|| stream.codec.clone()),
    }))
}

/// The picture sound alone plays over: the file's own cover art, else the platform's
/// thumbnail fetched beside it, else a waveform the transcoder draws.
async fn still_source(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    info: &MediaInfo,
    thumbnail: Option<&Url>,
) -> StillSource {
    if info.cover.is_some() {
        return StillSource::Cover;
    }
    let Some(thumbnail) = thumbnail else {
        return StillSource::Waveform;
    };
    let platform = job.resolver().unwrap_or("web").to_string();
    match fetch_thumbnail(ctx, &platform, thumbnail, job_dir).await {
        Ok(path) => StillSource::Picture { path },
        Err(error) => {
            tracing::info!(job = %job.id, %thumbnail, "thumbnail not fetched: {error}");
            let _ = ctx
                .note(
                    job,
                    Some(Stage::Transcode),
                    format!("Thumbnail not fetched ({error}). Drawing a waveform instead."),
                )
                .await;
            StillSource::Waveform
        }
    }
}

/// The still that stands for the output, as `poster.jpg` beside it: a frame of a video,
/// the picture itself scaled down, the cover art of sound that carries one, else the
/// platform's own picture for sound that carries none. Sound with no picture anywhere,
/// and a file that is not media, get none. Anything that goes wrong on the way is noted
/// in the job's log; the output stands without a still.
async fn make_thumbnail(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    output: &LocalFile,
) -> Option<LocalFile> {
    let dest = job_dir.join("poster.jpg");
    let outcome = match ctx.transcoder.thumbnail(output, &dest).await {
        Ok(file) => Ok(file),
        Err(TranscodeError::NotMedia) => return None,
        Err(TranscodeError::NoPicture) => {
            let platform = job.resolver().unwrap_or("web").to_string();
            let picture = job
                .artifacts
                .resolved
                .as_ref()
                .and_then(|r| r.thumbnail.clone())?;
            match fetch_thumbnail(ctx, &platform, &picture, job_dir).await {
                Ok(path) => match LocalFile::from_path(path).await {
                    Ok(fetched) => ctx
                        .transcoder
                        .thumbnail(&fetched, &dest)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error.to_string()),
    };
    match outcome {
        Ok(file) => Some(file),
        Err(error) => {
            let _ = ctx
                .note(
                    job,
                    Some(Stage::Transcode),
                    format!("Thumbnail not made: {error}"),
                )
                .await;
            None
        }
    }
}

/// How large a thumbnail may be.
const MAX_THUMBNAIL: usize = 16 * 1024 * 1024;

async fn fetch_thumbnail(
    ctx: &Context,
    platform: &str,
    url: &Url,
    job_dir: &Path,
) -> Result<PathBuf, String> {
    let response = ctx
        .http
        .get(url.clone())
        .platform(platform)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.is_success() {
        return Err(format!("HTTP {}", response.status.as_u16()));
    }
    let content_type = response
        .header("content-type")
        .map(|value| value.to_string());
    let bytes = response
        .bytes(MAX_THUMBNAIL)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty response".into());
    }
    let extension = content_type
        .as_deref()
        .and_then(crate::media::Container::from_mime)
        .map(|c| c.extension().to_string())
        .or_else(|| crate::resolve::path_extension(url))
        .unwrap_or_else(|| "jpg".into());
    let path = job_dir.join(format!("thumbnail.{extension}"));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    let probed = ctx
        .transcoder
        .probe(&path)
        .await
        .map_err(|e| e.to_string())?;
    if probed.kind != MediaKind::Image {
        return Err(format!("{} is not a picture", url));
    }
    Ok(path)
}

fn describe_variant(v: &crate::resolve::Variant) -> String {
    let mut parts = Vec::new();
    if let (Some(w), Some(h)) = (v.width, v.height) {
        parts.push(format!("{w}x{h}"));
    } else if let Some(h) = v.height {
        parts.push(format!("{h}p"));
    }
    if let Some(codec) = &v.video {
        parts.push(format!("{codec:?}").to_lowercase());
    }
    if let Some(b) = v.bitrate {
        parts.push(format!("{} kb/s", b / 1000));
    }
    if let Some(s) = v.size {
        parts.push(format!("{s} bytes"));
    }
    if v.audio_url.is_some() {
        parts.push("with separate audio".into());
    }
    if parts.is_empty() {
        v.url.to_string()
    } else {
        parts.join(", ")
    }
}

fn describe_info(info: &crate::media::MediaInfo) -> String {
    let mut s = format!("{} {}", info.kind, info.container.extension());
    if let Some(v) = &info.video {
        s.push_str(&format!(" {} {}x{}", v.codec.as_str(), v.width, v.height));
    }
    if let Some(a) = &info.audio {
        s.push_str(&format!(" {}", a.codec.as_str()));
    }
    if let Some(d) = info.duration {
        s.push_str(&format!(" {:.1}s", d.as_secs_f64()));
    }
    s
}

fn describe_constraints(c: &Constraints, kind: MediaKind) -> String {
    let list = |containers: &[Container]| {
        containers
            .iter()
            .map(Container::extension)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match kind {
        MediaKind::Video => format!(
            "{}/{} under {} bytes",
            c.preferred_container().extension(),
            c.preferred_video().as_str(),
            c.max_bytes
        ),
        MediaKind::Audio => format!(
            "audio in {} under {} bytes",
            list(&c.audio_containers),
            c.max_bytes
        ),
        MediaKind::Image => format!(
            "images in {} under {} bytes",
            list(&c.image_containers),
            c.max_bytes
        ),
        MediaKind::File => format!("files under {} bytes", c.max_bytes),
    }
}

fn describe_target(target: &Target) -> String {
    match target {
        Target::Video(t) => format!(
            "{:?}/{:?} under {} bytes{}{}",
            t.container,
            t.video,
            t.max_bytes,
            match &t.burn {
                Some(burn) => format!(", burning in the {} subtitles", burn.label()),
                None => String::new(),
            },
            if t.still.is_some() {
                ", playing the sound over a still"
            } else {
                ""
            }
        ),
        Target::Audio(t) => format!(
            "{:?}/{:?} under {} bytes",
            t.container, t.codec, t.max_bytes
        ),
        Target::Image(t) => format!("{:?} under {} bytes", t.container, t.max_bytes),
        Target::File { max_bytes } => format!("a file under {max_bytes} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{ClipRange, SubtitleTrack};

    fn track(language: &str, auto: bool) -> SubtitleTrack {
        SubtitleTrack {
            url: Url::parse(&format!("https://s.test/{language}/{auto}")).unwrap(),
            language: language.into(),
            name: None,
            format: SubtitleFormat::Vtt,
            auto,
            headers: Vec::new(),
        }
    }

    #[test]
    fn subtitles_prefer_the_asked_language_and_human_tracks() {
        let tracks = vec![
            track("en", true),
            track("en", false),
            track("de", false),
            track("en-GB", false),
        ];
        let picked = subtitles::pick(&tracks, Some("EN"));
        assert_eq!(picked.len(), 2);
        assert!(picked.iter().all(|t| !t.auto));
        assert_eq!(picked[0].language, "en");
        let all = subtitles::pick(&tracks, Some("fr"));
        assert_eq!(all.len(), 3);
        assert!(
            all.iter()
                .find(|t| t.language == "en")
                .is_some_and(|t| !t.auto)
        );
    }

    #[test]
    fn clips_shorten_the_effective_duration() {
        let full = Some(Duration::from_secs(100));
        assert_eq!(effective_duration(full, None), full);
        assert_eq!(
            effective_duration(
                full,
                Some(ClipRange {
                    start: Duration::from_secs(20),
                    end: Some(Duration::from_secs(30))
                })
            ),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            effective_duration(
                full,
                Some(ClipRange {
                    start: Duration::from_secs(95),
                    end: Some(Duration::from_secs(300))
                })
            ),
            Some(Duration::from_secs(5))
        );
        assert_eq!(effective_duration(None, None), None);
    }
}
