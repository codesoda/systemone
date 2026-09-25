# Changelog

All notable changes to SystemOne are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Release notes for a
tag `vX.Y.Z` are taken from the matching `## [X.Y.Z]` section by the release
workflow.

## [Unreleased]

### Added

- `s1 setup`: an interactive walkthrough that picks a backend kind (only
  kinds and devices the build can run), asks for its settings, shows the
  change, validates it like `s1 config check`, and writes the user or
  project config (comments kept, `.bak` backup). It then offers to download
  the model with a progress bar and to run one test decision. Downloads for
  OpenJev, Laya, Kev and GLiNER2 are pinned to immutable Hugging Face
  revisions and verified by size and SHA-256 before they are used; Kev's
  pickle-free heads come from
  [codesoda/kev-heads](https://huggingface.co/codesoda/kev-heads). Hosted
  kinds store only the API key's environment variable name. `--yes` accepts
  every default (and runs without a terminal); `--no-download` skips the
  download.
- `systemone-weights` crate: one verified downloader for pinned model files
  (stream to `.part`, hash while streaming, rename only after size and
  SHA-256 match; redirects limited to HTTPS on the Hugging Face Hub).

- `kev` backend kind (`systemone-kev` crate): Kev pointer-head decision
  models (jaredpalmer/kev) on Qwen bases through the kev-core runtime
  (kev-rs), behind `kev-cpu` (Candle, Qwen3-generation checkpoints such as
  kev-0.6b), `kev-accelerate` (macOS BLAS) and `kev-metal` (MLX, Apple
  Silicon, Qwen3.5 hybrid checkpoints kev-0.8b/kev-4b; includes the CPU
  backend) build features. The operator points `model_dir` at an assembled
  checkpoint (`base/`, `adapter/`, `head.safetensors`, `head.meta.json`);
  a device/checkpoint-generation mismatch is a load error, never a silent
  fallback. Serves as the configured `model` (default `kev-latest`) with a
  `jev-latest` alias; `usage.output_tokens` follows upstream kev (the token
  count of the serialised answers). kev-core carries its own frozen parity
  and benchmark gates against upstream goldens in
  [kev-rs](https://github.com/codesoda/kev-rs), pinned at the v0.1.1
  release tag commit; `kev` is not in binary releases yet. The TypeSafe
  SDK smoke gains a `kev` expectation row
  (`SYSTEMONE_SMOKE_BACKEND=kev`, output tokens counted, float state
  accepted).

- `typesafe` backend kind (`systemone-remote` crate): direct hosted Jev
  passthrough to `https://api.typesafe.ai/v1/systemone` with a shared remote
  HTTP transport (bounded response bodies, disabled redirects, single send,
  no fallback), operator-configured API-key environment variable,
  selector stripping, verbatim answer/usage/model passthrough, sanitized error
  envelopes, and strict parsing of the TypeSafe `/v1/models` catalogue in the
  library API that the live smoke uses; the served catalogue is unchanged, as
  `GET /v1/models` serves the one card of the configured instance model, as it
  does for every other kind. Includes
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
  would be billed. A missing credential fails `load` as unavailable, the way
  a missing build feature or model directory does for the local kinds, and an
  environment value that is not valid UTF-8 reports as set-but-unusable
  instead of unset. Upstream distributions are checked at the precision they
  arrive in (two decimals per entry), so a correct body that sums to 0.99 is
  accepted, not refused after it was billed; SystemOne still never
  renormalizes. `s1 backends` and `s1 --version` report the kind in their
  `build` map, because a hosted adapter is always linked. Hosted answers are
  returned in the key order the request declared: the `answers` object follows
  the request's question order and each Choice `probabilities` map follows the
  order its options were declared in, so a hosted answer reads like a local
  one. That moves keys only; no probability, label, answer value, usage
  counter or model identity is changed. Alignment is not repair: an upstream
  body that misses, adds, renames or retypes an answer or a Choice label is an
  invalid body and fails the request. A score answer keeps its positional
  scale: one that covers more or fewer levels than the request declared is
  refused as well, instead of reaching the caller with the upstream's rubric.
  `model` defaults to the upstream alias `jev-latest` when the instance sets
  none, so pin a concrete version for a stable identity.
- `examples/systemone.config.toml` and the README Configuration section show
  a `typesafe` instance pinned to a concrete model version
  (`model = "jev-1.13.0"`, `aliases = ["jev-latest"]`). The upstream catalogue
  lists aliases only and never the version behind them, so pinning the version
  is what gives the instance a stable identity: SystemOne resolves the
  requested alias to the configured model before the call, and the answer names
  the same model as the single `/v1/models` card. Configuring an upstream alias
  as the instance `model` stays allowed and means the answer carries whichever
  concrete identity the API picked, because identity is passed through
  verbatim.
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
  bills the account. Every backend runs the same assertions. The only
  per-backend part is one `expectations` table, which records the two genuine
  differences: whether the backend generates nothing and reports zero output
  tokens (OpenJev, Laya, GLiNER2) or generates and reports a positive count
  (TypeSafe), and whether float JSON state values are rejected with a 422
  (OpenJev) or accepted (Laya, GLiNER2, TypeSafe). One model card, `model`
  equal to that card's name, the declared key order of every distribution, the
  score legend, the numeric bounds, positive input tokens and the
  unknown-model 404 are shared and unconditional.
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
