//! Jev-compatible HTTP service over a [`Registry`].
//!
//! Routes: `POST /v1/systemone`, `GET /v1/models`, `GET /v1/backends`,
//! `GET /healthz`, `GET /readyz`. Backend selection is the top-level
//! `backend` body field or `X-SystemOne-Backend` header (they must agree);
//! omitted means the configured default. Routing evidence is returned in
//! `x-systemone-*` headers so the JSON body stays SDK-compatible.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use systemone_core::HostError;

use crate::{
    registry::{Evaluated, Registry},
    wire::{self, WireError},
};

pub const BACKEND_HEADER: &str = "x-systemone-backend";
const RETRY_AFTER_SECS: &str = "1";

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub host: IpAddr,
    pub port: u16,
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    api_key: Option<Arc<str>>,
}

impl ServeOptions {
    /// Validate service options. Reads the bearer secret from `api_key_env`
    /// now so a missing secret is a startup error, and requires one for any
    /// non-loopback bind.
    pub fn new(
        host: IpAddr,
        port: u16,
        request_timeout_secs: u64,
        max_body_bytes: usize,
        api_key_env: Option<&str>,
    ) -> Result<Self, HostError> {
        if port == 0 {
            return Err(HostError::validation("server.port must be positive"));
        }
        if request_timeout_secs == 0 {
            return Err(HostError::validation(
                "server.request_timeout_secs must be positive",
            ));
        }
        let request_timeout = Duration::from_secs(request_timeout_secs);
        if Instant::now().checked_add(request_timeout).is_none() {
            return Err(HostError::validation(
                "server.request_timeout_secs is too large for this platform",
            ));
        }
        let api_key = api_key_env
            .map(|name| {
                if name.is_empty() || name.contains('=') {
                    return Err(HostError::validation(
                        "server.api_key_env must name a nonempty environment variable",
                    ));
                }
                let value = std::env::var(name).map_err(|_| {
                    HostError::validation(format!(
                        "server.api_key_env variable {name:?} is missing or not valid UTF-8"
                    ))
                })?;
                if value.trim().is_empty() {
                    return Err(HostError::validation(format!(
                        "server.api_key_env variable {name:?} must contain a nonempty bearer secret"
                    )));
                }
                HeaderValue::from_str(&format!("Bearer {value}")).map_err(|_| {
                    HostError::validation("bearer secret is not valid in an HTTP header")
                })?;
                Ok(Arc::<str>::from(value))
            })
            .transpose()?;
        if !host.is_loopback() && api_key.is_none() {
            return Err(HostError::validation(
                "non-loopback server.host requires server.api_key_env with a nonempty bearer secret",
            ));
        }
        Ok(Self {
            host,
            port,
            request_timeout,
            max_body_bytes,
            api_key,
        })
    }

    /// Test/embedding helper: supply the bearer secret directly.
    #[must_use]
    pub fn with_api_key(mut self, secret: Option<&str>) -> Self {
        self.api_key = secret.map(Arc::from);
        self
    }
}

/// Serve until SIGINT/SIGTERM (or console shutdown on Windows), then drain
/// and shut the registry down.
pub fn run(registry: Registry, options: ServeOptions) -> Result<(), HostError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| HostError::internal(format!("server runtime: {error}")))?;
    runtime.block_on(run_async(registry, options))
}

async fn run_async(registry: Registry, options: ServeOptions) -> Result<(), HostError> {
    let address = SocketAddr::new(options.host, options.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| HostError::unavailable(format!("cannot bind {address}: {error}")))?;
    let registry = Arc::new(registry);
    let app = router(AppState {
        registry: Arc::clone(&registry),
        request_timeout: options.request_timeout,
        max_body_bytes: options.max_body_bytes,
        api_key: options.api_key,
    });
    let loaded: Vec<String> = registry
        .loaded()
        .map(|(id, capabilities)| format!("{id}={}", capabilities.model.id))
        .collect();
    tracing::info!(address = %address, backends = ?loaded, "systemone server ready");
    let registry_for_signal = Arc::clone(&registry);
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            registry_for_signal.begin_shutdown();
            tracing::info!("systemone server stopping admission; waiting for in-flight work");
        })
        .await;
    let registry = Arc::try_unwrap(registry)
        .map_err(|_| HostError::internal("registry still referenced at shutdown"))?;
    let shutdown_result = registry.shutdown();
    serve_result.map_err(|error| HostError::internal(format!("server io: {error}")))?;
    shutdown_result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    pub api_key: Option<Arc<str>>,
}

impl AppState {
    /// Build state for embedding/tests without binding a socket.
    #[must_use]
    pub fn new(registry: Arc<Registry>, options: &ServeOptions) -> Self {
        Self {
            registry,
            request_timeout: options.request_timeout,
            max_body_bytes: options.max_body_bytes,
            api_key: options.api_key.clone(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/systemone", post(system_one))
        .route("/v1/models", get(models))
        .route("/v1/backends", get(backends))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
}

async fn health() -> Response {
    json_value(StatusCode::OK, serde_json::json!({"status": "ok"}))
}

async fn ready(State(state): State<AppState>) -> Response {
    if state.registry.all_ready() {
        json_value(StatusCode::OK, serde_json::json!({"status": "ready"}))
    } else {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "one or more enabled backends are not ready",
        )
    }
}

#[derive(Deserialize)]
struct BackendQuery {
    backend: Option<String>,
}

#[derive(Serialize)]
struct ModelsEnvelope<'a> {
    models: [ModelCard<'a>; 1],
}

#[derive(Serialize)]
struct ModelCard<'a> {
    name: &'a str,
    description: &'a str,
    release_date: &'a str,
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BackendQuery>,
) -> Response {
    if let Some(response) = authorization_error(&state, &headers) {
        return response;
    }
    let selector = match selector(query.backend.as_deref(), &headers) {
        Ok(selector) => selector,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.error_type, &error.message),
    };
    let backend = match state.registry.select(selector.as_deref()) {
        Ok(backend) => backend,
        Err(error) => return host_error(&error),
    };
    let Some(capabilities) = state.registry.capabilities(&backend) else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "backend is not loaded",
        );
    };
    let mut response = json_response(
        StatusCode::OK,
        &ModelsEnvelope {
            models: [ModelCard {
                name: &capabilities.model.id,
                description: &capabilities.model.description,
                release_date: &capabilities.model.release_date,
            }],
        },
    );
    insert_safe_header(response.headers_mut(), BACKEND_HEADER, backend.as_str());
    response
}

#[derive(Serialize)]
struct BackendsEnvelope<'a> {
    default_backend: Option<&'a str>,
    backends: Vec<BackendEntry<'a>>,
}

#[derive(Serialize)]
struct BackendEntry<'a> {
    #[serde(flatten)]
    description: &'a systemone_core::BackendDescription,
    enabled: bool,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<&'a systemone_core::Capabilities>,
}

async fn backends(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = authorization_error(&state, &headers) {
        return response;
    }
    let registry = &state.registry;
    let backends = registry
        .descriptions()
        .iter()
        .map(|description| BackendEntry {
            description,
            enabled: registry.capabilities(&description.id).is_some(),
            ready: registry.is_ready(&description.id),
            capabilities: registry.capabilities(&description.id),
        })
        .collect();
    json_response(
        StatusCode::OK,
        &BackendsEnvelope {
            default_backend: registry.default_backend().map(|id| id.as_str()),
            backends,
        },
    )
}

/// Combine body and header selectors; both present must match.
fn selector(body: Option<&str>, headers: &HeaderMap) -> Result<Option<String>, WireError> {
    let header = match headers.get(BACKEND_HEADER) {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| WireError {
                    error_type: "validation_error",
                    message: "X-SystemOne-Backend header must be ASCII".to_owned(),
                })?
                .trim()
                .to_owned(),
        ),
    };
    match (body, header) {
        (None, None) => Ok(None),
        (Some(body), None) => Ok(Some(body.to_owned())),
        (None, Some(header)) => Ok(Some(header)),
        (Some(body), Some(header)) if body == header => Ok(Some(header)),
        (Some(body), Some(header)) => Err(WireError {
            error_type: "validation_error",
            message: format!(
                "backend selector conflict: body {body:?} vs X-SystemOne-Backend {header:?}"
            ),
        }),
    }
}

async fn system_one(State(state): State<AppState>, request: Request) -> Response {
    let started = Instant::now();
    if let Some(response) = authorization_error(&state, request.headers()) {
        return response;
    }
    if !has_json_content_type(request.headers()) {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Content-Type must be application/json",
        );
    }
    let deadline = started + state.request_timeout;
    let remaining = match deadline.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => remaining,
        _ => return host_error(&HostError::Timeout),
    };
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(remaining, to_bytes(body, state.max_body_bytes)).await {
        Err(_) => return host_error(&HostError::Timeout),
        Ok(Err(error)) => {
            tracing::debug!(error = %error, "HTTP request body rejected");
            if std::error::Error::source(&error)
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                return api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    &format!(
                        "request body exceeds the {} byte limit",
                        state.max_body_bytes
                    ),
                );
            }
            return api_error(
                StatusCode::BAD_REQUEST,
                "body_read_error",
                "request body could not be read",
            );
        }
        Ok(Ok(bytes)) => bytes,
    };
    let parsed = match wire::parse_request(&bytes) {
        Ok(parsed) => parsed,
        Err(error) => {
            let status = if error.error_type == "invalid_json" {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            return api_error(status, error.error_type, &error.message);
        }
    };
    let selector = match selector(parsed.backend.as_deref(), &parts.headers) {
        Ok(selector) => selector,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.error_type, &error.message),
    };
    let backend = match state.registry.select(selector.as_deref()) {
        Ok(backend) => backend,
        Err(error) => return host_error(&error),
    };
    let request_id = format!("s1-{}", state.registry.next_request_id());
    match state
        .registry
        .evaluate(&backend, parsed.request, request_id.clone(), deadline)
        .await
    {
        Ok(evaluated) => success_response(&evaluated, &request_id),
        Err(error) => {
            let mut response = host_error(&error);
            insert_safe_header(response.headers_mut(), BACKEND_HEADER, backend.as_str());
            insert_safe_header(
                response.headers_mut(),
                "x-systemone-request-id",
                &request_id,
            );
            response
        }
    }
}

fn success_response(evaluated: &Evaluated, request_id: &str) -> Response {
    let body = wire::render_response(&evaluated.response);
    let mut response = json_response(StatusCode::OK, &body);
    let headers = response.headers_mut();
    insert_safe_header(headers, BACKEND_HEADER, evaluated.backend.as_str());
    insert_safe_header(headers, "x-systemone-model", &evaluated.response.model);
    insert_safe_header(headers, "x-systemone-request-id", request_id);
    insert_safe_header(
        headers,
        "x-systemone-elapsed-ms",
        &evaluated.elapsed.as_millis().to_string(),
    );
    let diagnostics = &evaluated.response.diagnostics;
    if let Some(execution) = &diagnostics.execution {
        insert_safe_header(headers, "x-systemone-execution", execution);
    }
    if let Some(fallback) = &diagnostics.fallback {
        insert_safe_header(headers, "x-systemone-fallback", fallback);
    }
    if let Some(status) = &diagnostics.probability_status {
        insert_safe_header(headers, "x-systemone-probability-status", status);
    }
    if let Some(id) = &diagnostics.provider_request_id {
        insert_safe_header(headers, "x-systemone-provider-request-id", id);
    }
    response
}

/// Map a host error to the compatible error envelope. Messages for
/// validation/unsupported errors are shown; internal failures are not.
pub fn host_error(error: &HostError) -> Response {
    match error {
        HostError::Validation(message) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            message,
        ),
        HostError::Unsupported(message) => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "unsupported", message)
        }
        HostError::NotFound(message) => api_error(StatusCode::NOT_FOUND, "not_found", message),
        HostError::Unavailable(message) => {
            tracing::error!(error = %message, "backend unavailable");
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_unavailable",
                "the selected backend is unavailable",
            )
        }
        HostError::Overloaded => {
            let mut response = api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "overloaded",
                "all admission slots for this backend are busy",
            );
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_static(RETRY_AFTER_SECS),
            );
            response
        }
        HostError::Timeout => api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "request_timeout",
            "the whole-request deadline elapsed",
        ),
        HostError::Cancelled => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "request_canceled",
            "request was canceled before inference completed",
        ),
        HostError::Upstream {
            status,
            code,
            message,
        } => {
            let status = status
                .and_then(|status| StatusCode::from_u16(status).ok())
                .filter(|status| status.is_client_error() || status.is_server_error())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            api_error(status, code.as_deref().unwrap_or("upstream_error"), message)
        }
        HostError::Internal(message) => {
            tracing::error!(error = %message, "inference failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "inference_error",
                "inference failed; see server diagnostics",
            )
        }
    }
}

fn authorization_error(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let secret = state.api_key.as_ref()?;
    let expected = format!("Bearer {secret}");
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value.as_bytes(), expected.as_bytes()));
    if authorized {
        None
    } else {
        let mut response = api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "a valid bearer token is required",
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        Some(response)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn insert_safe_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    let bounded: String = value
        .chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .take(256)
        .collect();
    if let Ok(value) = HeaderValue::from_str(&bounded) {
        headers.insert(header::HeaderName::from_static(name), value);
    }
}

async fn method_not_allowed(method: Method) -> Response {
    api_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        &format!("HTTP method {method} is not supported for this route"),
    )
}

async fn not_found() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "requested route was not found",
    )
}

#[derive(Serialize)]
struct ApiErrorBody<'a> {
    error_type: &'a str,
    message: &'a str,
}

fn api_error(status: StatusCode, error_type: &str, message: &str) -> Response {
    json_response(
        status,
        &ApiErrorBody {
            error_type,
            message,
        },
    )
}

fn json_value(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    (status, Json(value)).into_response()
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
