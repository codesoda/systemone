//! Pinned model files that an adapter can ask to be downloaded.
//!
//! An adapter describes *what* a model directory must hold: each file's path
//! inside the directory, an immutable source URL, its size and its SHA-256.
//! It does not download anything. The download itself lives in one shared
//! place so every backend verifies files the same way.

use serde::{Deserialize, Serialize};

use crate::HostError;

/// One file of a model directory, pinned by size and SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    /// Path relative to the model directory, with `/` separators.
    pub path: String,
    /// Source URL at an immutable revision.
    pub url: String,
    pub bytes: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

impl PinnedFile {
    /// Reject a pin that could escape the model directory or that is not
    /// fully specified. Called on every embedded table in tests and before
    /// any download starts.
    pub fn validate(&self) -> Result<(), HostError> {
        let path = self.path.as_str();
        let unsafe_path = path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..");
        if unsafe_path {
            return Err(HostError::validation(format!(
                "pinned file path {path:?} must be a relative path inside the model directory"
            )));
        }
        if !self.url.starts_with("https://") {
            return Err(HostError::validation(format!(
                "pinned file {path} must use an https URL"
            )));
        }
        let hex = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex {
            return Err(HostError::validation(format!(
                "pinned file {path} needs a lowercase hex SHA-256"
            )));
        }
        Ok(())
    }
}

/// Everything one model needs, as a list of pinned files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DownloadPlan {
    /// Short label shown to the user, e.g. `laya english`.
    pub label: String,
    pub files: Vec<PinnedFile>,
}

impl DownloadPlan {
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    pub fn validate(&self) -> Result<(), HostError> {
        if self.files.is_empty() {
            return Err(HostError::validation(format!(
                "download plan {} has no files",
                self.label
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for file in &self.files {
            file.validate()?;
            if !seen.insert(file.path.as_str()) {
                return Err(HostError::validation(format!(
                    "download plan {} lists {} twice",
                    self.label, file.path
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(path: &str) -> PinnedFile {
        PinnedFile {
            path: path.to_owned(),
            url: "https://huggingface.co/o/r/resolve/0000000000000000000000000000000000000000/f"
                .to_owned(),
            bytes: 1,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn paths_must_stay_inside_the_model_directory() {
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(pin(bad).validate().is_err(), "{bad}");
        }
        assert!(pin("tokenizer/tokenizer.json").validate().is_ok());
    }

    #[test]
    fn pins_need_https_and_a_hex_digest() {
        let mut file = pin("a");
        file.url = "http://example.com/a".to_owned();
        assert!(file.validate().is_err());
        let mut file = pin("a");
        file.sha256 = "A".repeat(64);
        assert!(file.validate().is_err());
    }

    #[test]
    fn plans_reject_duplicates_and_sum_sizes() {
        let plan = DownloadPlan {
            label: "x".to_owned(),
            files: vec![pin("a"), pin("b")],
        };
        assert_eq!(plan.total_bytes(), 2);
        assert!(plan.validate().is_ok());
        let plan = DownloadPlan {
            label: "x".to_owned(),
            files: vec![pin("a"), pin("a")],
        };
        assert!(plan.validate().is_err());
    }
}
