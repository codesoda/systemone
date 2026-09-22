# GLiNER2 adapter: held-out evaluation

This page records how the `gliner2` backend's Choice, Noul and Score mappings
behaved on a small held-out task set. It exists so the limitations in the
README are numbers, not adjectives. It is **not** a benchmark of GLiNER2.5,
and it says nothing about speed.

## Method

- Items: `evals/gliner2/heldout.json` — 24 Choice, 24 Noul, 16 Score tasks,
  hand-written for this evaluation. None appear in the gliner2-rs fixtures.
  Each has one expected answer a careful person would give.
- Runner: `evals/gliner2/run.py` sends the items through a real
  `s1 run --jsonl` against a configured `kind = "gliner2"` instance and
  scores the wire responses. Nothing in the runner knows where models live.
- Build: `s1 0.1.0` with `--features gliner2`, gliner2-rs
  `1492d6b6d9b688f11751094eb6c479e5fbb3883e`, ONNX Runtime 1.28.0 CPU, the
  three bundles at `codesoda/gliner2-onnx` revision
  `27310cd26099a387b9936a1e13b03d6a0700baf2`, default settings
  (4 intra-op threads, `verify_sha256 = true`), Apple M-series laptop.
- Metrics: Choice = argmax accuracy. Noul = accuracy of `P(true) ≥ 0.5`
  plus mean `P(true)` on expected-true and expected-false items. Score =
  argmax accuracy, within-one accuracy, and mean absolute error of the
  expected score against the expected level index.

Reproduce with:

```sh
python3 evals/gliner2/run.py --s1 target/release/s1 --config-dir <dir with systemone.config.toml>
```

## Results

| checkpoint | Choice | Noul (`no`/`yes`) | P(true) when true / false | Noul (`false`/`true`) | Score exact | Score within one | Score MAE |
| --- | --- | --- | --- | --- | --- | --- | --- |
| gliner2.5-small | 21/24 (88%) | 15/24 (62%) | 0.98 / 0.66 | 15/24 (62%) | 8/16 (50%) | 11/16 (69%) | 0.78 |
| gliner2.5-base | 22/24 (92%) | **23/24 (96%)** | 0.86 / 0.05 | 21/24 (88%) | 8/16 (50%) | 12/16 (75%) | 0.93 |
| gliner2.5-multi | **23/24 (96%)** | 20/24 (83%) | 0.84 / 0.43 | 15/24 (62%) | 8/16 (50%) | 10/16 (62%) | 0.91 |

## What this says

**Choice works.** Named categories with a one-line instruction, with or
without option descriptions, on plain text or compact JSON state. The two
base misses were both incident-priority items whose answer depended on
reading numbers in a JSON object (`affected_users: 3` → still `p1`); the
model does not weigh numeric fields.

**Noul works on `base` with `no`/`yes`.** 96% with clean separation
(0.86 vs 0.05). The one miss (`open_tuesday`) needed two facts from one
sentence. `false`/`true` is worse on every checkpoint, so `["no", "yes"]`
is the default `noul_labels`. `small` is strongly yes-biased (mean P(true)
0.66 on false items) and should not be used for Noul. `multi` is usable but
its false items sit near 0.43; treat its probabilities as a ranking, not a
calibrated rate.

**Score does not work as a grader.** Exact-level accuracy is 50% on every
checkpoint and the essay rubric came out inverted on `base` (the weakest
essay scored 3.71/4, the strongest 1.89/4). The mapping is what the plan
says it is: `sum(i · p[i])` over a zero-shot label classifier whose labels
happen to be ordered. The classifier has no notion that `4` is more than
`3`. Use Choice with named categories where you can. If you use Score on
this backend, expect coarse ordering at best, and do not use it to grade.

## What this does not say

- Nothing about latency or throughput. Any timing is a separate measurement.
- Nothing about calibration. `probability_status` says the distribution is
  a softmax over classifier logits, and that is all it is.
- Nothing about GLiNER2.5 extraction, which stays upstream and outside the
  decision contract.
- 64 items is enough to see the shape of the problem, not to rank
  checkpoints by a point or two.
