use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::WatchStream;
use tokio_util::sync::CancellationToken;

use crate::archive::Archiver;
use crate::config::EngineConfig;
use crate::download::{DownloadContext, Downloaded, Downloader, SubtitleChoice, subtitles};
use crate::error::StageError;
use crate::event::{EngineEvent, EventKind, Progress, ProgressSender};
use crate::http::Http;
use crate::job::{
    Delivery, Job, JobId, JobStatus, LogEntry, Request, SourceId, Stage, SubtitleMode,
};
use crate::media::{LocalFile, MediaInfo, MediaKind};
use crate::plan::{self, Plan};
use crate::publish::{self, Constraints, Publisher, QualityFloor};
use crate::resolve::{Resolution, Resolved, ResolverRegistry};
use crate::store::{JobStore, StoreError};
use crate::transcode::{
    AudioTarget, ImageTarget, Target, TranscodeError, Transcoder, VideoTarget, clipped_duration,
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
        tracing::info!(job = %job.id, stage = ?stage, "{}", entry.message);
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
    Failed { stage: Stage, message: String },
    Cancelled,
    Shutdown,
}

fn failed(stage: Stage) -> impl Fn(StageError) -> Interrupt {
    move |error| Interrupt::Failed {
        stage: error.stage(stage),
        message: error.to_string(),
    }
}

pub(crate) async fn run_job(
    ctx: Arc<Context>,
    mut job: Job,
    cancel: CancellationToken,
    shutdown: CancellationToken,
) {
    let job_dir = ctx.job_dir(job.id);
    let outcome = tokio::select! {
        result = execute(&ctx, &mut job, &job_dir) => result,
        _ = cancel.cancelled() => Err(Interrupt::Cancelled),
        _ = shutdown.cancelled() => Err(Interrupt::Shutdown),
    };
    let status = match outcome {
        Ok(()) => JobStatus::Done,
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
    if job.status == JobStatus::Done {
        tidy_job_dir(&job, &job_dir).await;
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

/// Removes what a finished job left in its directory beyond [`kept_files`]; a directory
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
    let settings = ctx.config().playlists;
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
            "{} listed {total} entries; queued {} as separate jobs",
            playlist.resolver,
            ids.len()
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

async fn execute(ctx: &Context, job: &mut Job, job_dir: &Path) -> Result<(), Interrupt> {
    let config = ctx.config();
    let limits = job.request.limits.applied_to(&config.limits);
    let url = job.request.url.clone();
    let origin = job.request.origin.clone();

    // Resolve
    let stage = Stage::Resolve;
    ctx.transition(job, JobStatus::Running { stage })
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let resolution = ctx
        .resolvers
        .resolve_without(&url, &job.request.disabled_platforms)
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let resolved = match resolution {
        Resolution::Media(resolved) => *resolved,
        Resolution::Playlist(playlist) => return expand_playlist(ctx, job, playlist).await,
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
    if resolved.live && limits.max_duration_secs.is_some_and(|l| l == 0) {
        return Err(failed(stage)(StageError::Rejected(
            "live streams are not accepted here".into(),
        )));
    }
    let variant = plan::select_variant(&resolved.variants, &limits, &resolved.resolver, media)
        .map_err(|e| failed(stage)(e.into()))?;
    ctx.note(
        job,
        Some(stage),
        format!(
            "{} resolved {media} with {} variant(s); selected {} {}",
            resolved.resolver,
            resolved.variants.len(),
            variant.kind.as_str(),
            describe_variant(&variant)
        ),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    job.artifacts.resolved = Some(resolved.clone());

    // Download
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
    let context = DownloadContext {
        max_bytes: limits.max_source_bytes,
        max_height: limits.max_height,
        max_live: Duration::from_secs(
            limits
                .max_duration_secs
                .unwrap_or(config.live.max_capture_secs)
                .min(config.live.max_capture_secs),
        ),
        clip,
        platform: resolved.resolver.clone(),
        download: config.download.clone(),
        subtitles: (job.request.options.subtitles != SubtitleMode::Skip).then(|| {
            SubtitleChoice {
                tracks: subtitles::pick(
                    &resolved.subtitles,
                    job.request.options.subtitle_language.as_deref(),
                ),
                language: job.request.options.subtitle_language.clone(),
            }
        }),
    };
    let (progress, forwarder) = ctx.progress(job.id, stage);
    let downloaded = downloader
        .download(&variant, job_dir, &context, progress)
        .await;
    let _ = forwarder.await;
    let Downloaded {
        file: mut source,
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
                .filter(|t| !local_subtitles.iter().any(|s| s.url.as_ref() == Some(&t.url)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !remaining.is_empty() {
        match subtitles::fetch_all(&ctx.http, &resolved.resolver, &remaining, job_dir).await {
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
            "downloaded {} bytes to {}{}",
            source.size,
            source.path.display(),
            if local_subtitles.is_empty() {
                String::new()
            } else {
                format!(" with {} subtitle track(s)", local_subtitles.len())
            }
        ),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;

    // Transcode
    let stage = Stage::Transcode;
    ctx.transition(job, JobStatus::Running { stage })
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let publisher = ctx.publishers.get(&origin.source).cloned().ok_or_else(|| {
        failed(stage)(StageError::Rejected(format!(
            "no publisher registered for source {}",
            origin.source
        )))
    })?;
    let constraints = publisher
        .constraints(job)
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    // A file that is not media is never probed; anything else is, and what the probe
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
                "{} called this {media}; the file is {kind}",
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
    source.info = info.clone();
    job.artifacts.source = Some(source.clone());
    let burn = match job.request.options.subtitles {
        SubtitleMode::Burn => local_subtitles.first().map(|s| s.path.clone()),
        _ => None,
    };
    let produced = produce(
        ctx,
        job,
        job_dir,
        &source,
        info.as_ref(),
        kind,
        &constraints,
        &limits,
        clip,
        burn.clone(),
    )
    .await;
    // A destination with a link to fall back on gets one when the upload cannot be
    // made, or would be too reduced; the output is then made for the page instead.
    let output = match produced {
        Ok(Produced::Upload(output)) => output,
        Ok(Produced::Link { output, reason }) => {
            let link = constraints
                .for_link()
                .expect("a link is produced only with a fallback");
            job.artifacts.delivery = Delivery::Link;
            job.artifacts.link_reason = Some(reason.clone());
            ctx.note(
                job,
                Some(stage),
                format!("{reason}; a link to the page will be posted instead of the file"),
            )
            .await
            .map_err(|e| failed(stage)(e.into()))?;
            match output {
                Some(output) => output,
                None => match produce(
                    ctx,
                    job,
                    job_dir,
                    &source,
                    info.as_ref(),
                    kind,
                    &link,
                    &limits,
                    clip,
                    burn,
                )
                .await
                .map_err(|e| failed(stage)(e))?
                {
                    Produced::Upload(output)
                    | Produced::Link {
                        output: Some(output),
                        ..
                    } => output,
                    Produced::Link { output: None, .. } => {
                        unreachable!("a destination without a fallback never asks for a link")
                    }
                },
            }
        }
        Err(error) => return Err(failed(stage)(error)),
    };
    job.artifacts.output = Some(output.clone());
    ctx.note(
        job,
        Some(stage),
        format!("output ready: {} bytes", output.size),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;

    // Publish
    let stage = Stage::Publish;
    ctx.transition(job, JobStatus::Running { stage })
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    let published = publisher
        .publish(job, &output)
        .await
        .map_err(|e| failed(stage)(e.into()))?;
    ctx.note(
        job,
        Some(stage),
        format!("published as {}", published.reference),
    )
    .await
    .map_err(|e| failed(stage)(e.into()))?;
    job.artifacts.published = Some(published);

    // Archive
    if let Some(archiver) = ctx.archiver.as_ref().filter(|a| a.enabled()) {
        let stage = Stage::Archive;
        ctx.transition(job, JobStatus::Running { stage })
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        let entry = archiver
            .archive(job)
            .await
            .map_err(|e| failed(stage)(e.into()))?;
        ctx.note(
            job,
            Some(stage),
            format!(
                "archived {} file(s), {} bytes",
                entry.files.len(),
                entry.bytes
            ),
        )
        .await
        .map_err(|e| failed(stage)(e.into()))?;
        job.artifacts.archived = Some(entry);
    }
    Ok(())
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

/// Makes the output for `constraints`: the source as it is when it fits, else a
/// transcode. With a fallback in the constraints, a video that cannot fit or would come
/// out under the quality floor, and a file too large to upload, become a link instead.
#[allow(clippy::too_many_arguments)]
async fn produce(
    ctx: &Context,
    job: &mut Job,
    job_dir: &Path,
    source: &LocalFile,
    info: Option<&MediaInfo>,
    kind: MediaKind,
    constraints: &Constraints,
    limits: &crate::config::Limits,
    clip: Option<crate::resolve::ClipRange>,
    burn: Option<PathBuf>,
) -> Result<Produced, StageError> {
    let stage = Stage::Transcode;
    let max_height = limits
        .max_height
        .min(constraints.max_height.unwrap_or(u32::MAX));
    let target = match kind {
        MediaKind::Video => {
            let mut target = VideoTarget::new(
                constraints.preferred_container(),
                constraints.preferred_video(),
                info.and_then(|i| i.audio.as_ref())
                    .map(|_| constraints.preferred_audio()),
                constraints.max_bytes,
                max_height,
            );
            target.clip = clip;
            target.burn_subtitles = burn;
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
    };
    let plan = match plan::plan(
        source,
        info,
        constraints,
        &crate::config::Limits {
            max_height,
            ..limits.clone()
        },
        target,
    ) {
        Ok(plan) => plan,
        Err(TranscodeError::CannotShrink { size, max_bytes })
            if constraints
                .fallback
                .as_ref()
                .is_some_and(|f| size <= f.max_bytes) =>
        {
            return Ok(Produced::Link {
                output: Some(source.clone()),
                reason: format!(
                    "the file is {size} bytes, over the {max_bytes} the destination takes"
                ),
            });
        }
        Err(error) => return Err(error.into()),
    };
    // A video whose byte budget cannot hold the floor is not worth encoding for upload.
    let floor = match (&constraints.fallback, info) {
        (Some(fallback), Some(info)) if kind == MediaKind::Video => {
            let floor = floor_for(&fallback.floor, source, info);
            let secs = clipped_duration(clip, info.duration.unwrap_or_default()).as_secs_f64();
            if matches!(plan, Plan::Transcode(_)) && secs > 0.0 {
                let budget = (constraints.max_bytes as f64 * 8.0 * 0.95 / secs) as u64;
                if budget < floor.min_bitrate {
                    return Ok(Produced::Link {
                        output: None,
                        reason: format!(
                            "{} bytes hold only {} kb/s of {:.0}s, under the {} kb/s floor",
                            constraints.max_bytes,
                            budget / 1000,
                            secs,
                            floor.min_bitrate / 1000
                        ),
                    });
                }
            }
            Some(floor)
        }
        _ => None,
    };
    let output = match plan {
        Plan::Passthrough => {
            ctx.note(
                job,
                Some(stage),
                format!(
                    "source already satisfies {}; publishing as is",
                    describe_constraints(constraints, kind)
                ),
            )
            .await?;
            source.clone()
        }
        Plan::Transcode(target) => {
            ctx.note(
                job,
                Some(stage),
                format!(
                    "source is {} {}; converting to {}{}",
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
                Ok(output) => output,
                Err(TranscodeError::BudgetUnreachable {
                    max_bytes,
                    duration_secs,
                }) if constraints.fallback.is_some() => {
                    return Ok(Produced::Link {
                        output: None,
                        reason: format!(
                            "{duration_secs}s of video cannot be fit under {max_bytes} bytes"
                        ),
                    });
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    if output.size > constraints.max_bytes {
        return Err(StageError::Rejected(format!(
            "output is {} bytes, destination allows {}",
            output.size, constraints.max_bytes
        )));
    }
    if let Some(floor) = floor
        && let Some(reason) = below_floor(&output, &floor)
    {
        return Ok(Produced::Link {
            output: None,
            reason,
        });
    }
    Ok(Produced::Upload(output))
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
    let mut s = format!("{} {:?}", info.kind, info.container);
    if let Some(v) = &info.video {
        s.push_str(&format!(" {:?} {}x{}", v.codec, v.width, v.height));
    }
    if let Some(a) = &info.audio {
        s.push_str(&format!(" {:?}", a.codec));
    }
    if let Some(d) = info.duration {
        s.push_str(&format!(" {:.1}s", d.as_secs_f64()));
    }
    s
}

fn describe_constraints(c: &Constraints, kind: MediaKind) -> String {
    match kind {
        MediaKind::Video => format!(
            "{:?}/{:?} under {} bytes",
            c.preferred_container(),
            c.preferred_video(),
            c.max_bytes
        ),
        MediaKind::Audio => format!(
            "audio in {:?} under {} bytes",
            c.audio_containers, c.max_bytes
        ),
        MediaKind::Image => format!(
            "images in {:?} under {} bytes",
            c.image_containers, c.max_bytes
        ),
        MediaKind::File => format!("files under {} bytes", c.max_bytes),
    }
}

fn describe_target(target: &Target) -> String {
    match target {
        Target::Video(t) => format!(
            "{:?}/{:?} under {} bytes{}",
            t.container,
            t.video,
            t.max_bytes,
            if t.burn_subtitles.is_some() {
                ", burning subtitles in"
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
    use crate::resolve::{ClipRange, SubtitleFormat, SubtitleTrack};
    use url::Url;

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
