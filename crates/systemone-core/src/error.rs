use thiserror::Error;

/// Errors crossing the host boundary.
///
/// Messages must already be safe to show to a caller: no credentials, no
/// full request states, no unsanitized upstream bodies.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HostError {
    /// The request is malformed or violates a documented limit.
    #[error("{0}")]
    Validation(String),
    /// The request is well formed but this host cannot satisfy it.
    #[error("{0}")]
    Unsupported(String),
    /// The host (or its build/device/model) is not available for work.
    #[error("{0}")]
    Unavailable(String),
    /// Bounded admission is full.
    #[error("host is overloaded")]
    Overloaded,
    /// The whole-request deadline elapsed.
    #[error("request deadline elapsed")]
    Timeout,
    /// The caller went away before completion.
    #[error("request was cancelled")]
    Cancelled,
    /// A hosted provider returned an error; status/code are sanitized.
    #[error("upstream error{}: {message}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
    Upstream {
        status: Option<u16>,
        code: Option<String>,
        message: String,
    },
    /// Anything else; message is a short diagnostic, not a stack dump.
    #[error("{0}")]
    Internal(String),
}

impl HostError {
    /// Short stable machine code for JSON error records.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Unsupported(_) => "unsupported",
            Self::Unavailable(_) => "unavailable",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Upstream { .. } => "upstream",
            Self::Internal(_) => "internal",
        }
    }

    /// Whether a resident host should be considered broken after this error.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
}
