//! Typed `[backends.<id>.settings]` for `kind = "gliner2"`.
//!
//! The operator points `model_dir` at a GLiNER2.5 bundle directory from
//! `codesoda/gliner2-onnx` (or a copy of just its classification files). The
//! adapter reads `config.json`, `tokenizer.json`, `encoder.onnx` and
//! `classifier.onnx`; the extraction heads are never opened. With
//! `verify_sha256 = true` (default) those four files are checked against the
//! bundle's `export_manifest.json` before anything loads.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systemone_core::{HostError, paths::absolutize};

/// Checkpoints verified against the pinned upstream in gliner2-rs.
pub const PROFILES: [&str; 3] = ["small", "base", "multi"];
pub const DEFAULT_PROFILE: &str = "base";
/// Historical gliner2-rs default; keeps its measured CPU baseline.
pub const DEFAULT_INTRA_THREADS: usize = 4;
/// Labels that stand for `false` and `true` in a Noul question. Evaluated on
/// the held-out set in `docs/gliner2-evaluation.md`; wording changes answers.
pub const DEFAULT_NOUL_LABELS: [&str; 2] = ["no", "yes"];
/// Prompt markers reserved by gliner2-rs (`scores::RESERVED_MARKERS`). A
/// label containing one fails upstream request validation, so a config that
/// uses one must fail here at resolution — not per request with an HTTP 422
/// that blames the caller.
pub const RESERVED_LABEL_MARKERS: [&str; 6] = ["[P]", "[L]", "[E]", "[C]", "[R]", "[DESCRIPTION]"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceSetting {
    Cpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OptimizationSetting {
    Disable,
    Basic,
    Extended,
    All,
}

/// Raw settings as written by the operator.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Gliner2Settings {
    /// One of [`PROFILES`]. Defaults to `base`.
    pub profile: Option<String>,
    /// Bundle directory. Required to load.
    pub model_dir: Option<PathBuf>,
    /// Only `cpu` exists. Present so a config that asks for anything else
    /// fails at validation instead of silently running on the CPU.
    pub device: Option<DeviceSetting>,
    /// ONNX Runtime intra-op threads (default 4).
    pub intra_threads: Option<usize>,
    /// ONNX Runtime inter-op threads; unset keeps sequential execution.
    pub inter_threads: Option<usize>,
    /// Graph optimization level (default `all`).
    pub optimization: Option<OptimizationSetting>,
    /// Verify the four classification files against `export_manifest.json`
    /// (default `true`).
    pub verify_sha256: Option<bool>,
    /// `[false_label, true_label]` for Noul questions. Defaults to
    /// `["no", "yes"]`.
    pub noul_labels: Option<[String; 2]>,
}

/// Settings after defaults and validation.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSettings {
    pub profile: String,
    pub model_dir: Option<PathBuf>,
    pub intra_threads: usize,
    pub inter_threads: Option<usize>,
    pub optimization: OptimizationSetting,
    pub verify_sha256: bool,
    pub noul_labels: [String; 2],
    /// The identity this backend serves as: `gliner2.5-<profile>`.
    pub model_id: String,
}

impl ResolvedSettings {
    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "profile": self.profile,
            "model_dir": self.model_dir.as_ref().map(|path| path.display().to_string()),
            "device": "cpu",
            "intra_threads": self.intra_threads,
            "inter_threads": self.inter_threads,
            "optimization": optimization_name(self.optimization),
            "verify_sha256": self.verify_sha256,
            "noul_labels": self.noul_labels,
        })
    }

    /// Bundle directory name published for this profile.
    #[must_use]
    pub fn bundle_name(&self) -> String {
        format!("gliner2.5-{}-v1", self.profile)
    }
}

#[must_use]
pub const fn optimization_name(level: OptimizationSetting) -> &'static str {
    match level {
        OptimizationSetting::Disable => "disable",
        OptimizationSetting::Basic => "basic",
        OptimizationSetting::Extended => "extended",
        OptimizationSetting::All => "all",
    }
}

pub fn resolve(
    settings: &Gliner2Settings,
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
    let model_id = format!("gliner2.5-{profile}");
    if let Some(model) = model
        && model != model_id
    {
        return Err(HostError::validation(format!(
            "model {model:?} does not match settings.profile {profile:?}; gliner2 backends serve {model_id}; omit `model` or set it to {model_id:?}"
        )));
    }
    let intra_threads = settings.intra_threads.unwrap_or(DEFAULT_INTRA_THREADS);
    if intra_threads == 0 {
        return Err(HostError::validation(
            "settings.intra_threads must be positive".to_owned(),
        ));
    }
    if settings.inter_threads == Some(0) {
        return Err(HostError::validation(
            "settings.inter_threads must be positive when set".to_owned(),
        ));
    }
    let noul_labels = settings
        .noul_labels
        .clone()
        .unwrap_or_else(|| DEFAULT_NOUL_LABELS.map(str::to_owned));
    if noul_labels[0] == noul_labels[1] || noul_labels.iter().any(String::is_empty) {
        return Err(HostError::validation(
            "settings.noul_labels must be two distinct non-empty labels".to_owned(),
        ));
    }
    for label in &noul_labels {
        if let Some(marker) = RESERVED_LABEL_MARKERS
            .iter()
            .find(|marker| label.contains(*marker))
        {
            return Err(HostError::validation(format!(
                "settings.noul_labels label {label:?} contains the reserved prompt marker {marker}"
            )));
        }
    }
    let model_dir = settings
        .model_dir
        .as_ref()
        .map(|path| absolutize(path, home))
        .transpose()?;
    Ok(ResolvedSettings {
        profile,
        model_dir,
        intra_threads,
        inter_threads: settings.inter_threads,
        optimization: settings.optimization.unwrap_or(OptimizationSetting::All),
        verify_sha256: settings.verify_sha256.unwrap_or(true),
        noul_labels,
        model_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_ok(settings: Gliner2Settings) -> ResolvedSettings {
        resolve(&settings, None, Some(Path::new("/home/test"))).unwrap()
    }

    #[test]
    fn defaults_are_base_cpu_four_threads_verified() {
        let resolved = resolve_ok(Gliner2Settings::default());
        assert_eq!(resolved.profile, "base");
        assert_eq!(resolved.model_id, "gliner2.5-base");
        assert_eq!(resolved.bundle_name(), "gliner2.5-base-v1");
        assert_eq!(resolved.intra_threads, 4);
        assert_eq!(resolved.inter_threads, None);
        assert_eq!(resolved.optimization, OptimizationSetting::All);
        assert!(resolved.verify_sha256);
        assert_eq!(resolved.noul_labels, ["no", "yes"]);
        assert!(resolved.model_dir.is_none());
        assert_eq!(resolved.describe()["device"], "cpu");
    }

    #[test]
    fn rejects_unknown_profile_mismatched_model_and_bad_threads() {
        let bad = Gliner2Settings {
            profile: Some("large".into()),
            ..Gliner2Settings::default()
        };
        assert!(resolve(&bad, None, None).is_err());
        assert!(resolve(&Gliner2Settings::default(), Some("gliner2.5-small"), None).is_err());
        assert!(resolve(&Gliner2Settings::default(), Some("gliner2.5-base"), None).is_ok());
        let bad = Gliner2Settings {
            intra_threads: Some(0),
            ..Gliner2Settings::default()
        };
        assert!(resolve(&bad, None, None).is_err());
        let bad = Gliner2Settings {
            inter_threads: Some(0),
            ..Gliner2Settings::default()
        };
        assert!(resolve(&bad, None, None).is_err());
    }

    #[test]
    fn rejects_degenerate_noul_labels() {
        for labels in [["yes", "yes"], ["", "yes"]] {
            let bad = Gliner2Settings {
                noul_labels: Some(labels.map(str::to_owned)),
                ..Gliner2Settings::default()
            };
            assert!(resolve(&bad, None, None).is_err());
        }
    }

    #[test]
    fn rejects_noul_labels_with_reserved_prompt_markers() {
        for labels in [["no", "[L]yes"], ["[DESCRIPTION]", "yes"]] {
            let bad = Gliner2Settings {
                noul_labels: Some(labels.map(str::to_owned)),
                ..Gliner2Settings::default()
            };
            let error = resolve(&bad, None, None).unwrap_err();
            assert!(matches!(error, HostError::Validation(_)), "{error}");
            assert!(
                error.to_string().contains("reserved prompt marker"),
                "{error}"
            );
        }
    }

    #[cfg(feature = "gliner2")]
    #[test]
    fn reserved_markers_match_upstream() {
        assert_eq!(
            RESERVED_LABEL_MARKERS.as_slice(),
            gliner2_rs::scores::RESERVED_MARKERS
        );
    }

    #[test]
    fn unknown_device_is_rejected_by_serde() {
        let error = serde_json::from_value::<Gliner2Settings>(serde_json::json!({
            "device": "coreml"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("coreml"), "{error}");
    }

    #[test]
    fn tilde_paths_expand_against_home() {
        let resolved = resolve_ok(Gliner2Settings {
            model_dir: Some("~/models/gliner2.5-base-v1".into()),
            ..Gliner2Settings::default()
        });
        assert_eq!(
            resolved.model_dir.unwrap(),
            PathBuf::from("/home/test/models/gliner2.5-base-v1")
        );
    }
}
