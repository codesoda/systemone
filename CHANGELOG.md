# Changelog

All notable changes to SystemOne are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Release notes for a
tag `vX.Y.Z` are taken from the matching `## [X.Y.Z]` section by the release
workflow.

## [Unreleased]

### Added

- `typesafe` backend kind (`systemone-remote` crate): direct hosted Jev
  passthrough to `https://api.typesafe.ai/v1/systemone` with a shared remote
  HTTP transport (bounded response bodies, disabled redirects, single send,
  no fallback), operator-configured API-key environment variable,
  selector stripping, verbatim answer/usage passthrough, sanitized error
  envelopes, and TypeSafe `/v1/models` catalogue validation. Includes
  credential-free mock tests and an opt-in, spend-acknowledged live smoke
  test (`TYPESAFE_API_KEY` + `SYSTEMONE_LIVE_SMOKE=spend-acknowledged`). The
  adapter forwards request state unchanged, floats included, because the
  hosted API accepts them and OpenJev's float rejection is an adapter
  limitation rather than a SystemOne rule; it passes through upstream
  FastAPI-style `{"detail": …}` errors with their status and a sanitized
  message, keeps a reported `error_type` even when the envelope message is
  not a string, and reports itself unavailable in `s1 backends` when the
  configured API-key environment variable is unset or empty. Availability is
  credential-scoped only; the API is never probed, because a health request
  would be billed.
- Strict Jev response parsing in `systemone-http::wire` for hosted backends.
  Unknown top-level and answer fields are ignored upstream extensions, which
  includes a `confidence` on a `noul` answer, because the neutral answer has
  no such field. Score `legend` and `probabilities` keys must be contiguous
  zero-based decimal indexes, so a sparse or one-based scale is refused
  instead of silently renumbered onto the positional vectors. An upstream
  `id` longer than 256 bytes is dropped instead of rejected, so an
  already-billed response body still reaches the caller.
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
- GLiNER2 backend (`systemone-gliner2`, `kind = "gliner2"`) over gliner2-rs
  pinned by Git revision, behind the `gliner2` feature (ONNX Runtime, CPU
  only). Loads only the classifier files of a GLiNER2.5 bundle, verifies them
  against the bundle manifest, and maps Choice/Noul/Score onto full softmax
  distributions with one encoder pass per question. Held-out results per
  checkpoint are in `docs/gliner2-evaluation.md`; Score is documented as a
  derived ordinal classification that did not work as a grader. One binary
  can link llama.cpp, MLX/Candle and ONNX Runtime together (verified on
  Apple Silicon).
  With `verify_sha256` on (the default), a bundle loads only when its
  manifest is a validated, release-ready v1 manifest (at most 8 MiB) for a
  model and revision that gliner2-rs pins. Noul labels that contain a
  reserved GLiNER2 prompt marker are rejected when the config loads.
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
- TypeSafe JS SDK compatibility smoke under `compat/sdk-js/`, with a
  `SYSTEMONE_SMOKE_BACKEND=typesafe` leg that runs against the hosted API and
  bills the account. The shared assertions stay strict for every local
  backend; four checks relax for `typesafe` alone (catalogue size, resolved
  model identity, choice label order, real output tokens).
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
