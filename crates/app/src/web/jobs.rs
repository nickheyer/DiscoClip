//! Jobs as the web app sees them: listed and filtered, one in full with its stage log and
//! artifacts, counted, and streamed live as the engine works through them.

use std::convert::Infallible;
use std::time::Duration;

use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use discoclip_engine::job::{
    Job, JobId, JobStatus, Origin, Request, RequestLimits, RequestOptions, SourceId,
};
use discoclip_engine::media::safe_stem;
use discoclip_engine::{
    EngineEvent, EventKind, JobFilter, Order, ResolverStats, Stats, StatusKind, Utilisation,
};
use futures::{Stream, StreamExt};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_util::io::ReaderStream;
use url::Url;

use super::AppState;
use super::auth::{Auth, Identity, parse_id};
use super::error::ApiError;
use crate::local::SOURCE_ID as LOCAL_SOURCE;
use crate::users::Permission;

/// The most jobs one bulk request acts on.
const BULK_MAX: usize = 500;

/// How often the live feed restates the counts and the workers' load.
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// A job as a listing shows it: what was asked, where it stands, and what came of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobSummary {
    pub id: JobId,
    pub url: Url,
    pub status: JobStatus,
    pub source: SourceId,
    pub origin: Origin,
    pub destination: Option<String>,
    pub submitted_by: Option<String>,
    pub parent: Option<JobId>,
    pub retry_of: Option<JobId>,
    pub title: Option<String>,
    pub resolver: Option<String>,
    pub uploader: Option<String>,
    pub webpage_url: Option<Url>,
    pub thumbnail: Option<Url>,
    /// Seconds of media, when the resolver said.
    pub duration_secs: Option<f64>,
    pub live: bool,
    pub output_bytes: Option<u64>,
    pub published_url: Option<Url>,
    pub published_reference: Option<String>,
    pub children: usize,
    pub archived_files: usize,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

impl JobSummary {
    pub fn of(job: &Job) -> Self {
        let resolved = job.artifacts.resolved.as_ref();
        Self {
            id: job.id,
            url: job.request.url.clone(),
            status: job.status.clone(),
            source: job.request.origin.source.clone(),
            origin: job.request.origin.clone(),
            destination: job.request.destination.clone(),
            submitted_by: job.request.submitted_by.clone(),
            parent: job.request.parent,
            retry_of: job.request.retry_of,
            title: resolved.and_then(|r| r.title.clone()),
            resolver: resolved.map(|r| r.resolver.clone()),
            uploader: resolved.and_then(|r| r.uploader.clone()),
            webpage_url: resolved.and_then(|r| r.webpage_url.clone()),
            thumbnail: resolved.and_then(|r| r.thumbnail.clone()),
            duration_secs: resolved
                .and_then(|r| r.duration)
                .map(|d| d.as_secs_f64()),
            live: resolved.is_some_and(|r| r.live),
            output_bytes: job.artifacts.output.as_ref().map(|f| f.size),
            published_url: job.artifacts.published.as_ref().and_then(|p| p.url.clone()),
            published_reference: job.artifacts.published.as_ref().map(|p| p.reference.clone()),
            children: job.artifacts.children.len(),
            archived_files: job
                .artifacts
                .archived
                .as_ref()
                .map_or(0, |a| a.files.len()),
            created_at: job.created_at,
            updated_at: job.updated_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
        }
    }
}

/// The listing filters, as the query names them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ListQuery {
    pub source: Option<String>,
    pub status: Option<String>,
    pub resolver: Option<String>,
    pub parent: Option<String>,
    /// Leave out jobs expanded from playlists.
    pub top_level: Option<bool>,
    /// A substring of the link, the title, the submitter or the id.
    pub q: Option<String>,
    /// RFC 3339 timestamps.
    pub before: Option<String>,
    pub after: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// `newest` or `oldest`.
    pub order: Option<String>,
}

fn parse<T>(name: &str, value: Option<String>) -> Result<Option<T>, ApiError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .map(|text| {
            text.parse()
                .map_err(|e| ApiError::BadRequest(format!("{name}: {e}")))
        })
        .transpose()
}

impl ListQuery {
    pub fn filter(self) -> Result<JobFilter, ApiError> {
        let order = match self.order.as_deref() {
            None | Some("newest") => Order::Newest,
            Some("oldest") => Order::Oldest,
            Some(other) => {
                return Err(ApiError::BadRequest(format!(
                    "order: {other:?} is neither newest nor oldest"
                )));
            }
        };
        Ok(JobFilter {
            source: self.source.map(SourceId::new),
            status: parse::<StatusKind>("status", self.status)?,
            before: parse::<Timestamp>("before", self.before)?,
            after: parse::<Timestamp>("after", self.after)?,
            limit: self.limit,
            offset: self.offset,
            q: self.q,
            resolver: self.resolver,
            parent: parse::<JobId>("parent", self.parent)?,
            top_level: self.top_level.unwrap_or(false),
            order,
        })
    }
}

/// One page of jobs and how many match in all.
#[derive(Debug, Serialize)]
pub struct JobPage {
    pub jobs: Vec<JobSummary>,
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
}

pub async fn list(
    State(state): State<AppState>,
    Auth(_): Auth,
    Query(query): Query<ListQuery>,
) -> Result<Json<JobPage>, ApiError> {
    let filter = query.filter()?;
    let limit = filter.effective_limit();
    let offset = filter.offset.unwrap_or(0);
    let jobs = state.engine.list(&filter).await?;
    let total = state.engine.count(&filter).await?;
    Ok(Json(JobPage {
        jobs: jobs.iter().map(JobSummary::of).collect(),
        total,
        limit,
        offset,
    }))
}

/// One job in full: the request, every stage's timing, the log, and every artifact.
pub async fn get(
    State(state): State<AppState>,
    Auth(_): Auth,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id: JobId = parse_id(&id)?;
    Ok(Json(state.engine.get(id).await?.ok_or(ApiError::NotFound)?))
}

/// The jobs a playlist job expanded into, oldest first.
pub async fn children(
    State(state): State<AppState>,
    Auth(_): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<JobSummary>>, ApiError> {
    let id: JobId = parse_id(&id)?;
    state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    let children = state.engine.children(id).await?;
    Ok(Json(children.iter().map(JobSummary::of).collect()))
}

/// How the engine is doing: counts over every job and over the last day, the workers'
/// load, the jobs running now, and each resolver's record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobStats {
    pub counts: Stats,
    pub last_24h: Stats,
    pub utilisation: Utilisation,
    /// The queue as a whole: jobs waiting for a worker and jobs on one.
    pub queue_depth: u64,
    pub active: Vec<JobId>,
    pub resolvers: Vec<ResolverStats>,
    pub at: Timestamp,
}

pub async fn read_stats(state: &AppState) -> Result<JobStats, ApiError> {
    let now = Timestamp::now();
    let counts = state.engine.stats().await?;
    let last_24h = state
        .engine
        .stats_since(now - SignedDuration::from_hours(24))
        .await?;
    let utilisation = state.engine.utilisation();
    Ok(JobStats {
        queue_depth: counts.queued + counts.running,
        counts,
        last_24h,
        utilisation,
        active: state.engine.active(),
        resolvers: state.engine.resolver_stats().await?,
        at: now,
    })
}

pub async fn stats(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Result<Json<JobStats>, ApiError> {
    Ok(Json(read_stats(&state).await?))
}

/// What the live feed carries about one job event, with the summary of the job as it
/// stands after it, so a listing can show a job it has not seen before.
#[derive(Debug, Serialize)]
pub struct FeedEvent {
    #[serde(flatten)]
    pub event: EngineEvent,
    pub job_summary: Option<JobSummary>,
}

fn stats_event(stats: &JobStats) -> Event {
    Event::default()
        .event("stats")
        .json_data(stats)
        .expect("job stats serialize")
}

/// Whether an event changes what a listing shows of the job, so its summary is sent along.
fn carries_summary(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Submitted { .. } | EventKind::Status { .. } | EventKind::Children { .. }
    )
}

/// The engine's counts and load now, then every job event as it happens, with the counts
/// restated every couple of seconds, as server-sent `stats` and `job` events. A `job`
/// event carries the job's summary whenever its status changed.
pub async fn events(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let first = stats_event(&read_stats(&state).await?);
    let live = BroadcastStream::new(state.engine.subscribe());
    let engine = state.engine.clone();
    let jobs = live.filter_map(move |item| {
        let engine = engine.clone();
        async move {
            match item {
                Ok(event) => {
                    let job_summary = if carries_summary(&event.kind) {
                        engine.get(event.job).await.ok().flatten().as_ref().map(JobSummary::of)
                    } else {
                        None
                    };
                    let feed = FeedEvent { event, job_summary };
                    Some(Ok(Event::default()
                        .event("job")
                        .json_data(&feed)
                        .expect("job events serialize")))
                }
                // A slow reader missed some events; the next stats event catches it up.
                Err(BroadcastStreamRecvError::Lagged(_)) => None,
            }
        }
    });
    let ticker_state = state.clone();
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let ticks = IntervalStream::new(ticker).skip(1).filter_map(move |_| {
        let state = ticker_state.clone();
        async move {
            match read_stats(&state).await {
                Ok(stats) => Some(Ok(stats_event(&stats))),
                Err(error) => {
                    tracing::warn!("job stats not read for the live feed: {error:?}");
                    None
                }
            }
        }
    });
    let stream = futures::stream::once(async move { Ok(first) })
        .chain(tokio_stream::StreamExt::merge(jobs, ticks));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// A link submitted from the web app, published to the local directory.
#[derive(Debug, Deserialize)]
pub struct SubmitRequest {
    pub url: Url,
    #[serde(default)]
    pub limits: RequestLimits,
    #[serde(default)]
    pub options: RequestOptions,
}

#[derive(Debug, Serialize)]
pub struct Submitted {
    pub id: JobId,
}

fn local_origin(identity: &Identity) -> Origin {
    Origin {
        source: SourceId::new(LOCAL_SOURCE),
        reference: identity.user.username.clone(),
        url: None,
    }
}

/// Queues a link on behalf of the account, as the local source.
pub async fn submit(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(request): Json<SubmitRequest>,
) -> Result<(StatusCode, Json<Submitted>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    if !matches!(request.url.scheme(), "http" | "https") || request.url.host_str().is_none() {
        return Err(ApiError::BadRequest(format!(
            "{} is not an http(s) link",
            request.url
        )));
    }
    let mut job = Request::new(local_origin(&identity), request.url.clone());
    job.limits = request.limits;
    job.options = request.options;
    job.submitted_by = Some(identity.user.username.clone());
    let id = state.engine.submit(job).await?;
    tracing::info!(by = identity.user.username, job = %id, url = %request.url, "link submitted");
    Ok((StatusCode::ACCEPTED, Json(Submitted { id })))
}

/// Queues a fresh job with the same request as a finished one.
pub async fn retry(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Submitted>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    let id: JobId = parse_id(&id)?;
    let new_id = state.engine.retry(id).await?;
    tracing::info!(by = identity.user.username, job = %id, retry = %new_id, "job retried");
    Ok((StatusCode::ACCEPTED, Json(Submitted { id: new_id })))
}

/// Stops a queued or running job.
pub async fn cancel(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageJobs)?;
    let id: JobId = parse_id(&id)?;
    state.engine.cancel(id).await?;
    tracing::info!(by = identity.user.username, job = %id, "job cancelled");
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a finished job's record and whatever it left in the cache.
pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageJobs)?;
    let id: JobId = parse_id(&id)?;
    state.engine.delete(id).await?;
    tracing::info!(by = identity.user.username, job = %id, "job deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkAction {
    Retry,
    Cancel,
    Delete,
}

#[derive(Debug, Deserialize)]
pub struct BulkRequest {
    pub action: BulkAction,
    pub ids: Vec<JobId>,
}

/// How one job fared in a bulk request.
#[derive(Debug, Serialize)]
pub struct BulkOutcome {
    pub id: JobId,
    pub ok: bool,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// The job queued in its place, for a retry.
    pub job: Option<JobId>,
}

#[derive(Debug, Serialize)]
pub struct BulkResponse {
    pub action: BulkAction,
    pub results: Vec<BulkOutcome>,
    pub succeeded: usize,
    pub failed: usize,
}

/// Retries, cancels or deletes several jobs; each is reported on its own.
pub async fn bulk(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(request): Json<BulkRequest>,
) -> Result<Json<BulkResponse>, ApiError> {
    identity.require(Permission::ManageJobs)?;
    if request.ids.is_empty() {
        return Err(ApiError::BadRequest("ids is empty".into()));
    }
    if request.ids.len() > BULK_MAX {
        return Err(ApiError::BadRequest(format!(
            "at most {BULK_MAX} jobs per request"
        )));
    }
    let mut results = Vec::with_capacity(request.ids.len());
    for id in request.ids {
        let outcome: Result<Option<JobId>, String> = match request.action {
            BulkAction::Retry => state
                .engine
                .retry(id)
                .await
                .map(Some)
                .map_err(|e| e.to_string()),
            BulkAction::Cancel => state
                .engine
                .cancel(id)
                .await
                .map(|()| None)
                .map_err(|e| e.to_string()),
            BulkAction::Delete => state
                .engine
                .delete(id)
                .await
                .map(|()| None)
                .map_err(|e| e.to_string()),
        };
        results.push(match outcome {
            Ok(job) => BulkOutcome {
                id,
                ok: true,
                error: None,
                job,
            },
            Err(error) => BulkOutcome {
                id,
                ok: false,
                error: Some(error),
                job: None,
            },
        });
    }
    let succeeded = results.iter().filter(|r| r.ok).count();
    let failed = results.len() - succeeded;
    tracing::info!(by = identity.user.username, action = ?request.action, succeeded, failed, "bulk job action");
    Ok(Json(BulkResponse {
        action: request.action,
        results,
        succeeded,
        failed,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Artifact {
    Output,
    Source,
    Subtitle,
}

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    #[serde(default = "output")]
    pub artifact: Artifact,
    /// Which subtitle track, for `subtitle`.
    #[serde(default)]
    pub index: usize,
    /// Show in the browser rather than save.
    #[serde(default)]
    pub inline: bool,
}

fn output() -> Artifact {
    Artifact::Output
}

fn content_type_for(path: &FsPath) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "flv" => "video/x-flv",
        "avi" => "video/x-msvideo",
        "gif" => "image/gif",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "vtt" => "text/vtt; charset=utf-8",
        "srt" => "application/x-subrip; charset=utf-8",
        "ass" | "ssa" => "text/x-ssa; charset=utf-8",
        "ttml" => "application/ttml+xml; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The first existing path among the cached artifact and its archived copy.
async fn first_present(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    for path in candidates {
        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()) {
            return Some(path);
        }
    }
    None
}

/// Where the artifact's bytes are, and the name to offer them under.
async fn locate(job: &Job, query: &DownloadQuery) -> Result<(PathBuf, String), ApiError> {
    let id = job.id.to_string();
    let stem = format!("{}-{}", safe_stem(job.title(), "video"), &id[..8]);
    let archived = job.artifacts.archived.as_ref();
    let (candidates, suffix): (Vec<PathBuf>, &str) = match query.artifact {
        Artifact::Output => (
            job.artifacts
                .output
                .iter()
                .map(|f| f.path.clone())
                .chain(archived.and_then(|a| a.output.clone()))
                .collect(),
            "",
        ),
        Artifact::Source => (
            job.artifacts
                .source
                .iter()
                .map(|f| f.path.clone())
                .chain(archived.and_then(|a| a.source.clone()))
                .collect(),
            "-source",
        ),
        Artifact::Subtitle => {
            let track = job
                .artifacts
                .subtitles
                .get(query.index)
                .ok_or(ApiError::NotFound)?;
            let language = track
                .language
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(32)
                .collect::<String>();
            let path = first_present(vec![track.path.clone()]).await.ok_or_else(|| {
                ApiError::Conflict("the subtitle file is no longer in the cache".into())
            })?;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("vtt")
                .to_string();
            return Ok((path, format!("{stem}.{language}.{ext}")));
        }
    };
    if candidates.is_empty() {
        return Err(ApiError::NotFound);
    }
    let path = first_present(candidates).await.ok_or_else(|| {
        ApiError::Conflict(
            "the file is no longer in the cache or the archive; retention removed it".into(),
        )
    })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_string();
    Ok((path, format!("{stem}{suffix}.{ext}")))
}

/// A `Range: bytes=` header as the one span it asks for, within `len`.
fn byte_range(headers: &HeaderMap, len: u64) -> Result<Option<(u64, u64)>, ApiError> {
    let Some(value) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let unsatisfiable = || ApiError::RangeNotSatisfiable(len);
    let spec = value
        .strip_prefix("bytes=")
        .ok_or_else(unsatisfiable)?
        .trim();
    if spec.contains(',') {
        return Err(unsatisfiable());
    }
    let (start, end) = spec.split_once('-').ok_or_else(unsatisfiable)?;
    let range = if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| unsatisfiable())?;
        if suffix == 0 || len == 0 {
            return Err(unsatisfiable());
        }
        (len.saturating_sub(suffix), len - 1)
    } else {
        let start: u64 = start.parse().map_err(|_| unsatisfiable())?;
        let end: u64 = if end.is_empty() {
            len.saturating_sub(1)
        } else {
            end.parse().map_err(|_| unsatisfiable())?
        };
        if start >= len || end < start {
            return Err(unsatisfiable());
        }
        (start, end.min(len - 1))
    };
    Ok(Some(range))
}

/// Streams a file, whole or the range asked for, with the headers a browser or a video
/// element needs to save or play it.
async fn serve_file(
    path: &FsPath,
    filename: &str,
    inline: bool,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::Internal(format!("opening {}: {e}", path.display())))?;
    let len = file
        .metadata()
        .await
        .map_err(|e| ApiError::Internal(format!("reading {}: {e}", path.display())))?
        .len();
    let disposition = format!(
        "{}; filename=\"{}\"",
        if inline { "inline" } else { "attachment" },
        filename.replace('"', "")
    );
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, content_type_for(path))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&disposition)
                .map_err(|e| ApiError::Internal(format!("content disposition: {e}")))?,
        );
    let body = match byte_range(headers, len)? {
        Some((start, end)) => {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| ApiError::Internal(format!("seeking {}: {e}", path.display())))?;
            let span = end - start + 1;
            response = response
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, span);
            Body::from_stream(ReaderStream::new(file.take(span)))
        }
        None => {
            response = response
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, len);
            Body::from_stream(ReaderStream::new(file))
        }
    };
    response
        .body(body)
        .map_err(|e| ApiError::Internal(format!("building response: {e}")))
}

/// The job's output, source or a subtitle file, from the cache or the archive.
pub async fn download(
    State(state): State<AppState>,
    Auth(_): Auth,
    Path(id): Path<String>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: JobId = parse_id(&id)?;
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    let (path, filename) = locate(&job, &query).await?;
    serve_file(&path, &filename, query.inline, &headers).await
}

impl IntoResponse for Submitted {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use discoclip_engine::job::{JobStatus, Origin, Request, SourceId};
    use discoclip_engine::media::LocalFile;
    use discoclip_engine::{JobStore, Stage};
    use serde_json::json;
    use url::Url;

    use super::byte_range;
    use crate::web::testing::{Client, SUPPORTED_HOST, app_with_admin_db};

    #[tokio::test]
    async fn links_are_submitted_retried_cancelled_and_deleted() {
        let (app, db) = app_with_admin_db().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;

        let (status, body) = admin
            .post("/api/jobs", json!({"url": "ftp://video.test/clip"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = admin
            .post("/api/jobs", json!({"url": "https://nothing-handles.test/clip"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("no resolver handles"));
        let (status, body) = admin
            .post(
                "/api/jobs",
                json!({
                    "url": format!("https://{SUPPORTED_HOST}/clip"),
                    "limits": {"max_height": 720},
                    "options": {"subtitles": "burn", "clip": {"start": {"secs": 5, "nanos": 0}, "end": null}}
                }),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (_, job) = admin.get(&format!("/api/jobs/{id}")).await;
        assert_eq!(job["request"]["origin"]["source"], "local");
        assert_eq!(job["request"]["origin"]["reference"], "nick");
        assert_eq!(job["request"]["submitted_by"], "nick");
        assert_eq!(job["request"]["limits"]["max_height"], 720);
        assert_eq!(job["request"]["options"]["subtitles"], "burn");
        assert_eq!(job["request"]["options"]["clip"]["start"]["secs"], 5);

        // Not finished: no retry, no delete; cancel works and then the others do.
        let (status, _) = admin
            .send(Method::POST, &format!("/api/jobs/{id}/retry"), None)
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = admin.delete(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = admin
            .send(Method::POST, &format!("/api/jobs/{id}/cancel"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, job) = admin.get(&format!("/api/jobs/{id}")).await;
        assert_eq!(job["status"]["status"], "cancelled");
        let (status, _) = admin
            .send(Method::POST, &format!("/api/jobs/{id}/cancel"), None)
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = admin
            .send(Method::POST, &format!("/api/jobs/{id}/retry"), None)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let retry = body["id"].as_str().unwrap().to_string();
        assert_ne!(retry, id);
        let (_, job) = admin.get(&format!("/api/jobs/{retry}")).await;
        assert_eq!(job["request"]["retry_of"], id);
        let (status, _) = admin.delete(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = admin.get(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = admin.delete(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Bulk: one cancel that works, one that does not.
        let (status, body) = admin
            .post(
                "/api/jobs/bulk",
                json!({"action": "cancel", "ids": [retry, "00000000-0000-0000-0000-000000000000"]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["succeeded"], 1);
        assert_eq!(body["failed"], 1);
        assert_eq!(body["results"][0]["ok"], true);
        assert_eq!(body["results"][1]["ok"], false);
        assert!(body["results"][1]["error"].as_str().unwrap().contains("not found"));
        let (status, body) = admin
            .post("/api/jobs/bulk", json!({"action": "retry", "ids": [retry]}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["results"][0]["job"].is_string());
        let (status, _) = admin
            .post("/api/jobs/bulk", json!({"action": "delete", "ids": []}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = admin
            .post("/api/jobs/bulk", json!({"action": "explode", "ids": [retry]}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // Viewers read but do not act.
        app.state
            .users
            .create("viewer", Some("battery staple"), crate::users::Role::Viewer)
            .await
            .unwrap();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let (status, _) = viewer.get("/api/jobs").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = viewer
            .post("/api/jobs", json!({"url": format!("https://{SUPPORTED_HOST}/x")}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = viewer.delete(&format!("/api/jobs/{retry}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = viewer
            .post("/api/jobs/bulk", json!({"action": "cancel", "ids": [retry]}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(db.get(retry.parse().unwrap()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn artifacts_download_whole_and_by_range() {
        let (app, db) = app_with_admin_db().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let dir = std::env::temp_dir().join(format!("discoclip-download-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("output.mp4");
        std::fs::write(&output, b"0123456789").unwrap();
        let subtitle = dir.join("subtitles.en.0.vtt");
        std::fs::write(&subtitle, "WEBVTT\n").unwrap();

        let origin = Origin {
            source: SourceId::new("local"),
            reference: "nick".into(),
            url: None,
        };
        let mut job = discoclip_engine::Job::new(Request::new(
            origin,
            Url::parse(&format!("https://{SUPPORTED_HOST}/clip")).unwrap(),
        ));
        let mut resolved = discoclip_engine::Resolved::new("web");
        resolved.title = Some("My Clip!".into());
        job.artifacts.resolved = Some(resolved);
        job.artifacts.output = Some(LocalFile {
            path: output.clone(),
            size: 10,
            info: None,
        });
        job.artifacts.source = Some(LocalFile {
            path: dir.join("gone.mp4"),
            size: 3,
            info: None,
        });
        job.artifacts.subtitles = vec![discoclip_engine::download::LocalSubtitle {
            language: "en".into(),
            name: None,
            path: subtitle.clone(),
            format: discoclip_engine::resolve::SubtitleFormat::Vtt,
        }];
        job.status = JobStatus::Done;
        db.insert(&job).await.unwrap();
        let id = job.id.to_string();
        let short = &id[..8];

        let (status, headers, body) = admin.raw(&format!("/api/jobs/{id}/download")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"0123456789");
        assert_eq!(headers["content-type"], "video/mp4");
        assert_eq!(headers["accept-ranges"], "bytes");
        assert_eq!(
            headers["content-disposition"],
            format!("attachment; filename=\"my-clip-{short}.mp4\"")
        );
        admin.headers = vec![("range".into(), "bytes=2-5".into())];
        let (status, headers, body) = admin.raw(&format!("/api/jobs/{id}/download?inline=true")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"2345");
        assert_eq!(headers["content-range"], "bytes 2-5/10");
        assert_eq!(headers["content-length"], "4");
        assert!(headers["content-disposition"].to_str().unwrap().starts_with("inline"));
        admin.headers = vec![("range".into(), "bytes=-3".into())];
        let (status, _, body) = admin.raw(&format!("/api/jobs/{id}/download")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"789");
        admin.headers = vec![("range".into(), "bytes=20-".into())];
        let (status, headers, _) = admin.raw(&format!("/api/jobs/{id}/download")).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(headers["content-range"], "bytes */10");
        admin.headers.clear();

        let (status, headers, body) = admin
            .raw(&format!("/api/jobs/{id}/download?artifact=subtitle&index=0"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"WEBVTT\n");
        assert!(headers["content-type"].to_str().unwrap().starts_with("text/vtt"));
        assert_eq!(
            headers["content-disposition"],
            format!("attachment; filename=\"my-clip-{short}.en.vtt\"")
        );
        let (status, _, _) = admin
            .raw(&format!("/api/jobs/{id}/download?artifact=subtitle&index=3"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The source was tidied away after the job finished.
        let (status, _, _) = admin
            .raw(&format!("/api/jobs/{id}/download?artifact=source"))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) = admin
            .raw("/api/jobs/00000000-0000-0000-0000-000000000000/download")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Deleting the job removes its cached directory too.
        let cache_dir = app.state.engine.cache_dir().join("jobs").join(&id);
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("output.mp4"), b"x").unwrap();
        let (status, _) = admin.delete(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!cache_dir.exists());
        std::fs::remove_dir_all(&dir).unwrap();
        let _ = Stage::Resolve;
    }

    #[test]
    fn ranges_are_read_as_browsers_send_them() {
        use axum::http::{HeaderMap, HeaderValue};
        let with = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert("range", HeaderValue::from_str(value).unwrap());
            map
        };
        assert_eq!(byte_range(&HeaderMap::new(), 10).unwrap(), None);
        assert_eq!(byte_range(&with("bytes=0-"), 10).unwrap(), Some((0, 9)));
        assert_eq!(byte_range(&with("bytes=3-100"), 10).unwrap(), Some((3, 9)));
        assert_eq!(byte_range(&with("bytes=-4"), 10).unwrap(), Some((6, 9)));
        assert!(byte_range(&with("bytes=5-2"), 10).is_err());
        assert!(byte_range(&with("bytes=0-1,3-4"), 10).is_err());
        assert!(byte_range(&with("items=0-1"), 10).is_err());
        assert!(byte_range(&with("bytes=0-"), 0).is_err());
    }

    #[tokio::test]
    async fn jobs_are_listed_counted_and_fetched() {
        let (app, _db) = app_with_admin_db().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (status, body) = admin.get("/api/jobs").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], 0);
        assert_eq!(body["jobs"], json!([]));

        let origin = Origin {
            source: SourceId::new("discord"),
            reference: "x".into(),
            url: None,
        };
        let mut request = Request::new(
            origin,
            Url::parse(&format!("https://{SUPPORTED_HOST}/clip")).unwrap(),
        );
        request.submitted_by = Some("discord:9".into());
        let id = app.state.engine.submit(request).await.unwrap();

        let (status, body) = admin.get("/api/jobs?status=queued&q=clip").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], 1);
        assert_eq!(body["jobs"][0]["id"], id.to_string());
        assert_eq!(body["jobs"][0]["status"]["status"], "queued");
        assert_eq!(body["jobs"][0]["submitted_by"], "discord:9");
        assert_eq!(body["jobs"][0]["source"], "discord");
        let (status, body) = admin.get("/api/jobs?status=done").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], 0);
        let (status, _) = admin.get("/api/jobs?status=bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = admin.get("/api/jobs?order=sideways").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = admin.get(&format!("/api/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["request"]["submitted_by"], "discord:9");
        assert_eq!(body["log"], json!([]));
        let (status, body) = admin.get(&format!("/api/jobs/{id}/children")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!([]));
        let (status, _) = admin
            .get("/api/jobs/00000000-0000-0000-0000-000000000000")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = admin.get("/api/jobs/stats").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["counts"]["queued"], 1);
        assert_eq!(body["last_24h"]["queued"], 1);
        assert_eq!(body["queue_depth"], 1);
        assert_eq!(body["utilisation"]["workers"], 2);
        assert_eq!(body["utilisation"]["waiting"], 1);
        assert_eq!(body["active"], json!([]));

        // The feed opens with the stats and carries the job's submission.
        let stream = admin.stream("/api/jobs/events").await;
        assert!(stream.contains("event: stats"), "{stream}");
        assert!(stream.contains("\"queue_depth\":1"), "{stream}");
        app.state
            .engine
            .cancel(id)
            .await
            .unwrap();
        let stream = admin.stream("/api/jobs/events").await;
        assert!(stream.contains("\"queue_depth\":0"), "{stream}");

        let mut anonymous = Client::new(&app);
        let (status, _) = anonymous.get("/api/jobs").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_feed_relays_job_events_with_summaries() {
        let (app, _db) = app_with_admin_db().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let engine = app.state.engine.clone();
        let feed = tokio::spawn({
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            async move { client.stream("/api/jobs/events").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let origin = Origin {
            source: SourceId::new("discord"),
            reference: "x".into(),
            url: None,
        };
        let id = engine
            .submit(Request::new(
                origin,
                Url::parse(&format!("https://{SUPPORTED_HOST}/live")).unwrap(),
            ))
            .await
            .unwrap();
        let text = feed.await.unwrap();
        assert!(text.contains("event: job"), "{text}");
        assert!(text.contains("\"kind\":\"submitted\""), "{text}");
        assert!(text.contains(&format!("\"job\":\"{id}\"")), "{text}");
        assert!(text.contains("\"job_summary\":{"), "{text}");
    }
}
