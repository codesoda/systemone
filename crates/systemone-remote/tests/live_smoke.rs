//! Opt-in, spend-bounded live smoke test for the TypeSafe adapter.
//!
//! Not run by default. This test makes two real calls to
//! `https://api.typesafe.ai`: one `GET /v1/models` for the catalogue and
//! one billed `POST /v1/systemone`. It therefore spends money, and runs
//! only when both gates are set:
//!
//! ```sh
//! TYPESAFE_API_KEY=... \
//! SYSTEMONE_LIVE_SMOKE=spend-acknowledged \
//! cargo test -p systemone-remote --test live_smoke -- --ignored
//! ```
//!
//! `SYSTEMONE_LIVE_SMOKE=spend-acknowledged` is the explicit acknowledgement
//! that this test bills the configured account.

use std::time::Duration;

use reqwest::Url;
use systemone_core::{
    Backend, BackendId, CallContext, DecisionHost, DecisionRequest, ProviderKind,
};
use systemone_http::wire;
use systemone_remote::transport::RemoteTransport;
use systemone_remote::{BASE_URL, DEFAULT_MODEL, TypesafeBackend, TypesafeHost, TypesafeSettings};

const ACKNOWLEDGEMENT: &str = "spend-acknowledged";

#[test]
#[ignore = "spends money; requires TYPESAFE_API_KEY and SYSTEMONE_LIVE_SMOKE=spend-acknowledged"]
fn live_typesafe_direct_round_trip() {
    if std::env::var("SYSTEMONE_LIVE_SMOKE").ok().as_deref() != Some(ACKNOWLEDGEMENT) {
        panic!(
            "this live smoke test spends money; set SYSTEMONE_LIVE_SMOKE={ACKNOWLEDGEMENT} \
             together with TYPESAFE_API_KEY to acknowledge and enable it"
        );
    }
    let api_key = std::env::var("TYPESAFE_API_KEY")
        .expect("TYPESAFE_API_KEY must be set for the live smoke test");
    if api_key.trim().is_empty() {
        panic!("TYPESAFE_API_KEY is set but empty");
    }
    let backend = TypesafeBackend::new(
        BackendId::new("direct").expect("backend id"),
        None,
        vec![],
        &TypesafeSettings {
            api_key_env: "TYPESAFE_API_KEY".to_owned(),
        },
    )
    .expect("backend");
    assert_eq!(backend.kind(), ProviderKind::Typesafe);
    let mut host = TypesafeHost::new(
        RemoteTransport::new().expect("remote client"),
        Url::parse(BASE_URL).expect("base URL"),
        DEFAULT_MODEL.to_owned(),
        vec![],
        api_key,
    );

    let models = host
        .list_models(Duration::from_secs(30))
        .expect("live /v1/models");
    assert!(!models.is_empty(), "live catalogue must not be empty");
    for model in &models {
        assert!(!model.name.is_empty());
    }

    let body = br#"{"state":{"topic":"live smoke"},"questions":{"pick":{"type":"choice","criteria":{"alpha":null,"beta":null}},"worth":{"type":"noul","criteria":{"true":"it works"}}}}"#;
    let request: DecisionRequest = wire::parse_request(body)
        .expect("parse smoke request")
        .request;
    let response = host
        .evaluate(&request, &CallContext::new("live-smoke", None))
        .expect("live evaluate");
    assert!(!response.model.is_empty());
    let answered: Vec<&str> = response.answers.iter().map(|(id, _)| id.as_str()).collect();
    assert!(answered.contains(&"pick"), "choice answer present");
    assert!(answered.contains(&"worth"), "noul answer present");
    eprintln!(
        "live smoke ok: model={} questions=2 providers_request_id={:?}",
        response.model, response.diagnostics.provider_request_id
    );
}
