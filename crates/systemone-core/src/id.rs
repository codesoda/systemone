use std::fmt;

use serde::{Deserialize, Serialize};

use crate::HostError;

/// Name of a configured backend instance (`[backends.<id>]`).
///
/// Distinct from the provider kind and from the model: several instances of
/// one kind may run different models, devices or settings. IDs are lowercase
/// ASCII letters, digits and single hyphens, starting with a letter, so they
/// round-trip through `SYSTEMONE_BACKENDS__<ID>__*` environment variables.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct BackendId(String);

impl BackendId {
    pub fn new(value: impl Into<String>) -> Result<Self, HostError> {
        let value = value.into();
        let bytes = value.as_bytes();
        let valid = !bytes.is_empty()
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            && !value.contains("--")
            && !value.ends_with('-');
        if valid {
            Ok(Self(value))
        } else {
            Err(HostError::Validation(format!(
                "backend ID {value:?} must be lowercase ASCII letters, digits and single hyphens, starting with a letter"
            )))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BackendId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// The vendor/implementation family of a backend instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenJev,
    Laya,
    Gliner2,
    Vercel,
    OpenRouter,
    Typesafe,
}

impl ProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenJev => "openjev",
            Self::Laya => "laya",
            Self::Gliner2 => "gliner2",
            Self::Vercel => "vercel",
            Self::OpenRouter => "openrouter",
            Self::Typesafe => "typesafe",
        }
    }

    /// Whether this kind runs inference locally (loads weights) or forwards
    /// requests to a hosted service.
    #[must_use]
    pub const fn is_local(self) -> bool {
        matches!(self, Self::OpenJev | Self::Laya | Self::Gliner2)
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_ids_are_restricted_to_env_safe_names() {
        for valid in ["local", "cloud-vercel", "a1", "x-y-z"] {
            assert!(BackendId::new(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "Local", "1a", "a--b", "a-", "-a", "a_b", "a.b", "a b"] {
            assert!(BackendId::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn provider_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenRouter).unwrap(),
            "\"openrouter\""
        );
        assert_eq!(
            serde_json::from_str::<ProviderKind>("\"openjev\"").unwrap(),
            ProviderKind::OpenJev
        );
    }
}
