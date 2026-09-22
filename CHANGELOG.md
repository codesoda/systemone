# Changelog

All notable changes to SystemOne are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Release notes for a
tag `vX.Y.Z` are taken from the matching `## [X.Y.Z]` section by the release
workflow.

## [Unreleased]

### Added

- Rust workspace with `systemone-core`, `systemone-config`, `systemone-openjev`,
  `systemone-http` and `systemone-cli` (binary `s1`).
- Neutral `DecisionHost` trait (`capabilities`, `evaluate`, `shutdown`) that
  connects every command and API route to a backend, plus optional extension
  traits (`ModelStore`) that each backend reports as supported or not.
- OpenJev backend over `openjev-core`/`openjev-llama` pinned by Git revision,
  with receipt-gated shared execution and explicit serial fallback.
- Laya backend (`systemone-laya`, `kind = "laya"`) over `laya-core` pinned by
  Git revision, behind the `laya-cpu` (Candle), `laya-accelerate` (Candle + Apple BLAS) and
  `laya-metal` (MLX) features.
  One batched forward pass per request, SHA-256-verified profile directories,
  Metal warm-up at load, deterministic one-option Choice, empty-string default
  for missing instructions, and `x-systemone-truncation` disclosure. Verified
  end to end with the JS SDK smoke on CPU and Metal; numerical parity with the
  upstream Python runtime is gated in laya-rs, not here.
- Commands: `serve`, `run` (single request or `--jsonl` batch through one model
  load), `call`, `decide`, `noul`, `score`, `backends`, `models`,
  `config check|show`, `openjev models pull|path`, `openjev probe`.
- Jev-compatible HTTP API: `POST /v1/systemone`, `GET /v1/models`,
  `GET /v1/backends`, `/healthz`, `/readyz`; bearer auth off loopback, bounded
  admission, request deadlines, graceful shutdown, `x-systemone-*` headers.
- Layered configuration: built-ins < `~/.systemone/systemone.config.toml` <
  `./systemone.config.toml` < `SYSTEMONE_*` < CLI, with per-value provenance.
- Release pipeline: deterministic archives with `BUILD-INFO.json`, CMake
  configuration gate, dynamic-linkage check, `SHA256SUMS`, immutable GitHub
  Releases on `v*` tags, and `install.sh`.
- Third-party notice bundle generated with cargo-about for both targets, the
  Rust 1.95.0 library notice, and the MPL-2.0 `colored`/`option-ext` source
  archives checked against `Cargo.lock`.
- aislop quality gate (`failBelow: 95`).
- TypeSafe JS SDK compatibility smoke under `compat/sdk-js/`.
- README on the Best-README-Template layout with the staged VHS walkthrough
  (`demo/`, `docs/demo.md`), plus `CONTRIBUTING.md`, `SECURITY.md`,
  `SUPPORT.md` and `CODE_OF_CONDUCT.md`. Future work lives in GitHub issues;
  this changelog replaces `docs/PROGRESS.md` and per-release note files.

### Changed

- openjev-rs became a library-only repository at `8452ef0`; its CLI, HTTP
  server, demo, installer and release tooling moved here.

### Fixed

- Unknown model or backend returns HTTP 404 (`not_found`), which the official
  TypeSafe SDK expects. Found by the SDK smoke test.
- The probe child now sets the library's own `OPENJEV_PROBE_CHILD` guard so
  `s1 openjev probe` can run unverified candidates.

### Verified

- Apple Silicon (Metal) with `qwen3-0.6b`: `s1 run`, `s1 serve` + `s1 call`,
  `@typesafe-ai/sdk@0.6.0` smoke, `s1 openjev probe --mode shared` (fails the
  frozen gate as upstream did; execution stays `serial` and says so), and a
  packaged release binary answering from outside the checkout.
- Linux x86-64: compiled, tested without model weights, packaged and
  linkage-checked in CI only. Not yet run with real weights.
