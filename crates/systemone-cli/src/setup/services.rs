//! The real side effects behind [`super::Services`].

use std::{
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use systemone_config::{BackendConfig, Config, toml};
use systemone_core::{
    Answer, Backend, BackendId, CallContext, DecisionRequest, DownloadPlan, ModelStatus,
    NoulQuestion, ProviderKind, Question,
};
use systemone_weights::{Downloader, Progress, TerminalProgress};

use super::{Services, TestResult};
use crate::{CliError, backends};

pub struct RealServices;

fn openjev_backend(device: &str) -> Result<Arc<dyn Backend>, CliError> {
    let mut settings = toml::Table::new();
    settings.insert("device".to_owned(), toml::Value::String(device.to_owned()));
    let config = BackendConfig {
        kind: ProviderKind::OpenJev,
        enabled: true,
        model: None,
        aliases: Vec::new(),
        queue_capacity: 1,
        max_in_flight: 1,
        settings,
    };
    Ok(backends::build(&BackendId::new("setup")?, &config)?)
}

impl Services for RealServices {
    fn env_is_set(&self, name: &str) -> bool {
        std::env::var_os(name).is_some_and(|value| !value.is_empty())
    }

    fn available_space(&self, directory: &Path) -> Option<u64> {
        systemone_weights::available_space(directory)
    }

    fn openjev_models(&mut self, device: &str) -> Result<Vec<ModelStatus>, CliError> {
        let backend = openjev_backend(device)?;
        Ok(backend.model_store().require("model store")?.list()?)
    }

    fn pull_openjev(&mut self, device: &str, model: &ModelStatus) -> Result<(), CliError> {
        let backend = openjev_backend(device)?;
        // The OpenJev store downloads into its own verified cache and
        // reports nothing while it runs, so progress is the growth of the
        // cache directory on disk: close to the bytes received.
        let cache = backend
            .describe()
            .settings
            .get("cache_dir")
            .and_then(serde_json::Value::as_str)
            .map(std::path::PathBuf::from);
        let start = cache.as_deref().map_or(0, directory_bytes);
        let (done, finished) = mpsc::channel();
        let id = model.id.clone();
        let worker = thread::spawn(move || {
            let result = backend
                .model_store()
                .require("model store")
                .and_then(|store| store.pull(&id, false));
            let _ = done.send(());
            result
        });
        let mut progress = TerminalProgress::new(std::io::stderr());
        progress.begin(&model.id, model.bytes);
        progress.file(&model.file);
        let mut reported = 0_u64;
        while finished.recv_timeout(Duration::from_millis(250)).is_err() {
            if let Some(cache) = &cache {
                let grown = directory_bytes(cache)
                    .saturating_sub(start)
                    .min(model.bytes);
                progress.advance(grown.saturating_sub(reported));
                reported = reported.max(grown);
            }
        }
        let result = worker
            .join()
            .map_err(|_| CliError::runtime("download_failed", "the download thread panicked"))?;
        if result.is_ok() {
            progress.advance(model.bytes.saturating_sub(reported));
        }
        progress.finish();
        result?;
        Ok(())
    }

    fn download(&mut self, plan: &DownloadPlan, directory: &Path) -> Result<(), CliError> {
        let downloader = Downloader::new()
            .map_err(|error| CliError::runtime("download_failed", error.to_string()))?;
        let mut progress = TerminalProgress::new(std::io::stderr());
        downloader
            .download(plan, directory, &mut progress)
            .map(|_| ())
            .map_err(|error| {
                // End the progress line before the error is printed.
                eprintln!();
                CliError::runtime("download_failed", error.to_string())
            })
    }

    fn test_decision(&mut self, config: &Config, backend: &str) -> Result<TestResult, CliError> {
        let instance = backends::configure_one(config, Some(backend))?;
        let backend = instance.backend?;
        let started = Instant::now();
        let mut host = backend.load()?;
        let request = DecisionRequest::new(
            None,
            serde_json::Value::String(
                "Order 1142: the customer's card was charged twice for the same invoice."
                    .to_owned(),
            ),
            vec![(
                "charged-twice".to_owned(),
                Question::Noul(NoulQuestion {
                    instructions: Some(serde_json::Value::String(
                        "Was the customer charged more than once?".to_owned(),
                    )),
                    true_description: None,
                    false_description: None,
                }),
            )],
        )?;
        let result = host.capabilities().check(&request).and_then(|()| {
            let context = CallContext::new(
                "s1-setup-test",
                Some(Instant::now() + Duration::from_secs(config.server.request_timeout_secs)),
            );
            host.evaluate(&request, &context)
        });
        let shutdown = host.shutdown();
        let response = result?;
        shutdown?;
        let probability_true = response
            .answers
            .iter()
            .find_map(|(_, answer)| match answer {
                Answer::Noul(noul) => Some(noul.probability_true),
                _ => None,
            })
            .ok_or_else(|| {
                CliError::runtime("test_failed", "the backend returned no yes/no answer")
            })?;
        Ok(TestResult {
            model: response.model,
            probability_true,
            seconds: started.elapsed().as_secs_f64(),
        })
    }
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}
