//! Composition root: reads the provisioning file, bootstraps the settings store from it,
//! builds the engine with every resolver, downloader, publisher and archiver, and runs the
//! server until it is told to stop.

use std::process::ExitCode;
use std::sync::Arc;

use discoclip_bot::{Clients, DiscordEndpoints, DiscordPublisher};
use discoclip_engine::archive::FsArchiver;
use discoclip_engine::download::HttpDownloader;
use discoclip_engine::download::dash::DashDownloader;
use discoclip_engine::download::hls::HlsDownloader;
use discoclip_engine::ffmpeg::Ffmpeg;
use discoclip_engine::resolve::{Resolver, standard_resolvers};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::transcode::FfmpegTranscoder;
use discoclip_engine::{Engine, EngineBuilder, Http};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::applications::ApplicationStore;
use crate::args::Args;
use crate::bots::BotManager;
use crate::config;
use crate::cookies::CookieStore;
use crate::discord::BotGuildStore;
use crate::fixtures::{FixtureRunner, FixtureStore};
use crate::local::{LocalPublisher, SharedLocalConfig};
use crate::migrations;
use crate::oauth::Registry;
use crate::rules::RuleStore;
use crate::secrets::{KEY_FILE, Keyring, SecretError};
use crate::settings::{self, Settings, SettingsStore};
use crate::telemetry::{self, LogHandle};
use crate::web::{self, Services, WebApp};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Settings(#[from] settings::SettingsError),
    #[error(transparent)]
    Engine(#[from] discoclip_engine::EngineError),
    #[error(transparent)]
    Store(#[from] discoclip_engine::StoreError),
    #[error(transparent)]
    Ffmpeg(#[from] discoclip_engine::ffmpeg::FfmpegError),
    #[error(transparent)]
    Web(#[from] web::WebError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Application(#[from] crate::applications::ApplicationError),
    #[error(transparent)]
    Rules(#[from] crate::rules::RuleError),
    #[error(transparent)]
    Cookies(#[from] crate::cookies::CookieError),
}

pub fn run(args: Args) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        // Provisioning names where the database lives; everything else the server runs on
        // is read from that database after provisioning has been written into it.
        let provisioning = config::load(args.config.as_deref())?;
        let data_dir = provisioning.data_dir()?;
        let store = SqliteStore::open(&data_dir.join("discoclip.db")).await?;
        let keyring = Keyring::load_or_create(&data_dir.join(KEY_FILE))?;
        let migrated = migrations::apply(&store).await?;
        let boot = settings::bootstrap(&store, &provisioning).await?;
        let log = telemetry::init(&boot.settings.log.level);
        if migrated > 0 {
            tracing::info!(count = migrated, "database migrated");
        }
        match &provisioning.file {
            Some(path) => tracing::info!(file = %path.display(), "provisioning file applied"),
            None => tracing::info!("no provisioning file found; settings read from the database"),
        }
        for key in &boot.kept {
            tracing::warn!(
                key,
                "provisioned value not applied: it was changed in the app"
            );
        }
        serve(Startup {
            settings: boot.settings,
            store,
            keyring,
            log,
            data_dir,
            provisioning_file: provisioning.file.clone(),
        })
        .await
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(10));
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn resolvers(http: &Http) -> Vec<Arc<dyn Resolver>> {
    standard_resolvers(http)
}

/// The engine as the settings shape it, with the ffmpeg handle it runs on.
async fn builder(
    settings: &Settings,
    store: SqliteStore,
) -> Result<(EngineBuilder, Ffmpeg), Error> {
    let engine_config = settings.engine.clone();
    tokio::fs::create_dir_all(&engine_config.cache_dir).await?;
    let ffmpeg = Ffmpeg::provision(&engine_config.cache_dir).await?;
    let store = Arc::new(store);
    let http = Http::new(settings.http.clone());
    let mut builder = Engine::builder(engine_config.clone(), store, http.clone())
        .downloader(HttpDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(HlsDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(DashDownloader::new(http.clone(), ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg.clone()));
    for resolver in resolvers(&http) {
        builder = builder.resolver_arc(resolver);
    }
    builder = builder.archiver(FsArchiver::new(engine_config.archive));
    Ok((builder, ffmpeg))
}

/// Cancels `token` on the first shutdown signal and forces exit on the second.
fn cancel_on_signal(token: CancellationToken) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::spawn(async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            tracing::info!("shutdown requested");
            token.cancel();
            let code = tokio::select! {
                _ = interrupt.recv() => 130,
                _ = terminate.recv() => 143,
            };
            tracing::warn!("second shutdown signal received; forcing exit");
            std::process::exit(code);
        });
    }
    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        tokio::spawn(async move {
            ctrl_c.recv().await;
            tracing::info!("shutdown requested");
            token.cancel();
            ctrl_c.recv().await;
            tracing::warn!("second shutdown signal received; forcing exit");
            std::process::exit(130);
        });
    }
    Ok(())
}

/// What the server starts from once provisioning has been applied.
struct Startup {
    settings: Settings,
    store: SqliteStore,
    keyring: Keyring,
    log: LogHandle,
    data_dir: std::path::PathBuf,
    provisioning_file: Option<std::path::PathBuf>,
}

/// Runs the server: the engine and the web app are the process, and end it when they fail.
/// The Discord bot is supervised beside them; its failures show up in the web app, never as
/// an exit.
async fn serve(startup: Startup) -> Result<ExitCode, Error> {
    let started_at = jiff::Timestamp::now();
    let Startup {
        settings,
        store,
        keyring,
        log,
        data_dir,
        provisioning_file,
    } = startup;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting discoclip");

    let endpoints = DiscordEndpoints::default();
    let clients: Clients = Clients::default();
    let (mut builder, ffmpeg) = builder(&settings, store.clone()).await?;
    let local: SharedLocalConfig = Arc::new(std::sync::RwLock::new(settings.local.clone()));
    builder = builder
        .publisher(LocalPublisher::new(local.clone()))
        .publisher(DiscordPublisher::new(clients.clone()));
    let engine = builder.build()?;
    let handle = engine.handle();
    let jars = CookieStore::new(store.clone(), keyring.clone())
        .jars()
        .await?;
    for (platform, jar) in jars {
        tracing::info!(platform, cookies = jar.len(), "session cookies loaded");
        handle.http().set_jar(&platform, jar);
    }

    let shutdown = CancellationToken::new();
    cancel_on_signal(shutdown.clone())?;

    let fixtures = Arc::new(FixtureRunner::new(
        handle.clone(),
        FixtureStore::new(store.clone()),
        Arc::new(std::sync::RwLock::new(settings.fixtures.clone())),
    ));

    let rules = RuleStore::new(store.clone());
    rules.load().await?;
    let bots = Arc::new(BotManager::new(
        handle.clone(),
        endpoints.clone(),
        clients,
        BotGuildStore::new(store.clone()),
        rules.cache(),
        shutdown.clone(),
    ));
    let applications = ApplicationStore::new(store.clone(), keyring.clone())
        .all_credentials()
        .await?;
    if applications.is_empty() {
        tracing::info!("no discord applications yet: add one in the web app to run a bot");
    }
    for (application, credentials) in &applications {
        bots.launch(application, &credentials.bot_token).await;
    }

    let mut tasks: JoinSet<Result<&'static str, Error>> = JoinSet::new();
    let token = shutdown.clone();
    tasks.spawn(async move {
        engine.run(token).await?;
        Ok("engine")
    });
    let token = shutdown.clone();
    let schedule = fixtures.clone();
    tasks.spawn(async move {
        schedule.schedule(token).await;
        Ok("fixtures")
    });
    let token = shutdown.clone();
    let app = WebApp::new(
        &settings,
        Registry::from_config(&settings.auth),
        Services {
            store: store.clone(),
            keyring,
            bots: bots.clone(),
            rules,
            discord: endpoints,
            engine: handle,
            fixtures,
            settings: SettingsStore::new(store),
            log,
            ffmpeg,
            local,
            data_dir,
            provisioning_file,
            started_at,
        },
    )
    .await?;
    tasks.spawn(async move {
        app.serve(token).await?;
        Ok("web")
    });

    let mut code = ExitCode::SUCCESS;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(name)) => tracing::info!("{name} stopped"),
            Ok(Err(error)) => {
                tracing::error!("{error}");
                code = ExitCode::FAILURE;
                shutdown.cancel();
            }
            Err(error) => {
                tracing::error!("task failed: {error}");
                code = ExitCode::FAILURE;
                shutdown.cancel();
            }
        }
    }
    shutdown.cancel();
    bots.stop_all().await;
    Ok(code)
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn shutdown_signal_child() {
        if std::env::var_os("DISCOCLIP_TEST_SHUTDOWN_SIGNAL").is_none() {
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let shutdown = CancellationToken::new();
            super::cancel_on_signal(shutdown.clone()).unwrap();
            println!("signal handler ready");
            shutdown.cancelled().await;
            println!("shutdown started");
            std::future::pending::<()>().await;
        });
    }

    #[tokio::test]
    async fn shutdown_second_ctrl_c_forces_exit() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "compose::tests::shutdown_signal_child",
                "--nocapture",
            ])
            .env("DISCOCLIP_TEST_SHUTDOWN_SIGNAL", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            // Wait for the handler before the first signal, and for graceful shutdown
            // before the second so the operating system cannot coalesce them.
            for expected in ["signal handler ready", "shutdown started"] {
                loop {
                    let line = output
                        .next_line()
                        .await
                        .unwrap()
                        .expect("child remains alive until the second Ctrl-C");
                    if line == expected {
                        break;
                    }
                }
                assert!(
                    Command::new("kill")
                        .args(["-INT", &child.id().unwrap().to_string()])
                        .status()
                        .await
                        .unwrap()
                        .success()
                );
            }
            assert_eq!(child.wait().await.unwrap().code(), Some(130));
        })
        .await
        .expect("the second Ctrl-C exits even while graceful shutdown is stuck");
    }
}
