use serde::Serialize;
use serde_json::Value;

use crate::HostError;

/// Result of evaluating a [`crate::DecisionRequest`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionResponse {
    /// The model that actually produced the answers (never an alias).
    pub model: String,
    /// External question ID → answer, in request order.
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
    /// Routing/execution evidence for headers and logs; not part of the Jev
    /// body.
    #[serde(default)]
    pub diagnostics: Diagnostics,
}

impl DecisionResponse {
    /// Validate that every distribution is finite, nonnegative, correctly
    /// sized and normalized within `tolerance`. Hosts call this before
    /// returning; the service refuses to repair corrupt responses.
    pub fn validate(&self, tolerance: f64) -> Result<(), HostError> {
        for (id, answer) in &self.answers {
            match answer {
                Answer::Choice(choice) => {
                    let probabilities: Vec<f64> =
                        choice.probabilities.iter().map(|(_, p)| *p).collect();
                    check_distribution(id, &probabilities, tolerance)?;
                    if !choice
                        .probabilities
                        .iter()
                        .any(|(label, _)| label == &choice.choice)
                    {
                        return Err(HostError::internal(format!(
                            "answer {id:?} selected a label outside its distribution"
                        )));
                    }
                    check_unit(id, "confidence", choice.confidence)?;
                }
                Answer::Noul(noul) => {
                    check_distribution(
                        id,
                        &[noul.probability_true, 1.0 - noul.probability_true],
                        tolerance,
                    )?;
                }
                Answer::Score(score) => {
                    check_distribution(id, &score.probabilities, tolerance)?;
                    if score.probabilities.len() != score.legend.len() {
                        return Err(HostError::internal(format!(
                            "answer {id:?} distribution and legend lengths differ"
                        )));
                    }
                    let max_level = (score.legend.len() - 1) as f64;
                    if !score.score.is_finite() || score.score < 0.0 || score.score > max_level {
                        return Err(HostError::internal(format!(
                            "answer {id:?} score {} is outside 0..={max_level}",
                            score.score
                        )));
                    }
                    check_unit(id, "confidence", score.confidence)?;
                }
            }
        }
        Ok(())
    }
}

fn check_unit(id: &str, field: &str, value: Option<f64>) -> Result<(), HostError> {
    if let Some(value) = value
        && (!value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(HostError::internal(format!(
            "answer {id:?} {field} {value} is outside 0..=1"
        )));
    }
    Ok(())
}

fn check_distribution(id: &str, probabilities: &[f64], tolerance: f64) -> Result<(), HostError> {
    if probabilities.is_empty() {
        return Err(HostError::internal(format!(
            "answer {id:?} has an empty distribution"
        )));
    }
    if probabilities
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(HostError::internal(format!(
            "answer {id:?} contains a non-finite or negative probability"
        )));
    }
    let total: f64 = probabilities.iter().sum();
    if (total - 1.0).abs() > tolerance {
        return Err(HostError::internal(format!(
            "answer {id:?} distribution sums to {total}, not 1 within {tolerance}"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice(ChoiceAnswer),
    Noul(NoulAnswer),
    Score(ScoreAnswer),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChoiceAnswer {
    pub choice: String,
    /// Full ordered distribution over the request's labels (unrounded).
    pub probabilities: Vec<(String, f64)>,
    /// Host-defined confidence; see `Capabilities::confidence_definition`.
    pub confidence: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NoulAnswer {
    /// P(true), unrounded.
    pub probability_true: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScoreAnswer {
    /// Expected zero-based level.
    pub score: f64,
    /// Ordered level distribution (unrounded).
    pub probabilities: Vec<f64>,
    pub confidence: Option<f64>,
    /// The request's level descriptions, echoed in order.
    pub legend: Vec<Value>,
}

/// Provider usage. `None` means the provider did not report a value; it is
/// never invented as zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-reported cost in the provider's unit, if any.
    pub cost: Option<f64>,
}

/// Non-body evidence about how a response was produced.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Diagnostics {
    /// Short execution disclosure, e.g. `requested=shared; effective=serial`.
    pub execution: Option<String>,
    /// Why a requested execution path was not used.
    pub fallback: Option<String>,
    /// Honest statement of what the probabilities mean.
    pub probability_status: Option<String>,
    /// Provider-assigned request identity, if any.
    pub provider_request_id: Option<String>,
    /// What the host cut to fit its budget, e.g. `state_tokens=120;
    /// rows_at_max_len=2`. `None` when nothing was truncated.
    pub truncation: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(answers: Vec<(String, Answer)>) -> DecisionResponse {
        DecisionResponse {
            model: "m".into(),
            answers,
            usage: Usage::default(),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn validation_rejects_corrupt_distributions() {
        let good = response(vec![(
            "c".into(),
            Answer::Choice(ChoiceAnswer {
                choice: "a".into(),
                probabilities: vec![("a".into(), 0.6), ("b".into(), 0.4)],
                confidence: Some(0.2),
            }),
        )]);
        assert!(good.validate(1e-6).is_ok());
        let unnormalized = response(vec![(
            "c".into(),
            Answer::Choice(ChoiceAnswer {
                choice: "a".into(),
                probabilities: vec![("a".into(), 0.6), ("b".into(), 0.6)],
                confidence: None,
            }),
        )]);
        assert!(unnormalized.validate(1e-6).is_err());
        let foreign_choice = response(vec![(
            "c".into(),
            Answer::Choice(ChoiceAnswer {
                choice: "z".into(),
                probabilities: vec![("a".into(), 1.0)],
                confidence: None,
            }),
        )]);
        assert!(foreign_choice.validate(1e-6).is_err());
        let nan = response(vec![(
            "n".into(),
            Answer::Noul(NoulAnswer {
                probability_true: f64::NAN,
            }),
        )]);
        assert!(nan.validate(1e-6).is_err());
        let score = response(vec![(
            "s".into(),
            Answer::Score(ScoreAnswer {
                score: 2.5,
                probabilities: vec![0.5, 0.5],
                confidence: None,
                legend: vec![Value::Null, Value::Null],
            }),
        )]);
        assert!(score.validate(1e-6).is_err());
    }
}
