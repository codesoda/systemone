//! `s1` command implementation.
//!
//! stdout carries JSON results only; diagnostics and errors go to stderr as
//! JSON records. Exit code 2 means usage/validation, 1 means runtime failure.

pub mod args;
pub mod backends;
pub mod commands;
pub mod openjev;
pub mod output;

use std::{
    ffi::OsString,
    io::{Read, Write},
};

use clap::{CommandFactory, Parser, error::ErrorKind};
use serde::Serialize;
use systemone_config::{ConfigError, Override, ResolveOptions, Resolved};
use systemone_core::HostError;

use crate::args::{Cli, Command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorClass {
    Validation,
    Runtime,
}

#[derive(Clone, Debug)]
pub struct CliError {
    code: String,
    message: String,
    class: ErrorClass,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "usage".to_owned(),
            message: message.into(),
            class: ErrorClass::Validation,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            code: "validation".to_owned(),
            message: message.into(),
            class: ErrorClass::Validation,
        }
    }

    pub fn runtime(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            class: ErrorClass::Runtime,
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    const fn exit_code(&self) -> i32 {
        match self.class {
            ErrorClass::Validation => 2,
            ErrorClass::Runtime => 1,
        }
    }
}

impl From<HostError> for CliError {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Validation(message) => Self::validation(message),
            HostError::Unsupported(message) => Self {
                code: "unsupported".to_owned(),
                message,
                class: ErrorClass::Validation,
            },
            HostError::NotFound(message) => Self {
                code: "not_found".to_owned(),
                message,
                class: ErrorClass::Validation,
            },
            other => Self::runtime(other.code(), other.to_string()),
        }
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        Self {
            code: "configuration".to_owned(),
            message: error.0,
            class: ErrorClass::Validation,
        }
    }
}

#[derive(Serialize)]
struct ErrorRecord<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

/// Parsed arguments plus resolved configuration.
pub struct Invocation {
    pub cli: Cli,
    pub resolved: Resolved,
}

pub enum ParseOutcome {
    Ready(Box<Invocation>),
    Help(String),
    Version,
    Error(CliError),
}

/// Parse arguments and resolve configuration once. Help/version bypass all
/// configuration sources so they work even when config is broken.
pub fn parse(arguments: Vec<OsString>) -> ParseOutcome {
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return ParseOutcome::Help(error.to_string());
        }
        Err(error) if error.kind() == ErrorKind::DisplayVersion => return ParseOutcome::Version,
        Err(error) => return ParseOutcome::Error(CliError::usage(error.to_string())),
    };
    let mut overrides = Vec::with_capacity(cli.global.overrides.len() + 4);
    for text in &cli.global.overrides {
        match Override::parse(text) {
            Ok(override_) => overrides.push(override_),
            Err(error) => return ParseOutcome::Error(error.into()),
        }
    }
    if let Some(pretty) = cli.global.pretty {
        overrides.push(Override {
            path: "output.pretty".to_owned(),
            value: pretty.to_string(),
        });
    }
    if let Some(quiet) = cli.global.quiet {
        overrides.push(Override {
            path: "output.quiet".to_owned(),
            value: quiet.to_string(),
        });
    }
    if let Some(Command::Serve(serve)) = &cli.command {
        if let Some(host) = serve.host {
            overrides.push(Override {
                path: "server.host".to_owned(),
                value: host.to_string(),
            });
        }
        if let Some(port) = serve.port {
            overrides.push(Override {
                path: "server.port".to_owned(),
                value: port.to_string(),
            });
        }
        if let Some(default) = &serve.default_backend {
            overrides.push(Override {
                path: "default_backend".to_owned(),
                value: default.clone(),
            });
        }
    }
    let options = ResolveOptions {
        no_config: cli.global.no_config,
        overrides,
        ..Default::default()
    };
    match systemone_config::resolve(&options) {
        Ok(resolved) => ParseOutcome::Ready(Box::new(Invocation { cli, resolved })),
        Err(error) => ParseOutcome::Error(error.into()),
    }
}

pub fn run_with_io<I, T, R, W, E>(
    arguments: I,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
    R: Read,
    W: Write,
    E: Write,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    run_parsed_with_io(parse(arguments), stdin, stdin_is_terminal, stdout, stderr)
}

pub fn run_parsed_with_io<R: Read, W: Write, E: Write>(
    parsed: ParseOutcome,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> i32 {
    match parsed {
        ParseOutcome::Ready(invocation) => {
            match execute(*invocation, stdin, stdin_is_terminal, stdout, stderr) {
                Ok(code) => code,
                Err(error) => emit_error(stderr, &error),
            }
        }
        ParseOutcome::Help(text) => {
            let mut root = Cli::command();
            root.build();
            let output = output::HelpOutput {
                schema: "systemone-help-v1",
                command: "s1",
                usage: root.render_usage().to_string(),
                text,
            };
            i32::from(output::write_json(stdout, &output, false).is_err())
        }
        ParseOutcome::Version => {
            let output = output::VersionOutput {
                schema: "systemone-version-v1",
                version: env!("CARGO_PKG_VERSION"),
                build: build_identity(),
                openjev: systemone_openjev::compiled_feature(),
                laya: systemone_laya::compiled_feature(),
            };
            i32::from(output::write_json(stdout, &output, false).is_err())
        }
        ParseOutcome::Error(error) => emit_error(stderr, &error),
    }
}

fn execute<R: Read, W: Write, E: Write>(
    invocation: Invocation,
    stdin: &mut R,
    stdin_is_terminal: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32, CliError> {
    let Invocation { cli, resolved } = invocation;
    let command = cli
        .command
        .ok_or_else(|| CliError::usage("a command is required; see s1 --help"))?;
    match command {
        Command::Serve(_) => commands::serve(&resolved),
        Command::Run(args) => {
            commands::run(&resolved, &args, stdin, stdin_is_terminal, stdout, stderr)
        }
        Command::Call(args) => {
            commands::call(&resolved, &args, stdin, stdin_is_terminal, stdout, stderr)
        }
        Command::Decide(args) => {
            commands::decide(&resolved, &args, stdin, stdin_is_terminal, stdout, stderr)
        }
        Command::Noul(args) => {
            commands::noul(&resolved, &args, stdin, stdin_is_terminal, stdout, stderr)
        }
        Command::Score(args) => {
            commands::score(&resolved, &args, stdin, stdin_is_terminal, stdout, stderr)
        }
        Command::Backends => commands::backends(&resolved, stdout),
        Command::Models(args) => commands::models(&resolved, &args, stdout),
        Command::Config(args) => commands::config(&resolved, &args.command, stdout),
        Command::Openjev(args) => openjev::execute(&resolved, &args, stdout, stderr),
    }
    .map(|()| 0)
}

fn emit_error(writer: &mut impl Write, error: &CliError) -> i32 {
    let code = error.exit_code();
    let record = ErrorRecord {
        error: ErrorDetail {
            code: &error.code,
            message: &error.message,
        },
    };
    if output::write_json(writer, &record, false).is_err() {
        return if code == 0 { 1 } else { code };
    }
    code
}

const fn build_identity() -> &'static str {
    if cfg!(feature = "cuda") {
        "native-cuda"
    } else if cfg!(feature = "metal") {
        "native-metal"
    } else if cfg!(feature = "native") {
        "native-cpu"
    } else {
        "remote-only"
    }
}
