//! Database settings. Startup provisioning seeds values without replacing app edits.
//!
//! Rows store JSON values at dotted paths. Missing paths use defaults, sections derive
//! from child paths, and arrays remain single values.
//!
//! Changes and their audit entries share a transaction.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use discoclip_bot::DiscordSettings;
use discoclip_engine::rusqlite::{self, Connection, params};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::{EngineConfig, HttpConfig, StoreError};
use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use serde::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use url::Url;

use crate::audit::{self, Action, Actor, Target};
use crate::backup::BackupConfig;
use crate::config::{ConfigError, Format, Provisioning};
use crate::db::{nanos, timestamp, transact};
use crate::fixtures::FixtureConfig;
use crate::web::proxy::Network;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub log: LogConfig,
    pub engine: EngineConfig,
    /// How resolvers and downloaders reach the platforms: user agent, timeouts, retries,
    /// per-host rate limits and proxies.
    pub http: HttpConfig,
    pub local: LocalConfig,
    /// How long an upload to Discord gets and how often it is sent again
    pub discord: DiscordSettings,
    /// How often every platform's fixture links are resolved, and how long one may take.
    pub fixtures: FixtureConfig,
    pub web: WebConfig,
    pub auth: AuthConfig,
    /// When and where the database is backed up, and how many backups are kept.
    pub backup: BackupConfig,
}

impl Settings {
    /// Settings with every optional section present, so the type of every path can be read
    /// off them: what the environment's text is coerced against.
    pub fn exemplar() -> Self {
        let client = OAuthClient {
            client_id: String::new(),
            client_secret: SecretString::from(String::new()),
        };
        let mut settings = Settings::default();
        settings.engine.ffmpeg = Some(PathBuf::from("/usr/bin/ffmpeg"));
        settings.http.proxies.default =
            Some(Url::parse("http://proxy.invalid:3128").expect("valid"));
        settings.web.public_url = Some(Url::parse("https://example.invalid/").expect("valid"));
        settings.web.tls = Some(TlsConfig {
            cert: PathBuf::from("cert.pem"),
            key: PathBuf::from("key.pem"),
        });
        settings.auth.github = Some(client.clone());
        settings.auth.google = Some(client.clone());
        settings.auth.oidc = Some(OidcClient {
            name: default_oidc_name(),
            issuer: Url::parse("https://example.invalid/").expect("valid"),
            client_id: client.client_id,
            client_secret: client.client_secret,
            scopes: default_oidc_scopes(),
        });
        settings
    }

    /// Checks what the types alone cannot: values that parse but would not run.
    pub fn validate(&self) -> Result<(), SettingsError> {
        let invalid = |path: &str, message: String| SettingsError::Invalid {
            path: path.to_string(),
            message,
        };
        crate::telemetry::check_filter(&self.log.level)
            .map_err(|e| invalid("log.level", format!("not a tracing filter: {e}")))?;
        if self.engine.workers == 0 {
            return Err(invalid("engine.workers", "must be at least 1".into()));
        }
        if self.engine.download.connections == 0 {
            return Err(invalid(
                "engine.download.connections",
                "must be at least 1".into(),
            ));
        }
        if self.engine.download.chunk_bytes < 64 * 1024 {
            return Err(invalid(
                "engine.download.chunk_bytes",
                "must be at least 65536".into(),
            ));
        }
        if self.engine.retention.sweep_interval_secs == 0 {
            return Err(invalid(
                "engine.retention.sweep_interval_secs",
                "must be at least 1".into(),
            ));
        }
        if self.engine.cache_dir.as_os_str().is_empty() {
            return Err(invalid("engine.cache_dir", "cannot be empty".into()));
        }
        if self
            .engine
            .ffmpeg
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(invalid("engine.ffmpeg", "cannot be empty".into()));
        }
        if self.engine.transcode.vaapi_device.as_os_str().is_empty() {
            return Err(invalid(
                "engine.transcode.vaapi_device",
                "cannot be empty".into(),
            ));
        }
        self.discord
            .check()
            .map_err(|(path, message)| invalid(&path, message))?;
        if self.engine.archive.dir.as_os_str().is_empty() {
            return Err(invalid("engine.archive.dir", "cannot be empty".into()));
        }
        if self.http.user_agent.trim().is_empty() {
            return Err(invalid("http.user_agent", "cannot be empty".into()));
        }
        if self.http.retry.attempts == 0 {
            return Err(invalid("http.retry.attempts", "must be at least 1".into()));
        }
        if self.http.rate_limits.default.per_second < 0.0 {
            return Err(invalid(
                "http.rate_limits.default.per_second",
                "cannot be negative".into(),
            ));
        }
        for (host, rate) in &self.http.rate_limits.hosts {
            if rate.per_second < 0.0 {
                return Err(invalid(
                    &format!("http.rate_limits.hosts.{host}.per_second"),
                    "cannot be negative".into(),
                ));
            }
        }
        for (name, proxy) in self
            .http
            .proxies
            .default
            .iter()
            .map(|p| ("http.proxies.default".to_string(), p))
            .chain(
                self.http
                    .proxies
                    .platforms
                    .iter()
                    .map(|(k, p)| (format!("http.proxies.platforms.{k}"), p)),
            )
            .chain(
                self.http
                    .proxies
                    .hosts
                    .iter()
                    .map(|(k, p)| (format!("http.proxies.hosts.{k}"), p)),
            )
        {
            if !matches!(
                proxy.scheme(),
                "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a"
            ) {
                return Err(invalid(
                    &name,
                    format!("{} is not an http or socks proxy URL", proxy.scheme()),
                ));
            }
        }
        if self.backup.dir.as_os_str().is_empty() {
            return Err(invalid("backup.dir", "cannot be empty".into()));
        }
        if self.backup.keep == 0 {
            return Err(invalid("backup.keep", "must be at least 1".into()));
        }
        if self.backup.interval_secs == 0 {
            return Err(invalid("backup.interval_secs", "must be at least 1".into()));
        }
        if self.local.dir.as_os_str().is_empty() {
            return Err(invalid("local.dir", "cannot be empty".into()));
        }
        if self.fixtures.timeout_secs == 0 {
            return Err(invalid(
                "fixtures.timeout_secs",
                "must be at least 1".into(),
            ));
        }
        if let Some(tls) = &self.web.tls {
            if tls.cert.as_os_str().is_empty() {
                return Err(invalid("web.tls.cert", "cannot be empty".into()));
            }
            if tls.key.as_os_str().is_empty() {
                return Err(invalid("web.tls.key", "cannot be empty".into()));
            }
        }
        if let Some(url) = &self.web.public_url
            && !matches!(url.scheme(), "http" | "https")
        {
            return Err(invalid(
                "web.public_url",
                "must be an http or https URL".into(),
            ));
        }
        Ok(())
    }
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

/// Where jobs submitted from the web app are published. What they are made to is the
/// built-in profile's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub dir: PathBuf,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("data/local"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub bind: SocketAddr,
    /// How browsers reach the app, such as `https://clips.example.com`. Login providers
    /// send them back here. Without it the address a request arrived at is used.
    pub public_url: Option<Url>,
    /// Serve HTTPS from these PEM files. Without them the app speaks plain HTTP, as it
    /// does behind a reverse proxy that terminates TLS.
    pub tls: Option<TlsConfig>,
    /// Addresses and networks of reverse proxies in front of the app. A request that
    /// arrives from one of them is read for the `Forwarded`, `X-Forwarded-For`,
    /// `X-Forwarded-Proto` and `X-Forwarded-Host` headers the proxy adds, so sessions,
    /// rate limits, the audit log and login callbacks see the browser rather than the
    /// proxy. Anything else on the wire keeps its own address.
    pub trusted_proxies: Vec<Network>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            public_url: None,
            tls: None,
            trusted_proxies: Vec::new(),
        }
    }
}

/// A certificate chain and its private key, both PEM. The files are read again when they
/// change on disk, so a renewed certificate takes effect without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Who wrote a stored value last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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

/// Keys whose values are stored whole rather than split into the paths beneath them:
/// maps keyed by host names, whose keys carry dots of their own.
pub const ATOMIC_KEYS: [&str; 3] = [
    "http.rate_limits.hosts",
    "http.proxies.platforms",
    "http.proxies.hosts",
];

/// Keys that used to be settings and are profile values now
pub const REMOVED_KEYS: [&str; 8] = [
    "engine.limits",
    "engine.live",
    "engine.playlists",
    "local.max_bytes",
    "local.target",
    "discord.limits",
    "discord.target",
    "discord.guilds",
];

/// The removed key `path` is, or lies beneath
pub fn removed_key(path: &str) -> Option<&'static str> {
    REMOVED_KEYS.iter().copied().find(|removed| {
        path == *removed
            || path
                .strip_prefix(removed)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// What a change to a removed key is answered with
pub const MOVED_INTO_PROFILES: &str =
    "moved into profiles: set it on the Default profile or on a profile assigned where it applies";

/// The atomic key `path` is, or lies beneath.
pub fn atomic_key(path: &str) -> Option<&'static str> {
    ATOMIC_KEYS.iter().copied().find(|atomic| {
        path == *atomic
            || path
                .strip_prefix(atomic)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// The paths the store keeps `value` at when it is written to `key`: every leaf beneath
/// an object, arrays and atomic maps whole.
pub fn leaves_of(key: &str, value: &Json) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    crate::config::json_leaves(value, key, &mut out);
    out
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

/// Keys to store and keys to remove, applied together and checked as a whole.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Change {
    pub set: BTreeMap<String, Json>,
    pub reset: Vec<String>,
}

impl Change {
    pub fn set(key: &str, value: Json) -> Self {
        Self {
            set: BTreeMap::from([(key.to_string(), value)]),
            reset: Vec::new(),
        }
    }

    pub fn reset(key: &str) -> Self {
        Self {
            set: BTreeMap::new(),
            reset: vec![key.to_string()],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.reset.is_empty()
    }

    /// The keys the change touches, sets first.
    pub fn keys(&self) -> Vec<String> {
        self.set
            .keys()
            .cloned()
            .chain(self.reset.iter().cloned())
            .collect()
    }

    fn check_keys(&self) -> Result<(), SettingsError> {
        for key in self.keys() {
            if key.is_empty() || key.split('.').any(|segment| segment.is_empty()) {
                return Err(SettingsError::Invalid {
                    path: key,
                    message: "is not a dotted settings path".into(),
                });
            }
            if let Some(atomic) = atomic_key(&key)
                && atomic != key
            {
                return Err(SettingsError::Invalid {
                    path: key.clone(),
                    message: format!("is part of {atomic}, which is set as a whole"),
                });
            }
            if removed_key(&key).is_some() {
                return Err(SettingsError::Invalid {
                    path: key.clone(),
                    message: MOVED_INTO_PROFILES.into(),
                });
            }
        }
        Ok(())
    }
}

/// The settings after a change: what the server now runs on and the keys written.
#[derive(Debug, Clone)]
pub struct Changed {
    pub settings: Settings,
    /// The leaf keys stored.
    pub written: Vec<String>,
    /// The keys removed.
    pub removed: Vec<String>,
}

/// One stored value as the app shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryView {
    pub key: String,
    pub source: Source,
    pub updated_at: Timestamp,
}

/// The settings as the app edits them: what the server runs on with secrets withheld,
/// the defaults beside them, which values are stored and by whom, and which secrets
/// are set.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub settings: Json,
    pub defaults: Json,
    /// Settings with every optional section filled in with placeholder values, so the
    /// shape and type of every path can be read even where `defaults` holds `null`.
    pub exemplar: Json,
    pub entries: Vec<EntryView>,
    /// Secret keys that hold a value. Their values are withheld from `settings`.
    pub secrets: Vec<String>,
    /// Every key that is a secret, set or not.
    pub secret_keys: Vec<String>,
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
            let mut kept = provisioning.removed();
            for key in &kept {
                tracing::warn!(
                    key,
                    "provisioned setting ignored: it is a profile value now"
                );
            }
            let mut written = Vec::new();
            for (key, value) in values {
                let changed_in_app = existing
                    .iter()
                    .any(|entry| entry.source == Source::App && related(&entry.key, &key));
                if changed_in_app {
                    kept.push(key);
                    continue;
                }
                written.extend(write(tx, &key, &value, Source::Provisioning, now)?);
            }
            let after = read_entries(tx)?;
            log_leaves(
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
    ) -> Result<Changed, SettingsError> {
        let change = self.import_change(provisioning).await?;
        self.apply(actor, Action::SettingsImport, &change).await
    }

    /// The change an import of `provisioning` produces, over what is stored.
    pub async fn import_change(
        &self,
        provisioning: &Provisioning,
    ) -> Result<Change, SettingsError> {
        let provisioning = provisioning.clone();
        transact(&self.db, move |tx| {
            let existing = read_entries(tx)?;
            for key in provisioning.removed() {
                tracing::warn!(key, "imported setting ignored: it is a profile value now");
            }
            let set = provisioning.resolve(&assemble(&existing)?)?;
            Ok(Change {
                set,
                reset: Vec::new(),
            })
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

    /// The settings as the app shows them.
    pub async fn view(&self) -> Result<View, SettingsError> {
        transact(&self.db, |tx| {
            let entries = read_entries(tx)?;
            let settings = decode(&entries)?;
            Ok(view_of(&settings, &entries))
        })
        .await
    }

    /// What the settings would be after `change`, without storing anything.
    pub async fn preview(&self, change: &Change) -> Result<Settings, SettingsError> {
        change.check_keys()?;
        let change = change.clone();
        transact(&self.db, move |tx| {
            let before = read_entries(tx)?;
            let change = with_stored_secrets(change, &before);
            let mut rows: BTreeMap<String, Json> = before
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect();
            for key in &change.reset {
                rows.retain(|stored, _| !(stored == key || beneath(stored, key)));
            }
            for (key, value) in &change.set {
                rows.retain(|stored, _| !related(stored, key));
                rows.extend(leaves_of(key, value));
            }
            let entries: Vec<Entry> = rows
                .into_iter()
                .map(|(key, value)| Entry {
                    key,
                    value,
                    source: Source::App,
                    updated_at: Timestamp::UNIX_EPOCH,
                })
                .collect();
            decode(&entries)
        })
        .await
    }

    /// Stores `change` as done by `actor` in one transaction, logged as `action`: every
    /// key set is written as the leaves beneath it, replacing whatever was stored at, around
    /// or beneath it. Every key reset is removed with everything beneath it. Nothing is
    /// written unless the settings as a whole stay valid. The new settings are returned.
    pub async fn apply(
        &self,
        actor: &Actor,
        action: Action,
        change: &Change,
    ) -> Result<Changed, SettingsError> {
        change.check_keys()?;
        let change = change.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let before = read_entries(tx)?;
            let before_tree = assemble(&before)?;
            let change = with_stored_secrets(change, &before);
            let now = Timestamp::now();
            let mut removed = Vec::new();
            for key in &change.reset {
                removed.extend(remove(tx, key)?);
            }
            let mut written = Vec::new();
            for (key, value) in &change.set {
                written.extend(write(tx, key, value, Source::App, now)?);
            }
            let after = read_entries(tx)?;
            let settings = decode(&after)?;
            for key in &change.reset {
                let gone = removed_around(key, &before, &after, true);
                if gone.is_empty() {
                    continue;
                }
                audit::record(
                    tx,
                    &actor,
                    Action::SettingsReset,
                    Target::setting(key),
                    audit::settings_details(key, at_path(&before_tree, key), None, &gone),
                )?;
            }
            for (key, value) in &change.set {
                let previous = at_path(&before_tree, key);
                let leaves = leaves_of(key, value);
                let gone: BTreeMap<String, Json> = removed_around(key, &before, &after, true)
                    .into_iter()
                    .filter(|(gone, _)| !leaves.contains_key(gone))
                    .collect();
                let unchanged = previous == Some(value)
                    && gone.is_empty()
                    && leaves.keys().all(|leaf| {
                        before
                            .iter()
                            .any(|entry| &entry.key == leaf && entry.source == Source::App)
                    });
                if unchanged {
                    continue;
                }
                audit::record(
                    tx,
                    &actor,
                    action,
                    Target::setting(key),
                    audit::settings_details(key, previous, Some(value), &gone),
                )?;
            }
            Ok(Changed {
                settings,
                written,
                removed,
            })
        })
        .await
    }

    /// Stores `value` at `key` as an app change by `actor`.
    pub async fn set(
        &self,
        actor: &Actor,
        key: &str,
        value: Json,
    ) -> Result<Settings, SettingsError> {
        Ok(self
            .apply(actor, Action::SettingsSet, &Change::set(key, value))
            .await?
            .settings)
    }

    /// Removes the stored value at `key` and everything beneath it, so the defaults apply.
    pub async fn reset(&self, actor: &Actor, key: &str) -> Result<Settings, SettingsError> {
        Ok(self
            .apply(actor, Action::SettingsReset, &Change::reset(key))
            .await?
            .settings)
    }
}

/// Whether one path is the other, contains it, or lies beneath it.
fn related(a: &str, b: &str) -> bool {
    a == b || beneath(a, b) || beneath(b, a)
}

/// Whether `path` lies strictly beneath `ancestor`.
fn beneath(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// The value at the dotted `path` of `tree`, when there is one.
fn at_path<'a>(tree: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.')
        .try_fold(tree, |node, segment| node.get(segment))
}

/// The values stored around or beneath `key` before that are gone after. `key` itself
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

/// Logs what writing the leaf `keys` changed, one entry per key: the value before and
/// after, and the values stored around it that the write removed. A key the write left
/// exactly as it was, in value and source, is not logged.
fn log_leaves(
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

/// Removes `key` and every row beneath it. The keys removed.
fn remove(conn: &Connection, key: &str) -> Result<Vec<String>, SettingsError> {
    let mut stmt = conn.prepare(
        "DELETE FROM settings WHERE key = ?1 OR substr(key, 1, length(?1) + 1) = ?1 || '.' RETURNING key",
    )?;
    let rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Stores `value` at `key` as the leaves beneath it, removing any value stored at a section
/// around it or a path beneath it. The leaf keys written.
fn write(
    conn: &Connection,
    key: &str,
    value: &Json,
    source: Source,
    now: Timestamp,
) -> Result<Vec<String>, SettingsError> {
    conn.execute(
        "DELETE FROM settings WHERE
            substr(key, 1, length(?1) + 1) = ?1 || '.' OR
            substr(?1, 1, length(key) + 1) = key || '.'",
        params![key],
    )?;
    let leaves = leaves_of(key, value);
    if leaves.is_empty() {
        conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        return Ok(Vec::new());
    }
    if !leaves.contains_key(key) {
        conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
    }
    let mut written = Vec::with_capacity(leaves.len());
    for (leaf, value) in leaves {
        let encoded =
            serde_json::to_string(&value).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        conn.execute(
            "INSERT INTO settings (key, value, source, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                source = excluded.source,
                updated_at = excluded.updated_at
             WHERE settings.value != excluded.value OR settings.source != excluded.source",
            params![leaf, encoded, source.as_str(), nanos(now)],
        )?;
        written.push(leaf);
    }
    Ok(written)
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
    let settings: Settings =
        serde_path_to_error::deserialize(assemble(entries)?).map_err(|error| {
            SettingsError::Invalid {
                path: error.path().to_string(),
                message: error.into_inner().to_string(),
            }
        })?;
    settings.validate()?;
    Ok(settings)
}

/// Every leaf of `tree` that is a secret, whether or not it holds a value.
fn secret_paths(tree: &Json) -> Vec<String> {
    let mut leaves = BTreeMap::new();
    crate::config::json_leaves(tree, "", &mut leaves);
    leaves
        .into_keys()
        .filter(|key| key.split('.').any(audit::is_secret_name))
        .collect()
}

/// The change with every secret it sends as `null` replaced by the stored one, so the
/// settings it previews and applies are the same.
fn with_stored_secrets(mut change: Change, before: &[Entry]) -> Change {
    for (key, value) in change.set.iter_mut() {
        keep_stored_secrets(key, value, before);
    }
    change
}

/// Puts the stored value back into every secret beneath `key` that `value` sends as
/// `null`. The app reads secrets withheld as `null`, so sending a section back unchanged
/// keeps its secret rather than erasing it.
fn keep_stored_secrets(key: &str, value: &mut Json, before: &[Entry]) {
    let stored = |leaf: &str| before.iter().find(|entry| entry.key == leaf);
    if key.split('.').any(audit::is_secret_name) {
        if value.is_null()
            && let Some(entry) = stored(key)
        {
            *value = entry.value.clone();
        }
        return;
    }
    let leaves = leaves_of(key, value);
    for (leaf, sent) in &leaves {
        if !sent.is_null() || !leaf.split('.').any(audit::is_secret_name) {
            continue;
        }
        let Some(entry) = stored(leaf) else {
            continue;
        };
        let Some(rest) = leaf
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        if let Some(slot) = rest
            .split('.')
            .try_fold(&mut *value, |node, segment| node.get_mut(segment))
        {
            *slot = entry.value.clone();
        }
    }
}

/// The settings tree with every secret withheld, and the keys of the secrets that are set.
fn withhold_secrets(tree: &Json) -> (Json, Vec<String>) {
    let mut leaves = BTreeMap::new();
    crate::config::json_leaves(tree, "", &mut leaves);
    let mut secrets = Vec::new();
    let mut withheld = tree.clone();
    for (key, value) in &leaves {
        if !key.split('.').any(audit::is_secret_name) {
            continue;
        }
        let set = match value {
            Json::Null => false,
            Json::String(text) => !text.is_empty(),
            _ => true,
        };
        if set {
            secrets.push(key.clone());
        }
        if let Some(slot) = key
            .split('.')
            .try_fold(&mut withheld, |node, segment| node.get_mut(segment))
        {
            *slot = Json::Null;
        }
    }
    (withheld, secrets)
}

fn view_of(settings: &Settings, entries: &[Entry]) -> View {
    let tree = serde_json::to_value(settings).unwrap_or(Json::Null);
    let (withheld, secrets) = withhold_secrets(&tree);
    let (defaults, _) =
        withhold_secrets(&serde_json::to_value(Settings::default()).unwrap_or(Json::Null));
    let full = serde_json::to_value(Settings::exemplar()).unwrap_or(Json::Null);
    let secret_keys = secret_paths(&full);
    let (exemplar, _) = withhold_secrets(&full);
    View {
        settings: withheld,
        defaults,
        exemplar,
        secret_keys,
        entries: entries
            .iter()
            .map(|entry| EntryView {
                key: entry.key.clone(),
                source: entry.source,
                updated_at: entry.updated_at,
            })
            .collect(),
        secrets,
    }
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
        assert_eq!(settings.backup.keep, 7);
        assert_eq!(settings.web.bind.port(), 8080);
        assert!(store.entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bootstrap_writes_only_provisioned_keys() {
        let store = store().await;
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 4}, "backup": {"keep": 5}}),
            ))
            .await
            .unwrap();
        assert!(boot.kept.is_empty());
        assert_eq!(boot.settings.engine.workers, 4);
        assert_eq!(boot.settings.engine.retention.jobs_days, 90);
        assert_eq!(boot.settings.backup.keep, 5);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("backup.keep", Source::Provisioning),
                ("engine.workers", Source::Provisioning)
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
        let archive = boot.settings.engine.archive;
        assert_eq!(archive.dir, std::path::PathBuf::from("from-app"));
        assert_eq!(archive.keep, discoclip_engine::archive::Keep::Both);
        assert!(archive.enabled);
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
        // A stored null, from before the archive had a switch, still turns it off.
        let settings = store
            .set(&actor(), "engine.archive", Json::Null)
            .await
            .unwrap();
        assert!(!settings.engine.archive.enabled);
        let entries = store.entries().await.unwrap();
        assert_eq!(keys(&entries), vec![("engine.archive", Source::App)]);

        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"archive": {"dir": "archive"}}}),
            ))
            .await
            .unwrap();
        assert!(!boot.settings.engine.archive.enabled);
        assert_eq!(boot.kept, vec!["engine.archive.dir".to_string()]);
    }

    #[tokio::test]
    async fn app_change_beneath_a_key_blocks_provisioning_of_the_section() {
        let store = store().await;
        store
            .set(&actor(), "engine.retention.jobs_days", json!(45))
            .await
            .unwrap();
        // A section set whole is stored as the paths beneath it.
        store
            .set(&actor(), "backup", json!({"keep": 7}))
            .await
            .unwrap();
        let boot = store
            .bootstrap(&provisioning(
                json!({"engine": {"retention": {"jobs_days": 30}}, "backup": {"keep": 9}}),
            ))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.retention.jobs_days, 45);
        assert_eq!(boot.settings.backup.keep, 7);
        assert_eq!(boot.kept, vec!["backup.keep", "engine.retention.jobs_days"]);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("backup.keep", Source::App),
                ("engine.retention.jobs_days", Source::App)
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
            .set(&actor(), "engine.retention.jobs_days", json!(45))
            .await
            .unwrap();
        store
            .set(&actor(), "engine.retention.failed_jobs_days", json!(10))
            .await
            .unwrap();
        let settings = store
            .set(&actor(), "engine.retention", json!({"jobs_days": 30}))
            .await
            .unwrap();
        assert_eq!(settings.engine.retention.jobs_days, 30);
        assert_eq!(settings.engine.retention.failed_jobs_days, 30);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![("engine.retention.jobs_days", Source::App)]
        );

        let settings = store
            .set(&actor(), "engine.retention.jobs_days", json!(15))
            .await
            .unwrap();
        assert_eq!(settings.engine.retention.jobs_days, 15);
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![("engine.retention.jobs_days", Source::App)]
        );
    }

    #[tokio::test]
    async fn reset_removes_the_key_and_everything_beneath_it() {
        let store = store().await;
        store
            .bootstrap(&provisioning(json!({
                "web": {"tls": {"cert": "a.pem", "key": "b.pem"}}, "engine": {"workers": 5}
            })))
            .await
            .unwrap();
        let error = store.reset(&actor(), "web.tls.cert").await.unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }));
        assert!(store.load().await.unwrap().web.tls.is_some());

        let settings = store.reset(&actor(), "web.tls").await.unwrap();
        assert!(settings.web.tls.is_none());
        assert_eq!(settings.engine.workers, 5);
        let settings = store.reset(&actor(), "engine.workers").await.unwrap();
        assert_eq!(settings.engine.workers, 2);
        assert!(store.entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stored_values_round_trip_through_the_types() {
        let store = store().await;
        let settings = store
            .set(&actor(), "backup", json!({"keep": 3}))
            .await
            .unwrap();
        assert_eq!(settings.backup.keep, 3);
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
                "engine": {"workers": 5, "retention": {"jobs_days": 45}},
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
            .set(&actor(), "engine.ffmpeg", Json::Null)
            .await
            .unwrap();
        let error = store.export(Format::Toml).await.unwrap_err();
        assert!(matches!(
            error,
            SettingsError::Provisioning(ConfigError::Unrepresentable { .. })
        ));
        assert!(store.export(Format::Yaml).await.unwrap().contains("ffmpeg"));
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
        assert_eq!(imported.written, vec!["engine.workers", "web.bind"]);
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

    #[tokio::test]
    async fn semantic_checks_refuse_values_that_would_not_run() {
        let store = store().await;
        for (key, value) in [
            ("log.level", json!("not a [filter")),
            ("engine.workers", json!(0)),
            ("engine.download.connections", json!(0)),
            ("engine.retention.sweep_interval_secs", json!(0)),
            ("http.retry.attempts", json!(0)),
            ("http.proxies.default", json!("ftp://proxy:1")),
            ("backup.keep", json!(0)),
            ("fixtures.timeout_secs", json!(0)),
            ("web.public_url", json!("ftp://clips.example.com")),
            ("web.tls", json!({"cert": "", "key": "k"})),
        ] {
            let error = store.set(&actor(), key, value).await.unwrap_err();
            assert!(
                matches!(error, SettingsError::Invalid { .. }),
                "{key}: {error}"
            );
        }
        assert!(store.entries().await.unwrap().is_empty());
        store
            .set(&actor(), "log.level", json!("info,discoclip_engine=debug"))
            .await
            .unwrap();
        store
            .set(
                &actor(),
                "http.proxies.default",
                json!("socks5h://proxy:1080"),
            )
            .await
            .unwrap();
        let settings = store.load().await.unwrap();
        assert_eq!(settings.http.proxies.default.unwrap().scheme(), "socks5h");
    }

    #[tokio::test]
    async fn removed_keys_are_refused_and_ignored_by_provisioning() {
        let store = store().await;
        let error = store
            .set(&actor(), "engine.limits.max_height", json!(720))
            .await
            .unwrap_err();
        match error {
            SettingsError::Invalid { path, message } => {
                assert_eq!(path, "engine.limits.max_height");
                assert!(message.contains("moved into profiles"), "{message}");
            }
            other => panic!("unexpected {other}"),
        }
        for key in [
            "discord.guilds",
            "local.target.container",
            "engine.playlists",
        ] {
            assert!(
                store.set(&actor(), key, json!(1)).await.is_err(),
                "{key} still accepted"
            );
        }
        let boot = store
            .bootstrap(&provisioning(json!({
                "engine": {"workers": 4, "limits": {"max_height": 480}},
                "discord": {"limits": {"base_bytes": 1}}
            })))
            .await
            .unwrap();
        assert_eq!(boot.settings.engine.workers, 4);
        assert_eq!(
            boot.kept,
            vec!["discord.limits.base_bytes", "engine.limits.max_height"]
        );
        assert_eq!(
            keys(&store.entries().await.unwrap()),
            vec![("engine.workers", Source::Provisioning)]
        );
        let imported = store
            .import(
                &actor(),
                &Provisioning::from_text(
                    "[engine]\nworkers = 9\n[local]\nmax_bytes = 5\n",
                    Format::Toml,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imported.written, vec!["engine.workers"]);
        assert_eq!(imported.settings.engine.workers, 9);
    }

    #[tokio::test]
    async fn maps_keyed_by_hosts_are_stored_whole() {
        let store = store().await;
        let settings = store
            .set(
                &actor(),
                "http.rate_limits.hosts",
                json!({"youtube.com": {"per_second": 1.0, "burst": 2}}),
            )
            .await
            .unwrap();
        assert_eq!(
            settings.http.rate_limits.hosts["youtube.com"].per_second,
            1.0
        );
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![("http.rate_limits.hosts", Source::App)]
        );
        let error = store
            .set(
                &actor(),
                "http.rate_limits.hosts.youtube.com.burst",
                json!(3),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { path, .. } if path == "http.rate_limits.hosts.youtube.com.burst")
        );
        // Provisioning the map adds to the stored hosts unless the app changed them.
        let fresh = self::store().await;
        fresh
            .bootstrap(&provisioning(
                json!({"http": {"proxies": {"hosts": {"a.test": "socks5://p:1"}}}}),
            ))
            .await
            .unwrap();
        let boot = fresh
            .bootstrap(&provisioning(
                json!({"http": {"proxies": {"hosts": {"b.test": "http://q:2"}}}}),
            ))
            .await
            .unwrap();
        assert_eq!(boot.settings.http.proxies.hosts.len(), 2);
        let entries = fresh.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![("http.proxies.hosts", Source::Provisioning)]
        );
        for format in Format::ALL {
            let text = fresh.export(format).await.unwrap();
            assert!(text.contains("a.test"), "{format}: {text}");
            let again = self::store().await;
            again
                .bootstrap(&Provisioning::from_text(&text, format).unwrap())
                .await
                .unwrap();
            assert_eq!(again.load().await.unwrap().http.proxies.hosts.len(), 2);
        }
    }

    #[tokio::test]
    async fn changes_set_and_reset_together_after_a_preview() {
        let store = store().await;
        store
            .bootstrap(&provisioning(
                json!({"engine": {"workers": 4}, "log": {"level": "warn"}}),
            ))
            .await
            .unwrap();
        let change = Change {
            set: BTreeMap::from([
                ("engine.workers".to_string(), json!(6)),
                (
                    "web.tls".to_string(),
                    json!({"cert": "c.pem", "key": "k.pem"}),
                ),
            ]),
            reset: vec!["log.level".into()],
        };
        let preview = store.preview(&change).await.unwrap();
        assert_eq!(preview.engine.workers, 6);
        assert_eq!(preview.log.level, "info");
        assert_eq!(preview.web.tls.unwrap().cert, PathBuf::from("c.pem"));
        assert_eq!(store.load().await.unwrap().engine.workers, 4);
        assert_eq!(store.entries().await.unwrap().len(), 2);

        let changed = store
            .apply(&actor(), Action::SettingsSet, &change)
            .await
            .unwrap();
        assert_eq!(
            changed.written,
            vec!["engine.workers", "web.tls.cert", "web.tls.key"]
        );
        assert_eq!(changed.removed, vec!["log.level"]);
        assert_eq!(changed.settings.engine.workers, 6);
        assert_eq!(changed.settings.log.level, "info");
        let entries = store.entries().await.unwrap();
        assert_eq!(
            keys(&entries),
            vec![
                ("engine.workers", Source::App),
                ("web.tls.cert", Source::App),
                ("web.tls.key", Source::App)
            ]
        );
        // A bad key in a change writes nothing of it.
        let bad = Change {
            set: BTreeMap::from([
                ("engine.workers".to_string(), json!(1)),
                ("engine.bogus".to_string(), json!(1)),
            ]),
            reset: Vec::new(),
        };
        assert!(store.preview(&bad).await.is_err());
        assert!(
            store
                .apply(&actor(), Action::SettingsSet, &bad)
                .await
                .is_err()
        );
        assert_eq!(store.load().await.unwrap().engine.workers, 6);
        assert!(matches!(
            store.preview(&Change::set("", json!(1))).await,
            Err(SettingsError::Invalid { .. })
        ));
        assert!(matches!(
            store.preview(&Change::set("a..b", json!(1))).await,
            Err(SettingsError::Invalid { .. })
        ));
        // Writing beneath a section stored as null replaces the null.
        store
            .set(&actor(), "engine.archive", Json::Null)
            .await
            .unwrap();
        let settings = store
            .set(&actor(), "engine.archive.dir", json!("arch"))
            .await
            .unwrap();
        assert_eq!(settings.engine.archive.dir, PathBuf::from("arch"));
        assert!(settings.engine.archive.enabled);

        assert!(
            store
                .entries()
                .await
                .unwrap()
                .iter()
                .all(|entry| entry.key != "engine.archive")
        );
    }

    #[tokio::test]
    async fn views_withhold_secrets_and_name_the_ones_set() {
        let store = store().await;
        let view = store.view().await.unwrap();
        assert!(view.secrets.is_empty());
        assert!(view.entries.is_empty());
        assert_eq!(view.settings["engine"]["workers"], 2);
        assert_eq!(view.defaults["engine"]["workers"], 2);
        assert_eq!(view.settings["auth"]["github"], Json::Null);
        store
            .apply(
                &actor(),
                Action::SettingsSet,
                &Change {
                    set: BTreeMap::from([
                        ("auth.github.client_id".to_string(), json!("id")),
                        ("auth.github.client_secret".to_string(), json!("s3cret")),
                        ("engine.workers".to_string(), json!(3)),
                    ]),
                    reset: Vec::new(),
                },
            )
            .await
            .unwrap();
        let view = store.view().await.unwrap();
        assert_eq!(view.settings["auth"]["github"]["client_id"], "id");
        assert_eq!(view.settings["auth"]["github"]["client_secret"], Json::Null);
        assert_eq!(view.secrets, vec!["auth.github.client_secret"]);
        assert!(!serde_json::to_string(&view).unwrap().contains("s3cret"));
        let stored: Vec<&str> = view.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            stored,
            vec![
                "auth.github.client_id",
                "auth.github.client_secret",
                "engine.workers"
            ]
        );
        assert!(view.entries.iter().all(|e| e.source == Source::App));
        assert_eq!(view.exemplar["auth"]["github"]["client_id"], "");
        assert_eq!(view.exemplar["auth"]["github"]["client_secret"], Json::Null);
        assert_eq!(view.exemplar["web"]["tls"]["cert"], "cert.pem");
        assert_eq!(view.exemplar["engine"]["archive"]["keep"], "output");
        assert_eq!(
            view.secret_keys,
            vec![
                "auth.github.client_secret",
                "auth.google.client_secret",
                "auth.oidc.client_secret"
            ]
        );
    }

    #[tokio::test]
    async fn a_secret_sent_as_null_keeps_the_stored_one() {
        let store = store().await;
        store
            .set(
                &actor(),
                "auth.github",
                json!({"client_id": "id", "client_secret": "s3cret"}),
            )
            .await
            .unwrap();
        let settings = store
            .set(
                &actor(),
                "auth.github",
                json!({"client_id": "new-id", "client_secret": null}),
            )
            .await
            .unwrap();
        let github = settings.auth.github.as_ref().unwrap();
        assert_eq!(github.client_id, "new-id");
        assert_eq!(github.client_secret.expose_secret(), "s3cret");
        let previewed = store
            .preview(&Change::set(
                "auth.github",
                json!({"client_id": "newer-id", "client_secret": null}),
            ))
            .await
            .unwrap();
        assert_eq!(
            previewed.auth.github.unwrap().client_secret.expose_secret(),
            "s3cret"
        );
        let settings = store
            .set(&actor(), "auth.github.client_secret", Json::Null)
            .await
            .unwrap();
        assert_eq!(
            settings.auth.github.unwrap().client_secret.expose_secret(),
            "s3cret"
        );
        let missing = store
            .set(
                &actor(),
                "auth.google",
                json!({"client_id": "g", "client_secret": null}),
            )
            .await;
        assert!(
            missing.is_err(),
            "a section with no secret stored cannot keep one"
        );
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
