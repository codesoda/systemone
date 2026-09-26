//! Opt-in, spend-bounded live smoke tests for the hosted adapters.
//!
//! Not run by default. Each test makes at most two real calls to one
//! provider: the models catalogue (TypeSafe and Vercel only; OpenRouter's is
//! its general Models API) and one billed `POST …/systemone` with two
//! questions. They spend money, and run only when both gates are set:
//!
//! ```sh
//! TYPESAFE_API_KEY=... \
//! SYSTEMONE_LIVE_SMOKE=spend-acknowledged \
//! cargo test -p systemone-remote --test live_smoke -- --ignored live_typesafe
//!
//! AI_GATEWAY_API_KEY=...  SYSTEMONE_LIVE_SMOKE=spend-acknowledged \
//! cargo test -p systemone-remote --test live_smoke -- --ignored live_vercel
//!
//! OPENROUTER_API_KEY=...  SYSTEMONE_LIVE_SMOKE=spend-acknowledged \
//! cargo test -p systemone-remote --test live_smoke -- --ignored live_openrouter
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
use systemone_remote::{
    BASE_URL, DEFAULT_MODEL, HostedBackend, HostedHost, HostedSettings, OPENROUTER, Provider,
    TYPESAFE, VERCEL,
};

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
    let backend = HostedBackend::new(
        &TYPESAFE,
        BackendId::new("direct").expect("backend id"),
        None,
        vec![],
        &HostedSettings {
            api_key_env: "TYPESAFE_API_KEY".to_owned(),
        },
    )
    .expect("backend");
    assert_eq!(backend.kind(), ProviderKind::Typesafe);
    let mut host = HostedHost::new(
        &TYPESAFE,
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
    assert_eq!(
        answered,
        vec!["pick", "worth"],
        "answers follow request order"
    );
    let systemone_core::Answer::Choice(choice) = &response.answers[0].1 else {
        panic!("expected a choice answer for the choice question");
    };
    let labels: Vec<&str> = choice
        .probabilities
        .iter()
        .map(|(label, _)| label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["alpha", "beta"],
        "choice labels follow the declared order"
    );
    eprintln!(
        "live smoke ok: model={} questions=2 providers_request_id={:?}",
        response.model, response.diagnostics.provider_request_id
    );
}

/// One billed round trip through a gateway provider.
fn live_gateway_round_trip(provider: &'static Provider, list_catalogue: bool) {
    if std::env::var("SYSTEMONE_LIVE_SMOKE").ok().as_deref() != Some(ACKNOWLEDGEMENT) {
        panic!(
            "this live smoke test spends money; set SYSTEMONE_LIVE_SMOKE={ACKNOWLEDGEMENT} \
             together with {} to acknowledge and enable it",
            provider.default_api_key_env
        );
    }
    let api_key = std::env::var(provider.default_api_key_env)
        .unwrap_or_else(|_| panic!("{} must be set", provider.default_api_key_env));
    assert!(
        !api_key.trim().is_empty(),
        "{} is empty",
        provider.default_api_key_env
    );
    let mut host = HostedHost::new(
        provider,
        RemoteTransport::new().expect("remote client"),
        Url::parse(provider.base_url).expect("base URL"),
        DEFAULT_MODEL.to_owned(),
        vec![],
        api_key,
    );
    if list_catalogue {
        let models = host
            .list_models(Duration::from_secs(30))
            .expect("live catalogue");
        eprintln!(
            "{} catalogue: {:?}",
            provider.name,
            models.iter().map(|model| &model.name).collect::<Vec<_>>()
        );
    }
    let body = br#"{"state":{"topic":"live smoke"},"questions":{"pick":{"type":"choice","criteria":{"alpha":null,"beta":null}},"worth":{"type":"noul","criteria":{"true":"it works"}}}}"#;
    let request: DecisionRequest = wire::parse_request(body)
        .expect("parse smoke request")
        .request;
    let response = host
        .evaluate(&request, &CallContext::new("live-smoke", None))
        .expect("live evaluate");
    let answered: Vec<&str> = response.answers.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(answered, vec!["pick", "worth"]);
    eprintln!(
        "{} live smoke ok: model={} usage={:?} request_id={:?} upstream_provider={:?}",
        provider.name,
        response.model,
        response.usage,
        response.diagnostics.provider_request_id,
        response.diagnostics.upstream_provider
    );
}

#[test]
#[ignore = "spends money; requires AI_GATEWAY_API_KEY and SYSTEMONE_LIVE_SMOKE=spend-acknowledged"]
fn live_vercel_round_trip() {
    live_gateway_round_trip(&VERCEL, true);
}

#[test]
#[ignore = "spends money; requires OPENROUTER_API_KEY and SYSTEMONE_LIVE_SMOKE=spend-acknowledged"]
fn live_openrouter_round_trip() {
    live_gateway_round_trip(&OPENROUTER, true);
}
