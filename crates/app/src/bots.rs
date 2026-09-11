//! One supervised bot per Discord application, started when the server starts and when an
//! application is added, and stopped when it is removed.

use std::collections::HashMap;
use std::sync::Arc;

use discoclip_bot::{
    BotControl, BotRuntime, BotStatus, Clients, ControlError, DiscordConfig, DiscordEndpoints,
    GuildEvent, http_client, supervise,
};
use discoclip_engine::EngineHandle;
use serde::Serialize;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::applications::{Application, ApplicationId};
use crate::discord::{BotGuildStore, JoinedGuild};
use crate::rules::RuleCache;

struct Running {
    control: BotControl,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

/// A bot's status, as it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotEvent {
    pub application: ApplicationId,
    #[serde(flatten)]
    pub status: BotStatus,
}

pub struct BotManager {
    engine: EngineHandle,
    endpoints: DiscordEndpoints,
    /// Shared with the Discord publisher, which posts through the same clients.
    clients: Clients,
    guilds: BotGuildStore,
    rules: RuleCache,
    shutdown: CancellationToken,
    bots: Mutex<HashMap<ApplicationId, Running>>,
    events: broadcast::Sender<BotEvent>,
}

impl BotManager {
    pub fn new(
        engine: EngineHandle,
        endpoints: DiscordEndpoints,
        clients: Clients,
        guilds: BotGuildStore,
        rules: RuleCache,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            engine,
            endpoints,
            clients,
            guilds,
            rules,
            shutdown,
            bots: Mutex::new(HashMap::new()),
            events: broadcast::channel(256).0,
        }
    }

    /// Every status change of every bot, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<BotEvent> {
        self.events.subscribe()
    }

    /// The status of every bot now, by application.
    pub async fn snapshot(&self) -> Vec<BotEvent> {
        let mut all: Vec<BotEvent> = self
            .bots
            .lock()
            .await
            .iter()
            .map(|(id, running)| BotEvent {
                application: *id,
                status: running.control.status(),
            })
            .collect();
        all.sort_by_key(|event| event.application.0);
        all
    }

    async fn control(&self, id: ApplicationId) -> Result<BotControl, ControlError> {
        self.bots
            .lock()
            .await
            .get(&id)
            .map(|running| running.control.clone())
            .ok_or(ControlError::Gone)
    }

    /// Starts a stopped bot; returns once the supervisor has taken the command.
    pub async fn start(&self, id: ApplicationId) -> Result<(), ControlError> {
        self.control(id).await?.start().await
    }

    /// Stops a running bot; returns once it is down.
    pub async fn stop(&self, id: ApplicationId) -> Result<(), ControlError> {
        self.control(id).await?.stop().await
    }

    /// Stops and starts a bot; returns once it is starting again.
    pub async fn restart(&self, id: ApplicationId) -> Result<(), ControlError> {
        let control = self.control(id).await?;
        control.stop().await?;
        control.start().await
    }

    /// The REST client of a running application's bot.
    pub fn client(&self, id: ApplicationId) -> Option<Arc<twilight_http::Client>> {
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.0)
            .cloned()
    }

    /// Supervises the bot of `application` with `bot_token`, replacing one already
    /// running; it starts at once unless the application is disabled.
    pub async fn launch(&self, application: &Application, bot_token: &str) {
        let mut bots = self.bots.lock().await;
        if let Some(previous) = bots.remove(&application.id) {
            Self::end(previous).await;
        }
        let http = http_client(bot_token, &self.endpoints);
        self.clients
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(application.id.0, http.clone());
        let stop = self.shutdown.child_token();
        let config = DiscordConfig {
            token: bot_token.into(),
        };
        let (events, inbox) = mpsc::channel(64);
        tokio::spawn(track_guilds(self.guilds.clone(), application.id, inbox));
        let (control, task) = supervise(
            BotRuntime {
                application: application.id.0,
                config,
                http,
                engine: self.engine.clone(),
                endpoints: self.endpoints.clone(),
                guild_events: events,
                rules: Arc::new(self.rules.clone()),
            },
            application.enabled,
            stop.clone(),
        );
        tokio::spawn(forward_status(
            application.id,
            control.subscribe(),
            self.events.clone(),
        ));
        tracing::info!(application = %application.id, name = application.name, enabled = application.enabled, "discord bot launched");
        bots.insert(
            application.id,
            Running {
                control,
                stop,
                task,
            },
        );
    }

    /// Stops and forgets the bot of `id`; whether there was one.
    pub async fn retire(&self, id: ApplicationId) -> bool {
        let running = self.bots.lock().await.remove(&id);
        self.clients
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id.0);
        match running {
            Some(running) => {
                Self::end(running).await;
                tracing::info!(application = %id, "discord bot retired");
                true
            }
            None => false,
        }
    }

    async fn end(running: Running) {
        running.stop.cancel();
        if let Err(error) = running.task.await {
            tracing::error!("bot supervisor task failed: {error}");
        }
    }

    pub async fn status(&self, id: ApplicationId) -> Option<BotStatus> {
        self.bots
            .lock()
            .await
            .get(&id)
            .map(|running| running.control.status())
    }

    pub async fn statuses(&self) -> HashMap<ApplicationId, BotStatus> {
        self.bots
            .lock()
            .await
            .iter()
            .map(|(id, running)| (*id, running.control.status()))
            .collect()
    }

    /// Stops every bot; for shutdown.
    pub async fn stop_all(&self) {
        let bots: Vec<Running> = self.bots.lock().await.drain().map(|(_, r)| r).collect();
        for running in bots {
            Self::end(running).await;
        }
        self.clients
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// Keeps the guild ledger of `application` up with what its bot reports, until the bot is
/// gone.
async fn track_guilds(
    store: BotGuildStore,
    application: ApplicationId,
    mut inbox: mpsc::Receiver<GuildEvent>,
) {
    while let Some(event) = inbox.recv().await {
        let outcome = match event {
            GuildEvent::Ready { guilds } => {
                let present = guilds.iter().map(|id| id.to_string()).collect();
                store.reconcile(application, present).await.map(|gone| {
                    for guild in gone {
                        tracing::info!(%application, guild, "bot was removed from a guild while away");
                    }
                })
            }
            GuildEvent::Joined {
                guild,
                name,
                icon,
                member_count,
            } => store
                .joined(
                    application,
                    JoinedGuild {
                        guild_id: guild.to_string(),
                        name,
                        icon,
                        member_count,
                    },
                )
                .await
                .map(|stored| {
                    if stored.joined_at == stored.updated_at {
                        tracing::info!(%application, guild = stored.guild_id, name = stored.name, "bot joined a guild");
                    }
                }),
            GuildEvent::Left { guild } => store.left(application, &guild.to_string()).await.map(|was| {
                if was {
                    tracing::info!(%application, guild = %guild, "bot left a guild");
                }
            }),
        };
        if let Err(error) = outcome {
            tracing::error!(%application, "could not record a guild event: {error}");
        }
    }
}

/// Turns one bot's status changes into events, until its supervisor is gone.
async fn forward_status(
    application: ApplicationId,
    mut status: tokio::sync::watch::Receiver<BotStatus>,
    events: broadcast::Sender<BotEvent>,
) {
    loop {
        let _ = events.send(BotEvent {
            application,
            status: status.borrow_and_update().clone(),
        });
        if status.changed().await.is_err() {
            return;
        }
    }
}
