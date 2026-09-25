//! SystemOne host adapter for Laya (ModernBERT/mmBERT encoders with
//! calibrated decision heads) through the `laya-core` runtime.
//!
//! The upstream crate owns tokenization, budgets, the network, calibration
//! and its own parity gate against the frozen Python goldens; this crate
//! only converts neutral SystemOne requests into `laya_core` requests and
//! evaluations back. Nothing here reads environment variables.
//!
//! Adapter policy is documented on [`convert`] (missing instructions →
//! empty string; single-option Choice answered deterministically; one
//! batched forward pass per request).

pub mod download;
pub mod settings;

#[cfg(feature = "laya")]
pub mod convert;
#[cfg(feature = "laya")]
pub mod host;

use systemone_core::{
    Backend, BackendDescription, BackendId, DecisionHost, HostError, ProviderKind,
};

pub use settings::{DeviceSetting, LayaSettings, PrecisionSetting, ResolvedSettings};

/// Which runtime backends this build links.
#[must_use]
pub const fn compiled_feature() -> &'static str {
    if cfg!(all(feature = "laya-metal", feature = "laya-cpu")) {
        "laya-metal+cpu"
    } else if cfg!(feature = "laya-metal") {
        "laya-metal"
    } else if cfg!(feature = "laya-cpu") {
        "laya-cpu"
    } else {
        "backend-disabled"
    }
}

/// A validated `kind = "laya"` backend instance. Constructing it resolves
/// settings but touches no files.
pub struct LayaBackend {
    id: BackendId,
    settings: ResolvedSettings,
    aliases: Vec<String>,
}

impl LayaBackend {
    pub fn new(
        id: BackendId,
        model: Option<&str>,
        aliases: Vec<String>,
        settings: &LayaSettings,
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
        if !cfg!(feature = "laya") {
            return Some(
                "this build has no laya runtime; rebuild with --features laya-cpu or laya-metal"
                    .to_owned(),
            );
        }
        if !settings::device_compiled(self.settings.device) {
            return Some(format!(
                "device {} requires a build with the laya-{} feature",
                settings::device_name(self.settings.device),
                settings::device_name(self.settings.device)
            ));
        }
        if self.settings.model_dir.is_none() {
            return Some(
                "settings.model_dir is not set; point it at a directory holding the pinned profile files"
                    .to_owned(),
            );
        }
        None
    }
}

impl Backend for LayaBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Laya
    }

    fn describe(&self) -> BackendDescription {
        let reason = self.unavailable_reason();
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::Laya,
            model: self.settings.model_id.clone(),
            available: reason.is_none(),
            unavailable_reason: reason,
            settings: self.settings.describe(),
        }
    }

    #[cfg(feature = "laya")]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(HostError::unavailable(reason));
        }
        host::LayaHost::load(&self.settings, &self.aliases)
            .map(|host| Box::new(host) as Box<dyn DecisionHost>)
    }

    #[cfg(not(feature = "laya"))]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        let _ = &self.aliases;
        Err(HostError::unavailable(
            self.unavailable_reason()
                .unwrap_or_else(|| "laya backend unavailable".to_owned()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(settings: LayaSettings) -> LayaBackend {
        LayaBackend::new(
            BackendId::new("laya").unwrap(),
            None,
            vec![],
            &settings,
            Some(std::path::Path::new("/home/test")),
        )
        .unwrap()
    }

    #[test]
    fn describes_without_loading_and_names_the_missing_piece() {
        let description = backend(LayaSettings::default()).describe();
        assert_eq!(description.kind, ProviderKind::Laya);
        assert_eq!(description.model, "laya-english");
        assert!(!description.available);
        let reason = description.unavailable_reason.unwrap();
        if cfg!(feature = "laya-cpu") {
            assert!(reason.contains("model_dir"), "{reason}");
        } else {
            assert!(reason.contains("laya"), "{reason}");
        }
        assert_eq!(description.settings["device"], "cpu");
    }

    #[test]
    fn model_dir_alone_makes_a_compiled_device_available() {
        let description = backend(LayaSettings {
            model_dir: Some("/models/english".into()),
            ..LayaSettings::default()
        })
        .describe();
        assert_eq!(description.available, cfg!(feature = "laya-cpu"));
    }

    #[test]
    fn model_store_is_unsupported() {
        assert!(
            backend(LayaSettings::default())
                .model_store()
                .require("model store")
                .is_err()
        );
    }

    #[test]
    fn load_fails_explicitly_when_not_loadable() {
        assert!(matches!(
            backend(LayaSettings::default()).load(),
            Err(HostError::Unavailable(_))
        ));
    }
}
