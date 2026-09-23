//! Typed `[backends.<id>.settings]` for `kind = "kev"`.
//!
//! Kev has no model registry or downloader in this adapter: the operator
//! points `model_dir` at a directory holding one assembled checkpoint —
//! `base/` (the pinned Qwen base snapshot: `config.json`,
//! `tokenizer.json`, `tokenizer_config.json`, `*.safetensors`),
//! `adapter/` (`adapter_config.json`, `adapter_model.safetensors`),
//! `head.safetensors` and `head.meta.json` (the pickle-free conversion of
//! upstream's `head.pt`; kev-rs never executes pickle).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systemone_core::{HostError, paths::absolutize};

/// The identity a kev backend serves as when `model` is not configured.
pub const DEFAULT_MODEL_ID: &str = "kev-latest";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceSetting {
    /// Candle, fp32, Qwen3 attention-only checkpoints (kev-0.6b).
    Cpu,
    /// MLX (Apple Silicon), Qwen3.5 hybrid checkpoints (kev-0.8b, kev-4b).
    Metal,
}

/// Raw settings as written by the operator.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KevSettings {
    /// Directory holding the assembled checkpoint. Required to load.
    pub model_dir: Option<PathBuf>,
    /// `cpu` (Candle) or `metal` (MLX). Defaults to `cpu`. The runtime
    /// additionally checks the base generation matches the device — a
    /// hybrid (Qwen3.5) base on `cpu` or an attention-only (Qwen3) base on
    /// `metal` is a load error, never a silent fallback.
    pub device: Option<DeviceSetting>,
}

/// Settings after defaults and validation. Paths are absolute where a home
/// directory was available.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSettings {
    pub model_dir: Option<PathBuf>,
    pub device: DeviceSetting,
    /// The identity this backend serves as: configured `model` or
    /// `kev-latest`.
    pub model_id: String,
}

impl ResolvedSettings {
    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "model_dir": self.model_dir.as_ref().map(|path| path.display().to_string()),
            "device": device_name(self.device),
        })
    }
}

#[must_use]
pub const fn device_name(device: DeviceSetting) -> &'static str {
    match device {
        DeviceSetting::Cpu => "cpu",
        DeviceSetting::Metal => "metal",
    }
}

/// Whether this build links the runtime for `device`.
#[must_use]
pub const fn device_compiled(device: DeviceSetting) -> bool {
    match device {
        DeviceSetting::Cpu => cfg!(feature = "kev-cpu"),
        DeviceSetting::Metal => cfg!(feature = "kev-metal"),
    }
}

pub fn resolve(
    settings: &KevSettings,
    model: Option<&str>,
    home: Option<&Path>,
) -> Result<ResolvedSettings, HostError> {
    let model_id = match model {
        Some(value) if value.trim().is_empty() => {
            return Err(HostError::validation("model must not be empty".to_owned()));
        }
        Some(value) => value.to_owned(),
        None => DEFAULT_MODEL_ID.to_owned(),
    };
    let device = settings.device.unwrap_or(DeviceSetting::Cpu);
    let model_dir = settings
        .model_dir
        .as_ref()
        .map(|path| absolutize(path, home))
        .transpose()?;
    Ok(ResolvedSettings {
        model_dir,
        device,
        model_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_cpu_and_kev_latest() {
        let resolved = resolve(&KevSettings::default(), None, Some(Path::new("/h"))).unwrap();
        assert_eq!(resolved.device, DeviceSetting::Cpu);
        assert_eq!(resolved.model_id, "kev-latest");
        assert!(resolved.model_dir.is_none());
    }

    #[test]
    fn expands_tilde_and_takes_the_configured_model_name() {
        let settings = KevSettings {
            model_dir: Some("~/models/kev-0.8b".into()),
            device: Some(DeviceSetting::Metal),
        };
        let resolved = resolve(&settings, Some("kev-0.8b"), Some(Path::new("/home/t"))).unwrap();
        assert_eq!(
            resolved.model_dir.as_deref(),
            Some(Path::new("/home/t/models/kev-0.8b"))
        );
        assert_eq!(resolved.model_id, "kev-0.8b");
        assert_eq!(resolved.describe()["device"], "metal");
    }

    #[test]
    fn rejects_an_empty_model_name() {
        assert!(matches!(
            resolve(&KevSettings::default(), Some("  "), None),
            Err(HostError::Validation(_))
        ));
    }
}
