# SystemOne

**One CLI and Jev-compatible HTTP API for local and hosted decision models.**

Use runtime-defined **Choice**, **Noul** (probability of true), and **Score** questions with OpenJev, Laya, GLiNER2, or hosted Jev through **Vercel AI Gateway** and **OpenRouter**. Select a default backend in config, or override it for a request.

> **Status: design/planning only.** This repository currently contains the architecture and cross-repo implementation plan—not a working binary, server, installer, or release. Commands and configuration below describe the intended interface.

## Contents

- [Why a separate project?](#why-a-separate-project)
- [Backends](#backends)
- [Planned installation and usage](#planned-installation-and-usage)
- [Configuration](#configuration)
- [HTTP API](#http-api)
- [Architecture and cross-repo plan](#architecture-and-cross-repo-plan)
- [Platforms and performance](#platforms-and-performance)
- [Contributing](#contributing)
- [Acknowledgements and license](#acknowledgements-and-license)

## Why a separate project?

Applications should not have to change their integration every time they change decision models.

SystemOne provides the common interface and service layer. It depends on the inference libraries in [openjev-rs](https://github.com/codesoda/openjev-rs), [laya-rs](https://github.com/codesoda/laya-rs), and [gliner2-rs](https://github.com/codesoda/gliner2-rs)—it does not merge or rename those projects.

- **Choice:** choose among supplied options and return their probabilities.
- **Noul:** return the probability that a proposition is true.
- **Score:** return the expected zero-based level on an ordered rubric.

These are typed decisions, not generated prose. Matching an API schema does **not** mean different models have the same accuracy or calibrated confidence.

## Backends

All entries below are **planned SystemOne adapters**, not currently available integrations.

| Backend kind | Implementation | Integration status / constraint |
| --- | --- | --- |
| `openjev` | Frozen LLM next-token scoring through `openjev-core` / `openjev-llama` | Existing libraries; preserve prompt/token parity and explicit serial fallback |
| `laya` | Bidirectional encoder and trained decision heads through laya-rs | Rust runtime prerequisite; Python baseline exists |
| `gliner2` | Runtime-defined label classification through gliner2-rs | Needs full ordered probability distribution API and tested decision mappings |
| `vercel` | Hosted Jev through Vercel AI Gateway | Dedicated TypeSafe-compatible endpoint, not chat completions |
| `openrouter` | Hosted Jev through OpenRouter | Dedicated System One endpoint; model-list normalization required |

Enable only the instances you need. Disabled local backends do not load weights; disabled remote backends do not read credentials or send requests. Enabling an unavailable adapter or a model that cannot load must produce a clear startup error. There is **no silent local-to-cloud fallback**.

## Planned installation and usage

The release goal is downloadable binaries for **macOS, Linux, and Windows**, with separately documented accelerator support. No download or install command is available yet.

Planned commands:

```sh
# Load enabled local models once; serve until stopped.
systemone serve

# Separate terminal: call the running server without loading models again.
systemone call --url http://127.0.0.1:8080 --input request.json

# One-shot local/remote execution through the same config and adapters.
systemone run --input request.json
cat request.json | systemone run --backend cloud-vercel

# Inspect available backends and safely inspect configuration.
systemone backends
systemone models
systemone config check
systemone config show --redact
```

`serve` is a subcommand, not `--serve`. A one-shot `run` exits after its work; `call` reuses a resident server. Results go to stdout as JSON; logs go to stderr. `--pretty` formats JSON for people.

## Configuration

Planned precedence, matching Discuss CLI's layering:

**built-ins < user file < current-directory file < environment < CLI**

- macOS/Linux user file: `~/.systemone/systemone.config.toml`
- Windows user file: `%USERPROFILE%\.systemone\systemone.config.toml`
- Project file: `./systemone.config.toml` (current directory only; no ancestor search)
- `--no-config` bypasses both config files and `SYSTEMONE_*` defaults. Explicitly selected remote credentials are still resolved when needed.

SystemOne resolves configuration once and passes explicit typed settings into each adapter. It does **not** read the upstream CLIs' config files or shell out to those CLIs.

Illustrative configuration (also in [examples/systemone.config.toml](examples/systemone.config.toml)):

```toml
default_backend = "local"

[server]
host = "127.0.0.1"
port = 8080

[backends.local]
kind = "openjev"
enabled = true
model = "qwen3-0.6b"

[backends.local.settings]
device = "cpu"
threads = 4

[backends.cloud-vercel]
kind = "vercel"
enabled = false
model = "jev-latest"

[backends.cloud-vercel.settings]
api_key_env = "AI_GATEWAY_API_KEY"

[backends.cloud-openrouter]
kind = "openrouter"
enabled = false
model = "jev-latest"

[backends.cloud-openrouter.settings]
api_key_env = "OPENROUTER_API_KEY"
```

For example, `SYSTEMONE_DEFAULT_BACKEND=cloud-vercel` selects that instance once it is explicitly enabled. Backend settings, queue policy, environment mappings, validation and precedence are specified in the [cross-repo plan](docs/plans/cross-repo.md#4-configuration-contract).

## HTTP API

Planned routes:

| Route | Purpose |
| --- | --- |
| `POST /v1/systemone` | Jev-style typed decisions; optional `backend` extension |
| `GET /v1/models` | SDK-compatible model catalogue |
| `GET /v1/backends` | SystemOne extension: capabilities, readiness, limits |
| `GET /healthz` | Process liveness |
| `GET /readyz` | Required backend readiness |

Example `request.json`, also available [here](examples/request.json):

```json
{
  "model": "jev-latest",
  "state": "I was charged twice. Please refund the duplicate.",
  "questions": {
    "department": {
      "type": "choice",
      "instructions": "Which team should handle this?",
      "criteria": {
        "billing": "Payments and refunds",
        "technical": "Bugs and outages"
      }
    },
    "refund_requested": {
      "type": "noul",
      "instructions": "Does the customer explicitly request a refund?"
    }
  }
}
```

```sh
curl -sS http://127.0.0.1:8080/v1/systemone \
  -H 'Content-Type: application/json' \
  --data-binary @request.json
```

An ordinary Jev request uses the configured default backend. Add `"backend": "cloud-openrouter"` at the top level to select another **enabled** instance. This selector chooses the execution backend; `model` selects an allowed model within that backend. A disabled/unknown backend is an error, never an implicit fallback.

Responses retain Jev's `model`, `answers`, and `usage` fields. Routing evidence goes in `x-systemone-*` response headers by default, keeping JSON compact. The adapter strips SystemOne's `backend` field before sending requests upstream. An optional `X-SystemOne-Backend` request header supports SDKs that cannot serialize the extra field; conflicting selectors are rejected.

Vercel passthrough targets `https://ai-gateway.vercel.sh/typesafe/v1/systemone`; OpenRouter targets `https://openrouter.ai/api/v1/systemone`. Credentials stay on the server. Preserving provider fields, errors, usage and costs is part of the compatibility test gate—not a claim of compatibility already achieved.

## Architecture and cross-repo plan

```text
CLI / HTTP / SDK clients
         │
         ▼
SystemOne: config → validation → routing → bounded admission
         │
         ├── OpenJev adapter ── openjev-core + openjev-llama
         ├── Laya adapter ───── laya-rs inference library (to be built)
         ├── GLiNER2 adapter ── gliner2-rs classification library
         ├── Vercel adapter ── dedicated hosted Jev endpoint
         └── OpenRouter adapter ─ dedicated hosted Jev endpoint
```

**Start here: [Cross-repo implementation plan, v1](docs/plans/cross-repo.md).**

That document owns the shared contracts, work split, milestones, dependencies, verification gates and migration rules. Other repos should reference its stable URL or pin a commit for an implementation task instead of maintaining divergent copies.

Upstream libraries stay independently useful and do not depend on SystemOne. SystemOne implements its traits on adapter wrappers; native model code stays in the project that owns it. Existing standalone CLIs remain available unless separately deprecated.

## Platforms and performance

CPU operation on macOS, Linux and Windows is the portability baseline. Apple Silicon acceleration is a first-class target; CUDA and other execution providers are separately tested profiles, not assumed features of every binary. An explicitly requested accelerator requires a compatible build and device; it must fail clearly rather than silently switch to CPU.

[Laya-MLX](https://github.com/mizorewww/laya-mlx) is valuable implementation and benchmark evidence for Apple Silicon. Its published M3 Max measurements are **not** measurements of SystemOne or laya-rs, and an MLX Python port is not yet a Rust runtime.

Measure Python baselines first, then Rust library inference, then HTTP overhead and queued concurrency on the **same hardware, checkpoint, precision and workload**. Preserve numerical correctness before optimizing. See [research sources and optimization candidates](docs/research/sources.md).

## Contributing

Implementation starts with contract tests and the OpenJev adapter, followed by the HTTP service and hosted passthrough. GLiNER2 and Laya require their upstream library gates first. See the [milestone checklist](docs/plans/cross-repo.md#9-milestones-and-acceptance-gates).

Open an [issue](https://github.com/codesoda/systemone/issues) for proposed contract changes. Include affected repositories and compatibility tests. Do not commit model weights, credentials, private datasets or benchmark claims without reproducible evidence.

## Acknowledgements and license

Inspired by Jev/TypeSafe, SemIf/OpenJev, Convai Innovations' Laya, GLiNER2, and the independent Laya-MLX port. Also informed by [Avi Chawla's local decision-engine walkthrough](https://x.com/_avichawla/status/2101563610644496464?s=20).

SystemOne is independent and is not affiliated with or endorsed by TypeSafe, Vercel, OpenRouter, Convai Innovations, or other upstream authors. Project code and original documentation are [MIT licensed](LICENSE); dependencies, model weights and any reused upstream material retain their own licenses and notices.
