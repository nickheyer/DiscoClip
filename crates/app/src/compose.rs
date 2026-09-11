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
use discoclip_engine::resolve::Resolver;
use discoclip_engine::resolve::reddit::RedditResolver;
use discoclip_engine::resolve::twitter::TwitterResolver;
use discoclip_engine::resolve::web::WebResolver;
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::transcode::FfmpegTranscoder;
use discoclip_engine::{Engine, EngineBuilder, Http, HttpConfig};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::applications::ApplicationStore;
use crate::args::Args;
use crate::bots::BotManager;
use crate::config;
use crate::discord::BotGuildStore;
use crate::local::LocalPublisher;
use crate::migrations;
use crate::oauth::Registry;
use crate::rules::RuleStore;
use crate::secrets::{KEY_FILE, Keyring, SecretError};
use crate::settings::{self, Settings};
use crate::telemetry;
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
        telemetry::init(&boot.settings.log.level);
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
        serve(boot.settings, store, keyring).await
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

fn resolvers(http: &Http) -> Vec<Box<dyn Resolver>> {
    vec![
        Box::new(TwitterResolver::new(http.clone())),
        Box::new(RedditResolver::new(http.clone())),
        Box::new(WebResolver::new(http.clone())),
    ]
}

async fn builder(settings: &Settings, store: SqliteStore) -> Result<EngineBuilder, Error> {
    let engine_config = settings.engine.clone();
    tokio::fs::create_dir_all(&engine_config.cache_dir).await?;
    let ffmpeg = Ffmpeg::provision(&engine_config.cache_dir).await?;
    let store = Arc::new(store);
    let http = Http::new(HttpConfig::default());
    let max_height = engine_config.limits.max_height;
    let mut builder = Engine::builder(engine_config.clone(), store, http.clone())
        .downloader(HttpDownloader::new(http.clone()))
        .downloader(HlsDownloader::new(http.clone(), ffmpeg.clone()))
        .downloader(DashDownloader::new(
            http.clone(),
            ffmpeg.clone(),
            max_height,
        ))
        .transcoder(FfmpegTranscoder::new(ffmpeg));
    for resolver in resolvers(&http) {
        builder = builder.resolver_boxed(resolver);
    }
    builder = builder.archiver(FsArchiver::new(engine_config.archive));
    Ok(builder)
}

/// Cancels `token` once SIGINT or SIGTERM (Unix) or Ctrl-C (Windows) arrives.
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
        });
    }
    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        tokio::spawn(async move {
            ctrl_c.recv().await;
            tracing::info!("shutdown requested");
            token.cancel();
        });
    }
    Ok(())
}

/// Runs the server: the engine and the web app are the process, and end it when they fail.
/// The Discord bot is supervised beside them; its failures show up in the web app, never as
/// an exit.
async fn serve(
    settings: Settings,
    store: SqliteStore,
    keyring: Keyring,
) -> Result<ExitCode, Error> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting discoclip");

    let endpoints = DiscordEndpoints::default();
    let clients: Clients = Clients::default();
    let mut builder = builder(&settings, store.clone()).await?;
    builder = builder
        .publisher(LocalPublisher::new(
            settings.local.dir.clone(),
            settings.local.max_bytes,
        ))
        .publisher(DiscordPublisher::new(clients.clone()));
    let engine = builder.build()?;
    let handle = engine.handle();

    let shutdown = CancellationToken::new();
    cancel_on_signal(shutdown.clone())?;

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
    let app = WebApp::new(
        settings.web.clone(),
        Registry::from_config(&settings.auth),
        settings.auth.oauth_signup,
        Services {
            store,
            keyring,
            bots: bots.clone(),
            rules,
            discord: endpoints,
            engine: handle,
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
