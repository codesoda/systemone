//! Neutral SystemOne request ↔ gliner2-rs classification requests/scores.
//!
//! Adapter policy (documented in the SystemOne README):
//!
//! - **State** becomes the classified text: a JSON string is used as-is,
//!   anything else is compact JSON with non-ASCII intact. GLiNER has no
//!   Python prompt to match, so readability wins over escaping.
//! - **Instructions** become the task prompt (`"{id}: {instructions}"` in
//!   the upstream template). The question ID is the task name; it is
//!   visible to the model, so name questions meaningfully.
//! - **Choice** maps options to labels in order and option values to label
//!   descriptions. The answer is a full **softmax** over the options.
//! - **Noul** maps to two ordered labels (`settings.noul_labels`, default
//!   `["no", "yes"]`) with the false/true descriptions attached.
//!   `probability_true` is the softmax mass on the second label.
//! - **Score** maps levels to labels in order; the score is
//!   `sum(i * p[i])` over the softmax. This is a derived ordinal
//!   classification, not a trained scoring head.
//! - **A Choice with one option** is answered deterministically without
//!   inference, the same as the other local adapters.
//! - Each question is **one encoder pass on its own**. Scoring several
//!   tasks in one prompt changes every task's logits (the tasks see each
//!   other), so an answer would depend on which other questions were asked.
//!   Independence costs one pass per question and is deliberate.
//! - Sigmoid is never requested. Every distribution here is categorical.

use gliner2_rs::{Activation, ClassificationRequest, ClassificationScores, scores::ScoresError};
use serde_json::Value;
use systemone_core::{
    Answer, ChoiceAnswer, DecisionRequest, Diagnostics, HostError, NoulAnswer, Question,
    ScoreAnswer, Usage,
};

pub const PROBABILITY_STATUS: &str = "softmax over GLiNER2.5 classifier label logits (checkpoint classification_temperature applied once); not calibrated as decision probabilities";
pub const CONFIDENCE_DEFINITION: &str =
    "adapter-derived normalized margin: (max_p - 1/n) / (1 - 1/n) over the option distribution";

/// The text every question is scored against, plus per-question routes.
#[derive(Debug)]
pub struct Prepared {
    pub text: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub route: Route,
}

#[derive(Debug)]
pub enum Route {
    Inference(Box<ClassificationRequest>),
    SingletonChoice(String),
}

impl Prepared {
    /// Number of questions that need the model.
    #[must_use]
    pub fn inference_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.route, Route::Inference(_)))
            .count()
    }
}

pub fn prepare(
    request: &DecisionRequest,
    noul_labels: &[String; 2],
) -> Result<Prepared, HostError> {
    let text = render_text(&request.state);
    let mut entries = Vec::with_capacity(request.questions.len());
    for (id, question) in &request.questions {
        let route = match question {
            Question::Choice(choice) if choice.criteria.len() == 1 => {
                Route::SingletonChoice(choice.criteria[0].0.clone())
            }
            other => {
                let request = to_request(id, other, noul_labels)?;
                request
                    .validate()
                    .map_err(|error| scores_error(id, &error))?;
                Route::Inference(Box::new(request))
            }
        };
        entries.push(Entry {
            id: id.clone(),
            route,
        });
    }
    Ok(Prepared { text, entries })
}

/// Render a JSON value the way the model should read it.
#[must_use]
pub fn render_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn instruction(value: Option<&Value>) -> Option<String> {
    value.map(render_text).filter(|text| !text.is_empty())
}

fn to_request(
    id: &str,
    question: &Question,
    noul_labels: &[String; 2],
) -> Result<ClassificationRequest, HostError> {
    let (labels, descriptions, instructions) = match question {
        Question::Choice(choice) => {
            let labels: Vec<String> = choice
                .criteria
                .iter()
                .map(|(label, _)| label.clone())
                .collect();
            let descriptions = choice
                .criteria
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(label, value)| (label.clone(), render_text(value)))
                .collect();
            (labels, descriptions, choice.instructions.as_ref())
        }
        Question::Noul(noul) => {
            let mut descriptions = Vec::new();
            if let Some(value) = &noul.false_description {
                descriptions.push((noul_labels[0].clone(), render_text(value)));
            }
            if let Some(value) = &noul.true_description {
                descriptions.push((noul_labels[1].clone(), render_text(value)));
            }
            (
                noul_labels.to_vec(),
                descriptions,
                noul.instructions.as_ref(),
            )
        }
        Question::Score(score) => {
            let labels: Vec<String> = score.levels.iter().map(render_text).collect();
            for (index, label) in labels.iter().enumerate() {
                if label.is_empty() || labels[..index].contains(label) {
                    return Err(HostError::validation(format!(
                        "questions.{id:?}.criteria: level {index} renders as {label:?}, which is empty or repeats an earlier level; gliner2 needs distinct non-empty level labels"
                    )));
                }
            }
            (labels, Vec::new(), score.instructions.as_ref())
        }
    };
    let mut request = ClassificationRequest::new(id, labels, Activation::Softmax)
        .with_label_descriptions(descriptions);
    if let Some(text) = instruction(instructions) {
        request = request.with_instruction(text);
    }
    Ok(request)
}

fn scores_error(id: &str, error: &ScoresError) -> HostError {
    HostError::validation(format!("questions.{id:?}: {error}"))
}

/// Map an upstream failure onto the neutral classes. Request-shaped problems
/// surface as `ScoresError` somewhere in the chain; everything else is the
/// machine's fault.
pub fn map_error(id: &str, error: &anyhow::Error) -> HostError {
    if let Some(scores) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ScoresError>())
    {
        return scores_error(id, scores);
    }
    HostError::internal(format!("questions.{id:?}: {error:#}"))
}

/// Reassembled answers in request order.
pub struct Projected {
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
    pub diagnostics: Diagnostics,
}

/// `scores` is aligned with `prepared.entries`; `None` for deterministic
/// routes.
pub fn project(
    prepared: &Prepared,
    request: &DecisionRequest,
    scores: &[Option<ClassificationScores>],
    max_len: usize,
) -> Result<Projected, HostError> {
    if scores.len() != prepared.entries.len() {
        return Err(HostError::internal(format!(
            "{} score slots for {} questions",
            scores.len(),
            prepared.entries.len()
        )));
    }
    let mut answers = Vec::with_capacity(prepared.entries.len());
    let mut input_tokens = 0_u64;
    let mut dropped_words = 0_usize;
    let mut inferred = 0_usize;
    for ((entry, (_, question)), scores) in
        prepared.entries.iter().zip(&request.questions).zip(scores)
    {
        let answer = match (&entry.route, scores) {
            (Route::SingletonChoice(label), None) => Answer::Choice(ChoiceAnswer {
                choice: label.clone(),
                probabilities: vec![(label.clone(), 1.0)],
                confidence: Some(1.0),
            }),
            (Route::Inference(expected), Some(scores)) => {
                if scores.labels() != expected.labels.as_slice() {
                    return Err(HostError::internal(format!(
                        "label order changed for question {:?}",
                        entry.id
                    )));
                }
                inferred += 1;
                input_tokens += scores.usage().input_tokens as u64;
                dropped_words = dropped_words.max(scores.usage().truncated_words);
                project_scores(question, scores)?
            }
            _ => {
                return Err(HostError::internal(format!(
                    "route and scores disagree for question {:?}",
                    entry.id
                )));
            }
        };
        answers.push((entry.id.clone(), answer));
    }
    let truncation =
        (dropped_words > 0).then(|| format!("words_dropped={dropped_words}; max_len={max_len}"));
    Ok(Projected {
        answers,
        usage: Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(0),
            cost: None,
        },
        diagnostics: Diagnostics {
            execution: Some(format!(
                "per-question; rows={inferred}; deterministic={}; backend=onnxruntime-cpu",
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

fn project_scores(question: &Question, scores: &ClassificationScores) -> Result<Answer, HostError> {
    if !scores.is_categorical() {
        return Err(HostError::internal(
            "gliner2 adapter received a non-categorical distribution",
        ));
    }
    let probabilities: Vec<f64> = scores
        .probabilities()
        .iter()
        .map(|p| f64::from(*p))
        .collect();
    let confidence = Some(normalized_margin(&probabilities));
    match question {
        Question::Choice(choice) => {
            let label = choice
                .criteria
                .get(scores.argmax())
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
            let expected: f64 = probabilities
                .iter()
                .enumerate()
                .map(|(index, probability)| index as f64 * probability)
                .sum();
            Ok(Answer::Score(ScoreAnswer {
                score: expected,
                probabilities,
                confidence,
                legend: score.levels.clone(),
            }))
        }
    }
}

/// `(max - 1/n) / (1 - 1/n)`: 0 at uniform, 1 at certainty.
#[must_use]
pub fn normalized_margin(probabilities: &[f64]) -> f64 {
    let count = probabilities.len();
    if count < 2 {
        return 1.0;
    }
    let maximum = probabilities.iter().copied().fold(0.0_f64, f64::max);
    let baseline = 1.0 / count as f64;
    ((maximum - baseline) / (1.0 - baseline)).clamp(0.0, 1.0)
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
