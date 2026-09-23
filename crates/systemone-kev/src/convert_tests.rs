//! Adapter policy tests: no model, no files.

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

fn three_questions() -> DecisionRequest {
    request(vec![
        (
            "route",
            Question::Choice(ChoiceQuestion {
                instructions: Some(json!("Which queue?")),
                criteria: vec![
                    ("billing".to_owned(), json!("Payments")),
                    ("support".to_owned(), json!("General")),
                ],
            }),
        ),
        (
            "review",
            Question::Noul(NoulQuestion {
                instructions: Some(json!("Needs review?")),
                true_description: Some(json!("A human should look")),
                false_description: None,
            }),
        ),
        (
            "urgency",
            Question::Score(ScoreQuestion {
                instructions: Some(json!("How urgent?")),
                levels: vec![json!("low"), json!("medium"), json!("high")],
            }),
        ),
    ])
}

#[cfg(feature = "kev")]
#[test]
fn builds_the_upstream_wire_shape_with_null_instructions_for_missing_ones() {
    let request = request(vec![
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
            Question::Choice(ChoiceQuestion {
                instructions: Some(json!({"q": "café"})),
                criteria: vec![("x".to_owned(), json!(null))],
            }),
        ),
    ]);
    let kev = to_kev(&request).unwrap();
    // Upstream renders null instructions as the empty string; the noul
    // without descriptions must not carry a criteria object at all.
    assert_eq!(kev.questions["a"], json!({"type": "noul", "instructions": null}));
    assert_eq!(
        kev.questions["b"],
        json!({"type": "choice", "instructions": {"q": "café"}, "criteria": {"x": null}})
    );
    assert_eq!(kev.model, "kev-latest");
    // The record renders through kev-core's exact upstream port.
    let (record, metas) = kev_core::api::to_record(&kev).unwrap();
    assert_eq!(record.questions[0].instr, "");
    assert_eq!(record.questions[0].options, vec!["no", "yes"]);
    assert_eq!(record.questions[1].options, vec!["x"]);
    assert_eq!(metas[0].keys, vec!["false", "true"]);
    assert_eq!(record.state, "ticket: duplicate charge\namount: 12.5");
}

#[test]
fn projects_answers_in_request_order_with_upstream_confidences() {
    let request = three_questions();
    let probs = vec![
        vec![0.8, 0.2],
        vec![0.3, 0.7],
        vec![0.1, 0.2, 0.7],
    ];
    let projected = project(
        &request,
        &EvaluationView {
            probs: &probs,
            input_tokens: 42,
            output_tokens: 55,
            prefix_cache_hit: true,
        },
        "mlx",
    )
    .unwrap();
    assert_eq!(projected.answers.len(), 3);

    let (id, answer) = &projected.answers[0];
    assert_eq!(id, "route");
    let Answer::Choice(choice) = answer else {
        panic!("{answer:?}")
    };
    assert_eq!(choice.choice, "billing");
    // Upstream choice confidence: (0.8 - 0.5) / (1 - 0.5) = 0.6.
    assert!((choice.confidence.unwrap() - 0.6).abs() < 1e-12);
    assert_eq!(choice.probabilities[0], ("billing".to_owned(), 0.8));

    let (id, answer) = &projected.answers[1];
    assert_eq!(id, "review");
    let Answer::Noul(noul) = answer else {
        panic!("{answer:?}")
    };
    // Index 1 is `true`, upstream's layout.
    assert!((noul.probability_true - 0.7).abs() < 1e-12);

    let (id, answer) = &projected.answers[2];
    assert_eq!(id, "urgency");
    let Answer::Score(score) = answer else {
        panic!("{answer:?}")
    };
    // Expected level 0*0.1 + 1*0.2 + 2*0.7 = 1.6.
    assert!((score.score - 1.6).abs() < 1e-12);
    // Upstream score confidence: mode 2, E|i-2| = 0.4, 1 - 0.4/2 = 0.8.
    assert!((score.confidence.unwrap() - 0.8).abs() < 1e-12);
    assert_eq!(score.legend, vec![json!("low"), json!("medium"), json!("high")]);

    assert_eq!(projected.usage.input_tokens, Some(42));
    // Upstream kev semantics: the serialised-answer token count, not zero.
    assert_eq!(projected.usage.output_tokens, Some(55));
    let execution = projected.diagnostics.execution.unwrap();
    assert!(execution.contains("prefix_cache=hit"), "{execution}");
    assert!(execution.contains("backend=mlx"), "{execution}");
}

#[test]
fn distribution_length_mismatches_are_internal_errors() {
    let request = three_questions();
    let short = vec![vec![1.0], vec![0.3, 0.7], vec![0.1, 0.2, 0.7]];
    let error = project(
        &request,
        &EvaluationView {
            probs: &short,
            input_tokens: 0,
            output_tokens: 0,
            prefix_cache_hit: false,
        },
        "candle",
    )
    .unwrap_err();
    assert!(matches!(error, HostError::Internal(_)), "{error}");

    let wrong_count = vec![vec![0.5, 0.5]];
    let error = project(
        &request,
        &EvaluationView {
            probs: &wrong_count,
            input_tokens: 0,
            output_tokens: 0,
            prefix_cache_hit: false,
        },
        "candle",
    )
    .unwrap_err();
    assert!(matches!(error, HostError::Internal(_)), "{error}");
}

#[cfg(feature = "kev")]
#[test]
fn maps_runtime_errors_onto_the_neutral_classes() {
    use kev_core::KevError;
    assert!(matches!(
        map_error(KevError::InvalidRequest("bad".into())),
        HostError::Validation(_)
    ));
    assert!(matches!(
        map_error(KevError::ContextOverflow("too long".into())),
        HostError::Validation(_)
    ));
    assert!(matches!(
        map_error(KevError::Load("missing".into())),
        HostError::Unavailable(_)
    ));
    assert!(matches!(
        map_error(KevError::Inference("nan".into())),
        HostError::Internal(_)
    ));
}

#[test]
fn single_option_choice_keeps_full_certainty() {
    let request = request(vec![(
        "only",
        Question::Choice(ChoiceQuestion {
            instructions: None,
            criteria: vec![("keep".to_owned(), json!(null))],
        }),
    )]);
    let probs = vec![vec![1.0]];
    let projected = project(
        &request,
        &EvaluationView {
            probs: &probs,
            input_tokens: 5,
            output_tokens: 9,
            prefix_cache_hit: false,
        },
        "candle",
    )
    .unwrap();
    let Answer::Choice(choice) = &projected.answers[0].1 else {
        panic!("{:?}", projected.answers[0].1)
    };
    assert_eq!(choice.choice, "keep");
    assert_eq!(choice.confidence, Some(1.0));
}
