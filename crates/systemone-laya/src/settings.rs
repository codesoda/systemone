//! Typed `[backends.<id>.settings]` for `kind = "laya"`.
//!
//! Laya has no model registry or downloader in this adapter: the operator
//! points `model_dir` at a directory holding one pinned profile
//! (`model.safetensors`, `rl_agent_config.json`, `encoder/config.json`,
//! `tokenizer/tokenizer.json`, `tokenizer/tokenizer_config.json`). The
//! runtime verifies every file's SHA-256 against its embedded manifest
//! before it loads anything.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systemone_core::{HostError, paths::absolutize};

/// Profiles published for the pinned checkpoint revision.
pub const PROFILES: [&str; 3] = ["english", "multilingual", "typed-decisions"];
pub const DEFAULT_PROFILE: &str = "english";
/// `~/Library/Caches/laya-rs` on macOS, `~/.cache/laya-rs` elsewhere.
pub const CACHE_DIR_NAME: &str = "laya-rs";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceSetting {
    Cpu,
    Metal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrecisionSetting {
    F32,
    /// Metal only. A separately measured configuration in laya-rs; opt in
    /// knowingly.
    F16,
}

/// Raw settings as written by the operator.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayaSettings {
    /// One of [`PROFILES`]. Defaults to `english`.
    pub profile: Option<String>,
    /// Directory holding the profile files. Required to load.
    pub model_dir: Option<PathBuf>,
    /// Where the Metal kernel library is installed. Defaults to
    /// `~/Library/Caches/laya-rs` (macOS) or `~/.cache/laya-rs`.
    pub cache_dir: Option<PathBuf>,
    /// `cpu` (Candle) or `metal` (MLX). Defaults to `cpu`.
    pub device: Option<DeviceSetting>,
    /// `f32` (default) or `f16` (Metal only).
    pub precision: Option<PrecisionSetting>,
    /// Verify every model file's SHA-256 at load (default `true`). `false`
    /// checks sizes only; use it when the directory was verified elsewhere.
    pub verify_sha256: Option<bool>,
}

/// Settings after defaults and validation. Paths are absolute where a home
/// directory was available.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSettings {
    pub profile: String,
    pub model_dir: Option<PathBuf>,
    pub cache_dir: PathBuf,
    pub device: DeviceSetting,
    pub precision: PrecisionSetting,
    pub verify_sha256: bool,
    /// The identity this backend serves as: `laya-<profile>`.
    pub model_id: String,
}

impl ResolvedSettings {
    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "profile": self.profile,
            "model_dir": self.model_dir.as_ref().map(|path| path.display().to_string()),
            "cache_dir": self.cache_dir.display().to_string(),
            "device": device_name(self.device),
            "precision": precision_name(self.precision),
            "verify_sha256": self.verify_sha256,
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

#[must_use]
pub const fn precision_name(precision: PrecisionSetting) -> &'static str {
    match precision {
        PrecisionSetting::F32 => "f32",
        PrecisionSetting::F16 => "f16",
    }
}

/// Whether this build links the runtime for `device`.
#[must_use]
pub const fn device_compiled(device: DeviceSetting) -> bool {
    match device {
        DeviceSetting::Cpu => cfg!(feature = "laya-cpu"),
        DeviceSetting::Metal => cfg!(feature = "laya-metal"),
    }
}

pub fn resolve(
    settings: &LayaSettings,
    model: Option<&str>,
    home: Option<&Path>,
) -> Result<ResolvedSettings, HostError> {
    let profile = settings
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    if !PROFILES.contains(&profile.as_str()) {
        return Err(HostError::validation(format!(
            "settings.profile {profile:?} is not one of {}",
            PROFILES.join(", ")
        )));
    }
    let model_id = format!("laya-{profile}");
    if let Some(model) = model
        && model != model_id
    {
        return Err(HostError::validation(format!(
            "model {model:?} does not match settings.profile {profile:?}; laya backends serve {model_id}; omit `model` or set it to {model_id:?}"
        )));
    }
    let device = settings.device.unwrap_or(DeviceSetting::Cpu);
    let precision = settings.precision.unwrap_or(PrecisionSetting::F32);
    if precision == PrecisionSetting::F16 && device != DeviceSetting::Metal {
        return Err(HostError::validation(
            "settings.precision f16 requires settings.device = \"metal\"".to_owned(),
        ));
    }
    let model_dir = settings
        .model_dir
        .as_ref()
        .map(|path| absolutize(path, home))
        .transpose()?;
    let cache_dir = match &settings.cache_dir {
        Some(path) => absolutize(path, home)?,
        None => default_cache_dir(home)?,
    };
    Ok(ResolvedSettings {
        profile,
        model_dir,
        cache_dir,
        device,
        precision,
        verify_sha256: settings.verify_sha256.unwrap_or(true),
        model_id,
    })
}

fn default_cache_dir(home: Option<&Path>) -> Result<PathBuf, HostError> {
    let home = home.ok_or_else(|| {
        HostError::validation(
            "settings.cache_dir is required when no home directory is available".to_owned(),
        )
    })?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library").join("Caches").join(CACHE_DIR_NAME)
    } else {
        home.join(".cache").join(CACHE_DIR_NAME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_english_cpu_f32_with_platform_cache_dir() {
        let resolved = resolve(
            &LayaSettings::default(),
            None,
            Some(Path::new("/home/test")),
        )
        .unwrap();
        assert_eq!(resolved.profile, "english");
        assert_eq!(resolved.model_id, "laya-english");
        assert_eq!(resolved.device, DeviceSetting::Cpu);
        assert_eq!(resolved.precision, PrecisionSetting::F32);
        assert!(resolved.verify_sha256);
        assert!(resolved.model_dir.is_none());
        let expected = if cfg!(target_os = "macos") {
            "/home/test/Library/Caches/laya-rs"
        } else {
            "/home/test/.cache/laya-rs"
        };
        assert_eq!(resolved.cache_dir, Path::new(expected));
    }

    #[test]
    fn rejects_unknown_profile_mismatched_model_and_f16_on_cpu() {
        let bad_profile = LayaSettings {
            profile: Some("french".into()),
            ..LayaSettings::default()
        };
        assert!(matches!(
            resolve(&bad_profile, None, None),
            Err(HostError::Validation(_))
        ));
        let error = resolve(&LayaSettings::default(), Some("laya-multilingual"), None).unwrap_err();
        assert!(error.to_string().contains("laya-english"), "{error}");
        let f16_cpu = LayaSettings {
            precision: Some(PrecisionSetting::F16),
            ..LayaSettings::default()
        };
        assert!(resolve(&f16_cpu, None, Some(Path::new("/h"))).is_err());
    }

    #[test]
    fn expands_tilde_and_keeps_absolute_paths() {
        let settings = LayaSettings {
            model_dir: Some("~/models/laya/english".into()),
            cache_dir: Some("/var/cache/laya".into()),
            device: Some(DeviceSetting::Metal),
            precision: Some(PrecisionSetting::F16),
            ..LayaSettings::default()
        };
        let resolved =
            resolve(&settings, Some("laya-english"), Some(Path::new("/home/t"))).unwrap();
        assert_eq!(
            resolved.model_dir.as_deref(),
            Some(Path::new("/home/t/models/laya/english"))
        );
        assert_eq!(resolved.cache_dir, Path::new("/var/cache/laya"));
        assert_eq!(resolved.describe()["precision"], "f16");
    }
}
