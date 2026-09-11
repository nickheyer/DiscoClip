//! The audit log: who changed which setting, and who added, changed or removed which
//! Discord application, rule or bot, when, and what changed. An entry is written in the
//! transaction that makes the change, so a change that is stored is logged; values at keys
//! named like secrets are redacted before they are written.

use std::collections::BTreeMap;
use std::net::IpAddr;

use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, params, types::Value};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};
use crate::users::UserId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditId(pub Uuid);

impl std::fmt::Display for AuditId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for AuditId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// What carried an account's credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Session,
    Token,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Session => "session",
            Via::Token => "token",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "session" => Some(Via::Session),
            "token" => Some(Via::Token),
            _ => None,
        }
    }
}

/// Who made a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// An account, from a browser session or with an API token.
    User {
        id: UserId,
        username: String,
        via: Via,
        ip: IpAddr,
    },
    /// The provisioning file and the environment, at startup.
    Provisioning {
        /// The file that was read, when one was.
        file: Option<String>,
    },
}

impl Actor {
    /// An account for the stores' tests to act as.
    #[cfg(test)]
    pub fn test() -> Self {
        Actor::User {
            id: UserId(Uuid::nil()),
            username: "tester".into(),
            via: Via::Session,
            ip: IpAddr::from([10, 0, 0, 1]),
        }
    }

    fn columns(&self) -> [Value; 5] {
        match self {
            Actor::User {
                id,
                username,
                via,
                ip,
            } => [
                Value::Text("user".into()),
                Value::Text(id.to_string()),
                Value::Text(username.clone()),
                Value::Text(via.as_str().into()),
                Value::Text(ip.to_string()),
            ],
            Actor::Provisioning { file } => [
                Value::Text("provisioning".into()),
                Value::Null,
                file.clone().map_or(Value::Null, Value::Text),
                Value::Null,
                Value::Null,
            ],
        }
    }
}

/// What was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// A value stored from the app.
    SettingsSet,
    /// A value removed so the default applies.
    SettingsReset,
    /// A provisioning file imported from the app.
    SettingsImport,
    /// A value written by the provisioning file or the environment at startup.
    SettingsProvision,
    ApplicationCreate,
    ApplicationUpdate,
    ApplicationDelete,
    /// Where the slash commands are to be registered.
    CommandsSet,
    /// The commands registered at Discord, or not.
    CommandsRegister,
    BotStart,
    BotStop,
    BotRestart,
    RuleCreate,
    RuleUpdate,
    RuleDelete,
}

impl Action {
    pub const ALL: [Action; 15] = [
        Action::SettingsSet,
        Action::SettingsReset,
        Action::SettingsImport,
        Action::SettingsProvision,
        Action::ApplicationCreate,
        Action::ApplicationUpdate,
        Action::ApplicationDelete,
        Action::CommandsSet,
        Action::CommandsRegister,
        Action::BotStart,
        Action::BotStop,
        Action::BotRestart,
        Action::RuleCreate,
        Action::RuleUpdate,
        Action::RuleDelete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::SettingsSet => "settings.set",
            Action::SettingsReset => "settings.reset",
            Action::SettingsImport => "settings.import",
            Action::SettingsProvision => "settings.provision",
            Action::ApplicationCreate => "application.create",
            Action::ApplicationUpdate => "application.update",
            Action::ApplicationDelete => "application.delete",
            Action::CommandsSet => "application.commands.set",
            Action::CommandsRegister => "application.commands.register",
            Action::BotStart => "bot.start",
            Action::BotStop => "bot.stop",
            Action::BotRestart => "bot.restart",
            Action::RuleCreate => "rule.create",
            Action::RuleUpdate => "rule.update",
            Action::RuleDelete => "rule.delete",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Action::ALL
            .into_iter()
            .find(|action| action.as_str() == s)
            .ok_or_else(|| format!("unknown audit action {s}"))
    }
}

impl Serialize for Action {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What kind of thing a change was made to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A settings key such as `engine.workers`.
    Setting,
    Application,
    Rule,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Setting => "setting",
            TargetKind::Application => "application",
            TargetKind::Rule => "rule",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "setting" => Some(TargetKind::Setting),
            "application" => Some(TargetKind::Application),
            "rule" => Some(TargetKind::Rule),
            _ => None,
        }
    }
}

impl std::str::FromStr for TargetKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TargetKind::parse(s).ok_or_else(|| format!("unknown audit target kind {s}"))
    }
}

/// What a change was made to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub kind: TargetKind,
    /// The settings key, or the application's or rule's id.
    pub id: String,
    /// The application's name, or the rule's guild and channel, as they were at the time.
    pub name: Option<String>,
}

impl Target {
    pub fn setting(key: &str) -> Self {
        Self {
            kind: TargetKind::Setting,
            id: key.to_string(),
            name: None,
        }
    }

    pub fn application(id: impl std::fmt::Display, name: &str) -> Self {
        Self {
            kind: TargetKind::Application,
            id: id.to_string(),
            name: Some(name.to_string()),
        }
    }

    pub fn rule(id: impl std::fmt::Display, guild_id: &str, channel_id: &str) -> Self {
        Self {
            kind: TargetKind::Rule,
            id: id.to_string(),
            name: Some(format!("{guild_id}/{channel_id}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub id: AuditId,
    pub at: Timestamp,
    pub actor: Actor,
    pub action: Action,
    pub target: Target,
    /// What changed, as the action defines it; secrets are redacted.
    pub details: Json,
}

/// Whether a settings key segment or a field name holds a secret.
fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("secret") || name.contains("password") || name.contains("token")
}

const REDACTED: &str = "[redacted]";

/// `value`, as stored at `key`, with every secret replaced: the whole value when a segment
/// of `key` names a secret, else each field named like one, at any depth. Nulls stay, since
/// an absent secret reveals nothing.
pub fn redact(key: &str, value: &Json) -> Json {
    if key.split('.').any(is_secret_name) {
        return redact_value(value);
    }
    redact_fields(value)
}

fn redact_value(value: &Json) -> Json {
    match value {
        Json::Null => Json::Null,
        _ => Json::String(REDACTED.into()),
    }
}

fn redact_fields(value: &Json) -> Json {
    match value {
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(name, value)| {
                    let value = if is_secret_name(name) {
                        redact_value(value)
                    } else {
                        redact_fields(value)
                    };
                    (name.clone(), value)
                })
                .collect(),
        ),
        Json::Array(items) => Json::Array(items.iter().map(redact_fields).collect()),
        other => other.clone(),
    }
}

/// Writes one entry on `conn`, inside whatever transaction the caller holds.
pub fn record(
    conn: &Connection,
    actor: &Actor,
    action: Action,
    target: Target,
    details: Json,
) -> Result<Entry, StoreError> {
    let id = AuditId(Uuid::now_v7());
    let at = Timestamp::now();
    let [actor_kind, actor_id, actor_name, actor_via, actor_ip] = actor.columns();
    conn.execute(
        "INSERT INTO audit_log (id, at, actor_kind, actor_id, actor_name, actor_via, actor_ip, \
         action, target_kind, target_id, target_name, details)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            id.to_string(),
            nanos(at),
            actor_kind,
            actor_id,
            actor_name,
            actor_via,
            actor_ip,
            action.as_str(),
            target.kind.as_str(),
            target.id,
            target.name,
            serde_json::to_string(&details)?,
        ],
    )?;
    Ok(Entry {
        id,
        at,
        actor: actor.clone(),
        action,
        target,
        details,
    })
}

/// Which entries to list; newest first, `limit` at a time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Entries by this account.
    pub actor: Option<UserId>,
    pub action: Option<Action>,
    pub target_kind: Option<TargetKind>,
    /// With `target_kind`: entries about this one thing.
    pub target_id: Option<String>,
    /// Entries at or after this moment.
    pub since: Option<Timestamp>,
    /// Entries at or before this moment.
    pub until: Option<Timestamp>,
    pub limit: Option<usize>,
    /// Entries older than this one: the `next` of the previous page.
    pub before: Option<AuditId>,
}

impl Filter {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 500;

    pub fn effective_limit(&self) -> usize {
        self.limit
            .unwrap_or(Self::DEFAULT_LIMIT)
            .clamp(1, Self::MAX_LIMIT)
    }

    /// The `WHERE` clause and its bindings.
    fn clauses(&self) -> (String, Vec<Value>) {
        let mut clauses: Vec<&str> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        if let Some(actor) = self.actor {
            clauses.push("actor_id = ?");
            values.push(Value::Text(actor.to_string()));
        }
        if let Some(action) = self.action {
            clauses.push("action = ?");
            values.push(Value::Text(action.as_str().into()));
        }
        if let Some(kind) = self.target_kind {
            clauses.push("target_kind = ?");
            values.push(Value::Text(kind.as_str().into()));
        }
        if let Some(id) = &self.target_id {
            clauses.push("target_id = ?");
            values.push(Value::Text(id.clone()));
        }
        if let Some(since) = self.since {
            clauses.push("at >= ?");
            values.push(Value::Integer(nanos(since)));
        }
        if let Some(until) = self.until {
            clauses.push("at <= ?");
            values.push(Value::Integer(nanos(until)));
        }
        if let Some(before) = self.before {
            clauses.push("id < ?");
            values.push(Value::Text(before.to_string()));
        }
        let sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        (sql, values)
    }
}

/// One page of entries, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Pass as `before` to get the entries older than these; absent on the last page.
    pub next: Option<AuditId>,
}

#[derive(Clone)]
pub struct AuditStore {
    db: SqliteStore,
}

/// Ids are UUIDv7, so ordering by id is ordering by time of writing, and a page's cursor
/// is its last id.
const SELECT: &str = "SELECT id, at, actor_kind, actor_id, actor_name, actor_via, actor_ip, \
     action, target_kind, target_id, target_name, details FROM audit_log";

impl AuditStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Writes one entry in its own transaction, for actions that are not database changes.
    pub async fn record(
        &self,
        actor: &Actor,
        action: Action,
        target: Target,
        details: Json,
    ) -> Result<Entry, StoreError> {
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            record(tx, &actor, action, target, details)
        })
        .await
    }

    pub async fn list(&self, filter: &Filter) -> Result<Page, StoreError> {
        let filter = filter.clone();
        transact(&self.db, move |tx| {
            let (where_sql, mut values) = filter.clauses();
            let limit = filter.effective_limit();
            values.push(Value::Integer(limit as i64 + 1));
            let mut stmt = tx.prepare(&format!("{SELECT}{where_sql} ORDER BY id DESC LIMIT ?"))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), row_to_entry)?;
            let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
            let next = if entries.len() > limit {
                entries.truncate(limit);
                entries.last().map(|entry| entry.id)
            } else {
                None
            };
            Ok::<_, StoreError>(Page { entries, next })
        })
        .await
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let actor_kind: String = row.get(2)?;
    let actor_id: Option<String> = row.get(3)?;
    let actor_name: Option<String> = row.get(4)?;
    let actor_via: Option<String> = row.get(5)?;
    let actor_ip: Option<String> = row.get(6)?;
    let action: String = row.get(7)?;
    let target_kind: String = row.get(8)?;
    let details: String = row.get(11)?;
    let actor = match actor_kind.as_str() {
        "user" => {
            let user_id = actor_id
                .ok_or_else(|| corrupt(format!("audit entry {id}: user actor without an id")))?;
            let via = actor_via
                .as_deref()
                .and_then(Via::parse)
                .ok_or_else(|| corrupt(format!("audit entry {id}: bad actor via")))?;
            let ip = actor_ip
                .ok_or_else(|| corrupt(format!("audit entry {id}: user actor without an ip")))?;
            Actor::User {
                id: user_id
                    .parse()
                    .map_err(|e| corrupt(format!("audit entry {id} actor {user_id}: {e}")))?,
                username: actor_name.ok_or_else(|| {
                    corrupt(format!("audit entry {id}: user actor without a name"))
                })?,
                via,
                ip: ip
                    .parse()
                    .map_err(|e| corrupt(format!("audit entry {id} actor ip {ip}: {e}")))?,
            }
        }
        "provisioning" => Actor::Provisioning { file: actor_name },
        other => {
            return Err(corrupt(format!(
                "audit entry {id}: unknown actor kind {other}"
            )));
        }
    };
    Ok(Entry {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("audit entry id {id}: {e}")))?,
        at: timestamp("audit_log.at", row.get(1)?).map_err(|e| corrupt(e.to_string()))?,
        actor,
        action: action
            .parse()
            .map_err(|e| corrupt(format!("audit entry {id}: {e}")))?,
        target: Target {
            kind: TargetKind::parse(&target_kind).ok_or_else(|| {
                corrupt(format!(
                    "audit entry {id}: unknown target kind {target_kind}"
                ))
            })?,
            id: row.get(9)?,
            name: row.get(10)?,
        },
        details: serde_json::from_str(&details)
            .map_err(|e| corrupt(format!("audit entry {id} details: {e}")))?,
    })
}

/// A settings value before and after a change, for the `details` of a settings entry.
pub fn settings_details(
    key: &str,
    previous: Option<&Json>,
    value: Option<&Json>,
    removed: &BTreeMap<String, Json>,
) -> Json {
    let mut details = serde_json::Map::new();
    if let Some(value) = value {
        details.insert("value".into(), redact(key, value));
    }
    if let Some(previous) = previous {
        details.insert("previous".into(), redact(key, previous));
    }
    if !removed.is_empty() {
        details.insert(
            "removed".into(),
            Json::Object(
                removed
                    .iter()
                    .map(|(key, value)| (key.clone(), redact(key, value)))
                    .collect(),
            ),
        );
    }
    Json::Object(details)
}
