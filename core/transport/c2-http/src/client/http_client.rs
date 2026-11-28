//! HTTP client for CRM calls through a relay server.

use c2_mem::{BudgetedBuffer, DomainBudget, MemoryReservation};
use std::sync::{Arc, OnceLock};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::CONTENT_LENGTH;
use thiserror::Error;

use super::HttpCallControl;
use super::HttpTopicSubscription;
use super::{HttpResponseStream, HttpStreamCancellation};
use crate::http_wire::{
    MAX_CONTROL_ERROR_BYTES, add_metadata, read_control_body, read_control_error, single_header,
    topic_open_headers,
};
use crate::outcome_wire::{CALL_ID_HEADER, MAX_OUTCOME_JSON_BYTES, OutcomeJson};
use crate::{CALL_PATH, KEYED_PATH, OUTCOME_PATH, RECOVER_PATH, TOPIC_SUBSCRIBE_PATH};
use c2_config::CallMetadata;
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

    /// Received bounded HTTP control error body.
    #[error("HTTP {0}: {1:?}")]
    ResponseError(u16, Vec<u8>),

    /// Network / connection errors.
    #[error("HTTP request error: {0}")]
    Request(#[source] reqwest::Error),

    /// Local transport/runtime errors without a reqwest source.
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
/// Uses `reqwest::Client` internally with connection pooling. The data-plane
/// client carries no total call timeout: only the caller's explicit
/// `CallScope` / `CallOptions` deadline bounds a call.
pub struct HttpClient {
    data_client: reqwest::Client,
    control_client: reqwest::Client,
    base_url: String,
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
        budget: &DomainBudget,
        deadline: Option<std::time::Instant>,
        input_charged: bool,
    ) -> Result<reqwest::Body, HttpError> {
        match self {
            Self::Borrowed(data) => {
                crate::payload::reqwest_body_from_payload(data, budget, deadline).await
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
                crate::payload::reqwest_body_from_owned_payload(owner)
            }
        }
    }
}

struct ReservedInput {
    data: HttpInputOwner,
    _reservation: MemoryReservation,
}
impl AsRef<[u8]> for ReservedInput {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref().as_ref()
    }
}

#[derive(Clone, Copy)]
pub(crate) enum HttpUnaryMode<'a> {
    Plain,
    Keyed(&'a str),
    Recover(&'a str),
}

pub(super) const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl HttpClient {
    /// Create independent control and data pools with the same trust policy. The data-plane
    /// client has no total/upload timeout, no hidden POST replay and no
    /// redirects: inherited defaults, explicit deadlines and Unlimited belong
    /// to the caller's scope.
    pub fn new(
        base_url: &str,
        max_connections: usize,
        policy: &c2_config::ResolvedRelayClientConfig,
    ) -> Result<Self, HttpError> {
        let tls = crate::relay_tls_config(&policy.tls_trust)?;
        let build = |tls| {
            crate::tls::relay_client_builder_with_tls(policy.use_proxy, tls)
                .pool_max_idle_per_host(max_connections)
                .retry(reqwest::retry::never())
                .build()
                .map_err(|e| HttpError::Transport(e.to_string()))
        };
        Ok(Self {
            data_client: build(tls.clone())?,
            control_client: build(tls)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    pub(crate) async fn call_route_async(
        &self,
        expected: &ExpectedRouteContract,
        method_name: &str,
        data: HttpCallInput<'_>,
        budget: &DomainBudget,
        control: Option<&HttpCallControl>,
        mode: HttpUnaryMode<'_>,
        metadata: &CallMetadata,
    ) -> Result<BudgetedBuffer, HttpError> {
        let (path, call_id) = match mode {
            HttpUnaryMode::Plain => (CALL_PATH, None),
            HttpUnaryMode::Keyed(id) => (KEYED_PATH, Some(id)),
            HttpUnaryMode::Recover(id) => (RECOVER_PATH, Some(id)),
        };
        let mut request = self.data_client.post(format!(
            "{}{path}?route={}&method={}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(method_name)
        ));
        if let Some(id) = call_id {
            c2_config::validate_call_id(id).map_err(HttpError::InvalidInput)?;
            request = request.header(CALL_ID_HEADER, id);
        }
        let length = data.len();
        let deadline = control.and_then(HttpCallControl::deadline);
        super::call_control::check_active(control)?;
        let body = data
            .into_body(
                budget,
                deadline,
                control.is_some_and(|control| control.payload_budget().is_some()),
            )
            .await?;
        let request = request
            .header("Content-Type", "application/octet-stream")
            .header(CONTENT_LENGTH, length.to_string())
            .body(body);
        let request = add_expected_contract_headers(add_metadata(request, metadata), expected);
        let request = request
            .build()
            .map_err(|e| HttpError::InvalidInput(e.to_string()))?;
        // The body and headers are fully prepared before the actual send gate.
        super::call_control::check_active(control)?;
        if let Some(control) = control {
            control.check_dispatch()?;
        }
        let resp = self
            .data_client
            .execute(request)
            .await
            .map_err(HttpError::Request)?;

        crate::payload::read_http_response(resp, budget, None).await
    }

    pub(crate) async fn query_call_route_async(
        &self,
        expected: &ExpectedRouteContract,
        call_id: &str,
        metadata: &CallMetadata,
    ) -> Result<c2_wire::rpc::WireCallOutcome, HttpError> {
        c2_config::validate_call_id(call_id).map_err(HttpError::InvalidInput)?;
        let url = format!(
            "{}{OUTCOME_PATH}?route={}",
            self.base_url,
            encode_segment(&expected.route_name)
        );
        let request = self.control_client.get(url).header(CALL_ID_HEADER, call_id);
        let response = add_expected_contract_headers(add_metadata(request, metadata), expected)
            .send()
            .await
            .map_err(HttpError::Request)?;
        let status = response.status().as_u16();
        if status != 200 && status != 500 {
            return Err(read_control_error(response).await);
        }
        let limit = if status == 200 {
            MAX_OUTCOME_JSON_BYTES
        } else {
            MAX_CONTROL_ERROR_BYTES
        };
        let body = read_control_body(response, limit).await?;
        if status == 200 {
            let json: OutcomeJson<'_> = serde_json::from_slice(&body).map_err(|error| {
                HttpError::Transport(format!("invalid relay outcome JSON: {error}"))
            })?;
            c2_wire::rpc::WireCallOutcome::try_from(json)
                .map_err(|error| HttpError::Transport(format!("invalid relay outcome: {error}")))
        } else {
            let error = c2_error::C2Error::from_wire_bytes(&body).map_err(|error| {
                HttpError::Transport(format!("invalid relay outcome error: {error}"))
            })?;
            let error =
                error.ok_or_else(|| HttpError::Transport("empty relay outcome error".into()))?;
            Err(HttpError::ServerError(
                status,
                serde_json::to_string(&error.envelope()).expect("canonical envelope serializes"),
            ))
        }
    }

    pub(crate) async fn stream_route_async(
        &self,
        expected: &ExpectedRouteContract,
        method_name: &str,
        data: HttpCallInput<'_>,
        budget: &DomainBudget,
        send_headroom: MemoryReservation,
        completion_owner: Box<dyn Send>,
        cancellation: HttpStreamCancellation,
        control: &HttpCallControl,
        metadata: &CallMetadata,
    ) -> Result<HttpResponseStream, HttpError> {
        let url = format!(
            "{}{CALL_PATH}?route={}&method={}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(method_name),
        );
        let length = data.len();
        control.check_active()?;
        let body = data
            .into_body(
                budget,
                control.deadline(),
                control.payload_budget().is_some(),
            )
            .await?;
        let request = self
            .data_client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .header("Accept", crate::stream_wire::MEDIA_TYPE)
            .header(CONTENT_LENGTH, length.to_string())
            .body(body);
        let request = add_expected_contract_headers(add_metadata(request, metadata), expected)
            .build()
            .map_err(|error| HttpError::InvalidInput(error.to_string()))?;
        control.check_active()?;
        control.check_dispatch()?;
        let response = self
            .data_client
            .execute(request)
            .await
            .map_err(HttpError::Request)?;
        // The input owner follows the actual reqwest body. This reservation
        // was acquired atomically with it before dispatch and is now unused.
        drop(send_headroom);
        if response.status().as_u16() != 200 {
            return Err(crate::payload::read_http_error(response, budget, None).await);
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
            self.control_client.clone(),
            self.base_url.clone(),
            token,
            budget.clone(),
            crate::stream_wire::MAX_TERMINAL_PAYLOAD_BYTES,
            completion_owner,
            cancellation,
        ))
    }

    pub(crate) async fn topic_route_async(
        &self,
        expected: &ExpectedRouteContract,
        topic_name: &str,
        options: c2_wire::rpc::WireSubscribeOptions,
        budget: &DomainBudget,
        completion_owner: Box<dyn Send>,
        cancellation: HttpStreamCancellation,
        control: &HttpCallControl,
        metadata: &CallMetadata,
    ) -> Result<HttpTopicSubscription, HttpError> {
        struct CancelOpening(Option<HttpStreamCancellation>);
        impl Drop for CancelOpening {
            fn drop(&mut self) {
                if let Some(cancel) = self.0.take() {
                    cancel.cancel();
                }
            }
        }
        control.check_active()?;
        let url = format!(
            "{}{TOPIC_SUBSCRIBE_PATH}?route={}&topic={}",
            self.base_url,
            encode_segment(&expected.route_name),
            encode_segment(topic_name)
        );
        let request = self
            .data_client
            .post(url)
            .header("Accept", crate::topic_wire::MEDIA_TYPE)
            .header(
                "x-c2-topic-start",
                match options.start {
                    c2_wire::rpc::WireTopicStart::Latest => "latest",
                    c2_wire::rpc::WireTopicStart::Next => "next",
                },
            )
            .header(
                "x-c2-topic-on-lag",
                match options.on_lag {
                    c2_wire::rpc::WireLagPolicy::Skip => "skip",
                    c2_wire::rpc::WireLagPolicy::Error => "error",
                },
            )
            .header(CONTENT_LENGTH, "0");
        let request = add_expected_contract_headers(add_metadata(request, metadata), expected)
            .build()
            .map_err(|e| HttpError::InvalidInput(e.to_string()))?;
        control.check_active()?;
        control.check_dispatch()?;
        let data_client = self.data_client.clone();
        let control_client = self.control_client.clone();
        let base_url = self.base_url.clone();
        let budget = budget.clone();
        let actor_cancel = cancellation.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = async {
                let response = data_client
                    .execute(request)
                    .await
                    .map_err(HttpError::Request)?;
                if response.status().as_u16() != 200 {
                    return Err(crate::payload::read_http_error(response, &budget, None).await);
                }
                if response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    != Some(crate::topic_wire::MEDIA_TYPE)
                {
                    return Err(stream_protocol_error("invalid_topic_content_type"));
                }
                let token = single_header(response.headers(), "x-c2-call-token")
                    .filter(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| stream_protocol_error("invalid_call_token"))?
                    .to_owned();
                topic_open_headers(response.headers()).map_err(stream_protocol_error)?;
                HttpTopicSubscription::start(
                    response,
                    control_client,
                    base_url,
                    token,
                    budget,
                    options.on_lag,
                    completion_owner,
                    actor_cancel,
                )
                .await
            }
            .await;
            let _ = tx.send(result);
        });
        let mut guard = CancelOpening(Some(cancellation));
        let result = rx
            .await
            .map_err(|_| HttpError::Transport("HTTP topic opening task ended".into()))?;
        guard.0.take();
        result
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
            .control_client
            .get(&url)
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(HttpError::Request)?;
        Ok(resp.status().as_u16() == 200)
    }

    pub(crate) async fn probe_route_async(
        &self,
        expected: &ExpectedRouteContract,
    ) -> Result<(), HttpError> {
        let route = encode_segment(&expected.route_name);
        let url = if matches!(expected.route_name.as_str(), "." | "..") {
            format!("{}/_probe?route={route}", self.base_url)
        } else {
            format!("{}/_probe/{route}", self.base_url)
        };
        let request = add_expected_contract_headers(
            self.control_client
                .get(&url)
                .timeout(CONTROL_REQUEST_TIMEOUT),
            expected,
        );
        let response = request.send().await.map_err(HttpError::Request)?;
        if response.status().as_u16() == 200 {
            Ok(())
        } else {
            Err(read_control_error(response).await)
        }
    }

    /// Base URL of this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
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

pub(crate) fn add_expected_contract_headers(
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

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn topic_v2_client_requires_unique_valid_admission_and_head_headers() {
        use crate::http_wire::{TOPIC_ADMISSION_REQUIRED_HEADER, TOPIC_HEAD_SEQ_HEADER};
        use axum::{Router, body::Body, response::Response, routing::post};
        use c2_wire::rpc::{WireLagPolicy, WireSubscribeOptions, WireTopicStart};

        let expected = ExpectedRouteContract {
            route_name: "grid".into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "a".repeat(64),
            signature_hash: "b".repeat(64),
        };
        for (admission, head, reason) in [
            (&[][..], &["0"][..], "invalid_topic_admission_required"),
            (&["1", "1"], &["0"], "invalid_topic_admission_required"),
            (&["true"], &["0"], "invalid_topic_admission_required"),
            (&["1"], &[], "invalid_topic_head_seq"),
            (&["1"], &["0", "0"], "invalid_topic_head_seq"),
            (&["1"], &["+1"], "invalid_topic_head_seq"),
            (&["1"], &["18446744073709551616"], "invalid_topic_head_seq"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().route(
                TOPIC_SUBSCRIBE_PATH,
                post(move || async move {
                    let mut response = Response::builder()
                        .header("content-type", crate::topic_wire::MEDIA_TYPE)
                        .header("x-c2-call-token", "00000000000000000000000000000001");
                    for value in admission {
                        response = response.header(TOPIC_ADMISSION_REQUIRED_HEADER, *value);
                    }
                    for value in head {
                        response = response.header(TOPIC_HEAD_SEQ_HEADER, *value);
                    }
                    response.body(Body::empty()).unwrap()
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let client = HttpClient::new(&url, 1, &Default::default()).unwrap();
            let result = client
                .topic_route_async(
                    &expected,
                    "frames",
                    WireSubscribeOptions {
                        start: WireTopicStart::Next,
                        on_lag: WireLagPolicy::Skip,
                    },
                    &crate::payload::memory_only_budget(0),
                    Box::new(()),
                    HttpStreamCancellation::new(),
                    &HttpCallControl::new(|| Ok(()), || Ok(())),
                    &CallMetadata::empty(),
                )
                .await;
            server.abort();
            let Err(HttpError::ServerError(200, body)) = result else {
                panic!("malformed subscription headers must fail before reading the stream");
            };
            let error = c2_error::C2Error::from_json_bytes(body.as_bytes()).unwrap();
            assert_eq!(error.code, c2_error::ErrorCode::ProtocolViolation);
            assert_eq!(error.details["reason"], reason);
        }
    }

    #[tokio::test]
    async fn prepared_owned_body_guard_returns_intact_local_error_before_network() {
        let client = HttpClient::new("http://127.0.0.1:9", 100, &Default::default()).unwrap();
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
                "step",
                input,
                &crate::payload::memory_only_budget(1024),
                Some(&control),
                HttpUnaryMode::Plain,
                &CallMetadata::empty(),
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
    fn pools_leave_total_call_timeouts_to_the_call_scope() {
        let client = HttpClient::new("http://localhost:9988", 100, &Default::default()).unwrap();
        // reqwest's Client Debug projects its actual configured TotalTimeout
        // (reqwest 0.12.28 RequestConfig::fmt_as_field), not a local policy copy.
        assert!(!format!("{:?}", client.data_client).contains("TotalTimeout"));
        assert!(!format!("{:?}", client.control_client).contains("TotalTimeout"));
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

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tls_tests;
