//! Watch rules: which channels each application's bot listens in. A rule names one
//! channel, or none to watch every channel of its guild; a channel's own rule takes the
//! guild's place there, and turned off it keeps the channel out. Everything else about
//! what happens to a link, whose count, where results go, which platforms are taken,
//! is the profiles assigned. Edited in the app, read by the bots as they run through a
//! cache the store keeps up. Every change is written to the audit log in the same
//! transaction.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use discoclip_bot::{RuleSource, WatchRule};
use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};
use uuid::Uuid;

use crate::applications::ApplicationId;
use crate::audit::{self, Action, Actor, Target};
use crate::db::{nanos, timestamp, transact};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RuleId(pub Uuid);

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for RuleId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// What a rule says, as edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleInput {
    /// The channel watched, or `None` for every channel of the guild. A channel's own
    /// rule, enabled or not, takes the place of the guild's in that channel.
    #[serde(deserialize_with = "explicit")]
    pub channel_id: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// The field has to be there: `null` means every channel, and leaving it out means nothing.
fn explicit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(d)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub id: RuleId,
    pub application_id: ApplicationId,
    pub guild_id: String,
    #[serde(flatten)]
    pub input: RuleInput,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Rule {
    /// The rule as the bot applies it in `channel`: the rule's own channel, or for a rule
    /// watching every channel, the one a message arrived in.
    pub fn watch_rule(&self, channel: Id<ChannelMarker>) -> WatchRule {
        WatchRule { channel }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("rule {0} not found")]
    NotFound(RuleId),
    #[error("{0}")]
    Invalid(String),
    #[error("{}", duplicate(.0))]
    Duplicate(Option<String>),
}

fn duplicate(channel: &Option<String>) -> String {
    match channel {
        Some(channel) => format!("channel {channel} already has a rule for this application"),
        None => "this server already has a rule watching every channel".to_string(),
    }
}

impl From<rusqlite::Error> for RuleError {
    fn from(error: rusqlite::Error) -> Self {
        RuleError::Store(error.into())
    }
}

fn snowflake(what: &str, value: &str) -> Result<u64, RuleError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| RuleError::Invalid(format!("{what} {value:?} is not a Discord id")))
}

fn check(input: &RuleInput) -> Result<(), RuleError> {
    if let Some(channel) = &input.channel_id {
        snowflake("channel", channel)?;
    }
    Ok(())
}

/// The rules as the bots read them: every channel's own rule, enabled or not, and the
/// enabled rules watching a guild whole. A channel's own rule decides for that channel,
/// so a disabled one keeps the channel out of a guild watched whole.
#[derive(Clone, Default)]
pub struct RuleCache {
    inner: Arc<RwLock<Cached>>,
}

#[derive(Default)]
struct Cached {
    /// By application and channel: `Some` watches the channel, `None` leaves it alone.
    channels: HashMap<(Uuid, u64), Option<WatchRule>>,
    /// By application and guild: what every channel without a rule of its own gets. The
    /// rule's `channel` is a placeholder until a lookup fills in the message's.
    guilds: HashMap<(Uuid, u64), WatchRule>,
}

impl RuleSource for RuleCache {
    fn rule(
        &self,
        application: Uuid,
        guild: Option<Id<GuildMarker>>,
        channel: Id<ChannelMarker>,
    ) -> Option<WatchRule> {
        let cached = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(own) = cached.channels.get(&(application, channel.get())) {
            return own.clone();
        }
        let guild = guild?;
        cached
            .guilds
            .get(&(application, guild.get()))
            .map(|_| WatchRule { channel })
    }
}

#[derive(Clone)]
pub struct RuleStore {
    db: SqliteStore,
    cache: RuleCache,
}

const SELECT: &str =
    "SELECT id, application_id, guild_id, channel_id, enabled, created_at, updated_at";

impl RuleStore {
    /// Over a database the application's migrations have been applied to. `load` fills the
    /// cache before any bot reads it.
    pub fn new(db: SqliteStore) -> Self {
        Self {
            db,
            cache: RuleCache::default(),
        }
    }

    /// What the bots read.
    pub fn cache(&self) -> RuleCache {
        self.cache.clone()
    }

    /// Fills the cache from the database. Returns how many places are watched: channels
    /// with a rule of their own that is on, and guilds watched whole.
    pub async fn load(&self) -> Result<usize, RuleError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| refresh(tx, &cache)).await
    }

    /// Adds a rule by `actor`.
    pub async fn create(
        &self,
        actor: &Actor,
        application: ApplicationId,
        guild_id: &str,
        input: RuleInput,
    ) -> Result<Rule, RuleError> {
        check(&input)?;
        snowflake("guild", guild_id)?;
        let guild_id = guild_id.to_string();
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = RuleId(Uuid::now_v7());
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO watch_rules (id, application_id, guild_id, channel_id, enabled, \
                 created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![
                    id.to_string(),
                    application.to_string(),
                    guild_id,
                    input.channel_id,
                    input.enabled,
                    nanos(now),
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(RuleError::Duplicate(input.channel_id));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let rule = get_in(tx, id)?.ok_or(RuleError::NotFound(id))?;
            audit::record(
                tx,
                &actor,
                Action::RuleCreate,
                Target::rule(id, &rule.guild_id, rule.input.channel_id.as_deref()),
                json!({
                    "application_id": rule.application_id,
                    "guild_id": rule.guild_id,
                    "rule": rule.input,
                }),
            )?;
            Ok(rule)
        })
        .await
    }

    /// Replaces what a rule says, by `actor`.
    pub async fn update(
        &self,
        actor: &Actor,
        id: RuleId,
        input: RuleInput,
    ) -> Result<Rule, RuleError> {
        check(&input)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous = get_in(tx, id)?.ok_or(RuleError::NotFound(id))?;
            let now = Timestamp::now();
            let updated = tx.execute(
                "UPDATE watch_rules SET channel_id = ?2, enabled = ?3, updated_at = ?4 \
                 WHERE id = ?1",
                params![id.to_string(), input.channel_id, input.enabled, nanos(now)],
            );
            match updated {
                Ok(0) => return Err(RuleError::NotFound(id)),
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(RuleError::Duplicate(input.channel_id));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let rule = get_in(tx, id)?.ok_or(RuleError::NotFound(id))?;
            if rule.input != previous.input {
                audit::record(
                    tx,
                    &actor,
                    Action::RuleUpdate,
                    Target::rule(id, &rule.guild_id, rule.input.channel_id.as_deref()),
                    json!({
                        "application_id": rule.application_id,
                        "guild_id": rule.guild_id,
                        "rule": rule.input,
                        "previous": previous.input,
                    }),
                )?;
            }
            Ok(rule)
        })
        .await
    }

    /// Removes a rule by `actor`.
    pub async fn delete(&self, actor: &Actor, id: RuleId) -> Result<(), RuleError> {
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let rule = get_in(tx, id)?.ok_or(RuleError::NotFound(id))?;
            if tx.execute(
                "DELETE FROM watch_rules WHERE id = ?1",
                params![id.to_string()],
            )? == 0
            {
                return Err(RuleError::NotFound(id));
            }
            refresh(tx, &cache)?;
            audit::record(
                tx,
                &actor,
                Action::RuleDelete,
                Target::rule(id, &rule.guild_id, rule.input.channel_id.as_deref()),
                json!({
                    "application_id": rule.application_id,
                    "guild_id": rule.guild_id,
                    "rule": rule.input,
                }),
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get(&self, id: RuleId) -> Result<Option<Rule>, RuleError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    /// The rules of one guild under one application, by channel.
    pub async fn list_for_guild(
        &self,
        application: ApplicationId,
        guild_id: &str,
    ) -> Result<Vec<Rule>, RuleError> {
        let guild_id = guild_id.to_string();
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM watch_rules WHERE application_id = ?1 AND guild_id = ?2 ORDER BY channel_id, id"
            ))?;
            let rows = stmt.query_map(params![application.to_string(), guild_id], row_to_rule)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Every rule, by application, guild and channel.
    pub async fn list_all(&self) -> Result<Vec<Rule>, RuleError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM watch_rules ORDER BY application_id, guild_id, channel_id, id"
            ))?;
            let rows = stmt.query_map([], row_to_rule)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }
}

/// Reloads the cache from every rule: channel rules whether on or off, since an off one
/// keeps its channel out of a guild watched whole, and the guild rules that are on.
fn refresh(conn: &Connection, cache: &RuleCache) -> Result<usize, RuleError> {
    let mut stmt = conn.prepare(&format!("{SELECT} FROM watch_rules"))?;
    let rules = stmt
        .query_map([], row_to_rule)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cached = Cached::default();
    for rule in &rules {
        let application = rule.application_id.0;
        match &rule.input.channel_id {
            Some(channel) => {
                let Some(channel) = channel.parse().ok().and_then(Id::new_checked) else {
                    continue;
                };
                cached.channels.insert(
                    (application, channel.get()),
                    rule.input.enabled.then(|| rule.watch_rule(channel)),
                );
            }
            None if rule.input.enabled => {
                let Some(guild) = rule
                    .guild_id
                    .parse()
                    .ok()
                    .and_then(Id::<GuildMarker>::new_checked)
                else {
                    continue;
                };
                cached
                    .guilds
                    .insert((application, guild.get()), rule.watch_rule(Id::new(1)));
            }
            None => {}
        }
    }
    let count = cached.channels.values().filter(|own| own.is_some()).count() + cached.guilds.len();
    *cache.inner.write().unwrap_or_else(|e| e.into_inner()) = cached;
    Ok(count)
}

fn get_in(conn: &Connection, id: RuleId) -> Result<Option<Rule>, RuleError> {
    Ok(conn
        .query_row(
            &format!("{SELECT} FROM watch_rules WHERE id = ?1"),
            params![id.to_string()],
            row_to_rule,
        )
        .optional()?)
}

fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let application_id: String = row.get(1)?;
    Ok(Rule {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("rule id {id}: {e}")))?,
        application_id: application_id
            .parse()
            .map_err(|e| corrupt(format!("rule application {application_id}: {e}")))?,
        guild_id: row.get(2)?,
        input: RuleInput {
            channel_id: row.get(3)?,
            enabled: row.get(4)?,
        },
        created_at: timestamp("watch_rules.created_at", row.get(5)?)
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("watch_rules.updated_at", row.get(6)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::{ApplicationStore, Credentials};
    use crate::secrets::Keyring;

    fn actor() -> Actor {
        Actor::test()
    }

    async fn stores() -> (RuleStore, ApplicationId, ApplicationId, ApplicationStore) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let applications = ApplicationStore::new(db.clone(), Keyring::from_key([4; 32]));
        let credentials = || Credentials {
            bot_token: "t".into(),
            client_secret: None,
        };
        let a = applications
            .create(&actor(), "A", "1", credentials())
            .await
            .unwrap()
            .id;
        let b = applications
            .create(&actor(), "B", "2", credentials())
            .await
            .unwrap()
            .id;
        let store = RuleStore::new(db);
        store.load().await.unwrap();
        (store, a, b, applications)
    }

    fn input(channel: &str) -> RuleInput {
        RuleInput {
            channel_id: Some(channel.into()),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn rules_are_stored_and_the_cache_follows() {
        let (store, a, b, applications) = stores().await;
        let cache = store.cache();
        let guild = Some(Id::new(100));
        assert!(cache.rule(a.0, guild, Id::new(10)).is_none());

        let full = input("10");
        let rule = store
            .create(&actor(), a, "100", full.clone())
            .await
            .unwrap();
        assert_eq!(rule.input, full);
        assert_eq!(rule.guild_id, "100");
        let cached = cache.rule(a.0, guild, Id::new(10)).unwrap();
        assert_eq!(cached.channel, Id::new(10));
        assert!(cache.rule(b.0, guild, Id::new(10)).is_none());

        assert!(matches!(
            store.create(&actor(), a, "100", input("10")).await,
            Err(RuleError::Duplicate(c)) if c.as_deref() == Some("10")
        ));
        store.create(&actor(), b, "100", input("10")).await.unwrap();
        assert!(cache.rule(b.0, guild, Id::new(10)).is_some());
        for bad in [input("abc"), input("0")] {
            assert!(matches!(
                store.create(&actor(), a, "100", bad).await,
                Err(RuleError::Invalid(_))
            ));
        }
        assert!(matches!(
            store.create(&actor(), a, "guild", input("12")).await,
            Err(RuleError::Invalid(_))
        ));

        let disabled = store
            .update(
                &actor(),
                rule.id,
                RuleInput {
                    enabled: false,
                    ..full.clone()
                },
            )
            .await
            .unwrap();
        assert!(!disabled.input.enabled);
        assert!(cache.rule(a.0, guild, Id::new(10)).is_none());
        let moved = store
            .update(
                &actor(),
                rule.id,
                RuleInput {
                    channel_id: Some("13".into()),
                    ..full.clone()
                },
            )
            .await
            .unwrap();
        assert_eq!(moved.input.channel_id.as_deref(), Some("13"));
        assert!(cache.rule(a.0, guild, Id::new(13)).is_some());
        assert!(cache.rule(a.0, guild, Id::new(10)).is_none());

        assert_eq!(
            store.list_for_guild(a, "100").await.unwrap(),
            vec![moved.clone()]
        );
        assert!(store.list_for_guild(a, "200").await.unwrap().is_empty());
        assert_eq!(store.list_all().await.unwrap().len(), 2);
        assert_eq!(store.get(rule.id).await.unwrap(), Some(moved));

        store.delete(&actor(), rule.id).await.unwrap();
        assert!(matches!(
            store.delete(&actor(), rule.id).await,
            Err(RuleError::NotFound(_))
        ));
        assert!(cache.rule(a.0, guild, Id::new(13)).is_none());
        assert!(matches!(
            store.update(&actor(), rule.id, input("10")).await,
            Err(RuleError::NotFound(_))
        ));

        // Removing an application removes its rules. A fresh load notices.
        applications.delete(&actor(), b).await.unwrap();
        assert!(store.list_all().await.unwrap().is_empty());
        assert_eq!(store.load().await.unwrap(), 0);
        assert!(cache.rule(b.0, guild, Id::new(10)).is_none());
    }

    #[tokio::test]
    async fn a_rule_without_a_channel_watches_the_guild_whole() {
        let (store, a, b, _) = stores().await;
        let cache = store.cache();
        let guild = Some(Id::new(100));
        let whole = RuleInput {
            channel_id: None,
            enabled: true,
        };
        let rule = store
            .create(&actor(), a, "100", whole.clone())
            .await
            .unwrap();
        assert_eq!(rule.input, whole);
        assert!(matches!(
            store.create(&actor(), a, "100", whole.clone()).await,
            Err(RuleError::Duplicate(None))
        ));

        // Any channel of the guild gets the guild's rule, with itself as the channel.
        let picked = cache.rule(a.0, guild, Id::new(55)).unwrap();
        assert_eq!(picked.channel, Id::new(55));
        // Not another guild's channel, not a channel outside any guild, not another
        // application's bot.
        assert!(cache.rule(a.0, Some(Id::new(200)), Id::new(55)).is_none());
        assert!(cache.rule(a.0, None, Id::new(55)).is_none());
        assert!(cache.rule(b.0, guild, Id::new(55)).is_none());

        // A channel's own rule takes the guild's place: on, watching; off, leaving the
        // channel alone.
        let own = store.create(&actor(), a, "100", input("56")).await.unwrap();
        assert_eq!(
            cache.rule(a.0, guild, Id::new(56)).unwrap().channel,
            Id::new(56)
        );
        store
            .update(
                &actor(),
                own.id,
                RuleInput {
                    enabled: false,
                    ..input("56")
                },
            )
            .await
            .unwrap();
        assert!(cache.rule(a.0, guild, Id::new(56)).is_none());
        assert!(cache.rule(a.0, guild, Id::new(57)).is_some());
        assert_eq!(store.load().await.unwrap(), 1);

        // The guild's rule turned off watches nothing; its own channel rules stay as they are.
        store
            .update(
                &actor(),
                rule.id,
                RuleInput {
                    enabled: false,
                    ..whole.clone()
                },
            )
            .await
            .unwrap();
        assert!(cache.rule(a.0, guild, Id::new(57)).is_none());
        assert!(cache.rule(a.0, guild, Id::new(56)).is_none());
        store.delete(&actor(), rule.id).await.unwrap();
        assert_eq!(
            store.list_for_guild(a, "100").await.unwrap().len(),
            1,
            "the channel's own rule stays"
        );
    }
}
