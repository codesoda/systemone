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
    /// False when the peer closed before `Content-Length` bytes arrived.
    /// The mock records what it got instead of failing its thread.
    complete: bool,
}

impl Recorded {
    fn json(&self) -> serde_json::Value {
        assert!(
            self.complete,
            "request body was truncated: {} of the announced bytes arrived",
            self.body.len()
        );
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
    // A peer can close early, so the announced length is a claim, not a
    // fact. Bound the slice by what arrived; an out-of-range index here
    // would panic the accept thread and hang every later test.
    let announced_end = header_end.checked_add(content_length)?;
    let body_end = announced_end.min(buffer.len());
    let body = String::from_utf8_lossy(buffer.get(header_end..body_end)?).to_string();
    Some(Recorded {
        method,
        path,
        authorization,
        content_type,
        body,
        complete: body_end == announced_end,
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

/// A request with one choice question that declares three labels.
fn three_label_request() -> DecisionRequest {
    let body = br#"{"state":"s","questions":{"pick":{"type":"choice","criteria":{"alpha":null,"beta":null,"gamma":null}}}}"#;
    wire::parse_request(body)
        .expect("parse three-label request")
        .request
}

fn evaluate(host: &mut TypesafeHost) -> Result<DecisionResponse, HostError> {
    evaluate_with(host, &request())
}

fn evaluate_with(
    host: &mut TypesafeHost,
    request: &DecisionRequest,
) -> Result<DecisionResponse, HostError> {
    use systemone_core::DecisionHost;
    host.evaluate(request, &context_no_deadline())
}

/// Evaluate [`request`] against one scripted upstream body.
fn evaluate_reply(body: &str) -> Result<DecisionResponse, HostError> {
    let server = MockServer::start(Script::Reply(200, body.to_owned()));
    let mut host = host(&server.base());
    evaluate(&mut host)
}

fn expect_invalid_upstream_body(error: &HostError) -> &str {
    let HostError::Upstream {
        status,
        code,
        message,
    } = error
    else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(200));
    assert_eq!(code.as_deref(), Some("invalid_upstream_body"));
    message
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
    assert_unavailable(backend.load_with_key(base_url, None));

    // Variable empty: same.
    assert_unavailable(backend.load_with_key(base_url, Some(String::new())));

    // A real key loads a host against a loopback base URL.
    let host = backend
        .load_with_key(base_url, Some("sk-test-not-a-real-credential".to_owned()))
        .expect("load with key");
    let _ = host;
}

/// `s1 backends` must not call a backend available that `load` refuses.
/// The credential is the only precondition a hosted passthrough has; the
/// adapter needs no build feature and no local model, and it never spends
/// a billed request to probe reachability.
#[test]
fn availability_reports_the_missing_credential() {
    let backend = backend(vec![]);
    assert_eq!(backend.unavailable_reason_for(Some("sk-test")), None);

    let unset = backend
        .unavailable_reason_for(None)
        .expect("unset key is not available");
    assert!(unset.contains("TYPESAFE_API_KEY"), "got {unset}");
    assert!(unset.contains("api_key_env"), "got {unset}");

    let empty = backend
        .unavailable_reason_for(Some("  "))
        .expect("empty key is not available");
    assert!(empty.contains("set but holds no usable key"), "got {empty}");

    // The reason repeats what `load` would say, so the two never disagree.
    // A missing precondition is `Unavailable`, as it is for the local
    // kinds; it is not an operator configuration error.
    let Err(load_error) = backend.load_with_key("http://127.0.0.1:9", None) else {
        panic!("load must refuse a missing credential");
    };
    let HostError::Unavailable(message) = load_error else {
        panic!("expected an unavailable error, got {load_error:?}");
    };
    assert_eq!(message, unset);
}

fn assert_unavailable(result: Result<Box<dyn systemone_core::DecisionHost>, HostError>) {
    match result {
        Err(HostError::Unavailable(_)) => {}
        Err(other) => panic!("expected unavailable error, got {other:?}"),
        Ok(_) => panic!("expected unavailable error, load unexpectedly succeeded"),
    }
}

#[test]
fn load_builds_host_and_describe_hides_the_secret() {
    let backend = backend(vec!["alias-a".to_owned()]);
    assert_eq!(backend.kind(), systemone_core::ProviderKind::Typesafe);
    assert_eq!(backend.id().as_str(), "direct");
    let description = backend.describe();
    // Availability follows the credential, exactly as `load` does. The
    // test reads the environment; it never writes it.
    let key_usable = std::env::var("TYPESAFE_API_KEY")
        .map(|key| !key.trim().is_empty())
        .unwrap_or(false);
    assert_eq!(description.available, key_usable);
    assert_eq!(description.unavailable_reason.is_none(), key_usable);
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

/// Floats in state reach the API unchanged. The live TypeSafe API answers
/// such a request with HTTP 200, so the adapter adds no rejection of its
/// own. OpenJev's integer-only state is an adapter limitation, not a
/// SystemOne rule (`docs/plans/cross-repo.md` §5).
#[test]
fn floats_in_state_are_forwarded_unchanged() {
    let body = br#"{"state":{"nested":{"count":1.5},"ratios":[0.25]},"questions":{"worth":{"type":"noul","criteria":{"true":"it works"}}}}"#;
    let request = wire::parse_request(body).expect("parse").request;
    let reply = r#"{"model":"jev-latest","answers":{"worth":{"type":"noul","noul":0.62}}}"#;
    let server = MockServer::start(Script::Reply(200, reply.to_owned()));
    let mut host = host(&server.base());
    systemone_core::DecisionHost::evaluate(&mut host, &request, &context_no_deadline())
        .expect("floats are forwarded, not rejected");
    let sent = server.single().json();
    assert_eq!(
        sent["state"],
        serde_json::json!({"nested":{"count":1.5},"ratios":[0.25]}),
        "state must reach the upstream byte-for-byte"
    );
}

/// The upstream reports probabilities at Jev wire precision: two
/// decimals, each entry rounded on its own. Three such entries can sum to
/// 0.99. That body is correct, and SystemOne already paid for it, so the
/// normalization check reads it at the precision it arrives in. A sum
/// that misses by more than the rounding explains still fails; SystemOne
/// never renormalizes either one.
#[test]
fn wire_rounded_distributions_are_accepted_but_broken_ones_are_not() {
    let rounded = r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.33,"beta":0.33,"gamma":0.33},"confidence":0.5}}}"#;
    let server = MockServer::start(Script::Reply(200, rounded.to_owned()));
    let mut rounded_host = host(&server.base());
    let response = evaluate_with(&mut rounded_host, &three_label_request())
        .expect("a wire-rounded body is not corrupt");
    let systemone_core::Answer::Choice(choice) = &response.answers[0].1 else {
        panic!("expected a choice answer");
    };
    // Passed through as reported: still 0.33 each, still summing to 0.99.
    assert_eq!(
        choice.probabilities,
        vec![
            ("alpha".to_owned(), 0.33),
            ("beta".to_owned(), 0.33),
            ("gamma".to_owned(), 0.33)
        ]
    );

    let broken = r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.60,"beta":0.25,"gamma":0.00},"confidence":0.5}}}"#;
    let broken_server = MockServer::start(Script::Reply(200, broken.to_owned()));
    let mut broken_host = host(&broken_server.base());
    let error = evaluate_with(&mut broken_host, &three_label_request())
        .expect_err("0.85 is not rounding drift");
    let HostError::Upstream { status, code, .. } = &error else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(200));
    assert_eq!(code.as_deref(), Some("invalid_upstream_body"));
}

#[test]
fn upstream_error_type_survives_a_non_string_message() {
    for body in [
        r#"{"error_type":"rate_limited","message":{"detail":"slow down"}}"#,
        r#"{"error_type":"rate_limited","message":null}"#,
        r#"{"error_type":"rate_limited"}"#,
    ] {
        let server = MockServer::start(Script::Reply(429, body.to_owned()));
        let mut host = host(&server.base());
        let error = evaluate(&mut host).expect_err("must fail");
        let HostError::Upstream {
            status,
            code,
            message,
        } = &error
        else {
            panic!("expected HostError::Upstream, got {error:?}");
        };
        assert_eq!(*status, Some(429));
        // The classification the upstream did report is kept.
        assert_eq!(code.as_deref(), Some("rate_limited"), "body {body}");
        assert!(
            message.contains("429"),
            "body {body} gave message {message}"
        );
    }
}

/// A truncated request body must not panic the accept thread. A panic
/// there would take the listener down and hang every later connection.
#[test]
fn truncated_request_body_does_not_break_the_mock_listener() {
    use std::net::TcpStream;

    let server = MockServer::start(Script::Reply(200, upstream_ok()));
    let mut raw = TcpStream::connect(server.addr).expect("connect");
    raw.write_all(
        b"POST /v1/systemone HTTP/1.1\r\nHost: mock\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{\"model\":\"jev\",",
    )
    .expect("write truncated request");
    raw.shutdown(std::net::Shutdown::Write).expect("half close");
    let mut ignored = Vec::new();
    let _ = raw.read_to_end(&mut ignored);
    drop(raw);

    // The listener recorded what arrived and stayed alive: a normal call
    // through the same server still succeeds.
    let mut host = host(&server.base());
    let response = evaluate(&mut host).expect("listener still serves");
    assert_eq!(response.model, "jev-latest");

    let recorded = server.recorded.lock().expect("recorded lock");
    assert_eq!(recorded.len(), 2, "both connections were recorded");
    assert!(!recorded[0].complete, "first body was truncated");
    assert_eq!(recorded[0].body, r#"{"model":"jev","#);
    assert!(recorded[1].complete);
}

#[test]
fn upstream_detail_string_error_passes_through() {
    let server = MockServer::start(Script::Reply(
        400,
        r#"{"detail":"Noul question must have criteria or instructions: q"}"#.to_owned(),
    ));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream {
        status,
        code,
        message,
    } = &error
    else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(400));
    assert_eq!(code.as_deref(), Some("upstream_error"));
    assert!(message.contains("Noul question must have criteria or instructions"));
}

#[test]
fn upstream_detail_array_error_passes_through() {
    let server = MockServer::start(Script::Reply(
        422,
        r#"{"detail":[{"type":"missing","loc":["body","model"],"msg":"Field required"}]}"#
            .to_owned(),
    ));
    let mut host = host(&server.base());
    let error = evaluate(&mut host).expect_err("must fail");
    let HostError::Upstream {
        status,
        code,
        message,
    } = &error
    else {
        panic!("expected HostError::Upstream, got {error:?}");
    };
    assert_eq!(*status, Some(422));
    assert_eq!(code.as_deref(), Some("validation_error"));
    assert!(message.contains("Field required"));
}

/// SystemOne answers with one key order for every backend. The upstream
/// reports the labels of this choice question in reverse, and the adapter
/// returns them in the order the request declared. Only the position
/// moves: every value is the one the upstream reported.
#[test]
fn choice_probabilities_follow_the_declared_label_order() {
    let reversed = r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"beta":0.25,"alpha":0.75},"confidence":0.5},"worth":{"type":"noul","noul":0.62}}}"#;
    let response = evaluate_reply(reversed).expect("upstream success");
    let systemone_core::Answer::Choice(choice) = &response.answers[0].1 else {
        panic!("expected a choice answer");
    };
    assert_eq!(
        choice.probabilities,
        vec![("alpha".to_owned(), 0.75), ("beta".to_owned(), 0.25)],
        "declared order is alpha, beta"
    );
    assert_eq!(choice.choice, "alpha");
    assert_eq!(choice.confidence, Some(0.5));
}

/// The `answers` object follows the request's question order too.
#[test]
fn answers_follow_the_request_question_order() {
    let swapped = r#"{"model":"jev-latest","answers":{"worth":{"type":"noul","noul":0.62},"pick":{"type":"choice","choice":"beta","probabilities":{"alpha":0.25,"beta":0.75}}}}"#;
    let response = evaluate_reply(swapped).expect("upstream success");
    let answered: Vec<&str> = response.answers.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        answered,
        vec!["pick", "worth"],
        "request order is pick, worth"
    );
    let systemone_core::Answer::Noul(noul) = &response.answers[1].1 else {
        panic!("expected a noul answer");
    };
    assert!((noul.probability_true - 0.62).abs() < 1e-9);
}

/// Reordering is the only repair SystemOne does. A body that answers other
/// questions, other labels, or the wrong primitive is refused whole; no
/// label is invented, dropped or renamed to make it fit.
#[test]
fn answer_or_label_set_mismatch_is_an_invalid_upstream_body() {
    for (case, body) in [
        (
            "renamed label",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.75,"gamma":0.25}},"worth":{"type":"noul","noul":0.62}}}"#,
        ),
        (
            "extra label",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.5,"beta":0.25,"gamma":0.25}},"worth":{"type":"noul","noul":0.62}}}"#,
        ),
        (
            "missing label",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":1.0}},"worth":{"type":"noul","noul":0.62}}}"#,
        ),
        (
            "missing answer",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.75,"beta":0.25}}}}"#,
        ),
        (
            "extra answer",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.75,"beta":0.25}},"worth":{"type":"noul","noul":0.62},"spare":{"type":"noul","noul":0.1}}}"#,
        ),
        (
            "wrong primitive",
            r#"{"model":"jev-latest","answers":{"pick":{"type":"noul","noul":0.5},"worth":{"type":"noul","noul":0.62}}}"#,
        ),
    ] {
        let error = evaluate_reply(body).expect_err(case);
        let message = expect_invalid_upstream_body(&error);
        assert!(!message.is_empty(), "{case} must say what is wrong");
    }
}

/// A pinned instance: the concrete version is the served model and the
/// upstream alias is accepted as a request selector.
fn pinned_host(base: &Url) -> TypesafeHost {
    TypesafeHost::new(
        RemoteTransport::new().expect("client"),
        base.clone(),
        "jev-1.13.0".to_owned(),
        vec!["jev-latest".to_owned()],
        TEST_KEY.to_owned(),
    )
}

fn pinned_backend() -> TypesafeBackend {
    TypesafeBackend::new(
        BackendId::new("hosted").expect("backend id"),
        Some("jev-1.13.0"),
        vec!["jev-latest".to_owned()],
        &TypesafeSettings {
            api_key_env: "TYPESAFE_API_KEY".to_owned(),
        },
    )
    .expect("backend")
}

/// A request for the configured alias is sent upstream as the pinned
/// version, so the upstream answers with the identity `/v1/models`
/// advertises. The alias never travels; the upstream would otherwise
/// resolve it to a version of its own choice.
#[test]
fn a_configured_alias_is_resolved_to_the_pinned_model_before_the_call() {
    let upstream = r#"{"model":"jev-1.13.0","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.75,"beta":0.25}},"worth":{"type":"noul","noul":0.62}}}"#;
    let server = MockServer::start(Script::Reply(200, upstream.to_owned()));
    let mut host = pinned_host(&server.base());
    let aliased = DecisionRequest {
        model: Some("jev-latest".to_owned()),
        ..request()
    };
    let response = evaluate_with(&mut host, &aliased).expect("alias resolves");

    let sent = server.single().json();
    assert_eq!(
        sent["model"], "jev-1.13.0",
        "the alias never leaves SystemOne"
    );
    // The upstream identity passes through, and it is the name the
    // catalogue card carries.
    assert_eq!(response.model, "jev-1.13.0");
    assert_eq!(
        response.model,
        systemone_core::DecisionHost::capabilities(&host).model.id
    );
}

/// `GET /v1/models` serves exactly one card for a TypeSafe instance, named
/// after the configured model. The upstream catalogue is not consulted:
/// the base URL below is never contacted.
#[tokio::test]
async fn models_route_serves_one_card_named_after_the_configured_model() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request as HttpRequest, StatusCode},
    };
    use systemone_http::{AppState, Registry, RegistryEntry, ServeOptions, router};
    use tower::ServiceExt as _;

    /// Loads a real `TypesafeHost` against a base URL the test chooses.
    struct MockBaseBackend {
        inner: TypesafeBackend,
        base: String,
    }

    impl Backend for MockBaseBackend {
        fn id(&self) -> &BackendId {
            self.inner.id()
        }

        fn kind(&self) -> systemone_core::ProviderKind {
            self.inner.kind()
        }

        fn describe(&self) -> systemone_core::BackendDescription {
            self.inner.describe()
        }

        fn load(&self) -> Result<Box<dyn systemone_core::DecisionHost>, HostError> {
            self.inner
                .load_with_key(&self.base, Some(TEST_KEY.to_owned()))
        }
    }

    let backend = Arc::new(MockBaseBackend {
        inner: pinned_backend(),
        // Discard port: a catalogue request would fail loudly.
        base: "http://127.0.0.1:9".to_owned(),
    });
    let registry = Registry::load(
        vec![RegistryEntry {
            backend: Arc::clone(&backend) as Arc<dyn Backend>,
            queue_capacity: 1,
            max_in_flight: 1,
        }],
        vec![backend.describe()],
        Some(BackendId::new("hosted").expect("backend id")),
        4,
    )
    .expect("registry loads the hosted backend");
    let options = ServeOptions::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        8080,
        5,
        1024,
        None,
    )
    .expect("serve options");
    let app = router(AppState::new(Arc::new(registry), &options));

    let response = app
        .oneshot(
            HttpRequest::get("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route answers");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    let models = body["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1, "one instance serves one model: {body}");
    assert_eq!(models[0]["name"], "jev-1.13.0");
}
