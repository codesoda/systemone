//! Neutral SystemOne request ↔ `laya_core` request/evaluation.
//!
//! Adapter policy (documented in the crate README and the SystemOne README):
//!
//! - **Missing `instructions`** become the empty string. The upstream Python
//!   runtime indexes the field directly (`KeyError`); the native TypeSafe
//!   request type makes it optional. Non-string instructions are rendered
//!   the way upstream renders them (`json.dumps`, ASCII-escaped).
//! - **A Choice with one option** is answered deterministically
//!   (`probability 1.0`, `confidence 1.0`) without inference, the same as
//!   the OpenJev adapter, so a caller sees one behaviour across backends.
//!   Upstream Laya cannot score it (the action head needs two entries).
//! - **Noul** descriptions map to the upstream `criteria: {true, false}`
//!   object; absent descriptions use upstream's own defaults.
//! - Every other question in a request goes through **one batched forward
//!   pass**.

use laya_core::{
    Evaluation, LayaError, Question as LayaQuestion, QuestionResult, Request as LayaRequest,
    pyjson::json_text,
};
use serde_json::{Map, Value};
use systemone_core::{
    Answer, ChoiceAnswer, DecisionRequest, Diagnostics, HostError, NoulAnswer, Question,
    ScoreAnswer, Usage,
};

pub const PROBABILITY_STATUS: &str =
    "temperature-calibrated softmax over option logits (checkpoint calibration constants)";
pub const CONFIDENCE_DEFINITION: &str =
    "upstream Laya conf_score: 1 - normalized entropy of the option distribution, clipped to 0..=1";

/// Questions that go to the model, plus how to reassemble the answers.
#[derive(Debug)]
pub struct Prepared {
    pub entries: Vec<Entry>,
    /// `None` when every question was answered deterministically.
    pub request: Option<LayaRequest>,
}

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub route: Route,
}

#[derive(Debug)]
pub enum Route {
    /// Index into the inference request's questions.
    Inference(usize),
    SingletonChoice(String),
}

pub fn prepare(request: &DecisionRequest) -> Result<Prepared, HostError> {
    let mut entries = Vec::with_capacity(request.questions.len());
    let mut inference = Vec::new();
    for (id, question) in &request.questions {
        let route = match question {
            Question::Choice(choice) if choice.criteria.len() == 1 => {
                Route::SingletonChoice(choice.criteria[0].0.clone())
            }
            other => {
                inference.push((id.clone(), to_laya(other)));
                Route::Inference(inference.len() - 1)
            }
        };
        entries.push(Entry {
            id: id.clone(),
            route,
        });
    }
    let request = if inference.is_empty() {
        None
    } else {
        Some(LayaRequest::new(request.state.clone(), inference).map_err(map_error)?)
    };
    Ok(Prepared { entries, request })
}

fn instructions(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => json_text(other, true),
    }
}

fn to_laya(question: &Question) -> LayaQuestion {
    match question {
        Question::Choice(choice) => LayaQuestion::Choice {
            instructions: instructions(choice.instructions.as_ref()),
            criteria: choice.criteria.clone(),
        },
        Question::Score(score) => LayaQuestion::Score {
            instructions: instructions(score.instructions.as_ref()),
            levels: score.levels.clone(),
        },
        Question::Noul(noul) => {
            let mut criteria = Map::new();
            if let Some(value) = &noul.true_description {
                criteria.insert("true".to_owned(), value.clone());
            }
            if let Some(value) = &noul.false_description {
                criteria.insert("false".to_owned(), value.clone());
            }
            LayaQuestion::Noul {
                instructions: instructions(noul.instructions.as_ref()),
                criteria: if criteria.is_empty() {
                    Value::Null
                } else {
                    Value::Object(criteria)
                },
            }
        }
    }
}

/// Map runtime errors onto the neutral classes. Request-shaped problems are
/// validation/unsupported; everything about the machine is unavailable.
pub fn map_error(error: LayaError) -> HostError {
    match error {
        LayaError::InvalidRequest(message) => HostError::validation(message),
        LayaError::OptionsExceedBudget { .. } => HostError::validation(error.to_string()),
        LayaError::SingleOptionChoice { .. } => HostError::unsupported(error.to_string()),
        LayaError::Asset { .. } | LayaError::Checkpoint(_) | LayaError::Unavailable(_) => {
            HostError::unavailable(error.to_string())
        }
        LayaError::Inference(_) | LayaError::Tokenizer(_) => HostError::internal(error.to_string()),
    }
}

/// Reassemble answers in request order.
pub struct Projected {
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
    pub diagnostics: Diagnostics,
}

pub fn project(
    prepared: &Prepared,
    request: &DecisionRequest,
    evaluation: Option<&Evaluation>,
    backend: &str,
) -> Result<Projected, HostError> {
    let mut answers = Vec::with_capacity(prepared.entries.len());
    let mut inferred = 0;
    for (entry, (_, question)) in prepared.entries.iter().zip(&request.questions) {
        let answer = match &entry.route {
            Route::SingletonChoice(label) => Answer::Choice(ChoiceAnswer {
                choice: label.clone(),
                probabilities: vec![(label.clone(), 1.0)],
                confidence: Some(1.0),
            }),
            Route::Inference(index) => {
                let evaluation = evaluation
                    .ok_or_else(|| HostError::internal("inference route without an evaluation"))?;
                let result = evaluation.results.get(*index).ok_or_else(|| {
                    HostError::internal(format!("missing result for question {:?}", entry.id))
                })?;
                if result.id != entry.id {
                    return Err(HostError::internal(format!(
                        "result order mismatch: expected {:?}, got {:?}",
                        entry.id, result.id
                    )));
                }
                inferred += 1;
                project_result(question, result)?
            }
        };
        answers.push((entry.id.clone(), answer));
    }
    let input_tokens = evaluation.map_or(0, |evaluation| evaluation.input_tokens);
    let truncation = evaluation.and_then(|evaluation| {
        (evaluation.truncated_state_tokens > 0 || evaluation.rows_at_max_len > 0).then(|| {
            format!(
                "state_tokens={}; rows_at_max_len={}",
                evaluation.truncated_state_tokens, evaluation.rows_at_max_len
            )
        })
    });
    Ok(Projected {
        answers,
        usage: Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(0),
            cost: None,
        },
        diagnostics: Diagnostics {
            execution: Some(format!(
                "batched; rows={inferred}; deterministic={}; backend={backend}",
                prepared.entries.len() - inferred
            )),
            fallback: None,
            probability_status: Some(PROBABILITY_STATUS.to_owned()),
            provider_request_id: None,
            upstream_provider: None,
            truncation,
        },
    })
}

fn project_result(question: &Question, result: &QuestionResult) -> Result<Answer, HostError> {
    let probabilities: Vec<f64> = result.probabilities.iter().map(|p| f64::from(*p)).collect();
    let confidence = Some(f64::from(result.confidence));
    match question {
        Question::Choice(choice) => {
            if probabilities.len() != choice.criteria.len() {
                return Err(HostError::internal(format!(
                    "choice distribution has {} entries for {} options",
                    probabilities.len(),
                    choice.criteria.len()
                )));
            }
            let label = choice
                .criteria
                .get(result.argmax)
                .map(|(label, _)| label.clone())
                .ok_or_else(|| HostError::internal("argmax outside the option list"))?;
            Ok(Answer::Choice(ChoiceAnswer {
                choice: label,
                probabilities: choice
                    .criteria
                    .iter()
                    .map(|(label, _)| label.clone())
                    .zip(probabilities)
                    .collect(),
                confidence,
            }))
        }
        Question::Noul(_) => {
            // Upstream: index 1 is `true`.
            let probability_true = *probabilities.get(1).ok_or_else(|| {
                HostError::internal("noul distribution has fewer than two entries")
            })?;
            Ok(Answer::Noul(NoulAnswer { probability_true }))
        }
        Question::Score(score) => {
            if probabilities.len() != score.levels.len() {
                return Err(HostError::internal(format!(
                    "score distribution has {} entries for {} levels",
                    probabilities.len(),
                    score.levels.len()
                )));
            }
            Ok(Answer::Score(ScoreAnswer {
                score: result
                    .expected_score
                    .ok_or_else(|| HostError::internal("score result without expected score"))?,
                probabilities,
                confidence,
                legend: score.levels.clone(),
            }))
        }
    }
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
