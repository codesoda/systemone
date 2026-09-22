//! Shared/batch execution probe receipts.
//!
//! OpenJev only enables native shared-prefix or independent-batch execution
//! for an exact model/build/configuration after a probe run proves the
//! outputs match direct scoring within frozen tolerances. The probe runs in a
//! child process (`s1 openjev probe`) so a llama.cpp crash cannot take the
//! parent down; the parent publishes the receipt. This module owns the
//! library side of that flow; the CLI owns process orchestration.

use openjev_core::{Device, PromptProfile};
use openjev_llama::{NATIVE_PIN, PROBE_SUITE_VERSION, ProbeConfiguration};
use serde::{Deserialize, Serialize};

use crate::settings::ResolvedSettings;

pub fn probe_configuration(
    settings: &ResolvedSettings,
    artifact_sha256: &str,
    profile: PromptProfile,
) -> ProbeConfiguration {
    ProbeConfiguration {
        artifact_sha256: artifact_sha256.to_owned(),
        native_pin: NATIVE_PIN.to_owned(),
        probe_suite_version: PROBE_SUITE_VERSION.to_owned(),
        device: settings.device,
        gpu_layers_requested: settings.gpu_layers,
        offload_kqv: settings.device != Device::Cpu,
        op_offload: settings.device != Device::Cpu,
        threads: settings.threads,
        n_ctx: settings.n_ctx,
        max_tokens: settings.max_tokens,
        max_context_tokens: settings.max_context_tokens,
        n_batch: settings.n_batch,
        n_ubatch: settings.n_ubatch,
        n_seq_max: settings.max_sequences,
        kv_unified: true,
        profile,
    }
}

/// Report emitted by the probe child and re-emitted by the parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    pub schema: String,
    pub process_status: String,
    pub receipt: Option<openjev_llama::ProbeReceipt>,
    pub receipt_path: Option<String>,
    pub enabled: bool,
    pub failure_reason: Option<String>,
}

pub const PROBE_REPORT_SCHEMA: &str = "systemone-openjev-probe-report-v1";

#[cfg(feature = "native")]
pub use native::{prepare_publication, run_probe};

#[cfg(feature = "native")]
mod native {
    use openjev_core::{Decision, DecisionOption, Question, StateValue};
    use openjev_llama::{
        CacheOptions, EngineHandle, EngineOptions, MAX_ABS_SLOT_LOGIT, MAX_PROBABILITY_DELTA,
        ModelCache, ModelRegistry, ProbeCaseResult, ProbeCaseStatus, ProbeMode, ProbePublication,
        ProbeReceipt, begin_probe_publication, probe_id,
    };
    use systemone_core::HostError;

    use super::probe_configuration;
    use crate::{host::runtime, settings::ResolvedSettings};

    struct OwnedProbeCase {
        id: &'static str,
        decisions: Vec<Decision>,
        repeats: usize,
        require_over_512: bool,
    }

    /// Parent side: establish the exact probe identity and suspend any prior
    /// passing receipt before a child initializes llama.cpp. Resolves and
    /// verifies the artifact only; loads no model.
    pub fn prepare_publication(
        settings: &ResolvedSettings,
        mode: ProbeMode,
    ) -> Result<ProbePublication, HostError> {
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
        let expected = probe_configuration(
            settings,
            resolved.model().artifact_sha256(),
            resolved.model().profile(),
        );
        begin_probe_publication(&cache, resolved.model().id(), mode, &expected).map_err(runtime)
    }

    /// Child side: load the engine, run the owned probe suite and produce a
    /// receipt. The parent decides whether to publish it.
    pub fn run_probe(
        settings: &ResolvedSettings,
        mode: ProbeMode,
    ) -> Result<ProbeReceipt, HostError> {
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
        let model_id = resolved.model().id().to_owned();
        let configuration = probe_configuration(
            settings,
            resolved.model().artifact_sha256(),
            resolved.model().profile(),
        );
        let expected_probe_id = probe_id(mode, &configuration).map_err(runtime)?;
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
        let (model, artifact) = resolved.into_parts();
        let engine = EngineHandle::spawn_resolved(model, artifact, options).map_err(runtime)?;
        let cases = probe_cases(settings, mode)?;
        let mut results = Vec::with_capacity(cases.len());
        let mut decisive_failure = None;
        for case in cases {
            if let Some(reason) = &decisive_failure {
                results.push(ProbeCaseResult {
                    id: case.id.to_owned(),
                    status: ProbeCaseStatus::UnrunAfterDecisiveFailure,
                    rows: 0,
                    max_abs_slot_logit: None,
                    max_probability_delta: None,
                    same_first_argmax: None,
                    detail: Some(format!("not run after decisive failure: {reason}")),
                });
                continue;
            }
            match run_one_case(&engine, mode, &expected_probe_id, &case) {
                Ok(result) if result.status == ProbeCaseStatus::Passed => results.push(result),
                Ok(result) => {
                    decisive_failure = result.detail.clone();
                    results.push(result);
                }
                Err(error) => {
                    let reason = error.to_string();
                    decisive_failure = Some(reason.clone());
                    results.push(ProbeCaseResult {
                        id: case.id.to_owned(),
                        status: ProbeCaseStatus::Failed,
                        rows: 0,
                        max_abs_slot_logit: None,
                        max_probability_delta: None,
                        same_first_argmax: None,
                        detail: Some(reason),
                    });
                }
            }
        }
        let shutdown_error = engine.shutdown().err().map(|error| error.to_string());
        let failure_reason = shutdown_error.or(decisive_failure);
        ProbeReceipt::new(model_id, mode, configuration, results, failure_reason).map_err(runtime)
    }

    fn run_one_case(
        engine: &EngineHandle,
        mode: ProbeMode,
        probe_id: &str,
        case: &OwnedProbeCase,
    ) -> Result<ProbeCaseResult, HostError> {
        let mut baseline = Vec::with_capacity(case.decisions.len());
        for decision in &case.decisions {
            baseline.push(engine.score_direct(decision.clone()).map_err(runtime)?);
        }
        let rows = u32::try_from(case.decisions.len()).unwrap_or(u32::MAX);
        if case.require_over_512 && baseline.iter().all(|row| row.input_tokens <= 512) {
            return Ok(ProbeCaseResult {
                id: case.id.to_owned(),
                status: ProbeCaseStatus::Failed,
                rows,
                max_abs_slot_logit: None,
                max_probability_delta: None,
                same_first_argmax: None,
                detail: Some(
                    "owned long case did not produce a full prompt over 512 tokens".to_owned(),
                ),
            });
        }
        let mut max_logit = 0.0_f64;
        let mut max_probability = 0.0_f64;
        let mut same_argmax = true;
        for _ in 0..case.repeats {
            let candidate = match mode {
                ProbeMode::Shared => {
                    let state = case.decisions[0].state.clone();
                    if case.decisions.iter().any(|row| row.state != state) {
                        return Err(HostError::validation(
                            "owned shared probe case contains nonidentical states",
                        ));
                    }
                    let questions = case
                        .decisions
                        .iter()
                        .map(|row| {
                            Question::new(row.id.clone(), row.question.clone(), row.options.clone())
                        })
                        .collect::<openjev_core::types::Result<Vec<_>>>()
                        .map_err(|error| HostError::validation(error.to_string()))?;
                    engine
                        .probe_shared_candidate(state, questions, probe_id.to_owned())
                        .map_err(runtime)?
                }
                ProbeMode::Batch => engine
                    .probe_batch_candidate(case.decisions.clone(), probe_id.to_owned())
                    .map_err(runtime)?,
            };
            if candidate.len() != baseline.len()
                || candidate
                    .iter()
                    .zip(&baseline)
                    .any(|(actual, direct)| actual.id != direct.id)
            {
                return Ok(ProbeCaseResult {
                    id: case.id.to_owned(),
                    status: ProbeCaseStatus::Failed,
                    rows,
                    max_abs_slot_logit: None,
                    max_probability_delta: None,
                    same_first_argmax: Some(false),
                    detail: Some(
                        "output order or identity differs from direct baseline".to_owned(),
                    ),
                });
            }
            for (actual, direct) in candidate.iter().zip(&baseline) {
                if actual.option_logits.len() != direct.option_logits.len()
                    || actual.probabilities.len() != direct.probabilities.len()
                {
                    return Err(HostError::internal(
                        "candidate/direct vector lengths differ",
                    ));
                }
                for (left, right) in actual.option_logits.iter().zip(&direct.option_logits) {
                    max_logit = max_logit.max((left - right).abs());
                }
                for (left, right) in actual.probabilities.iter().zip(&direct.probabilities) {
                    max_probability = max_probability.max((left - right).abs());
                }
                same_argmax &= actual.choice_index == direct.choice_index;
            }
        }
        let passed = max_logit <= MAX_ABS_SLOT_LOGIT
            && max_probability <= MAX_PROBABILITY_DELTA
            && same_argmax;
        Ok(ProbeCaseResult {
            id: case.id.to_owned(),
            status: if passed {
                ProbeCaseStatus::Passed
            } else {
                ProbeCaseStatus::Failed
            },
            rows,
            max_abs_slot_logit: Some(max_logit),
            max_probability_delta: Some(max_probability),
            same_first_argmax: Some(same_argmax),
            detail: (!passed).then(|| {
                format!(
                    "frozen tolerance failure: max_abs_slot_logit={max_logit}, max_probability_delta={max_probability}, same_first_argmax={same_argmax}"
                )
            }),
        })
    }

    fn probe_cases(
        settings: &ResolvedSettings,
        mode: ProbeMode,
    ) -> Result<Vec<OwnedProbeCase>, HostError> {
        let binary = probe_options(2);
        let ternary = probe_options(3);
        let sixteen = probe_options(16);
        let short = state("probe short state")?;
        let changed = state("probe changed state with independent contents")?;
        let long_state = state(format!(
            "long owned evidence {}",
            "evidence-segment ".repeat(700)
        ))?;
        let suffix_words = usize::try_from(settings.n_batch)
            .unwrap_or(512)
            .saturating_add(160)
            .min(2_000);
        let long_question = format!(
            "Evaluate this deliberately long ragged criterion: {}",
            "criterion-segment ".repeat(suffix_words)
        );
        let one = vec![decision(
            "binary-1",
            short.clone(),
            "short binary",
            binary.clone(),
        )?];
        let ragged = vec![
            decision(
                "ragged-1",
                short.clone(),
                "short three-way",
                ternary.clone(),
            )?,
            decision("ragged-2", short.clone(), &long_question, ternary)?,
        ];
        let many = (1..=21)
            .map(|index| {
                let row_state = if mode == ProbeMode::Batch {
                    state(format!(
                        "{} independent-batch-state-{index}",
                        long_state.as_value().as_str().unwrap_or("owned long state")
                    ))?
                } else {
                    long_state.clone()
                };
                decision(
                    &format!("wide-{index:02}"),
                    row_state,
                    &format!(
                        "sixteen-way criterion {index} {}",
                        "ragged ".repeat(index % 7)
                    ),
                    sixteen.clone(),
                )
            })
            .collect::<Result<Vec<_>, HostError>>()?;
        let changed_case = vec![
            decision(
                "changed-1",
                changed.clone(),
                "changed state first",
                binary.clone(),
            )?,
            decision(
                "changed-2",
                if mode == ProbeMode::Batch {
                    state("a second distinct independent batch state")?
                } else {
                    changed
                },
                "changed state second",
                binary.clone(),
            )?,
        ];
        let cycles = (1..=settings.max_sequences.saturating_add(1))
            .map(|index| {
                let row_state = if mode == ProbeMode::Batch {
                    state(format!("independent cycle state {index}"))?
                } else {
                    short.clone()
                };
                decision(
                    &format!("cycle-{index:02}"),
                    row_state,
                    &format!("copy clear cycle {index}"),
                    binary.clone(),
                )
            })
            .collect::<Result<Vec<_>, HostError>>()?;
        Ok(vec![
            OwnedProbeCase {
                id: "binary-short-1-branch",
                decisions: one,
                repeats: 1,
                require_over_512: false,
            },
            OwnedProbeCase {
                id: "three-way-ragged-2-branches-multichunk",
                decisions: ragged,
                repeats: 1,
                require_over_512: true,
            },
            OwnedProbeCase {
                id: "sixteen-way-long-state-21-branches",
                decisions: many,
                repeats: 1,
                require_over_512: true,
            },
            OwnedProbeCase {
                id: "changed-state-isolation",
                decisions: changed_case,
                repeats: 1,
                require_over_512: false,
            },
            OwnedProbeCase {
                id: "repeated-copy-clear-cycles",
                decisions: cycles,
                repeats: 3,
                require_over_512: false,
            },
        ])
    }

    fn state(text: impl Into<String>) -> Result<StateValue, HostError> {
        StateValue::string(text).map_err(|error| HostError::validation(error.to_string()))
    }

    fn probe_options(count: usize) -> Vec<DecisionOption> {
        (1..=count)
            .map(|index| DecisionOption {
                id: format!("option-{index}"),
                description: format!("Owned deterministic option {index}"),
            })
            .collect()
    }

    fn decision(
        id: &str,
        state: StateValue,
        question: &str,
        options: Vec<DecisionOption>,
    ) -> Result<Decision, HostError> {
        Decision::new(id, state, question, options)
            .map_err(|error| HostError::validation(error.to_string()))
    }
}
