use serde_json::Value;

use crate::HostError;

/// A Jev-style typed decision request after wire validation.
///
/// Ordering is preserved everywhere: questions, choice criteria and score
/// levels keep the caller's order because option identity and position are
/// part of the answer. Values remain arbitrary JSON until a host adapter
/// decides what it can render; this type never coerces them.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionRequest {
    /// Requested model within the selected backend; `None` uses the instance
    /// default.
    pub model: Option<String>,
    /// The state as supplied (may be `null` or empty; adapters decide).
    pub state: Value,
    /// External question ID → question, in wire order. IDs are unique.
    pub questions: Vec<(String, Question)>,
}

impl DecisionRequest {
    pub fn new(
        model: Option<String>,
        state: Value,
        questions: Vec<(String, Question)>,
    ) -> Result<Self, HostError> {
        if questions.is_empty() {
            return Err(HostError::validation(
                "questions must contain at least one question",
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(questions.len());
        for (id, question) in &questions {
            if !seen.insert(id.as_str()) {
                return Err(HostError::validation(format!(
                    "duplicate question ID {id:?}"
                )));
            }
            question.validate(id)?;
        }
        Ok(Self {
            model,
            state,
            questions,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Question {
    Choice(ChoiceQuestion),
    Noul(NoulQuestion),
    Score(ScoreQuestion),
}

impl Question {
    #[must_use]
    pub const fn primitive(&self) -> crate::Primitive {
        match self {
            Self::Choice(_) => crate::Primitive::Choice,
            Self::Noul(_) => crate::Primitive::Noul,
            Self::Score(_) => crate::Primitive::Score,
        }
    }

    fn validate(&self, id: &str) -> Result<(), HostError> {
        match self {
            Self::Choice(choice) => {
                if choice.criteria.is_empty() {
                    return Err(HostError::validation(format!(
                        "questions.{id:?}.criteria must contain at least one option"
                    )));
                }
                let mut seen = std::collections::HashSet::new();
                for (label, _) in &choice.criteria {
                    if !seen.insert(label.as_str()) {
                        return Err(HostError::validation(format!(
                            "questions.{id:?}.criteria has duplicate label {label:?}"
                        )));
                    }
                }
            }
            Self::Noul(_) => {}
            Self::Score(score) => {
                if score.levels.len() < 2 {
                    return Err(HostError::validation(format!(
                        "questions.{id:?}.criteria must contain at least two levels"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Choose one of the supplied options.
#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceQuestion {
    /// Optional instructions (string, object, array or null on the wire).
    pub instructions: Option<Value>,
    /// Ordered `label → description` pairs. Descriptions may be null.
    pub criteria: Vec<(String, Value)>,
}

/// Probability that a proposition is true.
#[derive(Clone, Debug, PartialEq)]
pub struct NoulQuestion {
    pub instructions: Option<Value>,
    /// Optional description of the `true` outcome.
    pub true_description: Option<Value>,
    /// Optional description of the `false` outcome.
    pub false_description: Option<Value>,
}

/// Expected zero-based level on an ordered rubric.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreQuestion {
    pub instructions: Option<Value>,
    /// Ordered level descriptions; index is the level value.
    pub levels: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noul() -> Question {
        Question::Noul(NoulQuestion {
            instructions: None,
            true_description: None,
            false_description: None,
        })
    }

    #[test]
    fn rejects_empty_duplicate_and_underspecified_questions() {
        assert!(DecisionRequest::new(None, Value::Null, vec![]).is_err());
        assert!(
            DecisionRequest::new(
                None,
                Value::Null,
                vec![("a".into(), noul()), ("a".into(), noul())]
            )
            .is_err()
        );
        let empty_choice = Question::Choice(ChoiceQuestion {
            instructions: None,
            criteria: vec![],
        });
        assert!(DecisionRequest::new(None, Value::Null, vec![("q".into(), empty_choice)]).is_err());
        let dup = Question::Choice(ChoiceQuestion {
            instructions: None,
            criteria: vec![("x".into(), Value::Null), ("x".into(), Value::Null)],
        });
        assert!(DecisionRequest::new(None, Value::Null, vec![("q".into(), dup)]).is_err());
        let one_level = Question::Score(ScoreQuestion {
            instructions: None,
            levels: vec![Value::Null],
        });
        assert!(DecisionRequest::new(None, Value::Null, vec![("q".into(), one_level)]).is_err());
        assert!(DecisionRequest::new(None, Value::Null, vec![("q".into(), noul())]).is_ok());
    }
}
