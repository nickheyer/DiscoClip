//! Gateway connection, event dispatch, and slash command handling.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use discoclip_engine::job::{JobId, JobStatus, Request};
use discoclip_engine::{EngineHandle, EventKind};
use futures::StreamExt;
use secrecy::ExposeSecret;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use twilight_gateway::{CloseFrame, ConfigBuilder, Event, EventTypeFlags, Intents, Message, Shard};
use twilight_http::Client;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::MessageFlags;
use twilight_model::gateway::CloseCode;
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, GuildMarker, MessageMarker};
use twilight_util::builder::InteractionResponseDataBuilder;
use uuid::Uuid;

use crate::commands::{self, Invocation};
use crate::config::{DiscordConfig, DiscordEndpoints};
use crate::origin::DiscordOrigin;
use crate::watch::{RuleSource, Watcher};

const WANTED: EventTypeFlags = EventTypeFlags::READY
    .union(EventTypeFlags::MESSAGE_CREATE)
    .union(EventTypeFlags::INTERACTION_CREATE)
    .union(EventTypeFlags::GUILD_CREATE)
    .union(EventTypeFlags::GUILD_DELETE);
const CLOSE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error("discord api: {0}")]
    Http(#[from] twilight_http::Error),
    #[error("discord response: {0}")]
    Decode(#[from] twilight_http::response::DeserializeBodyError),
    #[error("gateway: {0}")]
    Gateway(#[from] twilight_gateway::error::StartRecommendedError),
    /// Discord closed the gateway with a code the shard will not reconnect from.
    #[error("gateway closed with code {code}: {reason}")]
    Closed { code: u16, reason: String },
    #[error("gateway connection ended")]
    Disconnected,
}

/// Called with the bot user's name once the gateway reports it logged in.
pub type Connected = Arc<dyn Fn(String) + Send + Sync>;

/// What the gateway tells the bot about the guilds it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuildEvent {
    /// The guilds the bot is in at login; each follows with [`GuildEvent::Joined`].
    Ready { guilds: Vec<Id<GuildMarker>> },
    /// The bot is in this guild: at login, or just added.
    Joined {
        guild: Id<GuildMarker>,
        name: String,
        icon: Option<String>,
        member_count: Option<u64>,
    },
    /// The bot was removed from this guild.
    Left { guild: Id<GuildMarker> },
}

/// A REST client for `token`, sent to Discord or to the stand-in `endpoints` name.
pub fn http_client(token: &str, endpoints: &DiscordEndpoints) -> Arc<Client> {
    let mut builder = Client::builder().token(token.to_string());
    if let Some(proxy) = &endpoints.http_proxy {
        builder = builder.proxy(proxy.clone(), true);
    }
    Arc::new(builder.build())
}

pub struct Bot {
    application: Uuid,
    config: DiscordConfig,
    http: Arc<Client>,
    engine: EngineHandle,
    endpoints: DiscordEndpoints,
    guild_events: mpsc::Sender<GuildEvent>,
    rules: Arc<dyn RuleSource>,
}

struct Shared {
    http: Arc<Client>,
    engine: EngineHandle,
    watcher: Watcher,
    application: Id<ApplicationMarker>,
    /// The server's own id for the application.
    local_application: Uuid,
    guild_events: mpsc::Sender<GuildEvent>,
    /// Jobs started by `/clip`, keyed to the acknowledgement message they should update.
    tracked: Mutex<HashMap<JobId, Acknowledgement>>,
    seen_shards: Mutex<HashSet<u32>>,
    connected: Connected,
}

/// The reply to a `/clip` command, edited once its job finishes.
struct Acknowledgement {
    channel: Id<ChannelMarker>,
    message: Id<MessageMarker>,
    url: String,
}

impl Bot {
    /// `application` is the server's own id for the Discord application `config` belongs to;
    /// it is stamped on every request the bot submits.
    pub fn new(
        application: Uuid,
        config: DiscordConfig,
        http: Arc<Client>,
        engine: EngineHandle,
        endpoints: DiscordEndpoints,
        guild_events: mpsc::Sender<GuildEvent>,
        rules: Arc<dyn RuleSource>,
    ) -> Self {
        Self {
            application,
            config,
            http,
            engine,
            endpoints,
            guild_events,
            rules,
        }
    }

    /// Runs until `shutdown` is cancelled, or returns the error that stopped the bot.
    pub async fn run(
        self,
        shutdown: CancellationToken,
        connected: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<(), BotError> {
        let application = self.http.current_user_application().await?.model().await?;
        tracing::info!(application = %application.id, name = %application.name, "discord application ready");

        let shared = Arc::new(Shared {
            http: self.http.clone(),
            engine: self.engine.clone(),
            watcher: Watcher::new(self.application, self.rules.clone()),
            application: application.id,
            local_application: self.application,
            guild_events: self.guild_events.clone(),
            tracked: Mutex::new(HashMap::new()),
            seen_shards: Mutex::new(HashSet::new()),
            connected: Arc::new(connected),
        });

        let intents = Intents::GUILDS
            | Intents::GUILD_MESSAGES
            | Intents::DIRECT_MESSAGES
            | Intents::MESSAGE_CONTENT;
        let mut gateway =
            ConfigBuilder::new(self.config.token.expose_secret().to_string(), intents);
        if let Some(url) = &self.endpoints.gateway_url {
            gateway = gateway.proxy_url(url.clone());
        }
        let gateway = gateway.build();
        let shards =
            twilight_gateway::create_recommended(&self.http, gateway, |_, builder| builder.build())
                .await?;

        // One shard failing for good takes the others down with it, so the bot as a whole
        // reports a single outcome.
        let stop = shutdown.child_token();
        let mut tasks: JoinSet<Result<(), BotError>> = JoinSet::new();
        for shard in shards {
            tasks.spawn(shard_loop(shard, shared.clone(), stop.clone()));
        }
        let follow_token = stop.clone();
        tasks.spawn(async move {
            follow_jobs(shared, follow_token).await;
            Ok(())
        });
        let mut outcome = Ok(());
        while let Some(result) = tasks.join_next().await {
            let failed = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(error) => {
                    tracing::error!("bot task panicked: {error}");
                    BotError::Disconnected
                }
            };
            if outcome.is_ok() {
                outcome = Err(failed);
                stop.cancel();
            }
        }
        outcome
    }
}

async fn shard_loop(
    mut shard: Shard,
    shared: Arc<Shared>,
    shutdown: CancellationToken,
) -> Result<(), BotError> {
    let id = shard.id();
    loop {
        let item = tokio::select! {
            _ = shutdown.cancelled() => {
                shard.close(CloseFrame::NORMAL);
                // Discord answers the close frame with its own; a dropped connection ends
                // the drain just as well, since polling on would reconnect.
                let drain = async {
                    while let Some(item) = shard.next().await {
                        if matches!(item, Ok(Message::Close(_)) | Err(_)) {
                            break;
                        }
                    }
                };
                let _ = tokio::time::timeout(CLOSE_GRACE, drain).await;
                tracing::info!(shard = %id, "gateway closed");
                return Ok(());
            }
            item = shard.next() => item,
        };
        let message = match item {
            Some(Ok(message)) => message,
            Some(Err(error)) => {
                tracing::warn!(shard = %id, "gateway error: {error}");
                continue;
            }
            None => {
                tracing::warn!(shard = %id, "gateway stream ended");
                return Err(BotError::Disconnected);
            }
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(frame) => {
                let fatal = frame.as_ref().and_then(|frame| {
                    CloseCode::try_from(frame.code)
                        .ok()
                        .filter(|code| !code.can_reconnect())
                        .map(|_| (frame.code, frame.reason.to_string()))
                });
                if let Some((code, reason)) = fatal {
                    return Err(BotError::Closed { code, reason });
                }
                tracing::info!(shard = %id, ?frame, "gateway connection closed, reconnecting");
                continue;
            }
        };
        let event = match twilight_gateway::parse(text, WANTED) {
            Ok(Some(event)) => Event::from(event),
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(shard = %id, "could not parse gateway event: {error}");
                continue;
            }
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            handle_event(&shared, event, id.number()).await;
        });
    }
}

async fn handle_event(shared: &Shared, event: Event, shard: u32) {
    match event {
        Event::Ready(ready) => {
            let first = shared.seen_shards.lock().expect("shard set").insert(shard);
            if first {
                tracing::info!(shard, user = %ready.user.name, guilds = ready.guilds.len(), "connected to discord");
            }
            (shared.connected)(ready.user.name.clone());
            shared
                .report(GuildEvent::Ready {
                    guilds: ready.guilds.iter().map(|g| g.id).collect(),
                })
                .await;
        }
        Event::GuildCreate(created) => {
            let joined = match *created {
                GuildCreate::Available(guild) => Some(GuildEvent::Joined {
                    guild: guild.id,
                    name: guild.name,
                    icon: guild.icon.map(|hash| hash.to_string()),
                    member_count: guild.member_count,
                }),
                // An outage, not a join: Discord says the guild is unavailable.
                GuildCreate::Unavailable(guild) if guild.unavailable => None,
                // A guild with nothing but an id; its details are a request away.
                GuildCreate::Unavailable(guild) => {
                    match fetch_guild(&shared.http, guild.id).await {
                        Ok(event) => Some(event),
                        Err(error) => {
                            tracing::warn!(guild = %guild.id, "could not look up a joined guild: {error}");
                            None
                        }
                    }
                }
            };
            if let Some(event) = joined {
                shared.report(event).await;
            }
        }
        Event::GuildDelete(deleted) => {
            if deleted.unavailable == Some(true) {
                tracing::info!(guild = %deleted.id, "guild unavailable");
            } else {
                shared.report(GuildEvent::Left { guild: deleted.id }).await;
            }
        }
        Event::MessageCreate(message) => {
            for request in shared.watcher.requests(&message) {
                let url = request.url.clone();
                if !shared.engine.supports(&url) {
                    continue;
                }
                match shared.engine.submit(request).await {
                    Ok(id) => {
                        tracing::info!(job = %id, %url, channel = %message.channel_id, "queued from watched channel")
                    }
                    Err(error) => tracing::warn!(%url, "could not queue link: {error}"),
                }
            }
        }
        Event::InteractionCreate(interaction) => {
            handle_interaction(shared, interaction.0).await;
        }
        _ => {}
    }
}

impl Shared {
    async fn report(&self, event: GuildEvent) {
        if self.guild_events.send(event).await.is_err() {
            tracing::debug!("nothing is listening for guild events");
        }
    }
}

async fn fetch_guild(http: &Client, id: Id<GuildMarker>) -> Result<GuildEvent, BotError> {
    let guild = http.guild(id).await?.model().await?;
    Ok(GuildEvent::Joined {
        guild: guild.id,
        name: guild.name,
        icon: guild.icon.map(|hash| hash.to_string()),
        member_count: guild.member_count,
    })
}

async fn respond(
    shared: &Shared,
    interaction: &Interaction,
    content: String,
    ephemeral: bool,
) -> Result<(), BotError> {
    let mut data = InteractionResponseDataBuilder::new().content(content);
    if ephemeral {
        data = data.flags(MessageFlags::EPHEMERAL);
    }
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(data.build()),
    };
    shared
        .http
        .interaction(shared.application)
        .create_response(interaction.id, &interaction.token, &response)
        .await?;
    Ok(())
}

async fn handle_interaction(shared: &Shared, interaction: Interaction) {
    if interaction.kind != InteractionType::ApplicationCommand {
        return;
    }
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return;
    };
    let result = match commands::parse(data) {
        Invocation::Clip { url: Err(reason) } => respond(shared, &interaction, reason, true).await,
        Invocation::Clip { url: Ok(url) } => clip(shared, &interaction, url).await,
        Invocation::Status => status(shared, &interaction).await,
        Invocation::Unknown(name) => {
            respond(
                shared,
                &interaction,
                format!("Unknown command `{name}`"),
                true,
            )
            .await
        }
    };
    if let Err(error) = result {
        tracing::warn!(command = %data.name, "interaction failed: {error}");
    }
}

async fn clip(shared: &Shared, interaction: &Interaction, url: url::Url) -> Result<(), BotError> {
    if !shared.engine.supports(&url) {
        return respond(
            shared,
            interaction,
            format!("No resolver handles {url}"),
            true,
        )
        .await;
    }
    let Some(channel) = interaction.channel.as_ref().map(|c| c.id) else {
        return respond(
            shared,
            interaction,
            "This command needs a channel".into(),
            true,
        )
        .await;
    };
    respond(shared, interaction, format!("Queued {url}"), false).await?;
    let message = shared
        .http
        .interaction(shared.application)
        .response(&interaction.token)
        .await?
        .model()
        .await?;
    let origin = DiscordOrigin {
        application: shared.local_application,
        guild: interaction.guild_id,
        channel,
        message: Some(message.id),
        author: interaction.author().map(|u| u.id),
    }
    .to_origin();
    let mut request = Request::new(origin, url.clone());
    request.submitted_by = interaction.author().map(|u| format!("discord:{}", u.id));
    match shared.engine.submit(request).await {
        Ok(id) => {
            tracing::info!(job = %id, %url, "queued from /clip");
            shared.tracked.lock().expect("tracked jobs").insert(
                id,
                Acknowledgement {
                    channel,
                    message: message.id,
                    url: url.to_string(),
                },
            );
        }
        Err(error) => {
            let text = format!("Could not queue {url}: {error}");
            shared
                .http
                .interaction(shared.application)
                .update_response(&interaction.token)
                .content(Some(&text))
                .await?;
        }
    }
    Ok(())
}

async fn status(shared: &Shared, interaction: &Interaction) -> Result<(), BotError> {
    let text = match shared.engine.stats().await {
        Ok(stats) => format!(
            "DiscoClip {}: {} queued, {} running, {} done, {} failed, {} cancelled",
            env!("CARGO_PKG_VERSION"),
            stats.queued,
            stats.running,
            stats.done,
            stats.failed,
            stats.cancelled
        ),
        Err(error) => format!("Could not read job stats: {error}"),
    };
    respond(shared, interaction, text, true).await
}

/// Edits the `/clip` acknowledgement once its job finishes.
async fn follow_jobs(shared: Arc<Shared>, shutdown: CancellationToken) {
    let mut events = shared.engine.subscribe();
    loop {
        let event = tokio::select! {
            _ = shutdown.cancelled() => return,
            event = events.recv() => event,
        };
        let event = match event {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "engine event stream lagged");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let EventKind::Status { status } = event.kind else {
            continue;
        };
        if !status.is_terminal() {
            continue;
        }
        let tracked = shared
            .tracked
            .lock()
            .expect("tracked jobs")
            .remove(&event.job);
        let Some(Acknowledgement {
            channel,
            message,
            url,
        }) = tracked
        else {
            continue;
        };
        let text = match status {
            JobStatus::Done => format!("Clipped {url}"),
            JobStatus::Failed { stage, message } => {
                format!("Could not clip {url}: {stage} failed: {message}")
            }
            JobStatus::Cancelled => format!("Cancelled {url}"),
            JobStatus::Queued | JobStatus::Running { .. } => continue,
        };
        if let Err(error) = shared
            .http
            .update_message(channel, message)
            .content(Some(&text))
            .await
        {
            tracing::warn!(job = %event.job, "could not update acknowledgement: {error}");
        }
    }
}
