//! HTTP client for CRM calls through a relay server.

use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget, PayloadBufferReservation};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::CONTENT_LENGTH;
use thiserror::Error;

use super::HttpCallControl;
use super::{HttpResponseStream, HttpStreamCancellation};
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

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

// ── Shared tokio runtime ────────────────────────────────────────────────

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static RUNTIME_INITIALIZATION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    runtime_with_deadline(c2_config::ConnectDeadline::default())
        .expect("failed to create c2-http runtime")
}

pub(crate) fn runtime_with_deadline(
    deadline: c2_config::ConnectDeadline,
) -> Result<&'static tokio::runtime::Runtime, HttpError> {
    runtime_cell_with_deadline(&RUNTIME, &RUNTIME_INITIALIZATION, deadline)
}

fn runtime_cell_with_deadline<'a>(
    cell: &'a OnceLock<tokio::runtime::Runtime>,
    initialization: &parking_lot::Mutex<()>,
    deadline: c2_config::ConnectDeadline,
) -> Result<&'a tokio::runtime::Runtime, HttpError> {
    super::connect_deadline::check(deadline, "http_runtime_wait")?;
    if let Some(runtime) = cell.get() {
        return Ok(runtime);
    }
    // Every initializer, including ordinary calls with no caller deadline,
    // uses this coordinator. OnceLock::get never waits on initialization.
    let _initialization =
        super::connect_deadline::lock(initialization, deadline, "http_runtime_wait")?;
    if let Some(runtime) = cell.get() {
        return Ok(runtime);
    }
    super::connect_deadline::check(deadline, "http_runtime_start")?;
    let constructed = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("c2-http-io")
        .enable_all()
        .build();
    if let Err(error) = super::connect_deadline::check(deadline, "http_runtime_start") {
        // No work has been submitted. Dispose without publishing and without
        // blocking a caller that may itself be running in a Tokio context.
        if let Ok(runtime) = constructed {
            runtime.shutdown_background();
        }
        return Err(error);
    }
    let runtime = constructed.map_err(|error| {
        HttpError::Transport(format!("failed to create c2-http runtime: {error}"))
    })?;
    // The coordinator is the sole publication authority for this cell.
    cell.set(runtime)
        .expect("runtime initializer holds the coordinator");
    Ok(cell
        .get()
        .expect("runtime initializer must publish before returning"))
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
    CrmError(BudgetedBuffer),

    /// Received non-CRM HTTP error body, retaining its payload reservation.
    #[error("HTTP {0}: {1:?}")]
    ResponseError(u16, BudgetedBuffer),

    /// Network / connection errors.
    #[error("HTTP transport error: {0}")]
    Transport(String),

    /// Remote HTTP errors, or a canonical local response protocol violation
    /// paired with the actual received status (including 200/500).
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

    async fn into_body(
        self,
        chunk_size: u64,
        budget: &PayloadBufferBudget,
        deadline: Option<Instant>,
        input_charged: bool,
    ) -> Result<reqwest::Body, HttpError> {
        match self {
            Self::Borrowed(data) => {
                crate::payload::reqwest_body_from_payload(data, chunk_size, budget, deadline).await
            }
            Self::Owned(data) => {
                let owner = if input_charged {
                    data
                } else {
                    let reservation = crate::payload::reserve_payload(
                        budget,
                        data.as_ref().as_ref().len() as u64,
                        deadline,
                        "pre_dispatch",
                    )
                    .await?;
                    Arc::new(ReservedInput {
                        data,
                        _reservation: reservation,
                    })
                };
                crate::payload::reqwest_body_from_owned_payload(owner, chunk_size)
            }
        }
    }
}

struct ReservedInput {
    data: HttpInputOwner,
    _reservation: PayloadBufferReservation,
}
impl AsRef<[u8]> for ReservedInput {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref().as_ref()
    }
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

    pub(crate) async fn call_route_async(
        &self,
        expected: &ExpectedRouteContract,
        max_payload_size: u64,
        method_name: &str,
        data: HttpCallInput<'_>,
        budget: &PayloadBufferBudget,
        control: Option<&HttpCallControl>,
    ) -> Result<BudgetedBuffer, HttpError> {
        let url = format!(
            "{}/{}/{}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(method_name),
        );
        let length = data.len();
        let deadline = control.and_then(HttpCallControl::deadline);
        super::call_control::check_active(control)?;
        let body = data
            .into_body(
                self.remote_payload_chunk_size,
                budget,
                deadline,
                control.is_some_and(|control| control.payload_budget().is_some()),
            )
            .await?;
        let request = self
            .client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .header(CONTENT_LENGTH, length.to_string())
            .body(body);
        let request = add_expected_contract_headers(request, expected);
        let request = request
            .build()
            .map_err(|e| HttpError::InvalidInput(e.to_string()))?;
        // The body and headers are fully prepared before the actual send gate.
        super::call_control::check_active(control)?;
        if let Some(control) = control {
            control.check_dispatch()?;
        }
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        crate::payload::read_http_response(resp, max_payload_size, budget, None).await
    }

    pub(crate) async fn stream_route_async(
        &self,
        expected: &ExpectedRouteContract,
        max_payload_size: u64,
        method_name: &str,
        data: HttpCallInput<'_>,
        budget: &PayloadBufferBudget,
        send_headroom: PayloadBufferReservation,
        completion_owner: Box<dyn Send>,
        cancellation: HttpStreamCancellation,
        control: &HttpCallControl,
    ) -> Result<HttpResponseStream, HttpError> {
        let url = format!(
            "{}/{}/{}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(method_name),
        );
        let length = data.len();
        control.check_active()?;
        let body = data
            .into_body(
                self.remote_payload_chunk_size,
                budget,
                control.deadline(),
                control.payload_budget().is_some(),
            )
            .await?;
        let request = self
            .client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .header("Accept", crate::stream_wire::MEDIA_TYPE)
            .header(CONTENT_LENGTH, length.to_string())
            .body(body);
        let request = add_expected_contract_headers(request, expected)
            .build()
            .map_err(|error| HttpError::InvalidInput(error.to_string()))?;
        control.check_active()?;
        control.check_dispatch()?;
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        // The input owner follows the actual reqwest body. This reservation
        // was acquired atomically with it before dispatch and is now unused.
        drop(send_headroom);
        if response.status().as_u16() != 200 {
            let status = response.status().as_u16();
            let body = crate::payload::read_http_response_body(
                response,
                crate::stream_wire::MAX_TERMINAL_PAYLOAD_BYTES,
                budget,
                None,
                "dispatch_uncertain",
            )
            .await?;
            return Err(HttpError::ResponseError(status, body));
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        if media_type != Some(crate::stream_wire::MEDIA_TYPE) {
            return Err(stream_protocol_error("invalid_content_type"));
        }
        let token = response
            .headers()
            .get("x-c2-call-token")
            .and_then(|value| value.to_str().ok())
            .filter(|token| token.len() >= 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| stream_protocol_error("invalid_call_token"))?
            .to_owned();
        Ok(HttpResponseStream::start(
            response,
            self.client.clone(),
            self.base_url.clone(),
            token,
            budget.clone(),
            max_payload_size,
            completion_owner,
            cancellation,
        ))
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

    pub(crate) async fn probe_route_async(
        &self,
        expected: &ExpectedRouteContract,
        budget: &PayloadBufferBudget,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/_probe/{}",
            self.base_url,
            encode_segment(&expected.route_name)
        );
        let request = add_expected_contract_headers(self.probe_client.get(&url), expected);
        let resp = request
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        match resp.status().as_u16() {
            200 => Ok(()),
            code => {
                let body = crate::payload::read_http_response_body(
                    resp,
                    crate::stream_wire::MAX_TERMINAL_PAYLOAD_BYTES,
                    budget,
                    None,
                    "pre_dispatch",
                )
                .await?;
                Err(HttpError::ResponseError(code, body))
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

fn stream_protocol_error(reason: &'static str) -> HttpError {
    let error = c2_error::C2Error::new(
        c2_error::ErrorCode::ProtocolViolation,
        "HTTP stream response headers are invalid",
    )
    .with_details(std::collections::BTreeMap::from([
        ("transport".into(), "http".into()),
        ("transport_phase".into(), "dispatch_uncertain".into()),
        ("reason".into(), reason.into()),
    ]));
    HttpError::ServerError(
        200,
        serde_json::to_string(&error.envelope()).expect("canonical error envelope is serializable"),
    )
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

#[cfg(test)]
mod tests {
    #[test]
    fn connect_runtime_initialization_wait_obeys_deadline_and_recovers() {
        let shared = std::sync::Arc::new((
            std::sync::OnceLock::<tokio::runtime::Runtime>::new(),
            parking_lot::Mutex::new(()),
        ));
        let held = shared.1.lock();
        let waiter_shared = std::sync::Arc::clone(&shared);
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let deadline = c2_config::ConnectDeadline::start(
                c2_config::ConnectOptions::new()
                    .with_timeout(std::time::Duration::from_millis(100)),
            )
            .unwrap();
            let result = runtime_cell_with_deadline(&waiter_shared.0, &waiter_shared.1, deadline)
                .map(|_| ());
            finished_tx.send(result).unwrap();
        });
        let result = finished_rx.recv_timeout(std::time::Duration::from_secs(2));
        let no_publish = shared.0.get().is_none();
        drop(held);
        waiter.join().unwrap();
        let error = result
            .expect("connect must finish before releasing initialization coordinator")
            .unwrap_err();
        let HttpError::LocalCallRejected(error) = error else {
            panic!("canonical error required")
        };
        assert_eq!(error.code, c2_error::ErrorCode::CallDeadlineExceeded);
        assert_eq!(error.details["stage"], "http_runtime_wait");
        assert!(no_publish);
        let runtime =
            runtime_cell_with_deadline(&shared.0, &shared.1, c2_config::ConnectDeadline::default())
                .unwrap();
        assert!(std::ptr::eq(runtime, shared.0.get().unwrap()));
        // An initialized runtime never waits for the coordinator again.
        let _held = shared.1.lock();
        let fast_deadline = c2_config::ConnectDeadline::start(
            c2_config::ConnectOptions::new().with_timeout(std::time::Duration::from_millis(100)),
        )
        .unwrap();
        assert!(std::ptr::eq(
            runtime,
            runtime_cell_with_deadline(&shared.0, &shared.1, fast_deadline).unwrap()
        ));
    }

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
            move || {
                assert_eq!(
                    observe_body.strong_count(),
                    1,
                    "only the actual request reader retains the original input"
                );
                Err(rejected.clone())
            },
        );
        let input = HttpCallInput::Owned(owner);
        let error = client
            .call_route_async(
                &expected,
                1024,
                "step",
                input,
                &PayloadBufferBudget::new(1024),
                Some(&control),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, HttpError::LocalCallRejected(error) if error == rejection));
        assert!(
            weak.upgrade().is_none(),
            "rejected request releases its last input reader"
        );
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
