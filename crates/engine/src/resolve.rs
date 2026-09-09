use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::media::{AudioCodec, Container, VideoCodec};

#[async_trait]
pub trait Resolver: Send + Sync {
    fn id(&self) -> &'static str;
    fn matches(&self, url: &Url) -> bool;
    async fn resolve(&self, url: &Url) -> Result<Resolved, ResolveError>;
}

pub struct ResolverRegistry {
    resolvers: Vec<Box<dyn Resolver>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub resolver: String,
    pub title: Option<String>,
    pub duration: Option<Duration>,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantKind {
    File,
    Hls,
    Dash,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub url: Url,
    pub kind: VariantKind,
    pub container: Option<Container>,
    pub video: Option<VideoCodec>,
    pub audio: Option<AudioCodec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bitrate: Option<u64>,
    pub size: Option<u64>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no resolver handles {0}")]
    Unsupported(Url),
    #[error("nothing found at {0}")]
    NotFound(Url),
    #[error("{url} is unavailable: {reason}")]
    Unavailable { url: Url, reason: String },
    #[error("unexpected response from {url}: {detail}")]
    Malformed { url: Url, detail: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
