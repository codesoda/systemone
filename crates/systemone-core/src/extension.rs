//! Optional host capabilities beyond decisions.
//!
//! Every extension trait is reached through [`Extension`], so a caller can
//! always ask "does this backend support X?" and get either the
//! implementation or an explicit reason (for example, a hosted passthrough
//! does not download models).

use serde::Serialize;

use crate::HostError;

/// Support coverage of one extension trait for one backend.
#[derive(Debug)]
pub enum Extension<T> {
    Supported(T),
    Unsupported { reason: String },
}

impl<T> Extension<T> {
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported(_))
    }

    /// Unwrap the implementation or produce `HostError::Unsupported`.
    pub fn require(self, what: &str) -> Result<T, HostError> {
        match self {
            Self::Supported(value) => Ok(value),
            Self::Unsupported { reason } => Err(HostError::unsupported(format!(
                "{what} is not supported by this backend: {reason}"
            ))),
        }
    }

    /// Serializable coverage summary for `s1 backends` / `GET /v1/backends`.
    #[must_use]
    pub fn coverage(&self) -> ExtensionCoverage {
        match self {
            Self::Supported(_) => ExtensionCoverage {
                supported: true,
                reason: None,
            },
            Self::Unsupported { reason } => ExtensionCoverage {
                supported: false,
                reason: Some(reason.clone()),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExtensionCoverage {
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Inspect and populate a local model cache. Never loads weights.
pub trait ModelStore: Send + Sync {
    /// Known models and their cache status without touching the network.
    fn list(&self) -> Result<Vec<ModelStatus>, HostError>;
    /// Locate an already cached, verified artifact; no download.
    fn path(&self, id: &str) -> Result<ModelArtifact, HostError>;
    /// Download/verify an artifact into the cache. `repair` re-fetches a
    /// corrupt cache entry when the implementation supports it.
    fn pull(&self, id: &str, repair: bool) -> Result<ModelArtifact, HostError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelStatus {
    pub id: String,
    pub source: String,
    pub revision: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub cached: bool,
    pub verified: bool,
    pub cache_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelArtifact {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub integrity: String,
    pub cache_hit: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_extensions_explain_themselves() {
        let extension: Extension<()> = Extension::unsupported("hosted passthrough");
        assert!(!extension.is_supported());
        assert_eq!(
            extension.coverage(),
            ExtensionCoverage {
                supported: false,
                reason: Some("hosted passthrough".into())
            }
        );
        let error = extension.require("model store").unwrap_err();
        assert!(
            matches!(error, HostError::Unsupported(message) if message.contains("passthrough"))
        );
        assert!(Extension::Supported(1).require("x").is_ok());
    }
}
