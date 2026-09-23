//! The loaded host: one `kev_core::Runtime` owned by a dedicated worker
//! thread. The runtime's MLX backend holds compiled-graph handles that are
//! deliberately not `Send`, so the runtime is constructed on the worker and
//! never leaves it; the host talks to it over channels and stays `Send` as
//! `DecisionHost` requires.

use std::sync::mpsc;
use std::time::Instant;

use kev_core::runtime::{Device, LoadOptions, Runtime};
use systemone_core::{
    CallContext, Capabilities, DecisionHost, DecisionRequest, DecisionResponse, HostError,
    ModelIdentity, Primitive, ProviderKind,
};

use crate::{
    convert,
    settings::{DeviceSetting, ResolvedSettings, device_name},
};

/// Wire projection rounds to two decimals; the runtime returns fp32-derived
/// softmax rows that sum to one within float precision.
const DISTRIBUTION_TOLERANCE: f64 = 1e-4;

struct EvaluationResult {
    probs: Vec<Vec<f64>>,
    input_tokens: usize,
    output_tokens: usize,
    prefix_cache_hit: bool,
}

enum Job {
    Evaluate(
        kev_core::SystemOneRequest,
        mpsc::Sender<Result<EvaluationResult, HostError>>,
    ),
    Shutdown,
}

struct Ready {
    backend_name: &'static str,
    temperature: f32,
    hybrid: bool,
}

pub struct KevHost {
    jobs: Option<mpsc::Sender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
    capabilities: Capabilities,
    backend_name: &'static str,
}

impl KevHost {
    pub fn load(settings: &ResolvedSettings, aliases: &[String]) -> Result<Self, HostError> {
        let model_dir = settings.model_dir.clone().ok_or_else(|| {
            HostError::unavailable(
                "settings.model_dir is not set; point it at a directory holding the assembled checkpoint (base/, adapter/, head.safetensors, head.meta.json)",
            )
        })?;
        let device = match settings.device {
            DeviceSetting::Cpu => Device::Cpu,
            DeviceSetting::Metal => Device::Metal,
        };
        let options = LoadOptions {
            model_dir,
            device,
            temperature: None,
        };

        let (jobs, job_receiver) = mpsc::channel::<Job>();
        let (ready_sender, ready_receiver) = mpsc::channel::<Result<Ready, HostError>>();
        let started = Instant::now();
        let worker = std::thread::Builder::new()
            .name("kev-runtime".to_owned())
            .spawn(move || {
                let mut runtime = match Runtime::load(&options).map_err(convert::map_error) {
                    Ok(runtime) => {
                        let ready = Ready {
                            backend_name: runtime.backend_name,
                            temperature: runtime.head.temperature,
                            hybrid: runtime.hybrid,
                        };
                        // A dropped receiver means the caller gave up; exit.
                        if ready_sender.send(Ok(ready)).is_err() {
                            return;
                        }
                        runtime
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = job_receiver.recv() {
                    match job {
                        Job::Shutdown => break,
                        Job::Evaluate(request, reply) => {
                            let result = runtime
                                .evaluate(&request)
                                .map(|evaluation| EvaluationResult {
                                    probs: evaluation.probs,
                                    input_tokens: evaluation.input_tokens,
                                    output_tokens: evaluation.output_tokens,
                                    prefix_cache_hit: evaluation.prefix_cache_hit,
                                })
                                .map_err(convert::map_error);
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|error| HostError::internal(format!("spawn kev worker: {error}")))?;

        let ready = ready_receiver
            .recv()
            .map_err(|_| HostError::internal("kev worker exited before reporting readiness"))?;
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                let _ = worker.join();
                return Err(error);
            }
        };
        tracing::info!(
            backend = ready.backend_name,
            hybrid = ready.hybrid,
            temperature = ready.temperature,
            load_ms = started.elapsed().as_secs_f64() * 1000.0,
            "kev runtime ready"
        );

        let mut model_aliases = vec!["jev-latest".to_owned(), "kev-latest".to_owned()];
        model_aliases.retain(|alias| alias != &settings.model_id);
        for alias in aliases {
            if !model_aliases.contains(alias) && alias != &settings.model_id {
                model_aliases.push(alias.clone());
            }
        }
        let capabilities = Capabilities {
            kind: ProviderKind::Kev,
            model: ModelIdentity {
                id: settings.model_id.clone(),
                description: format!(
                    "Kev decision model ({} base, temperature {:.3}) served through kev-core on {}",
                    if ready.hybrid { "Qwen3.5 hybrid" } else { "Qwen3 attention-only" },
                    ready.temperature,
                    ready.backend_name,
                ),
                release_date: "unknown".to_owned(),
            },
            model_aliases,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
            max_questions: None,
            // Upstream kev caps options per question at 255; state and
            // branch token budgets are enforced by the runtime.
            max_options: Some(255),
            max_expanded_state_bytes: None,
            confidence_definition: convert::CONFIDENCE_DEFINITION.to_owned(),
            probability_definition: convert::PROBABILITY_STATUS.to_owned(),
            execution_modes: vec!["batched".to_owned()],
            device: Some(device_name(settings.device).to_owned()),
            batches_questions: true,
        };
        Ok(Self {
            jobs: Some(jobs),
            worker: Some(worker),
            capabilities,
            backend_name: ready.backend_name,
        })
    }
}

impl DecisionHost for KevHost {
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
        let jobs = self
            .jobs
            .as_ref()
            .ok_or_else(|| HostError::unavailable("kev host is shut down"))?;
        let kev_request = convert::to_kev(request)?;
        context.check()?;
        let (reply_sender, reply_receiver) = mpsc::channel();
        jobs.send(Job::Evaluate(kev_request, reply_sender))
            .map_err(|_| HostError::unavailable("kev worker is gone"))?;
        let evaluation = reply_receiver
            .recv()
            .map_err(|_| HostError::internal("kev worker dropped the request"))??;
        context.check()?;
        let projected = convert::project(
            request,
            &convert::EvaluationView {
                probs: &evaluation.probs,
                input_tokens: evaluation.input_tokens,
                output_tokens: evaluation.output_tokens,
                prefix_cache_hit: evaluation.prefix_cache_hit,
            },
            self.backend_name,
        )?;
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
        if let Some(jobs) = self.jobs.take() {
            let _ = jobs.send(Job::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }
}

impl Drop for KevHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
