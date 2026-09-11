//! Settings live in the database and are what the server runs on. Provisioning seeds them
//! at startup; the web app changes them afterwards, and a value changed in the app is never
//! overwritten by provisioning again.
//!
//! Each row holds one dotted path such as `engine.limits.max_height` with a JSON value.
//! Sections are not rows: they are implied by the paths beneath them. Arrays are single
//! values. A path that is not stored takes the default from its setting type.
//!
//! Every change is written to the audit log in the same transaction: what each key held
//! before and after, and what was removed around or beneath it.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use discoclip_engine::rusqlite::{self, Connection, params};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::{EngineConfig, StoreError};
use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use serde::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use url::Url;

use crate::audit::{self, Action, Actor, Target};
use crate::config::{ConfigError, Format, Provisioning};
use crate::db::{nanos, timestamp, transact};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub log: LogConfig,
    pub engine: EngineConfig,
    pub local: LocalConfig,
    pub web: WebConfig,
    pub auth: AuthConfig,
}

/// Serialized in the clear so the settings store and provisioning export can hold it.
fn expose<S: Serializer>(secret: &SecretString, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose_secret())
}

/// Ways to log in besides a password.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub github: Option<OAuthClient>,
    pub google: Option<OAuthClient>,
    pub oidc: Option<OidcClient>,
    /// Create a viewer account for a provider identity nobody has, at its first login.
    pub oauth_signup: bool,
}

/// An OAuth application registered at a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthClient {
    pub client_id: String,
    #[serde(serialize_with = "expose")]
    pub client_secret: SecretString,
}

/// An OpenID Connect issuer, found through its discovery document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcClient {
    /// What the login button says.
    #[serde(default = "default_oidc_name")]
    pub name: String,
    pub issuer: Url,
    pub client_id: String,
    #[serde(serialize_with = "expose")]
    pub client_secret: SecretString,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

fn default_oidc_name() -> String {
    "Single sign-on".into()
}

fn default_oidc_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// A tracing filter such as `info` or `info,discoclip_engine=debug`.
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

/// Where jobs submitted from the web app are published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("data/local"),
            max_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub bind: SocketAddr,
    /// How browsers reach the app, such as `https://clips.example.com`; login providers
    /// send them back here. Without it the address a request arrived at is used.
    pub public_url: Option<Url>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            public_url: None,
        }
    }
}

/// Who wrote a stored value last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The provisioning file or the environment, at startup.
    Provisioning,
    /// The web app, or an import.
    App,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Provisioning => "provisioning",
            Source::App => "app",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "provisioning" => Some(Source::Provisioning),
            "app" => Some(Source::App),
            _ => None,
        }
    }
}

/// One stored value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Json,
    pub source: Source,
    pub updated_at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provisioning(#[from] ConfigError),
    #[error("setting {path}: {message}")]
    Invalid { path: String, message: String },
    #[error("setting {0} is stored both as a value and as a section")]
    Conflict(String),
}

impl From<rusqlite::Error> for SettingsError {
    fn from(error: rusqlite::Error) -> Self {
        SettingsError::Store(error.into())
    }
}

/// The settings after provisioning has been applied.
#[derive(Debug, Clone)]
pub struct Bootstrapped {
    pub settings: Settings,
    /// Provisioned keys left untouched because the app had changed them, or a section
    /// around them, since.
    pub kept: Vec<String>,
}

/// The settings after an import.
#[derive(Debug, Clone)]
pub struct Imported {
    pub settings: Settings,
    /// The keys the import wrote.
    pub keys: Vec<String>,
}

/// Writes the provisioned values into `db`, keeping anything the app changed, and returns
/// what the server now runs on. `db` has had the application's migrations applied.
pub async fn bootstrap(
    db: &SqliteStore,
    provisioning: &Provisioning,
) -> Result<Bootstrapped, SettingsError> {
    SettingsStore::new(db.clone()).bootstrap(provisioning).await
}

#[derive(Clone)]
pub struct SettingsStore {
    db: SqliteStore,
}

impl SettingsStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Applies `provisioning`: each provisioned key is inserted, or updated when
    /// provisioning wrote it before. A key the app changed, or whose section or child the
    /// app changed, is kept as the app left it. What changed is logged as done by
    /// provisioning.
    pub async fn bootstrap(
        &self,
        provisioning: &Provisioning,
    ) -> Result<Bootstrapped, SettingsError> {
        let provisioning = provisioning.clone();
        let actor = Actor::Provisioning {
            file: provisioning
                .file
                .as_ref()
                .map(|path| path.display().to_string()),
        };
        transact(&self.db, move |tx| {
            let existing = read_entries(tx)?;
            let values = provisioning.resolve(&assemble(&existing)?)?;
            let now = Timestamp::now();
            let mut kept = Vec::new();
            let mut written = Vec::new();
            for (key, value) in values {
                let changed_in_app = existing
                    .iter()
                    .any(|entry| entry.source == Source::App && related(&entry.key, &key));
                if changed_in_app {
                    kept.push(key);
                    continue;
                }
                write(tx, &key, &value, Source::Provisioning, now)?;
                written.push(key);
            }
            let after = read_entries(tx)?;
            log_changes(
                tx,
                &actor,
                Action::SettingsProvision,
                &existing,
                &after,
                &written,
            )?;
            let settings = decode(&after)?;
            Ok(Bootstrapped { settings, kept })
        })
        .await
    }

    /// Writes every key of `provisioning` as an app change by `actor`, replacing whatever
    /// is stored at it. Nothing is written unless the settings as a whole stay valid.
    pub async fn import(
        &self,
        actor: &Actor,
        provisioning: &Provisioning,
    ) -> Result<Imported, SettingsError> {
        let provisioning = provisioning.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let existing = read_entries(tx)?;
            let values = provisioning.resolve(&assemble(&existing)?)?;
            let now = Timestamp::now();
            let mut keys = Vec::with_capacity(values.len());
            for (key, value) in values {
                write(tx, &key, &value, Source::App, now)?;
                keys.push(key);
            }
            let after = read_entries(tx)?;
            log_changes(tx, &actor, Action::SettingsImport, &existing, &after, &keys)?;
            let settings = decode(&after)?;
            Ok(Imported { settings, keys })
        })
        .await
    }

    /// The stored values as a provisioning file: importing it, or provisioning a fresh
    /// database with it, reproduces them.
    pub async fn export(&self, format: Format) -> Result<String, SettingsError> {
        transact(&self.db, move |tx| {
            Ok(format.render(&assemble(&read_entries(tx)?)?)?)
        })
        .await
    }

    /// What the server runs on: every stored value over the defaults.
    pub async fn load(&self) -> Result<Settings, SettingsError> {
        transact(&self.db, |tx| decode(&read_entries(tx)?)).await
    }

    /// Every stored value, by key.
    pub async fn entries(&self) -> Result<Vec<Entry>, SettingsError> {
        transact(&self.db, |tx| read_entries(tx)).await
    }

    /// Stores `value` at `key` as an app change by `actor`. Any value stored at a section
    /// around `key`, or at a path beneath it, is replaced. Nothing is written unless the
    /// settings as a whole stay valid; the new settings are returned.
    pub async fn set(
        &self,
        actor: &Actor,
        key: &str,
        value: Json,
    ) -> Result<Settings, SettingsError> {
        let key = key.to_string();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let before = read_entries(tx)?;
            write(tx, &key, &value, Source::App, Timestamp::now())?;
            let after = read_entries(tx)?;
            log_changes(
                tx,
                &actor,
                Action::SettingsSet,
                &before,
                &after,
                std::slice::from_ref(&key),
            )?;
            decode(&after)
        })
        .await
    }

    /// Removes the stored value at `key` and everything beneath it, so the defaults apply.
    /// Nothing is removed unless the settings as a whole stay valid; the new settings are
    /// returned.
    pub async fn reset(&self, actor: &Actor, key: &str) -> Result<Settings, SettingsError> {
        let key = key.to_string();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let before = read_entries(tx)?;
            tx.execute(
                "DELETE FROM settings WHERE key = ?1 OR substr(key, 1, length(?1) + 1) = ?1 || '.'",
                params![key],
            )?;
            let after = read_entries(tx)?;
            let removed = removed_around(&key, &before, &after, true);
            if !removed.is_empty() {
                audit::record(
                    tx,
                    &actor,
                    Action::SettingsReset,
                    Target::setting(&key),
                    audit::settings_details(&key, None, None, &removed),
                )?;
            }
            decode(&after)
        })
        .await
    }
}

/// Whether one path is the other, contains it, or lies beneath it.
fn related(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('.'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('.'))
}

/// The values stored around or beneath `key` before that are gone after; `key` itself
/// too when `including_key`.
fn removed_around(
    key: &str,
    before: &[Entry],
    after: &[Entry],
    including_key: bool,
) -> BTreeMap<String, Json> {
    before
        .iter()
        .filter(|entry| (including_key || entry.key != key) && related(&entry.key, key))
        .filter(|entry| !after.iter().any(|now| now.key == entry.key))
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect()
}

/// Logs what writing `keys` changed, one entry per key: the value before and after, and
/// the values stored around or beneath it that the write removed. A key the write left
/// exactly as it was, in value and source, is not logged.
fn log_changes(
    conn: &Connection,
    actor: &Actor,
    action: Action,
    before: &[Entry],
    after: &[Entry],
    keys: &[String],
) -> Result<(), SettingsError> {
    for key in keys {
        let previous = before.iter().find(|entry| &entry.key == key);
        let current = after.iter().find(|entry| &entry.key == key);
        let changed = match (previous, current) {
            (Some(was), Some(now)) => was.value != now.value || was.source != now.source,
            (None, None) => false,
            _ => true,
        };
        let removed = removed_around(key, before, after, false);
        if !changed && removed.is_empty() {
            continue;
        }
        audit::record(
            conn,
            actor,
            action,
            Target::setting(key),
            audit::settings_details(
                key,
                previous.map(|entry| &entry.value),
                current.map(|entry| &entry.value),
                &removed,
            ),
        )?;
    }
    Ok(())
}

fn read_entries(conn: &Connection) -> Result<Vec<Entry>, SettingsError> {
    let mut stmt =
        conn.prepare("SELECT key, value, source, updated_at FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (key, value, source, updated_at) = row?;
        let value = serde_json::from_str(&value)
            .map_err(|e| StoreError::Corrupt(format!("setting {key}: {e}")))?;
        let source = Source::parse(&source).ok_or_else(|| {
            StoreError::Corrupt(format!("setting {key}: unknown source {source}"))
        })?;
        let updated_at = timestamp("settings.updated_at", updated_at)?;
        entries.push(Entry {
            key,
            value,
            source,
            updated_at,
        });
    }
    Ok(entries)
}

/// Upserts `key`, removing any value stored at a section around it or a path beneath it.
fn write(
    conn: &Connection,
    key: &str,
    value: &Json,
    source: Source,
    now: Timestamp,
) -> Result<(), SettingsError> {
    conn.execute(
        "DELETE FROM settings WHERE key != ?1 AND (
            substr(key, 1, length(?1) + 1) = ?1 || '.' OR
            substr(?1, 1, length(key) + 1) = key || '.'
        )",
        params![key],
    )?;
    let encoded = serde_json::to_string(value).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    conn.execute(
        "INSERT INTO settings (key, value, source, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            source = excluded.source,
            updated_at = excluded.updated_at
         WHERE settings.value != excluded.value OR settings.source != excluded.source",
        params![key, encoded, source.as_str(), nanos(now)],
    )?;
    Ok(())
}

/// Nests dotted keys into one JSON tree.
fn assemble(entries: &[Entry]) -> Result<Json, SettingsError> {
    let mut root = serde_json::Map::new();
    for entry in entries {
        let segments: Vec<&str> = entry.key.split('.').collect();
        let (last, sections) = segments
            .split_last()
            .expect("split yields at least one segment");
        let mut node = &mut root;
        for section in sections {
            let child = node
                .entry(*section)
                .or_insert_with(|| Json::Object(serde_json::Map::new()));
            node = child
                .as_object_mut()
                .ok_or_else(|| SettingsError::Conflict(entry.key.clone()))?;
        }
        if node.contains_key(*last) {
            return Err(SettingsError::Conflict(entry.key.clone()));
        }
        node.insert((*last).to_string(), entry.value.clone());
    }
    Ok(Json::Object(root))
}

fn decode(entries: &[Entry]) -> Result<Settings, SettingsError> {
    serde_path_to_error::deserialize(assemble(entries)?).map_err(|error| SettingsError::Invalid {
        path: error.path().to_string(),
        message: error.into_inner().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn provisioning(tree: Json) -> Provisioning {
        Provisioning::from_tree(&tree).unwrap()
    }

    async fn store() -> SettingsStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        SettingsStore::new(db)
    }

    fn actor() -> Actor {
        Actor::test()
    }

    fn keys(entries: &[Entry]) -> Vec<(&str, Source)> {
        entries.iter().map(|e| (e.key.as_str(), e.source)).collect()
    }

    #[tokio::test]
    async fn fresh_store_yields_defaults() {
        let store = store().await;
        let settings = store.load().await.unwrap();
        assert_eq!(settings.engine.workers, 2);
        assert_eq!(settings.log.level, "info");
        assert_eq!(settings.local.max_bytes, 100 * 1024 * 1024);
        assert_eq!(settings.web.bind.port(), 8080);
        assert!(store.entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bootstrap_writes_only_provisioned_keys() {
        let store = store().await;
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 4}, "local": {"max_bytes": 5}}),
            ))
            .await
            .unwrap();
        assert!(boot.kept.is_empty());
        assert_eq!(boot.settings.engine.workers, 4);
        assert_eq!(boot.settings.engine.limits.max_height, 1080);
        assert_eq!(boot.settings.local.max_bytes, 5);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("engine.workers", Source::Provisioning),
                ("local.max_bytes", Source::Provisioning)
            ]
        );
    }

    #[tokio::test]
    async fn provisioning_updates_its_own_keys_and_leaves_removed_ones() {
        let store = store().await;
        store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 4}, "log": {"level": "debug"}}),
            ))
            .await
            .unwrap();
        let boot = store
            .bootstrap(&provisioning(json!({"engine": {"workers": 6}})))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.workers, 6);
        assert_eq!(boot.settings.log.level, "debug");
    }

    #[tokio::test]
    async fn provisioning_is_checked_over_what_is_stored() {
        let store = store().await;
        store
            .set(&actor(), "engine.archive.dir", json!("from-app"))
            .await
            .unwrap();
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"archive": {"keep": "both"}}}),
            ))
            .await
            .unwrap();
        let archive = boot.settings.engine.archive.unwrap();
        assert_eq!(archive.dir, std::path::PathBuf::from("from-app"));
        assert_eq!(archive.keep, discoclip_engine::archive::Keep::Both);
    }

    #[tokio::test]
    async fn app_changes_survive_provisioning() {
        let store = store().await;
        store
            .bootstrap(&provisioning(json!({"engine": {"workers": 4}})))
            .await
            .unwrap();
        let settings = store
            .set(&actor(), "engine.workers", json!(8))
            .await
            .unwrap();
        assert_eq!(settings.engine.workers, 8);
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 1}, "log": {"level": "warn"}}),
            ))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.workers, 8);
        assert_eq!(boot.settings.log.level, "warn");
        assert_eq!(boot.kept, vec!["engine.workers".to_string()]);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("engine.workers", Source::App),
                ("log.level", Source::Provisioning)
            ]
        );
    }

    #[tokio::test]
    async fn app_change_to_a_section_blocks_provisioning_beneath_it() {
        let store = store().await;
        store
            .bootstrap(&provisioning(
                json!({"engine": {"archive": {"dir": "archive"}}}),
            ))
            .await
            .unwrap();
        let settings = store
            .set(&actor(), "engine.archive", Json::Null)
            .await
            .unwrap();
        assert!(settings.engine.archive.is_none());
        let entries = store.entries().await.unwrap();
        assert_eq!(keys(&entries), vec![("engine.archive", Source::App)]);

        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"archive": {"dir": "archive"}}}),
            ))
            .await
            .unwrap();
        assert!(boot.settings.engine.archive.is_none());
        assert_eq!(boot.kept, vec!["engine.archive.dir".to_string()]);
    }

    #[tokio::test]
    async fn app_change_beneath_a_key_blocks_provisioning_of_the_section() {
        let store = store().await;
        store
            .set(&actor(), "engine.limits.max_height", json!(720))
            .await
            .unwrap();
        // A whole section provisioned as one value only happens through an import file
        // that stores it so; provisioning files always split sections into paths.
        store
            .set(&actor(), "local", json!({"max_bytes": 7}))
            .await
            .unwrap();
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"limits": {"max_height": 480}}, "local": {"max_bytes": 9}}),
            ))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.limits.max_height, 720);
        assert_eq!(boot.settings.local.max_bytes, 7);
        assert_eq!(
            boot.kept,
            vec!["engine.limits.max_height", "local.max_bytes"]
        );
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("engine.limits.max_height", Source::App),
                ("local", Source::App)
            ]
        );
    }

    #[tokio::test]
    async fn bad_provisioning_writes_nothing() {
        let store = store().await;
        store
            .set(&actor(), "engine.workers", json!(3))
            .await
            .unwrap();
        let error = store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": "many"}, "log": {"level": "warn"}}),
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Provisioning(_)));
        assert_eq!(store.entries().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_validates_before_committing() {
        let store = store().await;
        store
            .set(&actor(), "engine.workers", json!(3))
            .await
            .unwrap();
        let error = store
            .set(&actor(), "engine.workers", json!("many"))
            .await
            .unwrap_err();
        match error {
            SettingsError::Invalid { path, .. } => assert_eq!(path, "engine.workers"),
            other => panic!("unexpected {other}"),
        }
        assert_eq!(store.load().await.unwrap().engine.workers, 3);
        let error = store
            .set(&actor(), "engine.bogus", json!(1))
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }));
        assert_eq!(store.entries().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_replaces_values_around_and_beneath_the_key() {
        let store = store().await;
        store
            .set(&actor(), "engine.limits.max_height", json!(720))
            .await
            .unwrap();
        store
            .set(&actor(), "engine.limits.max_source_bytes", json!(10))
            .await
            .unwrap();
        let settings = store
            .set(&actor(), "engine.limits", json!({"max_height": 480}))
            .await
            .unwrap();
        assert_eq!(settings.engine.limits.max_height, 480);
        assert_eq!(
            settings.engine.limits.max_source_bytes,
            2 * 1024 * 1024 * 1024
        );
        let entries = store.entries().await.unwrap();
        assert_eq!(keys(&entries), vec![("engine.limits", Source::App)]);

        let settings = store
            .set(&actor(), "engine.limits.max_height", json!(360))
            .await
            .unwrap();
        assert_eq!(settings.engine.limits.max_height, 360);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![("engine.limits.max_height", Source::App)]
        );
    }

    #[tokio::test]
    async fn reset_removes_the_key_and_everything_beneath_it() {
        let store = store().await;
        store
            .bootstrap(&provisioning(json!({
                "engine": {"archive": {"dir": "a", "keep": "both"}, "workers": 5}
            })))
            .await
            .unwrap();
        let error = store
            .reset(&actor(), "engine.archive.dir")
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }));
        assert!(store.load().await.unwrap().engine.archive.is_some());

        let settings = store.reset(&actor(), "engine.archive").await.unwrap();
        assert!(settings.engine.archive.is_none());
        assert_eq!(settings.engine.workers, 5);
        let settings = store.reset(&actor(), "engine.workers").await.unwrap();
        assert_eq!(settings.engine.workers, 2);
        assert!(store.entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stored_values_round_trip_through_the_types() {
        let store = store().await;
        let settings = store
            .set(&actor(), "local", json!({"max_bytes": 3}))
            .await
            .unwrap();
        assert_eq!(settings.local.max_bytes, 3);
        let settings = store
            .set(&actor(),
                "auth.oidc",
                json!({"issuer": "https://issuer.example/", "client_id": "c", "client_secret": "s", "scopes": ["openid"]}),
            )
            .await
            .unwrap();
        let oidc = settings.auth.oidc.as_ref().unwrap();
        assert_eq!(oidc.name, "Single sign-on");
        assert_eq!(oidc.scopes, vec!["openid"]);
        let mut stored = std::collections::BTreeMap::new();
        crate::config::json_leaves(&serde_json::to_value(&settings).unwrap(), "", &mut stored);
        assert_eq!(stored["auth.oidc.scopes"], json!(["openid"]));
        let settings = store
            .set(&actor(), "web.bind", json!("0.0.0.0:9000"))
            .await
            .unwrap();
        assert_eq!(settings.web.bind.to_string(), "0.0.0.0:9000");
    }

    #[tokio::test]
    async fn export_reproduces_the_stored_values_in_every_format() {
        let store = store().await;
        store
            .bootstrap(&provisioning(json!({
                "auth": {"oidc": {"issuer": "https://issuer.example/", "client_id": "c", "client_secret": "s", "scopes": ["openid", "email"]}},
                "engine": {"workers": 5, "limits": {"max_height": 480}},
                "web": {"bind": "0.0.0.0:9000"}
            })))
            .await
            .unwrap();
        store
            .set(&actor(), "log.level", json!("debug"))
            .await
            .unwrap();
        let before = store.entries().await.unwrap();
        for format in Format::ALL {
            let text = store.export(format).await.unwrap();
            let fresh = self::store().await;
            fresh
                .bootstrap(&Provisioning::from_text(&text, format).unwrap())
                .await
                .unwrap();
            let after = fresh.entries().await.unwrap();
            let values = |entries: &[Entry]| -> Vec<(String, Json)> {
                entries
                    .iter()
                    .map(|e| (e.key.clone(), e.value.clone()))
                    .collect()
            };
            assert_eq!(values(&before), values(&after), "{format}");
        }
        assert_eq!(self::store().await.export(Format::Toml).await.unwrap(), "");
    }

    #[tokio::test]
    async fn export_to_toml_refuses_null_values() {
        let store = store().await;
        store
            .set(&actor(), "engine.limits.max_duration_secs", Json::Null)
            .await
            .unwrap();
        let error = store.export(Format::Toml).await.unwrap_err();
        assert!(matches!(
            error,
            SettingsError::Provisioning(ConfigError::Unrepresentable { .. })
        ));
        assert!(
            store
                .export(Format::Yaml)
                .await
                .unwrap()
                .contains("max_duration_secs")
        );
    }

    #[tokio::test]
    async fn import_overwrites_stored_values_as_app_changes() {
        let store = store().await;
        store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 4}, "log": {"level": "warn"}}),
            ))
            .await
            .unwrap();
        store
            .set(&actor(), "web.bind", json!("0.0.0.0:1"))
            .await
            .unwrap();
        let imported = store
            .import(
                &actor(),
                &Provisioning::from_text(
                    "[engine]\nworkers = 9\n[web]\nbind = \"0.0.0.0:2\"\n",
                    Format::Toml,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imported.keys, vec!["engine.workers", "web.bind"]);
        assert_eq!(imported.settings.engine.workers, 9);
        assert_eq!(imported.settings.web.bind.port(), 2);
        assert_eq!(imported.settings.log.level, "warn");
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("engine.workers", Source::App),
                ("log.level", Source::Provisioning),
                ("web.bind", Source::App)
            ]
        );
        // Provisioning no longer moves what the import wrote.
        let boot = store
            .bootstrap(&provisioning(json!({"engine": {"workers": 4}})))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.workers, 9);
        assert_eq!(boot.kept, vec!["engine.workers"]);
    }

    #[tokio::test]
    async fn bad_import_writes_nothing() {
        let store = store().await;
        store
            .set(&actor(), "engine.workers", json!(3))
            .await
            .unwrap();
        let error = store
            .import(
                &actor(),
                &Provisioning::from_text("[engine]\nworkers = 9\nbogus = 1\n", Format::Toml)
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Provisioning(_)));
        assert_eq!(store.load().await.unwrap().engine.workers, 3);
    }

    #[test]
    fn overlapping_rows_are_a_conflict() {
        let entries = vec![
            Entry {
                key: "engine".into(),
                value: json!(1),
                source: Source::App,
                updated_at: Timestamp::UNIX_EPOCH,
            },
            Entry {
                key: "engine.workers".into(),
                value: json!(1),
                source: Source::App,
                updated_at: Timestamp::UNIX_EPOCH,
            },
        ];
        assert!(matches!(
            assemble(&entries),
            Err(SettingsError::Conflict(_))
        ));
        let mut reversed = entries;
        reversed.reverse();
        assert!(matches!(
            assemble(&reversed),
            Err(SettingsError::Conflict(_))
        ));
    }

    #[test]
    fn related_paths() {
        assert!(related("a.b", "a.b"));
        assert!(related("a", "a.b"));
        assert!(related("a.b.c", "a"));
        assert!(!related("a.b", "a.bc"));
        assert!(!related("ab", "a"));
    }
}
