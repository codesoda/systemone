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

