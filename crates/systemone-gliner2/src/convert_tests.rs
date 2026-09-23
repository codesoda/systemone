use gliner2_rs::{Activation, ClassificationScores};
use serde_json::{Value, json};
use systemone_core::{
    Answer, ChoiceQuestion, DecisionRequest, NoulQuestion, Question, ScoreQuestion,
};

use super::*;

fn labels() -> [String; 2] {
    ["no".to_owned(), "yes".to_owned()]
}

fn choice(instructions: Option<Value>, criteria: &[(&str, Value)]) -> Question {
    Question::Choice(ChoiceQuestion {
        instructions,
        criteria: criteria
            .iter()
            .map(|(label, value)| ((*label).to_owned(), value.clone()))
            .collect(),
    })
}

fn request(state: Value, questions: Vec<(&str, Question)>) -> DecisionRequest {
    DecisionRequest::new(
        None,
        state,
        questions
            .into_iter()
            .map(|(id, question)| (id.to_owned(), question))
            .collect(),
    )
    .unwrap()
}

fn fake_scores(task: &str, labels: &[String], logits: &[f32]) -> ClassificationScores {
    ClassificationScores::new(
        task,
        labels.to_vec(),
        logits.to_vec(),
        1.0,
        Activation::Softmax,
    )
    .unwrap()
}

#[test]
fn state_and_instructions_render_readably() {
    assert_eq!(render_text(&json!("plain")), "plain");
    assert_eq!(render_text(&Value::Null), "");
    assert_eq!(
        render_text(&json!({"ticket": "Größe ✓", "n": 1})),
        r#"{"ticket":"Größe ✓","n":1}"#
    );
}

#[test]
fn choice_maps_options_descriptions_and_instructions() {
    let question = choice(
        Some(json!("Route the ticket.")),
        &[("billing", json!("money")), ("support", Value::Null)],
    );
    let prepared = prepare(
        &request(json!("I was charged twice"), vec![("dept", question)]),
        &labels(),
    )
    .unwrap();
    assert_eq!(prepared.text, "I was charged twice");
    let Route::Inference(inference) = &prepared.entries[0].route else {
        panic!("expected inference");
    };
    assert_eq!(inference.task, "dept");
    assert_eq!(inference.labels, ["billing", "support"]);
    assert_eq!(inference.instruction.as_deref(), Some("Route the ticket."));
    assert_eq!(
        inference.label_descriptions,
        [("billing".to_owned(), "money".to_owned())]
    );
    assert_eq!(inference.activation, Activation::Softmax);
}

#[test]
fn noul_uses_configured_ordered_labels_with_descriptions() {
    let question = Question::Noul(NoulQuestion {
        instructions: Some(json!("Is it urgent?")),
        true_description: Some(json!("needs action today")),
        false_description: None,
    });
    let prepared = prepare(
        &request(json!("server is down"), vec![("urgent", question)]),
        &labels(),
    )
    .unwrap();
    let Route::Inference(inference) = &prepared.entries[0].route else {
        panic!("expected inference");
    };
    assert_eq!(inference.labels, ["no", "yes"]);
    assert_eq!(
        inference.label_descriptions,
        [("yes".to_owned(), "needs action today".to_owned())]
    );
}

#[test]
fn score_levels_become_distinct_labels() {
    let question = Question::Score(ScoreQuestion {
        instructions: None,
        levels: vec![json!("low"), json!("mid"), json!(3)],
    });
    let prepared = prepare(&request(json!("x"), vec![("q", question)]), &labels()).unwrap();
    let Route::Inference(inference) = &prepared.entries[0].route else {
        panic!("expected inference");
    };
    assert_eq!(inference.labels, ["low", "mid", "3"]);

    let clash = Question::Score(ScoreQuestion {
        instructions: None,
        levels: vec![json!(1), json!("1")],
    });
    let error = prepare(&request(json!("x"), vec![("q", clash)]), &labels()).unwrap_err();
    assert!(matches!(error, HostError::Validation(_)), "{error}");
}

#[test]
fn reserved_markers_are_rejected_before_inference() {
    let question = choice(None, &[("a [L] b", Value::Null), ("c", Value::Null)]);
    let error = prepare(&request(json!("x"), vec![("q", question)]), &labels()).unwrap_err();
    assert!(
        error.to_string().contains("reserved prompt marker"),
        "{error}"
    );
}

#[test]
fn singleton_choice_is_deterministic_and_projected_without_scores() {
    let only = choice(None, &[("only", Value::Null)]);
    let two = choice(None, &[("a", Value::Null), ("b", Value::Null)]);
    let request = request(json!("x"), vec![("one", only), ("two", two)]);
    let prepared = prepare(&request, &labels()).unwrap();
    assert!(matches!(
        prepared.entries[0].route,
        Route::SingletonChoice(_)
    ));
    assert_eq!(prepared.inference_count(), 1);
    let scores = vec![
        None,
        Some(fake_scores(
            "two",
            &["a".to_owned(), "b".to_owned()],
            &[0.0, 2.0],
        )),
    ];
    let projected = project(&prepared, &request, &scores, 4096).unwrap();
    assert_eq!(
        projected.answers[0].1,
        Answer::Choice(systemone_core::ChoiceAnswer {
            choice: "only".into(),
            probabilities: vec![("only".into(), 1.0)],
            confidence: Some(1.0),
        })
    );
    let Answer::Choice(answer) = &projected.answers[1].1 else {
        panic!("expected choice");
    };
    assert_eq!(answer.choice, "b");
    assert_eq!(
        projected.diagnostics.execution.as_deref(),
        Some("per-question; rows=1; deterministic=1; backend=onnxruntime-cpu")
    );
    assert!(projected.diagnostics.truncation.is_none());
}

#[test]
fn noul_and_score_project_from_the_softmax() {
    let noul = Question::Noul(NoulQuestion {
        instructions: None,
        true_description: None,
        false_description: None,
    });
    let score = Question::Score(ScoreQuestion {
        instructions: None,
        levels: vec![json!("1"), json!("2"), json!("3")],
    });
    let request = request(json!("x"), vec![("n", noul), ("s", score)]);
    let prepared = prepare(&request, &labels()).unwrap();
    let scores = vec![
        Some(fake_scores("n", &labels(), &[0.0, 0.0])),
        Some(fake_scores(
            "s",
            &["1".to_owned(), "2".to_owned(), "3".to_owned()],
            &[0.0, 0.0, 30.0],
        )),
    ];
    let projected = project(&prepared, &request, &scores, 4096).unwrap();
    let Answer::Noul(noul) = &projected.answers[0].1 else {
        panic!("expected noul");
    };
    assert!((noul.probability_true - 0.5).abs() < 1e-6);
    let Answer::Score(score) = &projected.answers[1].1 else {
        panic!("expected score");
    };
    assert!((score.score - 2.0).abs() < 1e-6, "{}", score.score);
    assert_eq!(score.legend, vec![json!("1"), json!("2"), json!("3")]);
    assert!(score.confidence.unwrap() > 0.999);
}

#[test]
fn label_order_drift_and_truncation_are_reported() {
    let two = choice(None, &[("a", Value::Null), ("b", Value::Null)]);
    let request = request(json!("x"), vec![("q", two)]);
    let prepared = prepare(&request, &labels()).unwrap();
    let drifted = vec![Some(fake_scores(
        "q",
        &["b".to_owned(), "a".to_owned()],
        &[0.0, 1.0],
    ))];
    assert!(matches!(
        project(&prepared, &request, &drifted, 4096),
        Err(HostError::Internal(_))
    ));
    assert!(matches!(
        project(&prepared, &request, &[], 4096),
        Err(HostError::Internal(_))
    ));
}

#[test]
fn normalized_margin_spans_uniform_to_certain() {
    assert!((normalized_margin(&[0.5, 0.5])).abs() < 1e-12);
    assert!((normalized_margin(&[1.0, 0.0]) - 1.0).abs() < 1e-12);
    assert!((normalized_margin(&[0.25; 4])).abs() < 1e-12);
    assert_eq!(normalized_margin(&[1.0]), 1.0);
}
