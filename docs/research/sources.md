# Source register and performance research

These are implementation references and externally reported measurements, **not** SystemOne or laya-rs benchmark results. Pin code, weights and environment before reproducing them. Upstream URLs can change; commit links below capture the inspected implementation generation.

## Laya upstream

- Repository: <https://github.com/NandhaKishorM/laya>
- Inspected main: [`42626c348753fbb17572a813127df2278a1ec527`](https://github.com/NandhaKishorM/laya/tree/42626c348753fbb17572a813127df2278a1ec527)
- [README](https://github.com/NandhaKishorM/laya/blob/42626c348753fbb17572a813127df2278a1ec527/README.md), [benchmarks](https://github.com/NandhaKishorM/laya/blob/42626c348753fbb17572a813127df2278a1ec527/BENCHMARKS.md)

Relevant architecture: ModernBERT-large English/typed-decisions checkpoints and mmBERT-base multilingual encoder, trained decision heads, runtime question formatting and temperature calibration. Port the complete decision model, not just the encoder. Runtime-defined questions are independent encoder rows that can be batched.

Upstream reports T4 single-question timings of 39.5 ms English and 32.8 ms multilingual and emphasizes preloading to avoid multi-second checkpoint churn. Those timings are not comparable to a Rust port on a different machine without a matched run. Its README also reports weak base-checkpoint zero-shot typed-decisions performance, overconfidence, language-dependent failures and high-cardinality option-budget limits. Preserve those caveats; a trained scoring objective alone does not prove calibration for our workloads.

Use its Python implementation first to establish checkpoint-specific prompt/token/tensor/output goldens and timing. Reconcile changing bundled versus separate model repositories by pinned revisions/hashes, not model name alone. Code/weight licenses must be checked per artifact; upstream identifies Apache-2.0.

## Laya-MLX — Apple Silicon implementation reference

- Repository: <https://github.com/mizorewww/laya-mlx>
- Inspected main: [`fc1df62828a3fedf4d8229fdac1cbd85f1cdf337`](https://github.com/mizorewww/laya-mlx/tree/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337)
- [README](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/README.md)
- [Benchmark method and results](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/BENCHMARKS.md)
- [Performance research](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/docs/PERFORMANCE_RESEARCH.md)
- [Mathematical speedup investigation](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/docs/MATH_10X_RESEARCH.md)
- [Engineering speedup investigation](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/docs/ENGINEERING_10X_RESEARCH.md)
- [Measured Snake optimization](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/docs/SNAKE_OPTIMIZATION.md)
- [Published model provenance/checksums](https://github.com/mizorewww/laya-mlx/blob/fc1df62828a3fedf4d8229fdac1cbd85f1cdf337/benchmarks/results/hub-publication.json)

The inspected README reports:

| External measurement | Reported result |
| --- | --- |
| Machine | M3 Max, 40 GPU cores, 128 GiB RAM |
| FP16 short question p50 | English 13.42 ms; multilingual 7.39 ms |
| 50-question throughput (`batch_size=64`, API default 16) | 146.8 / 395.0 questions/sec |
| Peak MLX allocation, short question | 943.6 / 687.6 MiB |
| Fidelity corpus | 63 questions × 3 checkpoints × 2 precisions; 378/378 selected answers match upstream |
| Repeated stability | 100 calls per configuration, reported finite/deterministic and no measured active-memory growth |

Timing includes preparation, tokenization, synchronized inference, calibration and formatting, but excludes model loading. Selected-answer agreement on 63 questions is not full probability equivalence or a broad accuracy guarantee. Peak MLX allocation is not total process RSS.

Useful techniques to study/reproduce:

- Faithful local/global attention masks, RoPE bases/window boundaries and first-layer normalization.
- FP16 safetensors conversion with every parameter name/shape validated; conversion is not quantization or retraining.
- Rust-backed Hugging Face tokenization alongside MLX neural inference.
- Independent question batching, bounded tokenized-prefix caching, shared CPU state tokenization.
- Optional compilation, shape padding and warmup-cost accounting; these may regress some workloads.
- Paired ablations and synchronized timings; source-weight/checkpoint provenance and repeated memory tests.

The README explicitly says state hidden representations cannot simply be reused across arbitrary bidirectional question rows. It reports approximately 1.03–1.08× selected paired median improvements in further optimization investigations, not a universal 10× gain. The Snake result concerns a specific three-question loop and safety layer, not the one-question API benchmark.

This is an independent **Python/MLX port**, not an existing portable Rust dependency. Evaluate a Rust binding/native runtime or an equivalent Rust implementation in laya-rs. Do not introduce a hidden Python runtime into a supposedly self-contained SystemOne binary. MLX-specific acceleration must remain optional; macOS/Linux/Windows CPU support and native packaging need separate evidence. If reusing Apache-2.0 code, preserve license/NOTICE and attribution; do not infer MIT relicensing from this repo's license.

The linked detailed research reports are follow-up reading before optimization implementation; the numbers summarized here come from the inspected README, not an independent rerun.

## Avi Chawla — local Jev-style next-token scoring

- Requested post: <https://x.com/_avichawla/status/2101563610644496464?s=20>
- Linked article: <https://x.com/i/article/2101408350391136256>
- Title: **Build your own Jev (100% local)**.

The article text was accessible through the public FxTwitter mirror when inspecting the post. It describes SGLang `/v1/score`, single-token answer labels, selecting logits from one next-token vector, restricted softmax, and benchmarking against generation. It explicitly distinguishes recreating an inference path from Jev's training/calibration.

This is primarily an **OpenJev-style decoder scoring** reference, not a Laya encoder/Metal implementation. Relevant lessons: exact answer-position tokenization, conditional probabilities, an explicit OTHER/ESCALATE option when appropriate, and same-model comparisons with structured generation. Do not treat social-media benchmark claims as measurements of our code. No SGLang adapter is required in the initial five-backend scope; consider it later only with a separate use case and contract tests.

## Hosted Jev endpoint references

- Vercel: <https://vercel.com/docs/ai-gateway/sdks-and-apis/typesafe>
  - Base `https://ai-gateway.vercel.sh/typesafe`.
  - `POST /typesafe/v1/systemone`, `GET /typesafe/v1/models`.
  - Bearer gateway API key or Vercel OIDC token.
  - TypeSafe-compatible fields/errors. The docs also offer a newer evaluation API; our compatibility adapter intentionally uses the documented TypeSafe endpoint.
- OpenRouter: <https://openrouter.ai/docs/guides/community/typesafe-sdk>
  - Base `https://openrouter.ai/api`; `POST /api/v1/systemone`.
  - Bearer OpenRouter API key.
  - Accepts bare Jev aliases and namespaced IDs; response may include `id`, `provider`, `usage.cost`.
  - `/api/v1/models` has OpenRouter's catalogue shape, which the TypeSafe SDK rejects. SystemOne must normalize/filter or expose a configured compatible catalogue rather than proxy that shape blindly.

Recheck endpoint/model policies and pricing at implementation/live-test time. Preserve provider usage and avoid automatic billed retries. These are planned integrations; no authenticated provider requests were made to prepare this plan.

## Local source evidence

- OpenJev: `crates/openjev-core/src/{types,primitives}.rs`, `crates/openjev-llama/src/{model,engine}.rs`, `crates/openjev-cli/src/server/{mod,jev}.rs`.
- Laya Rust project: `docs/plans/initial-build.md`, `docs/PROGRESS.md`, Python baseline at `benchmarks/baseline/src/laya_baseline/smoke.py`.
- GLiNER Rust project: `src/{pipeline,classification,classifier,encoder}.rs`; classifier output currently omits the complete single-label distribution.

Exact inspected repo heads and cross-repo prerequisites are recorded in the [shared plan](../plans/cross-repo.md#2-evidence-and-current-baseline).
