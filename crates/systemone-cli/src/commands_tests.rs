//! Unit tests for one-shot and JSONL command paths (fake host, no model).

use std::time::Duration;

use serde_json::{Value, json};
use systemone_http::test_support::FakeBackend;

use super::*;

fn load_fake(backend: &FakeBackend) -> Loaded {
    Loaded::from_backend(backend, Duration::from_secs(5)).unwrap()
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn state_args(state: Option<&str>, json: Option<&str>) -> StateArgs {
    StateArgs {
        state: state.map(str::to_owned),
        state_json: json.map(str::to_owned),
        ..StateArgs::default()
    }
}

fn decide_args(question: &[&str], option: &[&str], option_id: &[&str]) -> DecideArgs {
    DecideArgs {
        question: question.iter().map(|s| (*s).to_owned()).collect(),
        option: option.iter().map(|s| (*s).to_owned()).collect(),
        option_id: option_id.iter().map(|s| (*s).to_owned()).collect(),
        id: None,
        state: StateArgs::default(),
        backend: None,
    }
}

#[test]
fn read_state_prefers_flags_then_piped_stdin_and_rejects_bad_json() {
    let mut stdin = std::io::Cursor::new("piped text");
    let value = read_state(&state_args(Some("flag"), None), &mut stdin, false).unwrap();
    assert_eq!(value, json!("flag"));
    let value = read_state(&state_args(None, Some(r#"{"a":1}"#)), &mut stdin, false).unwrap();
    assert_eq!(value, json!({"a": 1}));
    let value = read_state(&StateArgs::default(), &mut stdin, false).unwrap();
    assert_eq!(value, json!("piped text"));
    let error = read_state(&StateArgs::default(), &mut stdin, true).unwrap_err();
    assert!(error.message.contains("state is required"));
    let error = read_state(
        &state_args(None, Some(r#"{"a":1,"a":2}"#)),
        &mut stdin,
        false,
    )
    .unwrap_err();
    assert!(error.message.contains("--state-json"));
}

#[test]
fn decide_builds_labels_from_option_text_or_explicit_ids() {
    let args = DecideArgs {
        id: Some("route".into()),
        ..decide_args(&["Which team?", "Escalate?"], &["Billing", "Support"], &[])
    };
    let request = decide_request(&args, json!("s")).unwrap();
    let ids: Vec<&str> = request
        .questions
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["route/1", "route/2"]);
    let Question::Choice(choice) = &request.questions[0].1 else {
        panic!("expected choice");
    };
    assert_eq!(choice.instructions, Some(json!("Which team?")));
    assert_eq!(
        choice.criteria,
        vec![
            ("Billing".to_owned(), Value::Null),
            ("Support".to_owned(), Value::Null)
        ]
    );

    let args = decide_args(
        &["Escalate?"],
        &["Escalate now", "Handle normally"],
        &["yes", "no"],
    );
    let request = decide_request(&args, json!("s")).unwrap();
    assert_eq!(request.questions[0].0, "q-1");
    let Question::Choice(choice) = &request.questions[0].1 else {
        panic!("expected choice");
    };
    assert_eq!(
        choice.criteria,
        vec![
            ("yes".to_owned(), json!("Escalate now")),
            ("no".to_owned(), json!("Handle normally"))
        ]
    );

    let args = decide_args(&["Escalate?"], &["a", "b"], &["only-one"]);
    let error = decide_request(&args, json!("s")).unwrap_err();
    assert!(error.message.contains("--option-id count 1"));
}

#[test]
fn noul_and_score_map_to_jev_questions() {
    let args = NoulArgs {
        question: "Refund?".into(),
        true_description: Some("wants money back".into()),
        false_description: None,
        id: None,
        state: StateArgs::default(),
        backend: None,
    };
    let request = noul_request(&args, json!("s")).unwrap();
    let Question::Noul(noul) = &request.questions[0].1 else {
        panic!("expected noul");
    };
    assert_eq!(noul.instructions, Some(json!("Refund?")));
    assert_eq!(noul.true_description, Some(json!("wants money back")));
    assert_eq!(noul.false_description, None);

    let args = ScoreArgs {
        question: "How urgent?".into(),
        level: vec!["low".into(), "medium".into(), "high".into()],
        id: Some("urgency".into()),
        state: StateArgs::default(),
        backend: None,
    };
    let request = score_request(&args, json!({"severity": 3})).unwrap();
    assert_eq!(request.questions[0].0, "urgency");
    let Question::Score(score) = &request.questions[0].1 else {
        panic!("expected score");
    };
    assert_eq!(
        score.levels,
        vec![json!("low"), json!("medium"), json!("high")]
    );

    // Jev requires at least two levels; the neutral validator enforces it.
    let args = ScoreArgs {
        level: vec!["only".into()],
        ..args
    };
    assert!(score_request(&args, json!("s")).is_err());
}

#[test]
fn one_shot_output_matches_the_wire_shape() {
    let backend = FakeBackend::new("local");
    let args = decide_args(&["Which team?"], &["Billing", "Support"], &[]);
    let request = decide_request(&args, json!("s")).unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    evaluate_loaded(
        load_fake(&backend),
        &request,
        false,
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    let out = lines(&stdout);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["model"], "fake-local");
    assert_eq!(out[0]["answers"]["q-1"]["type"], "choice");
    assert_eq!(out[0]["answers"]["q-1"]["choice"], "Billing");
    assert_eq!(out[0]["answers"]["q-1"]["probabilities"]["Billing"], 0.5);
    let diag = lines(&stderr);
    assert_eq!(diag[0]["backend"], "local");
    assert_eq!(backend.state.lock().unwrap().loads, 1);
}

#[test]
fn jsonl_loads_once_keeps_going_after_row_errors_and_reports_failures() {
    let backend = FakeBackend::new("local");
    let text = concat!(
        r#"{"state":"a","questions":{"q":{"type":"noul"}}}"#,
        "\n",
        "\n",
        "{not json\n",
        r#"{"backend":"other","state":"b","questions":{"q":{"type":"noul"}}}"#,
        "\n",
        r#"{"state":"c","questions":{"q":{"type":"choice","criteria":{"a":null,"b":null,"c":null,"d":null,"e":null}}}}"#,
        "\n",
        r#"{"backend":"local","state":"d","questions":{"s":{"type":"score","criteria":["l","h"]}}}"#,
        "\n",
    );
    let (mut sink, mut stderr) = (Vec::new(), Vec::new());
    let error = run_jsonl_loaded(load_fake(&backend), text, &mut sink, &mut stderr).unwrap_err();
    assert_eq!(error.code, "rows_failed");
    let rows = lines(&sink);
    assert_eq!(rows.len(), 5, "one output line per non-blank input line");
    assert_eq!(rows[0]["answers"]["q"]["noul"], 0.25);
    assert_eq!(rows[1]["line"], 3);
    assert_eq!(rows[1]["error"]["error_type"], "invalid_json");
    assert_eq!(rows[2]["line"], 4);
    assert!(
        rows[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bound to \"local\"")
    );
    assert_eq!(rows[3]["line"], 5);
    assert_eq!(rows[3]["error"]["error_type"], "validation");
    assert_eq!(rows[4]["answers"]["s"]["type"], "score");
    let summary = lines(&stderr).pop().unwrap();
    assert_eq!(summary["rows"], 5);
    assert_eq!(summary["succeeded"], 2);
    assert_eq!(summary["failed"], 3);
    let state = backend.state.lock().unwrap();
    assert_eq!(state.loads, 1);
    assert_eq!(
        state.calls, 2,
        "only parseable, in-limit rows reach the host"
    );
}

#[test]
fn jsonl_stops_after_a_terminal_host_error() {
    let mut backend = FakeBackend::new("local");
    backend.fail_with = Some(HostError::unavailable("device lost"));
    let text = concat!(
        r#"{"state":"a","questions":{"q":{"type":"noul"}}}"#,
        "\n",
        r#"{"state":"b","questions":{"q":{"type":"noul"}}}"#,
        "\n",
    );
    let (mut sink, mut stderr) = (Vec::new(), Vec::new());
    let error = run_jsonl_loaded(load_fake(&backend), text, &mut sink, &mut stderr).unwrap_err();
    assert_eq!(error.code, "unavailable");
    let rows = lines(&sink);
    assert_eq!(rows.len(), 1, "the batch stops at the first terminal error");
    assert_eq!(rows[0]["error"]["error_type"], "unavailable");
    assert_eq!(backend.state.lock().unwrap().calls, 1);
}
