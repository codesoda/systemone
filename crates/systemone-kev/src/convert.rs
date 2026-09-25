//! Neutral SystemOne request ↔ `kev_core` request/evaluation.
//!
//! Adapter policy:
//!
//! - **Missing `instructions`** become JSON `null`, which upstream renders
//!   as the empty string — the same text the Python runtime produces for a
//!   request without instructions.
//! - **Noul** descriptions map to upstream's `criteria: {true, false}`
//!   object; absent descriptions fall back to the bare `yes`/`no` option
//!   texts, upstream's own default.
//! - Every question goes through the runtime; a single-option Choice is a
//!   softmax over one option (probability 1.0, confidence 1.0), which is
//!   what upstream serves and matches the deterministic answer the other
//!   local adapters give.
//! - `usage.output_tokens` follows upstream kev: the token count of the
//!   serialised answers (a billing-style figure, not generation).

#[cfg(feature = "kev")]
use serde_json::Map;
#[cfg(feature = "kev")]
use serde_json::Value;
use systemone_core::{
    Answer, ChoiceAnswer, DecisionRequest, Diagnostics, HostError, NoulAnswer, Question,
    ScoreAnswer, Usage,
};

pub const PROBABILITY_STATUS: &str =
    "temperature-calibrated softmax over pointer-head logits (checkpoint calibration constant)";
pub const CONFIDENCE_DEFINITION: &str = "upstream Kev: choice = (max - 1/K)/(1 - 1/K); score = 1 - E|level - mode|/(L-1); noul carries none";

/// Build the upstream-shaped kev request from a neutral one.
#[cfg(feature = "kev")]
pub fn to_kev(request: &DecisionRequest) -> Result<kev_core::SystemOneRequest, HostError> {
    let mut questions = Map::new();
    for (id, question) in &request.questions {
        questions.insert(id.clone(), question_value(question));
    }
    let raw = serde_json::json!({
        "state": request.state,
        "questions": questions,
    });
    serde_json::from_value(raw).map_err(|error| {
        HostError::validation(format!("request does not fit the kev wire: {error}"))
    })
}

#[cfg(feature = "kev")]
fn instructions(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

#[cfg(feature = "kev")]
fn question_value(question: &Question) -> Value {
    match question {
        Question::Choice(choice) => {
            let mut criteria = Map::new();
            for (label, description) in &choice.criteria {
                criteria.insert(label.clone(), description.clone());
            }
            serde_json::json!({
                "type": "choice",
                "instructions": instructions(choice.instructions.as_ref()),
                "criteria": criteria,
            })
        }
        Question::Score(score) => serde_json::json!({
            "type": "score",
            "instructions": instructions(score.instructions.as_ref()),
            "criteria": score.levels,
        }),
        Question::Noul(noul) => {
            let mut criteria = Map::new();
            if let Some(value) = &noul.true_description {
                criteria.insert("true".to_owned(), value.clone());
            }
            if let Some(value) = &noul.false_description {
                criteria.insert("false".to_owned(), value.clone());
            }
            let mut body = Map::new();
            body.insert("type".to_owned(), "noul".into());
            body.insert(
                "instructions".to_owned(),
                instructions(noul.instructions.as_ref()),
            );
            if !criteria.is_empty() {
                body.insert("criteria".to_owned(), Value::Object(criteria));
            }
            Value::Object(body)
        }
    }
}

/// Map runtime errors onto the neutral classes. Request-shaped problems are
/// validation; everything about the machine is unavailable.
#[cfg(feature = "kev")]
pub fn map_error(error: kev_core::KevError) -> HostError {
    use kev_core::KevError;
    match error {
        KevError::InvalidRequest(message) => HostError::validation(message),
        KevError::ContextOverflow(message) => HostError::validation(message),
        KevError::Load(message) => HostError::unavailable(message),
        KevError::Inference(message) => HostError::internal(message),
        KevError::Io(inner) => HostError::unavailable(inner.to_string()),
    }
}

/// Reassemble answers in request order from the runtime's full-precision
/// probabilities (the systemone wire applies its own rounding).
#[derive(Debug)]
pub struct Projected {
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
    pub diagnostics: Diagnostics,
}

pub struct EvaluationView<'a> {
    pub probs: &'a [Vec<f64>],
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub prefix_cache_hit: bool,
}

pub fn project(
    request: &DecisionRequest,
    evaluation: &EvaluationView<'_>,
    backend: &str,
) -> Result<Projected, HostError> {
    if evaluation.probs.len() != request.questions.len() {
        return Err(HostError::internal(format!(
            "runtime returned {} distributions for {} questions",
            evaluation.probs.len(),
            request.questions.len()
        )));
    }
    let mut answers = Vec::with_capacity(request.questions.len());
    for ((id, question), probs) in request.questions.iter().zip(evaluation.probs) {
        answers.push((id.clone(), project_one(question, probs)?));
    }
    Ok(Projected {
        answers,
        usage: Usage {
            input_tokens: Some(evaluation.input_tokens as u64),
            // Upstream kev semantics: tokens of the serialised answers.
            output_tokens: Some(evaluation.output_tokens as u64),
            cost: None,
        },
        diagnostics: Diagnostics {
            execution: Some(format!(
                "batched; rows={}; prefix_cache={}; backend={backend}",
                evaluation.probs.len(),
                if evaluation.prefix_cache_hit {
                    "hit"
                } else {
                    "miss"
                }
            )),
            fallback: None,
            probability_status: Some(PROBABILITY_STATUS.to_owned()),
            provider_request_id: None,
            truncation: None,
        },
    })
}

fn project_one(question: &Question, probs: &[f64]) -> Result<Answer, HostError> {
    match question {
        Question::Choice(choice) => {
            if probs.len() != choice.criteria.len() {
                return Err(HostError::internal(format!(
                    "choice distribution has {} entries for {} options",
                    probs.len(),
                    choice.criteria.len()
                )));
            }
            let top = argmax(probs)?;
            let label = choice
                .criteria
                .get(top)
                .map(|(label, _)| label.clone())
                .ok_or_else(|| HostError::internal("argmax outside the option list"))?;
            Ok(Answer::Choice(ChoiceAnswer {
                choice: label,
                probabilities: choice
                    .criteria
                    .iter()
                    .map(|(label, _)| label.clone())
                    .zip(probs.iter().copied())
                    .collect(),
                confidence: Some(choice_confidence(probs)),
            }))
        }
        Question::Noul(_) => {
            let probability_true = *probs.get(1).ok_or_else(|| {
                HostError::internal("noul distribution has fewer than two entries")
            })?;
            Ok(Answer::Noul(NoulAnswer { probability_true }))
        }
        Question::Score(score) => {
            if probs.len() != score.levels.len() {
                return Err(HostError::internal(format!(
                    "score distribution has {} entries for {} levels",
                    probs.len(),
                    score.levels.len()
                )));
            }
            let expected: f64 = probs.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
            Ok(Answer::Score(ScoreAnswer {
                score: expected,
                probabilities: probs.to_vec(),
                confidence: Some(score_confidence(probs)?),
                legend: score.levels.clone(),
            }))
        }
    }
}

fn argmax(probs: &[f64]) -> Result<usize, HostError> {
    probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(index, _)| index)
        .ok_or_else(|| HostError::internal("empty distribution"))
}

/// Upstream `api.choice_confidence`: (max - 1/K) / (1 - 1/K); 1.0 for K=1.
fn choice_confidence(probs: &[f64]) -> f64 {
    let k = probs.len() as f64;
    if probs.len() == 1 {
        return 1.0;
    }
    let max = probs.iter().copied().fold(f64::MIN, f64::max);
    ((max - 1.0 / k) / (1.0 - 1.0 / k)).clamp(0.0, 1.0)
}

/// Upstream `api.score_confidence`: 1 - E|level - mode| / (L - 1).
fn score_confidence(probs: &[f64]) -> Result<f64, HostError> {
    let l = probs.len();
    if l == 1 {
        return Ok(1.0);
    }
    let mode = argmax(probs)?;
    let expected: f64 = probs
        .iter()
        .enumerate()
        .map(|(i, p)| p * (i as f64 - mode as f64).abs())
        .sum();
    Ok((1.0 - expected / (l as f64 - 1.0)).clamp(0.0, 1.0))
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
