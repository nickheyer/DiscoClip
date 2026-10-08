//! A link posted before comes back out of the archive: the later job neither resolves nor
//! downloads again, and files already in the archive are not written to it twice.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use discoclip_engine::archive::{ArchiveConfig, FsArchiver, Keep};
use discoclip_engine::download::HttpDownloader;
use discoclip_engine::ffmpeg::Ffmpeg;
use discoclip_engine::http::transport::{
    Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
};
use discoclip_engine::job::{Job, JobStatus, Origin, Request, SourceId};
use discoclip_engine::media::{Container, LocalFile, MediaKind};
use discoclip_engine::policy::DeliveryPolicy;
use discoclip_engine::publish::{
    Constraints, LinkTarget, PublishError, Published, Publisher, QualityFloor,
};
use discoclip_engine::resolve::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::transcode::FfmpegTranscoder;
use discoclip_engine::{Engine, EngineConfig, EngineHandle, Http, JobId};
use tokio_util::sync::CancellationToken;
use url::Url;

const HOST: &str = "files.test";
const SMALL: u64 = 200_000;
const LARGE: u64 = 50_000_000;

/// A host of files that counts how often it is asked about them
struct Counted {
    resolves: Arc<AtomicUsize>,
}

#[async_trait]
impl Resolver for Counted {
    fn id(&self) -> &'static str {
        "files"
    }

    fn platform(&self) -> Platform {
        Platform {
            id: "files",
            name: "Files",
            hosts: &[HOST],
            features: &["files"],
            formats: &["mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        url.host_str() == Some(HOST)
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let name = url.path().trim_start_matches('/').to_string();
        let container = Container::from_name(&name).expect("test files have extensions");
        let mut resolved = Resolved::of("files", container.kind());
        let mut variant = Variant::file(url.clone());
        variant.container = Some(container);
        resolved.title = Some(name);
        resolved.variants = vec![variant];
        Ok(resolved.into())
    }
}

/// A destination that takes little from `small` origins and much from the rest
struct Memory {
    source: SourceId,
    published: Arc<Mutex<Vec<PathBuf>>>,
}

#[async_trait]
impl Publisher for Memory {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError> {
        let limit = if job.request.origin.reference == "small" {
            SMALL
        } else {
            LARGE
        };
        Ok(Constraints::universal(limit))
    }

    async fn link_target(&self, _job: &Job) -> Result<LinkTarget, PublishError> {
        Err(PublishError::NoLink("no view shows this job".into()))
    }

    async fn publish(&self, _job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        self.published.lock().unwrap().push(file.path.clone());
        Ok(Published {
            reference: file.path.display().to_string(),
            url: None,
            at: jiff::Timestamp::now(),
            notes: Vec::new(),
        })
    }
}

fn serve(url: &str, bytes: &[u8]) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            url: url.into(),
            headers: vec![
                ("content-type".into(), "video/mp4".into()),
                ("content-length".into(), bytes.len().to_string()),
            ],
            body: RecordedBody::from_bytes(bytes),
            truncated: false,
        },
    }
}

async fn finished(engine: &EngineHandle, id: JobId) -> Job {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let job = engine.get(id).await.unwrap().expect("job stored");
        if job.status.is_terminal() {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job {id} did not finish: {:?}",
            job.status
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A request for `url` from `reference`, taking any quality the destination fits
fn request(url: &str, reference: &str) -> Request {
    let mut request = Request::new(
        Origin {
            source: SourceId::new("memory"),
            reference: reference.into(),
            url: None,
            guild: None,
            channel: None,
        },
        Url::parse(url).unwrap(),
    );
    request.policy.delivery = DeliveryPolicy {
        floor: QualityFloor {
            min_height: 0,
            min_bitrate: 0,
        },
        ..DeliveryPolicy::default()
    };
    request
}

/// Every media file under `dir`, the job records left out
fn media_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e != "json") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn requests_made(http: &Http) -> u64 {
    http.stats().requests.iter().map(|(_, _, n)| n).sum()
}

struct Rig {
    dir: PathBuf,
    handle: EngineHandle,
    http: Http,
    resolves: Arc<AtomicUsize>,
    published: Arc<Mutex<Vec<PathBuf>>>,
    shutdown: CancellationToken,
    running: tokio::task::JoinHandle<Result<(), discoclip_engine::EngineError>>,
}

/// An engine with the archive keeping `keep`, serving one clip bigger than the small destination takes
async fn rig(keep: Keep) -> Rig {
    let dir = std::env::temp_dir().join(format!("discoclip-dedupe-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
    let clip_path = dir.join("clip.mp4");
    let mut a: Vec<OsString> = [
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30:duration=3",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-crf",
        "10",
        "-an",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    a.push(clip_path.as_os_str().to_owned());
    ffmpeg.run(a, |_| {}).await.unwrap();
    let clip = tokio::fs::read(&clip_path).await.unwrap();
    assert!(
        clip.len() as u64 > SMALL,
        "the clip must be too big for the small destination"
    );

    let mut fixture = Fixture::new("dedupe", None);
    fixture
        .exchanges
        .push(serve(&format!("https://{HOST}/clip.mp4"), &clip));
    let http = Http::replay(fixture);
    let published = Arc::new(Mutex::new(Vec::new()));
    let resolves = Arc::new(AtomicUsize::new(0));
    let config = EngineConfig {
        cache_dir: dir.join("cache"),
        workers: 1,
        archive: ArchiveConfig {
            enabled: true,
            dir: dir.join("archive"),
            keep,
        },
        ..EngineConfig::default()
    };
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let engine = Engine::builder(config.clone(), store, http.clone())
        .resolver(Counted {
            resolves: resolves.clone(),
        })
        .downloader(HttpDownloader::new(http.clone(), ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg))
        .archiver(FsArchiver::new(config.archive))
        .publisher(Memory {
            source: SourceId::new("memory"),
            published: published.clone(),
        })
        .build()
        .unwrap();
    let handle = engine.handle();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(engine.run(shutdown.clone()));
    Rig {
        dir,
        handle,
        http,
        resolves,
        published,
        shutdown,
        running,
    }
}

impl Rig {
    async fn run(&self, url: &str, reference: &str) -> Job {
        let id = self.handle.submit(request(url, reference)).await.unwrap();
        let job = finished(&self.handle, id).await;
        assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
        job
    }

    fn archive(&self) -> PathBuf {
        self.dir.join("archive")
    }

    async fn close(self) {
        self.shutdown.cancel();
        self.running.await.unwrap().unwrap();
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

fn noted(job: &Job, text: &str) -> bool {
    job.log.iter().any(|e| e.message.contains(text))
}

#[tokio::test]
async fn a_repeated_link_is_served_from_the_archive_without_the_network() {
    let rig = rig(Keep::Output).await;
    let link = format!("https://{HOST}/clip.mp4");

    let first = rig.run(&link, "large").await;
    assert_eq!(rig.resolves.load(Ordering::SeqCst), 1);
    assert!(noted(&first, "Downloaded"), "{:?}", first.log);
    let downloads = requests_made(&rig.http);
    assert!(downloads >= 1);
    let archived = first.artifacts.archived.clone().expect("archived");
    assert!(archived.output.is_some());
    assert_eq!(media_files(&rig.archive()).len(), archived.files.len() - 1);
    let after_first = media_files(&rig.archive());

    // The same link again, dressed in a tracking parameter, from a destination it fits.
    let second = rig.run(&format!("{link}?utm_source=again"), "large").await;
    assert_eq!(rig.resolves.load(Ordering::SeqCst), 1, "resolved again");
    assert_eq!(requests_made(&rig.http), downloads, "fetched again");
    assert_eq!(second.artifacts.reused_from, Some(first.id));
    assert!(
        noted(&second, "reusing its archived output",)
            && noted(&second, "Resolving, download and conversion skipped."),
        "{:?}",
        second.log
    );
    assert!(noted(&second, "Not archived again"), "{:?}", second.log);
    assert!(!noted(&second, "Downloaded"), "{:?}", second.log);
    assert_eq!(second.artifacts.archived, first.artifacts.archived);
    assert_eq!(
        second
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref()),
        Some("clip.mp4")
    );
    assert_eq!(media_files(&rig.archive()), after_first);
    assert_eq!(rig.published.lock().unwrap().len(), 2);

    // The same link for a destination the archived output is too big for: the output is
    // converted down rather than the source fetched again.
    let third = rig.run(&link, "small").await;
    assert_eq!(rig.resolves.load(Ordering::SeqCst), 1, "resolved again");
    assert_eq!(requests_made(&rig.http), downloads, "fetched again");
    assert!(
        [Some(first.id), Some(second.id)].contains(&third.artifacts.reused_from),
        "{:?}",
        third.artifacts.reused_from
    );
    assert!(
        noted(
            &third,
            "Converting from that output instead. Resolving and download skipped."
        ),
        "{:?}",
        third.log
    );
    let output = third.artifacts.output.as_ref().expect("output");
    assert!(output.size <= SMALL, "{} bytes", output.size);
    let third_archive = third.artifacts.archived.clone().expect("archived");
    assert_ne!(third_archive.output, archived.output);
    assert!(third_archive.source.is_none());
    assert_eq!(
        media_files(&rig.archive()).len(),
        after_first.len() + third_archive.files.len() - 1
    );
    rig.close().await;
}

#[tokio::test]
async fn a_source_read_from_the_archive_is_not_archived_twice() {
    let rig = rig(Keep::Both).await;
    let link = format!("https://{HOST}/clip.mp4");

    let first = rig.run(&link, "large").await;
    let archived = first.artifacts.archived.clone().expect("archived");
    assert!(archived.source.is_some() && archived.output.is_some());
    let downloads = requests_made(&rig.http);
    let after_first = media_files(&rig.archive());

    let second = rig.run(&link, "small").await;
    assert_eq!(rig.resolves.load(Ordering::SeqCst), 1, "resolved again");
    assert_eq!(requests_made(&rig.http), downloads, "fetched again");
    assert!(
        noted(
            &second,
            "Converting from its archived source instead. Resolving and download skipped."
        ),
        "{:?}",
        second.log
    );
    let output = second.artifacts.output.as_ref().expect("output");
    assert!(output.size <= SMALL, "{} bytes", output.size);
    let second_archive = second.artifacts.archived.clone().expect("archived");
    assert_eq!(second_archive.source, archived.source);
    assert_ne!(second_archive.output, archived.output);
    let files = media_files(&rig.archive());
    assert_eq!(
        files.len(),
        after_first.len() + second_archive.files.len() - 1,
        "{files:?}"
    );
    assert_eq!(
        files
            .iter()
            .filter(|f| f.to_string_lossy().contains("-source"))
            .count(),
        1,
        "{files:?}"
    );
    rig.close().await;
}
