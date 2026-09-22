//! Neutral contracts for SystemOne.
//!
//! This crate defines the one interface that connects a command or HTTP
//! endpoint to a host implementation ([`DecisionHost`]), the neutral request
//! and response types that cross that boundary, and optional host extension
//! traits (for example [`ModelStore`]) whose support is declared per backend
//! through [`Extension`].
//!
//! Nothing here depends on an inference library. Adapters convert these types
//! to and from their vendor representation and must never silently coerce,
//! truncate, or fabricate results the host did not produce.

pub mod capabilities;
pub mod error;
pub mod extension;
pub mod host;
pub mod id;
pub mod request;
pub mod response;

pub use capabilities::{Capabilities, ModelIdentity, Primitive};
pub use error::HostError;
pub use extension::{Extension, ExtensionCoverage, ModelArtifact, ModelStatus, ModelStore};
pub use host::{Backend, BackendDescription, CallContext, DecisionHost};
pub use id::{BackendId, ProviderKind};
pub use request::{ChoiceQuestion, DecisionRequest, NoulQuestion, Question, ScoreQuestion};
pub use response::{
    Answer, ChoiceAnswer, DecisionResponse, Diagnostics, NoulAnswer, ScoreAnswer, Usage,
};
