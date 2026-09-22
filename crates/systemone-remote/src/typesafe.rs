//! Direct TypeSafe Jev adapter (`kind = "typesafe"`).
//!
//! Calls `https://api.typesafe.ai/v1/systemone` (and `/v1/models`) with a
//! bearer key resolved from the environment at load time. The base URL is
//! fixed in code; a request can never choose its upstream. Responses are
//! passed through verbatim after strict parsing — SystemOne preserves
//! upstream answers, usage and model identity, and refuses to repair
//! corrupt distributions.

use std::time::{Duration, Instant};

use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use systemone_core::{
    Backend, BackendDescription, BackendId, CallContext, Capabilities, DecisionHost,
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
/// Upstream distributions must already be normalized within this tolerance;
/// a hosted passthrough never renormalizes them.
const DISTRIBUTION_TOLERANCE: f64 = 1e-6;
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
        // SAFETY-free by design: the key is read through `std::env::var`
        // (not removed), and tests drive `load_with_key` instead of
        // mutating the environment.
        let api_key = std::env::var(&self.api_key_env).map_err(|_| {
            HostError::validation(format!(
                "environment variable {} (backends.{}.settings.api_key_env) is not set; export the TypeSafe API key",
                self.api_key_env, self.id
            ))
        })?;
        self.load_with_key(base_url, Some(api_key))
    }

    /// Load with the credential supplied by the caller, so tests never
    /// mutate the environment. `None` means "unset", `Some("")` means
    /// "set but empty"; both are rejected.
    pub(crate) fn load_with_key(
        &self,
        base_url: &str,
        api_key: Option<String>,
    ) -> Result<Box<dyn DecisionHost>, HostError> {
        let base = Url::parse(base_url)
            .map_err(|error| HostError::validation(format!("invalid base URL: {error}")))?;
        RemoteTransport::validate_base_url(&base)?;
        let api_key = api_key.ok_or_else(|| {
            HostError::validation(format!(
                "environment variable {} (backends.{}.settings.api_key_env) is not set; export the TypeSafe API key",
                self.api_key_env, self.id
            ))
        })?;
        if api_key.trim().is_empty() {
            return Err(HostError::validation(format!(
                "environment variable {} is set but empty",
                self.api_key_env
            )));
        }
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
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::Typesafe,
            model: self.model.clone(),
            available: true,
            unavailable_reason: None,
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
                description: format!(
                    "TypeSafe hosted Jev model {model}, called as a direct passthrough"
                ),
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

    fn unreachable(error: RemoteError) -> HostError {
        match error {
            RemoteError::Unreachable(message) => {
                HostError::unavailable(format!("TypeSafe API unreachable: {message}"))
            }
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
        if let Ok(value) = wire::parse_strict(body)
            && let Value::Object(fields) = value
            && let Some(Value::String(message)) = fields.get("message")
        {
            let code = match fields.get("error_type") {
                Some(Value::String(error_type)) => Some(error_type.clone()),
                _ => Some("upstream_error".to_owned()),
            };
            return HostError::Upstream {
                status: Some(reply.status),
                code,
                message: self.sanitize(message),
            };
        }
        if let Ok(value) = wire::parse_strict(body)
            && let Value::Object(fields) = value
            && let Some(detail) = fields.get("detail")
        {
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
            Err(other) => return Err(Self::unreachable(other)),
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
        // SystemOne's service contract rejects floats on the wire so every
        // backend behaves identically; the upstream accepts them, so the
        // adapter enforces the rule before sending (openjev enforces it
        // during conversion).
        reject_floats(&request.state, "state").map_err(HostError::Validation)?;
        let timeout = Self::remaining_timeout(context)?;
        // Routing selectors never reach this body; `render_request`
        // projects only the neutral request.
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
            Err(other) => return Err(Self::unreachable(other)),
        };
        if !(200..300).contains(&reply.status) {
            return Err(self.upstream_error(&reply));
        }
        let response = wire::parse_response(&reply.body)
            .map_err(|error| self.invalid_body(reply.status, &error.message))?;
        response
            .validate(DISTRIBUTION_TOLERANCE)
            .map_err(|error| self.invalid_body(reply.status, &error.to_string()))?;
        Ok(response)
    }

    fn shutdown(&mut self) -> Result<(), HostError> {
        // The blocking client needs no teardown.
        Ok(())
    }
}

/// Reject floats anywhere inside `value`, naming the first offending
/// position via `path`. The wire format carries whole numbers only, so the
/// service contract is identical across backends; the hosted upstream is
/// permissive, so the adapter enforces the rule locally.
fn reject_floats(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::Number(number) if number.is_f64() => Err(format!("{path} must not contain floats")),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                reject_floats(item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        Value::Object(fields) => {
            for (key, field) in fields {
                reject_floats(field, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
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
