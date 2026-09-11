//! Keeps the Discord bot running alongside the web app. The bot starts by itself when a
//! token is configured, restarts after transient failures, stays down with a visible reason
//! when Discord rejects the token, and takes start and stop requests from the UI.

use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;

use discoclip_engine::EngineHandle;
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;
use twilight_http::error::ErrorType;
use twilight_model::gateway::CloseCode;
use uuid::Uuid;

use crate::client::{Bot, BotError, GuildEvent};
use crate::config::{DiscordConfig, DiscordEndpoints};
use crate::watch::RuleSource;

const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BotState {
    /// No token is configured, so there is nothing to run.
    Disabled,
    /// Turned off from the UI.
    Stopped,
    Starting,
    Connected {
        user: String,
    },
    /// A transient failure; the bot restarts by itself at `next_attempt_at`.
    Retrying {
        error: String,
        attempt: u32,
        next_attempt_at: Timestamp,
    },
    /// Discord rejected the token or the intents; the bot stays down until started again.
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotStatus {
    #[serde(flatten)]
    pub state: BotState,
    pub since: Timestamp,
}

impl BotStatus {
    fn new(state: BotState) -> Self {
        Self {
            state,
            since: Timestamp::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotCommand {
    Start,
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("no discord token is configured")]
    Disabled,
    #[error("the discord bot supervisor is not running")]
    Gone,
}

/// The UI's handle on the bot: current status, status changes, and start/stop.
type Command = (BotCommand, oneshot::Sender<()>);

#[derive(Clone)]
pub struct BotControl {
    status: watch::Receiver<BotStatus>,
    commands: Option<mpsc::Sender<Command>>,
}

impl BotControl {
    /// The control for a server without a token: the status stays `Disabled` and start and
    /// stop are refused.
    pub fn disabled() -> Self {
        let (sender, status) = watch::channel(BotStatus::new(BotState::Disabled));
        drop(sender);
        Self {
            status,
            commands: None,
        }
    }

    pub fn status(&self) -> BotStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<BotStatus> {
        self.status.clone()
    }

    pub async fn start(&self) -> Result<(), ControlError> {
        self.send(BotCommand::Start).await
    }

    pub async fn stop(&self) -> Result<(), ControlError> {
        self.send(BotCommand::Stop).await
    }

    /// Sends a command and returns once the supervisor has acted on it, so the status read
    /// afterwards reflects the command.
    async fn send(&self, command: BotCommand) -> Result<(), ControlError> {
        let commands = self.commands.as_ref().ok_or(ControlError::Disabled)?;
        let (done, acked) = oneshot::channel();
        commands
            .send((command, done))
            .await
            .map_err(|_| ControlError::Gone)?;
        acked.await.map_err(|_| ControlError::Gone)
    }
}

/// Publishes status for the supervisor and lets the bot report the moment it connects.
pub struct StatusHandle {
    sender: watch::Sender<BotStatus>,
}

impl StatusHandle {
    fn set(&self, state: BotState) {
        self.sender.send_replace(BotStatus::new(state));
    }

    fn current(&self) -> BotState {
        self.sender.borrow().state.clone()
    }

    /// Records a successful gateway login, unless the bot was stopped in the meantime.
    pub fn connected(&self, user: String) {
        self.sender.send_if_modified(|status| {
            if matches!(
                status.state,
                BotState::Starting | BotState::Connected { .. }
            ) {
                *status = BotStatus::new(BotState::Connected { user });
                true
            } else {
                false
            }
        });
    }
}

/// What every start of a bot is made from.
pub struct BotRuntime {
    /// The server's own id for the Discord application.
    pub application: Uuid,
    pub config: DiscordConfig,
    pub http: Arc<Client>,
    pub engine: EngineHandle,
    pub endpoints: DiscordEndpoints,
    pub guild_events: mpsc::Sender<GuildEvent>,
    pub rules: Arc<dyn RuleSource>,
}

/// Keeps the bot supervised until `shutdown` is cancelled, starting it at once when
/// `start` is set and waiting for a start command otherwise.
pub fn supervise(
    runtime: BotRuntime,
    start: bool,
    shutdown: CancellationToken,
) -> (BotControl, JoinHandle<()>) {
    let initial = if start {
        BotState::Starting
    } else {
        BotState::Stopped
    };
    let (sender, status) = watch::channel(BotStatus::new(initial));
    let (commands, inbox) = mpsc::channel(8);
    let handle = Arc::new(StatusHandle { sender });
    let task = tokio::spawn(run(runtime, start, handle, inbox, shutdown));
    (
        BotControl {
            status,
            commands: Some(commands),
        },
        task,
    )
}

enum Outcome {
    Stopped(oneshot::Sender<()>),
    Ended(Result<(), BotError>),
}

fn ack(done: oneshot::Sender<()>) {
    let _ = done.send(());
}

async fn run(
    runtime: BotRuntime,
    start: bool,
    status: Arc<StatusHandle>,
    mut commands: mpsc::Receiver<Command>,
    shutdown: CancellationToken,
) {
    let mut enabled = start;
    let mut attempt: u32 = 0;
    loop {
        if !enabled {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                command = commands.recv() => {
                    let Some((command, done)) = command else { return };
                    match command {
                        BotCommand::Start => {
                            enabled = true;
                            attempt = 0;
                        }
                        BotCommand::Stop => {
                            if status.current() != BotState::Stopped {
                                status.set(BotState::Stopped);
                            }
                        }
                    }
                    ack(done);
                }
            }
            continue;
        }

        status.set(BotState::Starting);
        let run_token = shutdown.child_token();
        let bot = Bot::new(
            runtime.application,
            runtime.config.clone(),
            runtime.http.clone(),
            runtime.engine.clone(),
            runtime.endpoints.clone(),
            runtime.guild_events.clone(),
            runtime.rules.clone(),
        );
        let reporter = status.clone();
        let mut running =
            Box::pin(bot.run(run_token.clone(), move |user| reporter.connected(user)));
        let outcome = loop {
            tokio::select! {
                result = &mut running => break Outcome::Ended(result),
                command = commands.recv() => {
                    let Some((command, done)) = command else {
                        run_token.cancel();
                        let _ = (&mut running).await;
                        return;
                    };
                    match command {
                        BotCommand::Stop => {
                            run_token.cancel();
                            let _ = (&mut running).await;
                            break Outcome::Stopped(done);
                        }
                        BotCommand::Start => ack(done),
                    }
                }
            }
        };
        if shutdown.is_cancelled() {
            return;
        }
        if matches!(status.current(), BotState::Connected { .. }) {
            attempt = 0;
        }
        match outcome {
            Outcome::Stopped(done) => {
                enabled = false;
                status.set(BotState::Stopped);
                tracing::info!("discord bot stopped");
                ack(done);
            }
            Outcome::Ended(result) => {
                let error = match result {
                    Ok(()) => BotError::Disconnected,
                    Err(error) => error,
                };
                if is_permanent(&error) {
                    enabled = false;
                    tracing::error!("discord bot cannot run: {error}");
                    status.set(BotState::Failed {
                        error: error.to_string(),
                    });
                    continue;
                }
                attempt += 1;
                let delay = backoff(attempt);
                tracing::warn!(attempt, retry_in = ?delay, "discord bot failed: {error}");
                status.set(BotState::Retrying {
                    error: error.to_string(),
                    attempt,
                    next_attempt_at: Timestamp::now()
                        + SignedDuration::try_from(delay).unwrap_or_default(),
                });
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.cancelled() => return,
                    command = commands.recv() => {
                        let Some((command, done)) = command else { return };
                        match command {
                            BotCommand::Start => attempt = 0,
                            BotCommand::Stop => {
                                enabled = false;
                                status.set(BotState::Stopped);
                            }
                        }
                        ack(done);
                    }
                }
            }
        }
    }
}

/// Exponential backoff between restarts, from `BACKOFF_MIN` doubling up to `BACKOFF_MAX`.
fn backoff(attempt: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1).min(16));
    BACKOFF_MIN.saturating_mul(factor).min(BACKOFF_MAX)
}

/// Failures that a restart cannot fix: a rejected token, or intents Discord will not grant.
fn is_permanent(error: &BotError) -> bool {
    match error {
        BotError::Http(error) => is_auth_rejection(error),
        BotError::Gateway(error) => error
            .source()
            .and_then(|source| source.downcast_ref::<twilight_http::Error>())
            .is_some_and(is_auth_rejection),
        BotError::Closed { code, .. } => {
            CloseCode::try_from(*code).is_ok_and(|code| !code.can_reconnect())
        }
        BotError::Decode(_) | BotError::Disconnected => false,
    }
}

fn is_auth_rejection(error: &twilight_http::Error) -> bool {
    match error.kind() {
        ErrorType::Unauthorized => true,
        ErrorType::Response { status, .. } => matches!(status.get(), 401 | 403),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(4), Duration::from_secs(40));
        assert_eq!(backoff(10), BACKOFF_MAX);
        assert_eq!(backoff(100), BACKOFF_MAX);
    }

    #[test]
    fn fatal_close_codes_are_permanent() {
        let closed = |code| BotError::Closed {
            code,
            reason: String::new(),
        };
        assert!(is_permanent(&closed(4004)));
        assert!(is_permanent(&closed(4014)));
        assert!(!is_permanent(&closed(4000)));
        assert!(!is_permanent(&closed(1000)));
        assert!(!is_permanent(&BotError::Disconnected));
    }

    #[test]
    fn status_serializes_flat() {
        let status = BotStatus::new(BotState::Connected {
            user: "DiscoClip".into(),
        });
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "connected");
        assert_eq!(json["user"], "DiscoClip");
        assert!(json["since"].is_string());
    }

    #[tokio::test]
    async fn control_without_token_is_disabled() {
        let control = BotControl::disabled();
        assert_eq!(control.status().state, BotState::Disabled);
        assert!(matches!(control.start().await, Err(ControlError::Disabled)));
        assert!(matches!(control.stop().await, Err(ControlError::Disabled)));
    }
}
