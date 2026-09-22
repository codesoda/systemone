//! Service tests against the deterministic fake backend (no model, no network).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use serde_json::Value;
use systemone_core::{Backend, BackendId, HostError, Primitive};
use tower::ServiceExt as _;

use super::*;
use crate::{
    registry::{Registry, RegistryEntry},
    test_support::{FakeBackend, Gate},
};

fn registry(backends: Vec<FakeBackend>, default: Option<&str>, max_admitted: usize) -> Registry {
    let descriptions = backends.iter().map(Backend::describe).collect();
    let entries = backends
        .into_iter()
        .map(|backend| RegistryEntry {
            backend: Arc::new(backend) as Arc<dyn Backend>,
            queue_capacity: 1,
            max_in_flight: 1,
        })
        .collect();
    Registry::load(
        entries,
        descriptions,
        default.map(|id| BackendId::new(id).unwrap()),
        max_admitted,
    )
    .unwrap()
}

fn app(registry: Registry, api_key: Option<&str>, timeout_secs: u64) -> Router {
    let options = ServeOptions::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        8080,
        timeout_secs,
        1024,
        None,
    )
    .unwrap()
    .with_api_key(api_key);
    router(AppState::new(Arc::new(registry), &options))
}

async fn call(app: &Router, request: HttpRequest<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, body)
}

fn post(body: &str) -> HttpRequest<Body> {
    HttpRequest::post("/v1/systemone")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

const REQUEST: &str = r#"{"model":"jev-latest","state":"s","questions":{"q":{"type":"choice","criteria":{"a":null,"b":"B"}},"n":{"type":"noul"}}}"#;

#[tokio::test]
async fn loads_once_and_answers_repeated_calls_with_routing_headers() {
    let backend = FakeBackend::new("local");
    let state = Arc::clone(&backend.state);
    let app = app(registry(vec![backend], Some("local"), 4), None, 5);
    for _ in 0..3 {
        let (status, headers, body) = call(&app, post(REQUEST)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "fake-local");
        assert_eq!(body["answers"]["q"]["choice"], "a");
        assert_eq!(body["answers"]["q"]["probabilities"]["b"], 0.5);
        assert_eq!(body["answers"]["n"]["noul"], 0.25);
        assert_eq!(body["usage"]["input_tokens"], 7);
        assert_eq!(headers["x-systemone-backend"], "local");
        assert_eq!(headers["x-systemone-model"], "fake-local");
        assert!(headers.contains_key("x-systemone-request-id"));
        assert_eq!(
            headers["x-systemone-execution"],
            "requested=direct; effective=direct"
        );
    }
    let state = state.lock().unwrap();
    assert_eq!(state.loads, 1);
    assert_eq!(state.calls, 3);
    assert_eq!(state.seen_models[0].as_deref(), Some("jev-latest"));
}

#[tokio::test]
async fn selects_backends_by_body_or_header_and_rejects_conflicts() {
    let app = app(
        registry(
            vec![FakeBackend::new("local"), FakeBackend::new("other")],
            Some("local"),
            4,
        ),
        None,
        5,
    );
    let body_selected = REQUEST.replacen("{", r#"{"backend":"other","#, 1);
    let (status, headers, body) = call(&app, post(&body_selected)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-systemone-backend"], "other");
    assert_eq!(body["model"], "fake-other");

    let header_selected = HttpRequest::post("/v1/systemone")
        .header("content-type", "application/json")
        .header("x-systemone-backend", "other")
        .body(Body::from(REQUEST))
        .unwrap();
    let (status, headers, _) = call(&app, header_selected).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-systemone-backend"], "other");

    let conflict = HttpRequest::post("/v1/systemone")
        .header("content-type", "application/json")
        .header("x-systemone-backend", "local")
        .body(Body::from(body_selected))
        .unwrap();
    let (status, _, body) = call(&app, conflict).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_type"], "validation_error");

    let unknown = REQUEST.replacen("{", r#"{"backend":"nope","#, 1);
    let (status, _, body) = call(&app, post(&unknown)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_type"], "not_found");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("unknown backend")
    );

    // A model name never selects a backend; an unserved model is 404 like
    // Jev, which the official SDK maps to its not-found error path.
    let by_model = REQUEST.replace("jev-latest", "fake-other");
    let (status, _, body) = call(&app, post(&by_model)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "model never selects a backend"
    );
    assert_eq!(body["error_type"], "not_found");
    assert!(body["message"].as_str().unwrap().contains("not served"));
}

#[tokio::test]
async fn disabled_backends_are_listed_but_not_selectable() {
    let enabled = FakeBackend::new("local");
    let disabled = FakeBackend::new("cloud");
    let descriptions = vec![enabled.describe(), disabled.describe()];
    let registry = Registry::load(
        vec![RegistryEntry {
            backend: Arc::new(enabled),
            queue_capacity: 1,
            max_in_flight: 1,
        }],
        descriptions,
        Some(BackendId::new("local").unwrap()),
        4,
    )
    .unwrap();
    let app = app(registry, None, 5);
    let selected = REQUEST.replacen("{", r#"{"backend":"cloud","#, 1);
    let (status, _, body) = call(&app, post(&selected)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["message"].as_str().unwrap().contains("not enabled"));

    let (status, _, body) = call(
        &app,
        HttpRequest::get("/v1/backends")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_backend"], "local");
    let backends = body["backends"].as_array().unwrap();
    assert_eq!(backends.len(), 2);
    assert_eq!(backends[0]["id"], "local");
    assert_eq!(backends[0]["enabled"], true);
    assert_eq!(backends[0]["ready"], true);
    assert_eq!(backends[0]["capabilities"]["model"]["id"], "fake-local");
    assert_eq!(backends[1]["id"], "cloud");
    assert_eq!(backends[1]["enabled"], false);
    assert!(backends[1].get("capabilities").is_none());
}

#[tokio::test]
async fn models_route_is_per_backend_and_health_routes_work() {
    let app = app(
        registry(
            vec![FakeBackend::new("local"), FakeBackend::new("other")],
            Some("local"),
            4,
        ),
        None,
        5,
    );
    let (status, headers, body) = call(
        &app,
        HttpRequest::get("/v1/models").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["models"][0]["name"], "fake-local");
    assert_eq!(body["models"][0]["release_date"], "unknown");
    assert_eq!(headers["x-systemone-backend"], "local");
    let (_, _, body) = call(
        &app,
        HttpRequest::get("/v1/models?backend=other")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["models"][0]["name"], "fake-other");
    let (status, _, _) = call(
        &app,
        HttpRequest::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(
        &app,
        HttpRequest::get("/readyz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) =
        call(&app, HttpRequest::get("/nope").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_type"], "not_found");
    let (status, _, _) = call(
        &app,
        HttpRequest::get("/v1/systemone")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn bearer_auth_is_enforced_on_every_route_except_health() {
    let app = app(
        registry(vec![FakeBackend::new("local")], Some("local"), 4),
        Some("secret"),
        5,
    );
    let (status, headers, body) = call(&app, post(REQUEST)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_type"], "authentication_error");
    assert_eq!(headers["www-authenticate"], "Bearer");
    for route in ["/v1/models", "/v1/backends"] {
        let (status, _, _) = call(&app, HttpRequest::get(route).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{route}");
    }
    let (status, _, _) = call(
        &app,
        HttpRequest::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let authorized = HttpRequest::post("/v1/systemone")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(REQUEST))
        .unwrap();
    let (status, _, _) = call(&app, authorized).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn wire_errors_and_limits_map_to_documented_statuses() {
    let app = app(
        registry(vec![FakeBackend::new("local")], Some("local"), 4),
        None,
        5,
    );
    let (status, _, body) = call(&app, post("{")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_type"], "invalid_json");
    let (status, _, body) = call(&app, post(r#"{"state":"s","questions":{}}"#)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error_type"], "validation_error");
    let wrong_type = HttpRequest::post("/v1/systemone")
        .header("content-type", "text/plain")
        .body(Body::from(REQUEST))
        .unwrap();
    let (status, _, _) = call(&app, wrong_type).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let big = format!(
        r#"{{"state":"{}","questions":{{"q":{{"type":"noul"}}}}}}"#,
        "x".repeat(2000)
    );
    let (status, _, body) = call(&app, post(&big)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error_type"], "body_too_large");
    // Capability limits are checked before dispatch.
    let questions = (0..5)
        .map(|i| format!(r#""q{i}":{{"type":"noul"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let many = format!(r#"{{"state":"s","questions":{{{questions}}}}}"#);
    let (status, _, body) = call(&app, post(&many)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["message"].as_str().unwrap().contains("more than 4"));
}

#[tokio::test]
async fn unsupported_primitives_are_rejected_not_derived() {
    let mut backend = FakeBackend::new("local");
    backend.primitives = vec![Primitive::Choice];
    let app = app(registry(vec![backend], Some("local"), 4), None, 5);
    let (status, _, body) = call(&app, post(REQUEST)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error_type"], "unsupported");
    assert!(body["message"].as_str().unwrap().contains("noul"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overload_returns_429_and_a_slow_backend_does_not_block_another() {
    let gate = Arc::new(Gate::default());
    let mut slow = FakeBackend::new("slow");
    slow.gate = Some(Arc::clone(&gate));
    let fast = FakeBackend::new("fast");
    let app = app(registry(vec![slow, fast], Some("slow"), 8), None, 30);
    let first = {
        let app = app.clone();
        tokio::spawn(async move { call(&app, post(REQUEST)).await })
    };
    assert!(gate.wait_entered(1, Duration::from_secs(5)));
    // Queue capacity 1: the second call queues, the third is overloaded.
    let second = {
        let app = app.clone();
        tokio::spawn(async move { call(&app, post(REQUEST)).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, headers, body) = call(&app, post(REQUEST)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(headers["retry-after"], "1");
    let fast_request = REQUEST.replacen("{", r#"{"backend":"fast","#, 1);
    let (status, _, body) = call(&app, post(&fast_request)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    gate.release();
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    assert_eq!(second.await.unwrap().0, StatusCode::OK);
}

#[tokio::test]
async fn deadline_elapsing_while_queued_returns_504_and_permit_survives_until_completion() {
    let gate = Arc::new(Gate::default());
    let mut slow = FakeBackend::new("slow");
    slow.gate = Some(Arc::clone(&gate));
    let state = Arc::clone(&slow.state);
    let app = app(registry(vec![slow], Some("slow"), 8), None, 1);
    let (status, _, body) = call(&app, post(REQUEST)).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error_type"], "request_timeout");
    // The host is still inside the call; the worker has not moved on.
    assert_eq!(state.lock().unwrap().calls, 1);
    gate.release();
}

#[tokio::test]
async fn terminal_host_errors_mark_the_backend_not_ready() {
    let mut backend = FakeBackend::new("local");
    backend.fail_with = Some(HostError::unavailable("engine died"));
    let app = app(registry(vec![backend], Some("local"), 4), None, 5);
    let (status, _, body) = call(&app, post(REQUEST)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_type"], "backend_unavailable");
    assert!(!body["message"].as_str().unwrap().contains("engine died"));
    let (status, _, _) = call(
        &app,
        HttpRequest::get("/readyz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn load_failure_is_a_startup_error_and_unloads_earlier_backends() {
    let good = FakeBackend::new("good");
    let good_state = Arc::clone(&good.state);
    let mut bad = FakeBackend::new("bad");
    bad.fail_load = true;
    let descriptions = vec![good.describe(), bad.describe()];
    let error = Registry::load(
        vec![good, bad]
            .into_iter()
            .map(|backend| RegistryEntry {
                backend: Arc::new(backend) as Arc<dyn Backend>,
                queue_capacity: 1,
                max_in_flight: 1,
            })
            .collect(),
        descriptions,
        None,
        4,
    )
    .err()
    .unwrap();
    assert!(matches!(error, HostError::Unavailable(message) if message.contains("bad")));
    let state = good_state.lock().unwrap();
    assert_eq!(state.loads, 1);
    assert_eq!(state.shutdowns, 1);
}

#[test]
fn serve_options_require_auth_off_loopback_and_reject_bad_env() {
    let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let public = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
    assert!(ServeOptions::new(loopback, 8080, 1, 1, None).is_ok());
    assert!(ServeOptions::new(public, 8080, 1, 1, None).is_err());
    assert!(ServeOptions::new(loopback, 0, 1, 1, None).is_err());
    assert!(ServeOptions::new(loopback, 8080, 0, 1, None).is_err());
    assert!(ServeOptions::new(loopback, 8080, 1, 1, Some("SYSTEMONE_TEST_MISSING_KEY")).is_err());
}
