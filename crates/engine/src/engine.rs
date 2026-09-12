use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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
use crate::job::{Job, JobId, JobStatus, Request, SourceId, StatusKind};
use crate::pipeline::{self, Context};
use crate::publish::Publisher;
use crate::resolve::{Platform, ResolveError, Resolver, ResolverRegistry, SessionCheck};
use crate::store::{JobFilter, JobStore, ResolverStats, Stats, StoreError};
use crate::transcode::Transcoder;

const EVENT_CAPACITY: usize = 4096;
const QUEUE_CAPACITY: usize = 10_000;

pub struct EngineBuilder {
    config: EngineConfig,
    store: Arc<dyn JobStore>,
    http: Http,
    resolvers: Vec<Box<dyn Resolver>>,
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
        self.resolvers.push(Box::new(resolver));
        self
    }

    pub fn resolver_boxed(mut self, resolver: Box<dyn Resolver>) -> Self {
        self.resolvers.push(resolver);
        self
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

    /// Recovers interrupted jobs, then dispatches queued jobs to workers until shutdown.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), EngineError> {
        let ctx = self.context.clone();
        let shared = self.handle.shared.clone();
        tokio::fs::create_dir_all(ctx.cache_dir().join("jobs")).await?;
        self.recover().await?;

        let workers = shared.workers.load(Ordering::Relaxed);
        let semaphore = shared.semaphore.clone();
        let mut tasks: JoinSet<()> = JoinSet::new();
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
            let token = CancellationToken::new();
            {
                // Recovery and a concurrent submit can both queue the same id; run it once.
                let mut active = shared.active.lock().expect("active jobs lock");
                if active.contains_key(&id) {
                    tracing::debug!(job = %id, "already running; dropping duplicate dispatch");
                    continue;
                }
                active.insert(id, token.clone());
            }
            let job = match ctx.store.get(id).await {
                Ok(Some(job)) if job.status == JobStatus::Queued => job,
                Ok(Some(job)) => {
                    tracing::debug!(job = %id, status = ?job.status, "skipping job that is no longer queued");
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
            let shutdown = shutdown.clone();
            tasks.spawn(async move {
                pipeline::run_job(ctx, job, token, shutdown).await;
                drop(permit);
                shared.active.lock().expect("active jobs lock").remove(&id);
            });
            while let Some(result) = tasks.try_join_next() {
                if let Err(error) = result {
                    tracing::error!("job task panicked: {error}");
                }
            }
        }

        tracing::info!(in_flight = tasks.len(), "engine stopping");
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!("job task panicked: {error}");
            }
        }
        Ok(())
    }

    /// Requeues the jobs that were running or waiting when the engine last stopped, and
    /// removes cache directories of jobs the store no longer has; a finished job's
    /// directory stays, since it holds the output the web app serves, until retention
    /// takes it.
    async fn recover(&self) -> Result<(), EngineError> {
        let ctx = &self.context;
        let active = ctx.store.list_active().await?;
        for mut job in active {
            if matches!(job.status, JobStatus::Running { .. }) {
                ctx.note(&mut job, None, "requeued after engine restart")
                    .await?;
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
                    job.status == JobStatus::Done && !pipeline::kept_files(&job).is_empty()
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

struct Shared {
    resolvers: Arc<ResolverRegistry>,
    store: Arc<dyn JobStore>,
    http: Http,
    sources: HashSet<SourceId>,
    submit: mpsc::Sender<JobId>,
    events: broadcast::Sender<EngineEvent>,
    active: Mutex<HashMap<JobId, CancellationToken>>,
    /// How many jobs may run at once; the semaphore holds that many permits.
    workers: AtomicUsize,
    semaphore: Arc<Semaphore>,
    queued: AtomicUsize,
    config: Arc<RwLock<EngineConfig>>,
    archiver: Option<Arc<dyn Archiver>>,
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

    /// Grows or shrinks the worker pool to `wanted`. Growing frees permits at once;
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

    /// The resolver that would take `url`.
    pub fn resolver_for(&self, url: &Url) -> Option<&'static str> {
        self.shared.resolvers.find(url).map(|r| r.id())
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

    /// Queues a fresh job with the same request as a finished one.
    pub async fn retry(&self, id: JobId) -> Result<JobId, RetryError> {
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
        Ok(self.submit(request).await?)
    }

    pub async fn cancel(&self, id: JobId) -> Result<(), CancelError> {
        let token = self
            .shared
            .active
            .lock()
            .expect("active jobs lock")
            .get(&id)
            .cloned();
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

    /// Removes finished jobs older than `before` of the given kinds; how many went.
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
    /// cache is under `max_bytes`; how many bytes were freed.
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
pub enum RetryError {
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("job {0} is still running; cancel it first")]
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
    #[error("job {0} is still running; cancel it first")]
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
