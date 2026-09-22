//! Shared hosted HTTP transport and direct Jev passthrough adapters.
//!
//! [`transport`] holds the single shared HTTP client policy used by every
//! hosted kind: one send per request (no retries, no provider fallback), no
//! redirects, no client header forwarding, and a bounded response body.
//! [`typesafe`] is the direct TypeSafe Jev adapter (`kind = "typesafe"`)
//! that calls `https://api.typesafe.ai/v1/systemone` without an
//! intermediary.
//!
//! Hosted passthroughs block on their own HTTP client inside `evaluate`,
//! which the core host contract permits. Base URLs are fixed in typed
//! configuration; a request can never choose the upstream it is forwarded
//! to.

pub mod transport;
pub mod typesafe;

#[cfg(test)]
mod typesafe_tests;

pub use transport::{MAX_RESPONSE_BYTES, RemoteError, RemoteReply, RemoteTransport};
pub use typesafe::{
    BASE_URL, DEFAULT_MODEL, TypesafeBackend, TypesafeHost, TypesafeModel, TypesafeSettings,
    parse_models_response,
};
