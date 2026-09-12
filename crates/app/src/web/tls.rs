//! Serving HTTPS: the certificate and key are read from PEM files, offered to every
//! connection, and read again when the files change on disk, so a renewed certificate
//! takes effect without a restart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use axum::Extension;
use axum::Router;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::proxy::Tls;
use crate::settings::TlsConfig;

/// How often the files are looked at for a change.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);
/// How long open connections get to finish once shutdown begins.
const DRAIN: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("could not read {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("{path} holds no certificate")]
    NoCertificate { path: PathBuf },
    #[error("the key in {path} cannot be used: {message}")]
    Key { path: PathBuf, message: String },
    #[error("tls configuration: {0}")]
    Config(String),
}

/// The provider every TLS connection is built on.
fn provider() -> Arc<CryptoProvider> {
    if let Some(provider) = CryptoProvider::get_default() {
        return provider.clone();
    }
    // Another library may install its own first; either way one is in place after this.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_key(config: &TlsConfig, provider: &CryptoProvider) -> Result<CertifiedKey, TlsError> {
    let read = |path: &Path, e: &dyn std::fmt::Display| TlsError::Read {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&config.cert)
        .map_err(|e| read(&config.cert, &e))?
        .collect::<Result<_, _>>()
        .map_err(|e| read(&config.cert, &e))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate {
            path: config.cert.clone(),
        });
    }
    let key = PrivateKeyDer::from_pem_file(&config.key).map_err(|e| read(&config.key, &e))?;
    CertifiedKey::from_der(certs, key, provider).map_err(|e| TlsError::Key {
        path: config.key.clone(),
        message: e.to_string(),
    })
}

/// The certificate every connection is offered, replaced when the files change.
pub struct Reloading {
    config: TlsConfig,
    provider: Arc<CryptoProvider>,
    key: RwLock<Arc<CertifiedKey>>,
    stamps: Mutex<(Option<SystemTime>, Option<SystemTime>)>,
}

impl std::fmt::Debug for Reloading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloading")
            .field("cert", &self.config.cert)
            .field("key", &self.config.key)
            .finish()
    }
}

impl Reloading {
    pub fn load(config: &TlsConfig) -> Result<Arc<Self>, TlsError> {
        let provider = provider();
        let key = read_key(config, &provider)?;
        Ok(Arc::new(Self {
            config: config.clone(),
            provider,
            key: RwLock::new(Arc::new(key)),
            stamps: Mutex::new((modified(&config.cert), modified(&config.key))),
        }))
    }

    /// Reads the files again when either has changed; whether the certificate changed.
    pub fn reload_if_changed(&self) -> Result<bool, TlsError> {
        let now = (modified(&self.config.cert), modified(&self.config.key));
        {
            let stamps = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
            if *stamps == now {
                return Ok(false);
            }
        }
        let key = read_key(&self.config, &self.provider)?;
        *self.key.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(key);
        *self.stamps.lock().unwrap_or_else(|e| e.into_inner()) = now;
        Ok(true)
    }

    pub fn server_config(self: &Arc<Self>) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        let mut config = rustls::ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| TlsError::Config(e.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }

    /// Looks at the files every [`RELOAD_INTERVAL`] until `shutdown`.
    pub async fn watch(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(RELOAD_INTERVAL) => {}
            }
            match self.reload_if_changed() {
                Ok(true) => {
                    tracing::info!(cert = %self.config.cert.display(), "tls certificate reloaded")
                }
                Ok(false) => {}
                Err(error) => tracing::error!("tls certificate not reloaded: {error}"),
            }
        }
    }
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.key.read().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

/// Accepts connections on `listener`, speaks TLS on each, and serves `router` over them
/// until `shutdown`, then lets open connections finish for a while.
pub async fn serve(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    router: Router,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(config);
    let graceful = GracefulShutdown::new();
    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let acceptor = acceptor.clone();
        let app = router
            .clone()
            .layer(Extension(ConnectInfo::<SocketAddr>(peer)))
            .layer(Extension(Tls));
        let service = TowerToHyperService::new(app);
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(tls) => tls,
                Err(error) => {
                    tracing::debug!(%peer, "tls handshake failed: {error}");
                    return;
                }
            };
            let builder = Builder::new(TokioExecutor::new());
            let connection = builder
                .serve_connection_with_upgrades(TokioIo::new(tls), service)
                .into_owned();
            if let Err(error) = watcher.watch(connection).await {
                tracing::debug!(%peer, "connection ended: {error}");
            }
        });
    }
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(DRAIN) => tracing::warn!("some https connections were still open at shutdown"),
    }
    Ok(())
}
