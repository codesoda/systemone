# Progress

## Planning bootstrap

- Created the separate SystemOne project without changing the upstream repos.
- Inspected OpenJev, Laya and GLiNER library boundaries and recorded source HEADs.
- Wrote the canonical cross-repo plan and planning-only README.
- Included five required backends: OpenJev, Laya, GLiNER2, Vercel AI Gateway and OpenRouter.
- Defined config ownership/precedence, backend selection, trait boundaries, HTTP compatibility, queues and cross-platform release gates.
- Recorded upstream Laya, Laya-MLX and Avi Chawla sources with measured-versus-proposed distinctions.
- Added illustrative JSON/TOML examples. No Rust implementation, model download, paid inference or benchmark execution is part of this bootstrap.
- Validation: parsed example files and embedded JSON/TOML with Python 3.12; checked local Markdown targets/heading anchors and `git diff --cached --check`. Independent read-only review found no internal contract contradictions; clarified startup errors and explicit accelerator behavior in the README. Rust build/test gates are not applicable until implementation exists.

## M0–M2: workspace, OpenJev adapter, HTTP service and `s1`

- Created the Cargo workspace: `systemone-core` (neutral `DecisionRequest`/`DecisionResponse`, `Capabilities`, `HostError`, the `DecisionHost` trait, `Backend` factory and `Extension<T>` coverage for optional traits such as `ModelStore`), `systemone-config` (Discuss-style layering ported from openjev's uncommitted work, `SYSTEMONE_*` mapping, provenance, redacted view), `systemone-openjev` (typed settings, Jev→Decision conversion moved from openjev's server, receipt-gated shared execution with explicit serial fallback, probe suite, model store), `systemone-http` (strict wire parser, per-backend owner-thread registry with bounded admission, Axum router with `x-systemone-*` headers) and `systemone-cli` (`s1`: serve/run/call/backends/models/config, plus `s1 openjev models|probe`).
- openjev-rs was stripped to a library-only repository at `8452ef0` (its CLI, server, demo, installer and release tooling removed) and is pinned by Git revision. The JS SDK compat fixture and HTTP schema were carried into `compat/`.
- Gates: `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings` (default and `--features metal`), 53 workspace tests. Manual evidence on Apple Silicon with the cached `qwen3-0.6b`: `s1 run` (28 s cold, one load), `s1 serve` ready in 13 s then `s1 call` and curl at ~230 ms per two-question request with honest `requested=shared; effective=serial` disclosure, `/v1/models`, `/v1/backends`, unknown-backend rejection, clean SIGTERM exit.
- Not done: Vercel/OpenRouter/Laya/GLiNER2 adapters (constructing an enabled instance is an explicit error), SDK smoke runs, eval/bench re-exposure, Windows verification, release artifacts.

## Confidence checks and CLI parity with the former `openjev` CLI

- The official TypeSafe JS SDK smoke (`compat/sdk-js/smoke.mjs`, `@typesafe-ai/sdk@0.6.0`) passes against `s1 serve` on Apple Silicon with the cached `qwen3-0.6b`. It caught a real regression: unknown models/backends returned 422 where Jev (and the SDK's error path) expect 404. `HostError::NotFound` now maps to 404 (`s1` exit 2).
- `s1 openjev probe --mode shared` runs end-to-end (after fixing the child env: the library gates unverified candidates on `OPENJEV_PROBE_CHILD`, which the parent now sets alongside `SYSTEMONE_OPENJEV_PROBE_CHILD`). The Metal/qwen3-0.6b profile fails the frozen gate (`max_abs_slot_logit=0.0347`) exactly as upstream recorded; the receipt is published as failed and the profile stays suspended, so serving continues to disclose `requested=shared; effective=serial`.
- Ported the remaining one-shot surface: `s1 decide|noul|score` build a neutral request from flags (`--state|--state-file|--state-json|--state-json-file` or piped text; `--option` text is the Jev label unless `--option-id` is given; `--level` is ordinal per Jev) and share `run`'s load/evaluate/render path. `s1 run --jsonl [--output]` evaluates one request per line through a single load, writes one row per input line (response or `{"line","error"}`), stops only on a terminal host error, prints a summary to stderr and exits 1 if any row failed. Verified live on Metal (5-row batch, ~11 s including load).
- Not ported by design: openjev's `--level-value` (not part of the Jev contract), `--compact`/full `Readout` output, `eval`, `bench`.

## Release pipeline

- Ported openjev-rs's reviewed release tooling to `s1`: `scripts/release.py` (deterministic tar.gz with `BUILD-INFO.json`, safe extraction, executable JSON smoke outside the checkout, dynamic-linkage check, `SHA256SUMS`), `scripts/check_native_build.py` (CMake cache gate: static, non-native CPU flags on Linux, embedded Metal library on macOS), `install.sh` (checksum-verified, no-root installer into `~/.systemone/bin`), and their 33 unit tests.
- Generated the `s1` third-party notice bundle with cargo-about 0.9.2 for both targets (284 packages, 151 license-text variants, including the git-pinned `openjev-core`/`openjev-llama`), the hash-pinned llama.cpp/ggml embedded-source notices, the Rust 1.95.0 library notice from the installed compiler, and the MPL-2.0 `colored`/`option-ext` crates.io archives checked against `Cargo.lock`. `scripts/check_third_party_licenses.py` fails CI if any of it drifts from the lockfile.
- `.github/workflows/ci-release.yml`: on every push/PR runs fmt, clippy and tests for the default and native feature sets on macOS-14 and Ubuntu 22.04, builds the release executable, checks the CMake configuration, smokes the binary without weights (including an explicit offline cache-miss error), packages and verifies the archive. On `v*` tags it additionally requires `docs/releases/<tag>.md`, cross-checks both archives against the tag commit, writes `SHA256SUMS` and creates the GitHub Release once (refusing to overwrite).
- Local dry run on Apple Silicon: `s1-v0.1.0-aarch64-apple-darwin.tar.gz` (6.9 MB) packaged and verified; the extracted binary answered a real `decide` from `/tmp` in 9 s with the cached qwen3-0.6b.

