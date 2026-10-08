use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::archive::Archiver;
use crate::config::EngineConfig;
use crate::download::Downloader;
use crate::event::{EngineEvent, EventKind};
use crate::http::Http;
use crate::job::{Job, JobId, JobStatus, Request, SourceId, Stage, StatusKind};
use crate::media::LocalFile;
use crate::pipeline::{self, Context};
use crate::policy::Policy;
use crate::publish::Publisher;
use crate::resolve::{
    Platform, Resolution, ResolveError, Resolver, ResolverRegistry, SessionCheck,
};
use crate::store::{JobFilter, JobStore, ResolverStats, Stats, StoreError};
use crate::transcode::{TranscodeError, Transcoder};

const EVENT_CAPACITY: usize = 4096;
const QUEUE_CAPACITY: usize = 10_000;

pub struct EngineBuilder {
    config: EngineConfig,
    store: Arc<dyn JobStore>,
    http: Http,
    resolvers: Vec<Arc<dyn Resolver>>,
    downloaders: Vec<Arc<dyn Downloader>>,
    transcoder: Option<Arc<dyn Transcoder>>,
    publishers: HashMap<SourceId, Arc<dyn Publisher>>,
    archiver: Option<Arc<dyn Archiver>>,
}

impl EngineBuilder {
    pub fn new(config: EngineConfig, store: Arc<dyn JobStore>, http: Http) -> Self {
        Self {
            config,
            store,
            http,
            resolvers: Vec::new(),
            downloaders: Vec::new(),
            transcoder: None,
            publishers: HashMap::new(),
            archiver: None,
        }
    }

    pub fn resolver(mut self, resolver: impl Resolver + 'static) -> Self {
        self.resolvers.push(Arc::new(resolver));
        self
    }

    pub fn resolver_arc(mut self, resolver: Arc<dyn Resolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }

    /// The ids of the resolvers registered so far.
    pub fn resolver_ids(&self) -> Vec<&'static str> {
        self.resolvers.iter().map(|r| r.id()).collect()
    }

    /// What the resolvers registered so far cover.
    pub fn platforms(&self) -> Vec<Platform> {
        self.resolvers.iter().map(|r| r.platform()).collect()
    }

    pub fn downloader(mut self, downloader: impl Downloader + 'static) -> Self {
        self.downloaders.push(Arc::new(downloader));
        self
    }

    pub fn transcoder(mut self, transcoder: impl Transcoder + 'static) -> Self {
        self.transcoder = Some(Arc::new(transcoder));
        self
    }

    pub fn publisher(mut self, publisher: impl Publisher + 'static) -> Self {
        let publisher: Arc<dyn Publisher> = Arc::new(publisher);
        self.publishers
            .insert(publisher.source().clone(), publisher);
        self
    }

    pub fn publisher_arc(mut self, publisher: Arc<dyn Publisher>) -> Self {
        self.publishers
            .insert(publisher.source().clone(), publisher);
        self
    }

    pub fn archiver(mut self, archiver: impl Archiver + 'static) -> Self {
        self.archiver = Some(Arc::new(archiver));
        self
    }

    pub fn build(self) -> Result<Engine, EngineError> {
        if self.resolvers.is_empty() {
            return Err(EngineError::Incomplete("no resolvers registered"));
        }
        if self.downloaders.is_empty() {
            return Err(EngineError::Incomplete("no downloaders registered"));
        }
        if self.publishers.is_empty() {
            return Err(EngineError::Incomplete("no publishers registered"));
        }
        let transcoder = self
            .transcoder
            .ok_or(EngineError::Incomplete("no transcoder registered"))?;
        let (submit, queue) = mpsc::channel(QUEUE_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        for resolver in &self.resolvers {
            self.http
                .seed_cookies(resolver.id(), resolver.consent_cookies());
        }
        let resolvers = Arc::new(ResolverRegistry::new(self.resolvers));
        let workers = self.config.workers.max(1);
        let config = Arc::new(RwLock::new(self.config));
        let shared = Arc::new(Shared {
            resolvers: resolvers.clone(),
            store: self.store.clone(),
            http: self.http.clone(),
            sources: self.publishers.keys().cloned().collect(),
            submit: submit.clone(),
            events: events.clone(),
            active: Mutex::new(HashMap::new()),
            workers: AtomicUsize::new(workers),
            semaphore: Arc::new(Semaphore::new(workers)),
            queued: AtomicUsize::new(0),
            config: config.clone(),
            archiver: self.archiver.clone(),
            transcoder: transcoder.clone(),
        });
        let context = Arc::new(Context {
            config,
            http: self.http,
            resolvers,
            downloaders: self.downloaders,
            transcoder,
            publishers: self.publishers,
            archiver: self.archiver,
            store: self.store,
            events,
            submit,
        });
        Ok(Engine {
            context,
            queue,
            handle: EngineHandle { shared },
        })
    }
}

pub struct Engine {
    context: Arc<Context>,
    queue: mpsc::Receiver<JobId>,
    handle: EngineHandle,
}

impl Engine {
    pub fn builder(config: EngineConfig, store: Arc<dyn JobStore>, http: Http) -> EngineBuilder {
        EngineBuilder::new(config, store, http)
    }

    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// Recovers interrupted jobs, then dispatches queued jobs to workers until `shutdown`.
    /// Then the jobs under way get the settings' grace period to finish. Those still
    /// running after it are interrupted and queued again for the next start, except a
    /// job publishing its output, which is left to finish.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), EngineError> {
        let ctx = self.context.clone();
        let shared = self.handle.shared.clone();
        tokio::fs::create_dir_all(ctx.cache_dir().join("jobs")).await?;
        self.recover().await?;

        let workers = shared.workers.load(Ordering::Relaxed);
        let semaphore = shared.semaphore.clone();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let interrupt = CancellationToken::new();
        tracing::info!(workers, "engine running");

        loop {
            let permit = tokio::select! {
                _ = shutdown.cancelled() => break,
                permit = semaphore.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let id = tokio::select! {
                _ = shutdown.cancelled() => break,
                id = self.queue.recv() => match id {
                    Some(id) => id,
                    None => break,
                },
            };
            shared.queued.fetch_sub(1, Ordering::Relaxed);
            let active_job = Active {
                cancel: CancellationToken::new(),
                stop: CancellationToken::new(),
            };
            {
                // Recovery and a concurrent submit can both queue the same id. Run it once.
                let mut active = shared.active.lock().expect("active jobs lock");
                if active.contains_key(&id) {
                    tracing::debug!(job = %id, "Job already running. Skipping duplicate dispatch.");
                    continue;
                }
                active.insert(id, active_job.clone());
            }
            let job = match ctx.store.get(id).await {
                Ok(Some(job)) if job.status == JobStatus::Queued => job,
                Ok(Some(job)) => {
                    tracing::debug!(job = %id, status = job.status.kind().as_str(), "skipping job that is no longer queued");
                    shared.active.lock().expect("active jobs lock").remove(&id);
                    continue;
                }
                Ok(None) => {
                    tracing::warn!(job = %id, "queued job vanished from store");
                    shared.active.lock().expect("active jobs lock").remove(&id);
                    continue;
                }
                Err(error) => {
                    tracing::error!(job = %id, "could not load job: {error}");
                    shared.active.lock().expect("active jobs lock").remove(&id);
                    continue;
                }
            };
            let ctx = ctx.clone();
            let shared = shared.clone();
            let interrupt = interrupt.clone();
            tasks.spawn(async move {
                pipeline::run_job(ctx, job, active_job.cancel, active_job.stop, interrupt).await;
                drop(permit);
                shared.active.lock().expect("active jobs lock").remove(&id);
            });
            while let Some(result) = tasks.try_join_next() {
                if let Err(error) = result {
                    tracing::error!("job task panicked: {error}");
                }
            }
        }

        let in_flight = tasks.len();
        let grace = Duration::from_secs(ctx.config().shutdown.grace_secs);
        if in_flight > 0 {
            tracing::info!(
                in_flight,
                grace_secs = grace.as_secs(),
                "engine stopping: jobs under way get the grace period to finish"
            );
        } else {
            tracing::info!("engine stopping");
        }
        let deadline = tokio::time::sleep(grace);
        tokio::pin!(deadline);
        let mut interrupted = false;
        loop {
            tokio::select! {
                next = tasks.join_next() => match next {
                    Some(Err(error)) => tracing::error!("job task panicked: {error}"),
                    Some(Ok(())) => {}
                    None => break,
                },
                _ = &mut deadline, if !interrupted => {
                    interrupted = true;
                    tracing::warn!(
                        remaining = tasks.len(),
                        "grace period over: interrupting the jobs under way. Each is queued again for the next start, except one publishing its output, which finishes."
                    );
                    interrupt.cancel();
                }
            }
        }
        tracing::info!("engine stopped");
        Ok(())
    }

    /// Requeues the jobs that were running or waiting when the engine last stopped, and
    /// removes cache directories of jobs the store no longer has. A finished job's
    /// directory stays, since it holds the output the web app serves, until retention
    /// takes it. A job that was capturing a live stream keeps its recording and carries
    /// on from it, as if the capture had been stopped.
    async fn recover(&self) -> Result<(), EngineError> {
        let ctx = &self.context;
        let active = ctx.store.list_active().await?;
        for mut job in active {
            if matches!(job.status, JobStatus::Running { .. }) {
                let recorded = match &job.artifacts.recording {
                    Some(recording) => tokio::fs::metadata(&recording.path)
                        .await
                        .is_ok_and(|m| m.is_file() && m.len() > 0),
                    None => false,
                };
                if recorded
                    && job.status
                        == (JobStatus::Running {
                            stage: Stage::Download,
                        })
                {
                    ctx.note(
                        &mut job,
                        Some(Stage::Download),
                        "Engine restarted during the capture. It carries on from the recording as it stands.",
                    )
                    .await?;
                } else {
                    job.artifacts.recording = None;
                    ctx.note(&mut job, None, "requeued after engine restart")
                        .await?;
                }
                ctx.transition(&mut job, JobStatus::Queued).await?;
            }
            self.handle
                .shared
                .enqueue(job.id)
                .await
                .map_err(|_| EngineError::Incomplete("job queue closed during recovery"))?;
        }
        let jobs_dir = ctx.cache_dir().join("jobs");
        let mut entries = tokio::fs::read_dir(&jobs_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let known = match name.parse::<JobId>() {
                Ok(id) => ctx.store.get(id).await?.is_some_and(|job| {
                    (job.status == JobStatus::Done && !pipeline::kept_files(&job).is_empty())
                        || (job.status == JobStatus::Queued && job.artifacts.recording.is_some())
                }),
                Err(_) => false,
            };
            if !known && let Err(error) = tokio::fs::remove_dir_all(entry.path()).await {
                tracing::warn!(
                    "could not remove stale cache dir {}: {error}",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }
}

/// The tokens a running job listens to: one that stops it wherever it is, one that ends
/// its live capture and keeps the recording.
#[derive(Clone)]
struct Active {
    cancel: CancellationToken,
    stop: CancellationToken,
}

struct Shared {
    resolvers: Arc<ResolverRegistry>,
    store: Arc<dyn JobStore>,
    http: Http,
    sources: HashSet<SourceId>,
    submit: mpsc::Sender<JobId>,
    events: broadcast::Sender<EngineEvent>,
    active: Mutex<HashMap<JobId, Active>>,
    /// How many jobs may run concurrently. The semaphore holds that many permits.
    workers: AtomicUsize,
    semaphore: Arc<Semaphore>,
    queued: AtomicUsize,
    config: Arc<RwLock<EngineConfig>>,
    archiver: Option<Arc<dyn Archiver>>,
    /// Makes the stills of outputs that finished before stills were made with them.
    transcoder: Arc<dyn Transcoder>,
}

impl Shared {
    async fn enqueue(&self, id: JobId) -> Result<(), ()> {
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.submit.send(id).await.map_err(|_| {
            self.queued.fetch_sub(1, Ordering::Relaxed);
        })
    }

    fn cache_dir(&self) -> PathBuf {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .cache_dir
            .clone()
    }

    /// Grows or shrinks the worker pool to `wanted`. Growing frees permits immediately.
    /// shrinking takes permits back as running jobs release them, so nothing is
    /// interrupted.
    fn resize_workers(self: &Arc<Self>, wanted: usize) {
        let wanted = wanted.max(1);
        let current = self.workers.swap(wanted, Ordering::SeqCst);
        if wanted > current {
            self.semaphore.add_permits(wanted - current);
        } else if wanted < current {
            let shrink = current - wanted;
            let semaphore = self.semaphore.clone();
            tokio::spawn(async move {
                if let Ok(permits) = semaphore.acquire_many(shrink as u32).await {
                    permits.forget();
                }
            });
        }
    }
}

/// Why a job's still could not be had.
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("thumbnail not made: {0}")]
    Transcode(#[from] TranscodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How busy the engine is right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Utilisation {
    pub workers: usize,
    pub active: usize,
    /// Jobs dispatched but not yet picked up by a worker.
    pub waiting: usize,
}

/// What the store and a resolver say about a platform's stored session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformSession {
    pub platform: String,
    pub cookies: usize,
    pub check: SessionCheck,
}

#[derive(Clone)]
pub struct EngineHandle {
    shared: Arc<Shared>,
}

impl EngineHandle {
    pub fn supports(&self, url: &Url) -> bool {
        self.shared.resolvers.supports(url)
    }

    pub fn resolvers(&self) -> Vec<&'static str> {
        self.shared.resolvers.ids()
    }

    pub fn platforms(&self) -> Vec<Platform> {
        self.shared.resolvers.platforms()
    }

    /// The display name of the platform behind a resolver id, else the id itself
    pub fn platform_name(&self, id: &str) -> String {
        self.shared
            .resolvers
            .get(id)
            .map(|r| r.platform().name.to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// The resolver that would take `url`.
    pub fn resolver_for(&self, url: &Url) -> Option<&'static str> {
        self.shared.resolvers.find(url).map(|r| r.id())
    }

    /// Every resolver that would take `url`, in the order they are offered it.
    pub fn resolvers_for(&self, url: &Url) -> Vec<&'static str> {
        self.shared.resolvers.matching(url)
    }

    /// Resolves `url` without queuing a job: what a fixture run needs, and nothing more.
    pub async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        self.shared.resolvers.resolve(url).await
    }

    pub fn http(&self) -> &Http {
        &self.shared.http
    }

    pub fn sources(&self) -> Vec<SourceId> {
        let mut sources: Vec<SourceId> = self.shared.sources.iter().cloned().collect();
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        sources
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.shared.events.subscribe()
    }

    pub fn utilisation(&self) -> Utilisation {
        Utilisation {
            workers: self.shared.workers.load(Ordering::Relaxed),
            active: self.shared.active.lock().expect("active jobs lock").len(),
            waiting: self.shared.queued.load(Ordering::Relaxed),
        }
    }

    /// The engine's settings as they stand now.
    pub fn config(&self) -> EngineConfig {
        self.shared
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the engine's settings while it runs: limits, playlist and live capture
    /// rules, retention and archiving apply to the next job, the worker pool grows or
    /// shrinks, and jobs from now on work under the new cache directory, whose job
    /// directory is created here. Jobs already running finish where they started.
    pub async fn reconfigure(&self, config: EngineConfig) -> Result<(), std::io::Error> {
        tokio::fs::create_dir_all(config.cache_dir.join("jobs")).await?;
        if let Some(archiver) = &self.shared.archiver {
            archiver.reconfigure(config.archive.clone());
        }

        self.shared.resize_workers(config.workers);
        *self
            .shared
            .config
            .write()
            .unwrap_or_else(|e| e.into_inner()) = config;
        Ok(())
    }

    /// The ids of the jobs running right now.
    pub fn active(&self) -> Vec<JobId> {
        let mut ids: Vec<JobId> = self
            .shared
            .active
            .lock()
            .expect("active jobs lock")
            .keys()
            .copied()
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    /// Asks `platform`'s resolver what its stored cookies are worth.
    pub async fn check_session(&self, platform: &str) -> Result<PlatformSession, ResolveError> {
        let resolver = self.shared.resolvers.get(platform).ok_or_else(|| {
            ResolveError::Unsupported(
                Url::parse(&format!("platform://{platform}")).expect("platform ids are urls"),
            )
        })?;
        let cookies = self.shared.http.jar(platform).len();
        let check = resolver.check_session().await?;
        Ok(PlatformSession {
            platform: platform.to_string(),
            cookies,
            check,
        })
    }

    pub async fn submit(&self, request: Request) -> Result<JobId, SubmitError> {
        if !self.shared.resolvers.supports(&request.url) {
            return Err(SubmitError::Unsupported(request.url));
        }
        if !self
            .shared
            .resolvers
            .supports_without(&request.url, &request.disabled_platforms)
        {
            let platform = self
                .shared
                .resolvers
                .matching(&request.url)
                .first()
                .copied()
                .expect("a resolver matches");
            return Err(SubmitError::Disabled {
                url: request.url,
                platform,
            });
        }
        if !self.shared.sources.contains(&request.origin.source) {
            return Err(SubmitError::UnknownSource(request.origin.source));
        }
        let permit = self
            .shared
            .submit
            .reserve()
            .await
            .map_err(|_| SubmitError::Closed)?;
        let job = Job::new(request);
        self.shared.store.insert(&job).await?;
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        permit.send(job.id);
        let _ = self.shared.events.send(EngineEvent {
            job: job.id,
            at: Timestamp::now(),
            kind: EventKind::Submitted {
                request: Box::new(job.request.clone()),
            },
        });
        let _ = self.shared.events.send(EngineEvent {
            job: job.id,
            at: Timestamp::now(),
            kind: EventKind::Status {
                status: JobStatus::Queued,
            },
        });
        Ok(job.id)
    }

    /// Queues the request of a finished job again under the policy in force where its link was seen
    pub async fn retry(
        &self,
        id: JobId,
        disabled_platforms: Vec<String>,
        policy: Policy,
    ) -> Result<JobId, RetryError> {
        let job = self
            .shared
            .store
            .get(id)
            .await?
            .ok_or(RetryError::NotFound(id))?;
        if !job.status.is_terminal() {
            return Err(RetryError::NotFinished(id));
        }
        let mut request = job.request.clone();
        request.retry_of = Some(id);
        request.disabled_platforms = disabled_platforms;
        request.policy = policy;
        Ok(self.submit(request).await?)
    }

    pub async fn cancel(&self, id: JobId) -> Result<(), CancelError> {
        let token = self
            .shared
            .active
            .lock()
            .expect("active jobs lock")
            .get(&id)
            .map(|active| active.cancel.clone());
        if let Some(token) = token {
            token.cancel();
            return Ok(());
        }
        let mut job = self
            .shared
            .store
            .get(id)
            .await?
            .ok_or(CancelError::NotFound(id))?;
        if job.status.is_terminal() {
            return Err(CancelError::Finished(id));
        }
        job.status = JobStatus::Cancelled;
        job.updated_at = Timestamp::now();
        job.finished_at = Some(job.updated_at);
        self.shared.store.update(&job).await?;
        let _ = self.shared.events.send(EngineEvent {
            job: id,
            at: Timestamp::now(),
            kind: EventKind::Status {
                status: JobStatus::Cancelled,
            },
        });
        Ok(())
    }

    /// Ends a running live capture, keeping what was recorded: the recording as it
    /// stands becomes the source and the job goes on to make and post its output.
    pub async fn stop(&self, id: JobId) -> Result<(), StopError> {
        let token = self
            .shared
            .active
            .lock()
            .expect("active jobs lock")
            .get(&id)
            .map(|active| active.stop.clone());
        let job = self
            .shared
            .store
            .get(id)
            .await?
            .ok_or(StopError::NotFound(id))?;
        let capturing = job.status
            == (JobStatus::Running {
                stage: Stage::Download,
            })
            && job.artifacts.recording.is_some();
        let Some(token) = token.filter(|_| capturing) else {
            return Err(StopError::NotCapturing(id));
        };
        if !token.is_cancelled() {
            token.cancel();
            let _ = self.shared.events.send(EngineEvent {
                job: id,
                at: Timestamp::now(),
                kind: EventKind::Stop,
            });
        }
        Ok(())
    }

    /// Removes a finished job's record and whatever it left in the cache.
    pub async fn delete(&self, id: JobId) -> Result<(), DeleteError> {
        let job = self
            .shared
            .store
            .get(id)
            .await?
            .ok_or(DeleteError::NotFound(id))?;
        if !job.status.is_terminal() {
            return Err(DeleteError::NotFinished(id));
        }
        self.shared.store.delete(id).await?;
        let dir = self.shared.cache_dir().join("jobs").join(id.to_string());
        if let Err(error) = tokio::fs::remove_dir_all(&dir).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(job = %id, "could not remove {}: {error}", dir.display());
        }
        let _ = self.shared.events.send(EngineEvent {
            job: id,
            at: Timestamp::now(),
            kind: EventKind::Deleted,
        });
        Ok(())
    }

    /// Removes finished jobs older than `before` of the given kinds. How many went.
    pub async fn purge(
        &self,
        before: Timestamp,
        kinds: &[StatusKind],
    ) -> Result<usize, StoreError> {
        let ids = self.shared.store.purge(before, kinds).await?;
        let jobs_dir = self.shared.cache_dir().join("jobs");
        for id in &ids {
            let dir = jobs_dir.join(id.to_string());
            let _ = tokio::fs::remove_dir_all(&dir).await;
            let _ = self.shared.events.send(EngineEvent {
                job: *id,
                at: Timestamp::now(),
                kind: EventKind::Deleted,
            });
        }
        Ok(ids.len())
    }

    /// Removes cached job directories of jobs that are not running, oldest first, until the
    /// cache is under `max_bytes`. How many bytes were freed.
    pub async fn trim_cache(&self, max_bytes: u64) -> Result<u64, std::io::Error> {
        let jobs_dir = self.shared.cache_dir().join("jobs");
        let active: HashSet<String> = self.active().iter().map(|id| id.to_string()).collect();
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total = 0u64;
        let mut dir = match tokio::fs::read_dir(&jobs_dir).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            let size = dir_size(&path).await;
            total += size;
            let name = entry.file_name().to_string_lossy().into_owned();
            if active.contains(&name) {
                continue;
            }
            let modified = entry
                .metadata()
                .await
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            entries.push((path, size, modified));
        }
        entries.sort_by_key(|(_, _, modified)| *modified);
        let mut freed = 0u64;
        for (path, size, _) in entries {
            if total <= max_bytes {
                break;
            }
            if tokio::fs::remove_dir_all(&path).await.is_ok() {
                total = total.saturating_sub(size);
                freed += size;
            }
        }
        Ok(freed)
    }

    pub async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        self.shared.store.get(id).await
    }

    /// The job without its resolved variants, enough to list, show and serve it
    pub async fn get_without_variants(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        self.shared.store.get_without_variants(id).await
    }

    /// The still on disk for the job, else one made now from a finished output and kept
    pub async fn thumbnail(&self, job: &Job) -> Result<Option<LocalFile>, ThumbnailError> {
        let id = job.id;
        let archived = job.artifacts.archived.as_ref();
        if let Some(thumbnail) = &job.artifacts.thumbnail {
            for path in
                std::iter::once(&thumbnail.path).chain(archived.and_then(|a| a.thumbnail.as_ref()))
            {
                if let Ok(meta) = tokio::fs::metadata(path).await
                    && meta.is_file()
                {
                    return Ok(Some(LocalFile {
                        path: path.clone(),
                        size: meta.len(),
                        info: thumbnail.info.clone(),
                    }));
                }
            }
        }
        // A job under way is the pipeline's to write; its still arrives with its output.
        if !job.status.is_terminal() {
            return Ok(None);
        }
        let Some(output) = job.artifacts.output.clone() else {
            return Ok(None);
        };
        let mut source = None;
        for path in std::iter::once(&output.path).chain(archived.and_then(|a| a.output.as_ref())) {
            if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()) {
                source = Some(LocalFile {
                    path: path.clone(),
                    size: output.size,
                    info: output.info.clone(),
                });
                break;
            }
        }
        let Some(source) = source else {
            return Ok(None);
        };
        let dir = self.shared.cache_dir().join("jobs").join(id.to_string());
        tokio::fs::create_dir_all(&dir).await?;
        let made = match self
            .shared
            .transcoder
            .thumbnail(&source, &dir.join("poster.jpg"))
            .await
        {
            Ok(file) => file,
            Err(TranscodeError::NoPicture | TranscodeError::NotMedia) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut stored = self
            .shared
            .store
            .get(id)
            .await?
            .ok_or(ThumbnailError::NotFound(id))?;
        stored.artifacts.thumbnail = Some(made.clone());
        self.shared.store.update(&stored).await?;
        Ok(Some(made))
    }

    pub async fn list(&self, filter: &JobFilter) -> Result<Vec<Job>, StoreError> {
        self.shared.store.list(filter).await
    }

    pub async fn count(&self, filter: &JobFilter) -> Result<u64, StoreError> {
        self.shared.store.count(filter).await
    }

    pub async fn children(&self, parent: JobId) -> Result<Vec<Job>, StoreError> {
        self.shared.store.children(parent).await
    }

    pub async fn stats(&self) -> Result<Stats, StoreError> {
        self.shared.store.stats().await
    }

    pub async fn stats_since(&self, since: Timestamp) -> Result<Stats, StoreError> {
        self.shared.store.stats_since(since).await
    }

    /// The resolvers of the jobs a filter admits, each once
    pub async fn job_resolvers(&self, filter: &JobFilter) -> Result<Vec<String>, StoreError> {
        self.shared.store.resolvers(filter).await
    }

    pub async fn resolver_stats(&self) -> Result<Vec<ResolverStats>, StoreError> {
        self.shared.store.resolver_stats().await
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.shared.cache_dir()
    }
}

/// The bytes under `path`, following no links.
pub async fn dir_size(path: &Path) -> u64 {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        fn walk(path: &Path) -> u64 {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return 0;
            };
            if meta.is_file() {
                return meta.len();
            }
            if !meta.is_dir() {
                return 0;
            }
            std::fs::read_dir(path)
                .map(|entries| entries.flatten().map(|e| walk(&e.path())).sum())
                .unwrap_or(0)
        }
        walk(&path)
    })
    .await
    .unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("no resolver handles {0}")]
    Unsupported(Url),
    #[error("{platform} links are turned off here: {url}")]
    Disabled { url: Url, platform: &'static str },
    #[error("no publisher registered for source {0}")]
    UnknownSource(SourceId),
    #[error("engine is not running")]
    Closed,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("job {0} has already finished")]
    Finished(JobId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("job {0} is not capturing a live stream")]
    NotCapturing(JobId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("Cancel running job {0} before continuing.")]
    NotFinished(JobId),
    #[error(transparent)]
    Submit(#[from] SubmitError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("Cancel running job {0} before continuing.")]
    NotFinished(JobId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine configuration incomplete: {0}")]
    Incomplete(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod shutdown_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::config::ShutdownConfig;
    use crate::download::{DownloadContext, DownloadError, Downloaded, Downloader};
    use crate::event::ProgressSender;
    use crate::http::HttpConfig;
    use crate::job::{Origin, Stage};
    use crate::media::{LocalFile, MediaInfo, MediaKind};
    use crate::publish::{Constraints, PublishError, Published};
    use crate::resolve::{Platform, ResolveError, Resolved, SessionSupport, Variant, VariantKind};
    use crate::store::sqlite::SqliteStore;
    use crate::transcode::{Target, TranscodeError, Transcoded};

    const HOST: &str = "stub.test";

    /// Resolves every link on the stub host to one file.
    struct Stub;

    #[async_trait]
    impl Resolver for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }

        fn platform(&self) -> Platform {
            Platform {
                id: "stub",
                name: "Stub",
                hosts: &[HOST],
                features: &[],
                formats: &["bin"],
                media: &[MediaKind::File],
                tags: &[],
                session: SessionSupport::None,
                on_by_default: true,
                examples: &[],
            }
        }

        fn matches(&self, url: &Url) -> bool {
            url.host_str() == Some(HOST)
        }

        async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
            let mut resolved = Resolved::of("stub", MediaKind::File);
            resolved.title = Some("a file".into());
            resolved.variants.push(Variant::file(url.clone()));
            Ok(Resolution::Media(Box::new(resolved)))
        }
    }

    /// Writes a small file after a while, as a download would.
    struct SlowDownload(Duration);

    #[async_trait]
    impl Downloader for SlowDownload {
        fn handles(&self, kind: VariantKind) -> bool {
            kind == VariantKind::File
        }

        async fn download(
            &self,
            _: &Variant,
            dir: &Path,
            _: &DownloadContext,
            _: ProgressSender,
        ) -> Result<Downloaded, DownloadError> {
            tokio::time::sleep(self.0).await;
            let path = dir.join("source.bin");
            tokio::fs::write(&path, b"payload").await?;
            Ok(Downloaded::file(LocalFile::from_path(path).await?))
        }
    }

    struct NoTranscode;

    #[async_trait]
    impl Transcoder for NoTranscode {
        async fn probe(&self, _: &Path) -> Result<MediaInfo, TranscodeError> {
            unreachable!("a file is never probed")
        }

        async fn transcode(
            &self,
            _: &LocalFile,
            _: &Target,
            _: &Path,
            _: ProgressSender,
        ) -> Result<Transcoded, TranscodeError> {
            unreachable!("a file that fits is never converted")
        }

        async fn thumbnail(&self, _: &LocalFile, _: &Path) -> Result<LocalFile, TranscodeError> {
            Err(TranscodeError::NotMedia)
        }
    }

    /// Takes a while to publish, and counts what it published.
    struct SlowPublish {
        source: SourceId,
        delay: Duration,
        published: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Publisher for SlowPublish {
        fn source(&self) -> &SourceId {
            &self.source
        }

        async fn constraints(&self, _: &Job) -> Result<Constraints, PublishError> {
            Ok(Constraints::universal(1 << 30))
        }

        async fn publish(&self, _: &Job, _: &LocalFile) -> Result<Published, PublishError> {
            tokio::time::sleep(self.delay).await;
            self.published.fetch_add(1, Ordering::SeqCst);
            Ok(Published {
                reference: "posted".into(),
                url: None,
                at: jiff::Timestamp::now(),
                notes: Vec::new(),
            })
        }
    }

    struct Harness {
        handle: EngineHandle,
        published: Arc<AtomicUsize>,
        shutdown: CancellationToken,
        running: tokio::task::JoinHandle<Result<(), EngineError>>,
        dir: PathBuf,
    }

    async fn harness(download: Duration, publish: Duration, grace_secs: u64) -> Harness {
        let dir = std::env::temp_dir().join(format!("discoclip-shutdown-{}", uuid::Uuid::now_v7()));
        let config = EngineConfig {
            cache_dir: dir.join("cache"),
            workers: 2,
            shutdown: ShutdownConfig { grace_secs },
            ..EngineConfig::default()
        };
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let published = Arc::new(AtomicUsize::new(0));
        let engine = Engine::builder(config, store, Http::new(HttpConfig::default()))
            .resolver(Stub)
            .downloader(SlowDownload(download))
            .transcoder(NoTranscode)
            .publisher(SlowPublish {
                source: SourceId::new("test"),
                delay: publish,
                published: published.clone(),
            })
            .build()
            .unwrap();
        let handle = engine.handle();
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(engine.run(shutdown.clone()));
        Harness {
            handle,
            published,
            shutdown,
            running,
            dir,
        }
    }

    fn request() -> Request {
        Request::new(
            Origin {
                source: SourceId::new("test"),
                reference: "r".into(),
                url: None,
                guild: None,
                channel: None,
            },
            Url::parse("https://stub.test/file.bin").unwrap(),
        )
    }

    async fn wait_for_stage(handle: &EngineHandle, id: JobId, wanted: Stage) {
        let began = Instant::now();
        loop {
            let job = handle.get(id).await.unwrap().unwrap();
            if job.status == (JobStatus::Running { stage: wanted }) {
                return;
            }
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "job never reached {wanted}: {:?}",
                job.status
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn jobs_under_way_finish_within_the_grace_period() {
        let h = harness(Duration::from_millis(400), Duration::from_millis(50), 10).await;
        let id = h.handle.submit(request()).await.unwrap();
        wait_for_stage(&h.handle, id, Stage::Download).await;
        h.shutdown.cancel();
        let began = Instant::now();
        h.running.await.unwrap().unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        let job = h.handle.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
        assert_eq!(h.published.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(h.dir);
    }

    #[tokio::test]
    async fn jobs_past_the_grace_period_are_queued_again() {
        let h = harness(Duration::from_secs(30), Duration::from_millis(10), 1).await;
        let id = h.handle.submit(request()).await.unwrap();
        wait_for_stage(&h.handle, id, Stage::Download).await;
        h.shutdown.cancel();
        let began = Instant::now();
        h.running.await.unwrap().unwrap();
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_secs(1) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        let job = h.handle.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Queued, "{:?}", job.log);
        assert_eq!(h.published.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(h.dir);
    }

    #[tokio::test]
    async fn publishing_runs_to_its_end_past_the_grace_period() {
        let h = harness(Duration::from_millis(10), Duration::from_secs(2), 0).await;
        let id = h.handle.submit(request()).await.unwrap();
        wait_for_stage(&h.handle, id, Stage::Publish).await;
        h.shutdown.cancel();
        let began = Instant::now();
        h.running.await.unwrap().unwrap();
        assert!(
            began.elapsed() >= Duration::from_millis(500),
            "{:?}",
            began.elapsed()
        );
        let job = h.handle.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
        assert_eq!(h.published.load(Ordering::SeqCst), 1);
        assert_eq!(job.artifacts.published.unwrap().reference, "posted");
        let _ = std::fs::remove_dir_all(h.dir);
    }

    #[tokio::test]
    async fn a_person_cancels_even_during_publishing() {
        let h = harness(Duration::from_millis(10), Duration::from_secs(30), 10).await;
        let id = h.handle.submit(request()).await.unwrap();
        wait_for_stage(&h.handle, id, Stage::Publish).await;
        h.handle.cancel(id).await.unwrap();
        let began = Instant::now();
        loop {
            let job = h.handle.get(id).await.unwrap().unwrap();
            if job.status == JobStatus::Cancelled {
                break;
            }
            assert!(began.elapsed() < Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(h.published.load(Ordering::SeqCst), 0);
        h.shutdown.cancel();
        h.running.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(h.dir);
    }
}
