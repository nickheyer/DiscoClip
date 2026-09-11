//! Provisioning input: one config file, then `DISCOCLIP_*` environment variables, the
//! latter overriding the former. Only keys that are actually set count as provisioned; they
//! seed the settings store at startup through `settings::bootstrap`, and the running server
//! reads the store, never this. The same file formats carry exported settings back out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use config::{FileFormat, Map, Source, Value, ValueKind};
use serde_json::Value as Json;

use crate::settings::Settings;

/// Environment variables `DISCOCLIP_<SECTION>__<KEY>` override config file values.
pub const ENV_PREFIX: &str = "DISCOCLIP";
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
    #[error(transparent)]
    Source(#[from] config::ConfigError),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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

    fn file_format(self) -> FileFormat {
        match self {
            Format::Toml => FileFormat::Toml,
            Format::Yaml => FileFormat::Yaml,
            Format::Json => FileFormat::Json,
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
    sources: config::Config,
    /// The config file that was read, when one was named or found.
    pub file: Option<PathBuf>,
}

impl Provisioning {
    /// A file's contents alone, without the environment.
    pub fn from_text(text: &str, format: Format) -> Result<Self, ConfigError> {
        let sources = config::Config::builder()
            .add_source(config::File::from_str(text, format.file_format()))
            .build()?;
        Ok(Self {
            sources,
            file: None,
        })
    }

    /// A settings tree, as if it had been read from a file.
    pub fn from_tree(tree: &Json) -> Result<Self, ConfigError> {
        Ok(Self {
            sources: config::Config::try_from(tree)?,
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
        match self.sources.get_string(DATA_DIR_KEY) {
            Ok(dir) => Ok(PathBuf::from(dir)),
            Err(config::ConfigError::NotFound(_)) => Ok(PathBuf::from(DEFAULT_DATA_DIR)),
            Err(error) => Err(error.into()),
        }
    }

    /// The provisioned settings, without the data directory.
    fn settings_sources(&self) -> Result<config::Config, ConfigError> {
        let mut tree: Json = self.sources.clone().try_deserialize()?;
        if let Some(table) = tree.as_object_mut() {
            table.remove(DATA_DIR_KEY);
        }
        Ok(config::Config::try_from(&tree)?)
    }

    fn paths(&self) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        if let Ok(table) = self.sources.collect() {
            leaf_paths(&table, "", &mut paths);
        }
        paths.remove(DATA_DIR_KEY);
        paths
    }

    /// Every provisioned key with its value in the form the settings store holds it, checked
    /// against the setting types over `base`, the tree already stored: a file can set
    /// `engine.archive.keep` alone when the directory is stored already, and `"4"` from the
    /// environment comes back as `4`.
    pub fn resolve(&self, base: &Json) -> Result<BTreeMap<String, Json>, ConfigError> {
        let merged = config::Config::builder()
            .add_source(config::Config::try_from(base)?)
            .add_source(self.settings_sources()?)
            .build()?;
        let settings: Settings = merged.try_deserialize()?;
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
    let sources = build(file.as_deref(), provisioning_vars(std::env::vars_os()))?;
    Ok(Provisioning { sources, file })
}

fn build(file: Option<&Path>, vars: Map<String, String>) -> Result<config::Config, ConfigError> {
    let mut builder = config::Config::builder();
    if let Some(path) = file {
        let format = Format::from_path(path)
            .ok_or_else(|| ConfigError::UnknownFormat(path.to_path_buf()))?;
        builder = builder.add_source(
            config::File::from(path)
                .format(format.file_format())
                .required(true),
        );
    }
    let environment = config::Environment::with_prefix(ENV_PREFIX)
        .prefix_separator("_")
        .separator("__")
        .source(Some(vars));
    Ok(builder.add_source(environment).build()?)
}

/// The canonical leaf at `path`, or the array leaf that contains it.
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

/// Dotted paths of every value in the merged sources; tables recurse, arrays are values.
fn leaf_paths(table: &Map<String, Value>, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in table {
        let path = join(prefix, key);
        match &value.kind {
            ValueKind::Table(inner) => leaf_paths(inner, &path, out),
            _ => {
                out.insert(path);
            }
        }
    }
}

/// Dotted paths of every value in a JSON tree; objects recurse, arrays are values.
pub fn json_leaves(value: &Json, prefix: &str, out: &mut BTreeMap<String, Json>) {
    match value {
        Json::Object(map) => {
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
fn provisioning_vars<K, V>(vars: impl Iterator<Item = (K, V)>) -> Map<String, String>
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

    fn vars(pairs: &[(&str, &str)]) -> Map<String, String> {
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
        vars: Map<String, String>,
    ) -> Result<BTreeMap<String, Json>, ConfigError> {
        let provisioning = Provisioning {
            sources: build(file, vars)?,
            file: None,
        };
        provisioning.resolve(&json!({}))
    }

    #[test]
    fn nothing_is_provisioned_without_file_or_environment() {
        assert!(resolve(None, vars(&[])).unwrap().is_empty());
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
        assert!(resolve(None, vars(&[("DISCOCLIP_BOGUS", "1")])).is_err());
        assert!(resolve(None, vars(&[("DISCOCLIP_ENGINE__WORKERS", "many")])).is_err());
        let file = temp_file("unknown.toml", "[engine]\nbogus = 1\n");
        assert!(resolve(Some(&file), vars(&[])).is_err());
        let file = temp_file("partial.toml", "[engine.archive]\nkeep = \"both\"\n");
        assert!(resolve(Some(&file), vars(&[])).is_err());
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
        let none = Provisioning {
            sources: build(None, vars(&[])).unwrap(),
            file: None,
        };
        assert_eq!(none.data_dir().unwrap(), PathBuf::from("data"));
        assert!(none.resolve(&json!({})).unwrap().is_empty());

        let file = temp_file(
            "data.toml",
            "data_dir = \"/var/lib/discoclip\"\n[log]\nlevel = \"warn\"\n",
        );
        let from_file = Provisioning {
            sources: build(Some(&file), vars(&[])).unwrap(),
            file: None,
        };
        assert_eq!(
            from_file.data_dir().unwrap(),
            PathBuf::from("/var/lib/discoclip")
        );
        let values = from_file.resolve(&json!({})).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["log.level"], json!("warn"));

        let from_env = Provisioning {
            sources: build(Some(&file), vars(&[("DISCOCLIP_DATA_DIR", "/srv/dc")])).unwrap(),
            file: None,
        };
        assert_eq!(from_env.data_dir().unwrap(), PathBuf::from("/srv/dc"));
        assert!(!from_env.is_empty());
        assert!(
            !from_env
                .resolve(&json!({}))
                .unwrap()
                .contains_key("data_dir")
        );
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
