use serde::{Deserialize, Serialize};

use crate::{HostError, ProviderKind, Question};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Primitive {
    Choice,
    Noul,
    Score,
}

impl Primitive {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Noul => "noul",
            Self::Score => "score",
        }
    }
}

/// Identity of the model a host actually serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelIdentity {
    /// Public model ID returned in responses and `/v1/models`.
    pub id: String,
    pub description: String,
    /// Publication date if known; `"unknown"` otherwise (never invented).
    pub release_date: String,
}

/// What a loaded host can do and how its numbers should be read.
///
/// The service checks requests against these limits before dispatch so hosts
/// receive only work they declared they can handle, and `/v1/backends`
/// exposes them for discovery.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Capabilities {
    pub kind: ProviderKind,
    pub model: ModelIdentity,
    /// Compatibility aliases (for example `jev-latest`) that resolve to
    /// `model.id` on this instance.
    pub model_aliases: Vec<String>,
    /// Which primitives are natively supported. Anything else is rejected
    /// with `HostError::Unsupported`, never derived silently.
    pub primitives: Vec<Primitive>,
    pub max_questions: Option<usize>,
    /// Maximum choice options / score levels per question.
    pub max_options: Option<usize>,
    /// Maximum serialized state bytes replicated across questions.
    pub max_expanded_state_bytes: Option<usize>,
    /// Human-readable definition of `confidence` values.
    pub confidence_definition: String,
    /// Human-readable definition of probabilities.
    pub probability_definition: String,
    /// Execution modes the host may use (informational).
    pub execution_modes: Vec<String>,
    /// Device/precision disclosure for local hosts.
    pub device: Option<String>,
    /// Whether the host performs one native call per request (`true`) or
    /// one per question.
    pub batches_questions: bool,
}

impl Capabilities {
    /// Reject a request that exceeds declared limits or uses unsupported
    /// primitives. Adapter-specific value rules are checked by the adapter.
    pub fn check(&self, request: &crate::DecisionRequest) -> Result<(), HostError> {
        if let Some(max) = self.max_questions
            && request.questions.len() > max
        {
            return Err(HostError::validation(format!(
                "questions must not contain more than {max} entries for this backend"
            )));
        }
        for (id, question) in &request.questions {
            let primitive = question.primitive();
            if !self.primitives.contains(&primitive) {
                return Err(HostError::unsupported(format!(
                    "questions.{id:?}: {} is not supported by backend model {}",
                    primitive.as_str(),
                    self.model.id
                )));
            }
            if let Some(max) = self.max_options {
                let count = match question {
                    Question::Choice(choice) => choice.criteria.len(),
                    Question::Score(score) => score.levels.len(),
                    Question::Noul(_) => 2,
                };
                if count > max {
                    return Err(HostError::validation(format!(
                        "questions.{id:?} has {count} options; this backend allows at most {max}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Resolve a requested model against this host: `None` and known aliases
    /// map to the served model; anything else is an error.
    pub fn resolve_model(&self, requested: Option<&str>) -> Result<&str, HostError> {
        match requested {
            None => Ok(&self.model.id),
            Some(model) if model == self.model.id => Ok(&self.model.id),
            Some(model) if self.model_aliases.iter().any(|alias| alias == model) => {
                Ok(&self.model.id)
            }
            Some(model) => Err(HostError::not_found(format!(
                "model {model:?} is not served by this backend (serving {})",
                self.model.id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::{ChoiceQuestion, DecisionRequest, NoulQuestion};

    fn capabilities() -> Capabilities {
        Capabilities {
            kind: ProviderKind::OpenJev,
            model: ModelIdentity {
                id: "m".into(),
                description: String::new(),
                release_date: "unknown".into(),
            },
            model_aliases: vec!["jev-latest".into()],
            primitives: vec![Primitive::Choice],
            max_questions: Some(1),
            max_options: Some(2),
            max_expanded_state_bytes: None,
            confidence_definition: String::new(),
            probability_definition: String::new(),
            execution_modes: vec![],
            device: None,
            batches_questions: false,
        }
    }

    #[test]
    fn checks_limits_and_primitives() {
        let capabilities = capabilities();
        let choice = |n: usize| {
            Question::Choice(ChoiceQuestion {
                instructions: None,
                criteria: (0..n).map(|i| (i.to_string(), Value::Null)).collect(),
            })
        };
        let ok = DecisionRequest::new(None, Value::Null, vec![("a".into(), choice(2))]).unwrap();
        assert!(capabilities.check(&ok).is_ok());
        let wide = DecisionRequest::new(None, Value::Null, vec![("a".into(), choice(3))]).unwrap();
        assert!(matches!(
            capabilities.check(&wide),
            Err(HostError::Validation(_))
        ));
        let many = DecisionRequest::new(
            None,
            Value::Null,
            vec![("a".into(), choice(2)), ("b".into(), choice(2))],
        )
        .unwrap();
        assert!(capabilities.check(&many).is_err());
        let noul = DecisionRequest::new(
            None,
            Value::Null,
            vec![(
                "n".into(),
                Question::Noul(NoulQuestion {
                    instructions: None,
                    true_description: None,
                    false_description: None,
                }),
            )],
        )
        .unwrap();
        assert!(matches!(
            capabilities.check(&noul),
            Err(HostError::Unsupported(_))
        ));
    }

    #[test]
    fn resolves_aliases_but_not_foreign_models() {
        let capabilities = capabilities();
        assert_eq!(capabilities.resolve_model(None).unwrap(), "m");
        assert_eq!(capabilities.resolve_model(Some("jev-latest")).unwrap(), "m");
        assert_eq!(capabilities.resolve_model(Some("m")).unwrap(), "m");
        assert!(capabilities.resolve_model(Some("other")).is_err());
    }
}
