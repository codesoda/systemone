//! `s1 setup`: walk a user through choosing, configuring and downloading one
//! backend, then write it to a config file.
//!
//! Every question goes through [`prompt::Prompter`] and every side effect
//! (model cache listing, downloads, the test decision) through [`Services`],
//! so each flow is testable with scripted answers and no network.
//!
//! Rules this command keeps:
//! - prompts and progress go to stderr; stdout gets one JSON summary;
//! - nothing is written until the merged result passes the same checks as
//!   `s1 config check`, and the user confirms;
//! - secrets are never asked for or written: hosted kinds store only the
//!   *name* of the environment variable that holds the key;
//! - only devices this build supports are offered.

pub mod document;
pub mod prompt;
mod services;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::Serialize;
use systemone_config::{Config, ConfigError, ResolveOptions, Resolved};
use systemone_core::{
    BackendId, DownloadPlan, HostError, ModelStatus, ProviderKind, paths::absolutize,
};
use systemone_weights::format_bytes;

pub use services::RealServices;

use self::{
    document::{BackendEntry, ConfigDocument, Setting},
    prompt::Prompter,
};
use crate::{CliError, args::SetupArgs, backends};

/// What this binary can run, per kind. Devices are listed best first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub openjev: Vec<&'static str>,
    pub laya: Vec<&'static str>,
    pub kev: Vec<&'static str>,
    pub gliner2: bool,
}

impl Build {
    #[must_use]
    pub fn current() -> Self {
        let openjev = match systemone_openjev::compiled_feature() {
            "native-metal" => vec!["metal", "cpu"],
            "native-cuda" => vec!["cuda", "cpu"],
            "native-cpu" => vec!["cpu"],
            _ => Vec::new(),
        };
        let laya = [
            (systemone_laya::DeviceSetting::Metal, "metal"),
            (systemone_laya::DeviceSetting::Cpu, "cpu"),
        ]
        .into_iter()
        .filter(|(device, _)| systemone_laya::settings::device_compiled(*device))
        .map(|(_, name)| name)
        .collect();
        let kev = [
            (systemone_kev::DeviceSetting::Metal, "metal"),
            (systemone_kev::DeviceSetting::Cpu, "cpu"),
        ]
        .into_iter()
        .filter(|(device, _)| systemone_kev::settings::device_compiled(*device))
        .map(|(_, name)| name)
        .collect();
        Self {
            openjev,
            laya,
            kev,
            gliner2: systemone_gliner2::compiled_feature() != "backend-disabled",
        }
    }

    /// `Ok` when this build can run `kind`, else the reason it cannot.
    fn supports(&self, kind: ProviderKind) -> Result<(), String> {
        let missing = |feature: &str| Err(format!("needs a build with {feature}"));
        match kind {
            ProviderKind::OpenJev if self.openjev.is_empty() => {
                missing("--features native (CPU), metal or cuda")
            }
            ProviderKind::Laya if self.laya.is_empty() => {
                missing("--features laya-cpu or laya-metal")
            }
            ProviderKind::Kev if self.kev.is_empty() => missing("--features kev-cpu or kev-metal"),
            ProviderKind::Gliner2 if !self.gliner2 => missing("--features gliner2"),
            ProviderKind::Vercel | ProviderKind::OpenRouter => {
                Err("planned; not implemented yet (issue #3)".to_owned())
            }
            _ => Ok(()),
        }
    }
}

/// Where setup reads and writes, and what the build supports.
pub struct Context {
    pub user_file: Option<PathBuf>,
    pub project_file: PathBuf,
    pub home: Option<PathBuf>,
    /// Environment for config resolution; `None` reads the process.
    pub environment: Option<Vec<(OsString, OsString)>>,
    pub build: Build,
}

impl Context {
    pub fn current() -> Result<Self, CliError> {
        let project_file = std::env::current_dir()
            .map_err(|error| {
                CliError::runtime("cwd", format!("cannot read the current directory: {error}"))
            })?
            .join(systemone_config::FILE_NAME);
        Ok(Self {
            user_file: systemone_config::user_file_path(),
            project_file,
            home: systemone_config::home_directory(),
            environment: None,
            build: Build::current(),
        })
    }

    fn resolve(&self, replace: Option<(&Path, &Path)>) -> Result<Resolved, ConfigError> {
        let swap = |path: PathBuf| match replace {
            Some((target, candidate)) if path == target => candidate.to_path_buf(),
            _ => path,
        };
        systemone_config::resolve(&ResolveOptions {
            no_config: false,
            user_file: Some(
                self.user_file
                    .clone()
                    .map_or_else(|| PathBuf::from("/nonexistent/s1-user-config"), swap),
            ),
            project_file: Some(swap(self.project_file.clone())),
            environment: self.environment.clone(),
            overrides: Vec::new(),
        })
    }
}

/// Side effects, so tests can replace them.
pub trait Services {
    fn env_is_set(&self, name: &str) -> bool;
    fn available_space(&self, directory: &Path) -> Option<u64>;
    /// Registered OpenJev models and whether each is cached.
    fn openjev_models(&mut self, device: &str) -> Result<Vec<ModelStatus>, CliError>;
    fn pull_openjev(&mut self, device: &str, model: &ModelStatus) -> Result<(), CliError>;
    fn download(&mut self, plan: &DownloadPlan, directory: &Path) -> Result<(), CliError>;
    /// Load `backend` from `config` and answer one yes/no question.
    fn test_decision(&mut self, config: &Config, backend: &str) -> Result<TestResult, CliError>;
}

pub struct TestResult {
    pub model: String,
    pub probability_true: f64,
    pub seconds: f64,
}

/// The one JSON document setup writes to stdout.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub schema: &'static str,
    pub path: String,
    pub backend: String,
    pub kind: String,
    pub default: bool,
    pub written: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// `downloaded`, `present`, `skipped`, `declined`, `unavailable` or `not_needed`.
    pub download: &'static str,
    /// `passed`, `failed`, `skipped` or `declined`.
    pub test: &'static str,
}

/// What the kind-specific step decided.
struct Choice {
    entry: BackendEntry,
    model: ModelSource,
}

enum ModelSource {
    Files {
        plan: Result<DownloadPlan, String>,
        directory: PathBuf,
    },
    OpenJev {
        device: String,
        model: Box<ModelStatus>,
    },
    Hosted {
        api_key_env: String,
    },
}

const KINDS: [(ProviderKind, &str); 7] = [
    (
        ProviderKind::OpenJev,
        "local LLM next-token scoring (llama.cpp)",
    ),
    (ProviderKind::Laya, "local encoder with decision heads"),
    (ProviderKind::Kev, "local pointer-head model on Qwen"),
    (
        ProviderKind::Gliner2,
        "local GLiNER2.5 classifier (ONNX, CPU)",
    ),
    (ProviderKind::Typesafe, "hosted Jev API (needs an API key)"),
    (ProviderKind::Vercel, "hosted Jev through Vercel AI Gateway"),
    (ProviderKind::OpenRouter, "hosted Jev through OpenRouter"),
];

fn parse_kind(text: &str) -> Result<ProviderKind, CliError> {
    KINDS
        .iter()
        .map(|(kind, _)| *kind)
        .find(|kind| kind.as_str() == text)
        .ok_or_else(|| {
            let names: Vec<&str> = KINDS.iter().map(|(kind, _)| kind.as_str()).collect();
            CliError::validation(format!(
                "unknown kind {text:?}; expected one of {}",
                names.join(", ")
            ))
        })
}

pub fn run(
    args: &SetupArgs,
    context: &Context,
    prompter: &mut dyn Prompter,
    services: &mut dyn Services,
) -> Result<Summary, CliError> {
    prompter.say("SystemOne setup: choose a backend, configure it, and fetch its model.");
    let target = choose_target(args, context, prompter)?;
    let mut document = ConfigDocument::load(&target)?;

    let current = context.resolve(None);
    let known = match &current {
        Ok(resolved) => {
            show_backends(resolved, prompter);
            resolved
                .config
                .backends
                .keys()
                .map(ToString::to_string)
                .collect()
        }
        Err(error) => {
            prompter.say(&format!(
                "Your current configuration does not resolve: {error}\nSetup can still change {}; the result must pass validation before it is written.",
                target.display()
            ));
            document
                .backends()
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        }
    };

    let editable = editable_backends(&document, current.as_ref().ok());
    let (name, kind, editing) = choose_backend(args, context, prompter, &editable, &known)?;
    let choice = configure_kind(kind, &name, editing, &document, context, prompter, services)?;

    let already_default = current
        .as_ref()
        .ok()
        .and_then(|resolved| resolved.config.default_backend.as_ref())
        .is_some_and(|default| default.as_str() == name);
    let make_default = already_default
        || prompter.confirm(&format!("Make {name} the default backend?"), !editing)?;

    document.set_backend(&choice.entry)?;
    if make_default && !already_default {
        document.set_default_backend(&name);
    }
    validate_candidate(context, &document)?;

    let mut summary = Summary {
        schema: "systemone-setup-v1",
        path: target.display().to_string(),
        backend: name.clone(),
        kind: kind.as_str().to_owned(),
        default: make_default,
        written: false,
        backup: None,
        download: "skipped",
        test: "skipped",
    };

    let text = document.text();
    if text == document.original() {
        prompter.say(&format!("{} already has these settings.", target.display()));
    } else {
        prompter.say(&format!(
            "Changes to {}:\n{}",
            target.display(),
            document::diff(document.original(), &text)
        ));
        if !prompter.confirm(&format!("Write {}?", target.display()), true)? {
            prompter.say("Nothing was written.");
            summary.download = "declined";
            return Ok(summary);
        }
        let backup = document.write()?;
        if let Some(backup) = &backup {
            prompter.say(&format!("Saved the previous file as {}.", backup.display()));
        }
        prompter.say(&format!("Wrote {}.", target.display()));
        summary.backup = backup.map(|path| path.display().to_string());
    }
    summary.written = true;

    summary.download = fetch_model(args, &name, &choice.model, prompter, services)?;
    summary.test = test_decision(
        args,
        context,
        &name,
        kind,
        &choice.model,
        &summary,
        prompter,
        services,
    )?;
    next_steps(&name, make_default, prompter);
    Ok(summary)
}

fn choose_target(
    args: &SetupArgs,
    context: &Context,
    prompter: &mut dyn Prompter,
) -> Result<PathBuf, CliError> {
    let no_home = || {
        CliError::validation("no home directory is set, so there is no user config; use --project")
    };
    if args.user {
        return context.user_file.clone().ok_or_else(no_home);
    }
    if args.project {
        return Ok(context.project_file.clone());
    }
    let Some(user) = &context.user_file else {
        prompter.say("No home directory is set; using the project config.");
        return Ok(context.project_file.clone());
    };
    let state = |path: &Path| if path.exists() { "exists" } else { "new" };
    let items = vec![
        format!("User config     {} ({})", user.display(), state(user)),
        format!(
            "Project config  {} ({}; overrides the user config)",
            context.project_file.display(),
            state(&context.project_file)
        ),
    ];
    let index = prompter.select("Which config file should setup write?", &items, 0)?;
    Ok(if index == 0 {
        user.clone()
    } else {
        context.project_file.clone()
    })
}

fn show_backends(resolved: &Resolved, prompter: &mut dyn Prompter) {
    let default = resolved.config.default_backend.as_ref();
    let mut lines = vec!["Configured backends:".to_owned()];
    for (id, backend) in &resolved.config.backends {
        let source = resolved
            .provenance
            .get(&format!("backends.{id}.kind"))
            .map_or("", String::as_str);
        lines.push(format!(
            "  {:<20} {:<10} {:<8} {}{}",
            id.as_str(),
            backend.kind.as_str(),
            if backend.enabled {
                "enabled"
            } else {
                "disabled"
            },
            source,
            if default == Some(id) {
                "  (default)"
            } else {
                ""
            }
        ));
    }
    prompter.say(&lines.join("\n"));
}

/// Backends setup may edit in this file: the ones it defines, plus the
/// built-in `local` instance when no file has redefined it.
fn editable_backends(
    document: &ConfigDocument,
    resolved: Option<&Resolved>,
) -> Vec<(String, ProviderKind)> {
    let mut editable: Vec<(String, ProviderKind)> = document
        .backends()
        .into_iter()
        .filter_map(|(name, kind)| Some((name, parse_kind(kind.as_deref()?).ok()?)))
        .collect();
    if let Some(resolved) = resolved {
        let builtin_local = resolved
            .provenance
            .get("backends.local.kind")
            .is_some_and(|source| source == "built-in");
        if builtin_local && !editable.iter().any(|(name, _)| name == "local") {
            editable.push(("local".to_owned(), ProviderKind::OpenJev));
        }
    }
    editable
}

fn choose_backend(
    args: &SetupArgs,
    context: &Context,
    prompter: &mut dyn Prompter,
    editable: &[(String, ProviderKind)],
    known: &[String],
) -> Result<(String, ProviderKind, bool), CliError> {
    if let Some(name) = &args.backend {
        BackendId::new(name.as_str())?;
        if let Some((_, kind)) = editable.iter().find(|(existing, _)| existing == name) {
            if let Some(requested) = &args.kind
                && parse_kind(requested)? != *kind
            {
                return Err(CliError::validation(format!(
                    "backend {name} is kind {kind}; setup does not change the kind of an existing backend"
                )));
            }
            return Ok((name.clone(), *kind, true));
        }
        if known.contains(name) {
            return Err(CliError::validation(format!(
                "backend {name} is defined in another config file; run setup for that file (--user or --project)"
            )));
        }
        let kind = choose_kind(args, context, prompter)?;
        return Ok((name.clone(), kind, false));
    }
    if !editable.is_empty() && args.kind.is_none() {
        let mut items = vec!["Add a new backend".to_owned()];
        items.extend(
            editable
                .iter()
                .map(|(name, kind)| format!("Edit {name} ({kind})")),
        );
        let index = prompter.select("What do you want to do?", &items, 0)?;
        if index > 0 {
            let (name, kind) = &editable[index - 1];
            return Ok((name.clone(), *kind, true));
        }
    }
    let kind = choose_kind(args, context, prompter)?;
    let suggestion = suggest_name(kind, known);
    let name = loop {
        let name = prompter.input("Name for this backend", &suggestion)?;
        if let Err(error) = BackendId::new(name.as_str()) {
            prompter.say(&format!("{error}"));
            continue;
        }
        if known.contains(&name) {
            prompter.say(&format!(
                "A backend named {name} already exists; pick another name or edit it instead."
            ));
            continue;
        }
        break name;
    };
    Ok((name, kind, false))
}

fn choose_kind(
    args: &SetupArgs,
    context: &Context,
    prompter: &mut dyn Prompter,
) -> Result<ProviderKind, CliError> {
    if let Some(text) = &args.kind {
        let kind = parse_kind(text)?;
        context
            .build
            .supports(kind)
            .map_err(|reason| CliError::validation(format!("kind {kind}: {reason}")))?;
        return Ok(kind);
    }
    let items: Vec<String> = KINDS
        .iter()
        .map(|(kind, about)| match context.build.supports(*kind) {
            Ok(()) => format!("{:<10} {about}", kind.as_str()),
            Err(reason) => format!("{:<10} {about} (unavailable: {reason})", kind.as_str()),
        })
        .collect();
    let default = KINDS
        .iter()
        .position(|(kind, _)| context.build.supports(*kind).is_ok())
        .unwrap_or(0);
    loop {
        let index = prompter.select("Which kind of backend?", &items, default)?;
        let kind = KINDS[index].0;
        match context.build.supports(kind) {
            Ok(()) => return Ok(kind),
            Err(reason) => prompter.say(&format!("{kind} is not available: {reason}.")),
        }
    }
}

fn suggest_name(kind: ProviderKind, known: &[String]) -> String {
    let base = match kind {
        ProviderKind::OpenJev => "local".to_owned(),
        ProviderKind::Typesafe | ProviderKind::Vercel | ProviderKind::OpenRouter => {
            kind.as_str().to_owned()
        }
        _ => format!("local-{}", kind.as_str()),
    };
    let taken = |name: &str| known.iter().any(|existing| existing == name);
    if !taken(&base) {
        return base;
    }
    let with_kind = format!("{base}-{}", kind.as_str());
    if kind == ProviderKind::OpenJev && !taken(&with_kind) {
        return with_kind;
    }
    (2..)
        .map(|index| format!("{base}-{index}"))
        .find(|name| !taken(name))
        .unwrap_or(base)
}

fn select_from(
    prompter: &mut dyn Prompter,
    prompt: &str,
    options: &[&str],
    current: Option<&str>,
) -> Result<String, CliError> {
    let items: Vec<String> = options.iter().map(|option| (*option).to_owned()).collect();
    let default = current
        .and_then(|current| options.iter().position(|option| *option == current))
        .unwrap_or(0);
    let index = prompter.select(prompt, &items, default)?;
    Ok(items[index].clone())
}

fn ask_directory(
    prompter: &mut dyn Prompter,
    context: &Context,
    default: &str,
) -> Result<(String, PathBuf), CliError> {
    loop {
        let text = prompter.input("Model directory", default)?;
        if text.is_empty() {
            prompter.say("A model directory is required.");
            continue;
        }
        match absolutize(Path::new(&text), context.home.as_deref()) {
            Ok(path) if path.exists() && !path.is_dir() => {
                prompter.say(&format!(
                    "{} exists and is not a directory.",
                    path.display()
                ));
            }
            Ok(path) => return Ok((text, path)),
            Err(error) => prompter.say(&error.to_string()),
        }
    }
}

fn configure_kind(
    kind: ProviderKind,
    name: &str,
    editing: bool,
    document: &ConfigDocument,
    context: &Context,
    prompter: &mut dyn Prompter,
    services: &mut dyn Services,
) -> Result<Choice, CliError> {
    let current = |settings: bool, key: &str| {
        if editing {
            document.backend_value(name, settings, key)
        } else {
            None
        }
    };
    let text = |value: &str| Setting::Text(value.to_owned());
    let entry = |model: Option<String>, settings: Vec<(&str, Setting)>| BackendEntry {
        name: name.to_owned(),
        kind: kind.as_str().to_owned(),
        model,
        settings: settings
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    };
    match kind {
        ProviderKind::OpenJev => {
            let device = select_from(
                prompter,
                "Device",
                &context.build.openjev,
                current(true, "device").as_deref(),
            )?;
            prompter.say(
                "Checking the OpenJev model cache (cached models are re-verified by SHA-256)…",
            );
            let models = services.openjev_models(&device)?;
            if models.is_empty() {
                return Err(CliError::runtime(
                    "no_models",
                    "the OpenJev registry lists no models",
                ));
            }
            let items: Vec<String> = models
                .iter()
                .map(|model| {
                    format!(
                        "{:<16} {:>9}  {}",
                        model.id,
                        format_bytes(model.bytes),
                        if model.cached && model.verified {
                            "downloaded"
                        } else {
                            "not downloaded"
                        }
                    )
                })
                .collect();
            let preferred = current(false, "model").unwrap_or_else(|| "qwen3-0.6b".to_owned());
            let default = models
                .iter()
                .position(|model| model.id == preferred)
                .unwrap_or(0);
            let model = models[prompter.select("Model", &items, default)?].clone();
            Ok(Choice {
                entry: entry(Some(model.id.clone()), vec![("device", text(&device))]),
                model: ModelSource::OpenJev {
                    device,
                    model: Box::new(model),
                },
            })
        }
        ProviderKind::Laya => {
            let profile = select_from(
                prompter,
                "Profile",
                &systemone_laya::settings::PROFILES,
                current(true, "profile").as_deref(),
            )?;
            let device = select_from(
                prompter,
                "Device",
                &context.build.laya,
                current(true, "device").as_deref(),
            )?;
            let default_dir = current(true, "model_dir")
                .unwrap_or_else(|| format!("~/.systemone/models/laya/{profile}"));
            let (dir_text, directory) = ask_directory(prompter, context, &default_dir)?;
            Ok(Choice {
                entry: entry(
                    None,
                    vec![
                        ("profile", text(&profile)),
                        ("model_dir", text(&dir_text)),
                        ("device", text(&device)),
                    ],
                ),
                model: ModelSource::Files {
                    plan: systemone_laya::download::download_plan(&profile)
                        .map_err(|error| error.to_string()),
                    directory,
                },
            })
        }
        ProviderKind::Kev => {
            let available: Vec<&str> = systemone_kev::download::CHECKPOINTS
                .iter()
                .filter(|(_, device)| {
                    context
                        .build
                        .kev
                        .contains(&systemone_kev::settings::device_name(*device))
                })
                .map(|(checkpoint, _)| *checkpoint)
                .collect();
            let items: Vec<String> = available
                .iter()
                .map(|checkpoint| {
                    let device = systemone_kev::download::checkpoint_device(checkpoint)
                        .map_or("?", systemone_kev::settings::device_name);
                    let size = systemone_kev::download::download_plan(checkpoint)
                        .map_or_else(|_| String::new(), |plan| format_bytes(plan.total_bytes()));
                    format!("{checkpoint:<10} {device:<6} {size:>9}")
                })
                .collect();
            let preferred = current(false, "model");
            let default = available
                .iter()
                .position(|checkpoint| Some(*checkpoint) == preferred.as_deref())
                .unwrap_or(0);
            let checkpoint = available[prompter.select("Checkpoint", &items, default)?];
            let device = systemone_kev::download::checkpoint_device(checkpoint)
                .map_or("cpu", systemone_kev::settings::device_name);
            let default_dir = current(true, "model_dir")
                .unwrap_or_else(|| format!("~/.systemone/models/kev/{checkpoint}"));
            let (dir_text, directory) = ask_directory(prompter, context, &default_dir)?;
            Ok(Choice {
                entry: entry(
                    Some(checkpoint.to_owned()),
                    vec![("model_dir", text(&dir_text)), ("device", text(device))],
                ),
                model: ModelSource::Files {
                    plan: systemone_kev::download::download_plan(checkpoint)
                        .map_err(|error| error.to_string()),
                    directory,
                },
            })
        }
        ProviderKind::Gliner2 => {
            let profile = select_from(
                prompter,
                "Profile",
                &systemone_gliner2::settings::PROFILES,
                Some(
                    current(true, "profile")
                        .as_deref()
                        .unwrap_or(systemone_gliner2::settings::DEFAULT_PROFILE),
                ),
            )?;
            let default_dir = current(true, "model_dir")
                .unwrap_or_else(|| format!("~/.systemone/models/gliner2/gliner2.5-{profile}-v1"));
            let (dir_text, directory) = ask_directory(prompter, context, &default_dir)?;
            Ok(Choice {
                entry: entry(
                    None,
                    vec![("profile", text(&profile)), ("model_dir", text(&dir_text))],
                ),
                model: ModelSource::Files {
                    plan: systemone_gliner2::download::download_plan(&profile)
                        .map_err(|error| error.to_string()),
                    directory,
                },
            })
        }
        ProviderKind::Typesafe => {
            prompter.say("TypeSafe reads its API key from an environment variable. Setup stores only the variable's name, never the key.");
            let default =
                current(true, "api_key_env").unwrap_or_else(|| "TYPESAFE_API_KEY".to_owned());
            let api_key_env = loop {
                let value =
                    prompter.input("Environment variable that holds the API key", &default)?;
                if valid_env_name(&value) {
                    break value;
                }
                prompter.say("Use letters, digits and underscores, starting with a letter or underscore (for example TYPESAFE_API_KEY).");
            };
            if services.env_is_set(&api_key_env) {
                prompter.say(&format!("{api_key_env} is set in this shell."));
            } else {
                prompter.say(&format!(
                    "{api_key_env} is not set. Set it before you run s1, for example in your shell profile:\n  export {api_key_env}=<your key>"
                ));
            }
            Ok(Choice {
                entry: entry(None, vec![("api_key_env", text(&api_key_env))]),
                model: ModelSource::Hosted { api_key_env },
            })
        }
        ProviderKind::Vercel | ProviderKind::OpenRouter => Err(CliError::validation(format!(
            "kind {kind} is planned but not implemented yet"
        ))),
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Resolve the whole configuration with the candidate text in place of the
/// target file, then run the same checks as `s1 config check`.
fn validate_candidate(context: &Context, document: &ConfigDocument) -> Result<(), CliError> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let candidate = std::env::temp_dir().join(format!(
        "s1-setup-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&candidate, document.text()).map_err(|error| {
        CliError::runtime("config_io", format!("cannot stage the new config: {error}"))
    })?;
    let resolved = context.resolve(Some((document.path(), &candidate)));
    let _ = std::fs::remove_file(&candidate);
    let resolved = resolved.map_err(|error| {
        CliError::validation(format!(
            "the new configuration would not be valid, so nothing was written: {error}"
        ))
    })?;
    let mut problems = Vec::new();
    for instance in backends::configure_all(&resolved.config) {
        if let Err(error) = &instance.backend {
            match error {
                HostError::Unsupported(_) if !instance.config.enabled => {}
                _ => problems.push(format!("{}: {error}", instance.id)),
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(CliError::validation(format!(
            "the new configuration would not pass s1 config check, so nothing was written: {}",
            problems.join("; ")
        )))
    }
}

fn fetch_model(
    args: &SetupArgs,
    name: &str,
    model: &ModelSource,
    prompter: &mut dyn Prompter,
    services: &mut dyn Services,
) -> Result<&'static str, CliError> {
    let retry = format!("The config is saved. To download later, run: s1 setup --backend {name}");
    match model {
        ModelSource::Hosted { .. } => Ok("not_needed"),
        ModelSource::Files {
            plan: Err(reason), ..
        } => {
            prompter.say(&format!("Setup cannot download this model: {reason}."));
            Ok("unavailable")
        }
        ModelSource::Files {
            plan: Ok(plan),
            directory,
        } => {
            let missing = systemone_weights::missing_bytes(plan, directory)
                .map_err(|error| CliError::runtime("model_files", error.to_string()))?;
            if missing == 0 {
                prompter.say(&format!(
                    "All {} files of {} are already in {}.",
                    plan.files.len(),
                    plan.label,
                    directory.display()
                ));
                return Ok("present");
            }
            if args.no_download {
                prompter.say(&format!(
                    "Skipping the download ({}). {retry}",
                    format_bytes(missing)
                ));
                return Ok("skipped");
            }
            let space = services.available_space(directory);
            let space_text = space.map_or_else(
                || "free space unknown".to_owned(),
                |bytes| format!("{} free", format_bytes(bytes)),
            );
            prompter.say(&format!(
                "{}: {} to download into {} ({space_text}).",
                plan.label,
                format_bytes(missing),
                directory.display()
            ));
            let enough = space.is_none_or(|bytes| bytes > missing);
            if !enough {
                prompter.say("There is not enough free space for this download.");
            }
            if !prompter.confirm("Download it now?", enough)? {
                prompter.say(&retry);
                return Ok("declined");
            }
            services.download(plan, directory).map_err(|error| {
                CliError::runtime("download_failed", format!("{error}. {retry}"))
            })?;
            prompter.say(&format!("Downloaded and verified {}.", plan.label));
            Ok("downloaded")
        }
        ModelSource::OpenJev { device, model } => {
            if model.cached && model.verified {
                prompter.say(&format!("{} is already downloaded and verified.", model.id));
                return Ok("present");
            }
            if args.no_download {
                prompter.say(&format!(
                    "Skipping the download ({}). {retry}",
                    format_bytes(model.bytes)
                ));
                return Ok("skipped");
            }
            if !prompter.confirm(
                &format!("Download {} ({}) now?", model.id, format_bytes(model.bytes)),
                true,
            )? {
                prompter.say(&retry);
                return Ok("declined");
            }
            services.pull_openjev(device, model).map_err(|error| {
                CliError::runtime("download_failed", format!("{error}. {retry}"))
            })?;
            prompter.say(&format!("Downloaded and verified {}.", model.id));
            Ok("downloaded")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn test_decision(
    args: &SetupArgs,
    context: &Context,
    name: &str,
    kind: ProviderKind,
    model: &ModelSource,
    summary: &Summary,
    prompter: &mut dyn Prompter,
    services: &mut dyn Services,
) -> Result<&'static str, CliError> {
    if args.yes {
        return Ok("skipped");
    }
    let default = match model {
        ModelSource::Hosted { api_key_env } => {
            if !services.env_is_set(api_key_env) {
                return Ok("skipped");
            }
            prompter.say(&format!(
                "A test decision sends one request to {kind}, which is billed to your account."
            ));
            false
        }
        _ if matches!(summary.download, "present" | "downloaded") => true,
        _ => return Ok("skipped"),
    };
    if !prompter.confirm("Run one test decision now?", default)? {
        return Ok("declined");
    }
    let resolved = context
        .resolve(None)
        .map_err(|error| CliError::validation(error.to_string()))?;
    prompter.say("Loading the backend and asking one yes/no question…");
    match services.test_decision(&resolved.config, name) {
        Ok(result) => {
            prompter.say(&format!(
                "Test passed: {} answered P(yes) = {:.2} in {:.1} s.",
                result.model, result.probability_true, result.seconds
            ));
            Ok("passed")
        }
        Err(error) => {
            prompter.say(&format!("Test failed: {error}"));
            Ok("failed")
        }
    }
}

fn next_steps(name: &str, is_default: bool, prompter: &mut dyn Prompter) {
    let backend = if is_default {
        String::new()
    } else {
        format!(" --backend {name}")
    };
    prompter.say(&format!(
        "Next:\n  s1 noul{backend} --state 'The card was charged twice.' --question 'Was the customer charged more than once?'\n  s1 serve"
    ));
}

#[cfg(test)]
mod tests;
