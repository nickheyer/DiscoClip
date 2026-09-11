//! Watch rules: which channels each application's bot listens in, where results go, whose
//! links count, and how big they may be. Edited in the app, read by the bots as they run
//! through a cache the store keeps up.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use discoclip_bot::{RuleSource, WatchRule};
use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;
use uuid::Uuid;

use crate::applications::ApplicationId;
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
    pub channel_id: String,
    /// Where results go; the watched channel when absent.
    #[serde(default)]
    pub post_to: Option<String>,
    /// Link hosts picked up; every supported host when empty.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// Users whose links count; everyone when this and `allow_roles` are empty.
    #[serde(default)]
    pub allow_users: Vec<String>,
    #[serde(default)]
    pub allow_roles: Vec<String>,
    #[serde(default)]
    pub max_source_bytes: Option<u64>,
    #[serde(default)]
    pub max_duration_secs: Option<u64>,
    /// The tallest output accepted, in pixels; tightens the engine's own.
    #[serde(default)]
    pub max_height: Option<u32>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
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
    /// The rule as the bot applies it.
    pub fn watch_rule(&self) -> WatchRule {
        WatchRule {
            channel: Id::new(self.input.channel_id.parse().unwrap_or(1)),
            post_to: self
                .input
                .post_to
                .as_deref()
                .and_then(|id| id.parse().ok())
                .and_then(Id::new_checked),
            allow_hosts: self.input.allow_hosts.clone(),
            allow_users: self
                .input
                .allow_users
                .iter()
                .filter_map(|id| id.parse().ok())
                .filter_map(Id::new_checked)
                .collect(),
            allow_roles: self
                .input
                .allow_roles
                .iter()
                .filter_map(|id| id.parse().ok())
                .filter_map(Id::new_checked)
                .collect(),
            max_source_bytes: self.input.max_source_bytes,
            max_duration_secs: self.input.max_duration_secs,
            max_height: self.input.max_height,
        }
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
    #[error("channel {0} already has a rule for this application")]
    Duplicate(String),
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
    snowflake("channel", &input.channel_id)?;
    if let Some(post_to) = &input.post_to {
        snowflake("destination channel", post_to)?;
    }
    for user in &input.allow_users {
        snowflake("user", user)?;
    }
    for role in &input.allow_roles {
        snowflake("role", role)?;
    }
    if input.allow_hosts.iter().any(|host| host.trim().is_empty()) {
        return Err(RuleError::Invalid("a host cannot be empty".into()));
    }
    if input.max_source_bytes == Some(0) {
        return Err(RuleError::Invalid(
            "max_source_bytes must be above zero".into(),
        ));
    }
    if input.max_duration_secs == Some(0) {
        return Err(RuleError::Invalid(
            "max_duration_secs must be above zero".into(),
        ));
    }
    if input.max_height == Some(0) {
        return Err(RuleError::Invalid("max_height must be above zero".into()));
    }
    Ok(())
}

/// The enabled rules by application and channel, as the bots read them.
#[derive(Clone, Default)]
pub struct RuleCache {
    rules: Arc<RwLock<HashMap<(Uuid, u64), WatchRule>>>,
}

impl RuleSource for RuleCache {
    fn rule(&self, application: Uuid, channel: Id<ChannelMarker>) -> Option<WatchRule> {
        self.rules
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(application, channel.get()))
            .cloned()
    }
}

#[derive(Clone)]
pub struct RuleStore {
    db: SqliteStore,
    cache: RuleCache,
}

const SELECT: &str = "SELECT id, application_id, guild_id, channel_id, post_to, allow_hosts, allow_users, \
     allow_roles, max_source_bytes, max_duration_secs, max_height, enabled, created_at, updated_at";

impl RuleStore {
    /// Over a database the application's migrations have been applied to; `load` fills the
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

    /// Fills the cache from the database.
    pub async fn load(&self) -> Result<usize, RuleError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| refresh(tx, &cache)).await
    }

    pub async fn create(
        &self,
        application: ApplicationId,
        guild_id: &str,
        input: RuleInput,
    ) -> Result<Rule, RuleError> {
        check(&input)?;
        snowflake("guild", guild_id)?;
        let guild_id = guild_id.to_string();
        let cache = self.cache.clone();
        transact(&self.db, move |tx| {
            let id = RuleId(Uuid::now_v7());
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO watch_rules (id, application_id, guild_id, channel_id, post_to, allow_hosts, \
                 allow_users, allow_roles, max_source_bytes, max_duration_secs, max_height, enabled, \
                 created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                params![
                    id.to_string(),
                    application.to_string(),
                    guild_id,
                    input.channel_id,
                    input.post_to,
                    encode(&input.allow_hosts)?,
                    encode(&input.allow_users)?,
                    encode(&input.allow_roles)?,
                    input.max_source_bytes.map(|n| n as i64),
                    input.max_duration_secs.map(|n| n as i64),
                    input.max_height.map(i64::from),
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
            get_in(tx, id)?.ok_or(RuleError::NotFound(id))
        })
        .await
    }

    /// Replaces what a rule says.
    pub async fn update(&self, id: RuleId, input: RuleInput) -> Result<Rule, RuleError> {
        check(&input)?;
        let cache = self.cache.clone();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let updated = tx.execute(
                "UPDATE watch_rules SET channel_id = ?2, post_to = ?3, allow_hosts = ?4, allow_users = ?5, \
                 allow_roles = ?6, max_source_bytes = ?7, max_duration_secs = ?8, max_height = ?9, \
                 enabled = ?10, updated_at = ?11 \
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    input.channel_id,
                    input.post_to,
                    encode(&input.allow_hosts)?,
                    encode(&input.allow_users)?,
                    encode(&input.allow_roles)?,
                    input.max_source_bytes.map(|n| n as i64),
                    input.max_duration_secs.map(|n| n as i64),
                    input.max_height.map(i64::from),
                    input.enabled,
                    nanos(now),
                ],
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
            get_in(tx, id)?.ok_or(RuleError::NotFound(id))
        })
        .await
    }

    pub async fn delete(&self, id: RuleId) -> Result<(), RuleError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| {
            if tx.execute(
                "DELETE FROM watch_rules WHERE id = ?1",
                params![id.to_string()],
            )? == 0
            {
                return Err(RuleError::NotFound(id));
            }
            refresh(tx, &cache)?;
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

fn encode(list: &[String]) -> Result<String, RuleError> {
    serde_json::to_string(list).map_err(|e| RuleError::Store(StoreError::Corrupt(e.to_string())))
}

/// Reloads the cache from every enabled rule.
fn refresh(conn: &Connection, cache: &RuleCache) -> Result<usize, RuleError> {
    let mut stmt = conn.prepare(&format!("{SELECT} FROM watch_rules WHERE enabled = 1"))?;
    let rules = stmt
        .query_map([], row_to_rule)?
        .collect::<Result<Vec<_>, _>>()?;
    let map: HashMap<(Uuid, u64), WatchRule> = rules
        .iter()
        .filter_map(|rule| {
            let channel: u64 = rule.input.channel_id.parse().ok()?;
            Some(((rule.application_id.0, channel), rule.watch_rule()))
        })
        .collect();
    let count = map.len();
    *cache.rules.write().unwrap_or_else(|e| e.into_inner()) = map;
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
    let list = |index: usize| -> rusqlite::Result<Vec<String>> {
        let text: String = row.get(index)?;
        serde_json::from_str(&text).map_err(|e| corrupt(format!("rule {id} list: {e}")))
    };
    let max_source_bytes: Option<i64> = row.get(8)?;
    let max_duration_secs: Option<i64> = row.get(9)?;
    let max_height: Option<i64> = row.get(10)?;
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
            post_to: row.get(4)?,
            allow_hosts: list(5)?,
            allow_users: list(6)?,
            allow_roles: list(7)?,
            max_source_bytes: max_source_bytes.map(|n| n.max(0) as u64),
            max_duration_secs: max_duration_secs.map(|n| n.max(0) as u64),
            max_height: max_height.map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX)),
            enabled: row.get(11)?,
        },
        created_at: timestamp("watch_rules.created_at", row.get(12)?)
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("watch_rules.updated_at", row.get(13)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::{ApplicationStore, Credentials};
    use crate::secrets::Keyring;

    async fn stores() -> (RuleStore, ApplicationId, ApplicationId, ApplicationStore) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let applications = ApplicationStore::new(db.clone(), Keyring::from_key([4; 32]));
        let credentials = || Credentials {
            bot_token: "t".into(),
            client_secret: None,
        };
        let a = applications
            .create("A", "1", credentials())
            .await
            .unwrap()
            .id;
        let b = applications
            .create("B", "2", credentials())
            .await
            .unwrap()
            .id;
        let store = RuleStore::new(db);
        store.load().await.unwrap();
        (store, a, b, applications)
    }

    fn input(channel: &str) -> RuleInput {
        RuleInput {
            channel_id: channel.into(),
            post_to: None,
            allow_hosts: Vec::new(),
            allow_users: Vec::new(),
            allow_roles: Vec::new(),
            max_source_bytes: None,
            max_duration_secs: None,
            max_height: None,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn rules_are_stored_and_the_cache_follows() {
        let (store, a, b, applications) = stores().await;
        let cache = store.cache();
        assert!(cache.rule(a.0, Id::new(10)).is_none());

        let mut full = input("10");
        full.post_to = Some("11".into());
        full.allow_hosts = vec!["reddit.com".into()];
        full.allow_users = vec!["9".into()];
        full.allow_roles = vec!["500".into()];
        full.max_source_bytes = Some(1000);
        full.max_duration_secs = Some(30);
        full.max_height = Some(720);
        let rule = store.create(a, "100", full.clone()).await.unwrap();
        assert_eq!(rule.input, full);
        assert_eq!(rule.guild_id, "100");
        let cached = cache.rule(a.0, Id::new(10)).unwrap();
        assert_eq!(cached.post_to, Some(Id::new(11)));
        assert_eq!(cached.allow_users, vec![Id::new(9)]);
        assert_eq!(cached.allow_roles, vec![Id::new(500)]);
        assert_eq!(cached.max_source_bytes, Some(1000));
        assert_eq!(cached.max_duration_secs, Some(30));
        assert_eq!(cached.max_height, Some(720));
        assert!(cache.rule(b.0, Id::new(10)).is_none());

        assert!(matches!(
            store.create(a, "100", input("10")).await,
            Err(RuleError::Duplicate(c)) if c == "10"
        ));
        store.create(b, "100", input("10")).await.unwrap();
        assert!(cache.rule(b.0, Id::new(10)).is_some());
        for bad in [
            input("abc"),
            input("0"),
            RuleInput {
                post_to: Some("x".into()),
                ..input("12")
            },
            RuleInput {
                allow_users: vec!["".into()],
                ..input("12")
            },
            RuleInput {
                allow_hosts: vec![" ".into()],
                ..input("12")
            },
            RuleInput {
                max_source_bytes: Some(0),
                ..input("12")
            },
            RuleInput {
                max_duration_secs: Some(0),
                ..input("12")
            },
            RuleInput {
                max_height: Some(0),
                ..input("12")
            },
        ] {
            assert!(matches!(
                store.create(a, "100", bad).await,
                Err(RuleError::Invalid(_))
            ));
        }
        assert!(matches!(
            store.create(a, "guild", input("12")).await,
            Err(RuleError::Invalid(_))
        ));

        let disabled = store
            .update(
                rule.id,
                RuleInput {
                    enabled: false,
                    ..full.clone()
                },
            )
            .await
            .unwrap();
        assert!(!disabled.input.enabled);
        assert!(cache.rule(a.0, Id::new(10)).is_none());
        let moved = store
            .update(
                rule.id,
                RuleInput {
                    channel_id: "13".into(),
                    ..full.clone()
                },
            )
            .await
            .unwrap();
        assert_eq!(moved.input.channel_id, "13");
        assert!(cache.rule(a.0, Id::new(13)).is_some());
        assert!(cache.rule(a.0, Id::new(10)).is_none());

        assert_eq!(
            store.list_for_guild(a, "100").await.unwrap(),
            vec![moved.clone()]
        );
        assert!(store.list_for_guild(a, "200").await.unwrap().is_empty());
        assert_eq!(store.list_all().await.unwrap().len(), 2);
        assert_eq!(store.get(rule.id).await.unwrap(), Some(moved));

        store.delete(rule.id).await.unwrap();
        assert!(matches!(
            store.delete(rule.id).await,
            Err(RuleError::NotFound(_))
        ));
        assert!(cache.rule(a.0, Id::new(13)).is_none());
        assert!(matches!(
            store.update(rule.id, input("10")).await,
            Err(RuleError::NotFound(_))
        ));

        // Removing an application removes its rules; a fresh load notices.
        applications.delete(b).await.unwrap();
        assert!(store.list_all().await.unwrap().is_empty());
        assert_eq!(store.load().await.unwrap(), 0);
        assert!(cache.rule(b.0, Id::new(10)).is_none());
    }
}
