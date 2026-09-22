//! Conversion between neutral SystemOne types and OpenJev decisions.
//!
//! This is the Jev → OpenJev projection previously owned by the OpenJev HTTP
//! server. Every Jev primitive becomes an OpenJev `Decision` with a specific
//! option layout: Noul is a deliberately ordered `yes`/`no` choice (P(true)
//! is index 0), Score is a choice over `0..n` level IDs.

use std::collections::HashMap;

use openjev_core::{
    Decision, DecisionOption, ExecutionMode, OpenJevError, Readout, StateValue, first_argmax,
    normalized_margin, python_json_dumps,
};
use serde_json::{Map, Value};
use systemone_core::{
    Answer, ChoiceAnswer, DecisionRequest, Diagnostics, HostError, NoulAnswer, Question,
    ScoreAnswer, Usage,
};

pub const DEFAULT_INSTRUCTIONS: &str = "Select the best option.";
/// Native OpenJev decisions support 2–16 options; singleton Choice is handled
/// deterministically without inference.
pub const MAX_OPTIONS: usize = 16;
pub const MAX_QUESTIONS: usize = 64;
pub const MAX_EXPANDED_STATE_BYTES: usize = 4 * 1024 * 1024;
pub const PROBABILITY_STATUS: &str =
    "conditional option score; uncalibrated as decision confidence";
pub const CONFIDENCE_DEFINITION: &str =
    "normalized margin between the top two option probabilities; not calibrated";

#[derive(Debug)]
pub struct Prepared {
    pub entries: Vec<Entry>,
    /// Decisions requiring inference, in entry order.
    pub inference: Vec<Decision>,
}

#[derive(Debug)]
pub struct Entry {
    pub external_id: String,
    /// `None` for deterministic singleton Choice.
    pub internal_id: Option<String>,
    pub projection: Projection,
}

#[derive(Debug)]
pub enum Projection {
    Choice { labels: Vec<String> },
    Noul,
    Score { legend: Vec<Value> },
}

pub fn prepare(request: &DecisionRequest) -> Result<Prepared, HostError> {
    if request.questions.len() > MAX_QUESTIONS {
        return Err(HostError::validation(format!(
            "questions must not contain more than {MAX_QUESTIONS} entries"
        )));
    }
    let state = adapt_state(request.state.clone())?;
    let state_bytes = python_json_dumps(state.as_value())
        .map_err(|error| HostError::validation(format!("state cannot be measured: {error}")))?
        .len();
    // Every question owns a StateValue clone; bound replication conservatively.
    let expanded = state_bytes
        .checked_mul(request.questions.len())
        .ok_or_else(|| {
            HostError::validation("expanded state replication size exceeds the backend limit")
        })?;
    if expanded > MAX_EXPANDED_STATE_BYTES {
        return Err(HostError::validation(format!(
            "state replicated across questions exceeds the {MAX_EXPANDED_STATE_BYTES}-byte limit"
        )));
    }

    let mut entries = Vec::with_capacity(request.questions.len());
    let mut inference = Vec::with_capacity(request.questions.len());
    for (index, (external_id, question)) in request.questions.iter().enumerate() {
        let internal_id = format!("jev-question-{}", index + 1);
        let path = format!("questions.{external_id:?}");
        let (projection, decision) = match question {
            Question::Choice(choice) => {
                let instructions = render_instructions(choice.instructions.as_ref(), &path)?;
                prepare_choice(&path, &internal_id, &state, instructions, &choice.criteria)?
            }
            Question::Noul(noul) => {
                let instructions = render_instructions(noul.instructions.as_ref(), &path)?;
                let yes = match &noul.true_description {
                    Some(value) => {
                        render_description(value, &format!("{path}.criteria.true"), None)?
                    }
                    None => "Yes".to_owned(),
                };
                let no = match &noul.false_description {
                    Some(value) => {
                        render_description(value, &format!("{path}.criteria.false"), None)?
                    }
                    None => "No".to_owned(),
                };
                let decision = Decision::new(
                    &internal_id,
                    state.clone(),
                    instructions,
                    vec![
                        DecisionOption {
                            id: "yes".to_owned(),
                            description: yes,
                        },
                        DecisionOption {
                            id: "no".to_owned(),
                            description: no,
                        },
                    ],
                )
                .map_err(core_validation)?;
                (Projection::Noul, Some(decision))
            }
            Question::Score(score) => {
                let instructions = render_instructions(score.instructions.as_ref(), &path)?;
                if !(2..=MAX_OPTIONS).contains(&score.levels.len()) {
                    return Err(HostError::validation(format!(
                        "{path}.criteria must contain 2-{MAX_OPTIONS} levels"
                    )));
                }
                let mut options = Vec::with_capacity(score.levels.len());
                for (index, description) in score.levels.iter().enumerate() {
                    options.push(DecisionOption {
                        id: index.to_string(),
                        description: render_description(
                            description,
                            &format!("{path}.criteria[{index}]"),
                            None,
                        )?,
                    });
                }
                let decision = Decision::new(&internal_id, state.clone(), instructions, options)
                    .map_err(core_validation)?;
                (
                    Projection::Score {
                        legend: score.levels.clone(),
                    },
                    Some(decision),
                )
            }
        };
        let inferred_id = decision.as_ref().map(|_| internal_id);
        if let Some(decision) = decision {
            inference.push(decision);
        }
        entries.push(Entry {
            external_id: external_id.clone(),
            internal_id: inferred_id,
            projection,
        });
    }
    Ok(Prepared { entries, inference })
}

fn prepare_choice(
    path: &str,
    internal_id: &str,
    state: &StateValue,
    instructions: String,
    criteria: &[(String, Value)],
) -> Result<(Projection, Option<Decision>), HostError> {
    if criteria.is_empty() || criteria.len() > MAX_OPTIONS {
        return Err(HostError::validation(format!(
            "{path}.criteria must contain 1-{MAX_OPTIONS} options"
        )));
    }
    let mut labels = Vec::with_capacity(criteria.len());
    let mut options = Vec::with_capacity(criteria.len());
    for (label, description) in criteria {
        let rendered = render_description(
            description,
            &format!("{path}.criteria.{label:?}"),
            Some(label),
        )?;
        labels.push(label.clone());
        options.push(DecisionOption {
            id: label.clone(),
            description: rendered,
        });
    }
    let projection = Projection::Choice { labels };
    if options.len() == 1 {
        return Ok((projection, None));
    }
    let decision = Decision::new(internal_id, state.clone(), instructions, options)
        .map_err(core_validation)?;
    Ok((projection, Some(decision)))
}

fn render_instructions(value: Option<&Value>, path: &str) -> Result<String, HostError> {
    let Some(value) = value else {
        return Ok(DEFAULT_INSTRUCTIONS.to_owned());
    };
    let path = format!("{path}.instructions");
    require_entry_type(value, &path)?;
    if is_empty_entry(value) {
        return Ok(DEFAULT_INSTRUCTIONS.to_owned());
    }
    render_entry(value, &path)
}

fn render_description(
    value: &Value,
    path: &str,
    null_fallback: Option<&str>,
) -> Result<String, HostError> {
    require_entry_type(value, path)?;
    if value.is_null()
        && let Some(fallback) = null_fallback
    {
        return Ok(fallback.to_owned());
    }
    render_entry(value, path)
}

fn render_entry(value: &Value, path: &str) -> Result<String, HostError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Object(_) | Value::Array(_) | Value::Null => python_json_dumps(value)
            .map_err(|error| HostError::validation(format!("{path} cannot be rendered: {error}"))),
        _ => Err(HostError::validation(format!(
            "{path} must be a string, object, array, or null"
        ))),
    }
}

/// OpenJev's state contract requires a nonempty string/array/object with
/// integer-only numbers. Null/empty wire states are wrapped in one explicit
/// envelope rather than stringified; floats are rejected as an adapter
/// limitation.
fn adapt_state(value: Value) -> Result<StateValue, HostError> {
    match StateValue::try_from(value.clone()) {
        Ok(state) => Ok(state),
        Err(OpenJevError::Serialization { path, message }) => Err(HostError::validation(format!(
            "state{}: {message} (the openjev backend accepts integer-only JSON numbers)",
            path.strip_prefix('$').unwrap_or(&path)
        ))),
        Err(_) => {
            let mut envelope = Map::new();
            envelope.insert("value".to_owned(), value);
            StateValue::try_from(Value::Object(envelope)).map_err(core_validation)
        }
    }
}

fn require_entry_type(value: &Value, path: &str) -> Result<(), HostError> {
    if matches!(
        value,
        Value::String(_) | Value::Object(_) | Value::Array(_) | Value::Null
    ) {
        Ok(())
    } else {
        Err(HostError::validation(format!(
            "{path} must be a string, object, array, or null"
        )))
    }
}

fn is_empty_entry(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn core_validation(error: OpenJevError) -> HostError {
    HostError::validation(error.to_string())
}

fn core_runtime(error: OpenJevError) -> HostError {
    HostError::internal(format!("{}: {error}", error.code()))
}

/// Projected answers plus usage/diagnostics derived from the readouts.
pub struct Projected {
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
    pub diagnostics: Diagnostics,
}

pub fn project(prepared: Prepared, readouts: Vec<Readout>) -> Result<Projected, HostError> {
    let expected_rows = prepared
        .entries
        .iter()
        .filter(|entry| entry.internal_id.is_some())
        .count();
    if readouts.len() != expected_rows {
        return Err(HostError::internal(
            "openjev engine returned an unexpected number of rows",
        ));
    }
    let by_id: HashMap<_, _> = readouts
        .into_iter()
        .map(|readout| (readout.id.clone(), readout))
        .collect();
    if by_id.len() != expected_rows {
        return Err(HostError::internal(
            "openjev engine returned duplicate row identities",
        ));
    }
    let mut input_tokens = 0_u64;
    let mut answers = Vec::with_capacity(prepared.entries.len());
    let mut modes = Vec::new();
    for entry in prepared.entries {
        let answer = if let Some(internal_id) = entry.internal_id {
            let readout = by_id
                .get(&internal_id)
                .ok_or_else(|| HostError::internal("openjev engine result identity mismatch"))?;
            input_tokens = input_tokens.saturating_add(readout.input_tokens);
            modes.push((
                readout.execution.requested_mode,
                readout.execution.effective_mode,
                readout.execution.fallback_reason.clone(),
            ));
            project_readout(&entry.projection, readout)?
        } else {
            project_singleton(&entry.projection)?
        };
        answers.push((entry.external_id, answer));
    }
    let (execution, fallback) = disclose_execution(&modes);
    Ok(Projected {
        answers,
        usage: Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(0),
            cost: None,
        },
        diagnostics: Diagnostics {
            execution: Some(execution),
            fallback,
            probability_status: Some(PROBABILITY_STATUS.to_owned()),
            provider_request_id: None,
            truncation: None,
        },
    })
}

fn project_singleton(projection: &Projection) -> Result<Answer, HostError> {
    let Projection::Choice { labels } = projection else {
        return Err(HostError::internal(
            "only Choice supports deterministic singleton projection",
        ));
    };
    let label = labels
        .first()
        .ok_or_else(|| HostError::internal("singleton Choice has no label"))?;
    Ok(Answer::Choice(ChoiceAnswer {
        choice: label.clone(),
        probabilities: vec![(label.clone(), 1.0)],
        confidence: Some(1.0),
    }))
}

fn project_readout(projection: &Projection, readout: &Readout) -> Result<Answer, HostError> {
    match projection {
        Projection::Choice { labels } => {
            ensure_alignment(labels, readout)?;
            let choice_index = first_argmax(&readout.probabilities).map_err(core_runtime)?;
            let confidence = normalized_margin(&readout.probabilities).map_err(core_runtime)?;
            Ok(Answer::Choice(ChoiceAnswer {
                choice: labels[choice_index].clone(),
                probabilities: labels
                    .iter()
                    .cloned()
                    .zip(readout.probabilities.iter().copied())
                    .collect(),
                confidence: Some(confidence),
            }))
        }
        Projection::Noul => {
            ensure_alignment(&["yes".to_owned(), "no".to_owned()], readout)?;
            Ok(Answer::Noul(NoulAnswer {
                probability_true: readout.probabilities[0],
            }))
        }
        Projection::Score { legend } => {
            let expected: Vec<_> = (0..legend.len()).map(|index| index.to_string()).collect();
            ensure_alignment(&expected, readout)?;
            let confidence = normalized_margin(&readout.probabilities).map_err(core_runtime)?;
            let score: f64 = readout
                .probabilities
                .iter()
                .enumerate()
                .map(|(index, probability)| index as f64 * probability)
                .sum();
            Ok(Answer::Score(ScoreAnswer {
                score,
                probabilities: readout.probabilities.clone(),
                confidence: Some(confidence),
                legend: legend.clone(),
            }))
        }
    }
}

fn ensure_alignment(expected_ids: &[String], readout: &Readout) -> Result<(), HostError> {
    if readout.probabilities.len() != expected_ids.len() || readout.option_ids != expected_ids {
        return Err(HostError::internal(
            "openjev engine returned probabilities with mismatched option identities",
        ));
    }
    Ok(())
}

fn disclose_execution(
    modes: &[(ExecutionMode, ExecutionMode, Option<String>)],
) -> (String, Option<String>) {
    if modes.is_empty() {
        return (
            "requested=deterministic; effective=deterministic".to_owned(),
            None,
        );
    }
    let requested = common_mode(modes.iter().map(|mode| mode.0));
    let effective = common_mode(modes.iter().map(|mode| mode.1));
    let fallback = modes
        .iter()
        .find_map(|mode| mode.2.clone())
        .map(|reason| format!("serial full-prompt fallback: {reason}"));
    (
        format!("requested={requested}; effective={effective}"),
        fallback,
    )
}

fn common_mode(mut modes: impl Iterator<Item = ExecutionMode>) -> String {
    let first = modes.next().expect("nonempty execution disclosure");
    if modes.all(|mode| mode == first) {
        mode_name(first).to_owned()
    } else {
        "mixed".to_owned()
    }
}

pub(crate) const fn mode_name(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Direct => "direct",
        ExecutionMode::Serial => "serial",
        ExecutionMode::Shared => "shared",
        ExecutionMode::Batch => "batch",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use openjev_core::{
        Device, ExecutionMetadata, GpuLayersRequested, GpuLayersStatus, Integrity, ModelMetadata,
        Primitive, PromptProfile, TemplateMetadataStatus, standard_limitations,
    };
    use systemone_core::{ChoiceQuestion, NoulQuestion, ScoreQuestion};

    use super::*;

    pub(crate) fn readout(id: &str, option_ids: &[&str], probabilities: Vec<f64>) -> Readout {
        let option_ids: Vec<_> = option_ids.iter().map(|id| (*id).to_owned()).collect();
        let choice_index = first_argmax(&probabilities).unwrap();
        Readout {
            schema: "openjev-readout-v1".to_owned(),
            id: id.to_owned(),
            primitive: Primitive::Choice,
            choice: option_ids[choice_index].clone(),
            choice_index,
            option_logits: vec![0.0; option_ids.len()],
            answer_token_ids: (1..=u32::try_from(option_ids.len()).unwrap()).collect(),
            option_ids,
            probabilities,
            allowed_token_mass: 0.5,
            full_vocab_argmax_id: 1,
            full_vocab_log_normalizer: 1.0,
            input_tokens: 10,
            forward_seconds: None,
            total_seconds: None,
            prompt_sha256: "0".repeat(64),
            prompt_version: "direct-options-v1".to_owned(),
            model: ModelMetadata {
                id: "test".to_owned(),
                source: "test".to_owned(),
                revision: "test".to_owned(),
                file: "test".to_owned(),
                quant: "test".to_owned(),
                backend: "test".to_owned(),
                artifact_sha256: "1".repeat(64),
                integrity: Integrity::LocalUnverified,
                dtype: "test".to_owned(),
                native_reference: None,
                template_profile: PromptProfile::Qwen3,
                template_sha256: None,
                template_override: true,
                template_status: TemplateMetadataStatus::OverrideUnverified,
                template_equivalence_evidence: None,
                serving_config: None,
                adapter: None,
                adapter_sha256: None,
                adapter_revision: None,
                torch_version: None,
                transformers_version: None,
            },
            readout: openjev_core::DIRECT_READOUT.to_owned(),
            probability_status: openjev_core::PROBABILITY_STATUS.to_owned(),
            limitations: standard_limitations(),
            execution: ExecutionMetadata {
                requested_mode: ExecutionMode::Direct,
                effective_mode: ExecutionMode::Direct,
                fallback_reason: None,
                device: Device::Cpu,
                device_name: "test".to_owned(),
                gpu_layers_requested: GpuLayersRequested::Count(0),
                gpu_layers_actual: Some(0),
                gpu_layers_status: GpuLayersStatus::KnownDisabled,
                threads: 1,
                n_ctx_requested: None,
                n_ctx_actual: 128,
                max_tokens: 128,
                n_batch: 128,
                n_ubatch: 128,
                n_seq_max: 4,
                kv_unified: true,
                waves: 1,
                probe_id: None,
                run_id: "test".to_owned(),
                group_id: None,
            },
            confidence: None,
            confidence_status: None,
            p_yes: None,
            level_values: None,
            expected_value: None,
            argmax_level: None,
            cache_hit: Some(false),
            prefix_tokens: None,
            prefix_sha256: None,
            prefill_seconds: None,
            copy_seconds: None,
            suffix_forward_seconds: None,
            shared_timing: None,
            postprocess: None,
        }
    }

    fn choice(criteria: &[(&str, Value)]) -> Question {
        Question::Choice(ChoiceQuestion {
            instructions: None,
            criteria: criteria
                .iter()
                .map(|(label, value)| ((*label).to_owned(), value.clone()))
                .collect(),
        })
    }

    #[test]
    fn prepares_all_primitives_with_openjev_option_layouts() {
        let request = DecisionRequest::new(
            None,
            serde_json::json!({"z": 1, "a": null}),
            vec![
                (
                    "雪".into(),
                    Question::Choice(ChoiceQuestion {
                        instructions: Some(serde_json::json!({"task": "pick"})),
                        criteria: vec![
                            ("b".into(), Value::Null),
                            ("a".into(), serde_json::json!({"why": "A"})),
                        ],
                    }),
                ),
                (
                    "truth".into(),
                    Question::Noul(NoulQuestion {
                        instructions: None,
                        true_description: Some(serde_json::json!({"means": "yes"})),
                        false_description: Some(Value::String("Nope".into())),
                    }),
                ),
                (
                    "score".into(),
                    Question::Score(ScoreQuestion {
                        instructions: Some(serde_json::json!([])),
                        levels: vec![Value::Null, serde_json::json!({"level": "high"})],
                    }),
                ),
            ],
        )
        .unwrap();
        let prepared = prepare(&request).unwrap();
        assert_eq!(prepared.entries.len(), 3);
        assert_eq!(prepared.inference.len(), 3);
        assert_eq!(prepared.inference[0].options[0].id, "b");
        assert_eq!(prepared.inference[0].options[0].description, "b");
        assert_eq!(prepared.inference[0].question, "{\"task\": \"pick\"}");
        assert_eq!(prepared.inference[1].options[0].id, "yes");
        assert_eq!(prepared.inference[1].options[1].description, "Nope");
        assert_eq!(prepared.inference[2].question, DEFAULT_INSTRUCTIONS);
        assert_eq!(prepared.inference[2].options[0].description, "null");
    }

    #[test]
    fn singleton_choice_and_null_state_are_deterministic() {
        let request = DecisionRequest::new(
            None,
            Value::Null,
            vec![("only".into(), choice(&[("λ", Value::Null)]))],
        )
        .unwrap();
        let prepared = prepare(&request).unwrap();
        assert!(prepared.inference.is_empty());
        let projected = project(prepared, Vec::new()).unwrap();
        assert_eq!(
            projected.answers[0].1,
            Answer::Choice(ChoiceAnswer {
                choice: "λ".into(),
                probabilities: vec![("λ".into(), 1.0)],
                confidence: Some(1.0),
            })
        );
        assert_eq!(projected.usage.input_tokens, Some(0));
        assert_eq!(
            projected.diagnostics.execution.as_deref(),
            Some("requested=deterministic; effective=deterministic")
        );
    }

    #[test]
    fn projects_unrounded_values_in_request_order() {
        let request = DecisionRequest::new(
            None,
            Value::String("s".into()),
            vec![
                (
                    "c".into(),
                    choice(&[
                        ("first", Value::Null),
                        ("second", Value::Null),
                        ("third", Value::Null),
                    ]),
                ),
                (
                    "n".into(),
                    Question::Noul(NoulQuestion {
                        instructions: None,
                        true_description: None,
                        false_description: None,
                    }),
                ),
                (
                    "s".into(),
                    Question::Score(ScoreQuestion {
                        instructions: None,
                        levels: vec!["low".into(), "mid".into(), "high".into()],
                    }),
                ),
            ],
        )
        .unwrap();
        let prepared = prepare(&request).unwrap();
        let rows = vec![
            readout(
                "jev-question-3",
                &["0", "1", "2"],
                vec![0.126, 0.333, 0.541],
            ),
            readout(
                "jev-question-1",
                &["first", "second", "third"],
                vec![1.0 / 3.0; 3],
            ),
            readout("jev-question-2", &["yes", "no"], vec![0.126, 0.874]),
        ];
        let projected = project(prepared, rows).unwrap();
        assert_eq!(projected.answers[0].0, "c");
        match &projected.answers[0].1 {
            Answer::Choice(answer) => {
                assert_eq!(answer.choice, "first");
                assert_eq!(answer.probabilities[2].0, "third");
            }
            other => panic!("{other:?}"),
        }
        match &projected.answers[1].1 {
            Answer::Noul(answer) => assert!((answer.probability_true - 0.126).abs() < 1e-12),
            other => panic!("{other:?}"),
        }
        match &projected.answers[2].1 {
            Answer::Score(answer) => {
                assert!((answer.score - 1.415).abs() < 1e-12);
                assert_eq!(answer.legend[1], "mid");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(projected.usage.input_tokens, Some(30));
    }

    #[test]
    fn rejects_floats_option_counts_and_replication_limits() {
        let float_state = DecisionRequest::new(
            None,
            serde_json::json!({"x": 1.0}),
            vec![(
                "q".into(),
                choice(&[("a", Value::Null), ("b", Value::Null)]),
            )],
        )
        .unwrap();
        assert!(matches!(
            prepare(&float_state),
            Err(HostError::Validation(message)) if message.contains("integer-only")
        ));
        let wide: Vec<(&str, Value)> = (0..17).map(|_| ("o", Value::Null)).collect();
        let labels: Vec<(String, Value)> = wide
            .iter()
            .enumerate()
            .map(|(i, _)| (format!("o{i}"), Value::Null))
            .collect();
        let wide = DecisionRequest::new(
            None,
            Value::String("s".into()),
            vec![(
                "q".into(),
                Question::Choice(ChoiceQuestion {
                    instructions: None,
                    criteria: labels,
                }),
            )],
        )
        .unwrap();
        assert!(prepare(&wide).is_err());
        let questions: Vec<_> = (0..64)
            .map(|index| {
                (
                    format!("q{index}"),
                    Question::Noul(NoulQuestion {
                        instructions: None,
                        true_description: None,
                        false_description: None,
                    }),
                )
            })
            .collect();
        let replicated =
            DecisionRequest::new(None, Value::String("x".repeat(70_000)), questions).unwrap();
        assert!(matches!(
            prepare(&replicated),
            Err(HostError::Validation(message)) if message.contains("replicated")
        ));
    }

    #[test]
    fn misaligned_readouts_are_internal_errors() {
        let request = DecisionRequest::new(
            None,
            Value::String("s".into()),
            vec![(
                "c".into(),
                choice(&[("a", Value::Null), ("b", Value::Null)]),
            )],
        )
        .unwrap();
        let prepared = prepare(&request).unwrap();
        let rows = vec![readout("jev-question-1", &["b", "a"], vec![0.5, 0.5])];
        assert!(matches!(
            project(prepared, rows),
            Err(HostError::Internal(_))
        ));
    }
}
