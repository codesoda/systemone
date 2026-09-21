//! Jev-compatible HTTP service for SystemOne.
//!
//! - [`wire`]: strict request parsing and rounded response rendering.
//! - [`registry`]: one resident owner thread per enabled backend with
//!   bounded admission.
//! - [`server`]: the Axum router and process lifecycle.

pub mod registry;
pub mod server;
pub mod wire;

pub use registry::{Evaluated, Registry, RegistryEntry};
pub use server::{AppState, ServeOptions, router, run};
pub use wire::{ParsedRequest, SystemOneBody, WireError, parse_request, render_response};

#[cfg(test)]
pub mod test_support;
