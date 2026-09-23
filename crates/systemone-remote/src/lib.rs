//! Shared hosted HTTP transport and direct Jev passthrough adapters.
//!
//! [`transport`] holds the single shared HTTP client policy used by every
//! hosted kind: one send per request (no retries, no provider fallback), no
//! redirects, no client header forwarding, and a bounded response body.
//! [`typesafe`] is the TypeSafe Jev adapter (`kind = "typesafe"`)
//! that calls `https://api.typesafe.ai/v1/systemone` without an
//! intermediary.
//!
//! Hosted passthroughs block on their own HTTP client inside `evaluate`,
//! which the core host contract permits. Each adapter holds its base URL
//! as a code constant; neither a request nor operator configuration can
//! choose the upstream it is forwarded to.

pub mod transport;
pub mod typesafe;

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
mod typesafe_tests;

pub use transport::{MAX_RESPONSE_BYTES, RemoteError, RemoteReply, RemoteTransport};
pub use typesafe::{
    BASE_URL, DEFAULT_MODEL, TypesafeBackend, TypesafeHost, TypesafeModel, TypesafeSettings,
    parse_models_response,
};
