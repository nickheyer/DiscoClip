use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::archive::Archiver;
use crate::config::EngineConfig;
use crate::download::Downloader;
use crate::event::EngineEvent;
use crate::job::{Request, SourceId};
use crate::publish::Publisher;
use crate::resolve::ResolverRegistry;
use crate::store::{JobStore, StoreError};
use crate::transcode::Transcoder;

pub struct Engine {
    config: EngineConfig,
    resolvers: Arc<ResolverRegistry>,
    downloaders: Vec<Arc<dyn Downloader>>,
    transcoder: Arc<dyn Transcoder>,
    publishers: HashMap<SourceId, Arc<dyn Publisher>>,
    archiver: Option<Arc<dyn Archiver>>,
    store: Arc<dyn JobStore>,
    events: broadcast::Sender<EngineEvent>,
    queue: mpsc::Receiver<Request>,
}

#[derive(Clone)]
pub struct EngineHandle {
    resolvers: Arc<ResolverRegistry>,
    store: Arc<dyn JobStore>,
    submit: mpsc::Sender<Request>,
    events: broadcast::Sender<EngineEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("no resolver handles {0}")]
    Unsupported(Url),
    #[error("engine is not running")]
    Closed,
    #[error(transparent)]
    Store(#[from] StoreError),
}
