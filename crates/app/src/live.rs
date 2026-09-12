//! Settings taking effect while the server runs. Every part of the server that a setting
//! shapes is reached from here, so a change stored in the app is applied at once: the
//! log filter, the engine, the HTTP client the resolvers use, local publishing, the web
//! listener, the trusted proxies, the public URL and the login providers.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use discoclip_engine::EngineHandle;
use discoclip_engine::ffmpeg::Ffmpeg;
use tokio::net::TcpListener;
use tokio::sync::watch;
use url::Url;

use crate::local::SharedLocalConfig;
use crate::oauth::{OAuthService, Registry};
use crate::settings::{Settings, WebConfig};
use crate::telemetry::LogHandle;
use crate::web::proxy::Proxies;
use crate::web::tls;

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("setting {path}: {message}")]
    Rejected { path: String, message: String },
    #[error("log filter: {0}")]
    Log(String),
    #[error("engine: {0}")]
    Engine(#[from] std::io::Error),
    #[error(transparent)]
    Ffmpeg(#[from] discoclip_engine::ffmpeg::FfmpegError),
}

impl LiveError {
    fn rejected(path: &str, message: impl std::fmt::Display) -> Self {
        Self::Rejected {
            path: path.to_string(),
            message: message.to_string(),
        }
    }
}

/// The running parts of the server that settings reach.
pub struct Live {
    pub log: LogHandle,
    pub engine: EngineHandle,
    pub ffmpeg: Ffmpeg,
    pub local: SharedLocalConfig,
    pub oauth: Arc<OAuthService>,
    pub public_url: Arc<RwLock<Option<Url>>>,
    pub proxies: Arc<RwLock<Proxies>>,
    /// The listener's settings; the web app rebinds when the address or TLS files change.
    pub web: watch::Sender<WebConfig>,
}

impl Live {
    /// The listener's settings as they stand.
    pub fn web_config(&self) -> WebConfig {
        self.web.borrow().clone()
    }

    /// Checks what the types cannot: that a new address can be listened on, that new
    /// certificate files load, and that new directories can be created.
    pub async fn check(&self, current: &Settings, next: &Settings) -> Result<(), LiveError> {
        if next.web.bind != current.web.bind {
            check_bind(next.web.bind).await?;
        }
        if next.web.tls != current.web.tls
            && let Some(config) = &next.web.tls
        {
            tls::Reloading::load(config).map_err(|e| LiveError::rejected("web.tls", e))?;
        }
        if next.engine.cache_dir != current.engine.cache_dir {
            check_dir("engine.cache_dir", &next.engine.cache_dir).await?;
        }
        if next.engine.archive != current.engine.archive
            && let Some(archive) = &next.engine.archive
        {
            check_dir("engine.archive.dir", &archive.dir).await?;
        }
        if next.local.dir != current.local.dir {
            check_dir("local.dir", &next.local.dir).await?;
        }
        Ok(())
    }

    /// Pushes `settings` into every running part.
    pub async fn apply(&self, settings: &Settings) -> Result<(), LiveError> {
        self.log.set(&settings.log.level).map_err(LiveError::Log)?;
        self.engine.reconfigure(settings.engine.clone()).await?;
        if self.ffmpeg.cache_dir() != settings.engine.cache_dir {
            self.ffmpeg.relocate(&settings.engine.cache_dir).await?;
        }
        self.engine.http().configure(settings.http.clone());
        *self.local.write().unwrap_or_else(|e| e.into_inner()) = settings.local.clone();
        self.oauth
            .registry
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .replace_configured(Registry::configured(&settings.auth));
        self.oauth
            .signup
            .store(settings.auth.oauth_signup, Ordering::Relaxed);
        *self.public_url.write().unwrap_or_else(|e| e.into_inner()) =
            settings.web.public_url.clone();
        *self.proxies.write().unwrap_or_else(|e| e.into_inner()) =
            Proxies::new(settings.web.trusted_proxies.clone());
        self.web.send_replace(settings.web.clone());
        Ok(())
    }
}

async fn check_bind(addr: SocketAddr) -> Result<(), LiveError> {
    TcpListener::bind(addr)
        .await
        .map(drop)
        .map_err(|e| LiveError::rejected("web.bind", format!("cannot listen on {addr}: {e}")))
}

async fn check_dir(path: &str, dir: &Path) -> Result<(), LiveError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| LiveError::rejected(path, format!("cannot create {}: {e}", dir.display())))
}
