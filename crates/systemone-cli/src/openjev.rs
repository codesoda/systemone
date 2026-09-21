//! `s1 openjev …`: host-specific tooling for `kind = "openjev"` instances.
//!
//! Model cache operations go through the generic `ModelStore` extension;
//! the shared/batch probe is OpenJev-only and runs the engine in a child
//! process so a native crash cannot take the parent down.

use std::io::Write;

use systemone_config::{Resolved, settings_to_json};
use systemone_core::{Backend, BackendId, ProviderKind};
use systemone_openjev::{OpenJevBackend, OpenJevSettings, probe};

use crate::{
    CliError,
    args::{OpenJevArgs, OpenJevCommand, OpenJevModelsCommand, ProbeModeArg},
    output,
};

pub const PROBE_CHILD_ENV: &str = "SYSTEMONE_OPENJEV_PROBE_CHILD";

fn select(
    resolved: &Resolved,
    selector: Option<&str>,
) -> Result<(BackendId, OpenJevBackend), CliError> {
    let config = &resolved.config;
    let id = match selector {
        Some(selector) => BackendId::new(selector)?,
        None => config.default_backend.clone().ok_or_else(|| {
            CliError::validation("no default_backend is configured; pass --backend")
        })?,
    };
    let backend = config
        .backends
        .get(&id)
        .ok_or_else(|| CliError::validation(format!("unknown backend {id}")))?;
    if backend.kind != ProviderKind::OpenJev {
        return Err(CliError::validation(format!(
            "backend {id} is kind {}, not openjev",
            backend.kind
        )));
    }
    let settings: OpenJevSettings = serde_json::from_value(settings_to_json(&backend.settings))
        .map_err(|error| CliError::validation(format!("backends.{id}.settings: {error}")))?;
    let backend = OpenJevBackend::new(
        id.clone(),
        backend.model.as_deref(),
        backend.aliases.clone(),
        &settings,
        systemone_config::home_directory().as_deref(),
    )?;
    Ok((id, backend))
}

pub fn execute<W: Write, E: Write>(
    resolved: &Resolved,
    args: &OpenJevArgs,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let pretty = resolved.config.output.pretty;
    match &args.command {
        OpenJevCommand::Models(models) => {
            let (id, backend) = select(resolved, args.backend.as_deref())?;
            let store = backend.model_store().require("model store")?;
            let (artifact, action) = match &models.command {
                OpenJevModelsCommand::Pull { id, repair } => (store.pull(id, *repair)?, "pull"),
                OpenJevModelsCommand::Path { id } => (store.path(id)?, "path"),
            };
            output::write_json(
                stdout,
                &serde_json::json!({
                    "schema": "systemone-openjev-model-v1",
                    "backend": id,
                    "action": action,
                    "artifact": artifact,
                }),
                pretty,
            )
            .map_err(|error| CliError::runtime("output_io", error.to_string()))
        }
        OpenJevCommand::Probe { id, mode } => probe_command(
            resolved,
            args.backend.as_deref(),
            id.as_deref(),
            *mode,
            stdout,
            stderr,
        ),
    }
}

#[cfg(feature = "native")]
fn probe_command<W: Write, E: Write>(
    resolved: &Resolved,
    backend_selector: Option<&str>,
    model: Option<&str>,
    mode: ProbeModeArg,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    use openjev_probe_mode as probe_mode;
    let pretty = resolved.config.output.pretty;
    let (backend_id, backend) = select(resolved, backend_selector)?;
    let settings = match model {
        Some(model) if model != backend.settings().model_label => {
            let raw: OpenJevSettings = serde_json::from_value(settings_to_json(
                &resolved.config.backends[&backend_id].settings,
            ))
            .map_err(|error| CliError::validation(error.to_string()))?;
            systemone_openjev::settings::resolve(
                &raw,
                Some(model),
                systemone_config::home_directory().as_deref(),
            )?
        }
        _ => backend.settings().clone(),
    };
    let mode = probe_mode(mode);

    if std::env::var(PROBE_CHILD_ENV).as_deref() == Ok("1") {
        let receipt = probe::run_probe(&settings, mode)?;
        let enabled = receipt.passed;
        let report = probe::ProbeReport {
            schema: probe::PROBE_REPORT_SCHEMA.to_owned(),
            process_status: "completed".to_owned(),
            failure_reason: receipt.failure_reason.clone(),
            receipt: Some(receipt),
            receipt_path: None,
            enabled,
        };
        output::write_json(stdout, &report, pretty)
            .map_err(|error| CliError::runtime("output_io", error.to_string()))?;
        return if enabled {
            Ok(())
        } else {
            Err(CliError::runtime("probe_failed", "probe did not pass"))
        };
    }

    // Parent: establish the exact probe identity and suspend prior receipts
    // before the child initializes llama.cpp. The child receives the
    // parent's already-resolved configuration as explicit overrides.
    let publication = probe::prepare_publication(&settings, mode)?;
    let executable = std::env::current_exe()
        .map_err(|error| CliError::runtime("probe_parent", error.to_string()))?;
    let mut command = std::process::Command::new(executable);
    command.arg("--no-config");
    for (path, value) in child_overrides(resolved, &backend_id) {
        command.arg("--set").arg(format!("{path}={value}"));
    }
    if pretty {
        command.arg("--pretty");
    }
    command
        .args(["openjev", "--backend", backend_id.as_str(), "probe"])
        .arg(&settings.model_label)
        .args(["--mode", mode.as_str()])
        .env(PROBE_CHILD_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command
        .output()
        .map_err(|error| CliError::runtime("probe_parent", error.to_string()))?;
    stderr
        .write_all(&child.stderr)
        .map_err(|error| CliError::runtime("stderr_io", error.to_string()))?;
    let parsed = serde_json::from_slice::<probe::ProbeReport>(&child.stdout).ok();
    let mut report = match parsed {
        Some(report) if report.schema == probe::PROBE_REPORT_SCHEMA => report,
        Some(_) => probe::ProbeReport {
            schema: probe::PROBE_REPORT_SCHEMA.to_owned(),
            process_status: "invalid-child-report".to_owned(),
            receipt: None,
            receipt_path: None,
            enabled: false,
            failure_reason: Some("probe child returned an unexpected report schema".to_owned()),
        },
        None => {
            let status = child.status.code().map_or_else(
                || "terminated-by-signal".to_owned(),
                |code| format!("exit-{code}"),
            );
            probe::ProbeReport {
                schema: probe::PROBE_REPORT_SCHEMA.to_owned(),
                process_status: status,
                receipt: None,
                receipt_path: None,
                enabled: false,
                failure_reason: Some(if child.stdout.is_empty() {
                    "probe child crashed or failed before producing a JSON report".to_owned()
                } else {
                    "probe child produced malformed JSON; no success receipt was accepted"
                        .to_owned()
                }),
            }
        }
    };
    report.enabled = false;
    report.receipt_path = None;
    if report.process_status == "completed"
        && let Some(receipt) = report.receipt.clone()
    {
        if receipt.passed && !child.status.success() {
            report.process_status = "child-nonzero-after-passing-report".to_owned();
            report.failure_reason = Some(
                "probe child did not exit successfully; its passing candidate was not published"
                    .to_owned(),
            );
        } else {
            match publication.publish_child_result(&receipt, child.status.success()) {
                Ok(path) => {
                    report.receipt_path = Some(path.display().to_string());
                    report.enabled = receipt.passed && child.status.success();
                }
                Err(error) => {
                    report.process_status = "invalid-child-report".to_owned();
                    report.failure_reason = Some(format!(
                        "probe child receipt was not published by the parent: {error}"
                    ));
                }
            }
        }
    }
    let enabled = report.enabled;
    output::write_json(stdout, &report, pretty)
        .map_err(|error| CliError::runtime("output_io", error.to_string()))?;
    if enabled {
        Ok(())
    } else {
        Err(CliError::runtime("probe_failed", "probe did not pass"))
    }
}

#[cfg(feature = "native")]
const fn openjev_probe_mode(mode: ProbeModeArg) -> openjev_llama_probe::ProbeMode {
    match mode {
        ProbeModeArg::Shared => openjev_llama_probe::ProbeMode::Shared,
        ProbeModeArg::Batch => openjev_llama_probe::ProbeMode::Batch,
    }
}

#[cfg(feature = "native")]
use systemone_openjev::openjev_llama as openjev_llama_probe;

/// Flatten the resolved `[backends.<id>]` table into `--set` overrides so a
/// child reproduces the parent's configuration without re-reading files.
#[cfg(feature = "native")]
fn child_overrides(resolved: &Resolved, id: &BackendId) -> Vec<(String, String)> {
    fn flatten(prefix: &str, table: &toml_table::Table, out: &mut Vec<(String, String)>) {
        for (key, value) in table {
            let path = format!("{prefix}.{key}");
            match value {
                toml_table::Value::Table(inner) => flatten(&path, inner, out),
                toml_table::Value::String(value) => out.push((path, value.clone())),
                toml_table::Value::Integer(value) => out.push((path, value.to_string())),
                toml_table::Value::Boolean(value) => out.push((path, value.to_string())),
                toml_table::Value::Float(value) => out.push((path, value.to_string())),
                _ => {}
            }
        }
    }
    let mut out = vec![("default_backend".to_owned(), id.to_string())];
    if let Some(toml_table::Value::Table(backends)) = resolved.merged.get("backends")
        && let Some(toml_table::Value::Table(backend)) = backends.get(id.as_str())
    {
        flatten(&format!("backends.{id}"), backend, &mut out);
    }
    out
}

#[cfg(feature = "native")]
use systemone_config::toml as toml_table;

#[cfg(not(feature = "native"))]
fn probe_command<W: Write, E: Write>(
    _resolved: &Resolved,
    _backend: Option<&str>,
    _model: Option<&str>,
    _mode: ProbeModeArg,
    _stdout: &mut W,
    _stderr: &mut E,
) -> Result<(), CliError> {
    let _ = probe::PROBE_REPORT_SCHEMA;
    Err(systemone_core::HostError::unavailable(
        "openjev probe requires a build with the native, metal, or cuda feature",
    )
    .into())
}
