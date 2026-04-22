//! HTTP client for CRM calls through a relay server.

use std::sync::OnceLock;
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use thiserror::Error;

/// Characters allowed unencoded in URL path segments — matches Python's
/// ``urllib.parse.quote(s, safe='')`` so a `/` in a CRM route name is
/// percent-encoded as `%2F` rather than splitting the path.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

// ── Shared tokio runtime ────────────────────────────────────────────────

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("c2-http-io")
            .enable_all()
            .build()
            .expect("failed to create c2-http runtime")
    })
}

// ── Error type ──────────────────────────────────────────────────────────

/// Errors returned by [`HttpClient`] operations.
#[derive(Debug, Error)]
pub enum HttpError {
    /// HTTP 500 — body contains serialized CCError bytes.
    #[error("CRM method error")]
    CrmError(Vec<u8>),

    /// Network / connection errors.
    #[error("HTTP transport error: {0}")]
    Transport(String),

    /// Non-200, non-500 status codes.
    #[error("HTTP {0}: {1}")]
    ServerError(u16, String),
}

// ── HttpClient ──────────────────────────────────────────────────────────

/// Synchronous + async HTTP client for CRM relay calls.
///
/// Uses `reqwest::Client` internally with connection pooling.
pub struct HttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl HttpClient {
    /// Create a new client targeting `base_url`.
    pub fn new(
        base_url: &str,
        timeout_secs: f64,
        max_connections: usize,
    ) -> Result<Self, HttpError> {
        let client = crate::relay_client_builder()
            .timeout(Duration::from_secs_f64(timeout_secs))
            .pool_max_idle_per_host(max_connections)
            .build()
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Blocking CRM call (runs on the shared tokio runtime).
    pub fn call(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, HttpError> {
        let handle = runtime().handle();
        handle.block_on(self.call_async(route_name, method_name, data))
    }

    /// Async CRM call.
    pub async fn call_async(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, HttpError> {
        let url = format!(
            "{}/{}/{}",
            self.base_url,
            encode_segment(route_name),
            encode_segment(method_name),
        );
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        match resp.status().as_u16() {
            200 => {
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| HttpError::Transport(e.to_string()))?;
                Ok(bytes.to_vec())
            }
            500 => {
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| HttpError::Transport(e.to_string()))?;
                Err(HttpError::CrmError(body.to_vec()))
            }
            code => {
                let text = resp.text().await.unwrap_or_default();
                Err(HttpError::ServerError(code, text))
            }
        }
    }

    /// Health check — GET /health.
    pub fn health(&self) -> Result<bool, HttpError> {
        let handle = runtime().handle();
        handle.block_on(async {
            let url = format!("{}/health", self.base_url);
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| HttpError::Transport(e.to_string()))?;
            Ok(resp.status().as_u16() == 200)
        })
    }

    /// Base URL of this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
