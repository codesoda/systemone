//! Adapter policy tests: no model, no files.

use laya_core::{QuestionType, postprocess::QuestionResult};
use serde_json::json;
use systemone_core::{ChoiceQuestion, NoulQuestion, ScoreQuestion};

use super::*;

fn request(questions: Vec<(&str, Question)>) -> DecisionRequest {
    DecisionRequest::new(
        None,
        json!({"ticket": "duplicate charge", "amount": 12.5}),
        questions
            .into_iter()
            .map(|(id, question)| (id.to_owned(), question))
            .collect(),
    )
    .unwrap()
}

fn result(id: &str, kind: QuestionType, probabilities: Vec<f32>, argmax: usize) -> QuestionResult {
    let expected_score = (kind == QuestionType::Score).then(|| {
        probabilities
            .iter()
            .enumerate()
            .map(|(index, p)| index as f64 * f64::from(*p))
            .sum()
    });
    QuestionResult {
        id: id.to_owned(),
        question_type: kind,
        option_count: probabilities.len(),
        temperature: 1.0,
        temp_bucket: "default".to_owned(),
        raw_logits: vec![0.0; probabilities.len()],
        probabilities,
        confidence: 0.5,
        expected_score,
        act_logits: vec![0.0, 0.0],
        act_probs: vec![0.5, 0.5],
        argmax,
    }
}

#[test]
fn missing_and_structured_instructions_follow_upstream_rendering() {
    let prepared = prepare(&request(vec![
        (
            "a",
            Question::Noul(NoulQuestion {
                instructions: None,
                true_description: None,
                false_description: None,
            }),
        ),
        (
            "b",
            Question::Noul(NoulQuestion {
                instructions: Some(json!({"q": "café"})),
                true_description: Some(json!("yes")),
                false_description: None,
            }),
        ),
    ]))
    .unwrap();
    let laya = prepared.request.unwrap();
    assert_eq!(laya.questions[0].1.instructions(), "");
    assert_eq!(laya.questions[1].1.instructions(), r#"{"q": "caf\u00e9"}"#);
    match &laya.questions[1].1 {
        LayaQuestion::Noul { criteria, .. } => assert_eq!(criteria, &json!({"true": "yes"})),
        other => panic!("{other:?}"),
    }
    match &laya.questions[0].1 {
        LayaQuestion::Noul { criteria, .. } => assert!(criteria.is_null()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn singleton_choice_is_deterministic_and_skips_inference() {
    let only = request(vec![(
        "one",
        Question::Choice(ChoiceQuestion {
            instructions: None,
            criteria: vec![("only".to_owned(), Value::Null)],
        }),
    )]);
    let prepared = prepare(&only).unwrap();
    assert!(prepared.request.is_none());
    let projected = project(&prepared, &only, None, "none").unwrap();
    assert_eq!(
        projected.answers[0].1,
        Answer::Choice(ChoiceAnswer {
            choice: "only".to_owned(),
            probabilities: vec![("only".to_owned(), 1.0)],
            confidence: Some(1.0),
        })
    );
    assert_eq!(projected.usage.input_tokens, Some(0));
    assert_eq!(
        projected.diagnostics.execution.as_deref(),
        Some("batched; rows=0; deterministic=1; backend=none")
    );
}

#[test]
fn projects_every_primitive_in_request_order() {
    let mixed = request(vec![
        (
            "route",
            Question::Choice(ChoiceQuestion {
                instructions: Some(json!("Which team?")),
                criteria: vec![
                    ("billing".to_owned(), json!("money")),
                    ("support".to_owned(), Value::Null),
                ],
            }),
        ),
        (
            "solo",
            Question::Choice(ChoiceQuestion {
                instructions: None,
                criteria: vec![("x".to_owned(), Value::Null)],
            }),
        ),
        (
            "urgent",
            Question::Score(ScoreQuestion {
                instructions: None,
                levels: vec![json!("low"), json!("high")],
            }),
        ),
        (
            "review",
            Question::Noul(NoulQuestion {
                instructions: None,
                true_description: None,
                false_description: None,
            }),
        ),
    ]);
    let prepared = prepare(&mixed).unwrap();
    let laya = prepared.request.as_ref().unwrap();
    assert_eq!(laya.questions.len(), 3);
    let evaluation = Evaluation {
        results: vec![
            result("route", QuestionType::Choice, vec![0.25, 0.75], 1),
            result("urgent", QuestionType::Score, vec![0.4, 0.6], 1),
            result("review", QuestionType::Noul, vec![0.3, 0.7], 1),
        ],
        input_tokens: 42,
        truncated_state_tokens: 3,
        rows_at_max_len: 1,
        backend: "candle-cpu-f32".to_owned(),
        hidden_cls: vec![],
    };
    let projected = project(&prepared, &mixed, Some(&evaluation), "candle-cpu-f32").unwrap();
    let ids: Vec<&str> = projected
        .answers
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["route", "solo", "urgent", "review"]);
    match &projected.answers[0].1 {
        Answer::Choice(choice) => {
            assert_eq!(choice.choice, "support");
            assert_eq!(choice.probabilities[1].1, 0.75);
        }
        other => panic!("{other:?}"),
    }
    match &projected.answers[2].1 {
        Answer::Score(score) => {
            assert!((score.score - 0.6).abs() < 1e-6);
            assert_eq!(score.legend, vec![json!("low"), json!("high")]);
        }
        other => panic!("{other:?}"),
    }
    match &projected.answers[3].1 {
        Answer::Noul(noul) => assert!((noul.probability_true - 0.7).abs() < 1e-6),
        other => panic!("{other:?}"),
    }
    assert_eq!(projected.usage.input_tokens, Some(42));
    assert_eq!(
        projected.diagnostics.truncation.as_deref(),
        Some("state_tokens=3; rows_at_max_len=1")
    );
}

#[test]
fn runtime_errors_map_to_neutral_classes() {
    assert!(matches!(
        map_error(LayaError::OptionsExceedBudget {
            question: "q".into(),
            head_max_len: 192
        }),
        HostError::Validation(_)
    ));
    assert!(matches!(
        map_error(LayaError::SingleOptionChoice {
            question: "q".into()
        }),
        HostError::Unsupported(_)
    ));
    assert!(matches!(
        map_error(LayaError::Unavailable("no gpu".into())),
        HostError::Unavailable(_)
    ));
    assert!(matches!(
        map_error(LayaError::Inference("nan".into())),
        HostError::Internal(_)
    ));
}
