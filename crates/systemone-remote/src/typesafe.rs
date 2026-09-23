//! TypeSafe Jev adapter (`kind = "typesafe"`).
//!
//! Calls `https://api.typesafe.ai/v1/systemone` (and `/v1/models`) with a
//! bearer key resolved from the environment at load time. The base URL is
//! fixed in code; a request can never choose its upstream. Responses are
//! passed through verbatim after strict parsing — SystemOne preserves
//! upstream answers, usage and model identity, and refuses to repair
//! corrupt distributions. Only key order is normalized: answers and
//! choice labels are returned in the order the request declared, as
//! every other backend does. A body that answers different questions or
//! different labels is refused, not reordered into shape.
//!
//! Requests pass through too. The adapter adds no value restriction of its
//! own: the upstream owns its request limits, and duplicating them here
//! would refuse requests the live service answers. Float state values are
//! the example. The OpenJev adapter rejects them because its own state
//! type is integer-only; that is an adapter limitation, not a SystemOne
//! rule (see `docs/plans/cross-repo.md` §5). The TypeSafe API accepts
//! floats, so this adapter forwards them.

use std::time::{Duration, Instant};

use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use systemone_core::{
    Answer, Backend, BackendDescription, BackendId, CallContext, Capabilities, DecisionHost,
    DecisionRequest, DecisionResponse, HostError, ModelIdentity, Primitive, ProviderKind,
};
use systemone_http::wire::{self, WireError};

use crate::transport::{MAX_RESPONSE_BYTES, RemoteError, RemoteReply, RemoteTransport};

/// Production base URL of the TypeSafe Jev API.
pub const BASE_URL: &str = "https://api.typesafe.ai";
const SYSTEMONE_PATH: &str = "/v1/systemone";
const MODELS_PATH: &str = "/v1/models";
/// Model used when the backend sets none.
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Half of one Jev wire step. Upstream probabilities arrive at wire
/// precision: two decimals, each entry rounded on its own (see
/// [`wire::round_wire`]). The entries of one distribution therefore need
/// not sum to exactly one, so the normalization check runs at the same
/// precision as the numbers it reads. A hosted passthrough never
/// renormalizes them; it only refuses a distribution that misses by more
/// than the rounding can explain.
const WIRE_ROUNDING_BOUND: f64 = 0.005;
/// Slack for the binary representation of the bound itself.
const TOLERANCE_EPSILON: f64 = 1e-9;
/// Upstream timeout used when the caller set no deadline.
const DEFAULT_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest upstream error message passed through to callers.
const MAX_ERROR_MESSAGE_CHARS: usize = 500;

const CONFIDENCE_DEFINITION: &str = "Self-reported by the TypeSafe Jev API and passed through unchanged; SystemOne does not recompute it.";
const PROBABILITY_DEFINITION: &str = "Normalized distributions reported by the TypeSafe Jev API and passed through unchanged; SystemOne refuses to renormalize them.";

/// Typed settings for `kind = "typesafe"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypesafeSettings {
    /// Environment variable holding the API key. The key is read at load
    /// time; it never lives in configuration.
    pub api_key_env: String,
}

/// A configured, validated TypeSafe backend that has loaded nothing.
///
/// Construction validates settings only; [`Backend::load`] resolves the
/// credential and builds the transport.
pub struct TypesafeBackend {
    id: BackendId,
    model: String,
    aliases: Vec<String>,
    api_key_env: String,
}

impl TypesafeBackend {
    pub fn new(
        id: BackendId,
        model: Option<&str>,
        aliases: Vec<String>,
        settings: &TypesafeSettings,
    ) -> Result<Self, HostError> {
        let api_key_env = settings.api_key_env.trim().to_owned();
        if api_key_env.is_empty() {
            return Err(HostError::validation(
                "settings.api_key_env must name the environment variable holding the TypeSafe API key",
            ));
        }
        if api_key_env.contains('=') || api_key_env.chars().any(char::is_whitespace) {
            return Err(HostError::validation(
                "settings.api_key_env must be an environment variable name, not a value",
            ));
        }
        let model = model.unwrap_or(DEFAULT_MODEL);
        if model.is_empty() {
            return Err(HostError::validation("model must not be empty"));
        }
        Ok(Self {
            id,
            model: model.to_owned(),
            aliases,
            api_key_env,
        })
    }

    /// Load against an explicit base URL. Outside tests this is always
    /// [`BASE_URL`]; the base URL is never request-supplied.
    pub(crate) fn load_with_base(
        &self,
        base_url: &str,
    ) -> Result<Box<dyn DecisionHost>, HostError> {
        // The key is read from the environment (never removed), and tests
        // drive `load_with_key` instead of mutating the environment.
        self.load_with_key(base_url, self.resolve_key())
    }

    /// Read the credential named by `api_key_env`.
    ///
    /// `None` means the variable is absent. A value that is not valid
    /// UTF-8 cannot carry a bearer token, so it reads as an unusable value
    /// rather than an absent one; the two report different reasons.
    fn resolve_key(&self) -> Option<String> {
        match std::env::var(&self.api_key_env) {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotUnicode(_)) => Some(String::new()),
            Err(std::env::VarError::NotPresent) => None,
        }
    }

    /// Why this backend cannot serve a request right now.
    ///
    /// A hosted passthrough needs no build feature and no local model, so
    /// the credential named by `settings.api_key_env` is the only
    /// precondition. [`Backend::load`] fails with the same message, so
    /// `s1 backends` never reports a backend as available that cannot
    /// load. Reachability of the API is not checked: a health probe would
    /// be a billed request.
    fn unavailable_reason(&self) -> Option<String> {
        self.unavailable_reason_for(self.resolve_key().as_deref())
    }

    /// Availability for an already-resolved credential. Tests use this so
    /// they never mutate the environment.
    pub(crate) fn unavailable_reason_for(&self, api_key: Option<&str>) -> Option<String> {
        match api_key {
            Some(key) if !key.trim().is_empty() => None,
            Some(_) => Some(format!(
                "environment variable {} is set but holds no usable key",
                self.api_key_env
            )),
            None => Some(format!(
                "environment variable {} (backends.{}.settings.api_key_env) is not set; export the TypeSafe API key",
                self.api_key_env, self.id
            )),
        }
    }

    /// Load with the credential supplied by the caller, so tests never
    /// mutate the environment. `None` means "unset", `Some("")` means
    /// "set but unusable"; both are rejected.
    ///
    /// A missing credential is an unmet precondition, not a malformed
    /// configuration, so it fails the way the local kinds fail a missing
    /// build feature or model directory: [`HostError::unavailable`] with
    /// the reason `s1 backends` already prints.
    pub(crate) fn load_with_key(
        &self,
        base_url: &str,
        api_key: Option<String>,
    ) -> Result<Box<dyn DecisionHost>, HostError> {
        let base = Url::parse(base_url)
            .map_err(|error| HostError::validation(format!("invalid base URL: {error}")))?;
        RemoteTransport::validate_base_url(&base)?;
        let api_key = match api_key {
            Some(key) if self.unavailable_reason_for(Some(&key)).is_none() => key,
            unusable => {
                return Err(HostError::unavailable(
                    self.unavailable_reason_for(unusable.as_deref())
                        .unwrap_or_else(|| "typesafe backend unavailable".to_owned()),
                ));
            }
        };
        let transport = RemoteTransport::new()?;
        Ok(Box::new(TypesafeHost::new(
            transport,
            base,
            self.model.clone(),
            self.aliases.clone(),
            api_key,
        )))
    }
}

impl Backend for TypesafeBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Typesafe
    }

    fn describe(&self) -> BackendDescription {
        let reason = self.unavailable_reason();
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::Typesafe,
            model: self.model.clone(),
            available: reason.is_none(),
            unavailable_reason: reason,
            // Non-secret by construction: only the variable's name.
            settings: serde_json::json!({ "api_key_env": self.api_key_env }),
        }
    }

    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        self.load_with_base(BASE_URL)
    }
}

/// One entry of the TypeSafe `/v1/models` catalogue, kept exactly as the
/// upstream reported it (no catalogue normalization).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TypesafeModel {
    pub name: String,
    pub description: String,
    pub release_date: String,
}

/// A loaded TypeSafe host: one resident HTTP passthrough.
///
/// `evaluate` blocks on the shared transport, which the core host contract
/// permits for hosted kinds. One request is one upstream call.
pub struct TypesafeHost {
    transport: RemoteTransport,
    base_url: Url,
    capabilities: Capabilities,
    api_key: String,
}

impl TypesafeHost {
    #[must_use]
    pub fn new(
        transport: RemoteTransport,
        base_url: Url,
        model: String,
        aliases: Vec<String>,
        api_key: String,
    ) -> Self {
        let capabilities = Capabilities {
            kind: ProviderKind::Typesafe,
            model: ModelIdentity {
                id: model.clone(),
                description: format!("TypeSafe hosted Jev model {model}"),
                // The catalogue, not the adapter, owns release dates.
                release_date: "unknown".to_owned(),
            },
            model_aliases: aliases,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
            // The upstream enforces its own request limits; duplicating them
            // here would risk contradicting the live service.
            max_questions: None,
            max_options: None,
            max_expanded_state_bytes: None,
            confidence_definition: CONFIDENCE_DEFINITION.to_owned(),
            probability_definition: PROBABILITY_DEFINITION.to_owned(),
            execution_modes: vec!["remote".to_owned()],
            device: None,
            batches_questions: true,
        };
        Self {
            transport,
            base_url,
            capabilities,
            api_key,
        }
    }

    fn systemone_url(&self) -> Url {
        self.base_url
            .join(SYSTEMONE_PATH)
            .expect("static relative path")
    }

    fn models_url(&self) -> Url {
        self.base_url
            .join(MODELS_PATH)
            .expect("static relative path")
    }

    fn remaining_timeout(context: &CallContext) -> Result<Duration, HostError> {
        let remaining = match context.deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => DEFAULT_UPSTREAM_TIMEOUT,
        };
        if remaining.is_zero() {
            return Err(HostError::Timeout);
        }
        Ok(remaining)
    }

    fn unreachable(&self, error: RemoteError) -> HostError {
        match error {
            RemoteError::Unreachable(message) => HostError::unavailable(
                self.sanitize(&format!("TypeSafe API unreachable: {message}")),
            ),
            other => unreachable!("caller handled {other:?} first"),
        }
    }

    /// Map a non-2xx or unparseable upstream reply to a sanitized
    /// `HostError::Upstream`. The bearer key value is redacted from any
    /// upstream-echoed message. The upstream reports errors both as the
    /// shared envelope (`{"error_type","message"}`) and FastAPI-style
    /// (`{"detail": string | [{"msg": …}]}`); the status always passes
    /// through unchanged.
    fn upstream_error(&self, reply: &RemoteReply) -> HostError {
        let body = std::str::from_utf8(&reply.body).unwrap_or("");
        let fields = match wire::parse_strict(body) {
            Ok(Value::Object(fields)) => fields,
            _ => {
                return HostError::Upstream {
                    status: Some(reply.status),
                    code: None,
                    message: self.sanitize("upstream returned an error with an unrecognized body"),
                };
            }
        };
        let error_type = fields.get("error_type").and_then(Value::as_str);
        let message = fields.get("message").and_then(Value::as_str);
        // One usable half of the envelope is enough. A malformed message
        // never discards the classification the upstream did report.
        if error_type.is_some() || message.is_some() {
            let message = message.map_or_else(
                || format!("upstream returned HTTP {}", reply.status),
                str::to_owned,
            );
            return HostError::Upstream {
                status: Some(reply.status),
                code: Some(error_type.unwrap_or("upstream_error").to_owned()),
                message: self.sanitize(&message),
            };
        }
        if let Some(detail) = fields.get("detail") {
            let code = if matches!(detail, Value::Array(_)) {
                "validation_error"
            } else {
                "upstream_error"
            };
            let message = match detail {
                Value::String(message) => message.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item.get("msg").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("; "),
                _ => String::new(),
            };
            let message = if message.is_empty() {
                format!("upstream returned HTTP {}", reply.status)
            } else {
                message
            };
            return HostError::Upstream {
                status: Some(reply.status),
                code: Some(code.to_owned()),
                message: self.sanitize(&message),
            };
        }
        HostError::Upstream {
            status: Some(reply.status),
            code: None,
            message: self.sanitize("upstream returned an error with an unrecognized body"),
        }
    }

    fn invalid_body(&self, status: u16, message: &str) -> HostError {
        HostError::Upstream {
            status: Some(status),
            code: Some("invalid_upstream_body".to_owned()),
            message: self.sanitize(message),
        }
    }

    /// Redact the bearer key, strip control characters, and bound length.
    fn sanitize(&self, message: &str) -> String {
        let redacted = if self.api_key.is_empty() {
            message.to_owned()
        } else {
            message.replace(&self.api_key, "[redacted]")
        };
        let cleaned: String = redacted.chars().filter(|c| !c.is_control()).collect();
        if cleaned.chars().count() > MAX_ERROR_MESSAGE_CHARS {
            cleaned.chars().take(MAX_ERROR_MESSAGE_CHARS).collect()
        } else {
            cleaned
        }
    }

    /// Fetch the upstream `/v1/models` catalogue. Outside `evaluate`; used
    /// by tests and the opt-in live smoke test.
    pub fn list_models(&self, timeout: Duration) -> Result<Vec<TypesafeModel>, HostError> {
        let reply = match self.transport.send(
            Method::GET,
            &self.models_url(),
            Some(&self.api_key),
            None,
            timeout,
            MAX_RESPONSE_BYTES,
        ) {
            Ok(reply) => reply,
            Err(RemoteError::Timeout) => return Err(HostError::Timeout),
            Err(RemoteError::TooLarge(cap)) => {
                return Err(HostError::Upstream {
                    status: None,
                    code: Some("response_too_large".to_owned()),
                    message: self.sanitize(&format!(
                        "upstream models response exceeded the {cap}-byte cap"
                    )),
                });
            }
            Err(other) => return Err(self.unreachable(other)),
        };
        if !(200..300).contains(&reply.status) {
            return Err(self.upstream_error(&reply));
        }
        parse_models_response(&reply.body)
            .map_err(|error| self.invalid_body(reply.status, &error.message))
    }
}

impl DecisionHost for TypesafeHost {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn evaluate(
        &mut self,
        request: &DecisionRequest,
        context: &CallContext,
    ) -> Result<DecisionResponse, HostError> {
        let model = self
            .capabilities
            .resolve_model(request.model.as_deref())?
            .to_owned();
        context.check()?;
        let timeout = Self::remaining_timeout(context)?;
        // Routing selectors never reach this body; `render_request`
        // projects only the neutral request. State values, floats
        // included, go upstream unchanged: the API owns its own limits.
        let body = wire::render_request(request, &model);
        let reply = match self.transport.send(
            Method::POST,
            &self.systemone_url(),
            Some(&self.api_key),
            Some(&body),
            timeout,
            MAX_RESPONSE_BYTES,
        ) {
            Ok(reply) => reply,
            Err(RemoteError::Timeout) => return Err(HostError::Timeout),
            Err(RemoteError::TooLarge(cap)) => {
                return Err(HostError::Upstream {
                    status: None,
                    code: Some("response_too_large".to_owned()),
                    message: self
                        .sanitize(&format!("upstream response exceeded the {cap}-byte cap")),
                });
            }
            Err(other) => return Err(self.unreachable(other)),
        };
        if !(200..300).contains(&reply.status) {
            return Err(self.upstream_error(&reply));
        }
        let mut response = wire::parse_response(&reply.body)
            .map_err(|error| self.invalid_body(reply.status, &error.message))?;
        // The upstream owns its key order; SystemOne owns the one its
        // callers see. This moves entries into request order and refuses a
        // body that answers other questions or other labels. Values are
        // never touched.
        wire::align_to_request(&mut response, request)
            .map_err(|error| self.invalid_body(reply.status, &error.message))?;
        response
            .validate(distribution_tolerance(&response))
            .map_err(|error| self.invalid_body(reply.status, &error.to_string()))?;
        Ok(response)
    }

    fn shutdown(&mut self) -> Result<(), HostError> {
        // The blocking client needs no teardown.
        Ok(())
    }
}

/// Sum tolerance for one upstream response.
///
/// The upstream reports probabilities at Jev wire precision, so a
/// distribution of `n` entries can miss one by up to `n` half steps. The
/// widest distribution in the response sets the bound for all of them; a
/// stricter bound would refuse a correct body that SystemOne already paid
/// for, and SystemOne must never repair it instead.
fn distribution_tolerance(response: &DecisionResponse) -> f64 {
    let widest = response
        .answers
        .iter()
        .map(|(_, answer)| match answer {
            Answer::Choice(choice) => choice.probabilities.len(),
            // A noul distribution is `[p, 1 - p]`: one reported value.
            Answer::Noul(_) => 1,
            Answer::Score(score) => score.probabilities.len(),
        })
        .max()
        .unwrap_or(1);
    widest as f64 * WIRE_ROUNDING_BOUND + TOLERANCE_EPSILON
}

/// Parse the TypeSafe `/v1/models` catalogue body: an array of objects with
/// a required `name` string and optional `description`/`release_date`
/// strings. Extra fields are ignored; entries are kept exactly as reported.
pub fn parse_models_response(bytes: &[u8]) -> Result<Vec<TypesafeModel>, WireError> {
    let text = std::str::from_utf8(bytes).map_err(|_| WireError {
        error_type: "invalid_json",
        message: "models response must be valid UTF-8 JSON".to_owned(),
    })?;
    let value = wire::parse_strict(text)?;
    // The live API returns `{"models": [...]}`; a bare array is also
    // accepted so the catalogue shape stays compatible either way.
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(fields) => match fields.get("models") {
            Some(Value::Array(entries)) => entries.clone(),
            _ => {
                return Err(WireError {
                    error_type: "validation_error",
                    message: "models response must be {\"models\": [...]}".to_owned(),
                });
            }
        },
        _ => {
            return Err(WireError {
                error_type: "validation_error",
                message: "models response must be {\"models\": [...]}".to_owned(),
            });
        }
    };
    let mut models = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let fields = match entry {
            Value::Object(fields) => fields,
            _ => {
                return Err(WireError {
                    error_type: "validation_error",
                    message: format!("models[{index}] must be a JSON object"),
                });
            }
        };
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| WireError {
                error_type: "validation_error",
                message: format!("models[{index}].name must be a nonempty string"),
            })?;
        models.push(TypesafeModel {
            name: name.to_owned(),
            description: fields
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            release_date: fields
                .get("release_date")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        });
    }
    Ok(models)
}
