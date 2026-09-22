# SystemOne cross-repo implementation plan

**Contract version: 1 — M0–M2 implemented for OpenJev; remaining milestones tracked as GitHub issues.**

Canonical URL: <https://github.com/codesoda/systemone/blob/main/docs/plans/cross-repo.md>

Applies to codesoda/systemone, codesoda/openjev-rs, codesoda/laya-rs and codesoda/gliner2-rs. Link this document from upstream plans; pin its Git commit in implementation tasks. Changes to shared semantics require a SystemOne plan update and affected adapter/contract tests. Do not fork this plan into competing copies.

## 1. Outcome and boundaries

Build a standalone Rust `systemone` executable and embeddable library that provide one typed-decision interface over local inference libraries and hosted Jev. SystemOne owns configuration, API compatibility, backend selection, service queues, authentication, timeouts, telemetry and packaging. Underlying projects own tokenization, model loading, inference, native device integration and model-specific correctness.

Required backends: OpenJev, Laya, GLiNER2, Vercel AI Gateway and OpenRouter. A backend instance is a named configuration entry, not a vendor name or an automatic router. Multiple instances of one vendor may use different models/devices/settings.

Deliver a convenient resident HTTP service and one-shot CLI on macOS, Linux and Windows. Local model weights load once per enabled instance at startup, not once per HTTP request. Enabled does not mean universally supported: unsupported build/device/capability combinations must fail explicitly.

Not in v1: training, automatic semantic backend selection, speculative execution, automatic cloud failover, persistent job queues, arbitrary request-supplied upstream URLs, arbitrary request-triggered model downloads, rewriting upstream runtimes, or promising every backend has identical quality. Do not rename or merge existing repos.

This task initially delivers documentation only. No commands, adapters, server or binaries are available until the corresponding implementation gates pass.

## 2. Evidence and current baseline

Inspected source snapshots (these are observations, not a dependency lockfile):

| Repository | Inspected HEAD | Actual boundary |
| --- | --- | --- |
| openjev-rs | `8452ef0e5890497deb2cec95f16dc8d94d0c0c02` (pinned) | Library-only: `openjev-core` and `openjev-llama`. The former CLI/HTTP server was removed in this revision and reimplemented here |
| laya-rs | `23fff422666fd5039998fd8a55ca57f7e40d224b` (pinned) | `crates/laya-core`: Rust runtime with MLX (Metal) and Candle (CPU) backends, parity-gated against the frozen Python goldens; Python baseline/assets/goldens retained for that gate |
| gliner2-rs | `1492d6b6d9b688f11751094eb6c479e5fbb3883e` (pinned) | Rust `gliner2-rs` package, imported as `gliner2_rs`; GLiNER2.5 boundary runtime on direct ORT 1.28 with `ClassificationPipeline`, `score_classification*` (complete ordered distributions) and `RuntimeOptions` (CPU only) |

The GLiNER checkout was locally named `gliners2`; its remote is codesoda/gliner2-rs. Do not mistake a local directory name for a different repository.

OpenJev's uncommitted Discuss-style config work was ported into `systemone-config` and discarded upstream; the standalone `openjev` CLI no longer exists.

### Usable APIs and prerequisites

- OpenJev: `Decision::new`, `StateValue`, `Noul`, `Score`; model registry/cache resolution; `EngineHandle::spawn_resolved`, `score_direct`, `score_shared`, `score_batch`, `shutdown`. Read exact current signatures and feature gates from upstream source before implementation. The engine owns native state on a thread; its synchronous methods must not block HTTP executor threads.
- OpenJev's `server/jev.rs` is the existing wire/projection reference, not a universal inference contract. Core native decisions allow 2–16 options; the server handles singleton Choice deterministically. State restrictions include integer-only JSON in this adapter. These limits must not be imposed on every backend.
- GLiNER (historical finding, resolved at `1492d6b`): `Gliner2Pipeline::classify(text, task, labels, multi_label, cls_threshold)` returned winner-only `Single` or threshold-filtered `Multi`. Neither was the full categorical distribution needed here. Add a public pipeline API exposing ordered logits/probabilities with explicit activation and runtime settings; do not recreate private embedding assembly in SystemOne. Classification-only construction should avoid loading an unused extractor.
- Laya: build the actual Rust library behind its existing Python-first/parity gates. Amend its roadmap to make SystemOne the primary new multi-backend HTTP/queue layer rather than duplicating that work. This document does not itself change the upstream roadmap or remove a promised standalone CLI.

## 3. Workspace and trait design

Proposed SystemOne workspace:

```text
crates/
  systemone-core/          # neutral types, capability/error contracts, traits
  systemone-config/        # layered resolution and typed provider settings
  systemone-runtime/       # registry, dispatch, admission, lifecycle
  systemone-http/          # Jev wire projection, Axum service, SDK contracts
  systemone-cli/           # binary: systemone
  systemone-openjev/       # wrapper over upstream library crates
  systemone-laya/          # laya-core adapter (feature-gated: laya-cpu, laya-metal)
  systemone-gliner2/       # gliner2-rs adapter (feature-gated: gliner2; CPU only)
  systemone-remote/        # shared HTTP transport, separate Vercel/OpenRouter adapters
compat/                   # pinned SDK fixtures, request/response/error corpus
benchmarks/               # cross-backend quality and service performance
```

Depend on published crate releases where suitable; otherwise pin reviewed upstream Git revisions, commit Cargo.lock, and record native-library/weight revisions and licenses. Development path overrides must be optional and never required by release builds. No circular dependency: adapters depend on core and vendor libraries; vendor libraries do not depend on SystemOne.

### Neutral types

- `BackendId`: configured instance name; separate from `ProviderKind` and `ModelId`.
- `DecisionRequest`: ordered JSON state/questions and selected model; transport selectors resolved outside inference. Preserve option identities/order and arbitrary valid wire values until adapter validation; never silently coerce or truncate.
- `Question`: tagged `Choice`, `Noul`, `Score`, retaining instructions and criteria. Ordered collections must survive parse/render.
- `ChoiceResult`: selected option, complete ordered distribution, optional confidence plus its definition.
- `NoulResult`: P(true), optional full binary distribution and confidence provenance.
- `ScoreResult`: ordered level distribution, expected zero-based index, original rubric/legend, confidence provenance. Custom numeric scales are a future extension, not a silent change to Jev semantics.
- `DecisionResponse`: actual model, ordered answers, provider usage, optional internal diagnostics and allowed provider extensions.
- `Capabilities`: supported primitives and model aliases; state/option/question/context limits; truncation policy; batching/cancellation modes; precision/device; probability/confidence definitions; native versus derived primitive implementations.
- `BackendError`: validation, unsupported capability/model, unavailable, overload, timeout, cancelled, upstream and internal, with sanitized provider code/status where applicable.

Do not use OpenJev's `Readout` as the common type: token slots, KV receipts and logits are optional vendor evidence, not requirements for encoder models or remote APIs. Usage counters must retain their meaning; do not invent zero tokens when a provider reports unknown.

### A small object-safe trait family

The following are design signatures, not compiled API declarations. Use boxed futures (`Send`) for registry-held trait objects; finalize lifetimes/error/result definitions in M0 without changing responsibilities.

```rust
// BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send + 'a>>;
trait BackendInfo: Send + Sync {
    fn id(&self) -> &BackendId;
    fn capabilities(&self) -> &Capabilities;
}
trait DecisionBackend: BackendInfo {
    fn evaluate<'a>(&'a self, request: &'a DecisionRequest,
                    context: &'a CallContext) -> BackendFuture<'a, DecisionResponse>;
}
trait ChoiceBackend: BackendInfo {
    fn choice<'a>(&'a self, input: &'a ChoiceInput,
                  context: &'a CallContext) -> BackendFuture<'a, ChoiceResult>;
}
trait NoulBackend: BackendInfo {
    fn noul<'a>(&'a self, input: &'a NoulInput,
                context: &'a CallContext) -> BackendFuture<'a, NoulResult>;
}
trait ScoreBackend: BackendInfo {
    fn score<'a>(&'a self, input: &'a ScoreInput,
                 context: &'a CallContext) -> BackendFuture<'a, ScoreResult>;
}
```

Each adapter wrapper implements the supported primitive traits and the batch `DecisionBackend` facade. Primitive convenience calls may delegate to a single-item `evaluate`; never implement both defaults in terms of each other. An unsupported primitive is not fabricated to satisfy a type. `CallContext` carries deadline, cancellation and request ID—not plaintext credentials or HTTP framework types.

The dispatcher sends a multi-question request through `evaluate` once. It must not turn it into repeated HTTP calls or discard vendor batching merely because there are primitive traits. Local adapters enqueue owned work on their native owner threads as necessary; registry handles can be Send/Sync even when raw model contexts are not. A separate factory/lifecycle interface validates typed configuration, loads once, reports readiness and drains/shuts down resources.

### Primitive equivalence is tested, not assumed

- OpenJev: preserve exact prompts/token IDs, label verification, softmax and existing honesty strings. Preserve strict authored144 hash/token gates and runtime-specific receipts for shared/batch execution. Failed shared support remains an explicit serial fallback; requesting required shared execution still errors.
- GLiNER: Choice uses full **softmax** distribution, not thresholded independent sigmoid outputs. Noul uses deliberately ordered false/true labels and verified P(true). Score uses rubric label probabilities with `sum(i * p[i])`; label this as derived ordinal classification, not a trained calibrated scoring head. Runtime instructions/descriptions must map deliberately into the GLiNER schema, with held-out evaluation.
- Laya: preserve encoder/head architecture, prompt budgets, tokenizer, temperatures, output ordering and polarity. Its native true probability is index 1; OpenJev's native yes probability is index 0. Do not share positional assumptions.
- Remote: preserve provider-produced answers/confidence/usage and approved extensions. Do not softmax probabilities again or replace provider confidence with a local margin.
- Singleton Choice may be deterministic where the wire contract permits; report it as no inference rather than fake model certainty. Keep native limitations visible.
- Before wire rounding, validate distributions are finite, nonnegative, correctly ordered and normalized within a declared tolerance. Do not silently repair corrupt responses. Lock wire rounding, tie-breaking, null/empty input, error envelope and extra-field rules using SDK fixtures, rather than claiming Python/Laya/OpenJev all serialize identically.

## 4. Configuration contract

Precedence: **built-ins < user file < current-directory file < environment < CLI**. This matches Discuss CLI's inspected layering. CWD-only means no ancestor walk, even inside a Git repository.

Files: `~/.systemone/systemone.config.toml` on macOS/Linux; `%USERPROFILE%\.systemone\systemone.config.toml` on Windows; `./systemone.config.toml` at project level. Explicit `--no-config` bypasses files and `SYSTEMONE_*` argument defaults, but not credentials deliberately referenced by an explicitly configured remote backend. Help/version must work even with malformed config.

Rules:

1. Merge by backend instance ID and setting key, preserving omitted values. Reject malformed files, unknown keys, invalid enums and invalid individual values even if a later source overrides them. Validate cross-field requirements after merging (including enabled-backend credentials).
2. Resolve config once. No hidden reading of `~/.openjev`, Laya/GLiNER config files or their environment defaults inside adapters. Pass explicit cache paths, devices, model revisions and inference parameters through typed upstream options; add upstream option APIs if necessary.
3. Backend settings are vendor-owned typed namespaces under `[backends.<id>.settings]`, not arbitrary strings silently ignored by libraries. SystemOne validates and converts them, without reimplementing native inference. Separate settings schemas for each vendor.
4. Top level: `default_backend`, output options. `[server]`: host, port, inbound auth-env reference, body limits, deadlines. `[backends.<id>]`: kind, enabled, model, allowed model aliases, queue capacity, max in-flight. Only an enabled and usable instance can be the default.
5. Instances are disabled unless explicitly enabled. Initial built-ins contain no enabled remote instance. The example enables one CPU OpenJev model; explicit Metal is separately configurable. No runtime silent CPU fallback when Metal was requested.
6. ENV mappings: `SYSTEMONE_DEFAULT_BACKEND`, `SYSTEMONE_HOST`, `SYSTEMONE_PORT`, `SYSTEMONE_PRETTY`; `SYSTEMONE_BACKENDS__<ID>__ENABLED`, `__MODEL`, and `__SETTINGS__<KEY>` for backend leaves. Restrict IDs to lowercase ASCII letters/digits/single hyphens, starting with a letter; map ENV uppercase/underscore IDs back to lowercase/hyphens; reserve double underscores as delimiters. Reject unknown `SYSTEMONE_*` settings with a useful error. Credentials have their own explicitly referenced variable names.
7. CLI service overrides include `--host`, `--port`, `--default-backend`; generic typed leaf override `--set backends.local.settings.threads=4` is parsed against the settings schema, not arbitrary TOML injection. `--pretty=false` can negate a config boolean. Per-job `--backend` is a request selector, not a mutation to server defaults.
8. Relative paths resolve against invocation CWD, matching Discuss; document this prominently. Use platform home/cache discovery. Do not expand shell expressions. Never print secret contents in config diagnostics.
9. Credentials live in environment/secret injection, not inline TOML. `api_key_env` contains a variable name. Inbound service authentication is a different credential from outbound gateway keys. Never forward inbound Authorization to an upstream.
10. No model loading or network calls for config validation or listing disabled instances. `config show --redact` exposes resolved non-secret values and provenance; `config check` verifies structure without requesting inference. Explicit model preparation is a separate action.

Invalid default, enabled-but-uncompiled backend, unsupported platform/device, or missing required configuration is a startup error. Disabled backend configuration must still be syntactically valid but must not resolve secrets or allocate native models.

Config is immutable for a running process in v1; restart for changes. Do not implement unsafe hot reload of model owners or credential routing.

## 5. Request routing and wire compatibility

### Selection

`POST /v1/systemone` accepts the ordinary Jev fields `model`, `state`, `questions` plus optional top-level string `backend`. Also support `X-SystemOne-Backend` for SDKs that discard unknown body fields. If body/header both appear they must match; otherwise return validation error. CLI `--backend` similarly must not silently contradict an explicit selector in its request file.

Selection order is explicit request selector, otherwise the resolved `default_backend`. An unknown, disabled, unavailable or unauthorized backend errors. Do not reinterpret `model` as a backend name. Do not infer cloud selection from a model prefix.

Within the selected backend, omitted `model` uses that instance's configured model (document this convenience extension). For local instances, `jev-latest` is an explicit compatibility alias for the configured resident model; return actual model identity, not a claim to run Jev weights. Remote instances pass allowlisted provider models through. Request model selection never loads arbitrary checkpoints; only startup-configured local models are eligible.

No per-question routing in v1. One request goes to one selected instance. There is no automatic fallback; callers can explicitly retry another backend knowing the privacy/cost implications.

### Responses and endpoints

- Keep the Jev body fields and shapes: `model`, `answers`, `usage`; answers use `choice`/`probabilities`/`confidence`, `noul`, or `score`/`legend` as required by pinned SDK fixtures. Do not invent a new wrapper around ordinary responses.
- Preserve permitted upstream additions such as OpenRouter `id`, `provider`, `usage.cost`; preserve original numeric meanings and unknown usage rather than recomputing billing or guessing tokens.
- Default body stays compact. `x-systemone-backend`, `x-systemone-provider`, request ID, timing and execution disclosure headers expose routing; native diagnostics are opt-in, not a dump of prompts/logits in every response. Ensure proxy deployments can forward these headers.
- Preserve OpenJev's execution/probability honesty strings in diagnostic fields/headers; confidence semantics must be discoverable for all adapters. Forced typed answers can still be wrong.
- `GET /v1/models` returns a tested Jev SDK-compatible catalogue for the default or explicitly selected backend. Do not merge provider model IDs into an ambiguous global list. `GET /v1/backends` is the separate capability/discovery extension and never exposes credentials/local private paths.
- `/healthz` is process liveness. `/readyz` is ready only after all enabled required instances load/validate; default startup policy fails if an enabled local model cannot load. Runtime upstream network outages are reported per backend without a paid health inference request.
- Preserve and test provider HTTP error status and TypeSafe error payload where compatible. Transport timeouts and local errors use the documented compatible error envelope. Never leak upstream credentials, full request states or unsanitized internal errors.
- General wire validation is separate from adapter limits. Keep duplicate-key and depth/body-size safeguards, JSON option order, and valid numeric values. OpenJev's float restriction is an adapter-specific rejection, not a global restriction.

SDK compatibility gate must pin actual TypeSafe Python/JS SDK versions and include ordinary requests with no extension. Unknown field acceptance, criteria forms, precision and error shapes are verified, not guessed. Publish supported subset/limitations if any gate fails.

## 6. Vercel and OpenRouter passthrough

Both are required first-class adapters, not optional future inspiration.

| Kind | Configured base URL | POST path | Auth |
| --- | --- | --- | --- |
| `vercel` | `https://ai-gateway.vercel.sh/typesafe` | `/v1/systemone` | Bearer gateway API key; optionally operator-supplied OIDC token |
| `openrouter` | `https://openrouter.ai/api` | `/v1/systemone` | Bearer OpenRouter API key |

Use those dedicated APIs, not `/chat/completions` or JSON-generating prompts. Store base URLs in operator config only; reject request-provided destinations. Require HTTPS except explicit loopback test endpoints; disable redirects and document proxy behavior. Strip SystemOne selectors before forwarding. Preserve state/questions semantically without applying local prompt transformations.

Vercel documents TypeSafe request/response shapes and `/typesafe/v1/models`. OpenRouter documents bare model aliases (`jev-1.13`, `jev-latest`) and namespaced model responses. OpenRouter's `/api/v1/models` is **not** the TypeSafe models schema: obtain/validate eligible System One models and project a compatible catalogue, or serve the configured allowlist; never blindly proxy that catalogue into the SDK route. Version and test the model mapping.

Use bounded asynchronous HTTP concurrency per instance, independent of local model queues. Bound connection/total deadlines and response bytes. Preserve provider rate-limit/retry information as allowed. Disable inference POST retries by default to avoid duplicate billing; future retries need a documented idempotency/budget policy. Cancellation stops client work where possible but cannot guarantee the remote provider stops or avoids billing.

Remote mock tests are mandatory and credential-free. Live smoke tests are explicit opt-in with operator credentials, small fixed request counts and spending acknowledgement; no use of private fixtures. Never assume promotional pricing/free windows remain available. Do not log keys or user payloads by default.

## 7. Scheduling, lifecycle and security

SystemOne owns **in-memory bounded admission**, not a file-backed queue/MPSC. Process restart loses queued work; this is request/response inference, not durable jobs.

- Per-instance queue capacities and max-in-flight limits prevent one backend starving another. Also bound total accepted work and expanded state memory.
- Local default: one resident worker/model context per instance unless tested otherwise. Native owner-thread channels are implementation details, not a second unbounded queue. One SystemOne admission permit follows the request until native completion.
- HTTP handlers asynchronously await completion; a synchronous native inference method runs off the async executor. Client disconnection cancels queued work when possible. Once a noninterruptible call begins, it retains its permit until it actually completes, even if the HTTP deadline has expired.
- Queue wait counts toward the total request deadline. Distinguish overload (429), timeout (504), unavailable (503), and validation (400), subject to the SDK error contract. Bound upstream response bodies and request fanout.
- Multi-question batching is backend-specific: Laya independent encoder rows may batch; OpenJev may explicitly serialize due to receipts; GLiNER behavior must be measured. Optional micro-batching is later and cannot silently change truncation, ordering or numerical behavior.
- No assumption that multiple CPU threads imply multiple concurrent safe model contexts. Measure throughput/RAM before adding parallel contexts. Serialize controlled GPU benchmarks to avoid interference; document shared-device contention when serving multiple enabled instances.
- Load only enabled local models, verify pinned revisions/hashes, reuse one cache artifact per model, and avoid duplicate downloads. Detect aggregate RAM/VRAM pressure; report startup failure rather than swapping invisibly between models.
- Graceful shutdown stops admission, drains to a bounded deadline, signals workers, closes remote transports and releases models. Verify SIGINT/SIGTERM and Windows console shutdown semantics.
- Default loopback binding. Require configured inbound auth for non-loopback; constant-time secret comparison where applicable. No permissive CORS by default. Backend selection is restricted to operator-enabled instances and, if configured, per-key permissions. Treat state as untrusted data, not instructions to change routing or disclose secrets.

## 8. Cross-repo ownership and rollout

| Repo | Owns / required work | Does not own |
| --- | --- | --- |
| systemone | Shared traits/types, config, adapters, Jev wire API, queues, routing, hosted transport, SDK tests, CLI, packaging, comparative benchmarks | Native model rewrites |
| openjev-rs | Stable core/llama library surface, explicit options, cache/native lifecycle, prompt/readout parity, execution receipts and native benchmarks | New vendor-agnostic routing |
| laya-rs | Python baselines first, tokenizer/architecture/head parity, Rust runtime and device profiles, explicit typed options, batch/readout APIs, runtime benchmarks | Duplicate multi-backend service framework |
| gliner2-rs | Full ordered distribution API, explicit activation/polarity/options, classifier-only loading, execution-provider options, classification benchmarks | Pretending sigmoid confidences are categorical probabilities |

Implementation tasks in each repo should link this canonical plan and state the contract commit they implement. An initial upstream docs-only PR should add that link and reconcile any competing server roadmap; do not delete existing working servers or CLI config as part of this plan. Deprecation/migration is a separate reviewed decision.

Adapter extraction sequence:

1. Capture OpenJev HTTP wire/error/lifecycle tests as compatibility evidence with attribution.
2. Add missing upstream public APIs through small upstream changes; preserve standalone CLI behavior and numerical gates.
3. Land/tag or pin reviewed upstream revisions before depending on them from SystemOne. Never release against sibling path dependencies or dirty trees.
4. Implement wrappers here, with vendor-specific conversion tests and explicit capability rejection.
5. Publish migration examples: existing OpenJev server remains usable; change client base URL to SystemOne for multi-backend use. Translate config explicitly; do not silently import old per-repo config.

Native FFIs may link conflicting ONNX/BLAS/OpenMP/CUDA/Metal dependencies. Prove feature combinations build and initialize together early. Default remote-only build must not link local native runtimes. Offer documented platform/backend feature bundles if a single all-backend binary cannot be shipped safely; do not advertise unsupported combinations.

## 9. Milestones and status

Every milestone ends with formatting, `cargo clippy --workspace --all-targets -- -D warnings`, workspace tests, the relevant feature/platform checks, a `CHANGELOG.md` entry and a commit. Mark model-dependent tests as explicitly skipped when prerequisites are missing; do not count skips as inference evidence. Stop on failed semantics and record evidence rather than weakening tests.

| Milestone | Status | Where |
| --- | --- | --- |
| M0 Contracts and build feasibility | Done except the llama.cpp + ort linker check | [CHANGELOG](../../CHANGELOG.md), issues |
| M1 OpenJev adapter, one-shot CLI, config | Done | [CHANGELOG](../../CHANGELOG.md) |
| M2 Resident HTTP service, SDK compatibility | Done (JS SDK); Python SDK and overhead measurement open | [CHANGELOG](../../CHANGELOG.md), issues |
| M3 Hosted Jev passthrough (Vercel, OpenRouter) | Planned | GitHub issues |
| M4 GLiNER2 adapter | Done as a source build (`gliner2` feature, CPU); held-out results in `docs/gliner2-evaluation.md`; binary packaging open | GitHub issues |
| M5 Laya Python baseline → Rust runtime → adapter | Done as a source build (`laya-cpu`/`laya-metal`); binary packaging open | [CHANGELOG](../../CHANGELOG.md), issues |
| M6 Cross-backend quality and performance | Planned | GitHub issues |
| M7 Portable releases (Windows, signing, clean-machine smoke) | macOS/Linux archives ship; rest planned | GitHub issues |

Completed work is described in `CHANGELOG.md`. Remaining work is tracked as [GitHub issues](https://github.com/codesoda/systemone/issues); each issue carries the acceptance gate that used to live in this section. Do not add new checklists here.

## 10. Laya optimization research policy

Read [the source register](../research/sources.md) before choosing an accelerator. MLX is an Apple-oriented implementation reference, not the cross-platform abstraction itself.

Priority candidates: resident weights; Rust tokenizer; bounded CPU tokenization/prefix caching keyed by exact model/tokenizer/config; length bucketing; batch size; fused attention/MLP/norm primitives; FP16 with measured fidelity; shape specialization/compilation; explicit tensor evaluation/synchronization; eliminating CPU/GPU transfers and duplicate allocations. Measure each independently and in combination.

ModernBERT parity hazards include alternating local/global attention, inclusive sliding-window boundaries, local/global RoPE bases, first-layer normalization, padding masks and final-head behavior. Check state_dict names/shapes and unsupported RoPE scaling. Preserve decision Transformer/scoring/action heads, not merely the encoder.

Bidirectional state representations depend on the question. Sharing tokenization or exact immutable prefixes does **not** justify reusing arbitrary encoded state hidden vectors across questions. Avoid that shortcut unless a separately validated architectural change is intentionally made.

Rust binding paths to MLX, a native Rust Metal implementation, or export to a suitable runtime are alternatives to investigate—not already selected or guaranteed. CPU parity/portability remains mandatory. Review API maturity, licensing, required native toolchains and packaged shared libraries before committing. Support macOS/Linux/Windows at the service/CPU level without forcing Apple-only imports into non-Apple builds.

Laya-MLX reports useful small optimizations, not a universal further 10× speedup. Start from its current optimized baseline, account for hardware differences, and retain raw failed experiments. Quantization/retraining is a separate quality tradeoff, not a free speed improvement.

## 11. Blockers and decision log

Resolved: separate repo; SystemOne-owned common config/API/queue; upstream library independence; five explicit backend kinds including both hosted gateways; request override/default config; no cloud failover; in-memory queues; CWD config layering; CPU cross-platform baseline; library-first Laya work.

Open implementation decisions with required resolution gates:

| Decision | Gate / evidence | Safe interim behavior |
| --- | --- | --- |
| Exact SDK versions and wire precision/error projection | M0/M2 pinned fixtures + actual SDK parsing | Describe compatibility as planned |
| Upstream library revisions and native linker coexistence | M0 reviewed source/build matrix | Remote-only build; do not enable failing profile |
| GLiNER full distribution / provider controls | Delivered upstream (gliner2-rs #6/#7/#8) with goldens for three checkpoints | Adapter uses the full softmax; sigmoid is never requested |
| Laya Rust runtime and Metal implementation | M5 Python/MLX baseline + Rust parity/package feasibility | Adapter unavailable, no hidden Python dependency |
| Confidence/calibration comparability | M6 held-out domain-specific evidence | Expose provenance; no universal threshold guarantee |
| Accelerator release combinations | M7 clean-machine artifact tests | Ship only verified bundles and disclose omissions |

If a gate blocks, record commands, source/model versions, measured evidence, attempted approaches, exact blocker and next required input. Proceed with independent adapters where safe, but do not mark the blocked integration complete or relax upstream numerical gates. No publication of benchmark superiority without same-workload evidence.
