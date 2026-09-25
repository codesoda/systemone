//! SystemOne host adapter for Kev (pointer-head decision models on Qwen
//! bases) through the `kev-core` runtime.
//!
//! The upstream crate owns the tokenizer and special-token escaping,
//! request encoding, row isolation, the LoRA merge, the pointer head and
//! temperature calibration, and its own parity gates against the frozen
//! Python goldens; this crate only converts neutral SystemOne requests into
//! `kev_core` requests and evaluations back. Nothing here reads environment
//! variables.
//!
//! Adapter policy is documented on [`convert`] (missing instructions →
//! JSON null → upstream's empty string; noul descriptions → upstream's
//! `criteria` object; `usage.output_tokens` = upstream's serialised-answer
//! token count; one batched pass per request).

pub mod download;
pub mod settings;

pub mod convert;
#[cfg(feature = "kev")]
pub mod host;

use systemone_core::{
    Backend, BackendDescription, BackendId, DecisionHost, HostError, ProviderKind,
};

pub use settings::{DeviceSetting, KevSettings, ResolvedSettings};

/// Which runtime backends this build links.
#[must_use]
pub const fn compiled_feature() -> &'static str {
    if cfg!(all(feature = "kev-metal", feature = "kev-cpu")) {
        "kev-metal+cpu"
    } else if cfg!(feature = "kev-metal") {
        "kev-metal"
    } else if cfg!(feature = "kev-cpu") {
        "kev-cpu"
    } else {
        "backend-disabled"
    }
}

/// A validated `kind = "kev"` backend instance. Constructing it resolves
/// settings but touches no files.
pub struct KevBackend {
    id: BackendId,
    settings: ResolvedSettings,
    aliases: Vec<String>,
}

impl KevBackend {
    pub fn new(
        id: BackendId,
        model: Option<&str>,
        aliases: Vec<String>,
        settings: &KevSettings,
        home: Option<&std::path::Path>,
    ) -> Result<Self, HostError> {
        let settings = settings::resolve(settings, model, home)?;
        Ok(Self {
            id,
            settings,
            aliases,
        })
    }

    #[must_use]
    pub const fn settings(&self) -> &ResolvedSettings {
        &self.settings
    }

    fn unavailable_reason(&self) -> Option<String> {
        if !cfg!(feature = "kev") {
            return Some(
                "this build has no kev runtime; rebuild with --features kev-cpu or kev-metal"
                    .to_owned(),
            );
        }
        if !settings::device_compiled(self.settings.device) {
            return Some(format!(
                "device {} requires a build with the kev-{} feature",
                settings::device_name(self.settings.device),
                settings::device_name(self.settings.device)
            ));
        }
        if self.settings.model_dir.is_none() {
            return Some(
                "settings.model_dir is not set; point it at a directory holding the assembled checkpoint (base/, adapter/, head.safetensors, head.meta.json)"
                    .to_owned(),
            );
        }
        None
    }
}

impl Backend for KevBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Kev
    }

    fn describe(&self) -> BackendDescription {
        let reason = self.unavailable_reason();
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::Kev,
            model: self.settings.model_id.clone(),
            available: reason.is_none(),
            unavailable_reason: reason,
            settings: self.settings.describe(),
        }
    }

    #[cfg(feature = "kev")]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(HostError::unavailable(reason));
        }
        host::KevHost::load(&self.settings, &self.aliases)
            .map(|host| Box::new(host) as Box<dyn DecisionHost>)
    }

    #[cfg(not(feature = "kev"))]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        let _ = &self.aliases;
        Err(HostError::unavailable(
            self.unavailable_reason()
                .unwrap_or_else(|| "kev backend unavailable".to_owned()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(settings: KevSettings) -> KevBackend {
        KevBackend::new(
            BackendId::new("kev").unwrap(),
            None,
            vec![],
            &settings,
            Some(std::path::Path::new("/home/test")),
        )
        .unwrap()
    }

    #[test]
    fn describes_without_loading_and_names_the_missing_piece() {
        let description = backend(KevSettings::default()).describe();
        assert_eq!(description.kind, ProviderKind::Kev);
        assert_eq!(description.model, "kev-latest");
        assert!(!description.available);
        let reason = description.unavailable_reason.unwrap();
        if cfg!(feature = "kev-cpu") {
            assert!(reason.contains("model_dir"), "{reason}");
        } else {
            assert!(reason.contains("kev"), "{reason}");
        }
        assert_eq!(description.settings["device"], "cpu");
    }

    #[test]
    fn model_dir_alone_makes_a_compiled_device_available() {
        let description = backend(KevSettings {
            model_dir: Some("/models/kev-0.6b".into()),
            ..KevSettings::default()
        })
        .describe();
        assert_eq!(description.available, cfg!(feature = "kev-cpu"));
    }

    #[test]
    fn metal_device_requires_the_metal_feature() {
        let description = backend(KevSettings {
            model_dir: Some("/models/kev-0.8b".into()),
            device: Some(DeviceSetting::Metal),
        })
        .describe();
        assert_eq!(description.available, cfg!(feature = "kev-metal"));
    }

    #[test]
    fn model_store_is_unsupported() {
        assert!(
            backend(KevSettings::default())
                .model_store()
                .require("model store")
                .is_err()
        );
    }

    #[test]
    fn load_fails_explicitly_when_not_loadable() {
        assert!(matches!(
            backend(KevSettings::default()).load(),
            Err(HostError::Unavailable(_))
        ));
    }
}
