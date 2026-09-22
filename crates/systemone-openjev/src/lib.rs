//! SystemOne host adapter for OpenJev (frozen-LLM next-token option scoring
//! through `openjev-core` and `openjev-llama`).
//!
//! The upstream libraries own tokenization, prompts, model loading and
//! numerical parity gates; this crate only converts neutral SystemOne
//! requests into OpenJev decisions, drives the engine, and converts readouts
//! back. Nothing here depends on `OPENJEV_*` environment variables.

pub mod convert;
pub mod probe;
pub mod settings;
pub mod store;

#[cfg(feature = "native")]
pub mod host;

use systemone_core::{
    Backend, BackendDescription, BackendId, DecisionHost, Extension, HostError, ModelStore,
    ProviderKind,
};

/// Re-exported so callers can name upstream types (probe modes, receipts).
pub use openjev_llama;
pub use settings::{OpenJevSettings, ResolvedSettings};
pub use store::OpenJevModelStore;

/// Which accelerator this build was compiled for.
#[must_use]
pub const fn compiled_feature() -> &'static str {
    if cfg!(feature = "cuda") {
        "native-cuda"
    } else if cfg!(feature = "metal") {
        "native-metal"
    } else if cfg!(feature = "native") {
        "native-cpu"
    } else {
        "backend-disabled"
    }
}

/// A validated `kind = "openjev"` backend instance. Constructing it resolves
/// settings but loads nothing.
pub struct OpenJevBackend {
    id: BackendId,
    settings: ResolvedSettings,
    aliases: Vec<String>,
}

impl OpenJevBackend {
    pub fn new(
        id: BackendId,
        model: Option<&str>,
        aliases: Vec<String>,
        settings: &OpenJevSettings,
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
        if !cfg!(feature = "native") {
            return Some(
                "this build has no openjev native backend; rebuild with --features native, metal, or cuda"
                    .to_owned(),
            );
        }
        if !settings::device_compiled(self.settings.device) {
            return Some(format!(
                "device {} requires a build with the matching openjev feature",
                settings::device_name(self.settings.device)
            ));
        }
        None
    }
}

impl Backend for OpenJevBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenJev
    }

    fn describe(&self) -> BackendDescription {
        let reason = self.unavailable_reason();
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::OpenJev,
            model: self.settings.model_label.clone(),
            available: reason.is_none(),
            unavailable_reason: reason,
            settings: self.settings.describe(),
        }
    }

    #[cfg(feature = "native")]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(HostError::unavailable(reason));
        }
        host::OpenJevHost::load(&self.settings, &self.aliases)
            .map(|host| Box::new(host) as Box<dyn DecisionHost>)
    }

    #[cfg(not(feature = "native"))]
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        let _ = &self.aliases;
        Err(HostError::unavailable(
            self.unavailable_reason()
                .unwrap_or_else(|| "openjev backend unavailable".to_owned()),
        ))
    }

    fn model_store(&self) -> Extension<Box<dyn ModelStore>> {
        Extension::Supported(Box::new(OpenJevModelStore::new(self.settings.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> OpenJevBackend {
        OpenJevBackend::new(
            BackendId::new("local").unwrap(),
            None,
            vec![],
            &OpenJevSettings::default(),
            Some(std::path::Path::new("/home/test")),
        )
        .unwrap()
    }

    #[test]
    fn describes_without_loading_and_reports_build_availability() {
        let description = backend().describe();
        assert_eq!(description.kind, ProviderKind::OpenJev);
        assert_eq!(
            description.model,
            openjev_llama::ModelRegistry::bundled()
                .unwrap()
                .default_model()
        );
        assert_eq!(description.available, cfg!(feature = "native"));
        assert_eq!(description.settings["device"], "cpu");
    }

    #[test]
    fn model_store_is_supported_and_lists_registry_without_network() {
        let store = backend().model_store().require("model store").unwrap();
        let models = store.list().unwrap();
        assert_eq!(models.len(), 3);
        assert!(models.iter().all(|model| !model.cached));
    }

    #[cfg(not(feature = "native"))]
    #[test]
    fn load_fails_explicitly_without_native_feature() {
        assert!(matches!(backend().load(), Err(HostError::Unavailable(_))));
        let store = backend().model_store().require("model store").unwrap();
        assert!(matches!(
            store.pull("qwen3-0.6b", false),
            Err(HostError::Unavailable(_))
        ));
    }
}
