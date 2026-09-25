//! Verified downloads of pinned model files.
//!
//! Every local backend describes its model directory as a
//! [`DownloadPlan`]: pinned URLs with an expected size and SHA-256. This
//! crate is the one place that fetches those files, so they are all
//! verified the same way:
//!
//! - each file streams into `<name>.part` and is hashed while it streams;
//! - a file larger than its pin is refused as soon as it passes the size;
//! - only a file with the pinned size and SHA-256 is renamed into place, so
//!   an interrupted or corrupt download never leaves a file that looks
//!   complete;
//! - a file that already verifies is skipped, and one that does not is
//!   replaced;
//! - redirects must stay on HTTPS and on the Hugging Face Hub or its CDN.
//!
//! There are no retries: a checksum failure is reported, not repeated.

mod progress;

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};
pub use systemone_core::{DownloadPlan, PinnedFile};
use thiserror::Error;

pub use progress::{NoProgress, Progress, TerminalProgress, format_bytes};

const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
/// Hosts a pinned download may be served from or redirected to.
const ALLOWED_HOST_SUFFIXES: [&str; 2] = ["huggingface.co", "hf.co"];

#[derive(Debug, Error)]
pub enum WeightsError {
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {message}")]
    Download { path: String, message: String },
    #[error("{path}: expected {expected} bytes, got {actual}")]
    Size {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("{path}: SHA-256 mismatch (expected {expected}, got {actual})")]
    Checksum {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

impl WeightsError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// State of one pinned file inside a model directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    /// Present with the pinned size; the hash was not read.
    SizeMatches,
    /// Present with the pinned size and SHA-256.
    Verified,
    WrongSize,
    WrongChecksum,
}

impl FileState {
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::SizeMatches | Self::Verified)
    }
}

/// How thoroughly [`inspect`] checks files that are present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Compare sizes only. Fast; used to decide whether to offer a download.
    Size,
    /// Compare sizes and SHA-256.
    Full,
}

/// Inspect a model directory against a plan without changing anything.
pub fn inspect(
    plan: &DownloadPlan,
    directory: &Path,
    check: Check,
) -> Result<Vec<FileState>, WeightsError> {
    plan.files
        .iter()
        .map(|file| file_state(file, &target(directory, file), check))
        .collect()
}

/// Bytes still to download for `plan` into `directory` (size check only).
pub fn missing_bytes(plan: &DownloadPlan, directory: &Path) -> Result<u64, WeightsError> {
    let states = inspect(plan, directory, Check::Size)?;
    Ok(plan
        .files
        .iter()
        .zip(states)
        .filter(|(_, state)| !state.is_usable())
        .map(|(file, _)| file.bytes)
        .sum())
}

/// Space available to this user on the file system that will hold
/// `directory`. Walks up to the nearest existing ancestor, because the
/// directory itself usually does not exist yet.
#[must_use]
pub fn available_space(directory: &Path) -> Option<u64> {
    let mut candidate = Some(directory);
    while let Some(path) = candidate {
        if path.exists() {
            return fs4::available_space(path).ok();
        }
        candidate = path.parent();
    }
    None
}

/// What [`Downloader::download`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadReport {
    pub downloaded_files: usize,
    pub downloaded_bytes: u64,
    /// Files that were already present and verified.
    pub verified_files: usize,
}

pub struct Downloader {
    client: reqwest::blocking::Client,
    /// Test-only: accept `http://127.0.0.1` URLs from a local test server.
    allow_loopback_http: bool,
}

impl Downloader {
    pub fn new() -> Result<Self, WeightsError> {
        Self::build(false)
    }

    fn build(allow_loopback_http: bool) -> Result<Self, WeightsError> {
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            let url = attempt.url().clone();
            let loopback = allow_loopback_http && url.host_str() == Some("127.0.0.1");
            if loopback {
                return attempt.follow();
            }
            if url.scheme() != "https" {
                return attempt.error(format!("refused a redirect to non-HTTPS {url}"));
            }
            if !url.host_str().is_some_and(host_allowed) {
                return attempt.error(format!("refused a redirect to {url}"));
            }
            attempt.follow()
        });
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("systemone/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            // Applies to each read, not the whole body: large files are fine
            // as long as bytes keep arriving.
            .timeout(Duration::from_secs(120))
            .redirect(policy)
            .build()
            .map_err(|error| WeightsError::Invalid(format!("HTTP client: {error}")))?;
        Ok(Self {
            client,
            allow_loopback_http,
        })
    }

    /// Download every file of `plan` that is not already present and
    /// verified, into `directory`.
    pub fn download(
        &self,
        plan: &DownloadPlan,
        directory: &Path,
        progress: &mut dyn Progress,
    ) -> Result<DownloadReport, WeightsError> {
        self.validate(plan)?;
        fs::create_dir_all(directory).map_err(|error| WeightsError::io(directory, error))?;
        progress.begin(&plan.label, plan.total_bytes());
        let mut report = DownloadReport::default();
        for file in &plan.files {
            let path = target(directory, file);
            progress.file(&file.path);
            match file_state(file, &path, Check::Size)? {
                FileState::SizeMatches => {
                    progress.file(&format!("{} (checking)", file.path));
                    if file_state(file, &path, Check::Full)? == FileState::Verified {
                        progress.advance(file.bytes);
                        report.verified_files += 1;
                        continue;
                    }
                }
                FileState::Missing | FileState::WrongSize | FileState::WrongChecksum => {}
                FileState::Verified => unreachable!("a size check never reports Verified"),
            }
            self.fetch(file, &path, progress)?;
            report.downloaded_files += 1;
            report.downloaded_bytes += file.bytes;
        }
        progress.finish();
        Ok(report)
    }

    fn validate(&self, plan: &DownloadPlan) -> Result<(), WeightsError> {
        let result = if self.allow_loopback_http {
            let mut copy = plan.clone();
            for file in &mut copy.files {
                if let Some(rest) = file.url.strip_prefix("http://127.0.0.1") {
                    file.url = format!("https://127.0.0.1{rest}");
                }
            }
            copy.validate()
        } else {
            plan.validate()
        };
        result.map_err(|error| WeightsError::Invalid(error.to_string()))?;
        if !self.allow_loopback_http {
            for file in &plan.files {
                let host = reqwest::Url::parse(&file.url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned));
                if !host.as_deref().is_some_and(host_allowed) {
                    return Err(WeightsError::Invalid(format!(
                        "{}: {} is not a Hugging Face Hub URL",
                        file.path, file.url
                    )));
                }
            }
        }
        Ok(())
    }

    fn fetch(
        &self,
        file: &PinnedFile,
        path: &Path,
        progress: &mut dyn Progress,
    ) -> Result<(), WeightsError> {
        let partial = partial_path(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| WeightsError::io(parent, error))?;
        }
        let result = self.stream(file, &partial, progress);
        if let Err(error) = result {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        fs::rename(&partial, path).map_err(|error| {
            let _ = fs::remove_file(&partial);
            WeightsError::io(path, error)
        })
    }

    fn stream(
        &self,
        file: &PinnedFile,
        partial: &Path,
        progress: &mut dyn Progress,
    ) -> Result<(), WeightsError> {
        let download_error = |message: String| WeightsError::Download {
            path: file.path.clone(),
            message,
        };
        let mut response = self
            .client
            .get(&file.url)
            .send()
            .map_err(|error| download_error(error_chain(&error)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(download_error(format!("HTTP {status} from {}", file.url)));
        }
        if let Some(length) = response.content_length()
            && length != file.bytes
        {
            return Err(WeightsError::Size {
                path: file.path.clone(),
                expected: file.bytes,
                actual: length,
            });
        }
        let mut output = File::create(partial).map_err(|error| WeightsError::io(partial, error))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; CHUNK_BYTES];
        let mut written: u64 = 0;
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|error| download_error(format!("read failed: {error}")))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > file.bytes {
                return Err(WeightsError::Size {
                    path: file.path.clone(),
                    expected: file.bytes,
                    actual: written,
                });
            }
            let chunk = &buffer[..read];
            hasher.update(chunk);
            output
                .write_all(chunk)
                .map_err(|error| WeightsError::io(partial, error))?;
            progress.advance(read as u64);
        }
        if written != file.bytes {
            return Err(WeightsError::Size {
                path: file.path.clone(),
                expected: file.bytes,
                actual: written,
            });
        }
        let actual = hex(&hasher.finalize());
        if actual != file.sha256 {
            return Err(WeightsError::Checksum {
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual,
            });
        }
        output
            .sync_all()
            .map_err(|error| WeightsError::io(partial, error))
    }
}

fn host_allowed(host: &str) -> bool {
    ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn target(directory: &Path, file: &PinnedFile) -> PathBuf {
    file.path
        .split('/')
        .fold(directory.to_path_buf(), |path, part| path.join(part))
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

fn file_state(file: &PinnedFile, path: &Path, check: Check) -> Result<FileState, WeightsError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(FileState::Missing),
        Err(error) => return Err(WeightsError::io(path, error)),
    };
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Ok(FileState::WrongSize);
    }
    if check == Check::Size {
        return Ok(FileState::SizeMatches);
    }
    Ok(if sha256_file(path)? == file.sha256 {
        FileState::Verified
    } else {
        FileState::WrongChecksum
    })
}

/// SHA-256 of a file, as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, WeightsError> {
    let mut input = File::open(path).map_err(|error| WeightsError::io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| WeightsError::io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}

#[cfg(test)]
mod tests;
