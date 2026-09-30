//! Pinned files for each published Kev checkpoint, for `s1 setup`
//! downloads.
//!
//! A checkpoint directory is assembled from three sources, each at an
//! immutable revision:
//!
//! - `base/`: the Qwen base snapshot (pins from kev-rs `manifests/sources.json`);
//! - `adapter/`: the LoRA adapter repository, without upstream's `head.pt`
//!   pickle (same source);
//! - `head.safetensors` + `head.meta.json`: the pickle-free conversion of
//!   `head.pt`, published at `codesoda/kev-heads`. The expected checksums
//!   are the ones kev-rs records for its own conversion.

use std::collections::BTreeMap;

use serde::Deserialize;
use systemone_core::{DownloadPlan, HostError, PinnedFile};

use crate::settings::DeviceSetting;

const PINS: &str = include_str!("download_pins.json");

/// Published checkpoints and the device each one needs.
pub const CHECKPOINTS: [(&str, DeviceSetting); 3] = [
    ("kev-0.6b", DeviceSetting::Cpu),
    ("kev-0.8b", DeviceSetting::Metal),
    ("kev-4b", DeviceSetting::Metal),
];

#[derive(Deserialize)]
struct Pins {
    checkpoints: BTreeMap<String, CheckpointPins>,
}

#[derive(Deserialize)]
struct CheckpointPins {
    files: Vec<PinnedFile>,
}

/// The device a published checkpoint runs on.
#[must_use]
pub fn checkpoint_device(checkpoint: &str) -> Option<DeviceSetting> {
    CHECKPOINTS
        .iter()
        .find(|(name, _)| *name == checkpoint)
        .map(|(_, device)| *device)
}

/// Files to download for `checkpoint` (one of [`CHECKPOINTS`]).
pub fn download_plan(checkpoint: &str) -> Result<DownloadPlan, HostError> {
    if checkpoint_device(checkpoint).is_none() {
        let names: Vec<&str> = CHECKPOINTS.iter().map(|(name, _)| *name).collect();
        return Err(HostError::validation(format!(
            "unknown kev checkpoint {checkpoint:?}; expected one of {names:?}"
        )));
    }
    let pins: Pins = serde_json::from_str(PINS)
        .map_err(|error| HostError::internal(format!("embedded kev pins: {error}")))?;
    let entry = pins
        .checkpoints
        .get(checkpoint)
        .ok_or_else(|| HostError::internal(format!("embedded kev pins lack {checkpoint}")))?;
    Ok(DownloadPlan {
        label: checkpoint.to_owned(),
        files: entry.files.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_checkpoint_has_a_valid_assembled_layout() {
        for (checkpoint, _) in CHECKPOINTS {
            let plan = download_plan(checkpoint).unwrap();
            plan.validate().unwrap();
            let has = |path: &str| plan.files.iter().any(|file| file.path == path);
            for path in [
                "base/config.json",
                "base/tokenizer.json",
                "base/tokenizer_config.json",
                "adapter/adapter_config.json",
                "adapter/adapter_model.safetensors",
                "head.safetensors",
                "head.meta.json",
            ] {
                assert!(has(path), "{checkpoint} lacks {path}");
            }
            assert!(
                plan.files
                    .iter()
                    .any(|file| file.path.starts_with("base/")
                        && file.path.ends_with(".safetensors")),
                "{checkpoint} has no base weights"
            );
            assert!(!plan.files.iter().any(|file| file.path.ends_with(".pt")));
        }
        assert!(download_plan("kev-9b").is_err());
    }

    #[test]
    fn heads_come_from_a_pinned_revision() {
        for (checkpoint, _) in CHECKPOINTS {
            let plan = download_plan(checkpoint).unwrap();
            for file in plan
                .files
                .iter()
                .filter(|file| file.path.starts_with("head."))
            {
                let revision = file
                    .url
                    .strip_prefix("https://huggingface.co/codesoda/kev-heads/resolve/")
                    .and_then(|rest| rest.split('/').next())
                    .unwrap_or_default();
                assert!(
                    revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{checkpoint}: {} is not pinned to a commit",
                    file.url
                );
            }
        }
    }
}
