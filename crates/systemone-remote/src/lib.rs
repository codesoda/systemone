//! Shared hosted HTTP transport and the hosted Jev passthrough adapters.
//!
//! [`transport`] holds the single shared HTTP client policy used by every
//! hosted kind: one send per request (no retries, no provider fallback), no
//! redirects, no client header forwarding, and a bounded response body.
//! [`hosted`] is the passthrough adapter for `kind = "typesafe"` (direct),
//! `"vercel"` (Vercel AI Gateway) and `"openrouter"`; the three differ only
//! in their [`Provider`] profile.
//!
//! Hosted passthroughs block on their own HTTP client inside `evaluate`,
//! which the core host contract permits. Each provider's base URL is a code
//! constant; neither a request nor operator configuration can choose the
//! upstream it is forwarded to.

pub mod hosted;
pub mod transport;

/// How this build links the hosted adapters.
///
/// A hosted passthrough carries no inference runtime, so it needs no build
/// feature and is always linked. `s1 backends` and `s1 --version` report
/// this beside the local kinds, which can report `backend-disabled`.
#[must_use]
pub const fn compiled_feature() -> &'static str {
    "hosted"
}

#[cfg(test)]
mod hosted_tests;

pub use hosted::{
    BASE_URL, Catalogue, DEFAULT_MODEL, HostedBackend, HostedHost, HostedModel, HostedSettings,
    OPENROUTER, Provider, TYPESAFE, VERCEL, parse_models_response, parse_openrouter_models,
    provider,
};
pub use transport::{MAX_RESPONSE_BYTES, RemoteError, RemoteReply, RemoteTransport};
