//! `ModelStore` over the OpenJev registry and verified cache.

use openjev_llama::{ModelCache, ModelRegistry, ModelSpec};
use systemone_core::{HostError, ModelArtifact, ModelStatus, ModelStore};

use crate::settings::ResolvedSettings;

pub struct OpenJevModelStore {
    settings: ResolvedSettings,
}

impl OpenJevModelStore {
    #[must_use]
    pub const fn new(settings: ResolvedSettings) -> Self {
        Self { settings }
    }

    fn registry(&self) -> Result<ModelRegistry, HostError> {
        ModelRegistry::bundled().map_err(|error| HostError::internal(error.to_string()))
    }

    fn cache(&self) -> ModelCache {
        ModelCache::new(self.settings.cache_dir.clone())
    }
}

impl ModelStore for OpenJevModelStore {
    fn list(&self) -> Result<Vec<ModelStatus>, HostError> {
        let registry = self.registry()?;
        let cache = self.cache();
        let mut models = Vec::with_capacity(registry.list().len());
        for entry in registry.list() {
            let (cached, verified, cache_status, path) = match cache.inspect(entry) {
                Ok(Some(artifact)) => (
                    true,
                    true,
                    "verified-manifest-sha256".to_owned(),
                    Some(artifact.path.display().to_string()),
                ),
                Ok(None) => (false, false, "missing".to_owned(), None),
                Err(error) => (true, false, format!("verification-failed: {error}"), None),
            };
            models.push(ModelStatus {
                id: entry.id.clone(),
                source: entry.repo.clone(),
                revision: entry.revision.clone(),
                file: entry.file.clone(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
                cached,
                verified,
                cache_status,
                path,
                detail: Some(serde_json::json!({
                    "quant": entry.quant,
                    "template_profile": entry.profile,
                    "default": entry.id == registry.default_model(),
                    "shared_probe_status": "configuration-specific-local-receipt-required",
                })),
            });
        }
        Ok(models)
    }

    fn path(&self, id: &str) -> Result<ModelArtifact, HostError> {
        let registry = self.registry()?;
        let cache = self.cache();
        let entry = registry
            .resolve(id)
            .map_err(|error| HostError::validation(error.to_string()))?;
        let artifact = registry
            .path(&cache, id)
            .map_err(|error| HostError::unavailable(error.to_string()))?;
        Ok(ModelArtifact {
            id: entry.id.clone(),
            path: artifact.path.display().to_string(),
            bytes: artifact.bytes,
            sha256: artifact.sha256,
            integrity: "manifest-sha256".to_owned(),
            cache_hit: artifact.cache_hit,
        })
    }

    #[cfg(feature = "native")]
    fn pull(&self, id: &str, repair: bool) -> Result<ModelArtifact, HostError> {
        let registry = self.registry()?;
        let cache = self.cache();
        let spec = crate::settings::resolve(
            &crate::settings::OpenJevSettings {
                cache_dir: Some(self.settings.cache_dir.clone()),
                offline: self.settings.offline,
                ..Default::default()
            },
            Some(id),
            None,
        )?
        .model;
        if repair && !matches!(&spec, ModelSpec::RegistryId(_)) {
            return Err(HostError::unsupported(
                "repair is limited to registered cache-owned artifacts",
            ));
        }
        let resolved = openjev_llama::resolve_model_spec(
            &registry,
            &cache,
            &spec,
            openjev_llama::CacheOptions {
                offline: self.settings.offline,
                repair,
            },
        )
        .map_err(crate::host::runtime)?;
        let id = resolved.model().id().to_owned();
        let integrity = resolved.model().integrity();
        let (_, artifact) = resolved.into_parts();
        Ok(ModelArtifact {
            id,
            path: artifact.path.display().to_string(),
            bytes: artifact.bytes,
            sha256: artifact.sha256,
            integrity: match integrity {
                openjev_core::Integrity::ManifestSha256 => "manifest-sha256",
                openjev_core::Integrity::CallerSha256 => "caller-sha256",
                openjev_core::Integrity::LocalUnverified => "local-unverified",
            }
            .to_owned(),
            cache_hit: artifact.cache_hit,
        })
    }

    #[cfg(not(feature = "native"))]
    fn pull(&self, _id: &str, _repair: bool) -> Result<ModelArtifact, HostError> {
        let _ = ModelSpec::RegistryId(String::new());
        Err(HostError::unavailable(
            "model download requires a build with the openjev native, metal, or cuda feature",
        ))
    }
}
