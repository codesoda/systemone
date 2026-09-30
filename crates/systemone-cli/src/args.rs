use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const ROOT_AFTER_HELP: &str = r#"Examples:
  s1 setup
  s1 serve
  s1 run --input request.json
  s1 run --jsonl --input requests.jsonl --output answers.jsonl
  cat request.json | s1 run --backend cloud-vercel
  s1 decide --state 'Charged twice.' --question 'Which team?' --option Billing --option Support
  printf 'Refund requested.' | s1 noul --question 'Does the customer want a refund?'
  s1 score --state-json '{"severity":3}' --question 'How urgent?' --level low --level medium --level high
  s1 call --url http://127.0.0.1:8080 --input request.json
  s1 backends
  s1 models --backend local
  s1 config show
  s1 openjev models pull qwen3-0.6b

Configuration precedence: built-ins < ~/.systemone/systemone.config.toml <
./systemone.config.toml < SYSTEMONE_* < CLI (--set, --host, ...).
--no-config ignores both files and SYSTEMONE_* defaults.
stdout is JSON only; logs and errors go to stderr."#;

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
    /// Walk through choosing, configuring and downloading a backend.
    Setup(SetupArgs),
    /// Load enabled backends once and serve the Jev-compatible HTTP API.
    Serve(ServeArgs),
    /// Evaluate one request file (or JSONL of requests) in-process.
    Run(RunArgs),
    /// One-shot Choice question(s) built from flags.
    Decide(DecideArgs),
    /// One-shot Noul (yes/no probability) question built from flags.
    Noul(NoulArgs),
    /// One-shot Score (ordinal rubric) question built from flags.
    Score(ScoreArgs),
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
    /// Treat input as JSON Lines: one request per line, one model load,
    /// one output line per request (a response or a `{"line","error"}` row).
    #[arg(long)]
    pub jsonl: bool,
    /// Write results here instead of stdout (JSONL mode only).
    #[arg(long, requires = "jsonl")]
    pub output: Option<PathBuf>,
}

/// Decision state from exactly one source; text on piped stdin otherwise.
#[derive(Clone, Debug, Default, Args)]
pub struct StateArgs {
    /// State as plain text.
    #[arg(long, conflicts_with_all = ["state_file", "state_json", "state_json_file"])]
    pub state: Option<String>,
    /// State as plain text read from a file.
    #[arg(long, conflicts_with_all = ["state", "state_json", "state_json_file"])]
    pub state_file: Option<PathBuf>,
    /// State as a JSON value (strict: no duplicate keys).
    #[arg(long, conflicts_with_all = ["state", "state_file", "state_json_file"])]
    pub state_json: Option<String>,
    /// State as a JSON value read from a file.
    #[arg(long, conflicts_with_all = ["state", "state_file", "state_json"])]
    pub state_json_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
#[command(
    after_help = "Examples:\n  s1 decide --state 'Charged twice.' --question 'Which team?' --option Billing --option Support\n  s1 decide --state-json '{\"severity\":3}' --question 'Escalate?' --option-id yes --option 'Escalate now' --option-id no --option 'Handle normally'"
)]
pub struct DecideArgs {
    /// Question text; repeat to ask several questions over the same state and options.
    #[arg(long, required = true)]
    pub question: Vec<String>,
    /// Option text; repeat for each option (1–16). Used as the label unless --option-id is given.
    #[arg(long, required = true)]
    pub option: Vec<String>,
    /// Option labels, aligned with --option; then --option text becomes the description.
    #[arg(long)]
    pub option_id: Vec<String>,
    /// Question ID (default: q-1; with several questions: ID/1, ID/2, ...).
    #[arg(long)]
    pub id: Option<String>,
    #[command(flatten)]
    pub state: StateArgs,
    #[arg(long)]
    pub backend: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(
    after_help = "Example:\n  printf 'Refund requested.' | s1 noul --question 'Does the customer want a refund?'"
)]
pub struct NoulArgs {
    /// Proposition to evaluate.
    #[arg(long)]
    pub question: String,
    /// Optional description of the true outcome.
    #[arg(long = "true", value_name = "TEXT")]
    pub true_description: Option<String>,
    /// Optional description of the false outcome.
    #[arg(long = "false", value_name = "TEXT")]
    pub false_description: Option<String>,
    /// Question ID (default: q-1).
    #[arg(long)]
    pub id: Option<String>,
    #[command(flatten)]
    pub state: StateArgs,
    #[arg(long)]
    pub backend: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(
    after_help = "Example:\n  s1 score --state-json '{\"severity\":3}' --question 'How urgent?' --level low --level medium --level high\n\nLevels are ordinal: the first is 0, the last is n-1, as in the Jev API."
)]
pub struct ScoreArgs {
    /// Scoring instructions.
    #[arg(long)]
    pub question: String,
    /// Ordered level descriptions (2–16); the index is the score value.
    #[arg(long, required = true)]
    pub level: Vec<String>,
    /// Question ID (default: q-1).
    #[arg(long)]
    pub id: Option<String>,
    #[command(flatten)]
    pub state: StateArgs,
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

#[derive(Clone, Debug, Default, Args)]
pub struct SetupArgs {
    /// Write the user file (~/.systemone/systemone.config.toml).
    #[arg(long, conflicts_with = "project")]
    pub user: bool,
    /// Write the project file (./systemone.config.toml).
    #[arg(long)]
    pub project: bool,
    /// Backend instance name to add or edit.
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
    /// Backend kind (openjev, laya, kev, gliner2, typesafe).
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
    /// Accept every default: write, download, and skip the test decision.
    #[arg(long)]
    pub yes: bool,
    /// Do not download model files.
    #[arg(long)]
    pub no_download: bool,
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
