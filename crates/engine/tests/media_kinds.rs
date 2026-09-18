//! Every kind of media the engine takes, run through the whole pipeline with the embedded
//! ffmpeg: a still image scaled down to fit, audio alone re-encoded to fit, audio passed
//! through when it already fits, a plain file published as it is, and a video as before.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use discoclip_engine::download::HttpDownloader;
use discoclip_engine::ffmpeg::Ffmpeg;
use discoclip_engine::http::transport::{
    Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
};
use discoclip_engine::job::{Delivery, Job, JobStatus, Origin, Request, SourceId};
use discoclip_engine::media::{Container, LocalFile, MediaKind};
use discoclip_engine::publish::{
    Constraints, Fallback, PublishError, Published, Publisher, QualityFloor,
};
use discoclip_engine::resolve::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::transcode::FfmpegTranscoder;
use discoclip_engine::{Engine, EngineConfig, Http};
use tokio_util::sync::CancellationToken;
use url::Url;

const HOST: &str = "files.test";
/// What the small destination takes, tight enough that the image and the WAV must shrink.
const SMALL: u64 = 60_000;
const LARGE: u64 = 50_000_000;

/// A host that serves files of every kind, each named for what it is.
struct Files;

#[async_trait]
impl Resolver for Files {
    fn id(&self) -> &'static str {
        "files"
    }

    fn platform(&self) -> Platform {
        Platform {
            id: "files",
            name: "Files",
            hosts: &[HOST],
            features: &["files"],
            formats: &["png", "wav", "mp3", "txt", "mp4"],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            examples: &[],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        url.host_str() == Some(HOST)
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let name = url.path().trim_start_matches('/').to_string();
        let container = Container::from_name(&name).expect("test files have extensions");
        let kind = container.kind();
        let mut variant = Variant::file(url.clone());
        variant.audio_only = kind == MediaKind::Audio;
        variant.container = Some(container);
        let mut resolved = Resolved::of("files", kind);
        resolved.title = Some(name);
        resolved.variants = vec![variant];
        Ok(resolved.into())
    }
}

/// A destination that remembers what was published; `small` origins take little, and
/// `linked` origins take little but have a page to fall back on.
struct Memory {
    source: SourceId,
    published: Arc<Mutex<Vec<(String, PathBuf, Delivery)>>>,
}

/// What a fallback page takes: much more, at no less than 480p and 1 Mb/s.
fn fallback() -> Fallback {
    Fallback {
        max_bytes: LARGE,
        floor: QualityFloor {
            min_height: 480,
            min_bitrate: 1_000_000,
        },
    }
}

#[async_trait]
impl Publisher for Memory {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError> {
        let reference = job.request.origin.reference.as_str();
        let limit = if reference == "large" { LARGE } else { SMALL };
        let mut constraints = Constraints::universal(limit);
        if reference == "linked" {
            constraints.fallback = Some(fallback());
        }
        Ok(constraints)
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        self.published.lock().unwrap().push((
            job.request.origin.reference.clone(),
            file.path.clone(),
            job.artifacts.delivery,
        ));
        Ok(Published {
            reference: file.path.display().to_string(),
            url: None,
            at: jiff::Timestamp::now(),
        })
    }
}

fn serve(url: &str, content_type: &str, bytes: &[u8]) -> Exchange {
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
                ("content-type".into(), content_type.into()),
                ("content-length".into(), bytes.len().to_string()),
            ],
            body: RecordedBody::from_bytes(bytes),
            truncated: false,
        },
    }
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// Makes a file with ffmpeg from a lavfi source and reads it back.
async fn make(ffmpeg: &Ffmpeg, source: &str, extra: &[&str], out: &Path) -> Vec<u8> {
    let mut a = args(&["-loglevel", "error", "-f", "lavfi", "-i", source]);
    a.extend(args(extra));
    a.push(out.as_os_str().to_owned());
    ffmpeg.run(a, |_| {}).await.unwrap();
    tokio::fs::read(out).await.unwrap()
}

async fn finished(engine: &discoclip_engine::EngineHandle, id: discoclip_engine::JobId) -> Job {
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

fn request(name: &str, reference: &str) -> Request {
    Request::new(
        Origin {
            source: SourceId::new("memory"),
            reference: reference.into(),
            url: None,
            guild: None,
            channel: None,
        },
        Url::parse(&format!("https://{HOST}/{name}")).unwrap(),
    )
}

#[tokio::test]
async fn every_kind_of_media_goes_through_the_pipeline() {
    let dir = std::env::temp_dir().join(format!("discoclip-kinds-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
    let inputs = dir.join("inputs");
    tokio::fs::create_dir_all(&inputs).await.unwrap();

    let png = make(
        &ffmpeg,
        "testsrc2=size=1920x1080:rate=1:duration=1",
        &["-frames:v", "1", "-c:v", "png"],
        &inputs.join("pic.png"),
    )
    .await;
    assert!(
        png.len() > SMALL as usize,
        "the test picture must need shrinking"
    );
    let wav = make(
        &ffmpeg,
        "sine=frequency=440:duration=3",
        &["-c:a", "pcm_s16le", "-ar", "48000"],
        &inputs.join("tone.wav"),
    )
    .await;
    assert!(
        wav.len() > SMALL as usize,
        "the test WAV must need shrinking"
    );
    let mp3 = make(
        &ffmpeg,
        "sine=frequency=440:duration=2",
        &["-c:a", "libmp3lame", "-b:a", "32k"],
        &inputs.join("tone.mp3"),
    )
    .await;
    assert!(mp3.len() < SMALL as usize, "the test MP3 must fit as it is");
    let mp4 = make(
        &ffmpeg,
        "testsrc=size=64x64:rate=10:duration=1",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-an"],
        &inputs.join("clip.mp4"),
    )
    .await;
    let txt = b"just some words in a plain file\n".to_vec();

    let mut fixture = Fixture::new("kinds", None);
    fixture
        .exchanges
        .push(serve(&format!("https://{HOST}/pic.png"), "image/png", &png));
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/tone.wav"),
        "audio/wav",
        &wav,
    ));
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/tone.mp3"),
        "audio/mpeg",
        &mp3,
    ));
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/clip.mp4"),
        "video/mp4",
        &mp4,
    ));
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/notes.txt"),
        "text/plain",
        &txt,
    ));
    let http = Http::replay(fixture);

    let published = Arc::new(Mutex::new(Vec::new()));
    let config = EngineConfig {
        cache_dir: dir.join("cache"),
        workers: 1,
        ..EngineConfig::default()
    };
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let engine = Engine::builder(config, store, http.clone())
        .resolver(Files)
        .downloader(HttpDownloader::new(http, ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg.clone()))
        .publisher(Memory {
            source: SourceId::new("memory"),
            published: published.clone(),
        })
        .build()
        .unwrap();
    let handle = engine.handle();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(engine.run(shutdown.clone()));

    let mut ids = HashMap::new();
    for (name, reference) in [
        ("pic.png", "small"),
        ("tone.wav", "small"),
        ("tone.mp3", "small"),
        ("clip.mp4", "large"),
        ("notes.txt", "large"),
    ] {
        ids.insert(name, handle.submit(request(name, reference)).await.unwrap());
    }

    // The picture is over the small destination's limit as a PNG, so it is scaled down
    // and stays a PNG, since the destination shows PNG.
    let job = finished(&handle, ids["pic.png"]).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    assert_eq!(
        job.artifacts.resolved.as_ref().unwrap().media,
        MediaKind::Image
    );
    let source = job.artifacts.source.as_ref().unwrap();
    assert_eq!(source.info.as_ref().unwrap().kind, MediaKind::Image);
    let output = job.artifacts.output.as_ref().unwrap();
    assert!(output.size <= SMALL, "{} bytes", output.size);
    assert_eq!(output.path.extension().unwrap(), "png");
    let info = output.info.as_ref().unwrap();
    assert_eq!(info.kind, MediaKind::Image);
    let picture = info.video.as_ref().unwrap();
    assert!(picture.width < 1920 && picture.width > 0, "{picture:?}");
    assert_eq!(job.media(), MediaKind::Image);
    assert!(
        job.log
            .iter()
            .any(|e| e.message.contains("resolved image with 1 variant(s)")),
        "{:?}",
        job.log
    );

    // The WAV is neither small enough nor a format the destination plays, so it is
    // encoded to AAC in M4A under the limit.
    let job = finished(&handle, ids["tone.wav"]).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    let output = job.artifacts.output.as_ref().unwrap();
    assert!(output.size <= SMALL, "{} bytes", output.size);
    assert_eq!(output.path.extension().unwrap(), "m4a");
    let info = output.info.as_ref().unwrap();
    assert_eq!(info.kind, MediaKind::Audio);
    assert_eq!(info.container, Container::M4a);
    assert!(info.video.is_none());
    assert!(info.audio.is_some());
    assert!((info.duration.unwrap().as_secs_f64() - 3.0).abs() < 0.5);

    // The MP3 fits and is a format the destination plays, so it goes as it is.
    let job = finished(&handle, ids["tone.mp3"]).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    let source = job.artifacts.source.as_ref().unwrap();
    let output = job.artifacts.output.as_ref().unwrap();
    assert_eq!(output.path, source.path);
    assert_eq!(output.path.extension().unwrap(), "mp3");
    assert_eq!(output.info.as_ref().unwrap().kind, MediaKind::Audio);
    assert!(
        job.log
            .iter()
            .any(|e| e.message.contains("publishing as is")),
        "{:?}",
        job.log
    );

    // A video still goes through as before.
    let job = finished(&handle, ids["clip.mp4"]).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    let output = job.artifacts.output.as_ref().unwrap();
    assert_eq!(output.info.as_ref().unwrap().kind, MediaKind::Video);
    assert_eq!(output.path.extension().unwrap(), "mp4");

    // A plain file is never probed or converted: it is published as it is.
    let job = finished(&handle, ids["notes.txt"]).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    assert_eq!(
        job.artifacts.resolved.as_ref().unwrap().media,
        MediaKind::File
    );
    let source = job.artifacts.source.as_ref().unwrap();
    assert!(source.info.is_none());
    let output = job.artifacts.output.as_ref().unwrap();
    assert_eq!(output.path, source.path);
    assert_eq!(output.path.extension().unwrap(), "txt");
    assert_eq!(tokio::fs::read(&output.path).await.unwrap(), txt);
    assert_eq!(job.media(), MediaKind::File);

    let posted = published.lock().unwrap().clone();
    assert_eq!(posted.len(), 5);
    assert!(
        posted
            .iter()
            .all(|(_, _, delivery)| *delivery == Delivery::Upload)
    );

    shutdown.cancel();
    running.await.unwrap().unwrap();
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn a_file_too_large_for_its_destination_is_refused_not_converted() {
    let dir = std::env::temp_dir().join(format!("discoclip-kinds-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
    let big: Vec<u8> = (0..(SMALL as usize + 1)).map(|i| (i % 251) as u8).collect();
    let mut fixture = Fixture::new("big", None);
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/blob.bin"),
        "application/octet-stream",
        &big,
    ));
    let http = Http::replay(fixture);
    let config = EngineConfig {
        cache_dir: dir.join("cache"),
        workers: 1,
        ..EngineConfig::default()
    };
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let engine = Engine::builder(config, store, http.clone())
        .resolver(Files)
        .downloader(HttpDownloader::new(http, ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg))
        .publisher(Memory {
            source: SourceId::new("memory"),
            published: Arc::new(Mutex::new(Vec::new())),
        })
        .build()
        .unwrap();
    let handle = engine.handle();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(engine.run(shutdown.clone()));
    let id = handle.submit(request("blob.bin", "small")).await.unwrap();
    let job = finished(&handle, id).await;
    match &job.status {
        JobStatus::Failed { stage, message } => {
            assert_eq!(stage.as_str(), "transcode");
            assert!(
                message.contains("cannot be made smaller"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    shutdown.cancel();
    running.await.unwrap().unwrap();
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn a_destination_with_a_page_gets_a_link_when_the_upload_would_be_too_reduced() {
    let dir = std::env::temp_dir().join(format!("discoclip-kinds-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
    let inputs = dir.join("inputs");
    tokio::fs::create_dir_all(&inputs).await.unwrap();
    // Twenty seconds of 720p at a bitrate far above what the small destination can hold.
    let mp4 = make(
        &ffmpeg,
        "testsrc2=size=1280x720:rate=30:duration=20",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-b:v",
            "4M",
            "-pix_fmt",
            "yuv420p",
            "-an",
        ],
        &inputs.join("big.mp4"),
    )
    .await;
    assert!(mp4.len() > SMALL as usize);
    let big: Vec<u8> = (0..(SMALL as usize + 1)).map(|i| (i % 251) as u8).collect();
    let mut fixture = Fixture::new("linked", None);
    fixture
        .exchanges
        .push(serve(&format!("https://{HOST}/big.mp4"), "video/mp4", &mp4));
    fixture.exchanges.push(serve(
        &format!("https://{HOST}/blob.bin"),
        "application/octet-stream",
        &big,
    ));
    let http = Http::replay(fixture);
    let published = Arc::new(Mutex::new(Vec::new()));
    let config = EngineConfig {
        cache_dir: dir.join("cache"),
        workers: 1,
        ..EngineConfig::default()
    };
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let engine = Engine::builder(config, store, http.clone())
        .resolver(Files)
        .downloader(HttpDownloader::new(http, ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg))
        .publisher(Memory {
            source: SourceId::new("memory"),
            published: published.clone(),
        })
        .build()
        .unwrap();
    let handle = engine.handle();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(engine.run(shutdown.clone()));

    // 60 KB cannot hold 20 s at the 1 Mb/s floor, so the video is not even tried for
    // upload: the page gets an output under the fallback's bound, the source as it is.
    let id = handle.submit(request("big.mp4", "linked")).await.unwrap();
    let job = finished(&handle, id).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    assert_eq!(job.artifacts.delivery, Delivery::Link);
    let reason = job.artifacts.link_reason.clone().unwrap();
    assert!(reason.contains("under the 1000 kb/s floor"), "{reason}");
    let output = job.artifacts.output.as_ref().unwrap();
    let source = job.artifacts.source.as_ref().unwrap();
    assert_eq!(output.path, source.path);
    assert_eq!(
        output.info.as_ref().unwrap().video.as_ref().unwrap().height,
        720
    );
    assert!(
        job.log
            .iter()
            .any(|e| e.message.contains("a link to the page will be posted")),
        "{:?}",
        job.log
    );

    // A file over the limit but under the fallback's bound is linked as it is.
    let id = handle.submit(request("blob.bin", "linked")).await.unwrap();
    let job = finished(&handle, id).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.log);
    assert_eq!(job.artifacts.delivery, Delivery::Link);
    assert!(
        job.artifacts
            .link_reason
            .as_deref()
            .unwrap()
            .contains("over the 60000 the destination takes")
    );

    let posted = published.lock().unwrap().clone();
    assert_eq!(posted.len(), 2);
    assert!(
        posted
            .iter()
            .all(|(_, _, delivery)| *delivery == Delivery::Link)
    );

    shutdown.cancel();
    running.await.unwrap().unwrap();
    let _ = tokio::fs::remove_dir_all(&dir).await;
}
