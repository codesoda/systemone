//! Edit one configuration file in place, keeping comments, key order and
//! every table setup does not touch.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::CliError;

/// A setting value setup writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    Text(String),
    Integer(i64),
}

impl Setting {
    fn to_value(&self) -> Value {
        match self {
            Self::Text(text) => Value::from(text.as_str()),
            Self::Integer(number) => Value::from(*number),
        }
    }
}

/// One backend instance as setup writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendEntry {
    pub name: String,
    pub kind: String,
    pub model: Option<String>,
    /// Keys under `[backends.<name>.settings]`, in the order to write them.
    pub settings: Vec<(String, Setting)>,
}

pub struct ConfigDocument {
    path: PathBuf,
    original: String,
    document: DocumentMut,
    existed: bool,
}

impl ConfigDocument {
    /// Read `path`, or start an empty document when it does not exist.
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let (original, existed) = match fs::read_to_string(path) {
            Ok(text) => (text, true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), false),
            Err(error) => {
                return Err(CliError::validation(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        let document = original.parse::<DocumentMut>().map_err(|error| {
            CliError::validation(format!("{} is not valid TOML: {error}", path.display()))
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            original,
            document,
            existed,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn existed(&self) -> bool {
        self.existed
    }

    #[must_use]
    pub fn original(&self) -> &str {
        &self.original
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.document.to_string()
    }

    /// `(name, kind)` of every backend this file defines, in file order.
    #[must_use]
    pub fn backends(&self) -> Vec<(String, Option<String>)> {
        let Some(backends) = self.document.get("backends").and_then(Item::as_table_like) else {
            return Vec::new();
        };
        backends
            .iter()
            .map(|(name, item)| {
                let kind = item
                    .as_table_like()
                    .and_then(|table| table.get("kind"))
                    .and_then(Item::as_str)
                    .map(str::to_owned);
                (name.to_owned(), kind)
            })
            .collect()
    }

    /// A string value from `[backends.<name>]` or its `settings`.
    #[must_use]
    pub fn backend_value(&self, name: &str, settings: bool, key: &str) -> Option<String> {
        let mut item = self.document.get("backends")?.get(name)?;
        if settings {
            item = item.get("settings")?;
        }
        match item.get(key)? {
            Item::Value(Value::String(text)) => Some(text.value().clone()),
            Item::Value(Value::Integer(number)) => Some(number.value().to_string()),
            _ => None,
        }
    }

    /// Add or update one backend. Other keys of the backend (for example
    /// `aliases` or `queue_capacity`) and other backends stay as they are.
    /// When the kind changes, the old `settings` table is replaced, because
    /// it belongs to the old kind's schema.
    pub fn set_backend(&mut self, entry: &BackendEntry) -> Result<(), CliError> {
        let path = self.path.display().to_string();
        let backends = self
            .document
            .entry("backends")
            .or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            })
            .as_table_mut()
            .ok_or_else(|| not_a_table(&path, "backends"))?;
        let backend = backends
            .entry(&entry.name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| not_a_table(&path, &format!("backends.{}", entry.name)))?;
        let kind_changed = backend
            .get("kind")
            .and_then(Item::as_str)
            .is_some_and(|kind| kind != entry.kind);
        backend["kind"] = toml_edit::value(entry.kind.as_str());
        backend["enabled"] = toml_edit::value(true);
        match &entry.model {
            Some(model) => backend["model"] = toml_edit::value(model.as_str()),
            None => {
                backend.remove("model");
            }
        }
        if kind_changed {
            backend.remove("settings");
        }
        if entry.settings.is_empty() && !backend.contains_key("settings") {
            return Ok(());
        }
        let settings = backend
            .entry("settings")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| not_a_table(&path, &format!("backends.{}.settings", entry.name)))?;
        for (key, value) in &entry.settings {
            settings[key.as_str()] = Item::Value(value.to_value());
        }
        Ok(())
    }

    pub fn set_default_backend(&mut self, name: &str) {
        self.document["default_backend"] = toml_edit::value(name);
    }

    /// Write the document. An existing file is first copied to
    /// `<file>.bak`; the new text goes to a temporary file that is renamed
    /// over the target, so a failed write never leaves half a config.
    pub fn write(&self) -> Result<Option<PathBuf>, CliError> {
        let io = |error: io::Error| {
            CliError::runtime(
                "config_io",
                format!("cannot write {}: {error}", self.path.display()),
            )
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        let backup = if self.existed {
            let backup = with_suffix(&self.path, ".bak");
            fs::write(&backup, &self.original).map_err(io)?;
            Some(backup)
        } else {
            None
        };
        let temporary = with_suffix(&self.path, ".tmp");
        fs::write(&temporary, self.text()).map_err(io)?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            io(error)
        })?;
        Ok(backup)
    }
}

fn not_a_table(path: &str, key: &str) -> CliError {
    CliError::validation(format!(
        "{path}: {key} is not a [table]; setup only edits standard tables, so edit this file by hand"
    ))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// A short line diff for the review step: `-` removed, `+` added, with up
/// to two unchanged lines of context around each change.
#[must_use]
pub fn diff(before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    // Longest common subsequence table; config files are small.
    let mut lcs = vec![vec![0_usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut lines: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            lines.push((' ', old[i]));
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            lines.push(('-', old[i]));
            i += 1;
        } else {
            lines.push(('+', new[j]));
            j += 1;
        }
    }
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (mark, _))| *mark != ' ')
        .map(|(index, _)| index)
        .collect();
    let near_change = |index: usize| {
        changed
            .iter()
            .any(|&change| index + 2 >= change && index <= change + 2)
    };
    let mut out = Vec::new();
    let mut skipped = false;
    for (index, (mark, line)) in lines.iter().enumerate() {
        if *mark != ' ' || near_change(index) {
            if skipped {
                out.push("  …".to_owned());
            }
            skipped = false;
            out.push(format!("{mark} {line}"));
        } else {
            skipped = true;
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use systemone_config::toml;

    use super::*;

    const EXISTING: &str = r#"# My settings — keep this comment.
default_backend = "local"

[server]
port = 9090 # inline comment

[backends.local]
kind = "openjev"
enabled = true
model = "qwen3-0.6b"
aliases = ["fast"]

[backends.local.settings]
device = "cpu"
threads = 8 # tuned
"#;

    fn load(text: &str) -> (tempfile::TempDir, ConfigDocument) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("systemone.config.toml");
        fs::write(&path, text).unwrap();
        let document = ConfigDocument::load(&path).unwrap();
        (root, document)
    }

    fn laya() -> BackendEntry {
        BackendEntry {
            name: "local-laya".to_owned(),
            kind: "laya".to_owned(),
            model: None,
            settings: vec![
                ("profile".to_owned(), Setting::Text("english".to_owned())),
                (
                    "model_dir".to_owned(),
                    Setting::Text("~/.systemone/models/laya/english".to_owned()),
                ),
                ("device".to_owned(), Setting::Text("cpu".to_owned())),
            ],
        }
    }

    #[test]
    fn adding_a_backend_keeps_comments_order_and_other_backends() {
        let (_root, mut document) = load(EXISTING);
        document.set_backend(&laya()).unwrap();
        document.set_default_backend("local-laya");
        let text = document.text();
        assert!(
            text.starts_with(&EXISTING.replace(
                "default_backend = \"local\"",
                "default_backend = \"local-laya\""
            )),
            "{text}"
        );
        assert!(text.ends_with(
            "\n[backends.local-laya]\nkind = \"laya\"\nenabled = true\n\n[backends.local-laya.settings]\nprofile = \"english\"\nmodel_dir = \"~/.systemone/models/laya/english\"\ndevice = \"cpu\"\n"
        ), "{text}");
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["backends"]["local"]["settings"]["threads"].as_integer(),
            Some(8)
        );
    }

    #[test]
    fn editing_the_same_kind_keeps_unrelated_keys() {
        let (_root, mut document) = load(EXISTING);
        document
            .set_backend(&BackendEntry {
                name: "local".to_owned(),
                kind: "openjev".to_owned(),
                model: Some("qwen3.5-4b".to_owned()),
                settings: vec![("device".to_owned(), Setting::Text("metal".to_owned()))],
            })
            .unwrap();
        let text = document.text();
        assert!(text.contains("model = \"qwen3.5-4b\""), "{text}");
        assert!(text.contains("aliases = [\"fast\"]"), "{text}");
        assert!(text.contains("threads = 8 # tuned"), "{text}");
        assert!(text.contains("device = \"metal\""), "{text}");
        assert!(
            text.contains("# My settings — keep this comment."),
            "{text}"
        );
    }

    #[test]
    fn changing_the_kind_replaces_the_settings_table() {
        let (_root, mut document) = load(EXISTING);
        document
            .set_backend(&BackendEntry {
                name: "local".to_owned(),
                kind: "typesafe".to_owned(),
                model: None,
                settings: vec![(
                    "api_key_env".to_owned(),
                    Setting::Text("TYPESAFE_API_KEY".to_owned()),
                )],
            })
            .unwrap();
        let parsed: toml::Table = toml::from_str(&document.text()).unwrap();
        let settings = parsed["backends"]["local"]["settings"].as_table().unwrap();
        assert_eq!(settings.len(), 1);
        assert!(parsed["backends"]["local"].get("model").is_none());
    }

    #[test]
    fn a_new_file_matches_the_readme_shape() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/systemone.config.toml");
        let mut document = ConfigDocument::load(&path).unwrap();
        assert!(!document.existed());
        document.set_backend(&laya()).unwrap();
        document.set_default_backend("local-laya");
        assert_eq!(document.write().unwrap(), None);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "default_backend = \"local-laya\"\n\n[backends.local-laya]\nkind = \"laya\"\nenabled = true\n\n[backends.local-laya.settings]\nprofile = \"english\"\nmodel_dir = \"~/.systemone/models/laya/english\"\ndevice = \"cpu\"\n"
        );
    }

    #[test]
    fn writing_keeps_a_backup_and_leaves_no_temporary_file() {
        let (root, mut document) = load(EXISTING);
        document.set_backend(&laya()).unwrap();
        let backup = document.write().unwrap().unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), EXISTING);
        let mut names: Vec<String> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["systemone.config.toml", "systemone.config.toml.bak"]
        );
    }

    #[test]
    fn lists_backends_and_reads_values() {
        let (_root, document) = load(EXISTING);
        assert_eq!(
            document.backends(),
            [("local".to_owned(), Some("openjev".to_owned()))]
        );
        assert_eq!(
            document.backend_value("local", true, "threads").as_deref(),
            Some("8")
        );
        assert_eq!(
            document.backend_value("local", false, "model").as_deref(),
            Some("qwen3-0.6b")
        );
    }

    #[test]
    fn inline_backends_are_refused_rather_than_rewritten() {
        let (_root, mut document) = load("backends = { local = { kind = \"openjev\" } }\n");
        let error = document.set_backend(&laya()).unwrap_err();
        assert!(
            error.to_string().contains("edit this file by hand"),
            "{error}"
        );
    }

    #[test]
    fn diff_marks_changes_with_context() {
        let text = diff("a\nb\nc\nd\ne\nf\ng\n", "a\nb\nc\nd\nE\nf\ng\nh\n");
        assert_eq!(text, "  …\n  c\n  d\n- e\n+ E\n  f\n  g\n+ h");
    }
}
