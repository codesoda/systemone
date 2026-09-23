use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use serde::Serialize;

use crate::{
    BackendId, Capabilities, DecisionRequest, DecisionResponse, Extension, HostError, ModelStore,
    ProviderKind,
};

/// Per-call control information. Never carries credentials or transport
/// types.
#[derive(Clone, Debug)]
pub struct CallContext {
    pub request_id: String,
    pub deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

impl CallContext {
    #[must_use]
    pub fn new(request_id: impl Into<String>, deadline: Option<Instant>) -> Self {
        Self {
            request_id: request_id.into(),
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A flag the caller can flip to request cancellation.
    #[must_use]
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Fail fast between units of native work; a running noninterruptible
    /// call still completes.
    pub fn check(&self) -> Result<(), HostError> {
        if self.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(HostError::Timeout);
        }
        Ok(())
    }
}

/// A loaded host able to answer typed decisions.
///
/// Implementations own native state and are driven from one owner thread by
/// the service, so methods are synchronous and take `&mut self`. A hosted
/// passthrough may block on its own HTTP client inside `evaluate`.
///
/// One request goes through `evaluate` once: a multi-question request must
/// not be turned into repeated calls by the caller. Hosts batch, serialize or
/// fall back internally and disclose what they did in
/// [`crate::Diagnostics`].
pub trait DecisionHost: Send {
    fn capabilities(&self) -> &Capabilities;

    fn evaluate(
        &mut self,
        request: &DecisionRequest,
        context: &CallContext,
    ) -> Result<DecisionResponse, HostError>;

    /// Release native resources. Called once; errors are reported, not
    /// retried.
    fn shutdown(&mut self) -> Result<(), HostError>;
}

/// A configured, validated backend instance that has not loaded anything.
///
/// Building a `Backend` must not read secrets, download weights, allocate
/// native models or make network calls; [`Backend::load`] does that work.
pub trait Backend: Send + Sync {
    fn id(&self) -> &BackendId;
    fn kind(&self) -> ProviderKind;
    /// Non-secret summary for listing without loading.
    fn describe(&self) -> BackendDescription;
    /// Load weights / resolve credentials and return a ready host.
    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError>;
    /// Model cache management, if this kind has a local cache.
    fn model_store(&self) -> Extension<Box<dyn ModelStore>> {
        Extension::unsupported(format!(
            "{} backends do not manage a model cache",
            self.kind()
        ))
    }
}

/// Non-secret static description of a backend for listing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BackendDescription {
    pub id: BackendId,
    pub kind: ProviderKind,
    pub model: String,
    /// Whether [`Backend::load`] finds its preconditions met right now.
    /// A local kind checks the build feature and the model on disk; a
    /// hosted kind checks the credential it needs. Neither contacts the
    /// runtime or the API to find out.
    pub available: bool,
    /// Why not, if `available` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Non-secret resolved settings for display.
    pub settings: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn context_reports_cancellation_and_deadline() {
        let context = CallContext::new("r1", None);
        assert!(context.check().is_ok());
        context.cancellation().store(true, Ordering::Release);
        assert_eq!(context.check(), Err(HostError::Cancelled));

        let expired = CallContext::new("r2", Some(Instant::now() - Duration::from_secs(1)));
        assert_eq!(expired.check(), Err(HostError::Timeout));
    }
}
