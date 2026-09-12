//! Provisioning input: one config file, then `DISCOCLIP_*` environment variables, the
//! latter overriding the former. Only keys that are actually set count as provisioned; they
//! seed the settings store at startup through `settings::bootstrap`, and the running server
//! reads the store, never this. The same file formats carry exported settings back out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::settings::Settings;

/// Environment variables `DISCOCLIP_<SECTION>__<KEY>` override config file values.
pub const ENV_PREFIX: &str = "DISCOCLIP_";
/// Names the config file, like `--config`.
pub const CONFIG_PATH_VAR: &str = "DISCOCLIP_CONFIG";
/// The directory holding the database: provisioned like every other key, but never stored,
/// since the database is inside it.
pub const DATA_DIR_KEY: &str = "data_dir";
pub const DEFAULT_DATA_DIR: &str = "data";
const FILE_STEM: &str = "discoclip";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not a .toml, .yaml, .yml or .json file")]
    UnknownFormat(PathBuf),
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{format} in {origin}: {message}")]
    Parse {
        format: Format,
        origin: String,
        message: String,
    },
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error("could not encode settings: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("{0} is provisioned but no setting holds it")]
    Unplaced(String),
    #[error("{key} is null, which {format} cannot express; export as YAML or JSON instead")]
    Unrepresentable { key: String, format: Format },
    #[error("could not write {format}: {message}")]
    Render { format: Format, message: String },
}

/// A provisioning file format, by extension.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Toml,
    Yaml,
    Json,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Toml, Format::Yaml, Format::Json];

    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "toml" => Some(Format::Toml),
            "yaml" | "yml" => Some(Format::Yaml),
            "json" => Some(Format::Json),
            _ => None,
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Toml => &["toml"],
            Format::Yaml => &["yaml", "yml"],
            Format::Json => &["json"],
        }
    }

    /// Reads a file in this format as a JSON tree; a file with nothing in it is an
    /// empty tree.
    pub fn parse(self, text: &str, origin: &str) -> Result<Json, ConfigError> {
        let failed = |e: &dyn std::fmt::Display| ConfigError::Parse {
            format: self,
            origin: origin.to_string(),
            message: e.to_string(),
        };
        if text.trim().is_empty() {
            return Ok(Json::Object(serde_json::Map::new()));
        }
        let tree = match self {
            Format::Toml => {
                toml_to_json(toml::from_str::<toml::Value>(text).map_err(|e| failed(&e))?)
            }
            Format::Yaml => serde_yaml_ng::from_str::<Json>(text).map_err(|e| failed(&e))?,
            Format::Json => serde_json::from_str::<Json>(text).map_err(|e| failed(&e))?,
        };
        match tree {
            Json::Null => Ok(Json::Object(serde_json::Map::new())),
            Json::Object(_) => Ok(tree),
            other => Err(failed(&format!(
                "the document is {} rather than a table of settings",
                kind_of(&other)
            ))),
        }
    }

    /// Writes a settings tree as a file in this format.
    pub fn render(self, tree: &Json) -> Result<String, ConfigError> {
        let failed = |e: &dyn std::fmt::Display| ConfigError::Render {
            format: self,
            message: e.to_string(),
        };
        match self {
            Format::Toml => {
                if let Some(key) = first_null(tree, "") {
                    return Err(ConfigError::Unrepresentable { key, format: self });
                }
                toml::to_string_pretty(tree).map_err(|e| failed(&e))
            }
            Format::Yaml => serde_yaml_ng::to_string(tree).map_err(|e| failed(&e)),
            Format::Json => serde_json::to_string_pretty(tree)
                .map(|mut text| {
                    text.push('\n');
                    text
                })
                .map_err(|e| failed(&e)),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Toml => "TOML",
            Format::Yaml => "YAML",
            Format::Json => "JSON",
        })
    }
}

fn kind_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "a list",
        Json::Object(_) => "a table",
    }
}

fn toml_to_json(value: toml::Value) -> Json {
    match value {
        toml::Value::String(s) => Json::String(s),
        toml::Value::Integer(i) => Json::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Json::Null, Json::Number),
        toml::Value::Boolean(b) => Json::Bool(b),
        toml::Value::Datetime(d) => Json::String(d.to_string()),
        toml::Value::Array(items) => Json::Array(items.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Json::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_json(value)))
                .collect(),
        ),
    }
}

fn first_null(tree: &Json, prefix: &str) -> Option<String> {
    match tree {
        Json::Null => Some(prefix.to_string()),
        Json::Object(map) => map
            .iter()
            .find_map(|(key, value)| first_null(value, &join(prefix, key))),
        Json::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, value)| first_null(value, &format!("{prefix}[{index}]"))),
        _ => None,
    }
}

/// What a file and the environment set, and nothing else, before it is checked against
/// the setting types.
#[derive(Debug, Clone, Default)]
pub struct Provisioning {
    /// The file's tree.
    tree: Json,
    /// Environment overrides by dotted path, as the variables spell them.
    env: BTreeMap<String, String>,
    /// The config file that was read, when one was named or found.
    pub file: Option<PathBuf>,
}

impl Provisioning {
    /// A file's contents alone, without the environment.
    pub fn from_text(text: &str, format: Format) -> Result<Self, ConfigError> {
        Ok(Self {
            tree: format.parse(text, "the text given")?,
            env: BTreeMap::new(),
            file: None,
        })
    }

    /// A settings tree, as if it had been read from a file.
    pub fn from_tree(tree: &Json) -> Result<Self, ConfigError> {
        if !tree.is_object() {
            return Err(ConfigError::Invalid {
                path: String::new(),
                message: format!("a settings tree is a table, not {}", kind_of(tree)),
            });
        }
        Ok(Self {
            tree: tree.clone(),
            env: BTreeMap::new(),
            file: None,
        })
    }

    /// Whether anything at all is provisioned.
    pub fn is_empty(&self) -> bool {
        self.paths().is_empty()
    }

    /// Where the database lives: `data_dir` from the file or `DISCOCLIP_DATA_DIR`, else
    /// `data`, relative to the working directory.
    pub fn data_dir(&self) -> Result<PathBuf, ConfigError> {
        if let Some(dir) = self.env.get(DATA_DIR_KEY) {
            return Ok(PathBuf::from(dir));
        }
        match self.tree.get(DATA_DIR_KEY) {
            None | Some(Json::Null) => Ok(PathBuf::from(DEFAULT_DATA_DIR)),
            Some(Json::String(dir)) => Ok(PathBuf::from(dir)),
            Some(other) => Err(ConfigError::Invalid {
                path: DATA_DIR_KEY.into(),
                message: format!("is {} rather than a path", kind_of(other)),
            }),
        }
    }

    /// Every provisioned path: the file's leaves and the environment's variables, without
    /// the data directory.
    fn paths(&self) -> BTreeSet<String> {
        let mut leaves = BTreeMap::new();
        json_leaves(&self.tree, "", &mut leaves);
        let mut paths: BTreeSet<String> = leaves.into_keys().collect();
        paths.extend(self.env.keys().cloned());
        paths.remove(DATA_DIR_KEY);
        paths
    }

    /// Every provisioned key with its value in the form the settings store holds it, checked
    /// against the setting types over `base`, the tree already stored: a file can set
    /// `engine.archive.keep` alone when the directory is stored already, and `"4"` from the
    /// environment comes back as `4`.
    pub fn resolve(&self, base: &Json) -> Result<BTreeMap<String, Json>, ConfigError> {
        let mut merged = base.clone();
        if !merged.is_object() {
            merged = Json::Object(serde_json::Map::new());
        }
        deep_merge(&mut merged, &self.tree);
        let exemplar = serde_json::to_value(Settings::exemplar())?;
        for (path, raw) in &self.env {
            if path == DATA_DIR_KEY {
                continue;
            }
            let template = at_path(&merged, path)
                .filter(|value| !value.is_null())
                .or_else(|| at_path(&exemplar, path));
            let value = coerce(raw, template);
            set_at_path(&mut merged, path, value);
        }
        if let Some(table) = merged.as_object_mut() {
            table.remove(DATA_DIR_KEY);
        }
        let settings: Settings =
            serde_path_to_error::deserialize(merged).map_err(|error| ConfigError::Invalid {
                path: error.path().to_string(),
                message: error.into_inner().to_string(),
            })?;
        let mut canonical = BTreeMap::new();
        json_leaves(&serde_json::to_value(&settings)?, "", &mut canonical);

        let mut values = BTreeMap::new();
        for path in self.paths() {
            let (key, value) =
                holder(&canonical, &path).ok_or_else(|| ConfigError::Unplaced(path.clone()))?;
            values.insert(key.to_string(), value.clone());
        }
        Ok(values)
    }
}

/// Loads `explicit` when given, otherwise the first config file found in the search
/// directories, otherwise nothing; then layers environment overrides on top.
pub fn load(explicit: Option<&Path>) -> Result<Provisioning, ConfigError> {
    let file = explicit.map(Path::to_path_buf).or_else(discover);
    build(file.as_deref(), provisioning_vars(std::env::vars_os()))
}

fn build(file: Option<&Path>, vars: BTreeMap<String, String>) -> Result<Provisioning, ConfigError> {
    let tree = match file {
        Some(path) => {
            let format = Format::from_path(path)
                .ok_or_else(|| ConfigError::UnknownFormat(path.to_path_buf()))?;
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            format.parse(&text, &path.display().to_string())?
        }
        None => Json::Object(serde_json::Map::new()),
    };
    let mut env = BTreeMap::new();
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let path = rest
            .split("__")
            .map(|segment| segment.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
        env.insert(path, value);
    }
    Ok(Provisioning {
        tree,
        env,
        file: file.map(Path::to_path_buf),
    })
}

/// Lays `over` onto `tree`: tables merge key by key, anything else replaces.
fn deep_merge(tree: &mut Json, over: &Json) {
    match (tree, over) {
        (Json::Object(base), Json::Object(top)) => {
            for (key, value) in top {
                match base.get_mut(key) {
                    Some(slot) if slot.is_object() && value.is_object() => deep_merge(slot, value),
                    _ => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (tree, over) => *tree = over.clone(),
    }
}

/// The value at the dotted `path` of `tree`, when there is one.
fn at_path<'a>(tree: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.')
        .try_fold(tree, |node, segment| node.get(segment))
}

/// Puts `value` at the dotted `path` of `tree`, making tables along the way.
fn set_at_path(tree: &mut Json, path: &str, value: Json) {
    let segments: Vec<&str> = path.split('.').collect();
    let Some((last, sections)) = segments.split_last() else {
        return;
    };
    let mut node = tree;
    for section in sections {
        if !node.is_object() {
            *node = Json::Object(serde_json::Map::new());
        }
        let map = node.as_object_mut().expect("just made an object");
        node = map
            .entry(*section)
            .or_insert_with(|| Json::Object(serde_json::Map::new()));
    }
    if !node.is_object() {
        *node = Json::Object(serde_json::Map::new());
    }
    node.as_object_mut()
        .expect("just made an object")
        .insert((*last).to_string(), value);
}

/// An environment variable's text as the value its setting expects: numbers, booleans,
/// lists and tables where the setting holds one, `null` for an explicit null, and the
/// text itself everywhere else. Text that does not fit stays text, so the type check
/// names it.
fn coerce(raw: &str, template: Option<&Json>) -> Json {
    let text = raw.trim();
    if text == "null" {
        return Json::Null;
    }
    match template {
        Some(Json::Number(_)) => text
            .parse::<i64>()
            .map(Json::from)
            .or_else(|_| text.parse::<u64>().map(Json::from))
            .or_else(|_| {
                text.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Json::Number)
                    .ok_or(())
            })
            .unwrap_or_else(|_| Json::String(raw.to_string())),
        Some(Json::Bool(_)) => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Json::Bool(true),
            "false" | "no" | "off" | "0" => Json::Bool(false),
            _ => Json::String(raw.to_string()),
        },
        Some(Json::Array(_)) => match serde_json::from_str::<Json>(text) {
            Ok(Json::Array(items)) => Json::Array(items),
            _ => Json::Array(
                text.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| Json::String(item.to_string()))
                    .collect(),
            ),
        },
        Some(Json::Object(_)) => match serde_json::from_str::<Json>(text) {
            Ok(Json::Object(map)) => Json::Object(map),
            _ => Json::String(raw.to_string()),
        },
        _ => Json::String(raw.to_string()),
    }
}

/// The canonical leaf at `path`, or the array or map leaf that contains it.
fn holder<'a>(canonical: &'a BTreeMap<String, Json>, path: &str) -> Option<(&'a str, &'a Json)> {
    let mut candidate = path;
    loop {
        if let Some((key, value)) = canonical.get_key_value(candidate) {
            return Some((key.as_str(), value));
        }
        candidate = candidate.rsplit_once('.')?.0;
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Dotted paths of every value in a JSON tree; objects recurse, arrays and the maps
/// named by [`ATOMIC_KEYS`] are values.
///
/// [`ATOMIC_KEYS`]: crate::settings::ATOMIC_KEYS
pub fn json_leaves(value: &Json, prefix: &str, out: &mut BTreeMap<String, Json>) {
    match value {
        Json::Object(map) if crate::settings::atomic_key(prefix) != Some(prefix) => {
            for (key, value) in map {
                json_leaves(value, &join(prefix, key), out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

fn discover() -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .flat_map(|dir| {
            Format::ALL
                .iter()
                .flat_map(|format| format.extensions())
                .map(move |ext| dir.join(format!("{FILE_STEM}.{ext}")))
        })
        .find(|path| path.is_file())
}

/// The working directory, the user's config directory, and the system config directory.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(".")];
    if let Some(user) = dirs::config_dir() {
        dirs.push(user.join(FILE_STEM));
    }
    if cfg!(unix) {
        dirs.push(PathBuf::from("/etc").join(FILE_STEM));
    }
    dirs
}

/// The variables that provision settings: everything but the one naming the config file.
fn provisioning_vars<K, V>(vars: impl Iterator<Item = (K, V)>) -> BTreeMap<String, String>
where
    K: Into<std::ffi::OsString>,
    V: Into<std::ffi::OsString>,
{
    vars.filter_map(|(key, value)| {
        Some((
            key.into().into_string().ok()?,
            value.into().into_string().ok()?,
        ))
    })
    .filter(|(key, _)| key != CONFIG_PATH_VAR)
    .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("discoclip-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn resolve(
        file: Option<&Path>,
        vars: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Json>, ConfigError> {
        build(file, vars)?.resolve(&json!({}))
    }

    #[test]
    fn nothing_is_provisioned_without_file_or_environment() {
        assert!(resolve(None, vars(&[])).unwrap().is_empty());
        assert!(build(None, vars(&[])).unwrap().is_empty());
    }

    #[test]
    fn environment_overrides_file_and_values_are_typed() {
        let file = temp_file(
            "override.toml",
            "[engine]\nworkers = 1\n[log]\nlevel = \"warn\"\n",
        );
        let values = resolve(Some(&file), vars(&[("DISCOCLIP_ENGINE__WORKERS", "4")])).unwrap();
        assert_eq!(values["engine.workers"], json!(4));
        assert_eq!(values["log.level"], json!("warn"));
        assert_eq!(values.len(), 2);
        let values = resolve(
            None,
            vars(&[
                ("DISCOCLIP_ENGINE__PLAYLISTS__ENABLED", "no"),
                ("DISCOCLIP_HTTP__RATE_LIMITS__DEFAULT__PER_SECOND", "2.5"),
                ("DISCOCLIP_WEB__TRUSTED_PROXIES", "10.0.0.1, 10.0.0.0/8"),
                ("DISCOCLIP_AUTH__OIDC__SCOPES", "[\"openid\"]"),
                ("DISCOCLIP_AUTH__OIDC__ISSUER", "https://issuer.example/"),
                ("DISCOCLIP_AUTH__OIDC__CLIENT_ID", "c"),
                ("DISCOCLIP_AUTH__OIDC__CLIENT_SECRET", "s"),
                ("DISCOCLIP_ENGINE__LIMITS__MAX_DURATION_SECS", "null"),
                (
                    "DISCOCLIP_HTTP__PROXIES__HOSTS",
                    "{\"a.test\": \"socks5://p:1\"}",
                ),
            ]),
        )
        .unwrap();
        assert_eq!(values["engine.playlists.enabled"], json!(false));
        assert_eq!(values["http.rate_limits.default.per_second"], json!(2.5));
        assert_eq!(
            values["web.trusted_proxies"],
            json!(["10.0.0.1", "10.0.0.0/8"])
        );
        assert_eq!(values["auth.oidc.scopes"], json!(["openid"]));
        assert_eq!(values["engine.limits.max_duration_secs"], Json::Null);
        assert_eq!(
            values["http.proxies.hosts"],
            json!({"a.test": "socks5://p:1"})
        );
    }

    #[test]
    fn yaml_files_and_nested_keys() {
        let file = temp_file(
            "nested.yaml",
            "local:\n  max_bytes: 3\nlog:\n  level: debug\nengine:\n  limits:\n    max_height: 480\n",
        );
        let values = resolve(
            Some(&file),
            vars(&[("DISCOCLIP_ENGINE__LIMITS__MAX_HEIGHT", "720")]),
        )
        .unwrap();
        assert_eq!(values["log.level"], json!("debug"));
        assert_eq!(values["engine.limits.max_height"], json!(720));
        assert_eq!(values["local.max_bytes"], json!(3));
        assert!(!values.contains_key("engine.workers"));
        assert!(!values.contains_key("auth.oidc.scopes"));
    }

    #[test]
    fn arrays_are_single_values() {
        let file = temp_file(
            "scopes.toml",
            "[auth.oidc]\nissuer = \"https://issuer.example/\"\nclient_id = \"c\"\nclient_secret = \"s\"\nscopes = [\"openid\", \"email\"]\n",
        );
        let values = resolve(Some(&file), vars(&[])).unwrap();
        assert_eq!(values["auth.oidc.scopes"], json!(["openid", "email"]));
        assert_eq!(values["auth.oidc.client_secret"], json!("s"));
        assert!(!values.contains_key("auth.oidc.name"));
    }

    #[test]
    fn explicit_null_is_provisioned() {
        let file = temp_file(
            "null.yaml",
            "engine:\n  limits:\n    max_duration_secs: null\n",
        );
        let values = resolve(Some(&file), vars(&[])).unwrap();
        assert_eq!(values["engine.limits.max_duration_secs"], Json::Null);
    }

    #[test]
    fn a_section_from_the_environment_alone() {
        let values = resolve(None, vars(&[("DISCOCLIP_LOCAL__MAX_BYTES", "3")])).unwrap();
        assert_eq!(values["local.max_bytes"], json!(3));
    }

    #[test]
    fn digits_in_the_environment_stay_strings_where_the_setting_is_one() {
        let values = resolve(None, vars(&[("DISCOCLIP_LOCAL__DIR", "1234")])).unwrap();
        assert_eq!(values["local.dir"], json!("1234"));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_types() {
        assert!(matches!(
            resolve(None, vars(&[("DISCOCLIP_BOGUS", "1")])),
            Err(ConfigError::Invalid { .. })
        ));
        let error = resolve(None, vars(&[("DISCOCLIP_ENGINE__WORKERS", "many")])).unwrap_err();
        assert!(
            matches!(error, ConfigError::Invalid { ref path, .. } if path == "engine.workers"),
            "{error}"
        );
        let file = temp_file("unknown.toml", "[engine]\nbogus = 1\n");
        assert!(resolve(Some(&file), vars(&[])).is_err());
        let file = temp_file("partial.toml", "[engine.archive]\nkeep = \"both\"\n");
        assert!(resolve(Some(&file), vars(&[])).is_err());
        let file = temp_file("broken.toml", "[engine\nworkers = 1\n");
        assert!(matches!(
            build(Some(&file), vars(&[])),
            Err(ConfigError::Parse {
                format: Format::Toml,
                ..
            })
        ));
        assert!(matches!(
            Provisioning::from_text("[1, 2]", Format::Json),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            build(Some(Path::new("/nowhere/discoclip.toml")), vars(&[])),
            Err(ConfigError::Read { .. })
        ));
    }

    #[test]
    fn a_partial_section_is_checked_over_what_is_stored() {
        let provisioning =
            Provisioning::from_text("[engine.archive]\nkeep = \"both\"\n", Format::Toml).unwrap();
        let values = provisioning
            .resolve(&json!({"engine": {"archive": {"dir": "stored"}}}))
            .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["engine.archive.keep"], json!("both"));
        assert!(provisioning.resolve(&json!({})).is_err());
    }

    #[test]
    fn maps_keyed_by_hosts_are_single_values() {
        let file = temp_file(
            "hosts.toml",
            "[http.rate_limits.hosts.\"youtube.com\"]\nper_second = 1.0\nburst = 2\n[http.proxies.hosts]\n\"tiktok.com\" = \"socks5://p:1080\"\n",
        );
        let values = resolve(Some(&file), vars(&[])).unwrap();
        assert_eq!(
            values["http.rate_limits.hosts"],
            json!({"youtube.com": {"per_second": 1.0, "burst": 2}})
        );
        assert_eq!(
            values["http.proxies.hosts"],
            json!({"tiktok.com": "socks5://p:1080"})
        );
        assert_eq!(values.len(), 2);
        // Provisioning a map over a stored one keeps the stored hosts.
        let provisioning = Provisioning::from_text(
            "[http.rate_limits.hosts.\"reddit.com\"]\nper_second = 3.0\n",
            Format::Toml,
        )
        .unwrap();
        let values = provisioning
            .resolve(&json!({"http": {"rate_limits": {"hosts": {"youtube.com": {"per_second": 1.0, "burst": 2}}}}}))
            .unwrap();
        let hosts = values["http.rate_limits.hosts"].as_object().unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["reddit.com"]["burst"], 8);
    }

    #[test]
    fn unrelated_and_unprefixed_variables_are_ignored() {
        let values = resolve(None, vars(&[("HOME", "/nowhere"), ("DISCOCLIPX", "1")])).unwrap();
        assert!(values.is_empty());
    }

    #[test]
    fn the_config_path_variable_does_not_provision() {
        let vars = provisioning_vars(
            [
                (CONFIG_PATH_VAR, "x.toml"),
                ("DISCOCLIP_LOG__LEVEL", "warn"),
            ]
            .into_iter(),
        );
        assert_eq!(vars.len(), 1);
        assert_eq!(vars["DISCOCLIP_LOG__LEVEL"], "warn");
    }

    #[test]
    fn data_dir_is_provisioned_but_never_stored() {
        let none = build(None, vars(&[])).unwrap();
        assert_eq!(none.data_dir().unwrap(), PathBuf::from("data"));
        assert!(none.resolve(&json!({})).unwrap().is_empty());

        let file = temp_file(
            "data.toml",
            "data_dir = \"/var/lib/discoclip\"\n[log]\nlevel = \"warn\"\n",
        );
        let from_file = build(Some(&file), vars(&[])).unwrap();
        assert_eq!(
            from_file.data_dir().unwrap(),
            PathBuf::from("/var/lib/discoclip")
        );
        let values = from_file.resolve(&json!({})).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["log.level"], json!("warn"));

        let from_env = build(Some(&file), vars(&[("DISCOCLIP_DATA_DIR", "/srv/dc")])).unwrap();
        assert_eq!(from_env.data_dir().unwrap(), PathBuf::from("/srv/dc"));
        assert!(!from_env.is_empty());
        assert!(
            !from_env
                .resolve(&json!({}))
                .unwrap()
                .contains_key("data_dir")
        );
        let bad = Provisioning::from_tree(&json!({"data_dir": 3})).unwrap();
        assert!(matches!(bad.data_dir(), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn unknown_extension_is_refused() {
        let file = temp_file("discoclip.conf", "");
        assert!(matches!(
            build(Some(&file), vars(&[])),
            Err(ConfigError::UnknownFormat(_))
        ));
    }

    #[test]
    fn every_format_round_trips() {
        let tree = json!({
            "log": {"level": "debug"},
            "engine": {"workers": 3, "limits": {"max_source_bytes": 2147483648_u64}},
            "auth": {"oidc": {"issuer": "https://issuer.example/", "client_id": "c", "client_secret": "s", "scopes": ["openid"]}},
            "web": {"bind": "0.0.0.0:9000"}
        });
        for format in Format::ALL {
            let text = format.render(&tree).unwrap();
            let values = Provisioning::from_text(&text, format)
                .unwrap()
                .resolve(&json!({}))
                .unwrap();
            assert_eq!(values["log.level"], json!("debug"), "{format}");
            assert_eq!(values["engine.workers"], json!(3), "{format}");
            assert_eq!(
                values["engine.limits.max_source_bytes"],
                json!(2147483648_u64),
                "{format}"
            );
            assert_eq!(values["auth.oidc.scopes"], json!(["openid"]), "{format}");
            assert_eq!(values["web.bind"], json!("0.0.0.0:9000"), "{format}");
            assert_eq!(values.len(), 8, "{format}");
            assert!(
                Provisioning::from_text("", format).unwrap().is_empty(),
                "{format}"
            );
        }
    }

    #[test]
    fn toml_cannot_hold_null_but_yaml_and_json_can() {
        let tree = json!({"engine": {"limits": {"max_duration_secs": null}}});
        match Format::Toml.render(&tree).unwrap_err() {
            ConfigError::Unrepresentable { key, .. } => {
                assert_eq!(key, "engine.limits.max_duration_secs")
            }
            other => panic!("unexpected {other}"),
        }
        for format in [Format::Yaml, Format::Json] {
            let text = format.render(&tree).unwrap();
            let values = Provisioning::from_text(&text, format)
                .unwrap()
                .resolve(&json!({}))
                .unwrap();
            assert_eq!(values["engine.limits.max_duration_secs"], Json::Null);
        }
    }

    #[test]
    fn formats_come_from_extensions() {
        assert_eq!(Format::from_path(Path::new("a/b.toml")), Some(Format::Toml));
        assert_eq!(Format::from_path(Path::new("b.yml")), Some(Format::Yaml));
        assert_eq!(Format::from_path(Path::new("b.yaml")), Some(Format::Yaml));
        assert_eq!(Format::from_path(Path::new("b.json")), Some(Format::Json));
        assert_eq!(Format::from_path(Path::new("b.conf")), None);
        assert_eq!(Format::from_path(Path::new("b")), None);
    }
}
