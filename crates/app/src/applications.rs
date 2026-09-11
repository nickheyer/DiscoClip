//! Discord applications the server runs bots for, several per install, with their client
//! secrets and bot tokens sealed under the keyring. Every change is written to the audit
//! log in the same transaction; the secrets never are, only that they were replaced.

use discoclip_bot::DiscordEndpoints;
use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Value as Json, json};
use twilight_model::guild::Permissions;
use url::Url;
use uuid::Uuid;

use crate::audit::{self, Action, Actor, Target};
use crate::commands::{CommandMode, CommandScope};
use crate::db::{nanos, timestamp, transact};
use crate::secrets::{Keyring, SecretError};

pub const NAME_MAX: usize = 100;

/// What the bot asks for when it is added to a guild.
pub const INSTALL_SCOPES: [&str; 2] = ["bot", "applications.commands"];

/// The permissions the bot needs: to read the channels it watches and to post there.
pub fn install_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::EMBED_LINKS
        | Permissions::ATTACH_FILES
        | Permissions::READ_MESSAGE_HISTORY
}

/// A link that adds an application's bot to a guild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallLink {
    pub url: Url,
    pub scopes: Vec<String>,
    /// The permissions asked for, by name.
    pub permissions: Vec<String>,
}

/// The link for `client_id`'s bot, preselecting `guild` when given.
pub fn install_link(
    endpoints: &DiscordEndpoints,
    client_id: &str,
    guild: Option<&str>,
) -> InstallLink {
    let permissions = install_permissions();
    let mut url = endpoints.authorize_url();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("scope", &INSTALL_SCOPES.join(" "))
            .append_pair("permissions", &permissions.bits().to_string());
        if let Some(guild) = guild {
            query
                .append_pair("guild_id", guild)
                .append_pair("disable_guild_select", "true");
        }
    }
    InstallLink {
        url,
        scopes: INSTALL_SCOPES.iter().map(|s| s.to_string()).collect(),
        permissions: permissions
            .iter_names()
            .map(|(name, _)| name.to_string())
            .collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ApplicationId(pub Uuid);

impl std::fmt::Display for ApplicationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ApplicationId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Application {
    pub id: ApplicationId,
    pub name: String,
    /// Discord's id for the application, which is also its OAuth client id.
    pub client_id: String,
    /// Whether this application signs people in with Discord.
    pub login: bool,
    pub has_client_secret: bool,
    pub commands: CommandsState,
    /// Whether the bot is meant to run; a stopped bot stays stopped across restarts.
    pub enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Where the slash commands are set to be, and how the last registration went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandsState {
    #[serde(flatten)]
    pub scope: CommandScope,
    pub registered_at: Option<Timestamp>,
    /// Why the last registration failed, until one succeeds.
    pub error: Option<String>,
}

/// The secrets, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub bot_token: String,
    pub client_secret: Option<String>,
}

/// What an update may change; `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub name: Option<String>,
    pub bot_token: Option<String>,
    /// `Some(None)` removes the secret.
    pub client_secret: Option<Option<String>>,
    pub login: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error("application {0} not found")]
    NotFound(ApplicationId),
    #[error("an application name is 1 to {NAME_MAX} characters")]
    InvalidName,
    #[error("Discord application {0} is already added")]
    Duplicate(String),
    #[error("signing in with Discord needs the application's client secret")]
    LoginNeedsSecret,
    #[error("{0:?} is not a Discord guild id")]
    InvalidGuild(String),
}

impl From<rusqlite::Error> for ApplicationError {
    fn from(error: rusqlite::Error) -> Self {
        ApplicationError::Store(error.into())
    }
}

fn check_name(name: &str) -> Result<String, ApplicationError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return Err(ApplicationError::InvalidName);
    }
    Ok(name.to_string())
}

fn context(id: ApplicationId, column: &str) -> String {
    format!("discord_applications:{id}:{column}")
}

#[derive(Clone)]
pub struct ApplicationStore {
    db: SqliteStore,
    keyring: Keyring,
}

const SELECT: &str = "SELECT id, name, client_id, login, client_secret IS NOT NULL, created_at, updated_at, \
     commands_mode, commands_guilds, commands_registered_at, commands_error, enabled";

impl ApplicationStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore, keyring: Keyring) -> Self {
        Self { db, keyring }
    }

    /// Adds an application by `actor`; `client_id` is Discord's id for it.
    pub async fn create(
        &self,
        actor: &Actor,
        name: &str,
        client_id: &str,
        credentials: Credentials,
    ) -> Result<Application, ApplicationError> {
        let name = check_name(name)?;
        let client_id = client_id.to_string();
        let keyring = self.keyring.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = ApplicationId(Uuid::now_v7());
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO discord_applications (id, name, client_id, client_secret, bot_token, login, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                params![
                    id.to_string(),
                    name,
                    client_id,
                    credentials
                        .client_secret
                        .as_deref()
                        .map(|s| keyring.seal_str(s, &context(id, "client_secret"))),
                    keyring.seal_str(&credentials.bot_token, &context(id, "bot_token")),
                    nanos(now),
                    nanos(now),
                ],
            );
            match inserted {
                Ok(_) => {
                    audit::record(
                        tx,
                        &actor,
                        Action::ApplicationCreate,
                        Target::application(id, &name),
                        json!({
                            "name": name,
                            "client_id": client_id,
                            "has_client_secret": credentials.client_secret.is_some(),
                        }),
                    )?;
                    Ok(Application {
                        id,
                        name,
                        client_id,
                        login: false,
                        has_client_secret: credentials.client_secret.is_some(),
                        commands: CommandsState {
                            scope: CommandScope::default(),
                            registered_at: None,
                            error: None,
                        },
                        enabled: true,
                        created_at: now,
                        updated_at: now,
                    })
                }
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    Err(ApplicationError::Duplicate(client_id))
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    /// Every application, oldest first.
    pub async fn list(&self) -> Result<Vec<Application>, ApplicationError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM discord_applications ORDER BY created_at, id"
            ))?;
            let rows = stmt.query_map([], row_to_application)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn get(&self, id: ApplicationId) -> Result<Option<Application>, ApplicationError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    /// An application with its secrets opened.
    pub async fn credentials(
        &self,
        id: ApplicationId,
    ) -> Result<(Application, Credentials), ApplicationError> {
        let keyring = self.keyring.clone();
        transact(&self.db, move |tx| {
            let application = get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))?;
            let credentials = credentials_in(tx, &keyring, id)?;
            Ok((application, credentials))
        })
        .await
    }

    /// Every application with its secrets opened, oldest first; what starts the bots.
    pub async fn all_credentials(
        &self,
    ) -> Result<Vec<(Application, Credentials)>, ApplicationError> {
        let keyring = self.keyring.clone();
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM discord_applications ORDER BY created_at, id"
            ))?;
            let applications = stmt
                .query_map([], row_to_application)?
                .collect::<Result<Vec<_>, _>>()?;
            let mut all = Vec::with_capacity(applications.len());
            for application in applications {
                let credentials = credentials_in(tx, &keyring, application.id)?;
                all.push((application, credentials));
            }
            Ok(all)
        })
        .await
    }

    /// The application that signs people in with Discord, if one is set to.
    pub async fn login_application(
        &self,
    ) -> Result<Option<(Application, Credentials)>, ApplicationError> {
        let keyring = self.keyring.clone();
        transact(&self.db, move |tx| {
            let found = tx
                .query_row(
                    &format!("{SELECT} FROM discord_applications WHERE login = 1"),
                    [],
                    row_to_application,
                )
                .optional()?;
            match found {
                Some(application) => {
                    let credentials = credentials_in(tx, &keyring, application.id)?;
                    Ok(Some((application, credentials)))
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Applies `changes` by `actor`. Turning `login` on turns it off for every other
    /// application, and needs a client secret to be stored or given.
    pub async fn update(
        &self,
        actor: &Actor,
        id: ApplicationId,
        changes: Changes,
    ) -> Result<Application, ApplicationError> {
        let name = changes.name.as_deref().map(check_name).transpose()?;
        let keyring = self.keyring.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let mut application = get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))?;
            let previous = application.clone();
            let now = Timestamp::now();
            if let Some(name) = name {
                tx.execute(
                    "UPDATE discord_applications SET name = ?2 WHERE id = ?1",
                    params![id.to_string(), name],
                )?;
                application.name = name;
            }
            if let Some(token) = &changes.bot_token {
                tx.execute(
                    "UPDATE discord_applications SET bot_token = ?2 WHERE id = ?1",
                    params![
                        id.to_string(),
                        keyring.seal_str(token, &context(id, "bot_token"))
                    ],
                )?;
            }
            if let Some(secret) = &changes.client_secret {
                tx.execute(
                    "UPDATE discord_applications SET client_secret = ?2 WHERE id = ?1",
                    params![
                        id.to_string(),
                        secret
                            .as_deref()
                            .map(|s| keyring.seal_str(s, &context(id, "client_secret")))
                    ],
                )?;
                application.has_client_secret = secret.is_some();
            }
            if let Some(login) = changes.login {
                if login && !application.has_client_secret {
                    return Err(ApplicationError::LoginNeedsSecret);
                }
                if login {
                    tx.execute("UPDATE discord_applications SET login = 0", [])?;
                }
                tx.execute(
                    "UPDATE discord_applications SET login = ?2 WHERE id = ?1",
                    params![id.to_string(), login],
                )?;
                application.login = login;
            } else if application.login && !application.has_client_secret {
                // The secret went; so does what needed it.
                tx.execute(
                    "UPDATE discord_applications SET login = 0 WHERE id = ?1",
                    params![id.to_string()],
                )?;
                application.login = false;
            }
            tx.execute(
                "UPDATE discord_applications SET updated_at = ?2 WHERE id = ?1",
                params![id.to_string(), nanos(now)],
            )?;
            application.updated_at = now;
            let details = update_details(&previous, &application, &changes);
            if !details.is_empty() {
                audit::record(
                    tx,
                    &actor,
                    Action::ApplicationUpdate,
                    Target::application(id, &application.name),
                    Json::Object(details),
                )?;
            }
            Ok(application)
        })
        .await
    }

    /// Sets where the commands are to be registered, by `actor`; registering is the
    /// caller's next step.
    pub async fn set_commands(
        &self,
        actor: &Actor,
        id: ApplicationId,
        scope: CommandScope,
    ) -> Result<Application, ApplicationError> {
        for guild in &scope.guilds {
            if guild.parse::<u64>().ok().filter(|g| *g > 0).is_none() {
                return Err(ApplicationError::InvalidGuild(guild.clone()));
            }
        }
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous = get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))?;
            let guilds = serde_json::to_string(&scope.guilds)
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            if tx.execute(
                "UPDATE discord_applications SET commands_mode = ?2, commands_guilds = ?3, updated_at = ?4 WHERE id = ?1",
                params![id.to_string(), scope.mode.as_str(), guilds, nanos(Timestamp::now())],
            )? == 0
            {
                return Err(ApplicationError::NotFound(id));
            }
            if scope != previous.commands.scope {
                audit::record(
                    tx,
                    &actor,
                    Action::CommandsSet,
                    Target::application(id, &previous.name),
                    json!({ "scope": scope, "previous": previous.commands.scope }),
                )?;
            }
            get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))
        })
        .await
    }

    /// Sets whether the bot is meant to run.
    pub async fn set_enabled(
        &self,
        id: ApplicationId,
        enabled: bool,
    ) -> Result<Application, ApplicationError> {
        transact(&self.db, move |tx| {
            if tx.execute(
                "UPDATE discord_applications SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), enabled, nanos(Timestamp::now())],
            )? == 0
            {
                return Err(ApplicationError::NotFound(id));
            }
            get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))
        })
        .await
    }

    /// Records how registering the commands, as asked for by `actor`, went.
    pub async fn record_commands(
        &self,
        actor: &Actor,
        id: ApplicationId,
        outcome: Result<(), String>,
    ) -> Result<Application, ApplicationError> {
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let now = nanos(Timestamp::now());
            let changed = match &outcome {
                Ok(()) => tx.execute(
                    "UPDATE discord_applications SET commands_registered_at = ?2, commands_error = NULL WHERE id = ?1",
                    params![id.to_string(), now],
                )?,
                Err(error) => tx.execute(
                    "UPDATE discord_applications SET commands_error = ?2 WHERE id = ?1",
                    params![id.to_string(), error],
                )?,
            };
            if changed == 0 {
                return Err(ApplicationError::NotFound(id));
            }
            let application = get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))?;
            let mut details = json!({
                "scope": application.commands.scope,
                "registered": outcome.is_ok(),
            });
            if let Err(error) = &outcome {
                details["error"] = Json::String(error.clone());
            }
            audit::record(
                tx,
                &actor,
                Action::CommandsRegister,
                Target::application(id, &application.name),
                details,
            )?;
            Ok(application)
        })
        .await
    }

    /// Removes an application by `actor`, and through the database its rules and guilds.
    pub async fn delete(&self, actor: &Actor, id: ApplicationId) -> Result<(), ApplicationError> {
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let application = get_in(tx, id)?.ok_or(ApplicationError::NotFound(id))?;
            if tx.execute(
                "DELETE FROM discord_applications WHERE id = ?1",
                params![id.to_string()],
            )? == 0
            {
                return Err(ApplicationError::NotFound(id));
            }
            audit::record(
                tx,
                &actor,
                Action::ApplicationDelete,
                Target::application(id, &application.name),
                json!({ "name": application.name, "client_id": application.client_id }),
            )?;
            Ok(())
        })
        .await
    }
}

/// What an update changed, for the audit log: the fields that differ, with the name it
/// had before, and which secrets were replaced, set or removed, never their values.
fn update_details(
    previous: &Application,
    current: &Application,
    changes: &Changes,
) -> serde_json::Map<String, Json> {
    let mut details = serde_json::Map::new();
    if current.name != previous.name {
        details.insert("name".into(), json!(current.name));
        details.insert("previous_name".into(), json!(previous.name));
    }
    if changes.bot_token.is_some() {
        details.insert("bot_token".into(), json!("replaced"));
    }
    if let Some(secret) = &changes.client_secret {
        details.insert(
            "client_secret".into(),
            json!(if secret.is_some() { "set" } else { "removed" }),
        );
    }
    if current.login != previous.login {
        details.insert("login".into(), json!(current.login));
    }
    details
}

fn get_in(conn: &Connection, id: ApplicationId) -> Result<Option<Application>, ApplicationError> {
    Ok(conn
        .query_row(
            &format!("{SELECT} FROM discord_applications WHERE id = ?1"),
            params![id.to_string()],
            row_to_application,
        )
        .optional()?)
}

fn credentials_in(
    conn: &Connection,
    keyring: &Keyring,
    id: ApplicationId,
) -> Result<Credentials, ApplicationError> {
    let (token, secret): (String, Option<String>) = conn.query_row(
        "SELECT bot_token, client_secret FROM discord_applications WHERE id = ?1",
        params![id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Credentials {
        bot_token: keyring.open_str(&token, &context(id, "bot_token"))?,
        client_secret: secret
            .map(|s| keyring.open_str(&s, &context(id, "client_secret")))
            .transpose()?,
    })
}

fn row_to_application(row: &rusqlite::Row<'_>) -> rusqlite::Result<Application> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let mode: String = row.get(7)?;
    let guilds: String = row.get(8)?;
    let registered_at: Option<i64> = row.get(9)?;
    Ok(Application {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("application id {id}: {e}")))?,
        name: row.get(1)?,
        client_id: row.get(2)?,
        login: row.get(3)?,
        has_client_secret: row.get(4)?,
        commands: CommandsState {
            scope: CommandScope {
                mode: mode
                    .parse::<CommandMode>()
                    .map_err(|e| corrupt(format!("application {id}: {e}")))?,
                guilds: serde_json::from_str(&guilds)
                    .map_err(|e| corrupt(format!("application {id} command guilds: {e}")))?,
            },
            registered_at: registered_at
                .map(|at| timestamp("discord_applications.commands_registered_at", at))
                .transpose()
                .map_err(|e| corrupt(e.to_string()))?,
            error: row.get(10)?,
        },
        enabled: row.get(11)?,
        created_at: timestamp("discord_applications.created_at", row.get(5)?)
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("discord_applications.updated_at", row.get(6)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_links_carry_scopes_and_permissions() {
        let link = install_link(&DiscordEndpoints::default(), "1001", None);
        assert_eq!(link.url.host_str(), Some("discord.com"));
        assert_eq!(link.url.path(), "/oauth2/authorize");
        let query: std::collections::HashMap<String, String> =
            link.url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "1001");
        assert_eq!(query["scope"], "bot applications.commands");
        assert_eq!(query["permissions"], "274878024704");
        assert!(!query.contains_key("guild_id"));
        assert_eq!(link.scopes, vec!["bot", "applications.commands"]);
        assert!(link.permissions.contains(&"VIEW_CHANNEL".to_string()));
        assert!(link.permissions.contains(&"ATTACH_FILES".to_string()));
        assert_eq!(link.permissions.len(), 6);

        let picked = install_link(&DiscordEndpoints::default(), "1001", Some("42"));
        let query: std::collections::HashMap<String, String> =
            picked.url.query_pairs().into_owned().collect();
        assert_eq!(query["guild_id"], "42");
        assert_eq!(query["disable_guild_select"], "true");
    }

    async fn store() -> ApplicationStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        ApplicationStore::new(db, Keyring::from_key([5; 32]))
    }

    fn actor() -> Actor {
        Actor::test()
    }

    fn credentials(token: &str, secret: Option<&str>) -> Credentials {
        Credentials {
            bot_token: token.into(),
            client_secret: secret.map(Into::into),
        }
    }

    #[tokio::test]
    async fn applications_keep_their_secrets_sealed_and_open_them_back() {
        let store = store().await;
        let first = store
            .create(
                &actor(),
                " Clips ",
                "100",
                credentials("bot-1", Some("secret-1")),
            )
            .await
            .unwrap();
        assert_eq!(first.name, "Clips");
        assert_eq!(first.client_id, "100");
        assert!(first.has_client_secret);
        assert!(!first.login);
        assert_eq!(first.commands.scope, CommandScope::default());
        assert!(first.commands.registered_at.is_none());
        assert!(first.enabled);
        let second = store
            .create(&actor(), "Other", "200", credentials("bot-2", None))
            .await
            .unwrap();
        assert!(!second.has_client_secret);
        assert!(matches!(
            store.create(&actor(), "Again", "100", credentials("x", None)).await,
            Err(ApplicationError::Duplicate(id)) if id == "100"
        ));
        assert!(matches!(
            store
                .create(&actor(), "", "300", credentials("x", None))
                .await,
            Err(ApplicationError::InvalidName)
        ));

        let (application, opened) = store.credentials(first.id).await.unwrap();
        assert_eq!(application, first);
        assert_eq!(opened, credentials("bot-1", Some("secret-1")));
        let all = store.all_credentials().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].1, credentials("bot-2", None));
        assert_eq!(
            store.list().await.unwrap(),
            vec![first.clone(), second.clone()]
        );
        assert_eq!(store.get(first.id).await.unwrap(), Some(first.clone()));

        let raw: String = store
            .db
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT bot_token FROM discord_applications WHERE id = ?1",
                    params![first.id.to_string()],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert!(raw.starts_with("v1."));
        assert!(!raw.contains("bot-1"));
    }

    #[tokio::test]
    async fn updates_change_fields_and_one_application_signs_people_in() {
        let store = store().await;
        let a = store
            .create(&actor(), "A", "1", credentials("bot-a", Some("secret-a")))
            .await
            .unwrap();
        let b = store
            .create(&actor(), "B", "2", credentials("bot-b", None))
            .await
            .unwrap();
        assert!(store.login_application().await.unwrap().is_none());
        assert!(matches!(
            store
                .update(
                    &actor(),
                    b.id,
                    Changes {
                        login: Some(true),
                        ..Changes::default()
                    }
                )
                .await,
            Err(ApplicationError::LoginNeedsSecret)
        ));
        let a = store
            .update(
                &actor(),
                a.id,
                Changes {
                    name: Some("A renamed".into()),
                    login: Some(true),
                    ..Changes::default()
                },
            )
            .await
            .unwrap();
        assert!(a.login);
        assert_eq!(a.name, "A renamed");
        assert_eq!(store.login_application().await.unwrap().unwrap().0.id, a.id);

        let b = store
            .update(
                &actor(),
                b.id,
                Changes {
                    client_secret: Some(Some("secret-b".into())),
                    bot_token: Some("bot-b2".into()),
                    login: Some(true),
                    ..Changes::default()
                },
            )
            .await
            .unwrap();
        assert!(b.login);
        assert!(b.has_client_secret);
        assert!(!store.get(a.id).await.unwrap().unwrap().login);
        let (_, opened) = store.credentials(b.id).await.unwrap();
        assert_eq!(opened, credentials("bot-b2", Some("secret-b")));

        // Removing the secret ends signing in with it.
        let b = store
            .update(
                &actor(),
                b.id,
                Changes {
                    client_secret: Some(None),
                    ..Changes::default()
                },
            )
            .await
            .unwrap();
        assert!(!b.login);
        assert!(!b.has_client_secret);
        assert!(store.login_application().await.unwrap().is_none());

        // Command scope and registration outcomes.
        assert!(matches!(
            store
                .set_commands(&actor(),
                    a.id,
                    CommandScope {
                        mode: CommandMode::Guilds,
                        guilds: vec!["x".into()]
                    }
                )
                .await,
            Err(ApplicationError::InvalidGuild(g)) if g == "x"
        ));
        let scoped = store
            .set_commands(
                &actor(),
                a.id,
                CommandScope {
                    mode: CommandMode::Guilds,
                    guilds: vec!["100".into()],
                },
            )
            .await
            .unwrap();
        assert_eq!(scoped.commands.scope.mode, CommandMode::Guilds);
        assert_eq!(scoped.commands.scope.guilds, vec!["100"]);
        let failed = store
            .record_commands(&actor(), a.id, Err("Missing Access".into()))
            .await
            .unwrap();
        assert_eq!(failed.commands.error.as_deref(), Some("Missing Access"));
        assert!(failed.commands.registered_at.is_none());
        let done = store.record_commands(&actor(), a.id, Ok(())).await.unwrap();
        assert!(done.commands.error.is_none());
        assert!(done.commands.registered_at.is_some());
        let stopped = store.set_enabled(a.id, false).await.unwrap();
        assert!(!stopped.enabled);
        assert!(!store.all_credentials().await.unwrap()[0].0.enabled);
        assert!(store.set_enabled(a.id, true).await.unwrap().enabled);

        store.delete(&actor(), b.id).await.unwrap();
        assert!(matches!(
            store.delete(&actor(), b.id).await,
            Err(ApplicationError::NotFound(_))
        ));
        assert!(matches!(
            store
                .set_commands(&actor(), b.id, CommandScope::default())
                .await,
            Err(ApplicationError::NotFound(_))
        ));
        assert!(matches!(
            store.update(&actor(), b.id, Changes::default()).await,
            Err(ApplicationError::NotFound(_))
        ));
        assert_eq!(store.list().await.unwrap().len(), 1);
    }
}
