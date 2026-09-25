//! Typed `[backends.<id>.settings]` schema for `kind = "openjev"`.
//!
//! Every value SystemOne passes to the engine is explicit here. Nothing is
//! read from `OPENJEV_*` variables or `~/.openjev`; the model cache root is a
//! setting with a documented default.

use std::path::PathBuf;

use openjev_core::{Device, GpuLayersRequested, PromptProfile};
use openjev_llama::{ModelRegistry, ModelSpec, validate_hub_identity, validate_sha256};
use serde::{Deserialize, Serialize};
use systemone_core::HostError;

/// Default model cache root relative to the home directory. Shared with the
/// standalone OpenJev cache so previously verified weights are reused.
pub const DEFAULT_CACHE_SUBDIRECTORY: &str = ".cache/openjev";
/// Validated v1 upper bound for `max_sequences`.
pub const MAX_SEQUENCES_LIMIT: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceSetting {
    Cpu,
    Metal,
    Cuda,
}

/// `gpu_layers` as written: a TOML integer (`28`) or a string (`"28"`,
/// `"all"`). Both forms are accepted so a count works from a config file,
/// `--set` and `SYSTEMONE_*` alike.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GpuLayersSetting {
    Count(u32),
    Text(String),
}

impl GpuLayersSetting {
    fn as_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Count(count) => std::borrow::Cow::Owned(count.to_string()),
            Self::Text(text) => std::borrow::Cow::Borrowed(text),
        }
    }
}

/// Raw settings as written in configuration. All fields optional so a file
/// may set only what differs from the defaults.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenJevSettings {
    /// Model cache root. Defaults to `~/.cache/openjev`.
    pub cache_dir: Option<PathBuf>,
    /// Never download; fail on a cache miss.
    #[serde(default)]
    pub offline: bool,
    /// Expected SHA-256 for a custom local/Hub artifact.
    pub model_sha256: Option<String>,
    /// Prompt template profile for custom artifacts (`qwen3`, `qwen3.5`,
    /// `minicpm5`). Registered models carry their own.
    pub template_profile: Option<String>,
    pub device: Option<DeviceSetting>,
    /// `"all"` or a layer count (`28` or `"28"`). Defaults to 0 on CPU, all
    /// on accelerators.
    pub gpu_layers: Option<GpuLayersSetting>,
    pub threads: Option<u32>,
    pub n_ctx: Option<u32>,
    pub max_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub n_batch: Option<u32>,
    pub n_ubatch: Option<u32>,
    pub max_sequences: Option<u32>,
    /// Fail multi-question requests instead of falling back to serial
    /// full-prompt execution when no passing shared probe receipt exists.
    #[serde(default)]
    pub require_shared: bool,
}

/// Fully resolved engine configuration.
#[derive(Clone, Debug)]
pub struct ResolvedSettings {
    pub model: ModelSpec,
    pub model_label: String,
    pub cache_dir: PathBuf,
    pub offline: bool,
    pub device: Device,
    pub gpu_layers: GpuLayersRequested,
    pub threads: u32,
    pub n_ctx: Option<u32>,
    pub max_tokens: u32,
    pub max_context_tokens: u32,
    pub n_batch: u32,
    pub n_ubatch: u32,
    pub max_sequences: u32,
    pub require_shared: bool,
}

impl ResolvedSettings {
    /// Non-secret JSON summary for `s1 backends`.
    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model_label,
            "cache_dir": self.cache_dir.display().to_string(),
            "offline": self.offline,
            "device": device_name(self.device),
            "gpu_layers": match self.gpu_layers {
                GpuLayersRequested::All => "all".to_owned(),
                GpuLayersRequested::Count(count) => count.to_string(),
            },
            "threads": self.threads,
            "n_ctx": self.n_ctx,
            "max_tokens": self.max_tokens,
            "max_context_tokens": self.max_context_tokens,
            "n_batch": self.n_batch,
            "n_ubatch": self.n_ubatch,
            "max_sequences": self.max_sequences,
            "require_shared": self.require_shared,
        })
    }
}

pub(crate) const fn device_name(device: Device) -> &'static str {
    match device {
        Device::Cpu => "cpu",
        Device::Metal => "metal",
        Device::Cuda => "cuda",
    }
}

/// Whether this build can execute on the requested device.
pub(crate) const fn device_compiled(device: Device) -> bool {
    match device {
        Device::Cpu => cfg!(feature = "native"),
        Device::Metal => cfg!(feature = "metal"),
        Device::Cuda => cfg!(feature = "cuda"),
    }
}

pub fn resolve(
    settings: &OpenJevSettings,
    model: Option<&str>,
    home: Option<&std::path::Path>,
) -> Result<ResolvedSettings, HostError> {
    let registry =
        ModelRegistry::bundled().map_err(|error| HostError::internal(error.to_string()))?;
    let model_label = model.unwrap_or(registry.default_model()).to_owned();
    let model = parse_model_spec(settings, &model_label, &registry)?;
    let cache_dir = match &settings.cache_dir {
        Some(path) => path.clone(),
        None => home
            .ok_or_else(|| {
                HostError::validation(
                    "settings.cache_dir is required when the home directory cannot be resolved",
                )
            })?
            .join(DEFAULT_CACHE_SUBDIRECTORY),
    };
    let device = match settings.device {
        Some(DeviceSetting::Cpu) | None => Device::Cpu,
        Some(DeviceSetting::Metal) => Device::Metal,
        Some(DeviceSetting::Cuda) => Device::Cuda,
    };
    let gpu_layers = parse_gpu_layers(
        settings
            .gpu_layers
            .as_ref()
            .map(GpuLayersSetting::as_text)
            .as_deref(),
        device,
    )?;
    let threads = settings.threads.unwrap_or_else(default_threads);
    let max_tokens = settings.max_tokens.unwrap_or(4096);
    let max_context_tokens = settings.max_context_tokens.unwrap_or(32_768);
    let n_batch = settings.n_batch.unwrap_or(512);
    let n_ubatch = settings.n_ubatch.unwrap_or(512);
    let max_sequences = settings.max_sequences.unwrap_or(32);
    for (name, value) in [
        ("threads", threads),
        ("max_tokens", max_tokens),
        ("max_context_tokens", max_context_tokens),
        ("n_batch", n_batch),
        ("n_ubatch", n_ubatch),
        ("max_sequences", max_sequences),
    ] {
        if value == 0 {
            return Err(HostError::validation(format!(
                "settings.{name} must be positive"
            )));
        }
    }
    if max_sequences > MAX_SEQUENCES_LIMIT {
        return Err(HostError::validation(format!(
            "settings.max_sequences must not exceed the validated v1 limit of {MAX_SEQUENCES_LIMIT}"
        )));
    }
    if settings.n_ctx == Some(0) {
        return Err(HostError::validation("settings.n_ctx must be positive"));
    }
    if settings
        .n_ctx
        .is_some_and(|n_ctx| n_ctx > max_context_tokens)
    {
        return Err(HostError::validation(
            "settings.n_ctx must not exceed settings.max_context_tokens",
        ));
    }
    if settings.n_ctx.is_none() && max_context_tokens < 4096 {
        return Err(HostError::validation(
            "automatic context allocation requires settings.max_context_tokens >= 4096",
        ));
    }
    if n_ubatch > n_batch {
        return Err(HostError::validation(
            "settings.n_ubatch must not exceed settings.n_batch",
        ));
    }
    if threads > i32::MAX as u32 {
        return Err(HostError::validation("settings.threads exceeds i32::MAX"));
    }
    Ok(ResolvedSettings {
        model,
        model_label,
        cache_dir,
        offline: settings.offline,
        device,
        gpu_layers,
        threads,
        n_ctx: settings.n_ctx,
        max_tokens,
        max_context_tokens,
        n_batch,
        n_ubatch,
        max_sequences,
        require_shared: settings.require_shared,
    })
}

fn parse_model_spec(
    settings: &OpenJevSettings,
    value: &str,
    registry: &ModelRegistry,
) -> Result<ModelSpec, HostError> {
    if registry.resolve(value).is_ok() {
        if settings.model_sha256.is_some() || settings.template_profile.is_some() {
            return Err(HostError::validation(
                "settings.model_sha256 and settings.template_profile are only valid for custom artifacts",
            ));
        }
        return Ok(ModelSpec::RegistryId(value.to_owned()));
    }
    let profile = settings
        .template_profile
        .as_deref()
        .ok_or_else(|| {
            HostError::validation(
                "custom model artifacts require settings.template_profile = qwen3 | qwen3.5 | minicpm5",
            )
        })?
        .parse::<PromptProfile>()
        .map_err(|error| HostError::validation(format!("settings.template_profile: {error}")))?;
    if let Some(rest) = value.strip_prefix("hf:") {
        let (repo, suffix) = rest.split_once('@').ok_or_else(|| {
            HostError::validation("custom Hub model must be hf:OWNER/REPO@40HEX:FILENAME")
        })?;
        let (revision, file) = suffix.split_once(':').ok_or_else(|| {
            HostError::validation("custom Hub model must be hf:OWNER/REPO@40HEX:FILENAME")
        })?;
        validate_hub_identity(repo, revision, file)
            .map_err(|error| HostError::validation(error.to_string()))?;
        if !file.to_ascii_lowercase().ends_with(".gguf") {
            return Err(HostError::validation(
                "custom Hub model filename must end in .gguf",
            ));
        }
        let expected_sha256 = settings.model_sha256.clone().ok_or_else(|| {
            HostError::validation("custom Hub models require settings.model_sha256")
        })?;
        validate_sha256(&expected_sha256, "settings.model_sha256")
            .map_err(|error| HostError::validation(error.to_string()))?;
        return Ok(ModelSpec::Hub {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            file: file.to_owned(),
            expected_sha256,
            profile,
        });
    }
    let path = PathBuf::from(value);
    let looks_local = path.is_absolute()
        || value.contains('/')
        || value.starts_with('.')
        || value.to_ascii_lowercase().ends_with(".gguf");
    if !looks_local || !value.to_ascii_lowercase().ends_with(".gguf") {
        return Err(HostError::validation(format!(
            "unknown registered model {value:?}; local models must be a path ending in .gguf"
        )));
    }
    if let Some(expected) = &settings.model_sha256 {
        validate_sha256(expected, "settings.model_sha256")
            .map_err(|error| HostError::validation(error.to_string()))?;
    }
    Ok(ModelSpec::Local {
        path,
        expected_sha256: settings.model_sha256.clone(),
        profile,
    })
}

fn parse_gpu_layers(value: Option<&str>, device: Device) -> Result<GpuLayersRequested, HostError> {
    let requested = match value {
        None if device == Device::Cpu => GpuLayersRequested::Count(0),
        None | Some("all") => GpuLayersRequested::All,
        Some(value) => GpuLayersRequested::Count(value.parse::<u32>().map_err(|error| {
            HostError::validation(format!("invalid settings.gpu_layers {value:?}: {error}"))
        })?),
    };
    if device == Device::Cpu && requested != GpuLayersRequested::Count(0) {
        return Err(HostError::validation(
            "CPU execution requires settings.gpu_layers = 0; KQV/op offload is disabled",
        ));
    }
    Ok(requested)
}

fn default_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|value| u32::try_from(value.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/test")
    }

    #[test]
    fn gpu_layers_accepts_an_integer_or_a_string() {
        for raw in [
            serde_json::json!({"device": "metal", "gpu_layers": 28}),
            serde_json::json!({"device": "metal", "gpu_layers": "28"}),
        ] {
            let settings: OpenJevSettings = serde_json::from_value(raw).unwrap();
            let resolved = resolve(&settings, None, Some(&home())).unwrap();
            assert_eq!(resolved.gpu_layers, GpuLayersRequested::Count(28));
        }
        let settings: OpenJevSettings =
            serde_json::from_value(serde_json::json!({"device": "cuda", "gpu_layers": "all"}))
                .unwrap();
        assert_eq!(
            resolve(&settings, None, Some(&home())).unwrap().gpu_layers,
            GpuLayersRequested::All
        );
        let settings: OpenJevSettings =
            serde_json::from_value(serde_json::json!({"device": "metal", "gpu_layers": "many"}))
                .unwrap();
        assert!(resolve(&settings, None, Some(&home())).is_err());
    }

    #[test]
    fn defaults_resolve_registry_model_and_cache() {
        let resolved = resolve(&OpenJevSettings::default(), None, Some(&home())).unwrap();
        assert!(matches!(resolved.model, ModelSpec::RegistryId(_)));
        assert_eq!(resolved.cache_dir, home().join(DEFAULT_CACHE_SUBDIRECTORY));
        assert_eq!(resolved.device, Device::Cpu);
        assert_eq!(resolved.gpu_layers, GpuLayersRequested::Count(0));
        assert_eq!(resolved.max_sequences, 32);
    }

    #[test]
    fn rejects_invalid_combinations() {
        let bad = |settings: OpenJevSettings, model: Option<&str>| {
            resolve(&settings, model, Some(&home())).unwrap_err()
        };
        assert!(matches!(
            bad(
                OpenJevSettings {
                    max_sequences: Some(65),
                    ..Default::default()
                },
                None
            ),
            HostError::Validation(_)
        ));
        assert!(matches!(
            bad(
                OpenJevSettings {
                    gpu_layers: Some(GpuLayersSetting::Text("all".into())),
                    ..Default::default()
                },
                None
            ),
            HostError::Validation(_)
        ));
        assert!(matches!(
            bad(
                OpenJevSettings {
                    n_ubatch: Some(1024),
                    ..Default::default()
                },
                None
            ),
            HostError::Validation(_)
        ));
        assert!(matches!(
            bad(OpenJevSettings::default(), Some("not-a-model")),
            HostError::Validation(_)
        ));
        assert!(matches!(
            bad(OpenJevSettings::default(), Some("./custom.gguf")),
            HostError::Validation(message) if message.contains("template_profile")
        ));
    }

    #[test]
    fn custom_local_artifact_requires_profile() {
        let resolved = resolve(
            &OpenJevSettings {
                template_profile: Some("qwen3".into()),
                ..Default::default()
            },
            Some("./custom.gguf"),
            Some(&home()),
        )
        .unwrap();
        assert!(matches!(resolved.model, ModelSpec::Local { .. }));
    }

    #[test]
    fn unknown_settings_keys_are_rejected() {
        let error =
            serde_json::from_value::<OpenJevSettings>(serde_json::json!({"bogus": 1})).unwrap_err();
        assert!(error.to_string().contains("bogus"));
    }
}
