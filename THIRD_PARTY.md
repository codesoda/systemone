# Third-party material

SystemOne (`s1`) is MIT-licensed project code ([LICENSE](LICENSE)). It links
and redistributes third-party software under other terms. This file is an
attribution overview; the complete, generated notice bundle that ships inside
every binary archive is `THIRD_PARTY_LICENSES.html`, alongside the Rust
standard-library notice `RUST-COPYRIGHT-library.html`.

## Upstream decision libraries

- [codesoda/openjev-rs](https://github.com/codesoda/openjev-rs) (`openjev-core`,
  `openjev-llama`), MIT, pinned by Git revision in `Cargo.toml`. openjev-rs is
  itself an independent implementation inspired by
  [TheoLeeCJ/openjev](https://github.com/TheoLeeCJ/openjev) (SemIf), whose
  MIT-licensed prompt string, fixtures and portions of the validation, numeric
  and evaluation logic it ports; see openjev-rs's own `THIRD_PARTY.md` for the
  preserved copyright notice. The Jev wire projection in `systemone-http` was
  moved here from openjev-rs.
- [codesoda/laya-rs](https://github.com/codesoda/laya-rs) (`laya-core`),
  Apache-2.0, pinned by Git revision in `Cargo.toml` and enabled by the `laya-cpu` /
  `laya-metal` features. It reproduces the open
  [Laya](https://github.com/NandhaKishorM/laya) runtime by Convai Innovations
  and links [MLX](https://github.com/ml-explore/mlx) (MIT, through a vendored
  `mlx-sys` pinned in the same repository) or
  [Candle](https://github.com/huggingface/candle) (MIT OR Apache-2.0). These
  features are **not in the binary releases yet**, so the generated notice
  bundle below does not cover them; that is tracked as a release blocker for
  shipping Laya.
- [codesoda/gliner2-rs](https://github.com/codesoda/gliner2-rs) (`gliner2-rs`),
  pinned by Git revision in `Cargo.toml` and enabled by the `gliner2`
  feature. It runs ONNX exports of [GLiNER2](https://github.com/fastino-ai/GLiNER2)
  by Fastino AI (Apache-2.0) through [ONNX Runtime](https://github.com/microsoft/onnxruntime)
  (MIT) via the [ort](https://github.com/pykeio/ort) crate (MIT OR
  Apache-2.0), which fetches a pinned prebuilt static library at build time.
  gliner2-rs itself declares no license in its manifest yet; that must be
  settled before it ships in a binary release. These features are **not in
  the binary releases yet**, so the generated notice bundle below does not
  cover them.
- SystemOne is not affiliated with or endorsed by SemIf, TheoLeeCJ, TypeSafe
  AI, Convai Innovations, Fastino AI, or Jev. Names and marks belong to their respective
  owners.

## Native inference

- [llama.cpp](https://github.com/ggml-org/llama.cpp), MIT, pinned indirectly
  by `llama-cpp-2` 0.1.156 to commit
  `e79e4bf660e19f2ad851e06c6913f7a8c5852621` and compiled statically into the
  `native`/`metal` builds. Its bundled llamafile SGEMM, cpp-httplib,
  nlohmann/json, base64, subprocess, and adapted ggml CPU/Metal sources carry
  their own notices, which the generated bundle preserves verbatim from the
  hash-pinned upstream files.
- [utilityai/llama-cpp-rs](https://github.com/utilityai/llama-cpp-rs),
  MIT OR Apache-2.0, registry crates pinned to 0.1.156.
- [hf-hub](https://github.com/huggingface/hf-hub), Apache-2.0, pinned to 1.0.0,
  for verified model downloads. Its pinned hf-xet closure includes MPL-2.0-only
  [colored 3.1.1](https://github.com/mackwic/colored) and
  [option-ext 0.2.0](https://github.com/soc/option-ext); see below.
- [aws-lc-rs](https://github.com/aws/aws-lc-rs) / AWS-LC (through rustls),
  ISC AND (Apache-2.0 OR ISC) with compiled-in BoringSSL/OpenSSL, Fiat,
  s2n-bignum and Jitter Entropy notices; AWS-LC expressly elects BSD-3-Clause,
  not GPL-2.0, for Jitter Entropy.

## Service and CLI

- [axum](https://github.com/tokio-rs/axum) 0.8.9 and
  [Tokio](https://github.com/tokio-rs/tokio) 1.53.1, MIT, provide the resident
  HTTP service.
- [clap](https://github.com/clap-rs/clap) 4.6.7, MIT OR Apache-2.0.
- [reqwest](https://github.com/seanmonstar/reqwest) 0.13.5 with rustls,
  MIT OR Apache-2.0, for `s1 call`.
- The Jev wire adapter and `compat/sdk-js/` smoke are checked against
  [typesafe-ai/typesafe-sdk-js](https://github.com/typesafe-ai/typesafe-sdk-js)
  commit `66880ccded6cb642dc1809620c2b108c33730214`, npm package 0.6.0 (MIT).
  Two-decimal wire projection was checked against Vercel AI commit
  `20dd00abba618d5a516e0fee40ccd3e18a2bd1fb` (Apache-2.0). Neither is
  redistributed.

## MPL-2.0 covered-source availability

Each binary release includes `colored-3.1.1.crate` and
`option-ext-0.2.0.crate` at the archive root. These are the complete,
unmodified original crates.io source archives for the exact dependency
versions in `Cargo.lock`, not reconstructed source trees. Their SHA-256
values are checked against the corresponding crates.io checksums in
`Cargo.lock`, recorded in `licenses/THIRD_PARTY_LICENSES.metadata.json`, and
included in `BUILD-INFO.json` with the other packaged-file hashes.

Recipients may extract, use, modify, and redistribute those covered-source
archives under the MPL-2.0 terms included inside each archive and reproduced in
`THIRD_PARTY_LICENSES.html`. Those terms apply to their covered files; the
larger SystemOne program remains under its stated MIT license. This
source-availability provision is specific to these reviewed dependencies and
is not a blanket copyleft exception.

## Regenerating the bundle

```sh
python3 scripts/generate_third_party_licenses.py   # cargo-about 0.9.2, network
python3 scripts/check_third_party_licenses.py      # offline, run by CI
```

`about.toml` records license resolution and the exact llama-cpp bindings
license clarification. `licenses/THIRD_PARTY_LICENSES.metadata.json` pins the
`Cargo.lock` hash, bundle hash, target/features, native commit, exact native
source hashes, the two covered-source archive identities and hashes, and the
Rust notice's SHA-256 plus `rustc -vV` release/commit provenance. Regeneration
copies that notice from the matching installed Rust 1.95.0 compiler after
checking its `rustc` component manifest; it does not synthesize notice text.
CI runs the offline checker and fails if the lock, bundle, metadata, Rust
notice, workflow toolchain pin, or covered-source archive bytes disagree.

No model weights are distributed or covered by the SystemOne MIT license.
Model artifacts downloaded by `s1 openjev models pull` are subject to their
own model cards and licenses, listed in openjev-rs's `manifests/models.json`.
