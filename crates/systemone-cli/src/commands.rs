use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use systemone_config::Resolved;
use systemone_core::{CallContext, HostError};
use systemone_http::{Registry, RegistryEntry, ServeOptions, wire};

use crate::{
    CliError,
    args::{CallArgs, ConfigCommand, ModelsArgs, RunArgs},
    backends, output,
};

pub fn serve(resolved: &Resolved) -> Result<(), CliError> {
    let config = &resolved.config;
    let options = ServeOptions::new(
        config.server.host,
        config.server.port,
        config.server.request_timeout_secs,
        config.server.max_body_bytes,
        config.server.api_key_env.as_deref(),
    )?;
    let configured = backends::configure_all(config);
    let descriptions = configured
        .iter()
        .map(backends::Configured::describe)
        .collect();
    let mut entries = Vec::new();
    for instance in configured {
        if !instance.config.enabled {
            continue;
        }
        let backend = instance.backend?;
        entries.push(RegistryEntry {
            backend,
            queue_capacity: instance.config.queue_capacity,
            max_in_flight: instance.config.max_in_flight,
        });
    }
    let registry = Registry::load(
        entries,
        descriptions,
        config.default_backend.clone(),
        config.server.max_admitted_jobs,
    )?;
    systemone_http::run(registry, options)?;
    Ok(())
}

fn read_request<R: Read>(
    input: Option<&Path>,
    stdin: &mut R,
    stdin_is_terminal: bool,
) -> Result<Vec<u8>, CliError> {
    match input {
        Some(path) if path.as_os_str() != "-" => std::fs::read(path).map_err(|error| {
            CliError::validation(format!("cannot read {}: {error}", path.display()))
        }),
        _ => {
            if stdin_is_terminal && input.is_none() {
                return Err(CliError::validation(
                    "pass --input FILE or pipe a request JSON document on stdin",
                ));
            }
            let mut bytes = Vec::new();
            stdin
                .read_to_end(&mut bytes)
                .map_err(|error| CliError::runtime("stdin_io", error.to_string()))?;
            Ok(bytes)
        }
    }
}

fn wire_error(error: wire::WireError) -> CliError {
    CliError::validation(format!("{}: {}", error.error_type, error.message))
}

fn combine_selector(cli: Option<&str>, body: Option<&str>) -> Result<Option<String>, CliError> {
    match (cli, body) {
        (Some(cli), Some(body)) if cli != body => Err(CliError::validation(format!(
            "backend selector conflict: --backend {cli:?} vs request backend {body:?}"
        ))),
        (Some(selector), _) | (None, Some(selector)) => Ok(Some(selector.to_owned())),
        (None, None) => Ok(None),
    }
}

pub fn run<R: Read, W: Write, E: Write>(
    resolved: &Resolved,
    args: &RunArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let bytes = read_request(args.input.as_deref(), stdin, stdin_is_terminal)?;
    let parsed = wire::parse_request(&bytes).map_err(wire_error)?;
    let selector = combine_selector(args.backend.as_deref(), parsed.backend.as_deref())?;
    let instance = backends::configure_one(&resolved.config, selector.as_deref())?;
    if !instance.config.enabled {
        return Err(CliError::validation(format!(
            "backend {} is configured but not enabled",
            instance.id
        )));
    }
    let backend = instance.backend?;
    let mut host = backend.load()?;
    host.capabilities().check(&parsed.request)?;
    let deadline =
        Instant::now() + Duration::from_secs(resolved.config.server.request_timeout_secs);
    let context = CallContext::new("s1-run", Some(deadline));
    let result = host.evaluate(&parsed.request, &context);
    let shutdown = host.shutdown();
    let response = result?;
    shutdown?;
    let diagnostics = serde_json::json!({
        "backend": instance.id,
        "model": response.model,
        "diagnostics": response.diagnostics,
    });
    output::write_json(stderr, &diagnostics, false)
        .map_err(|error| CliError::runtime("stderr_io", error.to_string()))?;
    output::write_json(
        stdout,
        &wire::render_response(&response),
        resolved.config.output.pretty,
    )
    .map_err(|error| CliError::runtime("output_io", error.to_string()))
}

pub fn call<R: Read, W: Write, E: Write>(
    resolved: &Resolved,
    args: &CallArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let bytes = read_request(args.input.as_deref(), stdin, stdin_is_terminal)?;
    // Validate locally so malformed requests fail before a network call.
    let parsed = wire::parse_request(&bytes).map_err(wire_error)?;
    let selector = combine_selector(args.backend.as_deref(), parsed.backend.as_deref())?;
    let base = args.url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let url = format!("{base}/v1/systemone");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(args.timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| CliError::runtime("http_client", error.to_string()))?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .body(bytes);
    if let Some(selector) = &selector {
        request = request.header(systemone_http::server::BACKEND_HEADER, selector);
    }
    if let Some(name) = &args.api_key_env {
        let secret = std::env::var(name).map_err(|_| {
            CliError::validation(format!(
                "--api-key-env variable {name:?} is missing or not valid UTF-8"
            ))
        })?;
        request = request.bearer_auth(secret);
    }
    let response = request
        .send()
        .map_err(|error| CliError::runtime("http_request", error.to_string()))?;
    let status = response.status();
    let headers: serde_json::Map<String, serde_json::Value> = response
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("x-systemone-"))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), serde_json::Value::from(value)))
        })
        .collect();
    let body = response
        .bytes()
        .map_err(|error| CliError::runtime("http_body", error.to_string()))?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
        CliError::runtime(
            "http_body",
            format!("server returned non-JSON body (HTTP {status}): {error}"),
        )
    })?;
    output::write_json(
        stderr,
        &serde_json::json!({"status": status.as_u16(), "headers": headers}),
        false,
    )
    .map_err(|error| CliError::runtime("stderr_io", error.to_string()))?;
    if !status.is_success() {
        let error_type = body["error_type"].as_str().unwrap_or("http_error");
        let message = body["message"].as_str().unwrap_or("request failed");
        return Err(CliError::runtime(
            error_type,
            format!("HTTP {status}: {message}"),
        ));
    }
    output::write_json(stdout, &body, resolved.config.output.pretty)
        .map_err(|error| CliError::runtime("output_io", error.to_string()))
}

#[derive(Serialize)]
struct BackendsOutput {
    schema: &'static str,
    default_backend: Option<String>,
    build: &'static str,
    backends: Vec<backends::BackendListing>,
}

pub fn backends<W: Write>(resolved: &Resolved, stdout: &mut W) -> Result<(), CliError> {
    let configured = backends::configure_all(&resolved.config);
    let output = BackendsOutput {
        schema: "systemone-backends-v1",
        default_backend: resolved
            .config
            .default_backend
            .as_ref()
            .map(ToString::to_string),
        build: systemone_openjev::compiled_feature(),
        backends: configured.iter().map(backends::listing).collect(),
    };
    output::write_json(stdout, &output, resolved.config.output.pretty)
        .map_err(|error| CliError::runtime("output_io", error.to_string()))
}

#[derive(Serialize)]
struct ModelsOutput {
    schema: &'static str,
    backend: String,
    models: Vec<systemone_core::ModelStatus>,
}

pub fn models<W: Write>(
    resolved: &Resolved,
    args: &ModelsArgs,
    stdout: &mut W,
) -> Result<(), CliError> {
    let instance = backends::configure_one(&resolved.config, args.backend.as_deref())?;
    let backend = instance.backend?;
    let store = backend.model_store().require("model store")?;
    let output = ModelsOutput {
        schema: "systemone-models-v1",
        backend: instance.id.to_string(),
        models: store.list()?,
    };
    output::write_json(stdout, &output, resolved.config.output.pretty)
        .map_err(|error| CliError::runtime("output_io", error.to_string()))
}

pub fn config<W: Write>(
    resolved: &Resolved,
    command: &ConfigCommand,
    stdout: &mut W,
) -> Result<(), CliError> {
    match command {
        ConfigCommand::Check => {
            // Structure already validated; now validate every adapter's
            // typed settings without loading anything.
            let mut problems = Vec::new();
            for instance in backends::configure_all(&resolved.config) {
                if let Err(error) = &instance.backend {
                    match error {
                        HostError::Unsupported(_) if !instance.config.enabled => {}
                        _ => problems.push(format!("{}: {error}", instance.id)),
                    }
                }
            }
            if !problems.is_empty() {
                return Err(CliError::validation(problems.join("; ")));
            }
            let enabled: Vec<String> = resolved
                .config
                .enabled_backends()
                .map(|(id, _)| id.to_string())
                .collect();
            output::write_json(
                stdout,
                &serde_json::json!({
                    "schema": "systemone-config-check-v1",
                    "status": "ok",
                    "default_backend": resolved.config.default_backend,
                    "enabled_backends": enabled,
                }),
                resolved.config.output.pretty,
            )
        }
        ConfigCommand::Show => {
            let mut view = resolved.redacted();
            view["schema"] = serde_json::Value::from("systemone-config-show-v1");
            output::write_json(stdout, &view, resolved.config.output.pretty)
        }
    }
    .map_err(|error| CliError::runtime("output_io", error.to_string()))
}

/// Expose the shared backend pointer type for the openjev subcommand.
pub type SharedBackend = Arc<dyn systemone_core::Backend>;
