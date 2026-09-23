# Contributing to SystemOne

Thank you for contributing.

## Ground rules

- The project uses the [MIT License](LICENSE). Contributions use the same
  license. There is no CLA.
- Follow the [Code of Conduct](CODE_OF_CONDUCT.md).
- Keep pull requests focused, reviewable, and reversible.
- Read [README.md](README.md) and the
  [cross-repo plan](docs/plans/cross-repo.md) before you change shared
  semantics. The plan is canonical for the backend contract, configuration
  layering, routing, and wire compatibility. Change the plan and the affected
  tests in the same pull request.
- Track future work as GitHub issues. Record shipped work in
  [CHANGELOG.md](CHANGELOG.md) under `Unreleased`.

## What SystemOne promises

These rules protect users. A pull request that weakens one of them will not be
merged.

- **No silent fallback.** Never fall back to a cloud backend, a different
  device, a different model, or a different execution mode without telling the
  caller. Fail with a clear error, or disclose the fallback in the response
  headers and CLI diagnostics, as the OpenJev adapter does for shared → serial.
- **JSON-only stdout.** Result commands write one JSON document to stdout.
  Logs, diagnostics, and errors go to stderr as JSON records. Exit code 2 is a
  validation or usage error; exit code 1 is a runtime error.
- **Honest status.** Do not describe planned adapters, platforms, or releases
  as shipped. A test that skips because a model is missing is not evidence of
  inference. Do not weaken an upstream parity gate to make an optimization
  pass.
- **Pinned upstreams.** `openjev-core` and `openjev-llama` are Git
  dependencies pinned to a reviewed revision. Do not release with sibling
  `path` dependencies or with uncommitted upstream changes.
- **Nothing sensitive in the repository.** Never commit credentials, model
  weights, private request data, probe receipts from real runs, or fabricated
  results. Use synthetic fixtures.

## Development checks

Rust 1.95 is pinned by `rust-toolchain.toml`. The default build compiles no
inference runtime, so it runs no local inference; the hosted `typesafe`
adapter is always linked, so a default build does reach the network when a
hosted instance is configured and selected. `native`, `metal`, and `cuda` on
`systemone-cli` add OpenJev, `laya-cpu` / `laya-metal` add Laya, and `gliner2`
adds GLiNER2 (ONNX Runtime, fetched prebuilt at build time). All need CMake
plus a C/C++ toolchain; `laya-metal` compiles MLX from source (macOS).

Run these before you push:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
python3 scripts/check_third_party_licenses.py
aislop ci
```

If you touch the adapter, the HTTP service, or anything under `crates/`, also
build and run the native feature once. On Apple Silicon:

```sh
cargo clippy -p systemone-cli --all-targets --features metal,laya-metal,gliner2 -- -D warnings
cargo build -p systemone-cli --features metal,laya-metal,gliner2
```

CI runs the same checks on Linux for every pull request. The main Linux job
(`ubuntu-22.04`, with `laya-cpu`) also builds the release binary, so it keeps
the glibc 2.35 floor. `gliner2` runs in its own `ubuntu-24.04` job because the
ONNX Runtime library that `ort` downloads needs glibc 2.38. The macOS build
runs only for release tags.

### SDK smoke

The only backend-specific test SystemOne runs is the JS SDK smoke against a
real `s1 serve`; everything deeper (numerical parity, goldens, tolerances)
belongs to the backend's own repository. With a server running:

```sh
cd compat/sdk-js && npm ci
SYSTEMONE_BASE_URL=http://127.0.0.1:8080 node smoke.mjs                              # openjev
SYSTEMONE_BASE_URL=http://127.0.0.1:8080 SYSTEMONE_SMOKE_BACKEND=laya node smoke.mjs # laya
SYSTEMONE_BASE_URL=http://127.0.0.1:8080 SYSTEMONE_SMOKE_BACKEND=gliner2 node smoke.mjs # gliner2
SYSTEMONE_BASE_URL=http://127.0.0.1:8080 SYSTEMONE_SMOKE_BACKEND=typesafe node smoke.mjs # typesafe
```

Every backend runs the same assertions. The only per-backend part is the
`expectations` table at the top of `smoke.mjs`, which records two genuine
differences: whether the backend reports zero output tokens (OpenJev, Laya and
GLiNER2 generate nothing) or a positive count (TypeSafe generates), and whether
float JSON state values are rejected with a 422 (OpenJev) or accepted (Laya,
GLiNER2, TypeSafe). Everything else — one model card, `model` equal to that
card's name, the declared key order of every distribution, the score legend,
the numeric bounds, positive input tokens and the unknown-model 404 — is
shared and unconditional.

The `typesafe` leg needs a server that points at the hosted API. Write a
throw-away config in a scratch directory and start `s1` there, because `s1`
reads `./systemone.config.toml` from the working directory:

```toml
# /tmp/s1-typesafe/systemone.config.toml
default_backend = "hosted"

[server]
host = "127.0.0.1"
port = 8080

[backends.hosted]
kind = "typesafe"
enabled = true
model = "jev-1.13.0"
aliases = ["jev-latest"]

[backends.hosted.settings]
api_key_env = "TYPESAFE_API_KEY"

# The built-in defaults enable a local OpenJev instance, and `serve` refuses
# to start when an enabled backend cannot load. Partial overrides merge, so
# this one line is enough to leave it out of this run.
[backends.local]
enabled = false
```

```sh
cd /tmp/s1-typesafe && source ~/.typesafe.env && s1 serve
```

Read the key from a file that the shell sources, as above. A `VAR=value s1
serve` prefix puts it in the shell history and in the process listing.

Pin a concrete model version, as above. The upstream catalogue lists aliases
only (`jev-latest`, `jev-preview`) and does not reveal the version behind them,
so a request for `jev-latest` comes back as, for example, `jev-1.13.0`.
SystemOne passes upstream model identity through verbatim, so configuring the
alias as the instance `model` would make the answer name a model the catalogue
does not list. With the concrete version configured and the alias accepted
through `aliases`, SystemOne resolves `jev-latest` to `jev-1.13.0`, sends that
upstream, and the answer matches the single `/v1/models` card.

The key never belongs in the repository, in a committed config or in a log.
The run calls the hosted API and bills the account behind `TYPESAFE_API_KEY`,
so run it only when you change the TypeSafe adapter.

The GLiNER2 adapter also has a held-out product evaluation
(`evals/gliner2/run.py`, results in `docs/gliner2-evaluation.md`). Rerun it
when you change the mapping in `crates/systemone-gliner2/src/convert.rs` and
update the numbers.

### Changing dependencies

`Cargo.lock` is committed and every dependency is pinned exactly. When the lock
file changes, regenerate the third-party notice bundle and commit the result:

```sh
python3 scripts/generate_third_party_licenses.py   # needs cargo-about 0.9.2 and network
python3 scripts/check_third_party_licenses.py
```

See [THIRD_PARTY.md](THIRD_PARTY.md) for what the bundle contains and why the
`licenses/` directory exists.

### Changing the demo

The README animation is a staged VHS walkthrough under `demo/`. It executes
nothing. Keep `demo/scenes.json` and `docs/demo.md` in sync; `demo/test_session.py`
checks that. Re-record with `bash demo/record.sh`.

## Pull requests

- Explain the user outcome, what changed, and how you checked it. Say what is
  not verified.
- Add or update tests for behavior you change. Service and CLI tests use the
  fake backend in `systemone-http::test_support`; they need no model.
- Write comments in ASD-STE100 Simplified Technical English: active voice,
  present tense, one idea per sentence. Add a comment only for information the
  code cannot show.
- Do not suppress dead-code or deprecated-code lints. Remove the code or
  replace the API. A suppression needs an explanation and maintainer agreement.

## Releases

Maintainers cut releases. See [docs/RELEASE.md](docs/RELEASE.md) for what a
release contains and how it is verified. To release version `X.Y.Z`:

1. Set `version` in `Cargo.toml` (`[workspace.package]`).
2. Rename the `Unreleased` section of `CHANGELOG.md` to `[X.Y.Z]` and add the
   date.
3. Merge to `main`, then push the tag `vX.Y.Z`. CI builds both targets and
   publishes the release with notes taken from the changelog section.
