//! The loaded host: one `gliner2_rs::ClassificationPipeline` owned by the
//! service's owner thread. Every question is one encoder pass.

use std::{fs, io::Read, path::Path, time::Instant};

use gliner2_rs::{
    BundleManifest, BundleStatus, ClassificationPipeline, ClassificationScores, OptimizationLevel,
    RuntimeOptions, bundle::BOUNDARY_MODEL_PINS,
};
use sha2::{Digest, Sha256};
use systemone_core::{
    CallContext, Capabilities, DecisionHost, DecisionRequest, DecisionResponse, HostError,
    ModelIdentity, Primitive, ProviderKind,
};

use crate::{
    convert::{self, Route},
    settings::{OptimizationSetting, ResolvedSettings},
};

/// Wire projection rounds to two decimals; upstream checks its softmax sum
/// within `1e-4` already.
const DISTRIBUTION_TOLERANCE: f64 = 1e-4;
/// Files verified against the bundle manifest before loading.
const VERIFIED_FILES: [&str; 4] = [
    "config.json",
    "tokenizer.json",
    "encoder.onnx",
    "classifier.onnx",
];
const MANIFEST: &str = "export_manifest.json";
/// Upstream `read_manifest` caps manifests at 8 MiB; mirror that bound so a
/// corrupt or crafted manifest cannot balloon memory.
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

pub struct Gliner2Host {
    pipeline: Option<ClassificationPipeline>,
    capabilities: Capabilities,
    noul_labels: [String; 2],
    max_len: usize,
}

impl Gliner2Host {
    pub fn load(settings: &ResolvedSettings, aliases: &[String]) -> Result<Self, HostError> {
        let model_dir = settings.model_dir.clone().ok_or_else(|| {
            HostError::unavailable(
                "settings.model_dir is not set; point it at a GLiNER2.5 bundle directory",
            )
        })?;
        let started = Instant::now();
        let identity = if settings.verify_sha256 {
            Some(verify_against_manifest(
                &model_dir,
                &settings.bundle_name(),
            )?)
        } else {
            None
        };
        let options = RuntimeOptions::default()
            .with_intra_threads(settings.intra_threads)
            .with_inter_threads(settings.inter_threads)
            .with_optimization_level(match settings.optimization {
                OptimizationSetting::Disable => OptimizationLevel::Disable,
                OptimizationSetting::Basic => OptimizationLevel::Basic,
                OptimizationSetting::Extended => OptimizationLevel::Extended,
                OptimizationSetting::All => OptimizationLevel::All,
            });
        let pipeline = ClassificationPipeline::from_dir_with_options(&model_dir, options)
            .map_err(|error| HostError::unavailable(format!("{error:#}")))?;
        let report = pipeline.runtime_report();
        tracing::info!(
            profile = %settings.profile,
            model_dir = %model_dir.display(),
            intra_threads = report.intra_threads,
            native_runtime = %report.native_runtime,
            verified = identity.is_some(),
            load_ms = started.elapsed().as_secs_f64() * 1000.0,
            "gliner2 classification pipeline ready"
        );
        let mut model_aliases = vec!["jev-latest".to_owned()];
        model_aliases.extend(aliases.iter().cloned());
        let source = identity.as_ref().map_or_else(
            || "unverified directory".to_owned(),
            |identity| {
                // Verification pins the revision to a 40-char hex commit, but
                // never byte-slice a string that started as external input.
                let revision = identity
                    .hf_revision
                    .get(..12)
                    .unwrap_or(&identity.hf_revision);
                format!("{} @ {revision}", identity.hf_model)
            },
        );
        let capabilities = Capabilities {
            kind: ProviderKind::Gliner2,
            model: ModelIdentity {
                id: settings.model_id.clone(),
                description: format!(
                    "GLiNER2.5 {} classifier ({source}) through gliner2-rs on {}",
                    settings.profile, report.native_runtime
                ),
                release_date: "unknown".to_owned(),
            },
            model_aliases,
            primitives: vec![Primitive::Choice, Primitive::Noul, Primitive::Score],
            max_questions: None,
            max_options: None,
            max_expanded_state_bytes: None,
            confidence_definition: convert::CONFIDENCE_DEFINITION.to_owned(),
            probability_definition: convert::PROBABILITY_STATUS.to_owned(),
            execution_modes: vec!["per-question".to_owned()],
            device: Some("cpu".to_owned()),
            batches_questions: false,
        };
        Ok(Self {
            max_len: pipeline.max_len(),
            pipeline: Some(pipeline),
            capabilities,
            noul_labels: settings.noul_labels.clone(),
        })
    }
}

#[derive(Debug)]
struct BundleIdentity {
    hf_model: String,
    hf_revision: String,
}

/// Check the four classification files against the bundle's manifest. The
/// manifest is the same one `validate_bundle` reads; this checks a subset of
/// its files plus the bundle's pinned identity (model, revision, validated
/// release status) and makes no claim about the rest of the bundle. Unlike
/// upstream `read_manifest` it does not reject duplicate JSON keys; the
/// manifest lives beside the files it describes, so this is a provenance and
/// corruption check, not an authentication boundary.
fn verify_against_manifest(
    model_dir: &Path,
    expected_bundle: &str,
) -> Result<BundleIdentity, HostError> {
    let manifest_path = model_dir.join(MANIFEST);
    let file = fs::File::open(&manifest_path).map_err(|error| {
        HostError::unavailable(format!(
            "cannot read {} ({error}); set settings.verify_sha256 = false to load an unverified directory",
            manifest_path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            HostError::unavailable(format!("cannot read {}: {error}", manifest_path.display()))
        })?;
    if bytes.is_empty() {
        return Err(HostError::unavailable(format!(
            "{} is empty",
            manifest_path.display()
        )));
    }
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(HostError::unavailable(format!(
            "{} exceeds the {MAX_MANIFEST_BYTES} byte manifest limit",
            manifest_path.display()
        )));
    }
    let manifest: BundleManifest = serde_json::from_slice(&bytes).map_err(|error| {
        HostError::unavailable(format!("malformed {}: {error}", manifest_path.display()))
    })?;
    if manifest.manifest_version != 1 {
        return Err(HostError::unavailable(format!(
            "{} has manifest_version {}, expected 1",
            manifest_path.display(),
            manifest.manifest_version
        )));
    }
    if manifest.architecture != "boundary" {
        return Err(HostError::unavailable(format!(
            "{} describes architecture {:?}, expected boundary",
            manifest_path.display(),
            manifest.architecture
        )));
    }
    if manifest.status != BundleStatus::Validated || !manifest.release_ready {
        return Err(HostError::unavailable(format!(
            "{} is not a validated release bundle; set settings.verify_sha256 = false to load it unverified",
            model_dir.display()
        )));
    }
    let pin = BOUNDARY_MODEL_PINS
        .iter()
        .find(|pin| pin.hf_model == manifest.hf_model)
        .ok_or_else(|| {
            HostError::unavailable(format!(
                "{} names unpinned model {:?}; set settings.verify_sha256 = false to load it unverified",
                manifest_path.display(),
                manifest.hf_model
            ))
        })?;
    if manifest.hf_revision != pin.hf_revision {
        return Err(HostError::unavailable(format!(
            "{} records revision {:?} for {}, expected the pinned {}",
            manifest_path.display(),
            manifest.hf_revision,
            pin.hf_model,
            pin.hf_revision
        )));
    }
    let bundle_from_model = manifest.hf_model.rsplit('/').next().unwrap_or_default();
    if bundle_from_model != expected_bundle {
        return Err(HostError::unavailable(format!(
            "{} is {} but settings.profile expects {expected_bundle}",
            model_dir.display(),
            manifest.hf_model
        )));
    }
    for name in VERIFIED_FILES {
        let expected = manifest
            .files
            .get(name)
            .ok_or_else(|| HostError::unavailable(format!("{MANIFEST} has no entry for {name}")))?;
        let path = model_dir.join(name);
        let (bytes, digest) = sha256_file(&path)?;
        if bytes != expected.bytes || digest != expected.sha256 {
            return Err(HostError::unavailable(format!(
                "{} does not match {MANIFEST}: {bytes} bytes sha256 {digest}, expected {} bytes sha256 {}",
                path.display(),
                expected.bytes,
                expected.sha256
            )));
        }
    }
    Ok(BundleIdentity {
        hf_model: manifest.hf_model,
        hf_revision: manifest.hf_revision,
    })
}

fn sha256_file(path: &Path) -> Result<(u64, String), HostError> {
    let mut file = fs::File::open(path).map_err(|error| {
        HostError::unavailable(format!("cannot open {}: {error}", path.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            HostError::unavailable(format!("cannot read {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((total, digest))
}

impl DecisionHost for Gliner2Host {
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
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| HostError::unavailable("gliner2 host is shut down"))?;
        let prepared = convert::prepare(request, &self.noul_labels)?;
        let mut scores: Vec<Option<ClassificationScores>> =
            Vec::with_capacity(prepared.entries.len());
        for entry in &prepared.entries {
            context.check()?;
            scores.push(match &entry.route {
                Route::SingletonChoice(_) => None,
                Route::Inference(request) => Some(
                    pipeline
                        .score_classification(&prepared.text, request)
                        .map_err(|error| convert::map_error(&entry.id, &error))?,
                ),
            });
        }
        let projected = convert::project(&prepared, request, &scores, self.max_len)?;
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
        self.pipeline.take();
        Ok(())
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
