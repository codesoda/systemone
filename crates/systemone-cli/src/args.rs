use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const ROOT_AFTER_HELP: &str = r"Examples:
  s1 serve
  s1 run --input request.json
  cat request.json | s1 run --backend cloud-vercel
  s1 call --url http://127.0.0.1:8080 --input request.json
  s1 backends
  s1 models --backend local
  s1 config show
  s1 openjev models pull qwen3-0.6b

Configuration precedence: built-ins < ~/.systemone/systemone.config.toml <
./systemone.config.toml < SYSTEMONE_* < CLI (--set, --host, ...).
--no-config ignores both files and SYSTEMONE_* defaults.
stdout is JSON only; logs and errors go to stderr.";

#[derive(Clone, Debug, Parser)]
#[command(
    name = "s1",
    version,
    about = "One CLI and Jev-compatible HTTP API for local and hosted decision models",
    after_help = ROOT_AFTER_HELP
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Debug, Default, Args)]
pub struct GlobalArgs {
    /// Ignore configuration files and SYSTEMONE_* defaults.
    #[arg(long, global = true)]
    pub no_config: bool,
    /// Override one configuration leaf, e.g. --set backends.local.settings.threads=4
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
    /// Pretty-print JSON results.
    #[arg(long, global = true, action = clap::ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub pretty: Option<bool>,
    /// Reduce logging to warnings.
    #[arg(long, global = true, action = clap::ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub quiet: Option<bool>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Load enabled backends once and serve the Jev-compatible HTTP API.
    Serve(ServeArgs),
    /// Evaluate one request file in-process through the configured adapters.
    Run(RunArgs),
    /// Send one request file to a running s1 server.
    Call(CallArgs),
    /// List configured backends and their extension coverage (loads nothing).
    Backends,
    /// List models known to a backend's model store.
    Models(ModelsArgs),
    /// Inspect resolved configuration.
    Config(ConfigArgs),
    /// OpenJev-specific tooling.
    Openjev(OpenJevArgs),
}

#[derive(Clone, Debug, Args)]
#[command(
    after_help = "Examples:\n  s1 serve\n  s1 serve --host 0.0.0.0 --set server.api_key_env=S1_API_KEY"
)]
pub struct ServeArgs {
    #[arg(long)]
    pub host: Option<std::net::IpAddr>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub default_backend: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(after_help = "Example:\n  s1 run --input request.json --backend local")]
pub struct RunArgs {
    /// Request JSON file, or - for stdin (default when stdin is not a TTY).
    #[arg(long)]
    pub input: Option<PathBuf>,
    /// Backend instance to use; must agree with any selector in the file.
    #[arg(long)]
    pub backend: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(after_help = "Example:\n  s1 call --url http://127.0.0.1:8080 --input request.json")]
pub struct CallArgs {
    /// Server origin, optionally ending in /v1.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    pub url: String,
    #[arg(long)]
    pub input: Option<PathBuf>,
    #[arg(long)]
    pub backend: Option<String>,
    /// Environment variable containing the server's bearer secret.
    #[arg(long)]
    pub api_key_env: Option<String>,
    #[arg(long, default_value_t = 130, value_parser = clap::value_parser!(u64).range(1..=600))]
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, Args)]
pub struct ModelsArgs {
    #[arg(long)]
    pub backend: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ConfigCommand {
    /// Resolve and validate configuration, including adapter settings.
    Check,
    /// Print resolved non-secret configuration with provenance.
    Show,
}

#[derive(Clone, Debug, Args)]
pub struct OpenJevArgs {
    /// Backend instance of kind openjev (default: the default backend).
    #[arg(long, global = true)]
    pub backend: Option<String>,
    #[command(subcommand)]
    pub command: OpenJevCommand,
}

#[derive(Clone, Debug, Subcommand)]
pub enum OpenJevCommand {
    /// Verified model cache operations.
    Models(OpenJevModelsArgs),
    /// Run the shared/batch execution probe and publish its receipt.
    Probe {
        /// Registered model ID (default: the backend's configured model).
        id: Option<String>,
        #[arg(long, value_enum)]
        mode: ProbeModeArg,
    },
}

#[derive(Clone, Debug, Args)]
pub struct OpenJevModelsArgs {
    #[command(subcommand)]
    pub command: OpenJevModelsCommand,
}

#[derive(Clone, Debug, Subcommand)]
pub enum OpenJevModelsCommand {
    /// Download and verify a registered model.
    Pull {
        id: String,
        #[arg(long)]
        repair: bool,
    },
    /// Locate an already cached, verified model.
    Path { id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProbeModeArg {
    Shared,
    Batch,
}
