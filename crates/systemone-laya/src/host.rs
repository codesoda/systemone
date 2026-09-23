//! The loaded host: one `laya_core::Runtime` owned by the service's owner
//! thread. Every request is one batched forward pass.

use std::time::Instant;

use laya_core::{BackendSpec, LoadOptions, Precision, Profile, Runtime, Verification};
use systemone_core::{
    CallContext, Capabilities, DecisionHost, DecisionRequest, DecisionResponse, HostError,
    ModelIdentity, Primitive, ProviderKind,
};

use crate::{
    convert,
    settings::{DeviceSetting, PrecisionSetting, ResolvedSettings, device_name},
};

/// Wire projection rounds to two decimals; the runtime returns f32 softmax
/// rows that sum to one within float precision.
const DISTRIBUTION_TOLERANCE: f64 = 1e-4;

pub struct LayaHost {
    runtime: Option<Runtime>,
    capabilities: Capabilities,
}

impl LayaHost {
    pub fn load(settings: &ResolvedSettings, aliases: &[String]) -> Result<Self, HostError> {
        let model_dir = settings.model_dir.clone().ok_or_else(|| {
            HostError::unavailable(
                "settings.model_dir is not set; point it at a directory holding the pinned profile files",
            )
        })?;
        let profile: Profile = settings.profile.parse().map_err(convert::map_error)?;
        let backend = match settings.device {
            DeviceSetting::Cpu => BackendSpec::CandleCpu,
            DeviceSetting::Metal => BackendSpec::Mlx {
                precision: match settings.precision {
                    PrecisionSetting::F32 => Precision::F32,
                    PrecisionSetting::F16 => Precision::F16,
                },
                metallib_cache_dir: settings.cache_dir.clone(),
            },
        };
        let options = LoadOptions {
            profile,
            model_dir,
            backend,
            verification: if settings.verify_sha256 {
                Verification::Full
            } else {
                Verification::SizeOnly
            },
        };
        let started = Instant::now();
        let mut runtime = Runtime::load(&options).map_err(convert::map_error)?;
        let warmup_ms = runtime.warmup().map_err(convert::map_error)?;
        let info = runtime.info().clone();
        tracing::info!(
            profile = %info.profile,
            backend = %info.backend,
            weights_sha256 = %info.weights_sha256,
            load_ms = info.load_ms,
            warmup_ms,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            "laya runtime ready"
        );
        let mut model_aliases = vec!["jev-latest".to_owned()];
        model_aliases.extend(aliases.iter().cloned());
        let capabilities = Capabilities {
            kind: ProviderKind::Laya,
            model: ModelIdentity {
                id: settings.model_id.clone(),
                description: format!(
                    "Laya {} profile ({}/{} weights {}) served through laya-core on {}",
                    info.profile,
                    laya_core::HUB_REPO,
                    &laya_core::HUB_REVISION[..12],
                    &info.weights_sha256[..12],
                    info.backend
                ),
                release_date: "unknown".to_owned(),
            },
            model_aliases,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
            max_questions: None,
            // Options are bounded by the token budget, not by a count; a
            // request whose markers do not fit `max_len` is a validation
            // error from the runtime.
            max_options: None,
            max_expanded_state_bytes: None,
            confidence_definition: convert::CONFIDENCE_DEFINITION.to_owned(),
            probability_definition: convert::PROBABILITY_STATUS.to_owned(),
            execution_modes: vec!["batched".to_owned()],
            device: Some(device_name(settings.device).to_owned()),
            batches_questions: true,
        };
        Ok(Self {
            runtime: Some(runtime),
            capabilities,
        })
    }
}

impl DecisionHost for LayaHost {
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
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| HostError::unavailable("laya host is shut down"))?;
        let prepared = convert::prepare(request)?;
        context.check()?;
        let evaluation = match &prepared.request {
            Some(laya_request) => Some(runtime.evaluate(laya_request).map_err(convert::map_error)?),
            None => None,
        };
        context.check()?;
        let backend = runtime.info().backend.clone();
        let projected = convert::project(&prepared, request, evaluation.as_ref(), &backend)?;
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
        self.runtime.take();
        Ok(())
    }
}
