# Repository instructions

These instructions apply to the entire repository and to coding agents working
in it. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; it holds the rules,
checks, and release steps. This file adds what an agent needs beyond that.

## Sources of truth

- The code and its contract tests define the backend contract, configuration
  layering, routing, and wire compatibility. Future work goes to GitHub
  issues; shipped work goes to `CHANGELOG.md`. Do not commit plan documents.
- SystemOne owns the common config, service, routing, and adapters. Shared
  inference runtimes may live in internal workspace crates; do not create a
  separate repository for a model that reuses an existing runtime or backbone.

## Hard lines

- No silent cloud, device, model, or execution-mode fallback.
- Result commands: JSON only on stdout; logs and errors on stderr.
- Do not describe planned adapters, platforms, or releases as shipped.
- Do not weaken an upstream parity gate to enable an optimization.
- Never commit credentials, model weights, private data, or fabricated results.

## Working artifacts

Plans, handoffs, scratch notes, and review reports are local working
artifacts. Do not commit them. `.aislop/` (except `config.yml`), `target*/`,
`dist/`, and `out/` are ignored.

## Before you finish

Run the checks in `CONTRIBUTING.md`. Build the `metal` or `native` feature once
if you changed anything under `crates/`. Update `CHANGELOG.md` under
`Unreleased`. Keep the README status line true.
