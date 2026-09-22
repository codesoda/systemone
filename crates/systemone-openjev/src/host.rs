//! Loaded OpenJev engine behind the neutral `DecisionHost` trait.
//!
//! The engine owns llama.cpp state on its own thread; this host is driven
//! from the service's owner thread. Multi-question requests attempt native
//! shared-prefix execution when an exact-configuration probe receipt exists
//! and otherwise use the explicit serial full-prompt fallback, exactly as the
//! former OpenJev server did. `require_shared` turns that fallback into an
//! error.

use openjev_core::{
    CONFIDENCE_STATUS, Decision, DecisionOption, ExecutionMode, ModelMetadata, Question, Readout,
    StateValue, normalized_margin,
};
use openjev_llama::{
    BackendError, CacheOptions, EngineHandle, EngineOptions, ModelCache, ModelRegistry,
    ProbeEligibility, ProbeMode, load_passing_receipt,
};
use systemone_core::{
    CallContext, Capabilities, DecisionHost, DecisionRequest, DecisionResponse, HostError,
    ModelIdentity, Primitive, ProviderKind,
};

use crate::{
    convert::{self, CONFIDENCE_DEFINITION, MAX_OPTIONS, MAX_QUESTIONS, PROBABILITY_STATUS},
    probe::probe_configuration,
    settings::{ResolvedSettings, device_name},
};

pub const SHARED_FALLBACK_REASON: &str = "no passing exact-configuration shared probe receipt";
pub const DISTRIBUTION_TOLERANCE: f64 = 1e-6;
const WARMUP_ID: &str = "systemone-openjev-warmup";

pub struct OpenJevHost {
    engine: Option<EngineHandle>,
    shared_probe: Result<ProbeEligibility, String>,
    capabilities: Capabilities,
    max_sequences: u32,
    require_shared: bool,
}

impl OpenJevHost {
    /// Resolve the artifact, spawn the engine and run one disclosed warmup
    /// decision so readiness means "has answered once".
    pub fn load(settings: &ResolvedSettings, aliases: &[String]) -> Result<Self, HostError> {
        let registry = ModelRegistry::bundled().map_err(runtime)?;
        let cache = ModelCache::new(settings.cache_dir.clone());
        let resolved = openjev_llama::resolve_model_spec(
            &registry,
            &cache,
            &settings.model,
            CacheOptions {
                offline: settings.offline,
                repair: false,
            },
        )
        .map_err(runtime)?;
        let configuration = probe_configuration(
            settings,
            resolved.model().artifact_sha256(),
            resolved.model().profile(),
        );
        let model_id = resolved.model().id().to_owned();
        let shared_probe =
            load_passing_receipt(&cache, &model_id, ProbeMode::Shared, &configuration)
                .map_err(|error| error.to_string());
        let options = EngineOptions {
            device: settings.device,
            gpu_layers: settings.gpu_layers,
            threads: settings.threads,
            n_ctx: settings.n_ctx,
            max_tokens: settings.max_tokens,
            max_context_tokens: settings.max_context_tokens,
            n_batch: settings.n_batch,
            n_ubatch: settings.n_ubatch,
            max_sequences: settings.max_sequences,
        };
        options
            .validate()
            .map_err(|error| HostError::validation(error.to_string()))?;
        let (model, artifact) = resolved.into_parts();
        let engine = EngineHandle::spawn_resolved(model, artifact, options).map_err(runtime)?;
        let metadata = match warmup(&engine) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = engine.shutdown();
                return Err(error);
            }
        };
        let public_id = public_model_id(&metadata);
        let mut model_aliases = vec!["jev-latest".to_owned()];
        model_aliases.extend(aliases.iter().cloned());
        if settings.model_label != public_id && !model_aliases.contains(&settings.model_label) {
            model_aliases.push(settings.model_label.clone());
        }
        let capabilities = Capabilities {
            kind: ProviderKind::OpenJev,
            model: ModelIdentity {
                id: public_id,
                description: format!(
                    "Local OpenJev {} model served through {}",
                    metadata.quant, metadata.backend
                ),
                // GGUF manifests do not carry a trustworthy publication date.
                release_date: "unknown".to_owned(),
            },
            model_aliases,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
            max_questions: Some(MAX_QUESTIONS),
            max_options: Some(MAX_OPTIONS),
            max_expanded_state_bytes: Some(convert::MAX_EXPANDED_STATE_BYTES),
            confidence_definition: CONFIDENCE_DEFINITION.to_owned(),
            probability_definition: PROBABILITY_STATUS.to_owned(),
            execution_modes: vec![
                "direct".to_owned(),
                if shared_probe.is_ok() {
                    "shared".to_owned()
                } else {
                    "serial".to_owned()
                },
            ],
            device: Some(device_name(settings.device).to_owned()),
            batches_questions: true,
        };
        Ok(Self {
            engine: Some(engine),
            shared_probe,
            capabilities,
            max_sequences: settings.max_sequences,
            require_shared: settings.require_shared,
        })
    }

    fn engine(&self) -> Result<&EngineHandle, HostError> {
        self.engine
            .as_ref()
            .ok_or_else(|| HostError::unavailable("openjev engine is shut down"))
    }

    fn score_serial(
        &self,
        decision: Decision,
        requested_mode: ExecutionMode,
        group_id: Option<&str>,
        fallback: Option<&str>,
    ) -> Result<Readout, HostError> {
        let mut readout = self.engine()?.score_direct(decision).map_err(runtime)?;
        readout.execution.requested_mode = requested_mode;
        readout.execution.effective_mode = match requested_mode {
            ExecutionMode::Direct => ExecutionMode::Direct,
            ExecutionMode::Serial | ExecutionMode::Shared | ExecutionMode::Batch => {
                ExecutionMode::Serial
            }
        };
        readout.execution.fallback_reason =
            if readout.execution.requested_mode == readout.execution.effective_mode {
                None
            } else {
                Some(
                    fallback
                        .unwrap_or("requested execution mode used serial full-prompt fallback")
                        .to_owned(),
                )
            };
        readout.execution.group_id = group_id.map(str::to_owned);
        readout.model.serving_config = Some(
            match readout.execution.effective_mode {
                ExecutionMode::Direct => "llama-direct-v1",
                ExecutionMode::Serial => "llama-serial-full-prompt-v1",
                ExecutionMode::Shared => "llama-state-prefix-parallel-v1",
                ExecutionMode::Batch => "llama-independent-batch-v1",
            }
            .to_owned(),
        );
        finish_readout(readout)
    }

    /// Attempt one native shared group. Any failure discards the tentative
    /// group with a reason so the caller can fall back or refuse.
    fn attempt_shared_group(
        &self,
        group: &[Decision],
        group_id: &str,
    ) -> Result<Vec<Readout>, String> {
        let eligibility = self.shared_probe.as_ref().map_err(Clone::clone)?;
        let state = group
            .first()
            .ok_or_else(|| "shared group must not be empty".to_owned())?
            .state
            .clone();
        let questions = group
            .iter()
            .map(|decision| {
                Question::new(
                    decision.id.clone(),
                    decision.question.clone(),
                    decision.options.clone(),
                )
            })
            .collect::<openjev_core::types::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let raw = self
            .engine()
            .map_err(|error| error.to_string())?
            .score_shared(state, questions, eligibility.clone())
            .map_err(|error| {
                format!("native shared attempt failed; tentative group discarded: {error}")
            })?;
        if raw.len() != group.len() {
            return Err(format!(
                "native shared returned {} rows for {} inputs; tentative group discarded",
                raw.len(),
                group.len()
            ));
        }
        raw.into_iter()
            .zip(group)
            .map(|(mut readout, decision)| {
                if readout.id != decision.id
                    || readout.execution.requested_mode != ExecutionMode::Shared
                    || readout.execution.effective_mode != ExecutionMode::Shared
                    || readout.execution.probe_id.as_deref() != Some(eligibility.probe_id())
                {
                    return Err(
                        "native shared result identity/metadata mismatch; tentative group discarded"
                            .to_owned(),
                    );
                }
                readout.execution.group_id = Some(group_id.to_owned());
                finish_readout(readout)
                    .map_err(|error| format!("native shared adaptation failed: {error}"))
            })
            .collect()
    }
}

fn finish_readout(mut readout: Readout) -> Result<Readout, HostError> {
    readout.confidence = Some(normalized_margin(&readout.probabilities).map_err(core_runtime)?);
    readout.confidence_status = Some(CONFIDENCE_STATUS.to_owned());
    readout.validate().map_err(core_runtime)?;
    Ok(readout)
}

impl DecisionHost for OpenJevHost {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn evaluate(
        &mut self,
        request: &DecisionRequest,
        context: &CallContext,
    ) -> Result<DecisionResponse, HostError> {
        let model = self
            .capabilities
            .resolve_model(request.model.as_deref())?
            .to_owned();
        let mut prepared = convert::prepare(request)?;
        let inference = std::mem::take(&mut prepared.inference);
        if self.require_shared && inference.len() < 2 {
            return Err(HostError::unsupported(format!(
                "request has {} inferential questions; required shared execution needs at least two",
                inference.len()
            )));
        }
        let group_id = format!("systemone-openjev-{}", context.request_id);
        let mut readouts = Vec::with_capacity(inference.len());
        if inference.len() <= 1 {
            for decision in inference {
                context.check()?;
                readouts.push(self.score_serial(decision, ExecutionMode::Direct, None, None)?);
            }
        } else {
            let group_limit = self.max_sequences.saturating_sub(1).max(1) as usize;
            let mut native_failure = None;
            for group in inference.chunks(group_limit) {
                context.check()?;
                match self.attempt_shared_group(group, &group_id) {
                    Ok(mut rows) => readouts.append(&mut rows),
                    Err(reason) => {
                        native_failure = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = native_failure {
                if self.require_shared {
                    return Err(HostError::unsupported(format!(
                        "required shared execution cannot be satisfied: {reason}"
                    )));
                }
                tracing::warn!(
                    reason = %reason,
                    "discarded tentative shared rows; using fresh serial full-prompt fallback"
                );
                readouts.clear();
                for decision in inference {
                    context.check()?;
                    readouts.push(self.score_serial(
                        decision,
                        ExecutionMode::Shared,
                        Some(&group_id),
                        Some(&reason),
                    )?);
                }
            }
        }
        context.check()?;
        let projected = convert::project(prepared, readouts)?;
        let response = DecisionResponse {
            model,
            answers: projected.answers,
            usage: projected.usage,
            diagnostics: projected.diagnostics,
        };
        response.validate(DISTRIBUTION_TOLERANCE)?;
        Ok(response)
    }

    fn shutdown(&mut self) -> Result<(), HostError> {
        if let Some(engine) = self.engine.take() {
            engine.shutdown().map_err(runtime)?;
        }
        Ok(())
    }
}

fn warmup(engine: &EngineHandle) -> Result<ModelMetadata, HostError> {
    let decision = Decision::new(
        WARMUP_ID,
        StateValue::string("systemone resident warmup").map_err(core_validation)?,
        "Select the first option.",
        vec![
            DecisionOption {
                id: "ready".to_owned(),
                description: "Ready".to_owned(),
            },
            DecisionOption {
                id: "not-ready".to_owned(),
                description: "Not ready".to_owned(),
            },
        ],
    )
    .map_err(core_validation)?;
    let readout = engine.score_direct(decision).map_err(runtime)?;
    readout.validate().map_err(core_runtime)?;
    Ok(readout.model)
}

pub(crate) fn public_model_id(model: &ModelMetadata) -> String {
    if model.source == "local" {
        format!("openjev-local-{}", &model.artifact_sha256[..12])
    } else {
        model.id.clone()
    }
}

pub(crate) fn runtime(error: BackendError) -> HostError {
    match error {
        BackendError::Unavailable => HostError::unavailable(error.to_string()),
        BackendError::Worker(_) => HostError::unavailable(error.to_string()),
        BackendError::Configuration(_) | BackendError::UnknownModel(_) => {
            HostError::validation(error.to_string())
        }
        BackendError::OfflineMiss { .. }
        | BackendError::Integrity { .. }
        | BackendError::CallerIntegrity { .. }
        | BackendError::ArtifactChanged { .. }
        | BackendError::ModelLoad(_)
        | BackendError::Download(_) => HostError::unavailable(error.to_string()),
        other => HostError::internal(other.to_string()),
    }
}

fn core_validation(error: openjev_core::OpenJevError) -> HostError {
    HostError::validation(error.to_string())
}

fn core_runtime(error: openjev_core::OpenJevError) -> HostError {
    HostError::internal(format!("{}: {error}", error.code()))
}
