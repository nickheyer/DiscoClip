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
use twilight_http::error::ErrorType;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::guild::PremiumTier;
use twilight_model::http::attachment::Attachment;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;
use uuid::Uuid;

use crate::config::{DiscordSettings, SharedDiscordSettings};
use crate::directory::Directories;
use crate::link::LinkTargets;
use crate::origin::{DiscordOrigin, SOURCE_ID};

const LIMIT_CACHE: Duration = Duration::from_secs(300);
/// How long a REST lookup may wait behind Discord's rate limiter before it is given up.
const REST_WAIT: Duration = Duration::from_secs(10);

/// The REST clients of one running application. `lookups` answers within seconds or is
/// given up: guild details, link posts, replies. `uploads` carries attachments and is
/// given time in proportion to the file. Both share the token's rate limiter.
#[derive(Clone)]
pub struct DiscordClients {
    pub lookups: Arc<Client>,
    pub uploads: Arc<Client>,
}

/// The REST clients of the running applications, by the server's id for each. Shared with
/// whatever starts and stops the bots.
pub type Clients = Arc<RwLock<HashMap<Uuid, DiscordClients>>>;

pub struct DiscordPublisher {
    clients: Clients,
    directories: Directories,
    source: SourceId,
    /// Boost tiers read over REST for guilds the directory had not loaded, for a while.
    tiers: Mutex<HashMap<Id<GuildMarker>, (PremiumTier, Instant)>>,
    links: Arc<dyn LinkTargets>,
    settings: SharedDiscordSettings,
}

impl DiscordPublisher {
    pub fn new(
        clients: Clients,
        directories: Directories,
        links: Arc<dyn LinkTargets>,
        settings: SharedDiscordSettings,
    ) -> Self {
        Self {
            clients,
            directories,
            source: SourceId::new(SOURCE_ID),
            tiers: Mutex::new(HashMap::new()),
            links,
            settings,
        }
    }

    fn settings(&self) -> DiscordSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn clients_for(&self, application: Uuid) -> Result<DiscordClients, PublishError> {
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

    /// The guild's boost tier: from what the gateway told the bot about the guild, or,
    /// for a guild the bot has not loaded, from Discord, remembered for a while.
    async fn guild_tier(
        &self,
        application: Uuid,
        http: &Client,
        guild: Id<GuildMarker>,
    ) -> Result<PremiumTier, PublishError> {
        let known = self
            .directories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&application)
            .and_then(|directory| directory.guild(guild));
        if let Some(info) = known {
            return Ok(info.premium_tier);
        }
        if let Some((tier, at)) = self.tiers.lock().await.get(&guild)
            && at.elapsed() < LIMIT_CACHE
        {
            return Ok(*tier);
        }
        let model = tokio::time::timeout(REST_WAIT, http.guild(guild))
            .await
            .map_err(|_| {
                PublishError::Rejected(format!(
                    "Discord server lookup timed out after {}s. The API may be rate limited.",
                    REST_WAIT.as_secs()
                ))
            })?
            .map_err(|e| PublishError::Rejected(format!("guild lookup: {e}")))?
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("guild decode: {e}")))?;
        self.tiers
            .lock()
            .await
            .insert(guild, (model.premium_tier, Instant::now()));
        Ok(model.premium_tier)
    }

    /// The upload limit where `origin` posts: the server's own override, else its boost
    /// level's, else the base limit for a direct message.
    async fn upload_limit(
        &self,
        origin: &DiscordOrigin,
        http: &Client,
        settings: &DiscordSettings,
    ) -> Result<u64, PublishError> {
        let Some(guild) = origin.guild else {
            return Ok(settings.limit_for(None, PremiumTier::None));
        };
        if settings
            .guilds
            .get(&guild.get().to_string())
            .is_some_and(|over| over.max_bytes.is_some())
        {
            return Ok(settings.limit_for(Some(guild), PremiumTier::None));
        }
        let tier = self.guild_tier(origin.application, http, guild).await?;
        Ok(settings.limit_for(Some(guild), tier))
    }

    fn parse(origin: &Origin) -> Result<DiscordOrigin, PublishError> {
        DiscordOrigin::parse(origin)
            .ok_or_else(|| PublishError::InvalidOrigin(origin.reference.clone()))
    }
}

/// Whether Discord's side failed in a way another attempt may get past: the connection
/// broke before an answer, or the answer was a server error.
fn transient(error: &twilight_http::Error) -> bool {
    match error.kind() {
        ErrorType::RequestError => true,
        ErrorType::Response { status, .. } => status.get() >= 500,
        _ => false,
    }
}

/// How long to wait before attempt `next`, counted from one: two seconds, then four, then
/// eight.
fn backoff(next: u32) -> Duration {
    Duration::from_secs(1 << next.min(4))
}

/// A duration as people read it in an error: `2m 8s`.
fn wait_text(wait: Duration) -> String {
    let secs = wait.as_secs();
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

#[async_trait]
impl Publisher for DiscordPublisher {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let clients = self.clients_for(origin.application)?;
        let settings = self.settings();
        let limit = self
            .upload_limit(&origin, &clients.lookups, &settings)
            .await?;
        let mut constraints = settings.target_for(origin.guild).constraints(limit);
        constraints.fallback = self.links.link_for(job).map(|link| link.fallback);
        Ok(constraints)
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let clients = self.clients_for(origin.application)?;
        let settings = self.settings();
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
            let mut request = clients
                .lookups
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
        let limit = self
            .upload_limit(&origin, &clients.lookups, &settings)
            .await?;
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
        let attachments = [Attachment::from_bytes(filename, bytes, 1)];
        // Each attempt gets the whole budget: a grace for Discord to answer, plus the file
        // at the slowest rate the settings give an upload link credit for. An upload that
        // runs out of time is not sent again, since Discord may have posted it while the
        // answer was on its way.
        let budget = settings.upload.timeout(file.size);
        let attempts = settings.upload.attempts.max(1);
        let mut attempt = 1;
        let response = loop {
            let mut request = clients
                .uploads
                .create_message(destination)
                .attachments(&attachments)
                .allowed_mentions(Some(&allowed));
            if destination == origin.channel
                && let Some(message) = origin.message
            {
                request = request.reply(message).fail_if_not_exists(false);
            }
            match tokio::time::timeout(budget, request).await {
                Ok(Ok(response)) => break response,
                Ok(Err(error)) => {
                    if let ErrorType::Response { status, .. } = error.kind()
                        && status.get() == 413
                    {
                        return Err(PublishError::TooLarge {
                            size: file.size,
                            max: limit,
                        });
                    }
                    if attempt < attempts && transient(&error) {
                        let wait = backoff(attempt);
                        tracing::warn!(
                            job = %job.id,
                            attempt,
                            of = attempts,
                            "discord upload failed, sending again in {}: {error}",
                            wait_text(wait)
                        );
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(PublishError::Rejected(if attempt > 1 {
                        format!("{error} (after {attempt} attempts)")
                    } else {
                        error.to_string()
                    }));
                }
                Err(_) => {
                    return Err(PublishError::Rejected(format!(
                        "the upload of {} bytes did not finish within {}: the connection to \
                         Discord is too slow or has stalled. discord.upload in Settings sets \
                         the time an upload gets.",
                        file.size,
                        wait_text(budget)
                    )));
                }
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_and_waits_read_plainly() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(9), Duration::from_secs(16));
        assert_eq!(wait_text(Duration::from_secs(45)), "45s");
        assert_eq!(wait_text(Duration::from_secs(120)), "2m");
        assert_eq!(wait_text(Duration::from_secs(128)), "2m 8s");
    }
}
