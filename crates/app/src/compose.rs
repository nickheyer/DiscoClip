//! Composition root: reads the provisioning file, bootstraps the settings store from it,
//! builds the engine with every resolver, downloader, publisher and archiver, and runs the
//! server until it is told to stop.

use std::process::ExitCode;
use std::sync::Arc;

use discoclip_bot::{Clients, DiscordEndpoints, DiscordPublisher, SharedDiscordSettings};
use discoclip_engine::archive::FsArchiver;
use discoclip_engine::download::HttpDownloader;
use discoclip_engine::download::dash::DashDownloader;
use discoclip_engine::download::hls::HlsDownloader;
use discoclip_engine::download::ism::IsmDownloader;
use discoclip_engine::download::stream::StreamDownloader;
use discoclip_engine::download::whep::WhepDownloader;
use discoclip_engine::ffmpeg::Ffmpeg;
use discoclip_engine::resolve::{Resolver, standard_resolvers};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::transcode::FfmpegTranscoder;
use discoclip_engine::{Engine, EngineBuilder, Http};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::applications::ApplicationStore;
use crate::args::{Args, Command};
use crate::backup::{self, Backups};
use crate::bots::BotManager;
use crate::config;
use crate::cookies::CookieStore;
use crate::discord::BotGuildStore;
use crate::fixtures::{FixtureRunner, FixtureStore};
use crate::frontends::FrontendStore;
use crate::local::{LocalPublisher, SharedLocalConfig};
use crate::migrations;
use crate::oauth::Registry;
use crate::profiles::{PlatformFacts, ProfileStore};
use crate::retention::Retention;
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
    Profiles(#[from] crate::profiles::ProfileError),
    #[error(transparent)]
    Frontends(#[from] crate::frontends::FrontendError),
    #[error(transparent)]
    Cookies(#[from] crate::cookies::CookieError),
}

pub fn run(args: Args) -> ExitCode {
    if let Some(Command::Restore { file }) = &args.command {
        return restore(args.config.as_deref(), file);
    }
    let restores = Arc::new(crate::restore::Restores::default());
    let process_shutdown = CancellationToken::new();
    let mut log: Option<LogHandle> = None;
    let mut restarting = false;
    let mut recovery: Option<crate::restore::PreparedRestore> = None;
    loop {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("could not start runtime: {error}");
                return ExitCode::FAILURE;
            }
        };
        let result = runtime.block_on(async {
            let provisioning = config::load(args.config.as_deref())?;
            let data_dir = provisioning.data_dir()?;
            let store = SqliteStore::open(&data_dir.join(backup::DATABASE_FILE)).await?;
            let keyring = Keyring::load_or_create(&data_dir.join(KEY_FILE))?;
            migrations::apply(&store).await?;
            let settings = if restarting {
                // Provisioning is applied once per process. Reapplying it here could
                // overwrite values that the user just restored.
                SettingsStore::new(store.clone()).load().await?
            } else {
                let boot = settings::bootstrap(&store, &provisioning).await?;
                for key in &boot.kept {
                    tracing::warn!(
                        key,
                        "provisioned value not applied: it was changed in the app"
                    );
                }
                boot.settings
            };
            let log = log
                .get_or_insert_with(|| telemetry::init(&settings.log.level))
                .clone();
            let _ = log.set(&settings.log.level);
            serve(Startup {
                settings,
                store,
                keyring,
                log,
                data_dir,
                provisioning_file: provisioning.file.clone(),
                restores: restores.clone(),
                process_shutdown: process_shutdown.clone(),
            })
            .await
        });
        // Drop detached fixture tasks and bot supervisors as well as the joined
        // services. No task from the previous runtime may reach the restored DB.
        runtime.shutdown_timeout(std::time::Duration::from_secs(10));
        if process_shutdown.is_cancelled() {
            return finish(result);
        }
        if let Some(prepared) = restores.take_pending() {
            restarting = true;
            if !matches!(&result, Ok(code) if *code == ExitCode::SUCCESS) {
                restores.fail("The restore was cancelled because services could not stop cleanly. Your current database is unchanged.");
                continue;
            }
            match restores.apply(&prepared) {
                Ok(()) => recovery = Some(prepared),
                Err(error) => {
                    tracing::error!("database restore failed: {error}");
                    restores.fail(
                        "The backup could not be restored. Your current database is unchanged.",
                    );
                }
            }
            continue;
        }
        // A backup can be structurally valid yet contain unusable runtime settings.
        // If startup fails, bring back the safety copy so the UI remains available.
        if restores.starting()
            && let Some(prepared) = recovery.take()
        {
            tracing::error!("restored services could not start; recovering the previous database");
            if let Err(error) = backup::restore(&prepared.data_dir, &prepared.safety) {
                eprintln!("could not recover the previous database: {error}");
                return ExitCode::FAILURE;
            }
            restores.fail("DiscoClip could not start with that backup. Your previous database has been recovered.");
            continue;
        }
        return finish(result);
    }
}

fn finish(result: Result<ExitCode, Error>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `discoclip restore <file>`: the backup checked and put in place of the database under
/// the data directory the provisioning names.
fn restore(config: Option<&std::path::Path>, file: &std::path::Path) -> ExitCode {
    let data_dir = match config::load(config).and_then(|p| p.data_dir()) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match backup::restore(&data_dir, file) {
        Ok(()) => {
            println!(
                "Restored {} to {}. Start the server to use it.",
                file.display(),
                data_dir.join(backup::DATABASE_FILE).display()
            );
            ExitCode::SUCCESS
        }
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
    // The build and encoders the settings name. A choice the machine cannot meet is
    // reported on the health page, and video is encoded in software until it can.
    ffmpeg
        .configure(
            &engine_config.cache_dir,
            engine_config.ffmpeg.as_deref(),
            engine_config.transcode.encoder,
            &engine_config.transcode.vaapi_device,
        )
        .await?;
    let store = Arc::new(store);
    let http = Http::new(settings.http.clone());
    let mut builder = Engine::builder(engine_config.clone(), store, http.clone())
        .downloader(HttpDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(HlsDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(DashDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(IsmDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(StreamDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(WhepDownloader::new(http.clone(), ffmpeg.clone()))
        .transcoder(FfmpegTranscoder::new(ffmpeg.clone()));
    for resolver in resolvers(&http) {
        builder = builder.resolver_arc(resolver);
    }
    builder = builder.archiver(FsArchiver::new(engine_config.archive));
    Ok((builder, ffmpeg))
}

/// How long the bots get to close their gateway connections at shutdown.
const BOT_STOP: std::time::Duration = std::time::Duration::from_secs(15);

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
            tracing::warn!("Second shutdown signal received. Forcing exit.");
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
            tracing::warn!("Second shutdown signal received. Forcing exit.");
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
    restores: Arc<crate::restore::Restores>,
    process_shutdown: CancellationToken,
}

/// Runs the server: the engine and the web app are the process, and end it when they fail.
/// The Discord bot is supervised beside them. Its failures show up in the web app, never as
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
        restores,
        process_shutdown,
    } = startup;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting discoclip");

    let endpoints = DiscordEndpoints::default();
    let clients: Clients = Clients::default();
    let directories = discoclip_bot::Directories::default();
    let (mut builder, ffmpeg) = builder(&settings, store.clone()).await?;
    let local: SharedLocalConfig = Arc::new(std::sync::RwLock::new(settings.local.clone()));
    let discord_settings: SharedDiscordSettings =
        Arc::new(std::sync::RwLock::new(settings.discord.clone()));
    // The stores the publishers read are built before the engine, from the resolvers
    // it will carry.
    let platform_names: std::collections::HashMap<String, String> = builder
        .platforms()
        .into_iter()
        .map(|p| (p.id.to_string(), p.name.to_string()))
        .collect();
    let profiles = ProfileStore::new(
        store.clone(),
        builder
            .platforms()
            .into_iter()
            .map(|p| PlatformFacts {
                id: p.id,
                tags: p.tags,
                hosts: p.hosts,
                on_by_default: p.on_by_default,
            })
            .collect(),
    );
    let loaded = profiles.load().await?;
    tracing::info!(
        profiles = loaded,
        off_by_default = ?profiles.cache().off_by_default(),
        "profiles loaded"
    );
    let public_url = Arc::new(crate::public_url::PublicUrl::new(
        settings.web.public_url.clone(),
        store.clone(),
    ));
    public_url.load().await?;
    if let Some(url) = public_url.learned() {
        tracing::info!(public_url = %url, "public address learned in an earlier run");
    }

    let frontends = FrontendStore::new(
        store.clone(),
        keyring.clone(),
        profiles.cache(),
        public_url.clone(),
    );
    let loaded = frontends.load().await?;
    tracing::info!(frontends = loaded, "front ends loaded");
    builder = builder
        .publisher(LocalPublisher::new(local.clone()))
        .publisher(DiscordPublisher::new(
            clients.clone(),
            directories.clone(),
            Arc::new(frontends.cache()),
            discord_settings.clone(),
            platform_names,
        ));
    let engine = builder.build()?;
    let handle = engine.handle();
    let jars = CookieStore::new(store.clone(), keyring.clone())
        .jars()
        .await?;
    for (platform, jar) in jars {
        tracing::info!(platform, cookies = jar.len(), "session cookies loaded");
        handle.http().set_jar(&platform, jar);
    }

    let shutdown = process_shutdown.child_token();
    cancel_on_signal(process_shutdown)?;

    let fixture_store = FixtureStore::new(store.clone());
    let seeded = fixture_store.seed(&handle.platforms()).await?;
    if seeded > 0 {
        tracing::info!(links = seeded, "check links the platforms ship with added");
    }
    let fixtures = Arc::new(FixtureRunner::new(
        handle.clone(),
        fixture_store,
        Arc::new(std::sync::RwLock::new(settings.fixtures.clone())),
    ));

    let rules = RuleStore::new(store.clone());
    rules.load().await?;
    let bots = Arc::new(BotManager::new(
        handle.clone(),
        endpoints.clone(),
        crate::bots::BotServices {
            clients,
            directories,
            guilds: BotGuildStore::new(store.clone()),
            rules: rules.cache(),
            profiles: profiles.cache(),
            own_links: public_url.clone(),
        },
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

    let retention = Arc::new(Retention::new(handle.clone()));
    let backups = Arc::new(
        Backups::new(
            store.clone(),
            data_dir.clone(),
            Arc::new(std::sync::RwLock::new(settings.backup.clone())),
        )
        .with_restores(restores),
    );

    let mut tasks: JoinSet<Result<&'static str, Error>> = JoinSet::new();
    let token = shutdown.clone();
    tasks.spawn(async move {
        engine.run(token).await?;
        Ok("engine")
    });
    let token = shutdown.clone();
    let sweeper = retention.clone();
    tasks.spawn(async move {
        sweeper.schedule(token).await;
        Ok("retention")
    });
    let token = shutdown.clone();
    let scheduler = backups.clone();
    tasks.spawn(async move {
        scheduler.schedule(token).await;
        Ok("backups")
    });
    let token = shutdown.clone();
    let schedule = fixtures.clone();
    tasks.spawn(async move {
        schedule.schedule(token).await;
        Ok("fixtures")
    });
    let token = shutdown.clone();
    let learner = fixtures.clone();
    tasks.spawn(async move {
        learner.learn_from_jobs(token).await;
        Ok("fixture learning")
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
            profiles,
            frontends,
            public_url,
            discord: endpoints,
            engine: handle,
            fixtures,
            settings: SettingsStore::new(store.clone()),
            log,
            ffmpeg,
            local,
            discord_settings,
            data_dir,
            provisioning_file,
            started_at,
            backups,
            retention,
            shutdown: shutdown.clone(),
        },
    )
    .await?;
    tasks.spawn(async move {
        app.serve(token).await?;
        Ok("web")
    });

    let mut code = ExitCode::SUCCESS;
    let mut stopping_since: Option<std::time::Instant> = None;
    while let Some(result) = tasks.join_next().await {
        if stopping_since.is_none() && shutdown.is_cancelled() {
            stopping_since = Some(std::time::Instant::now());
        }
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
    let began = stopping_since.unwrap_or_else(std::time::Instant::now);
    // The bots go last: the engine's jobs post through their clients until the grace
    // period is over. A gateway that will not close is left behind after a while.
    match tokio::time::timeout(BOT_STOP, bots.stop_all()).await {
        Ok(()) => tracing::info!("bots stopped"),
        Err(_) => tracing::warn!(
            "some bots did not stop within {}s and are left to the runtime",
            BOT_STOP.as_secs()
        ),
    }
    // Everything the database holds is folded into its main file, so a copy of the file
    // alone is whole.
    match store
        .call(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })
        .await
    {
        Ok(()) => tracing::info!("database checkpointed"),
        Err(error) => tracing::warn!("database not checkpointed: {error}"),
    }
    store.close().await?;
    tracing::info!(took_secs = began.elapsed().as_secs(), "shutdown complete");
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
