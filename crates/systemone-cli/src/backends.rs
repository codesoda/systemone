//! Construct `Backend` instances from resolved configuration.
//!
//! Construction validates each kind's typed settings but loads nothing.
//! Kinds without an adapter in this build are reported, not silently
//! skipped.

use std::sync::Arc;

use serde::Serialize;
use systemone_config::{BackendConfig, Config, settings_to_json};
use systemone_core::{
    Backend, BackendDescription, BackendId, ExtensionCoverage, HostError, ProviderKind,
};
use systemone_gliner2::{Gliner2Backend, Gliner2Settings};
use systemone_kev::{KevBackend, KevSettings};
use systemone_laya::{LayaBackend, LayaSettings};
use systemone_openjev::{OpenJevBackend, OpenJevSettings};
use systemone_remote::{HostedBackend, HostedSettings};

/// A configured instance: either a constructed backend or the reason it
/// could not be constructed.
pub struct Configured {
    pub id: BackendId,
    pub config: BackendConfig,
    pub backend: Result<Arc<dyn Backend>, HostError>,
}

impl Configured {
    #[must_use]
    pub fn describe(&self) -> BackendDescription {
        match &self.backend {
            Ok(backend) => backend.describe(),
            Err(error) => BackendDescription {
                id: self.id.clone(),
                kind: self.config.kind,
                model: self.config.model.clone().unwrap_or_default(),
                available: false,
                unavailable_reason: Some(error.to_string()),
                settings: settings_to_json(&self.config.settings),
            },
        }
    }
}

pub fn build(id: &BackendId, config: &BackendConfig) -> Result<Arc<dyn Backend>, HostError> {
    match config.kind {
        ProviderKind::OpenJev => {
            let settings: OpenJevSettings =
                serde_json::from_value(settings_to_json(&config.settings)).map_err(|error| {
                    HostError::validation(format!("backends.{id}.settings: {error}"))
                })?;
            let backend = OpenJevBackend::new(
                id.clone(),
                config.model.as_deref(),
                config.aliases.clone(),
                &settings,
                systemone_config::home_directory().as_deref(),
            )
            .map_err(|error| prefix(id, error))?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Laya => {
            let settings: LayaSettings = serde_json::from_value(settings_to_json(&config.settings))
                .map_err(|error| {
                    HostError::validation(format!("backends.{id}.settings: {error}"))
                })?;
            let backend = LayaBackend::new(
                id.clone(),
                config.model.as_deref(),
                config.aliases.clone(),
                &settings,
                systemone_config::home_directory().as_deref(),
            )
            .map_err(|error| prefix(id, error))?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Kev => {
            let settings: KevSettings = serde_json::from_value(settings_to_json(&config.settings))
                .map_err(|error| {
                    HostError::validation(format!("backends.{id}.settings: {error}"))
                })?;
            let backend = KevBackend::new(
                id.clone(),
                config.model.as_deref(),
                config.aliases.clone(),
                &settings,
                systemone_config::home_directory().as_deref(),
            )
            .map_err(|error| prefix(id, error))?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Gliner2 => {
            let settings: Gliner2Settings =
                serde_json::from_value(settings_to_json(&config.settings)).map_err(|error| {
                    HostError::validation(format!("backends.{id}.settings: {error}"))
                })?;
            let backend = Gliner2Backend::new(
                id.clone(),
                config.model.as_deref(),
                config.aliases.clone(),
                &settings,
                systemone_config::home_directory().as_deref(),
            )
            .map_err(|error| prefix(id, error))?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Typesafe | ProviderKind::Vercel | ProviderKind::OpenRouter => {
            let provider = systemone_remote::provider(config.kind)
                .expect("every hosted kind has a provider profile");
            let settings: HostedSettings =
                serde_json::from_value(settings_to_json(&config.settings)).map_err(|error| {
                    HostError::validation(format!("backends.{id}.settings: {error}"))
                })?;
            let backend = HostedBackend::new(
                provider,
                id.clone(),
                config.model.as_deref(),
                config.aliases.clone(),
                &settings,
            )
            .map_err(|error| prefix(id, error))?;
            Ok(Arc::new(backend))
        }
    }
}

fn prefix(id: &BackendId, error: HostError) -> HostError {
    match error {
        HostError::Validation(message) => HostError::Validation(format!("backends.{id}.{message}")),
        other => other,
    }
}

/// Construct every configured instance (enabled or not).
#[must_use]
pub fn configure_all(config: &Config) -> Vec<Configured> {
    config
        .backends
        .iter()
        .map(|(id, backend)| Configured {
            id: id.clone(),
            config: backend.clone(),
            backend: build(id, backend),
        })
        .collect()
}

/// Construct one instance by ID or the default, requiring success.
pub fn configure_one(config: &Config, selector: Option<&str>) -> Result<Configured, HostError> {
    let id = match selector {
        Some(selector) => BackendId::new(selector)?,
        None => config.default_backend.clone().ok_or_else(|| {
            HostError::validation("no default_backend is configured; pass --backend")
        })?,
    };
    let backend = config
        .backends
        .get(&id)
        .ok_or_else(|| HostError::not_found(format!("unknown backend {id}")))?;
    Ok(Configured {
        id: id.clone(),
        config: backend.clone(),
        backend: build(&id, backend),
    })
}

#[derive(Serialize)]
pub struct BackendListing {
    #[serde(flatten)]
    pub description: BackendDescription,
    pub enabled: bool,
    pub queue_capacity: usize,
    pub max_in_flight: usize,
    pub aliases: Vec<String>,
    pub extensions: Extensions,
}

#[derive(Serialize)]
pub struct Extensions {
    pub model_store: ExtensionCoverage,
}

#[must_use]
pub fn listing(configured: &Configured) -> BackendListing {
    let model_store = match &configured.backend {
        Ok(backend) => backend.model_store().coverage(),
        Err(error) => ExtensionCoverage {
            supported: false,
            reason: Some(error.to_string()),
        },
    };
    BackendListing {
        description: configured.describe(),
        enabled: configured.config.enabled,
        queue_capacity: configured.config.queue_capacity,
        max_in_flight: configured.config.max_in_flight,
        aliases: configured.config.aliases.clone(),
        extensions: Extensions { model_store },
    }
}
