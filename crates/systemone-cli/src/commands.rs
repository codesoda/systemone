use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::Value;
use systemone_config::Resolved;
use systemone_core::{
    CallContext, ChoiceQuestion, DecisionHost, DecisionRequest, DecisionResponse, HostError,
    NoulQuestion, Question, ScoreQuestion,
};
use systemone_http::{Registry, RegistryEntry, ServeOptions, wire};

use crate::{
    CliError,
    args::{
        CallArgs, ConfigCommand, DecideArgs, ModelsArgs, NoulArgs, RunArgs, ScoreArgs, StateArgs,
    },
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

/// A loaded host plus the identity needed for diagnostics.
struct Loaded {
    id: String,
    host: Box<dyn DecisionHost>,
    timeout: Duration,
}

impl Loaded {
    /// Resolve and load the selected backend once. Disabled backends are a
    /// validation error, never a silent skip.
    fn load(resolved: &Resolved, selector: Option<&str>) -> Result<Self, CliError> {
        let instance = backends::configure_one(&resolved.config, selector)?;
        if !instance.config.enabled {
            return Err(CliError::validation(format!(
                "backend {} is configured but not enabled",
                instance.id
            )));
        }
        let backend = instance.backend?;
        Self::from_backend(
            backend.as_ref(),
            Duration::from_secs(resolved.config.server.request_timeout_secs),
        )
    }

    fn from_backend(
        backend: &dyn systemone_core::Backend,
        timeout: Duration,
    ) -> Result<Self, CliError> {
        let host = backend.load()?;
        Ok(Self {
            id: backend.id().to_string(),
            host,
            timeout,
        })
    }

    fn evaluate(
        &mut self,
        request: &DecisionRequest,
        request_id: &str,
    ) -> Result<DecisionResponse, HostError> {
        self.host.capabilities().check(request)?;
        let context = CallContext::new(request_id, Some(Instant::now() + self.timeout));
        self.host.evaluate(request, &context)
    }

    fn shutdown(mut self) -> Result<(), CliError> {
        self.host.shutdown()?;
        Ok(())
    }
}

fn diagnostics(id: &str, response: &DecisionResponse) -> Value {
    serde_json::json!({
        "backend": id,
        "model": response.model,
        "diagnostics": response.diagnostics,
    })
}

/// Load once, answer one request, emit diagnostics on stderr and the wire
/// response on stdout. Shared by `run`, `decide`, `noul` and `score`.
fn evaluate_once<W: Write, E: Write>(
    resolved: &Resolved,
    selector: Option<&str>,
    request: &DecisionRequest,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let loaded = Loaded::load(resolved, selector)?;
    evaluate_loaded(
        loaded,
        request,
        resolved.config.output.pretty,
        stdout,
        stderr,
    )
}

fn evaluate_loaded<W: Write, E: Write>(
    mut loaded: Loaded,
    request: &DecisionRequest,
    pretty: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let result = loaded.evaluate(request, "s1-run");
    let id = loaded.id.clone();
    let shutdown = loaded.shutdown();
    let response = result?;
    shutdown?;
    output::write_json(stderr, &diagnostics(&id, &response), false)
        .map_err(|error| CliError::runtime("stderr_io", error.to_string()))?;
    output::write_json(stdout, &wire::render_response(&response), pretty)
        .map_err(|error| CliError::runtime("output_io", error.to_string()))
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
    if args.jsonl {
        return run_jsonl(resolved, args, &bytes, stdout, stderr);
    }
    let parsed = wire::parse_request(&bytes).map_err(wire_error)?;
    let selector = combine_selector(args.backend.as_deref(), parsed.backend.as_deref())?;
    evaluate_once(
        resolved,
        selector.as_deref(),
        &parsed.request,
        stdout,
        stderr,
    )
}

/// Where JSONL rows go.
enum JsonlSink<'a, W: Write> {
    Stdout(&'a mut W),
    File(std::io::BufWriter<std::fs::File>),
}

impl<W: Write> Write for JsonlSink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdout(writer) => writer.write(buf),
            Self::File(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdout(writer) => writer.flush(),
            Self::File(writer) => writer.flush(),
        }
    }
}

#[derive(Serialize)]
struct JsonlErrorRow<'a> {
    line: usize,
    error: JsonlError<'a>,
}

#[derive(Serialize)]
struct JsonlError<'a> {
    error_type: &'a str,
    message: String,
}

fn jsonl_error(error_type: &str, message: impl Into<String>) -> (String, String) {
    (error_type.to_owned(), message.into())
}

fn run_jsonl<W: Write, E: Write>(
    resolved: &Resolved,
    args: &RunArgs,
    bytes: &[u8],
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CliError::validation("JSONL input must be valid UTF-8"))?;
    let sink = match &args.output {
        Some(path) => JsonlSink::File(std::io::BufWriter::new(
            std::fs::File::create(path).map_err(|error| {
                CliError::validation(format!("cannot create {}: {error}", path.display()))
            })?,
        )),
        None => JsonlSink::Stdout(stdout),
    };
    let loaded = Loaded::load(resolved, args.backend.as_deref())?;
    run_jsonl_loaded(loaded, text, sink, stderr)
}

/// One request per line through a single loaded host. Every non-blank line
/// yields exactly one output line in input order: the wire response, or a
/// row carrying the 1-based line number and error. Row failures never stop
/// the batch (a terminal host error does); the process exits 1 if any row
/// failed.
fn run_jsonl_loaded<S: Write, E: Write>(
    mut loaded: Loaded,
    text: &str,
    mut sink: S,
    stderr: &mut E,
) -> Result<(), CliError> {
    let (mut rows, mut succeeded, mut failed) = (0usize, 0usize, 0usize);
    let mut fatal: Option<CliError> = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows += 1;
        let line_number = index + 1;
        let outcome: Result<DecisionResponse, (String, String)> =
            wire::parse_request(line.as_bytes())
                .map_err(|error| jsonl_error(error.error_type, error.message))
                .and_then(|parsed| match parsed.backend.as_deref() {
                    None => Ok(parsed.request),
                    Some(body) if body == loaded.id => Ok(parsed.request),
                    Some(body) => Err(jsonl_error(
                        "validation_error",
                        format!(
                            "row selects backend {body:?} but this batch is bound to {:?}",
                            loaded.id
                        ),
                    )),
                })
                .and_then(|request| {
                    loaded
                        .evaluate(&request, &format!("s1-run-{line_number}"))
                        .map_err(|error| {
                            if error.is_terminal() {
                                fatal = Some(CliError::from(error.clone()));
                            }
                            jsonl_error(error.code(), error.to_string())
                        })
                });
        let written = match outcome {
            Ok(response) => {
                succeeded += 1;
                output::write_json(&mut sink, &wire::render_response(&response), false)
            }
            Err((error_type, message)) => {
                failed += 1;
                output::write_json(
                    &mut sink,
                    &JsonlErrorRow {
                        line: line_number,
                        error: JsonlError {
                            error_type: &error_type,
                            message,
                        },
                    },
                    false,
                )
            }
        };
        written.map_err(|error| CliError::runtime("output_io", error.to_string()))?;
        if fatal.is_some() {
            break;
        }
    }
    sink.flush()
        .map_err(|error| CliError::runtime("output_io", error.to_string()))?;
    drop(sink);
    let id = loaded.id.clone();
    let shutdown = loaded.shutdown();
    output::write_json(
        stderr,
        &serde_json::json!({
            "schema": "systemone-run-jsonl-summary-v1",
            "backend": id,
            "rows": rows,
            "succeeded": succeeded,
            "failed": failed,
        }),
        false,
    )
    .map_err(|error| CliError::runtime("stderr_io", error.to_string()))?;
    if let Some(error) = fatal {
        return Err(error);
    }
    shutdown?;
    if failed > 0 {
        return Err(CliError::runtime(
            "rows_failed",
            format!("{failed} of {rows} rows failed; see error rows in the output"),
        ));
    }
    Ok(())
}

fn read_utf8(path: &Path, what: &str) -> Result<String, CliError> {
    let bytes = std::fs::read(path).map_err(|error| {
        CliError::validation(format!("cannot read {what} {}: {error}", path.display()))
    })?;
    String::from_utf8(bytes)
        .map_err(|_| CliError::validation(format!("{what} {} is not valid UTF-8", path.display())))
}

/// Resolve the decision state from flags or piped stdin text.
fn read_state<R: Read>(
    args: &StateArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
) -> Result<Value, CliError> {
    let json = |text: &str, what: &str| {
        wire::parse_strict(text)
            .map_err(|error| CliError::validation(format!("{what}: {}", error.message)))
    };
    if let Some(text) = &args.state {
        return Ok(Value::String(text.clone()));
    }
    if let Some(path) = &args.state_file {
        return Ok(Value::String(read_utf8(path, "--state-file")?));
    }
    if let Some(text) = &args.state_json {
        return json(text, "--state-json");
    }
    if let Some(path) = &args.state_json_file {
        return json(&read_utf8(path, "--state-json-file")?, "--state-json-file");
    }
    let missing = || {
        CliError::validation(
            "state is required: use --state, --state-file, --state-json, --state-json-file, or pipe UTF-8 text on stdin",
        )
    };
    if stdin_is_terminal {
        return Err(missing());
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text).map_err(|error| {
        CliError::validation(format!("stdin state is not valid UTF-8: {error}"))
    })?;
    if text.trim().is_empty() {
        return Err(missing());
    }
    Ok(Value::String(text))
}

fn question_id(base: Option<&str>, index: usize, count: usize) -> String {
    match (base, count) {
        (Some(base), 1) => base.to_owned(),
        (Some(base), _) => format!("{base}/{}", index + 1),
        (None, _) => format!("q-{}", index + 1),
    }
}

fn build_request(
    state: Value,
    questions: Vec<(String, Question)>,
) -> Result<DecisionRequest, CliError> {
    DecisionRequest::new(None, state, questions).map_err(CliError::from)
}

fn decide_request(args: &DecideArgs, state: Value) -> Result<DecisionRequest, CliError> {
    let criteria: Vec<(String, Value)> = if args.option_id.is_empty() {
        args.option
            .iter()
            .map(|label| (label.clone(), Value::Null))
            .collect()
    } else {
        if args.option_id.len() != args.option.len() {
            return Err(CliError::validation(format!(
                "--option-id count {} does not match --option count {}",
                args.option_id.len(),
                args.option.len()
            )));
        }
        args.option_id
            .iter()
            .cloned()
            .zip(args.option.iter().map(|text| Value::String(text.clone())))
            .collect()
    };
    let count = args.question.len();
    let questions = args
        .question
        .iter()
        .enumerate()
        .map(|(index, text)| {
            (
                question_id(args.id.as_deref(), index, count),
                Question::Choice(ChoiceQuestion {
                    instructions: Some(Value::String(text.clone())),
                    criteria: criteria.clone(),
                }),
            )
        })
        .collect();
    build_request(state, questions)
}

pub fn decide<R: Read, W: Write, E: Write>(
    resolved: &Resolved,
    args: &DecideArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let state = read_state(&args.state, stdin, stdin_is_terminal)?;
    let request = decide_request(args, state)?;
    evaluate_once(resolved, args.backend.as_deref(), &request, stdout, stderr)
}

fn noul_request(args: &NoulArgs, state: Value) -> Result<DecisionRequest, CliError> {
    build_request(
        state,
        vec![(
            question_id(args.id.as_deref(), 0, 1),
            Question::Noul(NoulQuestion {
                instructions: Some(Value::String(args.question.clone())),
                true_description: args.true_description.clone().map(Value::String),
                false_description: args.false_description.clone().map(Value::String),
            }),
        )],
    )
}

pub fn noul<R: Read, W: Write, E: Write>(
    resolved: &Resolved,
    args: &NoulArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let state = read_state(&args.state, stdin, stdin_is_terminal)?;
    let request = noul_request(args, state)?;
    evaluate_once(resolved, args.backend.as_deref(), &request, stdout, stderr)
}

fn score_request(args: &ScoreArgs, state: Value) -> Result<DecisionRequest, CliError> {
    build_request(
        state,
        vec![(
            question_id(args.id.as_deref(), 0, 1),
            Question::Score(ScoreQuestion {
                instructions: Some(Value::String(args.question.clone())),
                levels: args
                    .level
                    .iter()
                    .map(|text| Value::String(text.clone()))
                    .collect(),
            }),
        )],
    )
}

pub fn score<R: Read, W: Write, E: Write>(
    resolved: &Resolved,
    args: &ScoreArgs,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<(), CliError> {
    let state = read_state(&args.state, stdin, stdin_is_terminal)?;
    let request = score_request(args, state)?;
    evaluate_once(resolved, args.backend.as_deref(), &request, stdout, stderr)
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
    tracing::info!(
        backend = %instance.id,
        "listing models; cached artifacts are re-verified by SHA-256, which takes a few seconds per gigabyte"
    );
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

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
