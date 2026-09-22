//! Shared hosted HTTP transport used by passthrough adapters.
//!
//! Fixed policy, shared by every hosted kind:
//!
//! - one send per request: no retries, no provider fallback, so a failed
//!   call is never billed twice;
//! - redirects are refused (a 3xx is surfaced to the caller instead of
//!   silently following it);
//! - no client headers are forwarded upstream, and no `x-systemone-*`
//!   value ever leaves the service;
//! - response bodies are read through a hard byte cap.
//!
//! The client itself holds no credentials; bearers are attached per request.
//! Base URLs must be HTTPS, except plain http on loopback for tests.

use std::io::Read;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::AUTHORIZATION;
use reqwest::{Method, Url};
use systemone_core::HostError;

/// Upper bound on a single upstream response body (8 MiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A single completed upstream exchange. Non-2xx statuses are returned as
/// replies; interpreting them is the adapter's job.
pub struct RemoteReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why an upstream call failed before a response completed.
#[derive(Debug)]
pub enum RemoteError {
    /// DNS, connect, TLS, or transport failure.
    Unreachable(String),
    /// The per-request deadline expired before the reply completed.
    Timeout,
    /// The response body exceeded the byte cap.
    TooLarge(usize),
}

/// Blocking HTTP client for hosted passthroughs. Holds no secrets.
pub struct RemoteTransport {
    client: Client,
}

impl RemoteTransport {
    /// Build the shared client. Construction does no I/O.
    pub fn new() -> Result<Self, HostError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("systemone/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                HostError::internal(format!("remote HTTP client construction: {error}"))
            })?;
        Ok(Self { client })
    }

    /// Base URLs must use HTTPS. Plain http is accepted only for loopback
    /// hosts so tests can run against a local mock server.
    pub fn validate_base_url(base: &Url) -> Result<(), HostError> {
        let loopback = matches!(
            base.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
        );
        if base.scheme() == "https" || base.scheme() == "http" && loopback {
            Ok(())
        } else {
            Err(HostError::validation(format!(
                "upstream base URL must use HTTPS; {base} does not"
            )))
        }
    }

    /// Send exactly one request and read the response body through
    /// `max_response_bytes`. `timeout` bounds the whole exchange, including
    /// the body read.
    pub fn send(
        &self,
        method: Method,
        url: &Url,
        bearer: Option<&str>,
        body: Option<&serde_json::Value>,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<RemoteReply, RemoteError> {
        let mut request = self.client.request(method, url.clone());
        if let Some(bearer) = bearer {
            request = request.header(AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let mut response = request.timeout(timeout).send().map_err(classify)?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = response.read(&mut chunk).map_err(io_classify)?;
            if read == 0 {
                break;
            }
            if body.len() + read > max_response_bytes {
                return Err(RemoteError::TooLarge(max_response_bytes));
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Ok(RemoteReply { status, body })
    }
}

fn classify(error: reqwest::Error) -> RemoteError {
    if error.is_timeout() {
        RemoteError::Timeout
    } else {
        RemoteError::Unreachable(error.to_string())
    }
}

fn io_classify(error: std::io::Error) -> RemoteError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        RemoteError::Timeout
    } else {
        RemoteError::Unreachable(error.to_string())
    }
}
