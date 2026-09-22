//! Credential-free tests against a loopback mock TypeSafe server.
//!
//! Tests never read real credentials. The mock server records exactly what
//! the adapter sent and serves scripted replies, including slow, huge, and
//! error ones.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use reqwest::Url;
use systemone_core::{
    Backend, BackendId, CallContext, DecisionRequest, DecisionResponse, Extension, HostError,
};
use systemone_http::wire;

use super::{
    TypesafeBackend, TypesafeHost, TypesafeSettings, parse_models_response,
    typesafe::{BASE_URL, DEFAULT_MODEL, TypesafeModel},
};
use crate::transport::{MAX_RESPONSE_BYTES, RemoteTransport};

/// What the mock server does with a request.
enum Script {
    /// Reply with this status and body.
    Reply(u16, String),
    /// Sleep first, then reply (drives the timeout path).
    DelayedReply(Duration, u16, String),
    /// Reply with a 200 and a body far beyond the byte cap.
    Huge,
}

/// A recorded request as the mock saw it.
#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    content_type: Option<String>,
    body: String,
}

impl Recorded {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("sent body is JSON")
    }
}

struct MockServer {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let addr = listener.local_addr().expect("local address");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                if let Some(recorded_request) = read_request(&mut stream) {
                    sink.lock().expect("recorded lock").push(recorded_request);
                    match &script {
                        Script::Reply(status, body) => {
                            write_reply(&mut stream, *status, body.as_bytes());
                        }
                        Script::DelayedReply(delay, status, body) => {
                            thread::sleep(*delay);
                            write_reply(&mut stream, *status, body.as_bytes());
                        }
                        Script::Huge => {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                MAX_RESPONSE_BYTES + 1024 * 1024
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let filler = vec![b'x'; 1024 * 1024];
                            for _ in 0..10 {
                                if stream.write_all(&filler).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Self { addr, recorded }
    }

    fn base(&self) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}", self.addr.port())).expect("base URL")
    }

    fn single(&self) -> Recorded {
        let recorded = self.recorded.lock().expect("recorded lock");
        assert_eq!(recorded.len(), 1, "expected exactly one upstream request");
        recorded[0].clone()
    }
}

fn write_reply(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status_text(status),
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        529 => "Site Overloaded",
        _ => "Status",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header.split("\r\n");
    let request_line = lines.next()?;
    let mut request_line_parts = request_line.split(' ');
    let method = request_line_parts.next()?.to_owned();
    let path = request_line_parts.next()?.to_owned();
    let mut authorization = None;
    let mut content_type = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.to_owned()),
            "content-type" => content_type = Some(value.to_owned()),
            "content-length" => content_length = value.parse().ok()?,
            _ => {}
        }
    }
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body =
        String::from_utf8_lossy(&buffer[header_end..header_end + content_length]).to_string();
    Some(Recorded {
        method,
        path,
        authorization,
        content_type,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A non-secret test key. Its value only matters for the redaction test.
const TEST_KEY: &str = "sk-test-not-a-real-credential";

fn host(base: &Url) -> TypesafeHost {
    TypesafeHost::new(
        RemoteTransport::new().expect("client"),
        base.clone(),
        "jev-latest".to_owned(),
        vec!["alias-a".to_owned()],
        TEST_KEY.to_owned(),
    )
}

fn backend(aliases: Vec<String>) -> TypesafeBackend {
    TypesafeBackend::new(
        BackendId::new("direct").expect("backend id"),
        None,
        aliases,
        &TypesafeSettings {
            api_key_env: "TYPESAFE_API_KEY".to_owned(),
        },
    )
    .expect("backend")
}

/// A neutral request: one choice question and one noul question.
fn request() -> DecisionRequest {
    let body = br#"{"backend":"direct","state":{"topic":"migration"},"questions":{"pick":{"type":"choice","criteria":{"alpha":null,"beta":null}},"worth":{"type":"noul","criteria":{"true":"it works"}}}}"#;
    wire::parse_request(body)
        .expect("parse test request")
        .request
}

fn context_no_deadline() -> CallContext {
    CallContext::new("test-call", None)
}

fn upstream_ok() -> String {
    r#"{"id":"resp-7","model":"jev-latest","usage":{"input_tokens":120,"output_tokens":30,"cost":0.0025},"answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.75,"beta":0.25},"confidence":0.5},"worth":{"type":"noul","noul":0.62}}}"#.to_owned()
}

fn evaluate(host: &mut TypesafeHost) -> Result<DecisionResponse, HostError> {
    use systemone_core::DecisionHost;
    host.evaluate(&request(), &context_no_deadline())
}

#[test]
fn success_passthrough_strips_selectors_and_sends_bearer() {
    let server = MockServer::start(Script::Reply(200, upstream_ok()));
    let mut host = host(&server.base());
    let response = evaluate(&mut host).expect("upstream success");

    // Upstream answers, usage and model identity pass through verbatim.
    assert_eq!(response.model, "jev-latest");
    assert_eq!(
        response.diagnostics.provider_request_id,
        Some("resp-7".to_owned())
    );
    let usage = &response.usage;
    assert_eq!(usage.input_tokens, Some(120));
    assert_eq!(usage.output_tokens, Some(30));
    let cost = usage.cost.expect("usage cost passthrough");
    assert!((cost - 0.0025).abs() < 1e-9);

    let recorded = server.single();
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/systemone");
    assert_eq!(recorded.authorization, Some(format!("Bearer {TEST_KEY}")));
    assert_eq!(recorded.content_type.as_deref(), Some("application/json"));
    let body = recorded.json();
    // Routing selector never leaves the service.
    assert!(body.get("backend").is_none());
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], serde_json::json!({"topic":"migration"}));
}

#[test]
fn upstream_error_statuses_pass_through_sanitized() {
    for status in [401, 422, 429, 529] {
        let server = MockServer::start(Script::Reply(
            status,
            format!(r#"{{"error_type":"mock_error","message":"mock failure {status}"}}"#),
        ));
        let mut host = host(&server.base());
        let error = evaluate(&mut host).expect_err("must fail");
        let HostError::Upstream {
            status: reply_status,
            code,
            message,
        } = &error
        else {
            panic!("expected HostError::Upstream for {status}, got {error:?}");
        };
        assert_eq!(*reply_status, Some(status));
        assert_eq!(code.as_deref(), Some("mock_error"));
        assert!(message.contains(&format!("mock failure {status}")));
    }
}

#[test]
fn upstream_error_code_comes_from_error_type() {
    let server = MockServer::start(Script::Reply(
        429,
        r#"{"error_type":"rate_limited","message":"slow down"}"#.to_owned(),
    ));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { status, code, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(429));
    assert_eq!(code.as_deref(), Some("rate_limited"));
}

#[test]
fn upstream_error_without_envelope_still_reports_status() {
    let server = MockServer::start(Script::Reply(500, "not json at all".to_owned()));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { status, code, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(500));
    assert!(code.is_none());
}

#[test]
fn secret_is_redacted_from_upstream_echoed_messages() {
    let server = MockServer::start(Script::Reply(
        500,
        format!(r#"{{"error_type":"internal","message":"key {TEST_KEY} leaked in logs"}}"#),
    ));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { message, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert!(message.contains("[redacted]"));
    assert!(!message.contains(TEST_KEY));
}

#[test]
fn control_characters_are_stripped_from_error_messages() {
    let server = MockServer::start(Script::Reply(
        500,
        r#"{"error_type":"internal","message":"line\u0007break\u001fx"}"#.to_owned(),
    ));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { message, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert!(!message.chars().any(char::is_control));
}

#[test]
fn slow_upstream_times_out_without_retry() {
    let server = MockServer::start(Script::DelayedReply(
        Duration::from_secs(30),
        200,
        upstream_ok(),
    ));
    let mut host = host(&server.base());
    let context = CallContext::new(
        "test-call",
        Some(std::time::Instant::now() + Duration::from_millis(300)),
    );
    use systemone_core::DecisionHost;
    let error = host
        .evaluate(&request(), &context)
        .expect_err("must time out");
    assert!(matches!(error, HostError::Timeout), "got {error:?}");
    // Exactly one request was sent: no billed retry.
    assert_eq!(server.single().method, "POST");
}

#[test]
fn oversized_response_body_is_rejected() {
    let server = MockServer::start(Script::Huge);
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { status, code, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, None);
    assert_eq!(code.as_deref(), Some("response_too_large"));
}

#[test]
fn malformed_success_body_is_rejected_not_repaired() {
    for bad in [
        String::from("this is not json"),
        String::from(r#"{"model":"","answers":{}}"#),
        String::from(
            r#"{"model":"jev-latest","answers":{"pick":{"choice":"alpha","probabilities":{"alpha":0.75,"beta":0.26}}}}"#,
        ),
        String::new(),
    ] {
        let server = MockServer::start(Script::Reply(200, bad));
        let mut host = host(&server.base());
        let error = evaluate(&mut host).expect_err("must fail");
        let HostError::Upstream { status, code, .. } = &error else {
            panic!("expected HostError::Upstream, got {error:?}");
        };
        assert_eq!(*status, Some(200));
        assert_eq!(code.as_deref(), Some("invalid_upstream_body"));
    }
}

#[test]
fn redirects_are_not_followed() {
    let server = MockServer::start(Script::Reply(301, String::new()));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream { status, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(301));
}

#[test]
fn model_selectors_resolve() {
    let server = MockServer::start(Script::Reply(200, upstream_ok()));
    let mut host = host(&server.base());

    // No selector: default id.
    let response = evaluate(&mut host).expect("default model");
    assert_eq!(response.model, "jev-latest");

    // Alias resolves to the same upstream model id.
    let aliased = request();
    let aliased = systemone_core::DecisionRequest {
        model: Some("alias-a".to_owned()),
        ..aliased
    };
    use systemone_core::DecisionHost;
    let response = host
        .evaluate(&aliased, &context_no_deadline())
        .expect("alias model");
    assert_eq!(response.model, "jev-latest");

    // Unknown model is rejected before any request is sent.
    let unknown = request();
    let unknown = systemone_core::DecisionRequest {
        model: Some("other-vendor".to_owned()),
        ..unknown
    };
    let error = host
        .evaluate(&unknown, &context_no_deadline())
        .expect_err("unknown model");
    assert!(matches!(error, HostError::NotFound(_)), "got {error:?}");
}

#[test]
fn model_catalogue_parses_and_is_not_normalized() {
    let server = MockServer::start(Script::Reply(
        200,
        r#"[{"name":"jev-latest","description":"TypeSafe Jev","release_date":"2026-01-15","extra":"ignored"},{"name":"jev-preview"}]"#.to_owned(),
    ));
    let host = host(&server.base());
    let models = host.list_models(Duration::from_secs(5)).expect("catalogue");
    assert_eq!(
        models,
        vec![
            TypesafeModel {
                name: "jev-latest".to_owned(),
                description: "TypeSafe Jev".to_owned(),
                release_date: "2026-01-15".to_owned(),
            },
            TypesafeModel {
                name: "jev-preview".to_owned(),
                description: String::new(),
                release_date: "unknown".to_owned(),
            },
        ]
    );
    let recorded = server.single();
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/v1/models");
    assert_eq!(recorded.authorization, Some(format!("Bearer {TEST_KEY}")));
}

#[test]
fn model_catalogue_requires_object_entries() {
    assert!(parse_models_response(br#"["jev-latest"]"#).is_err());
    assert!(parse_models_response(br#"[{"description":"no name"}]"#).is_err());
    assert!(parse_models_response(br#"{"object":true}"#).is_err());
    // An object without a models array, or a non-array models value, is rejected.
    assert!(parse_models_response(br#"{"models":{"object":true}}"#).is_err());
    // A bare array is still accepted (and validated) for compatibility.
    assert!(parse_models_response(br#"[{"name":"jev-latest"},{"name":"jev-preview"}]"#).is_ok());
    assert!(parse_models_response(b"\xff\xfe").is_err());
    assert!(parse_models_response(br#"[{"name":""}]"#).is_err());
    assert!(parse_models_response(br#"[]"#).is_ok());
}

#[test]
fn https_is_required_outside_loopback() {
    assert!(RemoteTransport::validate_base_url(&Url::parse(BASE_URL).expect("base URL")).is_ok());
    assert!(
        RemoteTransport::validate_base_url(&Url::parse("http://127.0.0.1:9").expect("url")).is_ok()
    );
    assert!(
        RemoteTransport::validate_base_url(&Url::parse("http://localhost:9").expect("url")).is_ok()
    );
    assert!(
        RemoteTransport::validate_base_url(&Url::parse("http://example.com").expect("url"))
            .is_err()
    );
    assert!(
        RemoteTransport::validate_base_url(&Url::parse("ftp://example.com").expect("url")).is_err()
    );
}

#[test]
fn load_requires_environment_key() {
    // Tests never mutate the environment (that is unsound in a threaded
    // test binary): they drive `load_with_key` with the credential the
    // loader would have resolved from `settings.api_key_env`.
    let backend = backend(vec![]);
    let base_url = "http://127.0.0.1:9"; // never contacted

    // Variable unset: load fails without contacting the network.
    assert_validation_error(backend.load_with_key(base_url, None));

    // Variable empty: same.
    assert_validation_error(backend.load_with_key(base_url, Some(String::new())));

    // A real key loads a host against a loopback base URL.
    let host = backend
        .load_with_key(base_url, Some("sk-test-not-a-real-credential".to_owned()))
        .expect("load with key");
    let _ = host;
}

fn assert_validation_error(result: Result<Box<dyn systemone_core::DecisionHost>, HostError>) {
    match result {
        Err(HostError::Validation(_)) => {}
        Err(other) => panic!("expected validation error, got {other:?}"),
        Ok(_) => panic!("expected validation error, load unexpectedly succeeded"),
    }
}

#[test]
fn load_builds_host_and_describe_hides_the_secret() {
    let backend = backend(vec!["alias-a".to_owned()]);
    assert_eq!(backend.kind(), systemone_core::ProviderKind::Typesafe);
    assert_eq!(backend.id().as_str(), "direct");
    let description = backend.describe();
    assert!(description.available);
    assert_eq!(description.model, DEFAULT_MODEL);
    assert_eq!(
        description.settings,
        serde_json::json!({"api_key_env": "TYPESAFE_API_KEY"})
    );
    assert!(
        !serde_json::to_string(&description.settings)
            .expect("settings json")
            .contains(TEST_KEY)
    );
    // Hosted passthroughs manage no local model cache.
    assert!(matches!(
        backend.model_store(),
        Extension::Unsupported { .. }
    ));
}

#[test]
fn settings_reject_secret_values_and_bad_env_names() {
    let id = BackendId::new("direct").expect("backend id");
    assert!(
        TypesafeBackend::new(
            id.clone(),
            None,
            vec![],
            &TypesafeSettings {
                api_key_env: "  ".to_owned(),
            },
        )
        .is_err()
    );
    assert!(
        TypesafeBackend::new(
            id.clone(),
            None,
            vec![],
            &TypesafeSettings {
                api_key_env: "KEY=s3cret".to_owned(),
            },
        )
        .is_err()
    );
    assert!(
        TypesafeBackend::new(
            id,
            Some(""),
            vec![],
            &TypesafeSettings {
                api_key_env: "OK_VAR".to_owned(),
            },
        )
        .is_err()
    );
    assert!(serde_json::from_str::<TypesafeSettings>(r#"{"api_key_env":"X","extra":1}"#).is_err());
}
