//! Uploads finished videos to Discord, sized to the guild's upload limit.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use discoclip_engine::job::{Job, Origin, SourceId};
use discoclip_engine::media::{LocalFile, safe_stem};
use discoclip_engine::publish::{Constraints, PublishError, Published, Publisher};
use jiff::Timestamp;
use tokio::sync::Mutex;
use twilight_http::Client;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::guild::PremiumTier;
use twilight_model::http::attachment::Attachment;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;
use uuid::Uuid;

use crate::origin::{DiscordOrigin, SOURCE_ID};

const MIB: u64 = 1024 * 1024;
const BASE_LIMIT: u64 = 10 * MIB;
const TIER2_LIMIT: u64 = 50 * MIB;
const TIER3_LIMIT: u64 = 100 * MIB;
const LIMIT_CACHE: Duration = Duration::from_secs(300);

pub fn upload_limit(tier: PremiumTier) -> u64 {
    match tier {
        PremiumTier::Tier2 => TIER2_LIMIT,
        PremiumTier::Tier3 => TIER3_LIMIT,
        _ => BASE_LIMIT,
    }
}

/// The REST clients of the running applications, by the server's id for each; shared with
/// whatever starts and stops the bots.
pub type Clients = Arc<RwLock<HashMap<Uuid, Arc<Client>>>>;

pub struct DiscordPublisher {
    clients: Clients,
    source: SourceId,
    limits: Mutex<HashMap<Id<GuildMarker>, (u64, Instant)>>,
}

impl DiscordPublisher {
    pub fn new(clients: Clients) -> Self {
        Self {
            clients,
            source: SourceId::new(SOURCE_ID),
            limits: Mutex::new(HashMap::new()),
        }
    }

    fn client_for(&self, application: Uuid) -> Result<Arc<Client>, PublishError> {
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&application)
            .cloned()
            .ok_or_else(|| {
                PublishError::Rejected(format!(
                    "discord application {application} is no longer configured"
                ))
            })
    }

    async fn guild_limit(
        &self,
        http: &Client,
        guild: Id<GuildMarker>,
    ) -> Result<u64, PublishError> {
        if let Some((limit, at)) = self.limits.lock().await.get(&guild)
            && at.elapsed() < LIMIT_CACHE
        {
            return Ok(*limit);
        }
        let model = http
            .guild(guild)
            .await
            .map_err(|e| PublishError::Rejected(format!("guild lookup: {e}")))?
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("guild decode: {e}")))?;
        let limit = upload_limit(model.premium_tier);
        self.limits
            .lock()
            .await
            .insert(guild, (limit, Instant::now()));
        Ok(limit)
    }

    fn parse(origin: &Origin) -> Result<DiscordOrigin, PublishError> {
        DiscordOrigin::parse(origin)
            .ok_or_else(|| PublishError::InvalidOrigin(origin.reference.clone()))
    }
}

#[async_trait]
impl Publisher for DiscordPublisher {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, origin: &Origin) -> Result<Constraints, PublishError> {
        let origin = Self::parse(origin)?;
        let http = self.client_for(origin.application)?;
        let limit = match origin.guild {
            Some(guild) => self.guild_limit(&http, guild).await?,
            None => BASE_LIMIT,
        };
        Ok(Constraints::universal(limit))
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let http = self.client_for(origin.application)?;
        let limit = match origin.guild {
            Some(guild) => self.guild_limit(&http, guild).await?,
            None => BASE_LIMIT,
        };
        if file.size > limit {
            return Err(PublishError::TooLarge {
                size: file.size,
                max: limit,
            });
        }
        // The request names where to post, in Discord's terms a channel id, when the rule
        // that picked the link up sends results elsewhere.
        let destination = job
            .request
            .destination
            .as_deref()
            .and_then(|channel| channel.parse::<u64>().ok())
            .and_then(Id::new_checked)
            .unwrap_or(origin.channel);
        let bytes = tokio::fs::read(&file.path).await?;
        let ext = file
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let id = job.id.to_string();
        let filename = format!("{}.{ext}", safe_stem(title, &id[..8]));
        let attachment = Attachment::from_bytes(filename, bytes, 1);
        let attachments = [attachment];
        let allowed = AllowedMentions::default();
        let mut request = http
            .create_message(destination)
            .attachments(&attachments)
            .allowed_mentions(Some(&allowed));
        if destination == origin.channel
            && let Some(message) = origin.message
        {
            request = request.reply(message).fail_if_not_exists(false);
        }
        let response = request.await.map_err(|e| match e.kind() {
            twilight_http::error::ErrorType::Response { status, .. } if status.get() == 413 => {
                PublishError::TooLarge {
                    size: file.size,
                    max: limit,
                }
            }
            _ => PublishError::Rejected(e.to_string()),
        })?;
        let message = response
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("message decode: {e}")))?;
        let posted = DiscordOrigin {
            application: origin.application,
            guild: message.guild_id.or(origin.guild),
            channel: message.channel_id,
            message: Some(message.id),
            author: Some(message.author.id),
        };
        Ok(Published {
            reference: message.id.to_string(),
            url: posted.jump_url(),
            at: Timestamp::now(),
        })
    }
}
