//! HTTP client for CRM calls through a relay server.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::CONTENT_LENGTH;
use thiserror::Error;

use super::{HttpCallControl, HttpCallPhase};
use c2_contract::ExpectedRouteContract;

/// Characters allowed unencoded in URL path segments — matches Python's
/// ``urllib.parse.quote(s, safe='')`` so a `/` in a CRM route name is
/// percent-encoded as `%2F` rather than splitting the path.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const EXPECTED_CRM_NS_HEADER: &str = "x-c2-expected-crm-ns";
const EXPECTED_CRM_NAME_HEADER: &str = "x-c2-expected-crm-name";
const EXPECTED_CRM_VER_HEADER: &str = "x-c2-expected-crm-ver";
const EXPECTED_ABI_HASH_HEADER: &str = "x-c2-expected-abi-hash";
const EXPECTED_SIGNATURE_HASH_HEADER: &str = "x-c2-expected-signature-hash";
const ROUTE_UID_HEADER: &str = "x-c2-route-uid";
const ROUTE_REVISION_HEADER: &str = "x-c2-route-revision";

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

// ── Shared tokio runtime ────────────────────────────────────────────────

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
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
    /// The logical call owner rejected a local guard. Preserve code and details
    /// for the caller's canonical conversion; this is never a transport failure.
    #[error("HTTP local call rejected: {0}")]
    LocalCallRejected(#[source] c2_error::C2Error),

    /// Invalid local request configuration.
    #[error("HTTP invalid request: {0}")]
    InvalidInput(String),

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
    probe_client: reqwest::Client,
    base_url: String,
    remote_payload_chunk_size: u64,
}

/// Shared owner of a controlled call's read-only input.
///
/// The owner must expose the same immutable byte slice throughout its lifetime.
/// Callers can keep retention permits alongside their bytes in this owner:
/// request bodies and their slices retain the complete owner, even after an
/// attempt future returns. This transport neither interprets nor releases a
/// caller's permit separately from the input. `Vec<u8>` is also a valid owner.
pub type HttpInputOwner = Arc<dyn AsRef<[u8]> + Send + Sync + 'static>;

pub(crate) enum HttpCallInput<'a> {
    Borrowed(&'a [u8]),
    Owned(HttpInputOwner),
}

impl HttpCallInput<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Borrowed(data) => data.len(),
            Self::Owned(data) => data.as_ref().as_ref().len(),
        }
    }

    fn body(&self, chunk_size: u64) -> Result<reqwest::Body, HttpError> {
        match self {
            Self::Borrowed(data) => crate::payload::reqwest_body_from_payload(data, chunk_size),
            Self::Owned(data) => {
                crate::payload::reqwest_body_from_owned_payload(Arc::clone(data), chunk_size)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpRouteToken {
    pub route_uid: String,
    pub route_revision: u64,
}

impl HttpClient {
    #[cfg(test)]
    fn new_with_proxy_policy(
        base_url: &str,
        timeout_secs: f64,
        max_connections: usize,
        use_proxy: bool,
    ) -> Result<Self, HttpError> {
        Self::new_with_transport_policy(
            base_url,
            timeout_secs,
            max_connections,
            use_proxy,
            c2_config::DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE,
        )
    }

    /// Create a new client with explicit proxy and remote payload batching policy.
    pub fn new_with_transport_policy(
        base_url: &str,
        timeout_secs: f64,
        max_connections: usize,
        use_proxy: bool,
        remote_payload_chunk_size: u64,
    ) -> Result<Self, HttpError> {
        Self::new_with_policy(
            base_url,
            timeout_secs,
            max_connections,
            use_proxy,
            remote_payload_chunk_size,
            false,
        )
    }

    pub(crate) fn new_controlled(
        base_url: &str,
        max_connections: usize,
        use_proxy: bool,
        remote_payload_chunk_size: u64,
    ) -> Result<Self, HttpError> {
        Self::new_with_policy(
            base_url,
            0.0,
            max_connections,
            use_proxy,
            remote_payload_chunk_size,
            true,
        )
    }

    fn new_with_policy(
        base_url: &str,
        timeout_secs: f64,
        max_connections: usize,
        use_proxy: bool,
        remote_payload_chunk_size: u64,
        controlled: bool,
    ) -> Result<Self, HttpError> {
        if !timeout_secs.is_finite() || timeout_secs < 0.0 {
            return Err(HttpError::Transport(
                "HTTP call timeout must be finite and >= 0".to_string(),
            ));
        }
        crate::payload::validate_remote_payload_chunk_size(remote_payload_chunk_size)?;
        let mut builder = crate::relay_client_builder_with_proxy(use_proxy)
            .pool_max_idle_per_host(max_connections);
        if timeout_secs > 0.0 {
            builder = builder.timeout(Duration::from_secs_f64(timeout_secs));
        }
        if controlled {
            // Core owns the logical deadline. No upload/total timeout or hidden
            // POST replay may bypass the per-call dispatch guard.
            builder = builder
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none());
        }
        let client = builder
            .build()
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        // Probes are control-plane operations with an independent safety timeout.
        // Waiting for them never resets the owner's original absolute deadline.
        let probe_client = if controlled {
            crate::relay_client_builder_with_proxy(use_proxy)
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| HttpError::Transport(e.to_string()))?
        } else {
            client.clone()
        };
        Ok(Self {
            client,
            probe_client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            remote_payload_chunk_size,
        })
    }

    pub(crate) async fn call_with_route_token_async(
        &self,
        expected: &ExpectedRouteContract,
        route_token: &HttpRouteToken,
        method_name: &str,
        data: &HttpCallInput<'_>,
        control: Option<&HttpCallControl>,
        previous_dispatch: Option<HttpCallPhase>,
    ) -> Result<Vec<u8>, HttpError> {
        let url = format!(
            "{}/{}/{}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(method_name),
        );
        let request = self
            .client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .header(CONTENT_LENGTH, data.len().to_string())
            .body(data.body(self.remote_payload_chunk_size)?);
        let request = add_expected_contract_headers(request, expected);
        let request = add_route_token_headers(request, route_token);
        let request = request
            .build()
            .map_err(|e| HttpError::InvalidInput(e.to_string()))?;
        // The body and headers are fully prepared before the actual send gate.
        super::call_control::check_active(control)?;
        if let Some(control) = control {
            control.check_dispatch(previous_dispatch)?;
        }
        let resp = self
            .client
            .execute(request)
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
        handle.block_on(self.health_async())
    }

    /// Async health check — GET /health.
    pub async fn health_async(&self) -> Result<bool, HttpError> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        Ok(resp.status().as_u16() == 200)
    }

    pub(crate) async fn probe_route_with_token_async(
        &self,
        expected: &ExpectedRouteContract,
        route_token: &HttpRouteToken,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/_probe/{}",
            self.base_url,
            encode_segment(&expected.route_name)
        );
        let request = add_expected_contract_headers(self.probe_client.get(&url), expected);
        let request = add_route_token_headers(request, route_token);
        let resp = request
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        match resp.status().as_u16() {
            200 => Ok(()),
            code => {
                let text = resp.text().await.unwrap_or_default();
                Err(HttpError::ServerError(code, text))
            }
        }
    }

    /// Base URL of this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn remote_payload_chunk_size(&self) -> u64 {
        self.remote_payload_chunk_size
    }
}

fn add_expected_contract_headers(
    request: reqwest::RequestBuilder,
    expected: &ExpectedRouteContract,
) -> reqwest::RequestBuilder {
    request
        .header(EXPECTED_CRM_NS_HEADER, &expected.crm_ns)
        .header(EXPECTED_CRM_NAME_HEADER, &expected.crm_name)
        .header(EXPECTED_CRM_VER_HEADER, &expected.crm_ver)
        .header(EXPECTED_ABI_HASH_HEADER, &expected.abi_hash)
        .header(EXPECTED_SIGNATURE_HASH_HEADER, &expected.signature_hash)
}

fn add_route_token_headers(
    request: reqwest::RequestBuilder,
    route_token: &HttpRouteToken,
) -> reqwest::RequestBuilder {
    request
        .header(ROUTE_UID_HEADER, &route_token.route_uid)
        .header(
            ROUTE_REVISION_HEADER,
            route_token.route_revision.to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prepared_owned_body_guard_returns_intact_local_error_before_network() {
        let client = HttpClient::new_controlled("http://127.0.0.1:9", 100, false, 3).unwrap();
        let expected = ExpectedRouteContract {
            route_name: "grid".into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "a".repeat(64),
            signature_hash: "b".repeat(64),
        };
        let token = HttpRouteToken {
            route_uid: "route-grid".into(),
            route_revision: 1,
        };
        let owner = Arc::new(vec![1, 2, 3, 4, 5, 6, 7]);
        let weak = Arc::downgrade(&owner);
        let observe_body = weak.clone();
        let rejection =
            c2_error::C2Error::new(c2_error::ErrorCode::ClientCallingResource, "expired")
                .with_details(std::collections::BTreeMap::from([(
                    "scope".into(),
                    "original".into(),
                )]));
        let rejected = rejection.clone();
        let control = HttpCallControl::new(
            || Ok(()),
            move |phase| {
                assert_eq!(phase, None);
                assert_eq!(
                    observe_body.strong_count(),
                    2,
                    "prepared body shares the original input"
                );
                Err(rejected.clone())
            },
        );
        let input = HttpCallInput::Owned(owner);
        let error = client
            .call_with_route_token_async(&expected, &token, "step", &input, Some(&control), None)
            .await
            .unwrap_err();
        assert!(matches!(error, HttpError::LocalCallRejected(error) if error == rejection));
        assert_eq!(
            weak.strong_count(),
            1,
            "rejected request releases its body owner"
        );
        drop(input);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn controlled_data_client_has_no_total_timeout_while_probe_remains_bounded() {
        let controlled =
            HttpClient::new_controlled("http://localhost:9988", 100, false, 1024).unwrap();
        let legacy =
            HttpClient::new_with_transport_policy("http://localhost:9988", 300.0, 100, false, 1024)
                .unwrap();
        // reqwest's Client Debug projects its actual configured TotalTimeout
        // (reqwest 0.12.28 RequestConfig::fmt_as_field), not a local policy copy.
        assert!(!format!("{:?}", controlled.client).contains("TotalTimeout"));
        assert!(format!("{:?}", controlled.probe_client).contains("TotalTimeout"));
        assert!(format!("{:?}", legacy.client).contains("TotalTimeout"));
    }

    #[test]
    fn zero_timeout_is_accepted_to_disable_total_call_timeout() {
        let client = HttpClient::new_with_proxy_policy("http://localhost:9991", 0.0, 100, false)
            .expect("zero timeout should be accepted");

        assert_eq!(client.base_url(), "http://localhost:9991");
    }

    #[test]
    fn invalid_timeout_is_rejected_without_panic() {
        for timeout in [-1.0, f64::NAN, f64::INFINITY] {
            let err = match HttpClient::new_with_proxy_policy(
                "http://localhost:9990",
                timeout,
                100,
                false,
            ) {
                Ok(_) => panic!("invalid timeout should fail"),
                Err(err) => err,
            };

            assert!(
                err.to_string().contains("timeout"),
                "unexpected error for {timeout}: {err}"
            );
        }
    }

    #[test]
    fn raw_http_client_does_not_expose_public_named_route_crm_calls() {
        let source = include_str!("http_client.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("http_client.rs must contain production section");
        for forbidden in [
            "pub fn call(\n        &self,\n        route_name: &str,",
            "pub async fn call_async(\n        &self,\n        route_name: &str,",
            "pub async fn probe_route_async(&self, route_name: &str)",
        ] {
            assert!(
                !production.contains(forbidden),
                "raw HttpClient must not expose public named-route CRM API: {forbidden}",
            );
        }
    }

    #[test]
    fn http_client_expected_contract_helpers_do_not_accept_optional_or_independent_route() {
        let source = include_str!("http_client.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("http_client.rs must contain production section");
        for forbidden in [
            "expected: Option<&ExpectedRouteContract>",
            "call_with_expected_crm_async(\n        &self,\n        route_name: &str,",
            "probe_route_with_expected_crm_async(\n        &self,\n        route_name: &str,",
        ] {
            assert!(
                !production.contains(forbidden),
                "HTTP expected-contract helpers must derive route from a required ExpectedRouteContract: {forbidden}",
            );
        }
    }
}
