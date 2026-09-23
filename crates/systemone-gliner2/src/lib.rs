//! SystemOne host adapter for GLiNER2.5 classification through gliner2-rs
//! and ONNX Runtime (CPU).
//!
//! The upstream crate owns the prompt template, tokenizer, encoder, label
//! state gathering, temperature and its own parity gate against the pinned
//! Python runtime; this crate only maps neutral SystemOne questions onto
//! classification requests and full softmax distributions back. Nothing
//! here reads environment variables.
//!
//! Adapter policy is documented on [`convert`]. Extraction (entities,
//! relations, JSON) stays upstream; it is outside the decision contract.

pub mod settings;

#[cfg(feature = "gliner2")]
pub mod convert;
#[cfg(feature = "gliner2")]
pub mod host;

use systemone_core::{
    Backend, BackendDescription, BackendId, DecisionHost, HostError, ProviderKind,
};

pub use settings::{Gliner2Settings, OptimizationSetting, ResolvedSettings};

/// Which runtime this build links.
#[must_use]
pub const fn compiled_feature() -> &'static str {
    if cfg!(feature = "gliner2") {
        "gliner2-cpu"
    } else {
        "backend-disabled"
    }
}

/// A validated `kind = "gliner2"` backend instance. Constructing it resolves
/// settings but touches no files.
pub struct Gliner2Backend {
    id: BackendId,
    settings: ResolvedSettings,
    aliases: Vec<String>,
}

impl Gliner2Backend {
    pub fn new(
        id: BackendId,
        model: Option<&str>,
        aliases: Vec<String>,
        settings: &Gliner2Settings,
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
        if !cfg!(feature = "gliner2") {
            return Some(
                "this build has no gliner2 runtime; rebuild with --features gliner2".to_owned(),
            );
        }
        if self.settings.model_dir.is_none() {
            return Some(
                "settings.model_dir is not set; point it at a GLiNER2.5 bundle directory"
                    .to_owned(),
            );
        }
        None
    }
}

impl Backend for Gliner2Backend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Gliner2
    }

    fn describe(&self) -> BackendDescription {
        let reason = self.unavailable_reason();
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::Gliner2,
            model: self.settings.model_id.clone(),
            available: reason.is_none(),
            unavailable_reason: reason,
            settings: self.settings.describe(),
        }
    }

    #[cfg(feature = "gliner2")]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(HostError::unavailable(reason));
        }
        host::Gliner2Host::load(&self.settings, &self.aliases)
            .map(|host| Box::new(host) as Box<dyn DecisionHost>)
    }

    #[cfg(not(feature = "gliner2"))]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        let _ = &self.aliases;
        Err(HostError::unavailable(
            self.unavailable_reason()
                .unwrap_or_else(|| "gliner2 backend unavailable".to_owned()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(settings: Gliner2Settings) -> Gliner2Backend {
        Gliner2Backend::new(
            BackendId::new("gliner2").unwrap(),
            None,
            vec![],
            &settings,
            Some(std::path::Path::new("/home/test")),
        )
        .unwrap()
    }

    #[test]
    fn describes_without_loading_and_names_the_missing_piece() {
        let description = backend(Gliner2Settings::default()).describe();
        assert_eq!(description.kind, ProviderKind::Gliner2);
        assert_eq!(description.model, "gliner2.5-base");
        assert!(!description.available);
        let reason = description.unavailable_reason.unwrap();
        if cfg!(feature = "gliner2") {
            assert!(reason.contains("model_dir"), "{reason}");
        } else {
            assert!(reason.contains("gliner2"), "{reason}");
        }
        assert_eq!(description.settings["device"], "cpu");
    }

    #[test]
    fn model_dir_alone_makes_a_compiled_build_available() {
        let description = backend(Gliner2Settings {
            model_dir: Some("/models/gliner2.5-base-v1".into()),
            ..Gliner2Settings::default()
        })
        .describe();
        assert_eq!(description.available, cfg!(feature = "gliner2"));
    }

    #[test]
    fn load_fails_explicitly_when_not_loadable() {
        assert!(matches!(
            backend(Gliner2Settings::default()).load(),
            Err(HostError::Unavailable(_))
        ));
    }
}
