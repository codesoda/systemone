//! Deterministic fake backend for service tests. No model, no network.

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use systemone_core::{
    Answer, Backend, BackendDescription, BackendId, CallContext, Capabilities, ChoiceAnswer,
    DecisionHost, DecisionRequest, DecisionResponse, Diagnostics, HostError, ModelIdentity,
    NoulAnswer, Primitive, ProviderKind, Question, ScoreAnswer, Usage,
};

#[derive(Default)]
pub struct FakeState {
    pub loads: usize,
    pub calls: usize,
    pub shutdowns: usize,
    pub seen_models: Vec<Option<String>>,
}

#[derive(Default)]
pub struct Gate {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Gate {
    pub fn wait_entered(&self, count: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        while state.0 < count {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
        true
    }

    pub fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }

    fn enter_and_block(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }
}

pub struct FakeBackend {
    pub id: BackendId,
    pub state: Arc<Mutex<FakeState>>,
    pub model: String,
    pub fail_load: bool,
    /// Block inside evaluate until the gate is released.
    pub gate: Option<Arc<Gate>>,
    /// Return this error from every evaluate call.
    pub fail_with: Option<HostError>,
    pub primitives: Vec<Primitive>,
}

impl FakeBackend {
    pub fn new(id: &str) -> Self {
        Self {
            id: BackendId::new(id).unwrap(),
            state: Arc::new(Mutex::new(FakeState::default())),
            model: format!("fake-{id}"),
            fail_load: false,
            gate: None,
            fail_with: None,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
        }
    }
}

impl Backend for FakeBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenJev
    }

    fn describe(&self) -> BackendDescription {
        BackendDescription {
            id: self.id.clone(),
            kind: ProviderKind::OpenJev,
            model: self.model.clone(),
            available: !self.fail_load,
            unavailable_reason: self.fail_load.then(|| "injected".to_owned()),
            settings: serde_json::json!({}),
        }
    }

    fn load(&self) -> Result<Box<dyn DecisionHost>, HostError> {
        self.state.lock().unwrap().loads += 1;
        if self.fail_load {
            return Err(HostError::unavailable("injected load failure"));
        }
        Ok(Box::new(FakeHost {
            state: Arc::clone(&self.state),
            gate: self.gate.clone(),
            fail_with: self.fail_with.clone(),
            capabilities: Capabilities {
                kind: ProviderKind::OpenJev,
                model: ModelIdentity {
                    id: self.model.clone(),
                    description: "fake".into(),
                    release_date: "unknown".into(),
                },
                model_aliases: vec!["jev-latest".into()],
                primitives: self.primitives.clone(),
                max_questions: Some(4),
                max_options: Some(4),
                max_expanded_state_bytes: None,
                confidence_definition: "test".into(),
                probability_definition: "test".into(),
                execution_modes: vec!["direct".into()],
                device: Some("cpu".into()),
                batches_questions: true,
            },
        }))
    }
}

pub struct FakeHost {
    state: Arc<Mutex<FakeState>>,
    gate: Option<Arc<Gate>>,
    fail_with: Option<HostError>,
    capabilities: Capabilities,
}

impl DecisionHost for FakeHost {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn evaluate(
        &mut self,
        request: &DecisionRequest,
        context: &CallContext,
    ) -> Result<DecisionResponse, HostError> {
        {
            let mut state = self.state.lock().unwrap();
            state.calls += 1;
            state.seen_models.push(request.model.clone());
        }
        if let Some(gate) = &self.gate {
            gate.enter_and_block();
        }
        if let Some(error) = &self.fail_with {
            return Err(error.clone());
        }
        context.check()?;
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let answer = match question {
                    Question::Choice(choice) => {
                        let n = choice.criteria.len() as f64;
                        Answer::Choice(ChoiceAnswer {
                            choice: choice.criteria[0].0.clone(),
                            probabilities: choice
                                .criteria
                                .iter()
                                .map(|(label, _)| (label.clone(), 1.0 / n))
                                .collect(),
                            confidence: Some(0.0),
                        })
                    }
                    Question::Noul(_) => Answer::Noul(NoulAnswer {
                        probability_true: 0.25,
                    }),
                    Question::Score(score) => {
                        let n = score.levels.len() as f64;
                        Answer::Score(ScoreAnswer {
                            score: (n - 1.0) / 2.0,
                            probabilities: vec![1.0 / n; score.levels.len()],
                            confidence: Some(0.0),
                            legend: score.levels.clone(),
                        })
                    }
                };
                (id.clone(), answer)
            })
            .collect();
        Ok(DecisionResponse {
            model: self.capabilities.model.id.clone(),
            answers,
            usage: Usage {
                input_tokens: Some(7),
                output_tokens: Some(0),
                cost: None,
            },
            diagnostics: Diagnostics {
                execution: Some("requested=direct; effective=direct".into()),
                fallback: None,
                probability_status: Some("test".into()),
                provider_request_id: None,
                upstream_provider: None,
                truncation: None,
            },
        })
    }

    fn shutdown(&mut self) -> Result<(), HostError> {
        self.state.lock().unwrap().shutdowns += 1;
        Ok(())
    }
}
