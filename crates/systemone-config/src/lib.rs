//! Layered configuration resolved once per process.
//!
//! Precedence, lowest to highest: built-ins < user file < current-directory
//! file < `SYSTEMONE_*` environment < CLI overrides. Every source is validated
//! on its own (unknown keys, wrong types) even when a higher layer overrides
//! it, so a malformed lower layer is never silently excused.
//!
//! Resolution never reads secrets, loads models or touches the network.
//! Backend `settings` tables are vendor-owned: this crate carries them as
//! opaque TOML and each adapter validates its own typed schema.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs, io,
    net::IpAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use systemone_core::{BackendId, ProviderKind};
use thiserror::Error;
use toml::{Table, Value};

pub use toml;

pub const FILE_NAME: &str = "systemone.config.toml";
pub const USER_DIRECTORY: &str = ".systemone";
pub const ENV_PREFIX: &str = "SYSTEMONE_";

/// Built-in defaults. One CPU OpenJev instance is enabled so `s1 serve` works
/// on a native build with no configuration; no remote instance is enabled.
pub const BUILTIN_DEFAULTS: &str = r#"
default_backend = "local"

[output]
pretty = false
quiet = false

[server]
host = "127.0.0.1"
port = 8080
request_timeout_secs = 120
max_body_bytes = 1048576
max_admitted_jobs = 16

[backends.local]
kind = "openjev"
enabled = true
model = "qwen3-0.6b"

[backends.local.settings]
device = "cpu"
"#;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Backend used when a request carries no selector. Must name an enabled
    /// instance.
    pub default_backend: Option<BackendId>,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub backends: BTreeMap<BackendId, BackendConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    #[serde(default)]
    pub pretty: bool,
    #[serde(default)]
    pub quiet: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub request_timeout_secs: u64,
    /// Name of the environment variable holding the inbound bearer secret.
    /// Required for non-loopback binds. The secret itself is never stored.
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub max_body_bytes: usize,
    /// Total accepted (queued + in-flight) jobs across all backends.
    pub max_admitted_jobs: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 8080,
            request_timeout_secs: 120,
            api_key_env: None,
            max_body_bytes: 1024 * 1024,
            max_admitted_jobs: 16,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub enabled: bool,
    /// Instance default model; `None` lets the adapter choose its registry
    /// default where one exists.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra request model names that resolve to `model` on this instance.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Queued jobs allowed for this instance before 429.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    /// Concurrent native/remote calls for this instance.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    /// Vendor-owned typed settings, validated by the adapter.
    #[serde(default)]
    pub settings: Table,
}

const fn default_queue_capacity() -> usize {
    8
}

const fn default_max_in_flight() -> usize {
    1
}

impl Config {
    /// Cross-field validation that does not depend on adapters.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.port == 0 {
            return Err(ConfigError::new("server.port must be positive"));
        }
        if self.server.request_timeout_secs == 0 {
            return Err(ConfigError::new(
                "server.request_timeout_secs must be positive",
            ));
        }
        if self.server.max_body_bytes == 0 || self.server.max_admitted_jobs == 0 {
            return Err(ConfigError::new(
                "server.max_body_bytes and server.max_admitted_jobs must be positive",
            ));
        }
        if let Some(name) = &self.server.api_key_env
            && (name.is_empty() || name.contains('='))
        {
            return Err(ConfigError::new(
                "server.api_key_env must name a nonempty environment variable",
            ));
        }
        for (id, backend) in &self.backends {
            if backend.queue_capacity == 0 || backend.max_in_flight == 0 {
                return Err(ConfigError::new(format!(
                    "backends.{id}: queue_capacity and max_in_flight must be positive"
                )));
            }
            if backend.model.as_deref() == Some("") {
                return Err(ConfigError::new(format!(
                    "backends.{id}.model must not be empty"
                )));
            }
        }
        if let Some(default) = &self.default_backend {
            match self.backends.get(default) {
                None => {
                    return Err(ConfigError::new(format!(
                        "default_backend {default:?} is not a configured backend"
                    )));
                }
                Some(backend) if !backend.enabled => {
                    return Err(ConfigError::new(format!(
                        "default_backend {default:?} is not enabled"
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    pub fn enabled_backends(&self) -> impl Iterator<Item = (&BackendId, &BackendConfig)> {
        self.backends.iter().filter(|(_, backend)| backend.enabled)
    }
}

/// A leaf override from the CLI (`--set a.b.c=value`, `--port 1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Override {
    pub path: String,
    pub value: String,
}

impl Override {
    /// Parse `path=value`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let (path, value) = text
            .split_once('=')
            .ok_or_else(|| ConfigError::new(format!("--set {text:?} must be KEY=VALUE")))?;
        if path.is_empty() {
            return Err(ConfigError::new(format!("--set {text:?} has an empty key")));
        }
        Ok(Self {
            path: path.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResolveOptions {
    /// Skip both files and the `SYSTEMONE_*` layer.
    pub no_config: bool,
    /// Explicit user file (defaults to `~/.systemone/systemone.config.toml`).
    pub user_file: Option<PathBuf>,
    /// Explicit project file (defaults to `./systemone.config.toml`).
    pub project_file: Option<PathBuf>,
    /// Environment snapshot; `None` reads the process environment.
    pub environment: Option<Vec<(OsString, OsString)>>,
    pub overrides: Vec<Override>,
}

/// Resolved configuration plus where each leaf came from.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub config: Config,
    /// Fully merged TOML for `config show`.
    pub merged: Table,
    /// Dotted leaf path → source label.
    pub provenance: BTreeMap<String, String>,
}

impl Resolved {
    /// Non-secret display document. Config carries only environment variable
    /// *names*, so nothing needs masking; this exists so callers never print
    /// the raw environment by mistake.
    #[must_use]
    pub fn redacted(&self) -> serde_json::Value {
        serde_json::json!({
            "config": toml_to_json(&Value::Table(self.merged.clone())),
            "provenance": self.provenance,
        })
    }
}

pub fn user_file_path() -> Option<PathBuf> {
    home_directory().map(|home| home.join(USER_DIRECTORY).join(FILE_NAME))
}

pub fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let variable = "USERPROFILE";
    #[cfg(not(windows))]
    let variable = "HOME";
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn resolve(options: &ResolveOptions) -> Result<Resolved, ConfigError> {
    let mut merged: Table = toml::from_str(BUILTIN_DEFAULTS).expect("built-in defaults parse");
    let mut provenance = BTreeMap::new();
    record_provenance(&mut provenance, "", &merged, "built-in");

    if !options.no_config {
        let user = options.user_file.clone().or_else(user_file_path);
        let project = match &options.project_file {
            Some(path) => path.clone(),
            None => std::env::current_dir()
                .map_err(|error| {
                    ConfigError::new(format!(
                        "cannot resolve project configuration directory: {error}"
                    ))
                })?
                .join(FILE_NAME),
        };
        if let Some(path) = user {
            read_file(&mut merged, &mut provenance, &path)?;
        }
        read_file(&mut merged, &mut provenance, &project)?;
        let environment: Vec<(OsString, OsString)> = match &options.environment {
            Some(environment) => environment.clone(),
            None => std::env::vars_os().collect(),
        };
        apply_environment(&mut merged, &mut provenance, &environment)?;
    }
    for override_ in &options.overrides {
        let value = coerce(&override_.value);
        set_path(&mut merged, &override_.path, value, "cli")
            .map_err(|message| ConfigError::new(format!("--set {}: {message}", override_.path)))?;
        provenance.insert(override_.path.clone(), "cli".to_owned());
    }
    let config: Config = Value::Table(merged.clone())
        .try_into()
        .map_err(|error| ConfigError::new(format!("resolved configuration: {error}")))?;
    config.validate()?;
    Ok(Resolved {
        config,
        merged,
        provenance,
    })
}

fn read_file(
    merged: &mut Table,
    provenance: &mut BTreeMap<String, String>,
    path: &Path,
) -> Result<(), ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ConfigError::new(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    let source = path.display().to_string();
    let table: Table =
        toml::from_str(&text).map_err(|error| ConfigError::new(format!("{source}: {error}")))?;
    // Eager validation: the file must be a valid partial config on its own.
    Value::Table(table.clone())
        .try_into::<Config>()
        .map(|_| ())
        .or_else(|error| {
            // A partial file may legitimately omit required server fields;
            // validate structure by merging into defaults instead.
            let mut probe: Table =
                toml::from_str(BUILTIN_DEFAULTS).expect("built-in defaults parse");
            merge_tables(&mut probe, &table);
            Value::Table(probe)
                .try_into::<Config>()
                .map(|_| ())
                .map_err(|_| ConfigError::new(format!("{source}: {error}")))
        })?;
    record_provenance(provenance, "", &table, &source);
    merge_tables(merged, &table);
    Ok(())
}

fn merge_tables(target: &mut Table, source: &Table) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn record_provenance(
    provenance: &mut BTreeMap<String, String>,
    prefix: &str,
    table: &Table,
    source: &str,
) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(inner) => record_provenance(provenance, &path, inner, source),
            _ => {
                provenance.insert(path, source.to_owned());
            }
        }
    }
}

/// Map a `SYSTEMONE_*` name to a dotted config path.
fn environment_path(name: &str) -> Result<Option<String>, ConfigError> {
    let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
        return Ok(None);
    };
    let path = match rest {
        "DEFAULT_BACKEND" => "default_backend".to_owned(),
        "HOST" => "server.host".to_owned(),
        "PORT" => "server.port".to_owned(),
        "REQUEST_TIMEOUT_SECS" => "server.request_timeout_secs".to_owned(),
        "API_KEY_ENV" => "server.api_key_env".to_owned(),
        "MAX_BODY_BYTES" => "server.max_body_bytes".to_owned(),
        "MAX_ADMITTED_JOBS" => "server.max_admitted_jobs".to_owned(),
        "PRETTY" => "output.pretty".to_owned(),
        "QUIET" => "output.quiet".to_owned(),
        _ => {
            let Some(rest) = rest.strip_prefix("BACKENDS__") else {
                return Err(ConfigError::new(format!(
                    "unknown environment setting {name}"
                )));
            };
            let mut parts = rest.split("__");
            let id = parts
                .next()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| ConfigError::new(format!("{name}: missing backend ID")))?
                .to_ascii_lowercase()
                .replace('_', "-");
            BackendId::new(&id).map_err(|error| ConfigError::new(format!("{name}: {error}")))?;
            let leaf = parts
                .next()
                .ok_or_else(|| ConfigError::new(format!("{name}: missing backend setting")))?;
            match (leaf, parts.next(), parts.next()) {
                ("ENABLED", None, _) => format!("backends.{id}.enabled"),
                ("MODEL", None, _) => format!("backends.{id}.model"),
                ("KIND", None, _) => format!("backends.{id}.kind"),
                ("QUEUE_CAPACITY", None, _) => format!("backends.{id}.queue_capacity"),
                ("MAX_IN_FLIGHT", None, _) => format!("backends.{id}.max_in_flight"),
                ("SETTINGS", Some(key), None) if !key.is_empty() => {
                    format!("backends.{id}.settings.{}", key.to_ascii_lowercase())
                }
                _ => {
                    return Err(ConfigError::new(format!(
                        "unknown environment setting {name}"
                    )));
                }
            }
        }
    };
    Ok(Some(path))
}

fn apply_environment(
    merged: &mut Table,
    provenance: &mut BTreeMap<String, String>,
    environment: &[(OsString, OsString)],
) -> Result<(), ConfigError> {
    let mut entries: Vec<(String, &OsStr)> = Vec::new();
    for (name, value) in environment {
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(ENV_PREFIX) {
            continue;
        }
        entries.push((name.to_owned(), value.as_os_str()));
    }
    entries.sort();
    for (name, value) in entries {
        let Some(path) = environment_path(&name)? else {
            continue;
        };
        let value = value
            .to_str()
            .ok_or_else(|| ConfigError::new(format!("{name} is not valid UTF-8")))?;
        if value.is_empty() {
            return Err(ConfigError::new(format!("{name} must not be empty")));
        }
        set_path(merged, &path, coerce(value), &name)
            .map_err(|message| ConfigError::new(format!("{name}: {message}")))?;
        provenance.insert(path, name);
    }
    Ok(())
}

/// Coerce an environment/CLI string into a TOML scalar: booleans and
/// integers are typed, everything else stays a string. Floats are not
/// inferred so model names such as `1.5` survive. A value wrapped in double
/// quotes is a TOML string literal, so `--set 'key="28"'` sets the string
/// `28` rather than the integer.
fn coerce(text: &str) -> Value {
    if text.len() >= 2
        && text.starts_with('"')
        && text.ends_with('"')
        && let Ok(table) = toml::from_str::<Table>(&format!("value = {text}"))
        && let Some(Value::String(inner)) = table.get("value")
    {
        return Value::String(inner.clone());
    }
    match text {
        "true" => Value::Boolean(true),
        "false" => Value::Boolean(false),
        _ => text
            .parse::<i64>()
            .map_or_else(|_| Value::String(text.to_owned()), Value::Integer),
    }
}

fn set_path(table: &mut Table, path: &str, value: Value, source: &str) -> Result<(), String> {
    let mut segments = path.split('.').peekable();
    let mut current = table;
    while let Some(segment) = segments.next() {
        if segment.is_empty() {
            return Err(format!("empty path segment from {source}"));
        }
        if segments.peek().is_none() {
            if matches!(current.get(segment), Some(Value::Table(_))) {
                return Err(format!("{segment} is a table, not a leaf"));
            }
            current.insert(segment.to_owned(), value);
            return Ok(());
        }
        let entry = current
            .entry(segment.to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        match entry {
            Value::Table(inner) => current = inner,
            _ => return Err(format!("{segment} is a leaf, not a table")),
        }
    }
    Err("empty path".to_owned())
}

fn toml_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(value) => serde_json::Value::String(value.clone()),
        Value::Integer(value) => serde_json::Value::from(*value),
        Value::Float(value) => serde_json::Value::from(*value),
        Value::Boolean(value) => serde_json::Value::Bool(*value),
        Value::Datetime(value) => serde_json::Value::String(value.to_string()),
        Value::Array(values) => serde_json::Value::Array(values.iter().map(toml_to_json).collect()),
        Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.clone(), toml_to_json(value)))
                .collect(),
        ),
    }
}

/// Convert an opaque settings table to JSON for adapters that deserialize
/// through `serde_json`.
#[must_use]
pub fn settings_to_json(settings: &Table) -> serde_json::Value {
    toml_to_json(&Value::Table(settings.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_with(
        user: &str,
        project: &str,
        env: &[(&str, &str)],
        overrides: &[&str],
    ) -> Result<Resolved, ConfigError> {
        let root = tempfile::tempdir().unwrap();
        let user_path = root.path().join("user.toml");
        let project_path = root.path().join(FILE_NAME);
        fs::write(&user_path, user).unwrap();
        fs::write(&project_path, project).unwrap();
        resolve(&ResolveOptions {
            no_config: false,
            user_file: Some(user_path),
            project_file: Some(project_path),
            environment: Some(
                env.iter()
                    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                    .collect(),
            ),
            overrides: overrides
                .iter()
                .map(|text| Override::parse(text).unwrap())
                .collect(),
        })
    }

    #[test]
    fn builtins_are_valid_and_enable_one_local_backend() {
        let resolved = resolve(&ResolveOptions {
            no_config: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            resolved.config.default_backend.as_ref().unwrap().as_str(),
            "local"
        );
        assert_eq!(resolved.config.enabled_backends().count(), 1);
        assert_eq!(resolved.provenance["server.port"], "built-in");
    }

    #[test]
    fn precedence_is_user_then_project_then_env_then_cli() {
        let resolved = resolve_with(
            "[server]\nport = 1\n[backends.local.settings]\nthreads = 2\n",
            "[server]\nport = 2\n",
            &[
                ("SYSTEMONE_PORT", "3"),
                ("SYSTEMONE_BACKENDS__LOCAL__SETTINGS__THREADS", "9"),
                ("UNRELATED", "x"),
            ],
            &["server.host=0.0.0.0", "server.api_key_env=KEY"],
        )
        .unwrap();
        assert_eq!(resolved.config.server.port, 3);
        assert_eq!(resolved.provenance["server.port"], "SYSTEMONE_PORT");
        assert_eq!(resolved.config.server.host.to_string(), "0.0.0.0");
        assert_eq!(resolved.provenance["server.host"], "cli");
        let threads = resolved.config.backends[&BackendId::new("local").unwrap()]
            .settings
            .get("threads")
            .unwrap()
            .as_integer();
        assert_eq!(threads, Some(9));
        assert_eq!(
            resolved.provenance["backends.local.settings.threads"],
            "SYSTEMONE_BACKENDS__LOCAL__SETTINGS__THREADS"
        );
    }

    #[test]
    fn env_ids_map_back_to_hyphenated_backend_names() {
        let resolved = resolve_with(
            "",
            "[backends.cloud-vercel]\nkind = \"vercel\"\nmodel = \"jev-latest\"\n",
            &[("SYSTEMONE_BACKENDS__CLOUD_VERCEL__ENABLED", "true")],
            &[],
        )
        .unwrap();
        let vercel = &resolved.config.backends[&BackendId::new("cloud-vercel").unwrap()];
        assert!(vercel.enabled);
        assert_eq!(vercel.kind, ProviderKind::Vercel);
    }

    #[test]
    fn malformed_lower_layers_fail_even_when_overridden() {
        let error = resolve_with(
            "[server]\nport = \"not-a-port\"\n",
            "[server]\nport = 2\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert!(error.0.contains("user.toml"), "{error}");

        let error = resolve_with("unknown_key = 1\n", "", &[], &[]).unwrap_err();
        assert!(error.0.contains("unknown_key"), "{error}");
    }

    #[test]
    fn unknown_env_settings_and_bad_values_are_errors() {
        assert!(resolve_with("", "", &[("SYSTEMONE_BOGUS", "1")], &[]).is_err());
        assert!(resolve_with("", "", &[("SYSTEMONE_PORT", "-1")], &[]).is_err());
        assert!(resolve_with("", "", &[("SYSTEMONE_PRETTY", "1")], &[]).is_err());
        assert!(resolve_with("", "", &[("SYSTEMONE_PORT", "")], &[]).is_err());
        assert!(
            resolve_with(
                "",
                "",
                &[("SYSTEMONE_BACKENDS__Bad_ID__ENABLED", "true")],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn default_backend_must_be_enabled_and_known() {
        assert!(resolve_with("", "default_backend = \"missing\"\n", &[], &[]).is_err());
        assert!(
            resolve_with(
                "",
                "",
                &[("SYSTEMONE_BACKENDS__LOCAL__ENABLED", "false")],
                &[]
            )
            .is_err()
        );
        let ok = resolve_with(
            "",
            "default_backend = \"cloud\"\n[backends.cloud]\nkind = \"openrouter\"\nenabled = true\nmodel = \"jev-latest\"\n",
            &[("SYSTEMONE_BACKENDS__LOCAL__ENABLED", "false")],
            &[],
        )
        .unwrap();
        assert_eq!(ok.config.enabled_backends().count(), 1);
    }

    #[test]
    fn no_config_ignores_files_and_environment_but_not_cli() {
        let root = tempfile::tempdir().unwrap();
        let project_path = root.path().join(FILE_NAME);
        fs::write(&project_path, "[server]\nport = 2\n").unwrap();
        let resolved = resolve(&ResolveOptions {
            no_config: true,
            project_file: Some(project_path),
            environment: Some(vec![("SYSTEMONE_PORT".into(), "3".into())]),
            overrides: vec![Override::parse("server.port=4").unwrap()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved.config.server.port, 4);
    }

    #[test]
    fn override_parsing_and_coercion() {
        assert!(Override::parse("novalue").is_err());
        assert!(Override::parse("=x").is_err());
        assert_eq!(coerce("true"), Value::Boolean(true));
        assert_eq!(coerce("42"), Value::Integer(42));
        assert_eq!(coerce("1.5"), Value::String("1.5".into()));
        assert_eq!(coerce("\"28\""), Value::String("28".into()));
        assert_eq!(coerce("\"true\""), Value::String("true".into()));
        assert_eq!(coerce("\"a \\\"b\\\"\""), Value::String("a \"b\"".into()));
        // An unbalanced or invalid literal stays the raw text.
        assert_eq!(coerce("\"x"), Value::String("\"x".into()));
        let error = resolve_with("", "", &[], &["server=1"]).unwrap_err();
        assert!(error.0.contains("table"), "{error}");
    }

    #[test]
    fn redacted_view_contains_provenance_only() {
        let resolved = resolve(&ResolveOptions {
            no_config: true,
            ..Default::default()
        })
        .unwrap();
        let view = resolved.redacted();
        assert_eq!(view["config"]["server"]["port"], 8080);
        assert_eq!(view["provenance"]["default_backend"], "built-in");
    }
}
