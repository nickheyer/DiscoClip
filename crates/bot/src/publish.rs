//! Uploads finished media to Discord, sized to the guild's upload limit.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use discoclip_engine::job::{Delivery, Job, Origin, SourceId};
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

use crate::directory::Directories;
use crate::link::LinkTargets;
use crate::origin::{DiscordOrigin, SOURCE_ID};

const MIB: u64 = 1024 * 1024;
const BASE_LIMIT: u64 = 10 * MIB;
const TIER2_LIMIT: u64 = 50 * MIB;
const TIER3_LIMIT: u64 = 100 * MIB;
const LIMIT_CACHE: Duration = Duration::from_secs(300);
/// How long a REST lookup may wait behind Discord's rate limiter before it is given up.
const REST_WAIT: Duration = Duration::from_secs(10);

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
    directories: Directories,
    source: SourceId,
    /// Limits read over REST for guilds the directory had not loaded, for a while.
    limits: Mutex<HashMap<Id<GuildMarker>, (u64, Instant)>>,
    links: Arc<dyn LinkTargets>,
}

impl DiscordPublisher {
    pub fn new(clients: Clients, directories: Directories, links: Arc<dyn LinkTargets>) -> Self {
        Self {
            clients,
            directories,
            source: SourceId::new(SOURCE_ID),
            limits: Mutex::new(HashMap::new()),
            links,
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

    /// The guild's upload limit: from what the gateway told the bot about the guild, or,
    /// for a guild the bot has not loaded, from Discord, remembered for a while.
    async fn guild_limit(
        &self,
        application: Uuid,
        http: &Client,
        guild: Id<GuildMarker>,
    ) -> Result<u64, PublishError> {
        let known = self
            .directories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&application)
            .and_then(|directory| directory.guild(guild));
        if let Some(info) = known {
            return Ok(upload_limit(info.premium_tier));
        }
        if let Some((limit, at)) = self.limits.lock().await.get(&guild)
            && at.elapsed() < LIMIT_CACHE
        {
            return Ok(*limit);
        }
        let model = tokio::time::timeout(REST_WAIT, http.guild(guild))
            .await
            .map_err(|_| {
                PublishError::Rejected(format!(
                    "Discord did not answer a guild lookup within {}s; its rate limit is likely exhausted",
                    REST_WAIT.as_secs()
                ))
            })?
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

    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let http = self.client_for(origin.application)?;
        let limit = match origin.guild {
            Some(guild) => self.guild_limit(origin.application, &http, guild).await?,
            None => BASE_LIMIT,
        };
        let mut constraints = Constraints::universal(limit);
        constraints.fallback = self.links.link_for(job).map(|link| link.fallback);
        Ok(constraints)
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let http = self.client_for(origin.application)?;
        // The request names where to post, in Discord's terms a channel id, when the rule
        // that picked the link up sends results elsewhere.
        let destination = job
            .request
            .destination
            .as_deref()
            .and_then(|channel| channel.parse::<u64>().ok())
            .and_then(Id::new_checked)
            .unwrap_or(origin.channel);
        let allowed = AllowedMentions::default();
        if job.artifacts.delivery == Delivery::Link {
            let link = self.links.link_for(job).ok_or_else(|| {
                PublishError::Rejected(
                    "the front end that was to show this media is no longer there".into(),
                )
            })?;
            let mut request = http
                .create_message(destination)
                .content(link.page.as_str())
                .allowed_mentions(Some(&allowed));
            if destination == origin.channel
                && let Some(message) = origin.message
            {
                request = request.reply(message).fail_if_not_exists(false);
            }
            let message = request
                .await
                .map_err(|e| PublishError::Rejected(e.to_string()))?
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
            return Ok(Published {
                reference: message.id.to_string(),
                url: posted.jump_url(),
                at: Timestamp::now(),
            });
        }
        let limit = match origin.guild {
            Some(guild) => self.guild_limit(origin.application, &http, guild).await?,
            None => BASE_LIMIT,
        };
        if file.size > limit {
            return Err(PublishError::TooLarge {
                size: file.size,
                max: limit,
            });
        }
        let bytes = tokio::fs::read(&file.path).await?;
        let ext = file
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let id = job.id.to_string();
        let filename = format!(
            "{}.{ext}",
            safe_stem(title, &format!("{}-{}", job.media(), &id[..8]))
        );
        let attachment = Attachment::from_bytes(filename, bytes, 1);
        let attachments = [attachment];
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
