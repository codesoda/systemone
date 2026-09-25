//! Pinned files for each GLiNER2.5 profile, for `s1 setup` downloads.
//!
//! The table covers exactly what the adapter reads (`config.json`,
//! `tokenizer.json`, `encoder.onnx`, `classifier.onnx`), the bundle's
//! `export_manifest.json` that `verify_sha256` checks them against, and the
//! bundle's `LICENSE` and `NOTICE`. The extraction heads are never
//! downloaded. Pins come from `codesoda/gliner2-onnx` at one immutable
//! revision and were cross-checked against each bundle's manifest.

use std::collections::BTreeMap;

use serde::Deserialize;
use systemone_core::{DownloadPlan, HostError, PinnedFile};

use crate::settings::PROFILES;

const PINS: &str = include_str!("download_pins.json");

#[derive(Deserialize)]
struct Pins {
    profiles: BTreeMap<String, ProfilePins>,
}

#[derive(Deserialize)]
struct ProfilePins {
    bundle: String,
    files: Vec<PinnedFile>,
}

/// Files to download for `profile` (one of [`PROFILES`]).
pub fn download_plan(profile: &str) -> Result<DownloadPlan, HostError> {
    if !PROFILES.contains(&profile) {
        return Err(HostError::validation(format!(
            "unknown gliner2 profile {profile:?}; expected one of {PROFILES:?}"
        )));
    }
    let pins: Pins = serde_json::from_str(PINS)
        .map_err(|error| HostError::internal(format!("embedded gliner2 pins: {error}")))?;
    let entry = pins.profiles.get(profile).ok_or_else(|| {
        HostError::internal(format!("embedded gliner2 pins lack profile {profile}"))
    })?;
    Ok(DownloadPlan {
        label: entry.bundle.clone(),
        files: entry.files.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_valid_plan_with_the_classification_files() {
        for profile in PROFILES {
            let plan = download_plan(profile).unwrap();
            plan.validate().unwrap();
            assert_eq!(plan.label, format!("gliner2.5-{profile}-v1"));
            for name in [
                "config.json",
                "tokenizer.json",
                "encoder.onnx",
                "classifier.onnx",
                "export_manifest.json",
            ] {
                assert!(
                    plan.files.iter().any(|file| file.path == name),
                    "{profile} lacks {name}"
                );
            }
            assert!(
                !plan
                    .files
                    .iter()
                    .any(|file| file.path.contains("extractor"))
            );
            for file in &plan.files {
                assert!(
                    file.url.starts_with(
                        "https://huggingface.co/codesoda/gliner2-onnx/resolve/27310cd26099a387b9936a1e13b03d6a0700baf2/"
                    ),
                    "{}",
                    file.url
                );
            }
        }
        assert!(download_plan("large").is_err());
    }
}
