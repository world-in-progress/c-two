//! Axum router for the multi-upstream relay server.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::{
    Json, Router,
    body::{Body, Bytes, HttpBody},
    extract::{
        ConnectInfo, DefaultBodyLimit, Extension, FromRequestParts, Path, Query, Request, State,
    },
    http::request::Parts,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures::StreamExt;

use crate::client::{LOCAL_ENDPOINT_NAMESPACE_HEADER, validate_local_endpoint_namespace};
use crate::payload::SegmentedPayload;
use crate::relay::authority::{
    ClaimedRouteContract, ControlError, LocalRegistration, LocalRouteOwner, RegisterPreparation,
    RegistrationWithdrawal, RouteAttestationError, RouteAuthority,
    attest_ipc_pending_route_contract, attest_ipc_route_contract, read_ipc_route_contract,
};
use crate::relay::conn_pool::UpstreamLease;
use crate::relay::gossip::{broadcast_route_announce, broadcast_route_withdraw};
use crate::relay::peer_handlers;
use crate::relay::route_table::valid_route_name;
use crate::relay::server::connect_register_attestation_client;
use crate::relay::state::{RegisterCommitResult, RelayState, UpstreamAcquireError};
use crate::relay::types::{PeerStatus, RouteEntry, RouteResolutionError, UpstreamEndpointKey};
use crate::send_budget::{SendBufferPlan, SendBuffers, reserve_send_buffers};
use c2_ipc::IpcClient;
use c2_mem::retention::{
    BudgetedBuffer, PayloadBufferBudget, PayloadBufferError, PayloadBufferReservation,
};
use tokio_util::sync::CancellationToken;

const CONTROL_BODY_LIMIT_BYTES: usize = 64 * 1024;
const EXPECTED_CRM_NS_HEADER: &str = "x-c2-expected-crm-ns";
const EXPECTED_CRM_NAME_HEADER: &str = "x-c2-expected-crm-name";
const EXPECTED_CRM_VER_HEADER: &str = "x-c2-expected-crm-ver";
const EXPECTED_ABI_HASH_HEADER: &str = "x-c2-expected-abi-hash";
const EXPECTED_SIGNATURE_HASH_HEADER: &str = "x-c2-expected-signature-hash";
const UNKNOWN_LENGTH_BODY_LIMIT_BYTES: u64 = 64 * 1024;
const REJECTED_BODY_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

struct ReadyRequestClient {
    lease: UpstreamLease,
    binding: c2_ipc::RouteBinding,
    route: RouteEntry,
}

enum RequestClient {
    Ready(Box<ReadyRequestClient>),
    WatchUnavailable {
        route: Box<RouteEntry>,
        reason: String,
    },
    NotFound,
    RouteError(c2_ipc::IpcError),
    Unreachable,
}

#[derive(Debug)]
struct ResponseFailure(Box<Response>);

impl ResponseFailure {
    fn new(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl From<Response> for ResponseFailure {
    fn from(response: Response) -> Self {
        Self::new(response)
    }
}

impl IntoResponse for ResponseFailure {
    fn into_response(self) -> Response {
        *self.0
    }
}

type ResponseResult<T> = Result<T, ResponseFailure>;

struct OptionalConnectInfo(Option<SocketAddr>);

impl<S> FromRequestParts<S> for OptionalConnectInfo
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let remote_addr = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr);
        std::future::ready(Ok(Self(remote_addr)))
    }
}

fn duplicate_route_response(
    state: &RelayState,
    name: &str,
    existing_address: Option<&str>,
    registrations: Option<String>,
) -> Response {
    let registrations = registrations.unwrap_or_else(|| {
        state.with_route_table(|table| {
            serde_json::to_string(&table.name_holders(name))
                .expect("registration metadata serializes")
        })
    });
    let mut details = BTreeMap::from([
        ("name".into(), name.into()),
        ("registrations".into(), registrations),
    ]);
    if let Some(address) = existing_address {
        details.insert("existing_address".into(), address.into());
    }
    json_error_response(
        StatusCode::CONFLICT,
        c2_error::C2Error::new(
            c2_error::ErrorCode::ResourceAlreadyRegistered,
            format!("route already registered: {name}"),
        )
        .with_details(details)
        .envelope(),
        None,
    )
}

fn route_resolution_error_response(name: &str, error: RouteResolutionError) -> Response {
    let (status, code, registrations) = match error {
        RouteResolutionError::Conflict(registrations) => (
            StatusCode::CONFLICT,
            c2_error::ErrorCode::RouteConflict,
            registrations,
        ),
        RouteResolutionError::Unavailable(registrations) => (
            StatusCode::SERVICE_UNAVAILABLE,
            c2_error::ErrorCode::ResourceUnavailable,
            registrations,
        ),
    };
    c2_error_response(
        status,
        code,
        format!("{}: {name}", code.name()),
        [
            ("route", name.into()),
            ("dispatch_phase", "pre_dispatch".into()),
            (
                "registrations",
                serde_json::to_string(&registrations).expect("registration metadata serializes"),
            ),
        ],
    )
}

fn resource_not_found_response(route_name: &str) -> Response {
    c2_error_response(
        StatusCode::NOT_FOUND,
        c2_error::ErrorCode::ResourceNotFound,
        format!("route not found: {route_name}"),
        [
            ("route", route_name.to_string()),
            ("dispatch_phase", "pre_dispatch".to_string()),
        ],
    )
}

fn resource_not_found_response_budgeted(
    route_name: &str,
    budget: &PayloadBufferBudget,
) -> Response {
    c2_error_response_with_budget(
        StatusCode::NOT_FOUND,
        c2_error::ErrorCode::ResourceNotFound,
        format!("route not found: {route_name}"),
        [
            ("route", route_name.to_string()),
            ("dispatch_phase", "pre_dispatch".to_string()),
        ],
        budget,
    )
}

fn resource_unavailable_response(route_name: &str, message: impl Into<String>) -> Response {
    resource_unavailable_response_with_phase(route_name, message, "pre_dispatch")
}

fn resource_unavailable_response_with_phase(
    route_name: &str,
    message: impl Into<String>,
    dispatch_phase: &str,
) -> Response {
    c2_error_response(
        StatusCode::BAD_GATEWAY,
        c2_error::ErrorCode::ResourceUnavailable,
        message,
        [
            ("route", route_name.to_string()),
            ("dispatch_phase", dispatch_phase.to_string()),
        ],
    )
}

fn resource_unavailable_response_budgeted(
    route_name: &str,
    message: impl Into<String>,
    dispatch_phase: &str,
    budget: &PayloadBufferBudget,
) -> Response {
    c2_error_response_with_budget(
        StatusCode::BAD_GATEWAY,
        c2_error::ErrorCode::ResourceUnavailable,
        message,
        [
            ("route", route_name.to_string()),
            ("dispatch_phase", dispatch_phase.to_string()),
        ],
        budget,
    )
}

fn route_watch_unavailable_response(route_name: &str, reason: impl Into<String>) -> Response {
    c2_error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        c2_error::ErrorCode::RouteWatchUnavailable,
        format!("route watch unavailable for {route_name}"),
        [("route", route_name.to_string()), ("reason", reason.into())],
    )
}

fn route_watch_unavailable_response_budgeted(
    route_name: &str,
    reason: impl Into<String>,
    budget: &PayloadBufferBudget,
) -> Response {
    c2_error_response_with_budget(
        StatusCode::SERVICE_UNAVAILABLE,
        c2_error::ErrorCode::RouteWatchUnavailable,
        format!("route watch unavailable for {route_name}"),
        [("route", route_name.to_string()), ("reason", reason.into())],
        budget,
    )
}

fn c2_error_response<I, K, V>(
    status: StatusCode,
    code: c2_error::ErrorCode,
    message: impl Into<String>,
    details: I,
) -> Response
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let details = details
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect::<BTreeMap<_, _>>();
    (
        status,
        Json(
            c2_error::C2Error::new(code, message)
                .with_details(details)
                .envelope(),
        ),
    )
        .into_response()
}

struct JsonSize(u64);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("JSON response length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reserve the exact serialized body before creating its owned Vec. The Body
/// carries the reservation through `Bytes` slices and clones until their last
/// real reference is released.
fn budgeted_json_response<T>(
    status: StatusCode,
    payload: T,
    budget: &PayloadBufferBudget,
    dispatch_uncertain: bool,
) -> Response
where
    T: serde::Serialize + Send + 'static,
{
    let mut size = JsonSize(0);
    if serde_json::to_writer(&mut size, &payload).is_err()
        || size.0 > budget.snapshot().limit_bytes
        || usize::try_from(size.0).is_err()
    {
        return if dispatch_uncertain {
            uncertain_error_control_response()
        } else {
            forwarding_capacity_response("", "error response exceeds payload budget")
        };
    }
    let budget = budget.clone();
    let body = Body::from_stream(futures::stream::once(async move {
        let reservation = budget
            .reserve(size.0)
            .await
            .map_err(std::io::Error::other)?;
        let mut data = Vec::with_capacity(size.0 as usize);
        serde_json::to_writer(&mut data, &payload).map_err(std::io::Error::other)?;
        Ok::<Bytes, std::io::Error>(Bytes::from_owner(BudgetedBuffer::new(data, reservation)))
    }));
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (header::CONTENT_LENGTH, size.0.to_string()),
        ],
        body,
    )
        .into_response()
}

fn json_error_response<T>(
    status: StatusCode,
    payload: T,
    budget: Option<&PayloadBufferBudget>,
) -> Response
where
    T: serde::Serialize + Send + 'static,
{
    match budget {
        Some(budget) => budgeted_json_response(status, payload, budget, false),
        None => (status, Json(payload)).into_response(),
    }
}

fn c2_error_response_with_budget<I, K, V>(
    status: StatusCode,
    code: c2_error::ErrorCode,
    message: impl Into<String>,
    details: I,
    budget: &PayloadBufferBudget,
) -> Response
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let details = details
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect::<BTreeMap<_, _>>();
    let dispatch_uncertain = details
        .get("dispatch_phase")
        .is_some_and(|phase| phase == "dispatch_uncertain");
    budgeted_json_response(
        status,
        c2_error::C2Error::new(code, message)
            .with_details(details)
            .envelope(),
        budget,
        dispatch_uncertain,
    )
}

#[derive(Clone, Copy)]
struct DeclaredBodyLength(Result<Option<u64>, &'static str>);

fn content_length_from_headers(headers: &HeaderMap) -> Result<Option<u64>, &'static str> {
    let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("Content-Length must appear at most once");
    }
    let raw = value
        .to_str()
        .map_err(|_| "Content-Length must be an ASCII unsigned integer")?;
    let content_length = raw
        .parse::<u64>()
        .map_err(|_| "Content-Length must be an unsigned integer")?;
    Ok(Some(content_length))
}

fn payload_too_large_response(
    route_name: &str,
    content_length: u64,
    max_payload_size: u64,
    budget: &PayloadBufferBudget,
) -> Response {
    json_error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        serde_json::json!({
            "error": "PayloadTooLarge",
            "route": route_name,
            "content_length": content_length,
            "max_payload_size": max_payload_size,
        }),
        Some(budget),
    )
}

fn unknown_length_too_large_response(
    route_name: &str,
    limit: u64,
    budget: &PayloadBufferBudget,
) -> Response {
    json_error_response(
        StatusCode::LENGTH_REQUIRED,
        serde_json::json!({
            "error": "ContentLengthRequired",
            "route": route_name,
            "message": format!("streaming relay calls larger than {limit} bytes require Content-Length"),
        }),
        Some(budget),
    )
}

#[derive(Debug, Default, serde::Deserialize)]
struct ResolveQuery {
    crm_ns: Option<String>,
    crm_name: Option<String>,
    crm_ver: Option<String>,
    abi_hash: Option<String>,
    signature_hash: Option<String>,
}

#[cfg(test)]
struct DataPlanePrecheckHook {
    state: std::sync::Weak<RelayState>,
    route_name: String,
    action: Box<dyn FnOnce() + Send + 'static>,
}

#[cfg(test)]
static DATA_PLANE_AFTER_PRECHECK_HOOKS: std::sync::Mutex<Vec<DataPlanePrecheckHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn set_data_plane_after_precheck_hook(
    state: &Arc<RelayState>,
    route_name: String,
    action: impl FnOnce() + Send + 'static,
) {
    DATA_PLANE_AFTER_PRECHECK_HOOKS
        .lock()
        .unwrap()
        .push(DataPlanePrecheckHook {
            state: Arc::downgrade(state),
            route_name,
            action: Box::new(action),
        });
}

#[cfg(test)]
fn run_data_plane_after_precheck_hook(state: &Arc<RelayState>, route_name: &str) {
    let hook = {
        let mut guard = DATA_PLANE_AFTER_PRECHECK_HOOKS.lock().unwrap();
        guard
            .iter()
            .position(|hook| {
                hook.route_name == route_name && hook.state.ptr_eq(&Arc::downgrade(state))
            })
            .map(|index| guard.remove(index))
    };
    if let Some(hook) = hook {
        (hook.action)();
    }
}

fn expected_crm_from_headers(
    route_name: &str,
    headers: &HeaderMap,
    budget: Option<&PayloadBufferBudget>,
) -> ResponseResult<c2_contract::ExpectedRouteContract> {
    let crm_ns = headers
        .get(EXPECTED_CRM_NS_HEADER)
        .map(|value| value.to_str().map(str::to_string));
    let crm_name = headers
        .get(EXPECTED_CRM_NAME_HEADER)
        .map(|value| value.to_str().map(str::to_string));
    let crm_ver = headers
        .get(EXPECTED_CRM_VER_HEADER)
        .map(|value| value.to_str().map(str::to_string));
    let abi_hash = headers
        .get(EXPECTED_ABI_HASH_HEADER)
        .map(|value| value.to_str().map(str::to_string));
    let signature_hash = headers
        .get(EXPECTED_SIGNATURE_HASH_HEADER)
        .map(|value| value.to_str().map(str::to_string));

    match (crm_ns, crm_name, crm_ver, abi_hash, signature_hash) {
        (None, None, None, None, None) => Err(json_error_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "InvalidCrmTag",
                "message": "expected CRM headers and hash headers are required",
            }),
            budget,
        )
        .into()),
        (
            Some(Ok(crm_ns)),
            Some(Ok(crm_name)),
            Some(Ok(crm_ver)),
            Some(Ok(abi_hash)),
            Some(Ok(signature_hash)),
        ) => {
            let expected = c2_contract::ExpectedRouteContract {
                route_name: route_name.to_string(),
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
            };
            if let Err(err) = c2_contract::validate_expected_route_contract(&expected) {
                return Err(json_error_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({
                        "error": "InvalidCrmTag",
                        "message": err.to_string(),
                    }),
                    budget,
                )
                .into());
            }
            Ok(expected)
        }
        (Some(Err(_)), _, _, _, _)
        | (_, Some(Err(_)), _, _, _)
        | (_, _, Some(Err(_)), _, _)
        | (_, _, _, Some(Err(_)), _)
        | (_, _, _, _, Some(Err(_))) => Err(json_error_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "InvalidCrmTag",
                "message": "expected CRM headers must be valid UTF-8",
            }),
            budget,
        )
        .into()),
        _ => Err(json_error_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "InvalidCrmTag",
                "message": "expected CRM headers and hash headers must be supplied together",
            }),
            budget,
        )
        .into()),
    }
}

fn crm_contract_mismatch_response(route_name: &str) -> Response {
    c2_error_response(
        StatusCode::CONFLICT,
        c2_error::ErrorCode::ContractMismatch,
        format!("CRM contract mismatch for route {route_name}"),
        [("route", route_name.to_string())],
    )
}

fn crm_contract_mismatch_response_budgeted(
    route_name: &str,
    budget: &PayloadBufferBudget,
) -> Response {
    c2_error_response_with_budget(
        StatusCode::CONFLICT,
        c2_error::ErrorCode::ContractMismatch,
        format!("CRM contract mismatch for route {route_name}"),
        [("route", route_name.to_string())],
        budget,
    )
}

fn route_matches_expected_crm(
    route: &RouteEntry,
    expected: &c2_contract::ExpectedRouteContract,
) -> bool {
    route.name == expected.route_name
        && route.crm_ns == expected.crm_ns
        && route.crm_name == expected.crm_name
        && route.crm_ver == expected.crm_ver
        && route.abi_hash == expected.abi_hash
        && route.signature_hash == expected.signature_hash
}

fn register_contract_claim_from_body(
    route_name: &str,
    body: &serde_json::Value,
) -> ResponseResult<Option<c2_contract::ExpectedRouteContract>> {
    let crm_ns = register_body_string_field(body, "crm_ns")?;
    let crm_name = register_body_string_field(body, "crm_name")?;
    let crm_ver = register_body_string_field(body, "crm_ver")?;
    let abi_hash = register_body_string_field(body, "abi_hash")?;
    let signature_hash = register_body_string_field(body, "signature_hash")?;

    match (crm_ns, crm_name, crm_ver, abi_hash, signature_hash) {
        (None, None, None, None, None) => Ok(None),
        (Some(crm_ns), Some(crm_name), Some(crm_ver), Some(abi_hash), Some(signature_hash)) => {
            let claimed = c2_contract::ExpectedRouteContract {
                route_name: route_name.to_string(),
                crm_ns: crm_ns.to_string(),
                crm_name: crm_name.to_string(),
                crm_ver: crm_ver.to_string(),
                abi_hash: abi_hash.to_string(),
                signature_hash: signature_hash.to_string(),
            };
            if let Err(err) = c2_contract::validate_expected_route_contract(&claimed) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "InvalidCrmTag",
                        "message": err.to_string(),
                    })),
                )
                    .into_response()
                    .into());
            }
            Ok(Some(claimed))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "InvalidCrmTag",
                "message": "crm_ns, crm_name, crm_ver, abi_hash, and signature_hash must be supplied together",
            })),
        )
            .into_response()
            .into()),
    }
}

fn local_namespace_from_headers(headers: &HeaderMap) -> Result<Option<&str>, Response> {
    let mut values = headers.get_all(LOCAL_ENDPOINT_NAMESPACE_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let namespace = value.to_str().ok();
    if values.next().is_some()
        || namespace.is_none_or(|value| validate_local_endpoint_namespace(value).is_err())
    {
        return Err((StatusCode::BAD_REQUEST, "Invalid local endpoint namespace").into_response());
    }
    Ok(namespace)
}

fn with_local_namespace_response(state: &RelayState, mut response: Response) -> Response {
    response.headers_mut().insert(
        LOCAL_ENDPOINT_NAMESPACE_HEADER,
        state
            .endpoint_context()
            .namespace_id()
            .parse()
            .expect("native namespace is valid ASCII"),
    );
    response
}

fn register_body_string_field<'a>(
    body: &'a serde_json::Value,
    field: &'static str,
) -> ResponseResult<Option<&'a str>> {
    match body.get(field) {
        Some(value) => value.as_str().map(Some).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "InvalidCrmTag",
                    "message": format!("{field} must be a string"),
                })),
            )
                .into_response()
                .into()
        }),
        None => Ok(None),
    }
}

fn register_body_bool_field(body: &serde_json::Value, field: &'static str) -> ResponseResult<bool> {
    match body.get(field) {
        Some(value) => value.as_bool().ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "InvalidRegisterRequest",
                    "message": format!("{field} must be a boolean"),
                })),
            )
                .into_response()
                .into()
        }),
        None => Ok(false),
    }
}

fn register_body_u64_field(body: &serde_json::Value, field: &'static str) -> ResponseResult<u64> {
    match body.get(field) {
        Some(value) => match value.as_u64() {
            Some(value) if value > 0 => Ok(value),
            _ => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "InvalidRegisterRequest",
                    "message": format!("{field} must be a positive integer"),
                })),
            )
                .into_response()
                .into()),
        },
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "InvalidRegisterRequest",
                "message": format!("Missing \"{field}\""),
            })),
        )
            .into_response()
            .into()),
    }
}

fn validate_expected_crm_for_route(
    state: &RelayState,
    route_name: &str,
    expected: &c2_contract::ExpectedRouteContract,
    budget: Option<&PayloadBufferBudget>,
) -> ResponseResult<RouteEntry> {
    let Some(route) = state.local_route(route_name) else {
        return Err(match budget {
            Some(budget) => resource_not_found_response_budgeted(route_name, budget),
            None => resource_not_found_response(route_name),
        }
        .into());
    };
    if route_matches_expected_crm(&route, expected) {
        Ok(route)
    } else {
        Err(match budget {
            Some(budget) => crm_contract_mismatch_response_budgeted(route_name, budget),
            None => crm_contract_mismatch_response(route_name),
        }
        .into())
    }
}

/// Build the relay axum router with control-plane and data-plane endpoints.
pub fn build_router(state: Arc<RelayState>) -> Router {
    let control_router = Router::new()
        .route("/_register", post(handle_register))
        .route("/_unregister", post(handle_unregister))
        .route("/_admin/unregister", post(handle_admin_unregister))
        .route("/_admin/force-leave", post(handle_admin_force_leave))
        .route("/_routes", get(handle_list_routes))
        .route("/_resolve/{*name}", get(handle_resolve))
        .route("/_probe/{*name}", get(handle_probe))
        .route("/_peers", get(handle_peers))
        .route("/_calls/{token}", delete(cancel_stream_call))
        .route("/_peer/announce", post(peer_handlers::handle_peer_announce))
        .route("/_peer/join", post(peer_handlers::handle_peer_join))
        .route("/_peer/sync", get(peer_handlers::handle_peer_sync))
        .route(
            "/_peer/heartbeat",
            post(peer_handlers::handle_peer_heartbeat),
        )
        .route("/_peer/leave", post(peer_handlers::handle_peer_leave))
        .route(
            "/_peer/force-leave",
            post(peer_handlers::handle_peer_force_leave),
        )
        .route("/_peer/digest", post(peer_handlers::handle_peer_digest))
        .route("/health", get(handle_health))
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT_BYTES));

    let data_router = Router::new()
        .route("/_echo", post(echo_handler))
        .route("/{route_name}/{method_name}", post(call_handler))
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .merge(control_router)
        .merge(data_router)
        .with_state(state)
        .layer(axum::middleware::from_fn(finish_unread_body_rejection))
}

// Retain the transport body at the router boundary until its first read. This
// also covers extractors that reject without reading (for example Json's 415).
// Once reading starts, the handler owns the body, including any partial read.
struct UnreadRequestBody {
    unread: Arc<parking_lot::Mutex<Option<Body>>>,
    reading: Option<Body>,
}

impl HttpBody for UnreadRequestBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.reading.is_none() {
            this.reading = this.unread.lock().take();
        }
        Pin::new(this.reading.as_mut().expect("first read owns the body")).poll_frame(cx)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match &self.reading {
            Some(body) => body.size_hint(),
            None => self.unread.lock().as_ref().unwrap().size_hint(),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.reading {
            Some(body) => body.is_end_stream(),
            None => self.unread.lock().as_ref().unwrap().is_end_stream(),
        }
    }
}

async fn finish_unread_body_rejection(
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    // Parse once, retaining the result for the call handler's existing error
    // precedence. Invalid declarations cannot authorize transport disposal.
    let length = DeclaredBodyLength(content_length_from_headers(request.headers()));
    let expect = request.headers().contains_key(header::EXPECT);
    request.extensions_mut().insert(length);
    let (parts, body) = request.into_parts();
    let unread = Arc::new(parking_lot::Mutex::new(Some(body)));
    let body = Body::new(UnreadRequestBody {
        unread: unread.clone(),
        reading: None,
    });
    let response = next.run(Request::from_parts(parts, body)).await;
    let body = unread.lock().take();
    if (response.status().is_client_error() || response.status().is_server_error())
        && let Some(body) = body
    {
        return finish_early_rejection(expect, length.0.ok().flatten(), body, response).await;
    }
    response
}

// -- Control-plane handlers -----------------------------------------------

/// `POST /_register` — register a new upstream CRM.
///
/// Body: `{"name": "grid", "server_id": "...", "server_instance_id": "...", "address": "ipc://...", "max_payload_size": 17179869184, "crm_ns": "...", "crm_name": "...", "crm_ver": "...", "abi_hash": "...", "signature_hash": "..."}`
/// The CRM contract is attested against the upstream IPC handshake. Optional
/// claimed contract fields, when present, must match that handshake exactly.
/// Returns: 201 on success, 409 on duplicate, 502 on connection failure.
async fn handle_register(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Namespace is a domain hint, never an authorization credential. Reject
    // mismatches before owner preparation or any local endpoint is probed.
    match local_namespace_from_headers(&headers) {
        Ok(Some(namespace)) if namespace != state.endpoint_context().namespace_id() => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "LocalEndpointNamespaceMismatch",
                    "message": "resource and relay local endpoint namespaces differ",
                })),
            )
                .into_response();
        }
        Ok(None) if !state.is_default_endpoint_namespace() => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "LocalEndpointNamespaceRequired",
                    "message": "custom relay endpoint context requires the resource's local endpoint namespace",
                })),
            )
                .into_response();
        }
        Err(response) => return response,
        _ => {}
    }
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"name\"").into_response(),
    };
    let address = match body.get("address").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"address\"").into_response(),
    };
    let server_id = match body.get("server_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"server_id\"").into_response(),
    };
    let server_instance_id = match body.get("server_instance_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, "Missing \"server_instance_id\"").into_response();
        }
    };
    let claimed_contract = match register_contract_claim_from_body(&name, &body) {
        Ok(claim) => claim,
        Err(response) => return response.into_response(),
    };
    let registration_token = match register_body_string_field(&body, "registration_token") {
        Ok(value) => value.map(str::to_string),
        Err(response) => return response.into_response(),
    };
    let prepare_only = match register_body_bool_field(&body, "prepare_only") {
        Ok(value) => value,
        Err(response) => return response.into_response(),
    };
    let max_payload_size = match register_body_u64_field(&body, "max_payload_size") {
        Ok(value) => value,
        Err(response) => return response.into_response(),
    };
    eprintln!(
        "[relay] Register request: name={name} server_id={server_id} server_instance_id={server_instance_id} address={address} prepare_only={prepare_only}"
    );
    if prepare_only && registration_token.is_none() {
        eprintln!(
            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=missing_registration_token"
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "PendingRouteAttestationRequired",
                "message": "prepare_only registration requires registration_token",
            })),
        )
            .into_response();
    }

    if let Err(ControlError::InvalidServerInstanceId { reason }) =
        RouteAuthority::new(&state).validate_server_instance_id(&server_instance_id)
    {
        eprintln!(
            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
        );
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }

    let replacement_candidate = match RouteAuthority::new(&state)
        .prepare_register(&name, &server_id, &server_instance_id, &address)
        .await
    {
        Ok(RegisterPreparation::Available { replacement }) => replacement,
        Ok(RegisterPreparation::SameOwner) => None,
        Ok(RegisterPreparation::DuplicateAlive { existing_address })
        | Err(ControlError::AddressMismatch { existing_address })
        | Err(ControlError::DuplicateRoute { existing_address }) => {
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=duplicate existing_address={existing_address}"
            );
            return duplicate_route_response(&state, &name, Some(&existing_address), None);
        }
        Err(ControlError::InvalidName { reason })
        | Err(ControlError::InvalidServerId { reason })
        | Err(ControlError::InvalidServerInstanceId { reason })
        | Err(ControlError::InvalidAddress { reason })
        | Err(ControlError::ContractMismatch { reason }) => {
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
            );
            return (StatusCode::BAD_REQUEST, reason).into_response();
        }
        Err(ControlError::PeerRouteHeld { registrations }) => {
            return duplicate_route_response(&state, &name, None, Some(registrations));
        }
        Err(ControlError::OwnerMismatch) | Err(ControlError::NotFound) => {
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=owner_not_replaceable"
            );
            return (StatusCode::CONFLICT, "Route owner is not replaceable").into_response();
        }
    };

    // Connect IPC client and attest the registered route contract.
    let (client, contract) = {
        let mut c = match connect_register_attestation_client(
            &address,
            state.endpoint_context(),
            &state.clients,
        )
        .await
        {
            Ok(client) => client,
            Err(e) => {
                eprintln!(
                    "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=connect_failed error={e}"
                );
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": format!("Failed to connect upstream '{name}' at {address}: {e}")})),
                )
                    .into_response();
            }
        };
        let identity_matches = c.server_id() == Some(server_id.as_str())
            && c.server_instance_id() == Some(server_instance_id.as_str());
        if !identity_matches {
            close_client(c);
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} server_instance_id={server_instance_id} address={address} reason=identity_mismatch"
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("IPC server identity mismatch for upstream '{name}' at {address}"),
                })),
            )
                .into_response();
        }
        let contract = match claimed_contract.as_ref() {
            Some(claimed_contract) => match attest_ipc_route_contract(
                &c,
                ClaimedRouteContract {
                    expected: claimed_contract,
                    max_payload_size,
                },
            )
            .await
            {
                Ok(contract) => contract,
                Err(RouteAttestationError::ContractMismatch { reason }) => {
                    close_client(c);
                    eprintln!(
                        "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                    );
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": reason })),
                    )
                        .into_response();
                }
                Err(RouteAttestationError::NotFound) => {
                    if !prepare_only {
                        close_client(c);
                        eprintln!(
                            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=pending_route_not_committed"
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "PendingRouteNotCommitted",
                                "message": format!("IPC upstream at {address} does not export committed route '{name}'"),
                            })),
                        )
                            .into_response();
                    }
                    let Some(registration_token) = registration_token.as_deref() else {
                        close_client(c);
                        eprintln!(
                            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=missing_registration_token"
                        );
                        return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": "PendingRouteAttestationRequired",
                                    "message": format!("IPC upstream at {address} does not export committed route '{name}' and no registration_token was provided"),
                                })),
                        )
                            .into_response();
                    };
                    match attest_ipc_pending_route_contract(
                        &mut c,
                        registration_token,
                        ClaimedRouteContract {
                            expected: claimed_contract,
                            max_payload_size,
                        },
                    )
                    .await
                    {
                        Ok(contract) => contract,
                        Err(RouteAttestationError::ContractMismatch { reason }) => {
                            close_client(c);
                            eprintln!(
                                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                            );
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({ "error": reason })),
                            )
                                .into_response();
                        }
                        Err(RouteAttestationError::NotFound) => {
                            close_client(c);
                            eprintln!(
                                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=pending_route_not_attested"
                            );
                            return (
                                    StatusCode::BAD_REQUEST,
                                    Json(serde_json::json!({
                                        "error": format!("IPC upstream at {address} does not attest pending route '{name}'"),
                                    })),
                                )
                                    .into_response();
                        }
                        Err(RouteAttestationError::UpstreamUnavailable { reason }) => {
                            close_client(c);
                            eprintln!(
                                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                            );
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(serde_json::json!({ "error": reason })),
                            )
                                .into_response();
                        }
                    }
                }
                Err(RouteAttestationError::UpstreamUnavailable { reason }) => {
                    close_client(c);
                    eprintln!(
                        "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({ "error": reason })),
                    )
                        .into_response();
                }
            },
            None => match c.rebuild_route_catalog().await {
                Ok(()) => match read_ipc_route_contract(&c, &name).await {
                    Ok(contract) => contract,
                    Err(RouteAttestationError::NotFound) => {
                        close_client(c);
                        eprintln!(
                            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=route_not_exported"
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": format!("IPC upstream at {address} does not export route '{name}'"),
                            })),
                        )
                            .into_response();
                    }
                    Err(RouteAttestationError::ContractMismatch { reason }) => {
                        close_client(c);
                        eprintln!(
                            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": reason })),
                        )
                            .into_response();
                    }
                    Err(RouteAttestationError::UpstreamUnavailable { reason }) => {
                        close_client(c);
                        eprintln!(
                            "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                        );
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(serde_json::json!({ "error": reason })),
                        )
                            .into_response();
                    }
                },
                Err(c2_ipc::IpcError::RouteNotFound(_)) => {
                    close_client(c);
                    eprintln!(
                        "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=route_not_exported"
                    );
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": format!("IPC upstream at {address} does not export route '{name}'"),
                        })),
                    )
                        .into_response();
                }
                Err(c2_ipc::IpcError::ContractMismatch(reason))
                | Err(c2_ipc::IpcError::Protocol(reason)) => {
                    close_client(c);
                    eprintln!(
                        "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
                    );
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": reason })),
                    )
                        .into_response();
                }
                Err(err) => {
                    close_client(c);
                    eprintln!(
                        "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=route_attestation_failed error={err}"
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({
                            "error": format!("Failed to attest upstream '{name}' at {address}: {err}"),
                        })),
                    )
                        .into_response();
                }
            },
        };
        if contract.max_payload_size != max_payload_size {
            close_client(c);
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=max_payload_size_mismatch claimed={max_payload_size} got={}",
                contract.max_payload_size
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!(
                        "IPC upstream route '{name}' max_payload_size mismatch: claimed {max_payload_size}, got {}",
                        contract.max_payload_size
                    ),
                })),
            )
                .into_response();
        }
        (c.into_shared(), contract)
    };

    if prepare_only {
        close_arc_client(&state, client);
        eprintln!(
            "[relay] Register prepared: name={name} server_id={server_id} server_instance_id={server_instance_id} address={address} crm={}/{}/{}",
            contract.crm_ns, contract.crm_name, contract.crm_ver,
        );
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "prepared",
                "name": name,
            })),
        )
            .into_response();
    }

    let replacement = match RouteAuthority::new(&state)
        .confirm_replacement_for_commit(replacement_candidate)
        .await
    {
        Ok(replacement) => replacement,
        Err(ControlError::DuplicateRoute { existing_address })
        | Err(ControlError::AddressMismatch { existing_address }) => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=duplicate existing_address={existing_address}"
            );
            return duplicate_route_response(&state, &name, Some(&existing_address), None);
        }
        Err(ControlError::PeerRouteHeld { registrations }) => {
            close_arc_client(&state, client);
            return duplicate_route_response(&state, &name, None, Some(registrations));
        }
        Err(ControlError::OwnerMismatch) | Err(ControlError::NotFound) => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason=owner_not_replaceable"
            );
            return (StatusCode::CONFLICT, "Route owner is not replaceable").into_response();
        }
        Err(ControlError::InvalidName { reason })
        | Err(ControlError::InvalidServerId { reason })
        | Err(ControlError::InvalidServerInstanceId { reason })
        | Err(ControlError::InvalidAddress { reason })
        | Err(ControlError::ContractMismatch { reason }) => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register rejected: name={name} server_id={server_id} address={address} reason={reason}"
            );
            return (StatusCode::BAD_REQUEST, reason).into_response();
        }
    };

    let entry = match state.commit_register_upstream(LocalRegistration {
        owner: LocalRouteOwner {
            name: name.clone(),
            server_id,
            server_instance_id,
            address,
        },
        contract,
        replacement,
    }) {
        RegisterCommitResult::Registered { entry } => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register committed: name={} server_id={} server_instance_id={} address={} crm={}/{}/{}",
                entry.name,
                entry.server_id.as_deref().unwrap_or(""),
                entry.server_instance_id.as_deref().unwrap_or(""),
                entry.ipc_address.as_deref().unwrap_or(""),
                entry.crm_ns,
                entry.crm_name,
                entry.crm_ver
            );
            #[cfg(test)]
            state.before_registration_watch_for_test().await;
            state.start_upstream_control(&entry);
            entry
        }
        RegisterCommitResult::SameOwner { entry } => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register same-owner: name={} server_id={} server_instance_id={} address={} crm={}/{}/{}",
                entry.name,
                entry.server_id.as_deref().unwrap_or(""),
                entry.server_instance_id.as_deref().unwrap_or(""),
                entry.ipc_address.as_deref().unwrap_or(""),
                entry.crm_ns,
                entry.crm_name,
                entry.crm_ver
            );
            #[cfg(test)]
            state.before_registration_watch_for_test().await;
            state.start_upstream_control(&entry);
            return (
                StatusCode::OK,
                Json(serde_json::json!({"registered": entry.name})),
            )
                .into_response();
        }
        RegisterCommitResult::Duplicate { existing_address }
        | RegisterCommitResult::ConflictingOwner { existing_address } => {
            close_arc_client(&state, client);
            eprintln!(
                "[relay] Register rejected: name={name} reason=duplicate existing_address={existing_address}"
            );
            return duplicate_route_response(&state, &name, Some(&existing_address), None);
        }
        RegisterCommitResult::PeerRouteHeld { registrations } => {
            close_arc_client(&state, client);
            return duplicate_route_response(&state, &name, None, Some(registrations));
        }
        RegisterCommitResult::Invalid { reason } => {
            close_arc_client(&state, client);
            eprintln!("[relay] Register rejected: name={name} reason={reason}");
            return (StatusCode::BAD_REQUEST, reason).into_response();
        }
    };

    broadcast_route_announce(&state, &entry);

    (
        StatusCode::CREATED,
        Json(serde_json::json!({"registered": name})),
    )
        .into_response()
}

fn close_arc_client(state: &RelayState, arc_client: Arc<IpcClient>) {
    state.clients.close(&arc_client);
}

fn close_client(client: crate::relay::client_lifecycle::ManagedClient) {
    drop(client);
}

/// Compare-remove an exact captured registration; no name-only Runtime teardown is accepted.
async fn handle_unregister(
    State(state): State<Arc<RelayState>>,
    Json(scope): Json<crate::client::RelayRegistrationScope>,
) -> Response {
    match RouteAuthority::new(&state).unregister_registration(&scope) {
        Ok(RegistrationWithdrawal::Removed {
            entry,
            removed_at,
            client,
        }) => {
            state.stop_upstream_control_if_owner_idle_for_route(&entry);
            if let Some(client) = client {
                close_arc_client(&state, client);
            }
            broadcast_route_withdraw(&state, &entry, removed_at);
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "removed"})),
            )
                .into_response()
        }
        Ok(RegistrationWithdrawal::AlreadyRemoved) => StatusCode::NO_CONTENT.into_response(),
        Ok(RegistrationWithdrawal::Superseded) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "superseded"})),
        )
            .into_response(),
        Ok(RegistrationWithdrawal::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            format!("invalid registration withdrawal: {error:?}"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ForceLeaveRequest {
    relay_id: String,
}

/// `POST /_admin/force-leave` — forget one unreachable relay in this relay's view.
async fn handle_admin_force_leave(
    State(state): State<Arc<RelayState>>,
    body: Result<Json<ForceLeaveRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    use crate::relay::route_table::ForceLeaveError;

    let Json(ForceLeaveRequest { relay_id }) = match body {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error.body_text()).into_response(),
    };
    match RouteAuthority::new(&state).force_leave_peer(&relay_id) {
        Ok(()) => {
            crate::relay::gossip::broadcast_force_leave(&state, relay_id.clone());
            (
                StatusCode::OK,
                Json(serde_json::json!({"force_left": relay_id})),
            )
                .into_response()
        }
        Err(ForceLeaveError::InvalidRelayId(reason)) => {
            (StatusCode::BAD_REQUEST, reason).into_response()
        }
        Err(ForceLeaveError::SelfRelay) => {
            (StatusCode::CONFLICT, "cannot force-leave this relay").into_response()
        }
        Err(ForceLeaveError::Alive) => {
            (StatusCode::CONFLICT, format!("relay {relay_id} is Alive")).into_response()
        }
        Err(ForceLeaveError::Unknown) => {
            (StatusCode::NOT_FOUND, format!("unknown relay {relay_id}")).into_response()
        }
    }
}

/// `POST /_admin/unregister` — explicit administrative name/server-id removal.
///
/// Body: `{"name": "grid", "server_id": "..."}`
/// Returns: 200 on success, 403 on owner mismatch, 404 on missing.
async fn handle_admin_unregister(
    State(state): State<Arc<RelayState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"name\"").into_response(),
    };
    let server_id = match body.get("server_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"server_id\"").into_response(),
    };
    eprintln!("[relay] Unregister request: name={name} server_id={server_id}");

    if let Err(ControlError::InvalidServerId { reason }) =
        RouteAuthority::new(&state).validate_server_id(&server_id)
    {
        eprintln!("[relay] Unregister rejected: name={name} server_id={server_id} reason={reason}");
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }

    match state.unregister_upstream(&name, &server_id) {
        crate::relay::state::UnregisterResult::Removed {
            entry,
            removed_at,
            client,
        } => {
            // Close old client asynchronously
            if let Some(arc_client) = client {
                close_arc_client(&state, arc_client);
            }

            broadcast_route_withdraw(&state, &entry, removed_at);
            eprintln!(
                "[relay] Unregister removed: name={} server_id={} removed_at={removed_at}",
                entry.name,
                entry.server_id.as_deref().unwrap_or("")
            );

            (
                StatusCode::OK,
                Json(serde_json::json!({"unregistered": name})),
            )
                .into_response()
        }
        crate::relay::state::UnregisterResult::AlreadyRemoved => {
            eprintln!("[relay] Unregister already removed: name={name} server_id={server_id}");
            (
                StatusCode::OK,
                Json(serde_json::json!({"unregistered": name})),
            )
                .into_response()
        }
        crate::relay::state::UnregisterResult::OwnerMismatch => {
            eprintln!(
                "[relay] Unregister rejected: name={name} server_id={server_id} reason=owner_mismatch"
            );
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "OwnerMismatch",
                    "name": name,
                })),
            )
                .into_response()
        }
        crate::relay::state::UnregisterResult::NotFound => {
            eprintln!(
                "[relay] Unregister rejected: name={name} server_id={server_id} reason=not_found"
            );
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Route name not registered: '{name}'")})),
            )
                .into_response()
        }
    }
}

/// `GET /_routes` — list all registered routes.
///
/// Intentionally omits `server_id` and `ipc_address` even for LOCAL routes:
/// this endpoint is reachable by anyone who can hit the relay's HTTP port, and
/// owner identity plus local UDS path are private to this relay.
async fn handle_list_routes(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let routes: Vec<serde_json::Value> = state.with_route_table(|table| {
        let conflicts = table.conflicts();
        table.list_routes().into_iter().map(|r| {
            let info = table.registration_info(&r);
            serde_json::json!({
                "name": r.name,
                "relay_id": r.relay_id,
                "relay_url": r.relay_url,
                "locality": match r.locality {
                    crate::relay::types::Locality::Local => "local",
                    crate::relay::types::Locality::Peer => "peer",
                },
                "crm_ns": r.crm_ns,
                "crm_name": r.crm_name,
                "crm_ver": r.crm_ver,
                "abi_hash": r.abi_hash,
                "signature_hash": r.signature_hash,
                "registered_at": r.registered_at,
                "relay_status": info.relay_status,
                "conflict": info.relay_status == PeerStatus::Alive && conflicts.contains_key(&r.name),
            })
        }).collect()
    });
    Json(serde_json::json!({"routes": routes}))
}

// -- Data-plane handlers --------------------------------------------------

/// `GET /health` — liveness check.
async fn handle_health(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let route_names = state.route_names();
    Json(serde_json::json!({
        "status": "ok",
        "routes": route_names,
    }))
}

/// `GET /_resolve/{name}` — resolve a complete expected CRM route contract.
async fn handle_resolve(
    Path(name): Path<String>,
    Query(query): Query<ResolveQuery>,
    State(state): State<Arc<RelayState>>,
    OptionalConnectInfo(remote_addr): OptionalConnectInfo,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !valid_route_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "InvalidRouteName",
                "message": "route name must be non-empty, control-character-free, and fit the wire route-name limit",
            })),
        )
            .into_response();
    }
    let namespace = match local_namespace_from_headers(&headers) {
        Ok(namespace) => namespace,
        Err(response) => return response,
    };
    let compatible_namespace = match namespace {
        Some(namespace) => namespace == state.endpoint_context().namespace_id(),
        None => state.is_default_endpoint_namespace(),
    };
    let expose_ipc_address =
        remote_addr.is_some_and(|addr| addr.ip().is_loopback()) && compatible_namespace;
    let expected_crm = match (
        &query.crm_ns,
        &query.crm_name,
        &query.crm_ver,
        &query.abi_hash,
        &query.signature_hash,
    ) {
        (Some(crm_ns), Some(crm_name), Some(crm_ver), Some(abi_hash), Some(signature_hash)) => {
            let expected = c2_contract::ExpectedRouteContract {
                route_name: name.clone(),
                crm_ns: crm_ns.clone(),
                crm_name: crm_name.clone(),
                crm_ver: crm_ver.clone(),
                abi_hash: abi_hash.clone(),
                signature_hash: signature_hash.clone(),
            };
            if let Err(err) = c2_contract::validate_expected_route_contract(&expected) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "InvalidCrmTag",
                        "message": err.to_string(),
                    })),
                )
                    .into_response();
            }
            expected
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "InvalidCrmTag",
                    "message": "crm_ns, crm_name, crm_ver, abi_hash, and signature_hash are required for relay resolve",
                })),
            )
                .into_response();
        }
    };
    let mut routes = match state.resolve_matching(&expected_crm) {
        Ok(Some(route)) => vec![route],
        Ok(None) => return resource_not_found_response(&name),
        Err(error) => return route_resolution_error_response(&name, error),
    };
    if !expose_ipc_address {
        for route in &mut routes {
            route.ipc_address = None;
            route.server_id = None;
            route.server_instance_id = None;
        }
    }
    with_local_namespace_response(&state, Json(routes).into_response())
}

/// `GET /_peers` — list known peer relays.
async fn handle_peers(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    Json(state.list_peers()).into_response()
}

struct PreparedUnknownInput {
    payload: SegmentedPayload,
    upload: PayloadBufferReservation,
    download: PayloadBufferReservation,
}

impl PreparedUnknownInput {
    fn len(&self) -> usize {
        self.payload.len()
    }
    #[cfg(test)]
    fn into_segments(self) -> Vec<BudgetedBuffer> {
        self.payload.into_segments()
    }
}

async fn cancel_stream_call(
    State(state): State<Arc<RelayState>>,
    Path(token): Path<String>,
) -> StatusCode {
    if state.forwarding.cancel_stream(&token) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn read_unknown_length_body(
    route_name: &str,
    body: Body,
    max_payload_size: u64,
    budget: &PayloadBufferBudget,
    cancelled: &CancellationToken,
    copy_cap: u64,
) -> ResponseResult<Option<PreparedUnknownInput>> {
    let limit = UNKNOWN_LENGTH_BODY_LIMIT_BYTES.min(max_payload_size);
    let mut payload = SegmentedPayload::new();
    let mut buffers = None;
    let mut source = body.into_data_stream();
    while let Some(next) = tokio::select! {
        biased;
        _ = cancelled.cancelled() => return Ok(None),
        next = source.next() => next,
    } {
        let chunk = next.map_err(|err| json_error_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error":"RequestBodyReadError", "route":route_name, "message":err.to_string()}),
            Some(budget),
        ))?;
        if chunk.is_empty() {
            continue;
        }
        let next_len = payload.len().checked_add(chunk.len()).ok_or_else(|| {
            payload_too_large_response(route_name, u64::MAX, max_payload_size, budget)
        })?;
        if next_len as u64 > max_payload_size {
            return Err(payload_too_large_response(
                route_name,
                next_len as u64,
                max_payload_size,
                budget,
            )
            .into());
        }
        if next_len as u64 > limit {
            return Err(unknown_length_too_large_response(route_name, limit, budget).into());
        }
        if buffers.is_none() {
            let plan = SendBufferPlan::new(
                budget.snapshot().limit_bytes,
                chunk.len() as u64,
                copy_cap,
                copy_cap,
            )
            .map_err(|error| forwarding_capacity_response(route_name, error.to_string()))?;
            let SendBuffers {
                input,
                upload,
                download,
            } = tokio::select! {
                biased;
                _ = cancelled.cancelled() => return Ok(None),
                reservation = reserve_send_buffers(budget, plan, None) => reservation,
            }
            .map_err(|error| forwarding_capacity_response(route_name, error.to_string()))?;
            payload.push_reserved_chunk(&chunk, input);
            buffers = Some((upload, download));
        } else {
            // Neither sending area has been allocated or used yet. Give
            // growing input only the unused part it needs, retaining both
            // directions' minimum progress unit; never wait on these charges.
            let snapshot = budget.snapshot();
            let mut deficit =
                (chunk.len() as u64).saturating_sub(snapshot.limit_bytes - snapshot.used_bytes);
            let (upload, download) = buffers
                .as_mut()
                .expect("first input admitted both directions");
            while deficit != 0 {
                let area = if upload.bytes() >= download.bytes() {
                    &mut *upload
                } else {
                    &mut *download
                };
                let refund = deficit.min(area.bytes().saturating_sub(1));
                if refund == 0 {
                    break;
                }
                drop(area.split_off(refund).expect("unused sending capacity"));
                deficit -= refund;
            }
            payload
                .push_chunk(&chunk, budget, None, "pre_dispatch")
                .await
                .map_err(|error| forwarding_capacity_response(route_name, error.to_string()))?;
        }
    }
    let (mut upload, mut download) = match buffers {
        Some(buffers) => buffers,
        None => {
            let plan = SendBufferPlan::new(budget.snapshot().limit_bytes, 0, copy_cap, copy_cap)
                .map_err(|error| forwarding_capacity_response(route_name, error.to_string()))?;
            let SendBuffers {
                input,
                upload,
                download,
            } = tokio::select! {
                biased;
                _ = cancelled.cancelled() => return Ok(None),
                reservation = reserve_send_buffers(budget, plan, None) => reservation,
            }
            .map_err(|error| forwarding_capacity_response(route_name, error.to_string()))?;
            drop(input);
            (upload, download)
        }
    };
    // The full input length is now known. Widen the upload copy window when
    // capacity permits, without making that optimization part of admission.
    // Both directions already own at least one byte for forward progress.
    let additional = copy_cap
        .min(payload.len() as u64)
        .saturating_sub(upload.bytes());
    if additional != 0 {
        let snapshot = budget.snapshot();
        let refund = additional
            .saturating_sub(snapshot.limit_bytes.saturating_sub(snapshot.used_bytes))
            .min(download.bytes().saturating_sub(1));
        drop(
            download
                .split_off(refund)
                .expect("unused receiving capacity"),
        );
        let snapshot = budget.snapshot();
        let growth = additional.min(snapshot.limit_bytes.saturating_sub(snapshot.used_bytes));
        if growth != 0 {
            match upload.try_grow(growth) {
                Ok(())
                | Err(
                    PayloadBufferError::TooLarge { .. }
                    | PayloadBufferError::InsufficientCapacity { .. },
                ) => {}
                Err(error) => {
                    return Err(forwarding_capacity_response(route_name, error.to_string()).into());
                }
            }
        }
    }
    Ok(Some(PreparedUnknownInput {
        payload,
        upload,
        download,
    }))
}

// The declared N bytes have one reservation. Each yielded Bytes owns a share
// of it so IPC may stop polling at exactly N without retaining the charge in
// the stream closure for the entire response wait.
struct ReservedIncomingChunk {
    bytes: Bytes,
    _reservation: Arc<PayloadBufferReservation>,
}

impl AsRef<[u8]> for ReservedIncomingChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

fn known_length_body_stream(
    body: Body,
    content_length: u64,
    reservation: PayloadBufferReservation,
) -> impl futures::Stream<Item = Result<Bytes, axum::Error>> + Send {
    let mut remaining = content_length;
    let mut reservation = (content_length != 0).then(|| Arc::new(reservation));
    body.into_data_stream().map(move |chunk| {
        let bytes = chunk?;
        let size = u64::try_from(bytes.len()).map_err(|_| {
            axum::Error::new(std::io::Error::other("request chunk length overflow"))
        })?;
        if size > remaining {
            return Err(axum::Error::new(std::io::Error::other(
                "request body exceeds Content-Length",
            )));
        }
        remaining -= size;
        let Some(owner) = reservation.as_ref().cloned() else {
            return Ok(bytes);
        };
        let bytes = Bytes::from_owner(ReservedIncomingChunk {
            bytes,
            _reservation: owner,
        });
        if remaining == 0 {
            reservation.take();
        }
        Ok(bytes)
    })
}

/// `GET /_probe/{name}` — verify that a route's local upstream is reachable.
async fn handle_probe(
    Path(route_name): Path<String>,
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
) -> Response {
    let response = handle_probe_in_context(route_name, state.clone(), headers).await;
    with_local_namespace_response(&state, response)
}

async fn handle_probe_in_context(
    route_name: String,
    state: Arc<RelayState>,
    headers: HeaderMap,
) -> Response {
    let expected_crm = match expected_crm_from_headers(&route_name, &headers, None) {
        Ok(expected) => expected,
        Err(response) => return response.into_response(),
    };
    if let Err(error @ RouteResolutionError::Conflict(_)) = state.resolve_matching(&expected_crm) {
        return route_resolution_error_response(&route_name, error);
    }
    let advertised_route =
        match validate_expected_crm_for_route(&state, &route_name, &expected_crm, None) {
            Ok(route) => route,
            Err(response) => return response.into_response(),
        };
    #[cfg(test)]
    run_data_plane_after_precheck_hook(&state, &route_name);
    match acquire_request_client_for_route(state, &advertised_route).await {
        RequestClient::Ready(ready) => {
            let ReadyRequestClient { lease, .. } = *ready;
            drop(lease);
            StatusCode::OK.into_response()
        }
        RequestClient::WatchUnavailable { route, reason } => {
            route_watch_unavailable_response(&route.name, reason)
        }
        RequestClient::NotFound => resource_not_found_response(&route_name),
        RequestClient::RouteError(error) => ipc_route_error_response(&route_name, &error, None),
        RequestClient::Unreachable => resource_unavailable_response(
            &route_name,
            format!("relay upstream unavailable: {route_name}"),
        ),
    }
}

/// `POST /{route_name}/{method_name}` — relay CRM call to upstream.
///
/// If the upstream was evicted by the idle sweeper, attempts a lazy
/// reconnect before returning 502.
async fn call_handler(
    State(state): State<Arc<RelayState>>,
    Path((route_name, method_name)): Path<(String, String)>,
    Extension(declared_length): Extension<DeclaredBodyLength>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let budget = state.forwarding.payload_budget();
    let expected_crm = match expected_crm_from_headers(&route_name, &headers, Some(&budget)) {
        Ok(expected) => expected,
        Err(response) => return response.into_response(),
    };
    let advertised_route =
        match validate_expected_crm_for_route(&state, &route_name, &expected_crm, Some(&budget)) {
            Ok(route) => route,
            Err(response) => return response.into_response(),
        };
    let content_length = match declared_length.0 {
        Ok(value) => value,
        Err(message) => {
            return json_error_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "InvalidContentLength", "message": message}),
                Some(&budget),
            );
        }
    };
    if let Some(content_length) = content_length
        && content_length > advertised_route.max_payload_size
    {
        return payload_too_large_response(
            &route_name,
            content_length,
            advertised_route.max_payload_size,
            &budget,
        );
    }
    #[cfg(test)]
    run_data_plane_after_precheck_hook(&state, &route_name);
    let server_stream = headers
        .get(header::ACCEPT)
        .is_some_and(|value| value.as_bytes() == crate::stream_wire::MEDIA_TYPE.as_bytes());
    let work_state = state.clone();
    let work_route_name = route_name.clone();
    // Keep the body in the HTTP handler until admission succeeds. A rejected
    // spawn leaves the unread transport body at the router boundary.
    let (body_tx, body_rx) = tokio::sync::oneshot::channel();
    let work = move |permit, cancelled| async move {
        let body = body_rx
            .await
            .expect("admitted forwarding input is sent before waiting");
        forward_call(
            work_state,
            work_route_name,
            method_name,
            advertised_route,
            content_length,
            body,
            permit,
            cancelled,
            server_stream,
        )
        .await
    };
    let waiter = match state.forwarding.spawn(work) {
        Ok(waiter) => waiter,
        Err(error) => {
            return forwarding_capacity_response(&route_name, error.to_string());
        }
    };
    // No await separates admission from this ownership handoff.
    body_tx
        .send(body)
        .expect("admitted forwarding task retains its input receiver");
    match waiter.await {
        Ok(response) => response,
        Err(_) => resource_unavailable_response_budgeted(
            &route_name,
            "relay forwarding transaction failed",
            "dispatch_uncertain",
            &budget,
        ),
    }
}

async fn finish_early_rejection(
    expect: bool,
    content_length: Option<u64>,
    mut body: Body,
    response: Response,
) -> Response {
    // Expect refusals must never poll Incoming: doing so sends 100 Continue.
    // Only a valid declared length permits disposal, including one above the
    // route's payload limit. Unknown or invalid lengths are never drained.
    if !expect && let Some(mut remaining) = content_length {
        // Hyper 1.x only tries one read after an unread Incoming is dropped,
        // then closes the read side. An eager sender can lose the final error
        // to a socket abort. Keep the receiver alive through bounded EOF instead.
        // This is transport disposal, with no allocation, decoding, IPC acquire
        // or forwarding permit. It belongs to this handler, never a spawned task.
        let _ = tokio::time::timeout(REJECTED_BODY_DRAIN_TIMEOUT, async {
            use axum::body::HttpBody;
            while let Some(frame) =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
            {
                let Ok(frame) = frame else { break };
                if let Some(data) = frame.data_ref() {
                    let Ok(length) = u64::try_from(data.len()) else {
                        break;
                    };
                    let Some(rest) = remaining.checked_sub(length) else {
                        break;
                    };
                    remaining = rest;
                }
                drop(frame);
                // Also bound ready-only streams of empty frames/trailers: the
                // timeout must get polled and other connections must progress.
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    // A source fault, excess bytes or timeout cannot replace the original refusal.
    response
}

fn forwarding_capacity_response(_route_name: &str, _message: impl Into<String>) -> Response {
    // This fixed control reply has no owned payload buffer. It must remain
    // deliverable even when B is zero or a legitimate call holds all of B.
    const BODY: &[u8] = br#"{"version":1,"code":717,"name":"CallCapacityExceeded","message":"relay forwarding capacity exceeded","details":{"dispatch_phase":"pre_dispatch"}}"#;
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(Bytes::from_static(BODY)),
    )
        .into_response()
}

fn uncertain_error_control_response() -> Response {
    const BODY: &[u8] = br#"{"version":1,"code":702,"name":"ResourceUnavailable","message":"relay error response exceeds payload budget","details":{"dispatch_phase":"dispatch_uncertain"}}"#;
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(Bytes::from_static(BODY)),
    )
        .into_response()
}

// This future belongs to the bounded native domain, never to the HTTP handler.
async fn forward_call(
    state: Arc<RelayState>,
    route_name: String,
    method_name: String,
    advertised_route: RouteEntry,
    content_length: Option<u64>,
    body: Body,
    permit: c2_mem::RetentionPermit,
    cancelled: CancellationToken,
    server_stream: bool,
) -> Response {
    let budget = state.forwarding.payload_budget();
    let copy_cap = state
        .config()
        .remote_payload_chunk_size
        .min(state.config().upstream_ipc.chunk_size)
        .max(1);
    let shutdown = state.forwarding.shutdown_token();
    let (input_len, chunks, upload, download): (
        _,
        futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>>,
        _,
        _,
    ) = match content_length {
        Some(input_len) => {
            let plan = match SendBufferPlan::new(
                budget.snapshot().limit_bytes,
                input_len,
                copy_cap,
                copy_cap,
            ) {
                Ok(plan) => plan,
                Err(error) => return forwarding_capacity_response(&route_name, error.to_string()),
            };
            let buffers = tokio::select! {
                biased;
                _ = cancelled.cancelled() => return StatusCode::NO_CONTENT.into_response(),
                _ = shutdown.cancelled(), if server_stream => return relay_shutdown_refusal(&route_name, &budget),
                buffers = reserve_send_buffers(&budget, plan, None) => buffers,
            };
            let SendBuffers {
                input,
                upload,
                download,
            } = match buffers {
                Ok(buffers) => buffers,
                Err(error) => return forwarding_capacity_response(&route_name, error.to_string()),
            };
            let chunks = known_length_body_stream(body, input_len, input)
                .map(|part| part.map_err(std::io::Error::other))
                .boxed();
            (input_len, chunks, upload, download)
        }
        None => {
            let prepared = tokio::select! {
                biased;
                _ = shutdown.cancelled(), if server_stream => return relay_shutdown_refusal(&route_name, &budget),
                prepared = read_unknown_length_body(&route_name, body, advertised_route.max_payload_size, &budget, &cancelled, copy_cap) => prepared,
            };
            let prepared = match prepared {
                Ok(Some(prepared)) => prepared,
                Ok(None) => return StatusCode::NO_CONTENT.into_response(),
                Err(response) => {
                    #[cfg(test)]
                    if response.0.status() == StatusCode::BAD_REQUEST {
                        state.forwarding.record_source_fault(true);
                    }
                    return response.into_response();
                }
            };
            let input_len = prepared.len() as u64;
            let chunks = futures::stream::iter(
                prepared
                    .payload
                    .into_segments()
                    .into_iter()
                    .map(|part| Ok::<_, std::io::Error>(Bytes::from_owner(part))),
            )
            .boxed();
            (input_len, chunks, prepared.upload, prepared.download)
        }
    };
    if cancelled.is_cancelled() {
        return StatusCode::NO_CONTENT.into_response();
    }
    if server_stream && shutdown.is_cancelled() {
        return relay_shutdown_refusal(&route_name, &budget);
    }
    let (lease, binding, current_route) =
        match acquire_request_client_for_route(state.clone(), &advertised_route).await {
            RequestClient::Ready(ready) => {
                let ReadyRequestClient {
                    lease,
                    binding,
                    route,
                } = *ready;
                (lease, binding, route)
            }
            RequestClient::WatchUnavailable { route, reason } => {
                return route_watch_unavailable_response_budgeted(&route.name, reason, &budget);
            }
            RequestClient::NotFound => {
                return resource_not_found_response_budgeted(&route_name, &budget);
            }
            RequestClient::RouteError(error) => {
                return ipc_route_error_response(&route_name, &error, Some(&budget));
            }
            RequestClient::Unreachable => {
                return resource_unavailable_response_budgeted(
                    &route_name,
                    format!("relay upstream unavailable: {route_name}"),
                    "pre_dispatch",
                    &budget,
                );
            }
        };
    if input_len > current_route.max_payload_size {
        return payload_too_large_response(
            &route_name,
            input_len,
            current_route.max_payload_size,
            &budget,
        );
    }
    let client = lease.client();
    if cancelled.is_cancelled() {
        return StatusCode::NO_CONTENT.into_response();
    }
    if server_stream && shutdown.is_cancelled() {
        return relay_shutdown_refusal(&route_name, &budget);
    }

    enum ForwardedResponse {
        Unary(c2_ipc::ResponseData),
        Stream(c2_ipc::IpcForwardStream),
    }
    let width = usize::try_from(download.bytes()).expect("local copy window is addressable");
    let call_result = if server_stream {
        match client
            .forward_stream_bound_sized_phased(&binding, &method_name, input_len, chunks, upload)
            .await
        {
            Ok(stream) => finish_stream_upload(stream, &cancelled, &shutdown)
                .await
                .map(ForwardedResponse::Stream),
            Err(error) => Err(error),
        }
    } else {
        client
            .forward_unary_bound_sized_phased(&binding, &method_name, input_len, chunks, upload)
            .await
            .map(ForwardedResponse::Unary)
    };

    match call_result {
        Ok(ForwardedResponse::Unary(result)) => super::streaming::unary_response(
            result,
            StatusCode::OK,
            download,
            budget.clone(),
            width,
        ),
        Ok(ForwardedResponse::Stream(stream)) => match super::streaming::stream_response(
            state.clone(),
            stream,
            lease,
            permit,
            download,
            width,
        ) {
            Ok(response) => response,
            Err(error) => resource_unavailable_response_budgeted(
                &route_name,
                format!("cannot create call cancellation token: {error}"),
                "dispatch_uncertain",
                &budget,
            ),
        },
        Err(error) => {
            let phase = error.phase();
            let error = error.into_source();
            #[cfg(test)]
            if matches!(&error, c2_ipc::IpcError::Io(source) if source.to_string().contains("request body stream error"))
            {
                match phase {
                    c2_ipc::TransportPhase::PreDispatch => {
                        state.forwarding.record_source_fault(false)
                    }
                    c2_ipc::TransportPhase::DispatchUncertain => {
                        state.forwarding.record_published_source_fault()
                    }
                }
            }
            match error {
                c2_ipc::IpcError::CallRejected(error) => {
                    let error = c2_ipc::IpcError::CallRejected(error);
                    if let Some(reason) = semantic_withdrawal_reason(&error) {
                        drop(lease);
                        remove_unreachable_route_for_error(
                            &state,
                            &advertised_route,
                            &error,
                            reason,
                        );
                    }
                    ipc_route_error_response(&route_name, &error, Some(&budget))
                }
                c2_ipc::IpcError::LocalCallRejected(error)
                    if matches!(phase, c2_ipc::TransportPhase::PreDispatch)
                        && error.code == c2_error::ErrorCode::CallCapacityExceeded =>
                {
                    forwarding_capacity_response(&route_name, error.message)
                }
                c2_ipc::IpcError::CrmError(err_bytes) => super::streaming::unary_response(
                    c2_ipc::ResponseData::Inline(err_bytes),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    download,
                    budget.clone(),
                    width,
                ),
                error if matches!(phase, c2_ipc::TransportPhase::PreDispatch) => {
                    if let Some(reason) = semantic_withdrawal_reason(&error) {
                        drop(lease);
                        remove_unreachable_route_for_error(
                            &state,
                            &advertised_route,
                            &error,
                            reason,
                        );
                    }
                    ipc_route_error_response(&route_name, &error, Some(&budget))
                }
                error
                    if matches!(
                        &error,
                        c2_ipc::IpcError::ResponseFailure(source)
                            if matches!(source.as_ref(), c2_ipc::IpcError::Chunk(c2_ipc::client::ChunkError::Capacity(_)))
                    ) && matches!(phase, c2_ipc::TransportPhase::DispatchUncertain) =>
                {
                    // Reply admission rejects only this RID; the receiver
                    // keeps serving other calls on the healthy connection.
                    // Dispatch already happened, so replay is still unsafe.
                    resource_unavailable_response_budgeted(
                        &route_name,
                        format!("relay error after possible dispatch: {error}"),
                        "dispatch_uncertain",
                        &budget,
                    )
                }
                error => {
                    // The request may already have reached the service. Evict
                    // the failed transport, but never tell the client that
                    // replay is safe.
                    if let Some(old_client) = lease.evict_current_client() {
                        // The Relay native lifecycle retains this owner and
                        // observes confirmed cleanup beyond transaction completion.
                        state.clients.close(&old_client);
                    }
                    resource_unavailable_response_budgeted(
                        &route_name,
                        format!("relay error after possible dispatch: {error}"),
                        "dispatch_uncertain",
                        &budget,
                    )
                }
            }
        }
    }
}

async fn finish_stream_upload(
    mut stream: c2_ipc::IpcForwardStream,
    cancelled: &CancellationToken,
    shutdown: &CancellationToken,
) -> Result<c2_ipc::IpcForwardStream, c2_ipc::IpcCallError> {
    let control = stream.cancellation();
    let uploaded = {
        let upload = stream.wait_uploaded();
        tokio::pin!(upload);
        let mut cancellation_sent = false;
        let mut shutdown_sent = false;
        loop {
            tokio::select! {
                biased;
                _ = cancelled.cancelled(), if !cancellation_sent => {
                    cancellation_sent = true;
                    let _ = control.cancel(c2_wire::rpc::CancelCause::Consumer);
                }
                _ = shutdown.cancelled(), if !shutdown_sent => {
                    shutdown_sent = true;
                    let _ = control.cancel(c2_wire::rpc::CancelCause::Lifecycle);
                }
                result = &mut upload => break result,
            }
        }
    };
    match uploaded {
        Ok(()) => Ok(stream),
        Err(error) => {
            let _ = control.cancel(c2_wire::rpc::CancelCause::Consumer);
            let _ = stream.completion().await;
            Err(error)
        }
    }
}

fn relay_shutdown_refusal(route: &str, budget: &PayloadBufferBudget) -> Response {
    ipc_route_error_response(
        route,
        &c2_ipc::IpcError::CallRejected(c2_error::C2Error::new(
            c2_error::ErrorCode::CallCancelled,
            "server stream cancelled by relay shutdown",
        )),
        Some(budget),
    )
}

async fn acquire_request_client_for_route(
    state: Arc<RelayState>,
    route: &RouteEntry,
) -> RequestClient {
    let route_name = route.name.clone();
    match state.acquire_upstream_for_route(route).await {
        Ok((lease, binding, route)) => RequestClient::Ready(Box::new(ReadyRequestClient {
            lease,
            binding,
            route,
        })),
        Err(UpstreamAcquireError::NotFound) => RequestClient::NotFound,
        Err(UpstreamAcquireError::WatchUnavailable { route, reason }) => {
            RequestClient::WatchUnavailable {
                route: Box::new(route),
                reason,
            }
        }
        Err(UpstreamAcquireError::Unreachable {
            route,
            address,
            error,
        }) => {
            let error_kind = upstream_acquire_error_kind(&error);
            eprintln!(
                "[relay] Failed to acquire upstream '{route_name}' at {address} ({error_kind}): {error}"
            );
            if let Some(reason) = semantic_withdrawal_reason(&error) {
                remove_unreachable_route_for_error(&state, &route, &error, reason);
            }
            match error {
                error @ (c2_ipc::IpcError::RouteNotFound(_)
                | c2_ipc::IpcError::RouteRemoved { .. }
                | c2_ipc::IpcError::RouteClosed { .. }
                | c2_ipc::IpcError::ContractMismatch(_)
                | c2_ipc::IpcError::IdentityMismatch { .. }
                | c2_ipc::IpcError::Protocol(_)
                | c2_ipc::IpcError::ProtocolVersionMismatch { .. }
                | c2_ipc::IpcError::MethodNotFound { .. }) => RequestClient::RouteError(error),
                _ => RequestClient::Unreachable,
            }
        }
    }
}

fn ipc_route_error_response(
    route_name: &str,
    error: &c2_ipc::IpcError,
    budget: Option<&PayloadBufferBudget>,
) -> Response {
    use c2_error::{C2Error, ErrorCode};
    let semantic_error_response =
        |status, error: C2Error| json_error_response(status, error.envelope(), budget);

    match error {
        c2_ipc::IpcError::CallRejected(error) => {
            let status = match error.code {
                ErrorCode::ResourceNotFound => StatusCode::NOT_FOUND,
                ErrorCode::UnsupportedCallMode => StatusCode::NOT_ACCEPTABLE,
                ErrorCode::ResourceRemoved => StatusCode::GONE,
                ErrorCode::ContractMismatch | ErrorCode::IdentityMismatch => StatusCode::CONFLICT,
                ErrorCode::ResourceClosed
                | ErrorCode::ResourceUnavailable
                | ErrorCode::CallCapacityExceeded => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::BAD_GATEWAY,
            };
            let mut projected = error.clone();
            projected
                .details
                .insert("dispatch_phase".into(), "pre_dispatch".into());
            semantic_error_response(status, projected)
        }
        c2_ipc::IpcError::LocalCallRejected(error) => {
            semantic_error_response(StatusCode::SERVICE_UNAVAILABLE, error.clone())
        }
        c2_ipc::IpcError::RouteNotFound(route) => semantic_error_response(
            StatusCode::NOT_FOUND,
            C2Error::new(
                ErrorCode::ResourceNotFound,
                format!("route not found: {route}"),
            )
            .with_details(BTreeMap::from([("route".to_string(), route.clone())])),
        ),
        c2_ipc::IpcError::RouteRemoved { route_name } => semantic_error_response(
            StatusCode::GONE,
            C2Error::new(
                ErrorCode::ResourceRemoved,
                format!("route removed: {route_name}"),
            )
            .with_details(BTreeMap::from([
                ("route".into(), route_name.clone()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
            ])),
        ),
        c2_ipc::IpcError::RouteClosed { route_name, reason } => semantic_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            C2Error::new(
                ErrorCode::ResourceClosed,
                format!("route closed: {route_name}"),
            )
            .with_details(BTreeMap::from([
                ("route".into(), route_name.clone()),
                ("reason".into(), reason.clone()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
            ])),
        ),
        c2_ipc::IpcError::ContractMismatch(message) => semantic_error_response(
            StatusCode::CONFLICT,
            C2Error::new(ErrorCode::ContractMismatch, message.clone()).with_details(
                BTreeMap::from([
                    ("dispatch_phase".to_string(), "pre_dispatch".to_string()),
                    ("route".to_string(), route_name.to_string()),
                    ("transport".to_string(), "ipc".to_string()),
                ]),
            ),
        ),
        c2_ipc::IpcError::IdentityMismatch {
            expected_server_id,
            expected_server_instance_id,
            actual_server_id,
            actual_server_instance_id,
        } => semantic_error_response(
            StatusCode::CONFLICT,
            C2Error::new(ErrorCode::IdentityMismatch, "IPC server identity mismatch").with_details(
                BTreeMap::from([
                    ("actual_server_id".to_string(), actual_server_id.clone()),
                    (
                        "actual_server_instance_id".to_string(),
                        actual_server_instance_id.clone(),
                    ),
                    ("expected_server_id".to_string(), expected_server_id.clone()),
                    (
                        "expected_server_instance_id".to_string(),
                        expected_server_instance_id.clone(),
                    ),
                    ("route".to_string(), route_name.to_string()),
                ]),
            ),
        ),
        c2_ipc::IpcError::MethodNotFound {
            route_name,
            method_name,
        } => semantic_error_response(
            StatusCode::BAD_GATEWAY,
            C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("method is not present in the acquired route: {method_name}"),
            )
            .with_details(BTreeMap::from([
                ("method".to_string(), method_name.clone()),
                ("route".to_string(), route_name.clone()),
            ])),
        ),
        c2_ipc::IpcError::ProtocolVersionMismatch { local, peer } => semantic_error_response(
            StatusCode::BAD_GATEWAY,
            C2Error::new(ErrorCode::ProtocolViolation, error.to_string()).with_details(
                BTreeMap::from([
                    ("route".to_string(), route_name.to_string()),
                    ("transport".to_string(), "ipc".to_string()),
                    ("local_protocol_version".to_string(), local.to_string()),
                    ("peer_protocol_version".to_string(), peer.to_string()),
                ]),
            ),
        ),
        c2_ipc::IpcError::Protocol(message) => semantic_error_response(
            StatusCode::BAD_GATEWAY,
            C2Error::new(
                ErrorCode::ProtocolViolation,
                "IPC protocol validation failed",
            )
            .with_details(BTreeMap::from([
                ("reason".to_string(), message.clone()),
                ("route".to_string(), route_name.to_string()),
                ("transport".to_string(), "ipc".to_string()),
            ])),
        ),
        error => match budget {
            Some(budget) => resource_unavailable_response_budgeted(
                route_name,
                format!("relay upstream unavailable before dispatch: {error}"),
                "pre_dispatch",
                budget,
            ),
            None => resource_unavailable_response(
                route_name,
                format!("relay upstream unavailable before dispatch: {error}"),
            ),
        },
    }
}

fn upstream_acquire_error_kind(error: &c2_ipc::IpcError) -> &'static str {
    match error {
        c2_ipc::IpcError::Io(_) | c2_ipc::IpcError::Connect(_) => "io",
        c2_ipc::IpcError::Config(_) => "config",
        c2_ipc::IpcError::Decode(_) => "decode",
        c2_ipc::IpcError::Protocol(_) => "protocol",
        c2_ipc::IpcError::ProtocolVersionMismatch { .. } => "protocol-version-mismatch",
        c2_ipc::IpcError::IdentityMismatch { .. } => "identity-mismatch",
        c2_ipc::IpcError::ContractMismatch(_) => "contract-mismatch",
        c2_ipc::IpcError::RouteNotFound(_) => "route-missing",
        c2_ipc::IpcError::RouteRemoved { .. } => "route-removed",
        c2_ipc::IpcError::RouteClosed { .. } => "route-closed",
        c2_ipc::IpcError::MethodNotFound { .. } => "method-missing",
        c2_ipc::IpcError::Shm(_) => "shm",
        c2_ipc::IpcError::Chunk(_) => "chunk",
        c2_ipc::IpcError::CrmError(_) => "crm-error",
        c2_ipc::IpcError::CallRejected(_) => "call-rejected",
        c2_ipc::IpcError::LocalCallRejected(_) => "local-call-rejected",
        c2_ipc::IpcError::Closed => "closed",
        c2_ipc::IpcError::Pool(_) => "pool",
        c2_ipc::IpcError::PoolRetired => "pool-retired",
        c2_ipc::IpcError::ResponseFailure(_) => "response-failure",
    }
}

fn semantic_withdrawal_reason(error: &c2_ipc::IpcError) -> Option<&'static str> {
    match error {
        c2_ipc::IpcError::CallRejected(error) => match error.code {
            c2_error::ErrorCode::IdentityMismatch => Some("identity-mismatch"),
            c2_error::ErrorCode::ContractMismatch => Some("contract-mismatch"),
            c2_error::ErrorCode::ResourceNotFound => Some("route-missing"),
            c2_error::ErrorCode::ResourceRemoved => Some("route-removed"),
            c2_error::ErrorCode::ResourceClosed => Some("route-closed"),
            _ => None,
        },
        c2_ipc::IpcError::IdentityMismatch { .. } => Some("identity-mismatch"),
        c2_ipc::IpcError::ContractMismatch(_) => Some("contract-mismatch"),
        c2_ipc::IpcError::RouteNotFound(_) => Some("route-missing"),
        c2_ipc::IpcError::RouteRemoved { .. } => Some("route-removed"),
        c2_ipc::IpcError::RouteClosed { .. } => Some("route-closed"),
        _ => None,
    }
}

fn remove_unreachable_route_for_error(
    state: &Arc<RelayState>,
    route: &RouteEntry,
    error: &c2_ipc::IpcError,
    reason: &'static str,
) {
    if matches!(error, c2_ipc::IpcError::IdentityMismatch { .. })
        || matches!(error, c2_ipc::IpcError::CallRejected(rejection)
            if rejection.code == c2_error::ErrorCode::IdentityMismatch)
    {
        remove_unreachable_owner_endpoint_routes(state, route, reason);
    } else {
        remove_unreachable_route(state, route, reason);
    }
}

fn remove_unreachable_owner_endpoint_routes(
    state: &Arc<RelayState>,
    route: &RouteEntry,
    reason: &'static str,
) {
    let Some(endpoint) = UpstreamEndpointKey::from_route(route) else {
        remove_unreachable_route(state, route, reason);
        return;
    };
    let routes = state.local_routes_for_owner(&endpoint);
    if routes.is_empty() {
        remove_unreachable_route(state, route, reason);
        return;
    }
    for route in routes {
        remove_unreachable_route(state, &route, reason);
    }
}

fn remove_unreachable_route(state: &Arc<RelayState>, route: &RouteEntry, reason: &'static str) {
    if let Some((entry, removed_at, client)) =
        state.remove_unreachable_local_upstream_if_matches(route)
    {
        state.stop_upstream_control_if_owner_idle_for_route(&entry);
        if let Some(client) = client {
            close_arc_client(&state, client);
        }
        eprintln!(
            "{}",
            unreachable_route_removal_log_line(&entry, removed_at, reason)
        );
        broadcast_route_withdraw(state, &entry, removed_at);
    }
}

fn unreachable_route_removal_log_line(
    entry: &RouteEntry,
    removed_at: f64,
    reason: &'static str,
) -> String {
    format!(
        "[relay] Removed unreachable route: name={} server_id={} server_instance_id={} address={} removed_at={} reason={reason}",
        entry.name,
        entry.server_id.as_deref().unwrap_or(""),
        entry.server_instance_id.as_deref().unwrap_or(""),
        entry.ipc_address.as_deref().unwrap_or(""),
        removed_at,
    )
}

/// `POST /_echo` — echo endpoint for benchmarking the relay itself.
///
/// Returns the request body immediately with no IPC round-trip.
async fn echo_handler(body: Bytes) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use c2_ipc::ClientIpcConfig;
    use c2_server::{
        ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits,
        Server, ServerIdentity, ServerIpcConfig,
    };
    use futures::StreamExt;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::relay::test_support::{
        register_echo_route, reserve_echo_route, shutdown_live_server, start_live_server,
        start_live_server_with_identity_and_contracts, start_live_server_with_routes,
        test_state_for_client,
    };
    use crate::relay::types::{Locality, RouteEntry, RouteInfo};

    const TEST_CRM_NS: &str = "test.relay";
    const TEST_CRM_NAME: &str = "RelayGrid";
    const TEST_CRM_VER: &str = "0.1.0";
    const TEST_ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_SIGNATURE_HASH: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    #[cfg(unix)]
    #[tokio::test]
    async fn register_connect_does_not_retry_protocol_version_mismatch() {
        use std::os::unix::fs::DirBuilderExt;
        use tokio::io::AsyncReadExt;

        struct TestRoot(std::path::PathBuf);
        impl Drop for TestRoot {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }

        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = TestRoot(std::path::PathBuf::from(format!(
            "/tmp/c2-m4-{}-{suffix:x}",
            std::process::id(),
        )));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let context = c2_config::LocalEndpointContext::with_unix_root(&root.0).unwrap();
        let address = "ipc://version-mismatch";
        let mut listener =
            c2_local::LocalListener::bind(&context.endpoint(address).unwrap()).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            tokio::pin!(stopped);
            let mut accepted = 0;
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    stream = listener.accept() => {
                        let mut stream = stream.unwrap();
                        accepted += 1;
                        let mut hello = [0; 25];
                        stream.read_exact(&mut hello).await.unwrap();
                        let mut response = c2_wire::handshake::encode_version_preamble(
                            c2_wire::handshake::VersionPreambleKind::VersionMismatch,
                        );
                        response[21..].copy_from_slice(
                            &(c2_wire::handshake::HANDSHAKE_VERSION - 1).to_le_bytes(),
                        );
                        stream.write_all(&response).await.unwrap();
                    }
                }
            }
            listener.close();
            accepted
        });
        let state = namespace_test_state(context.clone());
        let clients = state.clients.clone();
        let observer = {
            let clients = clients.clone();
            tokio::spawn(async move { clients.run().await })
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            LOCAL_ENDPOINT_NAMESPACE_HEADER,
            context.namespace_id().parse().unwrap(),
        );
        let response = handle_register(
            State(state),
            headers,
            Json(serde_json::json!({
                "name": "grid",
                "server_id": "version-server",
                "server_instance_id": "version-instance",
                "address": address,
                "max_payload_size": 1024,
            })),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        stop.send(()).unwrap();
        let accepted = peer.await.unwrap();
        clients.shutdown().await;
        observer.await.unwrap();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("protocol version mismatch")
        );
        assert_eq!(accepted, 1, "version mismatch must terminate registration");
    }

    #[test]
    fn semantic_withdrawal_reason_separates_authority_from_transport() {
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::IdentityMismatch {
                expected_server_id: "expected".into(),
                expected_server_instance_id: "expected-instance".into(),
                actual_server_id: "actual".into(),
                actual_server_instance_id: "actual-instance".into(),
            }),
            Some("identity-mismatch")
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::ContractMismatch(
                "wrong contract".into()
            )),
            Some("contract-mismatch")
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::RouteNotFound("grid".into())),
            Some("route-missing")
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::RouteRemoved {
                route_name: "grid".into(),
            }),
            Some("route-removed")
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::RouteClosed {
                route_name: "grid".into(),
                reason: "shutdown".into(),
            }),
            Some("route-closed")
        );

        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::Io(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            ))),
            None
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::Io(std::io::Error::from(
                std::io::ErrorKind::TimedOut,
            ))),
            None
        );
        assert_eq!(
            semantic_withdrawal_reason(&c2_ipc::IpcError::CallRejected(c2_error::C2Error::new(
                c2_error::ErrorCode::ResourceRemoved,
                "removed before callback",
            ))),
            Some("route-removed")
        );
    }

    #[tokio::test]
    async fn ipc_route_errors_preserve_exact_relay_semantics() {
        use c2_error::ErrorCode;

        let cases = [
            (
                c2_ipc::IpcError::RouteNotFound("grid".into()),
                StatusCode::NOT_FOUND,
                ErrorCode::ResourceNotFound,
            ),
            (
                c2_ipc::IpcError::RouteRemoved {
                    route_name: "grid".into(),
                },
                StatusCode::GONE,
                ErrorCode::ResourceRemoved,
            ),
            (
                c2_ipc::IpcError::RouteClosed {
                    route_name: "grid".into(),
                    reason: "shutdown".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ResourceClosed,
            ),
            (
                c2_ipc::IpcError::ContractMismatch("wrong contract".into()),
                StatusCode::CONFLICT,
                ErrorCode::ContractMismatch,
            ),
            (
                c2_ipc::IpcError::IdentityMismatch {
                    expected_server_id: "expected".into(),
                    expected_server_instance_id: "expected-instance".into(),
                    actual_server_id: "actual".into(),
                    actual_server_instance_id: "actual-instance".into(),
                },
                StatusCode::CONFLICT,
                ErrorCode::IdentityMismatch,
            ),
            (
                c2_ipc::IpcError::MethodNotFound {
                    route_name: "grid".into(),
                    method_name: "missing".into(),
                },
                StatusCode::BAD_GATEWAY,
                ErrorCode::ProtocolViolation,
            ),
            (
                c2_ipc::IpcError::ProtocolVersionMismatch {
                    local: 14,
                    peer: 15,
                },
                StatusCode::BAD_GATEWAY,
                ErrorCode::ProtocolViolation,
            ),
            (
                c2_ipc::IpcError::Protocol("malformed reply".into()),
                StatusCode::BAD_GATEWAY,
                ErrorCode::ProtocolViolation,
            ),
        ];

        for (error, expected_status, expected_code) in cases {
            let response = ipc_route_error_response("grid", &error, None);
            assert_eq!(response.status(), expected_status, "error={error}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["code"],
                u16::from(expected_code),
                "unexpected code for error={error}"
            );
            assert_eq!(
                body["name"],
                expected_code.name(),
                "unexpected name for error={error}"
            );
            if matches!(
                expected_code,
                ErrorCode::ResourceRemoved
                    | ErrorCode::ResourceClosed
                    | ErrorCode::ContractMismatch
            ) {
                assert_eq!(body["details"]["dispatch_phase"], "pre_dispatch");
            }
            assert_ne!(
                body["name"], "ResourceUnavailable",
                "semantic error was flattened: {error}"
            );
        }

        let response =
            ipc_route_error_response("grid", &c2_ipc::IpcError::Config("offline".into()), None);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["name"], "ResourceUnavailable");
        assert_eq!(body["details"]["dispatch_phase"], "pre_dispatch");
    }

    #[tokio::test]
    async fn confirmed_ipc_rejection_retains_canonical_error_and_phase() {
        use c2_error::{C2Error, ErrorCode};

        for (code, status) in [
            (ErrorCode::ResourceNotFound, StatusCode::NOT_FOUND),
            (ErrorCode::ResourceRemoved, StatusCode::GONE),
            (ErrorCode::ContractMismatch, StatusCode::CONFLICT),
            (ErrorCode::IdentityMismatch, StatusCode::CONFLICT),
            (ErrorCode::ResourceClosed, StatusCode::SERVICE_UNAVAILABLE),
            (
                ErrorCode::ResourceUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ErrorCode::CallCapacityExceeded,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (ErrorCode::ProtocolViolation, StatusCode::BAD_GATEWAY),
        ] {
            let original = C2Error::new(code, format!("rejected: {}", code.name()))
                .with_details(BTreeMap::from([("source".into(), "server".into())]));
            let response = ipc_route_error_response(
                "grid",
                &c2_ipc::IpcError::CallRejected(original.clone()),
                None,
            );
            assert_eq!(response.status(), status, "code={code:?}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let envelope: c2_error::C2ErrorEnvelope = serde_json::from_slice(&body).unwrap();
            let projected = C2Error::from_envelope(envelope).unwrap();
            assert_eq!(projected.code, original.code);
            assert_eq!(projected.message, original.message);
            assert_eq!(projected.details["source"], "server");
            assert_eq!(projected.details["dispatch_phase"], "pre_dispatch");
        }
    }

    #[test]
    fn unreachable_route_removal_log_names_route_and_reason() {
        let entry = RouteEntry {
            name: "grid".into(),
            relay_id: "relay-a".into(),
            relay_url: "http://relay-a:8080".into(),
            server_id: Some("server-grid".into()),
            server_instance_id: Some("server-grid-instance".into()),
            ipc_address: Some("ipc://grid".into()),
            crm_ns: TEST_CRM_NS.into(),
            crm_name: TEST_CRM_NAME.into(),
            crm_ver: TEST_CRM_VER.into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 1024,
            route_uid: Some("grid-uid".into()),
            locality: Locality::Local,
            registered_at: 1.0,
        };

        let line = unreachable_route_removal_log_line(&entry, 2.0, "route-missing");

        assert!(line.contains("[relay] Removed unreachable route:"));
        assert!(line.contains("name=grid"));
        assert!(line.contains("server_id=server-grid"));
        assert!(line.contains("server_instance_id=server-grid-instance"));
        assert!(line.contains("address=ipc://grid"));
        assert!(line.contains("removed_at=2"));
        assert!(line.contains("reason=route-missing"));
    }

    #[test]
    fn identity_mismatch_withdraws_all_routes_on_owner_endpoint() {
        let state = test_state();
        let address = "ipc://identity-mismatch-owner";
        for route_name in ["manager", "builder"] {
            match test_commit_registration!(
                &state,
                route_name.into(),
                "server-grid".into(),
                "server-grid-instance".into(),
                address.into(),
                TEST_CRM_NS.into(),
                TEST_CRM_NAME.into(),
                TEST_CRM_VER.into(),
                TEST_ABI_HASH.into(),
                TEST_SIGNATURE_HASH.into(),
                1024,
                format!("{route_name}-uid"),
                None,
            ) {
                RegisterCommitResult::Registered { .. } => {}
                _ => panic!("unexpected registration result for {route_name}"),
            }
        }
        match test_commit_registration!(
            &state,
            "other-endpoint".into(),
            "server-grid".into(),
            "server-grid-other-instance".into(),
            "ipc://identity-mismatch-other-owner".into(),
            TEST_CRM_NS.into(),
            TEST_CRM_NAME.into(),
            TEST_CRM_VER.into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            1024,
            "other-endpoint-uid".into(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected registration result for other endpoint"),
        }
        let route = state.local_route("manager").expect("manager route");
        let error = c2_ipc::IpcError::IdentityMismatch {
            expected_server_id: "server-grid".into(),
            expected_server_instance_id: "server-grid-instance".into(),
            actual_server_id: "server-grid-restarted".into(),
            actual_server_instance_id: "server-grid-restarted-instance".into(),
        };

        remove_unreachable_route_for_error(&state, &route, &error, "identity-mismatch");

        assert!(state.local_route("manager").is_none());
        assert!(
            state.local_route("builder").is_none(),
            "endpoint identity mismatch invalidates every local route owned by that endpoint"
        );
        assert!(
            state.local_route("other-endpoint").is_some(),
            "endpoint identity mismatch must not withdraw unrelated endpoints"
        );
    }

    #[test]
    fn closed_upstream_lookup_withdraws_the_matching_route() {
        let state = test_state();
        match test_commit_registration!(
            &state,
            "closed-upstream".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://closed-upstream".into(),
            TEST_CRM_NS.into(),
            TEST_CRM_NAME.into(),
            TEST_CRM_VER.into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            1024,
            "closed-upstream-uid".into(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected route registration result"),
        }
        let route = state
            .local_route("closed-upstream")
            .expect("advertised route");
        let error = c2_ipc::IpcError::RouteClosed {
            route_name: "closed-upstream".into(),
            reason: "Shutdown".into(),
        };
        let reason = semantic_withdrawal_reason(&error).expect("closed is authoritative");

        remove_unreachable_route_for_error(&state, &route, &error, reason);

        assert!(state.local_route("closed-upstream").is_none());
    }

    async fn wait_for_local_route_removed(state: &Arc<RelayState>, name: &str) {
        for _ in 0..40 {
            if state.local_route(name).is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for local route {name:?} to be removed");
    }

    struct RequestKindCallback;

    impl CrmCallback for RequestKindCallback {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: Vec<u8>,
            _response_pool: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(request))
        }
    }

    async fn start_request_kind_server(address: &str, server_id: &str) -> Arc<Server> {
        let server = Arc::new(
            Server::new_with_identity(
                address,
                ServerIpcConfig::default(),
                ServerIdentity {
                    server_id: server_id.to_string(),
                    server_instance_id: format!("{server_id}-instance"),
                },
            )
            .unwrap(),
        );
        let route = server
            .build_route(
                RouteBuildSpec {
                    name: "grid".into(),
                    crm_ns: "test.echo".into(),
                    crm_name: "Echo".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["ping".into()],
                    access_map: std::collections::HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(RequestKindCallback),
            )
            .unwrap();
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run().await;
        });
        server
            .wait_until_responsive(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        server
    }

    struct MarkerCallback {
        marker: &'static [u8],
    }

    impl CrmCallback for MarkerCallback {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: Vec<u8>,
            _response_pool: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(self.marker.to_vec()))
        }
    }

    async fn start_marker_server(
        address: &str,
        server_id: &str,
        server_instance_id: &str,
        route_name: &str,
        marker: &'static [u8],
    ) -> Arc<Server> {
        start_marker_server_in_context(
            &c2_config::LocalEndpointContext::default_for_platform().unwrap(),
            address,
            server_id,
            server_instance_id,
            route_name,
            marker,
        )
        .await
    }

    async fn start_marker_server_in_context(
        context: &c2_config::LocalEndpointContext,
        address: &str,
        server_id: &str,
        server_instance_id: &str,
        route_name: &str,
        marker: &'static [u8],
    ) -> Arc<Server> {
        start_callback_server_in_context(
            context,
            address,
            server_id,
            server_instance_id,
            route_name,
            Arc::new(MarkerCallback { marker }),
        )
        .await
    }

    async fn start_callback_server_in_context(
        context: &c2_config::LocalEndpointContext,
        address: &str,
        server_id: &str,
        server_instance_id: &str,
        route_name: &str,
        callback: Arc<dyn CrmCallback>,
    ) -> Arc<Server> {
        let server = Arc::new(
            Server::new_with_identity_and_endpoint(
                context.endpoint(address).unwrap(),
                ServerIpcConfig::default(),
                ServerIdentity {
                    server_id: server_id.to_string(),
                    server_instance_id: server_instance_id.to_string(),
                },
            )
            .unwrap(),
        );
        let route = server
            .build_route(
                RouteBuildSpec {
                    name: route_name.into(),
                    crm_ns: "test.echo".into(),
                    crm_name: "Echo".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["ping".into()],
                    access_map: std::collections::HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                callback,
            )
            .unwrap();
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let run_server = server.clone();
        tokio::spawn(async move {
            let _ = run_server.run().await;
        });
        server
            .wait_until_responsive(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        server
    }

    fn namespace_test_state(context: c2_config::LocalEndpointContext) -> Arc<RelayState> {
        Arc::new(RelayState::new_with_context(
            Arc::new(c2_config::RelayConfig::default()),
            Arc::new(crate::relay::test_support::NoopDisseminator),
            context,
        ))
    }

    fn install_namespace_test_route(state: &Arc<RelayState>) {
        assert!(matches!(
            test_commit_registration!(
                state,
                "grid".into(),
                "same-owner".into(),
                "same-instance".into(),
                "ipc://same-owner".into(),
                TEST_CRM_NS.into(),
                TEST_CRM_NAME.into(),
                TEST_CRM_VER.into(),
                TEST_ABI_HASH.into(),
                TEST_SIGNATURE_HASH.into(),
                1024,
                "grid-uid".into(),
                None,
            ),
            RegisterCommitResult::Registered { .. }
        ));
    }

    async fn namespace_resolve_response(
        state: Arc<RelayState>,
        namespace: Option<&str>,
    ) -> Response {
        let mut request = Request::builder().uri(format!(
            "/_resolve/grid?crm_ns={TEST_CRM_NS}&crm_name={TEST_CRM_NAME}&crm_ver={TEST_CRM_VER}&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
        ));
        if let Some(namespace) = namespace {
            request = request.header(LOCAL_ENDPOINT_NAMESPACE_HEADER, namespace);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        build_router(state).oneshot(request).await.unwrap()
    }

    // This runs on Windows too: the default namespace is the current user SID,
    // not a made-up Unix root or an environment-selected pipe backend.
    #[tokio::test]
    async fn default_relay_namespace_keeps_legacy_loopback_resolution() {
        let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
        let state = namespace_test_state(context.clone());
        install_namespace_test_route(&state);
        let response = namespace_resolve_response(state.clone(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[LOCAL_ENDPOINT_NAMESPACE_HEADER],
            context.namespace_id()
        );
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body[0]["ipc_address"], "ipc://same-owner");
        assert!(body[0].get("local_endpoint_namespace").is_none());
        let mismatch = "f".repeat(64);
        let response = namespace_resolve_response(state.clone(), Some(&mismatch)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(body[0]["ipc_address"].is_null());
        assert_eq!(body[0]["name"], "grid");
        assert!(state.local_route("grid").is_some());
        #[cfg(windows)]
        {
            assert_eq!(
                context.platform_kind(),
                c2_config::LocalEndpointNamespace::WindowsNamedPipe
            );
            assert!(context.unix_root().is_none());
            assert!(
                context
                    .endpoint("ipc://same-owner")
                    .unwrap()
                    .os_name()
                    .to_string_lossy()
                    .starts_with(r"\\.\pipe\c_two-")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_relay_suppresses_ipc_for_legacy_and_other_namespace_clients() {
        let root = crate::relay::test_support::EndpointTestRoot::new();
        let context = root.context();
        let state = namespace_test_state(context.clone());
        install_namespace_test_route(&state);
        for namespace in [
            None,
            Some("f".repeat(64)),
            Some(context.namespace_id().to_owned()),
        ] {
            let response = namespace_resolve_response(state.clone(), namespace.as_deref()).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(
                !body[0]["ipc_address"].is_null(),
                namespace.as_deref() == Some(context.namespace_id())
            );
            assert_eq!(body[0]["name"], "grid");
            assert!(body[0].get("route_uid").is_none());
        }
    }

    #[tokio::test]
    async fn register_namespace_rejects_before_parsing_or_owner_preparation() {
        let state =
            namespace_test_state(c2_config::LocalEndpointContext::default_for_platform().unwrap());
        install_namespace_test_route(&state);
        for (namespace, status) in [
            ("f".repeat(64), StatusCode::CONFLICT),
            ("bad".into(), StatusCode::BAD_REQUEST),
        ] {
            let response = build_router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/_register")
                        .header("content-type", "application/json")
                        .header(LOCAL_ENDPOINT_NAMESPACE_HEADER, namespace)
                        // Missing identity/address would fail parsing if the domain gate ran late.
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(
                state.local_route("grid").unwrap().route_uid.as_deref(),
                Some("grid-uid")
            );
        }
        let response = build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .header(
                        LOCAL_ENDPOINT_NAMESPACE_HEADER,
                        state.endpoint_context().namespace_id(),
                    )
                    .header(
                        LOCAL_ENDPOINT_NAMESPACE_HEADER,
                        state.endpoint_context().namespace_id(),
                    )
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            state.local_route("grid").unwrap().route_uid.as_deref(),
            Some("grid-uid")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_register_requires_namespace_before_any_owner_probe() {
        let root = crate::relay::test_support::EndpointTestRoot::new();
        let state = namespace_test_state(root.context());
        install_namespace_test_route(&state);
        let response = build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            state.local_route("grid").unwrap().route_uid.as_deref(),
            Some("grid-uid")
        );
        // Both missing and mismatched namespace checks precede route owner
        // preparation, which can itself open a captured owner's IPC endpoint.
        let source = source_between(
            include_str!("router.rs"),
            "async fn handle_register",
            "fn close_arc_client",
        )
        .unwrap();
        assert!(
            source.find("local_namespace_from_headers").unwrap()
                < source.find(".prepare_register(").unwrap()
        );
        assert!(
            source.find("local_namespace_from_headers").unwrap()
                < source.find("connect_register_attestation_client(").unwrap()
        );
    }

    #[tokio::test]
    async fn probe_namespace_metadata_is_frozen_even_when_route_is_absent() {
        let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
        let state = namespace_test_state(context.clone());
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/_probe/grid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()[LOCAL_ENDPOINT_NAMESPACE_HEADER],
            context.namespace_id()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_namespace_captured_owner_probe_refuses_replacing_live_owner() {
        let root = crate::relay::test_support::EndpointTestRoot::new();
        let context = root.context();
        let address = "ipc://custom-captured-owner";
        let server = start_marker_server_in_context(
            &context,
            address,
            "custom-owner",
            "custom-instance",
            "grid",
            b"old",
        )
        .await;
        let state = namespace_test_state(context.clone());
        let response = build_router(state.clone()).oneshot(Request::builder()
            .method("POST").uri("/_register").header("content-type", "application/json")
            .header(LOCAL_ENDPOINT_NAMESPACE_HEADER, context.namespace_id())
            .body(Body::from(serde_json::json!({
                "name": "grid", "server_id": "custom-owner", "server_instance_id": "custom-instance",
                "address": address, "max_payload_size": ServerIpcConfig::default().max_payload_size,
            }).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        state.evict_connection("grid");
        let preparation = RouteAuthority::new(&state)
            .prepare_register(
                "grid",
                "replacement-owner",
                "replacement-instance",
                "ipc://replacement-owner",
            )
            .await;
        let preserved = matches!(preparation, Ok(RegisterPreparation::DuplicateAlive { existing_address }) if existing_address == address);
        shutdown_live_server(&server).await;
        assert!(
            preserved,
            "captured owner probe must check the live custom-root owner rather than treating an absent default-root endpoint as dead"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_roots_same_owner_keep_attestation_pool_watch_and_http_in_frozen_namespace() {
        use crate::client::{
            RelayAwareClientConfig, RelayAwareHttpClient, RelayControlClient, RelayRegistration,
            RelayResolvedTarget,
        };
        let roots = [
            crate::relay::test_support::EndpointTestRoot::new(),
            crate::relay::test_support::EndpointTestRoot::new(),
        ];
        let contexts = [roots[0].context(), roots[1].context()];
        let address = "ipc://same-owner";
        let servers = [
            start_marker_server_in_context(
                &contexts[0],
                address,
                "same-owner",
                "same-instance",
                "grid",
                b"root-a",
            )
            .await,
            start_marker_server_in_context(
                &contexts[1],
                address,
                "same-owner",
                "same-instance",
                "grid",
                b"root-b",
            )
            .await,
        ];
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "grid".into(),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
        };
        let mut relay_tasks = Vec::new();
        let mut relay_states = Vec::new();
        let mut urls = Vec::new();
        for (index, context) in contexts.iter().enumerate() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(RelayState::new_with_context(
                Arc::new(c2_config::RelayConfig {
                    relay_id: format!("relay-{index}"),
                    advertise_url: url.clone(),
                    ..c2_config::RelayConfig::default()
                }),
                Arc::new(crate::relay::test_support::NoopDisseminator),
                context.clone(),
            ));
            let app = build_router(state.clone());
            relay_tasks.push(tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            }));
            let control = RelayControlClient::new(&url, false)
                .unwrap()
                .with_local_endpoint_context(context);
            let registration_expected = expected.clone();
            tokio::task::spawn_blocking(move || {
                control.register(RelayRegistration {
                    expected: &registration_expected,
                    server_id: "same-owner",
                    server_instance_id: "same-instance",
                    address,
                    max_payload_size: ServerIpcConfig::default().max_payload_size,
                })
            })
            .await
            .unwrap()
            .unwrap();
            relay_states.push(state);
            urls.push(url);
        }
        for index in 0..2 {
            let client = RelayAwareHttpClient::new(
                &urls[index],
                expected.clone(),
                false,
                RelayAwareClientConfig::default(),
            )
            .unwrap()
            .with_local_endpoint_context(&contexts[index]);
            let RelayResolvedTarget::Ipc { candidate } = tokio::task::spawn_blocking(move || {
                client.resolve_target_with_deadline(c2_config::ConnectDeadline::default())
            })
            .await
            .unwrap()
            .unwrap() else {
                panic!("matching namespace must propose direct IPC")
            };
            let mut ipc = IpcClient::with_endpoint(
                contexts[index].endpoint(&candidate.address).unwrap(),
                ClientIpcConfig::default(),
            )
            .unwrap();
            ipc.connect().await.unwrap();
            assert_eq!(ipc.server_id(), Some(candidate.server_id.as_str()));
            assert_eq!(
                ipc.server_instance_id(),
                Some(candidate.server_instance_id.as_str())
            );
            let binding = ipc.acquire_route(&expected).await.unwrap();
            let response = ipc.call_bound(&binding, "ping", b"").await.unwrap();
            assert_eq!(
                response.into_owned_bytes().unwrap(),
                if index == 0 {
                    b"root-a".to_vec()
                } else {
                    b"root-b".to_vec()
                }
            );
            ipc.close().await;
            let wrong_client = RelayAwareHttpClient::new(
                &urls[index],
                expected.clone(),
                false,
                RelayAwareClientConfig::default(),
            )
            .unwrap()
            .with_local_endpoint_context(&contexts[1 - index]);
            let (wrong_client, target) = tokio::task::spawn_blocking(move || {
                let target = wrong_client
                    .resolve_target_with_deadline(c2_config::ConnectDeadline::default());
                (wrong_client, target.unwrap())
            })
            .await
            .unwrap();
            assert!(matches!(target, RelayResolvedTarget::Http { .. }));
            assert_eq!(
                wrong_client.call_async("ping", b"").await.unwrap(),
                if index == 0 {
                    b"root-a".to_vec()
                } else {
                    b"root-b".to_vec()
                }
            );
            // Custom relay + legacy default client keeps the HTTP route.
            let legacy = RelayAwareHttpClient::new(
                &urls[index],
                expected.clone(),
                false,
                RelayAwareClientConfig::default(),
            )
            .unwrap();
            assert!(matches!(
                tokio::task::spawn_blocking(move || {
                    legacy.resolve_target_with_deadline(c2_config::ConnectDeadline::default())
                })
                .await
                .unwrap()
                .unwrap(),
                RelayResolvedTarget::Http { .. }
            ));
            let route_before = relay_states[index].local_route("grid").unwrap();
            let wrong_registration = RelayControlClient::new(&urls[index], false)
                .unwrap()
                .with_local_endpoint_context(&contexts[1 - index]);
            let registration_expected = expected.clone();
            let result = tokio::task::spawn_blocking(move || {
                wrong_registration.register(RelayRegistration {
                    expected: &registration_expected,
                    server_id: "same-owner",
                    server_instance_id: "same-instance",
                    address,
                    max_payload_size: ServerIpcConfig::default().max_payload_size,
                })
            })
            .await
            .unwrap();
            assert!(matches!(
                result,
                Err(crate::client::HttpError::ServerError(409, _))
            ));
            assert_eq!(
                relay_states[index].local_route("grid").unwrap().route_uid,
                route_before.route_uid
            );
            // Force the data plane to reconnect in its captured namespace.
            relay_states[index].evict_connection("grid");
            assert_eq!(
                wrong_client.call_async("ping", b"").await.unwrap(),
                if index == 0 {
                    b"root-a".to_vec()
                } else {
                    b"root-b".to_vec()
                }
            );
        }
        // Native unregister publishes a semantic route removal while the
        // connection is live. Generic server disconnect alone is not evidence
        // authorizing withdrawal, so it must not be this fixture's trigger.
        let root_b_before = relay_states[1].local_route("grid").unwrap();
        assert!(servers[0].unregister_route("grid").await);
        wait_for_local_route_removed(&relay_states[0], "grid").await;
        let root_b_after = relay_states[1].local_route("grid").unwrap();
        assert_eq!(root_b_after.route_uid, root_b_before.route_uid);
        assert_eq!(
            root_b_after.server_instance_id,
            root_b_before.server_instance_id
        );
        assert!(servers[1].unregister_route("grid").await);
        wait_for_local_route_removed(&relay_states[1], "grid").await;
        assert!(relay_states[0].local_route("grid").is_none());
        shutdown_live_server(&servers[0]).await;
        shutdown_live_server(&servers[1]).await;
        for task in relay_tasks {
            task.abort();
        }
    }

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let start_idx = source.find(start)?;
        let after_start = &source[start_idx..];
        let end_idx = after_start.find(end)?;
        Some(&after_start[..end_idx])
    }

    #[test]
    fn http_register_final_confirmation_stays_after_candidate_attestation() {
        let source = include_str!("router.rs");
        let body = source_between(source, "async fn handle_register", "fn close_arc_client")
            .expect("handle_register body should be found");
        let prepare = body
            .find(".prepare_register(")
            .expect("HTTP registration must prepare replacement eligibility");
        let attest = body
            .find("attest_ipc_route_contract(")
            .expect("HTTP registration must attest claimed route contracts");
        let read = body
            .find("read_ipc_route_contract(")
            .expect("HTTP registration must read unclaimed route contracts");
        let confirm = body
            .find(".confirm_replacement_for_commit(")
            .expect("HTTP registration must perform final replacement confirmation");
        let commit = body
            .find(".commit_register_upstream(")
            .expect("HTTP registration must commit through RelayState");

        assert!(
            prepare < attest && prepare < read,
            "preliminary eligibility must run before candidate route attestation"
        );
        assert!(
            attest < confirm && read < confirm,
            "final replacement proof must be created after candidate route attestation"
        );
        assert!(
            confirm < commit,
            "relay commit must consume only post-attestation final proof"
        );
    }

    #[test]
    fn http_control_plane_logs_register_and_unregister_events() {
        let source = include_str!("router.rs");
        let register_body =
            source_between(source, "async fn handle_register", "fn close_arc_client")
                .expect("handle_register body should be found");
        let unregister_body = source_between(
            source,
            "async fn handle_admin_unregister",
            "async fn handle_list_routes",
        )
        .expect("handle_unregister body should be found");

        for expected in [
            "[relay] Register request:",
            "[relay] Register committed:",
            "[relay] Register same-owner:",
            "[relay] Register rejected:",
        ] {
            assert!(
                register_body.contains(expected),
                "HTTP register path must log diagnostic event: {expected}"
            );
        }
        for expected in [
            "[relay] Unregister request:",
            "[relay] Unregister removed:",
            "[relay] Unregister already removed:",
            "[relay] Unregister rejected:",
        ] {
            assert!(
                unregister_body.contains(expected),
                "HTTP unregister path must log diagnostic event: {expected}"
            );
        }
    }

    fn test_state() -> Arc<RelayState> {
        test_state_for_client()
    }

    fn register_body(name: &str, server_id: &str, address: &str) -> Body {
        Body::from(
            serde_json::json!({
                "name": name,
                "server_id": server_id,
                "server_instance_id": format!("{server_id}-instance"),
                "address": address,
                "max_payload_size": ServerIpcConfig::default().max_payload_size,
            })
            .to_string(),
        )
    }

    async fn post_register(
        state: Arc<RelayState>,
        name: &str,
        server_id: &str,
        address: &str,
    ) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(register_body(name, server_id, address))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn post_register_json(state: Arc<RelayState>, body: serde_json::Value) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn post_register_with_crm(
        state: Arc<RelayState>,
        name: &str,
        server_id: &str,
        address: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
    ) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": name,
                            "server_id": server_id,
                            "server_instance_id": format!("{server_id}-instance"),
                            "address": address,
                            "crm_ns": crm_ns,
                            "crm_name": crm_name,
                            "crm_ver": crm_ver,
                            "abi_hash": TEST_ABI_HASH,
                            "signature_hash": TEST_SIGNATURE_HASH,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn post_register_with_crm_and_token(
        state: Arc<RelayState>,
        name: &str,
        server_id: &str,
        address: &str,
        registration_token: &str,
        crm: (&str, &str, &str),
    ) -> StatusCode {
        let (crm_ns, crm_name, crm_ver) = crm;
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": name,
                            "server_id": server_id,
                            "server_instance_id": format!("{server_id}-instance"),
                            "address": address,
                            "registration_token": registration_token,
                            "crm_ns": crm_ns,
                            "crm_name": crm_name,
                            "crm_ver": crm_ver,
                            "abi_hash": TEST_ABI_HASH,
                            "signature_hash": TEST_SIGNATURE_HASH,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    #[tokio::test]
    async fn register_rejects_incomplete_claimed_crm_tag_before_ipc_connect() {
        let state = Arc::new(RelayState::new(
            Arc::new(c2_config::RelayConfig {
                relay_id: "test-relay".into(),
                ..Default::default()
            }),
            Arc::new(crate::relay::test_support::NoopDisseminator),
        ));

        let status = post_register_with_crm(
            state.clone(),
            "grid",
            "server-grid",
            "ipc://grid",
            "test.mesh",
            "",
            "0.1.0",
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(state.list_routes().is_empty());
    }

    async fn post_unregister(state: Arc<RelayState>, name: &str, server_id: &str) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_admin/unregister")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": name,
                            "server_id": server_id,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    fn add_expected_echo_headers(
        builder: axum::http::request::Builder,
    ) -> axum::http::request::Builder {
        builder
            .header("x-c2-expected-crm-ns", "test.echo")
            .header("x-c2-expected-crm-name", "Echo")
            .header("x-c2-expected-crm-ver", "0.1.0")
            .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
            .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
    }

    fn add_expected_crm_tag_headers(
        builder: axum::http::request::Builder,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
    ) -> axum::http::request::Builder {
        builder
            .header("x-c2-expected-crm-ns", crm_ns)
            .header("x-c2-expected-crm-name", crm_name)
            .header("x-c2-expected-crm-ver", crm_ver)
            .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
            .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
    }

    async fn post_call(state: Arc<RelayState>, name: &str, method: &str) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/{name}/{method}"))
                        .header("content-type", "application/octet-stream"),
                )
                .body(Body::from(Vec::new()))
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn post_call_response(
        state: Arc<RelayState>,
        name: &str,
        method: &str,
    ) -> (StatusCode, Vec<u8>) {
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/{name}/{method}"))
                        .header("content-type", "application/octet-stream"),
                )
                .body(Body::from(Vec::new()))
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    async fn post_call_with_expected_crm_tag(
        state: Arc<RelayState>,
        name: &str,
        method: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
    ) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_crm_tag_headers(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/{name}/{method}"))
                        .header("content-type", "application/octet-stream"),
                    crm_ns,
                    crm_name,
                    crm_ver,
                )
                .body(Body::from(Vec::new()))
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn get_probe(state: Arc<RelayState>, name: &str) -> StatusCode {
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/_probe/{name}")),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn get_probe_response(state: Arc<RelayState>, name: &str) -> (StatusCode, Vec<u8>) {
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/_probe/{name}")),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    async fn get_resolve_without_query(state: Arc<RelayState>, name: &str) -> StatusCode {
        let app = build_router(state);
        let mut request = Request::builder()
            .method("GET")
            .uri(format!("/_resolve/{name}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn get_resolve_with_crm_tag(
        state: Arc<RelayState>,
        name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
    ) -> StatusCode {
        let app = build_router(state);
        let mut request = Request::builder()
            .method("GET")
            .uri(format!(
                "/_resolve/{name}?crm_ns={crm_ns}&crm_name={crm_name}&crm_ver={crm_ver}&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
            ))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn get_resolve_with_query(state: Arc<RelayState>, name: &str, query: &str) -> StatusCode {
        let app = build_router(state);
        let mut request = Request::builder()
            .method("GET")
            .uri(format!("/_resolve/{name}?{query}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    async fn get_resolve_uri(state: Arc<RelayState>, uri: &str) -> StatusCode {
        let app = build_router(state);
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    #[tokio::test]
    async fn resolve_requires_expected_contract_query() {
        let state = test_state();

        assert_eq!(
            get_resolve_without_query(state, "grid").await,
            StatusCode::BAD_REQUEST,
        );
    }

    #[tokio::test]
    async fn resolve_rejects_partial_crm_tag_query_instead_of_downgrading_to_name_only() {
        let state = test_state();

        assert_eq!(
            get_resolve_with_query(state, "grid", "crm_ns=test.echo&crm_ver=0.1.0").await,
            StatusCode::BAD_REQUEST,
        );
    }

    #[tokio::test]
    async fn resolve_rejects_control_character_route_name() {
        let state = test_state();

        assert_eq!(
            get_resolve_uri(state, "/_resolve/grid%00hidden").await,
            StatusCode::BAD_REQUEST,
        );
    }

    #[tokio::test]
    async fn resolve_rejects_control_character_crm_tag_query() {
        let state = test_state();

        assert_eq!(
            get_resolve_with_query(
                state,
                "grid",
                "crm_ns=test.echo&crm_name=Echo%00Hidden&crm_ver=0.1.0",
            )
            .await,
            StatusCode::BAD_REQUEST,
        );
    }

    #[tokio::test]
    async fn resolve_with_expected_contract_hides_name_match_with_wrong_crm_name() {
        let state = test_state();
        let address = format!(
            "ipc://relay_resolve_crm_name_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "OtherEcho", "0.1.0",)
                .await,
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "Echo", "0.1.0").await,
            StatusCode::OK,
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn resolve_with_expected_contract_hides_name_match_with_wrong_hash() {
        let state = test_state();
        let address = format!(
            "ipc://relay_resolve_hash_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            get_resolve_with_query(
                state.clone(),
                "grid",
                "crm_ns=test.echo&crm_name=Echo&crm_ver=0.1.0&abi_hash=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210&signature_hash=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )
            .await,
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "Echo", "0.1.0").await,
            StatusCode::OK,
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_data_plane_requires_expected_contract_headers() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grid/ping")
                    .header("content-type", "application/octet-stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("expected CRM headers and hash headers are required"),
            "unexpected body: {body}"
        );
    }

    #[tokio::test]
    async fn relay_probe_requires_expected_contract_headers() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_probe/grid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("expected CRM headers and hash headers are required"),
            "unexpected body: {body}"
        );
    }

    #[tokio::test]
    async fn relay_probe_accepts_contract_without_retired_header() {
        let state = test_state();
        let address = format!(
            "ipc://relay_probe_missing_route_token_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED,
        );

        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(Request::builder().method("GET").uri("/_probe/grid"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_probe_ignores_retired_header_value() {
        let retired_header = "x-c2-route-uid";
        let state = test_state();
        let address = format!(
            "ipc://relay_probe_stale_external_token_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED,
        );

        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("GET")
                        .uri("/_probe/grid")
                        .header(retired_header, "stale-grid-route-uid"),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_probe_ignores_repeated_retired_header() {
        let retired_header = "x-c2-route-uid";
        let state = test_state();
        let address = format!(
            "ipc://relay_probe_repeated_route_token_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED,
        );
        let current_route = state.local_route("grid").expect("route registered");

        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("GET")
                        .uri("/_probe/grid")
                        .header(retired_header, current_route.route_uid.as_deref().unwrap())
                        .header(retired_header, current_route.route_uid.as_deref().unwrap()),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn data_plane_rejects_partial_expected_hash_headers_before_acquire() {
        let state = test_state();
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grid/ping")
                    .header("content-type", "application/octet-stream")
                    .header("x-c2-expected-crm-ns", "test.echo")
                    .header("x-c2-expected-crm-name", "Echo")
                    .header("x-c2-expected-crm-ver", "0.1.0")
                    .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("expected CRM headers and hash headers must be supplied together"),
            "unexpected body: {body}"
        );
    }

    async fn get_resolve_routes_from(
        state: Arc<RelayState>,
        name: &str,
        remote_addr: SocketAddr,
    ) -> (StatusCode, Vec<RouteInfo>) {
        let app = build_router(state);
        let mut request = Request::builder()
            .method("GET")
            .uri(format!(
                "/_resolve/{name}?crm_ns={TEST_CRM_NS}&crm_name={TEST_CRM_NAME}&crm_ver={TEST_CRM_VER}&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
            ))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(remote_addr));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let routes = if status == StatusCode::OK {
            serde_json::from_slice(&body).unwrap()
        } else {
            Vec::new()
        };
        (status, routes)
    }

    #[tokio::test]
    async fn resolve_exposes_ipc_address_only_to_loopback_clients() {
        let state = test_state();
        test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "inst-grid".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            "grid-server-grid-uid".into(),
            None,
        );

        let (status, routes) = get_resolve_routes_from(
            state.clone(),
            "grid",
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
        assert_eq!(routes[0].server_id.as_deref(), Some("server-grid"));
        assert_eq!(routes[0].server_instance_id.as_deref(), Some("inst-grid"));
        assert!(
            serde_json::to_value(&routes[0])
                .unwrap()
                .get("route_uid")
                .is_none()
        );

        let (status, routes) = get_resolve_routes_from(
            state,
            "grid",
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 12345),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(routes[0].ipc_address, None);
        assert_eq!(routes[0].server_id, None);
        assert_eq!(routes[0].server_instance_id, None);
        assert!(
            serde_json::to_value(&routes[0])
                .unwrap()
                .get("route_uid")
                .is_none()
        );
    }

    #[tokio::test]
    async fn register_rejects_invalid_server_id_at_control_boundary() {
        let state = test_state();
        let too_long = "s".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        assert_eq!(
            post_register(state.clone(), "grid", " ", "ipc://grid").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_register(state.clone(), "grid", "bad/path", "ipc://grid").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_register(state, "grid", &too_long, "ipc://grid").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn register_rejects_invalid_server_instance_id_at_control_boundary() {
        let state = test_state();
        let too_long = "a".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        for server_instance_id in ["../bad", too_long.as_str()] {
            let app = build_router(state.clone());
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/_register")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "name": "grid",
                                "server_id": "server-grid",
                                "server_instance_id": server_instance_id,
                                "address": "ipc://grid",
                                "max_payload_size": ServerIpcConfig::default().max_payload_size,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(String::from_utf8_lossy(&body).contains("server_instance_id"));
        }
    }

    #[tokio::test]
    async fn register_rejects_ipc_handshake_identity_mismatch() {
        let state = test_state();
        let address = format!(
            "ipc://relay_identity_mismatch_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-actual").await;

        assert_eq!(
            post_register(state, "grid", "server-claimed", &address).await,
            StatusCode::BAD_GATEWAY
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_upstream_that_does_not_export_route() {
        let state = test_state();
        let address = format!(
            "ipc://relay_missing_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_routes(&address, "server-grid", &["counter"]).await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.list_routes().is_empty(),
            "relay must not advertise a route that the upstream handshake did not export"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_claimed_contract_when_upstream_does_not_export_route() {
        let state = test_state();
        let address = format!(
            "ipc://relay_claimed_missing_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_routes(&address, "server-grid", &["counter"]).await;

        assert_eq!(
            post_register_with_crm(
                state.clone(),
                "grid",
                "server-grid",
                &address,
                TEST_CRM_NS,
                TEST_CRM_NAME,
                TEST_CRM_VER,
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.list_routes().is_empty(),
            "relay must not advertise caller-claimed CRM metadata for an unexported IPC route"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_publishing_server_attested_pending_route_with_token() {
        let state = test_state();
        let address = format!(
            "ipc://relay_pending_attested_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[],
        )
        .await;
        let reservation = reserve_echo_route(&server, "grid").await;
        let registration_token = reservation.registration_token().to_string();

        assert_eq!(
            post_register_with_crm_and_token(
                state.clone(),
                "grid",
                "server-grid",
                &address,
                &registration_token,
                ("test.echo", "Echo", "0.1.0"),
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.list_routes().is_empty(),
            "ordinary register must not publish pending routes even with a valid token"
        );

        server.abort_reserved_route(reservation).await;
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_prepare_with_pending_token_does_not_publish_route() {
        let state = test_state();
        let address = format!(
            "ipc://relay_pending_prepare_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[],
        )
        .await;
        let reservation = reserve_echo_route(&server, "grid").await;
        let registration_token = reservation.registration_token().to_string();

        let prepare_status = post_register_json(
            state.clone(),
            serde_json::json!({
                "name": "grid",
                "server_id": "server-grid",
                "server_instance_id": "server-grid-instance",
                "address": address,
                "registration_token": registration_token,
                "prepare_only": true,
                "max_payload_size": ServerIpcConfig::default().max_payload_size,
                "crm_ns": "test.echo",
                "crm_name": "Echo",
                "crm_ver": "0.1.0",
                "abi_hash": TEST_ABI_HASH,
                "signature_hash": TEST_SIGNATURE_HASH,
            }),
        )
        .await;
        assert_eq!(prepare_status, StatusCode::ACCEPTED);
        assert!(
            state.list_routes().is_empty(),
            "prepare-only registration must not publish relay-visible route state"
        );

        server.commit_reserved_route(reservation).await.unwrap();
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert!(state.local_route("grid").is_some());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_derives_crm_contract_from_ipc_handshake() {
        let state = test_state();
        let address = format!(
            "ipc://relay_register_contract_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );

        let route = state.local_route("grid").unwrap().to_route_info();
        assert_eq!(route.crm_ns, "test.echo");
        assert_eq!(route.crm_ver, "0.1.0");

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_non_string_contract_claim_fields() {
        let state = test_state();
        let address = format!(
            "ipc://relay_register_non_string_contract_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        let status = post_register_json(
            state.clone(),
            serde_json::json!({
                "name": "grid",
                "server_id": "server-grid",
                "server_instance_id": "server-grid-instance",
                "address": address,
                "crm_ns": 1,
                "crm_name": true,
                "crm_ver": ["0.1.0"],
                "abi_hash": {"value": TEST_ABI_HASH},
                "signature_hash": null,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            state.list_routes().is_empty(),
            "relay must not silently ignore malformed claimed CRM contract fields"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_claimed_crm_contract_mismatch() {
        let state = test_state();
        let address = format!(
            "ipc://relay_register_contract_mismatch_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register_with_crm(
                state.clone(),
                "grid",
                "server-grid",
                &address,
                "wrong.grid",
                "WrongGrid",
                "9.9.9",
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.list_routes().is_empty(),
            "relay must not advertise a route with caller-claimed CRM metadata that disagrees with the IPC handshake"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_same_owner_rejects_claimed_crm_contract_mismatch() {
        let state = test_state();
        let address = format!(
            "ipc://relay_register_same_owner_contract_mismatch_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            post_register_with_crm(
                state.clone(),
                "grid",
                "server-grid",
                &address,
                "wrong.grid",
                "WrongGrid",
                "9.9.9",
            )
            .await,
            StatusCode::BAD_REQUEST
        );

        let route = state.local_route("grid").unwrap().to_route_info();
        assert_eq!(route.crm_ns, "test.echo");
        assert_eq!(route.crm_ver, "0.1.0");

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_same_owner_without_claim_reattests_ipc_contract() {
        let state = test_state();
        let address = format!(
            "ipc://relay_register_same_owner_unclaimed_contract_mismatch_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        match test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            "grid-server-grid-uid".into(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected initial registration result"),
        }
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[(
                "grid",
                "test.other",
                "OtherGrid",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::BAD_REQUEST
        );
        let route = state.local_route("grid").unwrap().to_route_info();
        assert_eq!(route.crm_ns, "test.echo");
        assert_eq!(route.crm_name, "Echo");

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn register_rejects_route_name_that_cannot_fit_wire_control() {
        let state = test_state();
        let long_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);

        assert_eq!(
            post_register(state, &long_name, "server-grid", "ipc://grid").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn call_generic_io_unreachable_upstream_keeps_local_route() {
        let state = test_state();

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", "ipc://missing-grid").await,
            StatusCode::BAD_GATEWAY
        );

        test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "inst-grid".into(),
            "ipc://missing-grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            "grid-missing-server-grid-uid".into(),
            None,
        );

        assert_eq!(
            post_call_with_expected_crm_tag(
                state.clone(),
                "grid",
                "step",
                TEST_CRM_NS,
                TEST_CRM_NAME,
                TEST_CRM_VER,
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert!(
            state.local_route("grid").is_some(),
            "generic IPC I/O errors must not withdraw a route"
        );
    }

    #[tokio::test]
    async fn call_route_missing_on_expected_owner_removes_local_route() {
        let state = test_state();
        let address = format!(
            "ipc://relay_route_missing_expected_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "inst-grid",
            &[(
                "other",
                TEST_CRM_NS,
                TEST_CRM_NAME,
                TEST_CRM_VER,
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "inst-grid".into(),
            address,
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            "grid-route-missing-server-grid-uid".into(),
            None,
        );

        assert_eq!(
            post_call_with_expected_crm_tag(
                state.clone(),
                "grid",
                "step",
                TEST_CRM_NS,
                TEST_CRM_NAME,
                TEST_CRM_VER,
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert!(
            state.list_routes().is_empty(),
            "connected owner route-missing is semantic withdrawal evidence"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn unregister_rejects_invalid_server_id_at_control_boundary() {
        let state = test_state();
        assert_eq!(
            post_unregister(state.clone(), "grid", " ").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_unregister(state, "grid", "bad\\path").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn oversized_control_plane_body_is_rejected() {
        let state = test_state();
        let app = build_router(state);
        let oversized = serde_json::json!({
            "name": "grid",
            "server_id": "server-grid",
            "address": "ipc://grid",
            "padding": "x".repeat(2 * 1024 * 1024),
        })
        .to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_register")
                    .header("content-type", "application/json")
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn data_plane_echo_allows_large_payloads() {
        let state = test_state();
        let app = build_router(state);
        let payload = vec![b'x'; 2 * 1024 * 1024];

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_echo")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(payload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), payload.len());
    }

    #[tokio::test]
    async fn relay_rejects_oversized_expect_content_length_without_polling_body() {
        let state = test_state();
        state.with_route_table_mut(|rt| {
            assert!(rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "test-relay".into(),
                relay_url: "http://localhost:9999".into(),
                server_id: Some("server-grid".into()),
                server_instance_id: Some("server-grid-instance".into()),
                ipc_address: Some("ipc://grid".into()),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 4,
                route_uid: Some("grid-oversized-uid".into()),
                locality: crate::relay::types::Locality::Local,
                registered_at: 1000.0,
            }));
        });
        let app = build_router(state);
        let stream = futures::stream::once(async {
            panic!("oversized relay request body should not be polled");
            #[allow(unreachable_code)]
            Ok::<Bytes, std::io::Error>(Bytes::new())
        });

        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/grid/ping")
                        .header("content-type", "application/octet-stream")
                        .header("content-length", "5")
                        .header("expect", "100-continue"),
                )
                .body(Body::from_stream(stream))
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"], "PayloadTooLarge");
        assert_eq!(payload["content_length"], 5);
        assert_eq!(payload["max_payload_size"], 4);
    }

    #[tokio::test]
    async fn relay_rejects_invalid_content_length_without_polling_body() {
        let state = test_state();
        state.with_route_table_mut(|rt| {
            assert!(rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "test-relay".into(),
                relay_url: "http://localhost:9999".into(),
                server_id: Some("server-grid".into()),
                server_instance_id: Some("server-grid-instance".into()),
                ipc_address: Some("ipc://grid".into()),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                route_uid: Some("grid-duplicate-content-length-uid".into()),
                locality: crate::relay::types::Locality::Local,
                registered_at: 1000.0,
            }));
        });
        let app = build_router(state);
        for values in [
            vec![axum::http::HeaderValue::from_static("5"); 2],
            vec![axum::http::HeaderValue::from_static("not-a-number")],
            vec![axum::http::HeaderValue::from_static("18446744073709551616")],
            vec![axum::http::HeaderValue::from_bytes(b"\xff").unwrap()],
        ] {
            let stream = futures::stream::once(async {
                panic!("invalid content-length request body should not be polled");
                #[allow(unreachable_code)]
                Ok::<Bytes, std::io::Error>(Bytes::new())
            });
            let mut request =
                add_expected_echo_headers(Request::builder().method("POST").uri("/grid/ping"));
            for value in values {
                request = request.header(header::CONTENT_LENGTH, value);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from_stream(stream)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(payload["error"], "InvalidContentLength");
        }
    }

    #[tokio::test]
    async fn relay_rejects_large_unknown_length_body_with_small_bound() {
        let state = test_state();
        let address = format!(
            "ipc://relay_unknown_length_large_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_request_kind_server(&address, "server-grid").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );

        let stream = futures::stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'a'; 32 * 1024])),
            Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'b'; 32 * 1024])),
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"c")),
        ]);
        let app = build_router(state);
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/grid/ping")
                        .header("content-type", "application/octet-stream"),
                )
                .body(Body::from_stream(stream))
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"], "ContentLengthRequired");

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_large_payload_uses_canonical_shm_ipc_request_path() {
        let state = test_state();
        let address = format!(
            "ipc://relay_large_payload_chunked_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_request_kind_server(&address, "server-grid").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );

        let payload = vec![b'x'; ClientIpcConfig::default().chunk_size as usize + 1];
        let content_length = payload.len().to_string();
        let route = state.local_route("grid");
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/grid/ping")
                        .header("content-type", "application/octet-stream")
                        .header("content-length", content_length),
                )
                .body(Body::from(payload.clone()))
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], payload.as_slice());
        let (lease, _, _) = state
            .acquire_upstream_for_route(route.as_ref().unwrap())
            .await
            .unwrap_or_else(|_| panic!("route remains acquirable"));
        let pool = unsafe { lease.client().request_pool_for_test() }.unwrap();
        assert!(
            pool.read().segment_count() > 0,
            "large request used the real SHM pool"
        );
        drop(lease);

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_reply_capacity_preserves_shared_connection_and_inflight_call() {
        use futures::FutureExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CapacityCallback {
            calls: [AtomicUsize; 3],
            blocked_started: tokio::sync::Notify,
            release: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        }

        impl CrmCallback for CapacityCallback {
            fn invoke(
                &self,
                _route_name: &str,
                method_idx: u16,
                request: Vec<u8>,
                _response_pool: Arc<c2_server::ResponsePool>,
            ) -> Result<ResponseMeta, CrmError> {
                // Consume through the real owner even when the HTTP request
                // reached us as SHM. No transport coordinates are discarded.
                let bytes = request;
                if bytes != [7; 32] {
                    return Err(CrmError::InternalError("unexpected request".into()));
                }
                self.calls[method_idx as usize].fetch_add(1, Ordering::SeqCst);
                if method_idx == 1 {
                    // Take the receiver before blocking; no synchronous guard
                    // survives the wait, and cleanup can always release us.
                    let release = self.release.lock().take().ok_or_else(|| {
                        CrmError::InternalError("blocked call was replayed".into())
                    })?;
                    self.blocked_started.notify_one();
                    release
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|err| {
                            CrmError::InternalError(format!("barrier release failed: {err}"))
                        })?;
                }
                Ok(ResponseMeta::Inline(vec![
                    method_idx as u8;
                    if method_idx == 0 {
                        2048
                    } else {
                        512
                    }
                ]))
            }
        }

        // Bind HTTP before starting any tasks, so a failed bind needs no
        // asynchronous cleanup. Exercise the router through a real listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let mut upstream_ipc = ClientIpcConfig::default();
        upstream_ipc.base.pool_segment_size = 64 * 1024;
        upstream_ipc.base.max_pool_segments = 2;
        upstream_ipc.base.reassembly_segment_size = 64 * 1024;
        upstream_ipc.base.reassembly_max_segments = 2;
        upstream_ipc.base.live_reassembly_budget_bytes = 1024;
        upstream_ipc.base.chunk_size = 256;
        upstream_ipc.shm_threshold = 1;
        upstream_ipc.validate().unwrap();
        let state = Arc::new(RelayState::new(
            Arc::new(c2_config::RelayConfig {
                upstream_ipc,
                idle_timeout_secs: 0,
                advertise_url: relay_url.clone(),
                ..c2_config::RelayConfig::default()
            }),
            Arc::new(crate::relay::test_support::NoopDisseminator),
        ));
        let mut server_config = ServerIpcConfig::default();
        // Both backing budgets are zero, forcing owned replies through the
        // real chunk fallback (eight 256-byte chunks for the rejected RID).
        server_config.base.shm_backing_budget_bytes = 0;
        server_config.base.file_backing_budget_bytes = 0;
        server_config.base.chunk_size = 256;
        server_config.shm_threshold = 1;
        server_config.max_execution_workers = 2;
        let address = format!(
            "ipc://relay_reply_capacity_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = Arc::new(
            Server::new_with_identity(
                &address,
                server_config,
                ServerIdentity {
                    server_id: "capacity-server".into(),
                    server_instance_id: "capacity-instance".into(),
                },
            )
            .unwrap(),
        );
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let callback = Arc::new(CapacityCallback {
            calls: std::array::from_fn(|_| AtomicUsize::new(0)),
            blocked_started: tokio::sync::Notify::new(),
            release: parking_lot::Mutex::new(Some(release_rx)),
        });
        let route = server
            .build_route(
                RouteBuildSpec {
                    name: "grid".into(),
                    crm_ns: "test.echo".into(),
                    crm_name: "Echo".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["oversized".into(), "blocked".into(), "ping".into()],
                    access_map: std::collections::HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                callback.clone(),
            )
            .unwrap();
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let run_server = server.clone();
        let mut ipc_task = tokio::spawn(async move { run_server.run().await });
        let (http_stop_tx, http_stop_rx) = tokio::sync::oneshot::channel();
        let app = build_router(state.clone());
        let mut http_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = http_stop_rx.await;
                })
                .await
        });

        // Run assertions inside a caught future so failures also release the
        // barrier, retire the upstream, and join both listener tasks.
        let mut original_client = None;
        let result = std::panic::AssertUnwindSafe(async {
            server.wait_until_responsive(Duration::from_secs(2)).await.unwrap();
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            let registered = http.post(format!("{relay_url}/_register"))
                .json(&serde_json::json!({
                    "name": "grid",
                    "server_id": "capacity-server",
                    "server_instance_id": "capacity-instance",
                    "address": address,
                    "max_payload_size": server.config().max_payload_size,
                }))
                .send().await.unwrap();
            assert_eq!(registered.status(), StatusCode::CREATED);
            let _ = registered.bytes().await.unwrap();
            let route = state.local_route("grid").unwrap();
            let (lease, _) = state.acquire_upstream("grid").await
                .unwrap_or_else(|_| panic!("initial upstream acquisition failed"));
            let client = lease.client();
            original_client = Some(client.clone());
            drop(lease);
            let request = |method: &str| {
                http.post(format!("{relay_url}/grid/{method}"))
                    .header(EXPECTED_CRM_NS_HEADER, "test.echo")
                    .header(EXPECTED_CRM_NAME_HEADER, "Echo")
                    .header(EXPECTED_CRM_VER_HEADER, "0.1.0")
                    .header(EXPECTED_ABI_HASH_HEADER, TEST_ABI_HASH)
                    .header(EXPECTED_SIGNATURE_HASH_HEADER, TEST_SIGNATURE_HASH)

                    .body(vec![7; 32])
            };
            let blocked = request("blocked").send();
            tokio::pin!(blocked);
            tokio::select! {
                _ = callback.blocked_started.notified() => {}
                response = &mut blocked => panic!("blocked call completed before barrier: {response:?}"),
                _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("blocked call did not reach IPC"),
            }
            let (lease, _) = state.acquire_upstream("grid").await
                .unwrap_or_else(|_| panic!("upstream acquisition failed while blocked"));
            assert!(Arc::ptr_eq(&client, &lease.client()));
            drop(lease);
            let rejected = request("oversized").send().await.unwrap();
            assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);
            let envelope = rejected.json::<c2_error::C2ErrorEnvelope>().await.unwrap();
            assert_eq!(envelope.code, 702);
            assert_eq!(envelope.name, "ResourceUnavailable");
            assert_eq!(envelope.details["dispatch_phase"], "dispatch_uncertain");
            assert_eq!(envelope.details["route"], "grid");
            assert_eq!(c2_error::C2Error::from_envelope(envelope).unwrap().code,
                c2_error::ErrorCode::ResourceUnavailable);
            let budget = state.upstream_memory_snapshot();
            assert_eq!(budget.reassembly.limit_bytes, 1024);
            assert_eq!(budget.reassembly.rejected_allocations, 1,
                "must hit real reply admission, not a synthetic transport error");
            assert_eq!(budget.reassembly.used_bytes, 0);
            assert!(budget.shm.peak_bytes > 0, "requests must use real owned SHM");
            assert_eq!(callback.calls.each_ref().map(|n| n.load(Ordering::SeqCst)), [1, 1, 0]);

            // The original pooled connection must survive, while the other
            // RID is still in flight. The old generic eviction fails here.
            let (lease, current_route) = state.acquire_upstream("grid").await
                .unwrap_or_else(|_| panic!("upstream acquisition failed after capacity rejection"));
            assert!(Arc::ptr_eq(&client, &lease.client()), "capacity must not evict the shared client");
            assert!(client.is_connected());
            assert_eq!(current_route.route_uid, route.route_uid);
            assert_eq!(UpstreamEndpointKey::from_route(&current_route), UpstreamEndpointKey::from_route(&route));
            assert!(route_matches_expected_crm(&current_route, &c2_contract::ExpectedRouteContract {
                route_name: route.name.clone(),
                crm_ns: route.crm_ns.clone(),
                crm_name: route.crm_name.clone(),
                crm_ver: route.crm_ver.clone(),
                abi_hash: route.abi_hash.clone(),
                signature_hash: route.signature_hash.clone(),
            }));
            drop(lease);
            release_tx.send(()).unwrap();
            let completed = blocked.await.unwrap();
            assert_eq!(completed.status(), StatusCode::OK);
            assert_eq!(completed.bytes().await.unwrap().as_ref(), &[1; 512]);
            let ping_address = address.clone();
            assert!(tokio::task::spawn_blocking(move || {
                c2_ipc::ping(&ping_address, Duration::from_secs(1))
            }).await.unwrap().unwrap());
            let pong = request("ping").send().await.unwrap();
            assert_eq!(pong.status(), StatusCode::OK);
            assert_eq!(pong.bytes().await.unwrap().as_ref(), &[2; 512]);
            let (lease, final_route) = state.acquire_upstream("grid").await
                .unwrap_or_else(|_| panic!("upstream acquisition failed after subsequent call"));
            assert!(Arc::ptr_eq(&client, &lease.client()));
            assert!(client.is_connected());
            assert_eq!(final_route.route_uid, route.route_uid);
            drop(lease);
            assert_eq!(callback.calls.each_ref().map(|n| n.load(Ordering::SeqCst)), [1, 1, 1],
                "capacity rejection must not replay either dispatched call");
            assert_eq!(state.upstream_memory_snapshot().reassembly.used_bytes, 0);
        }).catch_unwind().await;

        let _ = release_tx.send(());
        if let crate::relay::state::UnregisterResult::Removed {
            client: Some(client),
            ..
        } = state.unregister_upstream("grid", "capacity-server")
        {
            client.close_shared().await;
        }
        if let Some(client) = original_client {
            client.close_shared().await;
        }
        let _ = http_stop_tx.send(());
        let ipc_stopped = server.shutdown_and_wait(Duration::from_secs(2)).await;
        let http_stopped = tokio::time::timeout(Duration::from_secs(2), &mut http_task).await;
        if http_stopped.is_err() {
            http_task.abort();
            let _ = http_task.await;
        }
        let ipc_joined = tokio::time::timeout(Duration::from_secs(2), &mut ipc_task).await;
        if ipc_joined.is_err() {
            ipc_task.abort();
            let _ = ipc_task.await;
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        http_stopped.unwrap().unwrap().unwrap();
        ipc_stopped.unwrap();
        ipc_joined.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn relay_response_cursor_uses_charged_remote_payload_chunks() {
        let budget = PayloadBufferBudget::new(3);
        let response = super::super::streaming::unary_response(
            c2_ipc::ResponseData::Inline(b"abcdefg".to_vec()),
            StatusCode::OK,
            budget.try_reserve(3).unwrap(),
            budget.clone(),
            3,
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "7");

        let mut stream = response.into_body().into_data_stream();
        let mut chunks = Vec::new();
        while let Some(next) = stream.next().await {
            chunks.push(next.expect("body chunk").to_vec());
        }

        assert_eq!(
            chunks,
            vec![b"abc".to_vec(), b"def".to_vec(), b"g".to_vec()]
        );
    }

    #[tokio::test]
    async fn call_with_expected_crm_tag_rejects_route_tag_mismatch_before_forwarding() {
        let state = test_state();
        let address = format!(
            "ipc://relay_call_wrong_crm_tag_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[(
                "grid",
                "test.other",
                "OtherEcho",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );

        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grid/ping")
                    .header("content-type", "application/octet-stream")
                    .header("x-c2-expected-crm-ns", "test.echo")
                    .header("x-c2-expected-crm-name", "Echo")
                    .header("x-c2-expected-crm-ver", "0.1.0")
                    .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
                    .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["code"], 709);
        assert_eq!(body["name"], "ContractMismatch");
        assert_eq!(body["details"]["route"], "grid");

        shutdown_live_server(&server).await;
    }

    fn install_route_swap_after_precheck(
        state: Arc<RelayState>,
        route_name: &str,
        new_address: &str,
        crm_ns: &'static str,
        crm_name: &'static str,
        max_payload_size: u64,
    ) {
        let state_for_hook = state.clone();
        let route_for_hook = route_name.to_string();
        let new_address_for_hook = new_address.to_string();
        set_data_plane_after_precheck_hook(&state, route_name.to_string(), move || {
            if let crate::relay::state::UnregisterResult::Removed { client, .. } =
                state_for_hook.unregister_upstream(&route_for_hook, "server-old")
                && let Some(client) = client
            {
                tokio::spawn(async move { client.close_shared().await });
            }
            match test_commit_registration!(
                &state_for_hook,
                route_for_hook.clone(),
                "server-new".into(),
                "server-new-instance".into(),
                new_address_for_hook,
                crm_ns.into(),
                crm_name.into(),
                "0.1.0".into(),
                TEST_ABI_HASH.to_string(),
                TEST_SIGNATURE_HASH.to_string(),
                max_payload_size,
                format!("{route_for_hook}-server-new-uid"),
                None,
            ) {
                RegisterCommitResult::Registered { .. }
                | RegisterCommitResult::SameOwner { .. } => {}
                RegisterCommitResult::Duplicate { existing_address }
                | RegisterCommitResult::ConflictingOwner { existing_address } => {
                    panic!(
                        "failed to install replacement route in precheck hook: {existing_address}"
                    )
                }
                RegisterCommitResult::PeerRouteHeld { registrations } => {
                    panic!("unexpected peer-held name: {registrations}")
                }
                RegisterCommitResult::Invalid { reason } => {
                    panic!("failed to install replacement route in precheck hook: {reason}")
                }
            }
        });
    }

    #[tokio::test]
    async fn call_uses_current_registration_after_precheck() {
        let state = test_state();
        let suffix = unique_suffix();
        let route_name = format!("grid-token-toctou-call-{suffix}");
        let old_address = format!(
            "ipc://relay_call_token_old_{}_{}",
            std::process::id(),
            suffix
        );
        let new_address = format!(
            "ipc://relay_call_token_new_{}_{}",
            std::process::id(),
            suffix
        );
        let old_server = start_marker_server(
            &old_address,
            "server-old",
            "server-old-instance",
            &route_name,
            b"old-route",
        )
        .await;
        let new_server = start_marker_server(
            &new_address,
            "server-new",
            "server-new-instance",
            &route_name,
            b"new-route",
        )
        .await;

        assert_eq!(
            post_register(state.clone(), &route_name, "server-old", &old_address).await,
            StatusCode::CREATED
        );
        install_route_swap_after_precheck(
            state.clone(),
            &route_name,
            &new_address,
            "test.echo",
            "Echo",
            1024,
        );

        let (status, body) = post_call_response(state.clone(), &route_name, "ping").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"new-route");

        shutdown_live_server(&old_server).await;
        shutdown_live_server(&new_server).await;
    }

    // Gate 2: "没有在线注册时，调用在派发前失败，不进入任何资源".
    // A replaced registration's smaller limit is also decided before the sole OPEN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_payload_limit_is_checked_before_dispatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountCalls(Arc<AtomicUsize>);
        impl CrmCallback for CountCalls {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _: Vec<u8>,
                _: Arc<c2_server::ResponsePool>,
            ) -> Result<ResponseMeta, CrmError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ResponseMeta::Inline(Vec::new()))
            }
        }
        #[cfg(unix)]
        let root = crate::relay::test_support::EndpointTestRoot::new();
        #[cfg(unix)]
        let context = root.context();
        #[cfg(windows)]
        let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
        for known_length in [true, false] {
            let suffix = unique_suffix();
            let route = format!("payload-replacement-{suffix}");
            let old_address = format!("ipc://limit-old-{suffix}");
            let new_address = format!("ipc://limit-new-{suffix}");
            let calls = Arc::new(AtomicUsize::new(0));
            let old = start_callback_server_in_context(
                &context,
                &old_address,
                "server-old",
                "server-old-instance",
                &route,
                Arc::new(CountCalls(calls.clone())),
            )
            .await;
            let new = start_callback_server_in_context(
                &context,
                &new_address,
                "server-new",
                "server-new-instance",
                &route,
                Arc::new(CountCalls(calls.clone())),
            )
            .await;
            let state = namespace_test_state(context.clone());
            let clients = state.clients.clone();
            let observer = tokio::spawn(async move {
                clients.run().await;
            });
            // Test-owned registration attestation, without HTTP namespace-dependent setup.
            assert!(matches!(
                test_commit_registration!(
                    &state,
                    route.clone(),
                    "server-old".into(),
                    "server-old-instance".into(),
                    old_address,
                    "test.echo".into(),
                    "Echo".into(),
                    "0.1.0".into(),
                    TEST_ABI_HASH.into(),
                    TEST_SIGNATURE_HASH.into(),
                    1024,
                    format!("old-{suffix}"),
                    None
                ),
                RegisterCommitResult::Registered { .. }
            ));
            install_route_swap_after_precheck(
                state.clone(),
                &route,
                &new_address,
                "test.echo",
                "Echo",
                3,
            );
            let mut request = add_expected_echo_headers(
                Request::builder()
                    .method("POST")
                    .uri(format!("/{route}/ping")),
            );
            let body = if known_length {
                request = request.header("content-length", 4);
                Body::from("data")
            } else {
                Body::from_stream(futures::stream::iter([Ok::<_, std::io::Error>(
                    Bytes::from_static(b"data"),
                )]))
            };
            let response = build_router(state.clone())
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let error: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(error["max_payload_size"], 3);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "neither registration may run"
            );
            state.stop_upstream_controls().await;
            tokio::time::timeout(Duration::from_secs(5), state.clients.shutdown())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), observer)
                .await
                .unwrap()
                .unwrap();
            shutdown_live_server(&old).await;
            shutdown_live_server(&new).await;
        }
    }

    async fn mismatched_route_replacement_after_precheck(probe: bool) {
        let state = test_state();
        let suffix = unique_suffix();
        let route_name = format!("grid-toctou-{suffix}");
        let old_address = format!("ipc://relay_toctou_old_{}_{}", std::process::id(), suffix);
        let new_address = format!("ipc://relay_toctou_new_{}_{}", std::process::id(), suffix);
        let old_server = start_live_server_with_identity_and_contracts(
            &old_address,
            "server-old",
            "server-old-instance",
            &[(
                &route_name,
                "test.echo",
                "Echo",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;
        let new_server = start_live_server_with_identity_and_contracts(
            &new_address,
            "server-new",
            "server-new-instance",
            &[(
                &route_name,
                "test.other",
                "OtherEcho",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        assert_eq!(
            post_register(state.clone(), &route_name, "server-old", &old_address).await,
            StatusCode::CREATED
        );
        install_route_swap_after_precheck(
            state.clone(),
            &route_name,
            &new_address,
            "test.other",
            "OtherEcho",
            1024,
        );

        let (status, body) = if probe {
            get_probe_response(state.clone(), &route_name).await
        } else {
            post_call_response(state.clone(), &route_name, "ping").await
        };
        assert_eq!(status, StatusCode::CONFLICT);
        let error: c2_error::C2ErrorEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.version, 1);
        assert_eq!(error.code, 709);
        assert_eq!(error.name, "ContractMismatch");
        assert_eq!(error.details["route"], route_name);

        shutdown_live_server(&old_server).await;
        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn call_rejects_contract_replacement_after_precheck() {
        mismatched_route_replacement_after_precheck(false).await;
    }

    #[tokio::test]
    async fn probe_rejects_contract_replacement_after_precheck() {
        mismatched_route_replacement_after_precheck(true).await;
    }

    #[tokio::test]
    async fn register_allows_replacement_when_idle_evicted_server_no_longer_serves_route() {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_route_removed_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let new_address = format!(
            "ipc://relay_route_replacement_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let old_server = start_live_server(&old_address, "server-old").await;
        register_echo_route(&old_server, "counter").await;
        let new_server = start_live_server(&new_address, "server-new").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        assert!(old_server.unregister_route("grid").await);

        assert_eq!(
            post_register(state.clone(), "grid", "server-new", &new_address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            state.get_address("grid").as_deref(),
            Some(new_address.as_str())
        );

        shutdown_live_server(&old_server).await;
        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn final_replacement_confirmation_rejects_silent_recovered_owner() {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_final_probe_old_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let candidate_address = format!(
            "ipc://relay_final_probe_candidate_{}_{}",
            std::process::id(),
            unique_suffix()
        );

        let old_server = start_live_server(&old_address, "server-old").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let preliminary = RouteAuthority::new(&state)
            .prepare_register(
                "grid",
                "server-new",
                "server-new-instance",
                &candidate_address,
            )
            .await
            .expect("preliminary replacement should be allowed");
        let replacement = match preliminary {
            RegisterPreparation::Available {
                replacement: Some(replacement),
            } => replacement,
            _ => panic!("expected replacement candidate"),
        };

        let recovered_old = start_live_server(&old_address, "server-old").await;
        let confirmed = RouteAuthority::new(&state)
            .confirm_replacement_for_commit(Some(replacement))
            .await;

        assert!(matches!(
            confirmed,
            Err(ControlError::DuplicateRoute { existing_address })
                if existing_address == old_address
        ));

        shutdown_live_server(&recovered_old).await;
    }

    #[tokio::test]
    async fn final_replacement_confirmation_allows_still_dead_owner() {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_final_probe_dead_old_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let candidate_address = format!(
            "ipc://relay_final_probe_dead_candidate_{}_{}",
            std::process::id(),
            unique_suffix()
        );

        let old_server = start_live_server(&old_address, "server-old").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let preliminary = RouteAuthority::new(&state)
            .prepare_register(
                "grid",
                "server-new",
                "server-new-instance",
                &candidate_address,
            )
            .await
            .expect("preliminary replacement should be allowed");
        let replacement = match preliminary {
            RegisterPreparation::Available {
                replacement: Some(replacement),
            } => replacement,
            _ => panic!("expected replacement candidate"),
        };

        let confirmed = RouteAuthority::new(&state)
            .confirm_replacement_for_commit(Some(replacement))
            .await
            .expect("dead owner should confirm replacement");

        assert!(confirmed.is_some());
    }

    #[tokio::test]
    async fn final_replacement_confirmation_rejects_silent_recovered_owner_from_command_preparation()
     {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_final_probe_command_old_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let candidate_address = format!(
            "ipc://relay_final_probe_command_candidate_{}_{}",
            std::process::id(),
            unique_suffix()
        );

        let old_server = start_live_server(&old_address, "server-old").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let replacement = RouteAuthority::new(&state)
            .prepare_candidate_registration("grid", "server-new", &candidate_address)
            .await
            .expect("preliminary command-loop replacement should be allowed")
            .expect("expected replacement candidate");

        let recovered_old = start_live_server(&old_address, "server-old").await;
        let confirmed = RouteAuthority::new(&state)
            .confirm_replacement_for_commit(Some(replacement))
            .await;

        assert!(matches!(
            confirmed,
            Err(ControlError::DuplicateRoute { existing_address })
                if existing_address == old_address
        ));

        shutdown_live_server(&recovered_old).await;
    }

    #[tokio::test]
    async fn probe_generic_io_unreachable_upstream_keeps_local_route() {
        let state = test_state();
        let stale_address = format!(
            "ipc://relay_probe_stale_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let stale_server = start_live_server(&stale_address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &stale_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&stale_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(
            get_probe(state.clone(), "grid").await,
            StatusCode::BAD_GATEWAY
        );
        assert!(state.local_route("grid").is_some());
        assert_eq!(
            get_probe(state.clone(), "grid").await,
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn probe_missing_upstream_route_removes_stale_local_route() {
        let state = test_state();
        let owner_address = format!(
            "ipc://relay_probe_missing_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        // The owner is alive and answers catalog lookups but never served this
        // route, so the authoritative lookup must answer never-found. The
        // local route is committed directly so the background control watch
        // cannot withdraw it first: the data-plane acquisition is the only
        // actor that observes the stale route.
        let owner_server =
            start_live_server_with_routes(&owner_address, "server-grid", &["other"]).await;

        match test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            owner_address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            1024,
            "grid-never-served-uid".into(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected registration result for never-found stale route"),
        }

        let (status, body) = get_probe_response(state.clone(), "grid").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], 701);
        assert_eq!(body["name"], "ResourceNotFound");
        assert_eq!(body["message"], "route not found: grid");
        assert_eq!(body["details"]["route"], "grid");
        assert!(
            state.local_route("grid").is_none(),
            "never-found upstream route is semantic withdrawal evidence"
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "Echo", "0.1.0").await,
            StatusCode::NOT_FOUND
        );

        shutdown_live_server(&owner_server).await;
    }

    #[tokio::test]
    async fn probe_known_removed_upstream_route_removes_stale_local_route() {
        let state = test_state();
        let owner_address = format!(
            "ipc://relay_probe_removed_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let owner_server = start_live_server(&owner_address, "server-grid").await;

        // Capture the live route identity before removal. The server
        // The directory UID is used only to register the route with the relay.
        // Its later removal is reported publicly by route name.
        let mut attested =
            c2_ipc::IpcClient::with_config(&owner_address, ClientIpcConfig::default()).unwrap();
        attested
            .connect()
            .await
            .expect("attestation client connects");
        let contract = attested
            .route_contract("grid")
            .expect("server exports grid");
        let table = attested
            .attest_route_for_registration(&contract)
            .await
            .unwrap();
        let route_uid = table.route_uid.clone();
        let max_payload_size = table.max_payload_size;
        attested.close().await;

        assert!(owner_server.unregister_route("grid").await);

        // An explicit unregister leaves a tombstone, so the authoritative
        // lookup answers known-removed (410), not never-found (404). The
        // local route is committed after the removal so the data-plane
        // acquisition deterministically observes the tombstone.
        match test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            owner_address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            max_payload_size,
            route_uid.clone(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected registration result for known-removed stale route"),
        }

        let (status, body) = get_probe_response(state.clone(), "grid").await;
        assert_eq!(status, StatusCode::GONE);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], 708);
        assert_eq!(body["name"], "ResourceRemoved");
        assert_eq!(body["message"], "route removed: grid");
        assert_eq!(body["details"]["route"], "grid");
        assert!(body["details"].get("route_uid").is_none());
        assert!(
            state.local_route("grid").is_none(),
            "known-removed upstream route is semantic withdrawal evidence"
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "Echo", "0.1.0").await,
            StatusCode::NOT_FOUND
        );

        shutdown_live_server(&owner_server).await;
    }

    #[tokio::test]
    async fn probe_reconnect_rejects_same_route_name_with_different_server_instance_id() {
        let state = test_state();
        let address = format!(
            "ipc://relay_probe_wrong_instance_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let old_server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let new_server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-restarted",
            &[(
                "grid",
                "test.echo",
                "Echo",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        wait_for_local_route_removed(&state, "grid").await;
        assert_eq!(
            get_probe(state.clone(), "grid").await,
            StatusCode::NOT_FOUND
        );
        assert!(state.local_route("grid").is_none());

        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn probe_reconnect_rejects_same_route_name_with_different_crm_tag() {
        let state = test_state();
        let address = format!(
            "ipc://relay_probe_wrong_crm_tag_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let old_server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let new_server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[(
                "grid",
                "test.other",
                "OtherEcho",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;

        wait_for_local_route_removed(&state, "grid").await;
        assert_eq!(
            get_probe(state.clone(), "grid").await,
            StatusCode::NOT_FOUND
        );
        assert!(state.local_route("grid").is_none());

        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn call_known_removed_upstream_route_removes_stale_local_route() {
        let state = test_state();
        let owner_address = format!(
            "ipc://relay_call_removed_route_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let owner_server = start_live_server(&owner_address, "server-grid").await;

        let mut attested =
            c2_ipc::IpcClient::with_config(&owner_address, ClientIpcConfig::default()).unwrap();
        attested
            .connect()
            .await
            .expect("attestation client connects");
        let contract = attested
            .route_contract("grid")
            .expect("server exports grid");
        let table = attested
            .attest_route_for_registration(&contract)
            .await
            .unwrap();
        let route_uid = table.route_uid.clone();
        let max_payload_size = table.max_payload_size;
        attested.close().await;

        assert!(owner_server.unregister_route("grid").await);

        // The explicit unregister leaves a tombstone, so the call acquisition
        // fails pre-dispatch with known-removed evidence and withdraws the
        // stale local route. Committing the local route after the removal
        // keeps the fixture independent of control-watch timing.
        match test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            owner_address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            max_payload_size,
            route_uid.clone(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected registration result for known-removed stale route"),
        }

        // The canonical ResourceRemoved envelope is also the no-dispatch
        // proof: a dispatched call would have returned 200 with the echo body.
        let (status, body) = post_call_response(state.clone(), "grid", "ping").await;
        assert_eq!(status, StatusCode::GONE);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], 708);
        assert_eq!(body["name"], "ResourceRemoved");
        assert_eq!(body["message"], "route removed: grid");
        assert_eq!(body["details"]["route"], "grid");
        assert!(body["details"].get("route_uid").is_none());
        assert!(
            state.local_route("grid").is_none(),
            "known-removed upstream route is semantic withdrawal evidence"
        );
        assert_eq!(
            get_resolve_with_crm_tag(state.clone(), "grid", "test.echo", "Echo", "0.1.0").await,
            StatusCode::NOT_FOUND
        );

        shutdown_live_server(&owner_server).await;
    }

    #[tokio::test]
    async fn register_rejects_different_address_when_idle_evicted_owner_is_alive() {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_old_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let new_address = format!(
            "ipc://relay_new_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let old_server = start_live_server(&old_address, "server-old").await;
        let new_server = start_live_server(&new_address, "server-new").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");

        assert_eq!(
            post_register(state.clone(), "grid", "server-new", &new_address).await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            state.get_address("grid").as_deref(),
            Some(old_address.as_str())
        );

        shutdown_live_server(&old_server).await;
        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn register_allows_different_address_when_idle_evicted_owner_is_dead() {
        let state = test_state();
        let old_address = format!(
            "ipc://relay_dead_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let new_address = format!(
            "ipc://relay_replacement_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let old_server = start_live_server(&old_address, "server-old").await;
        let new_server = start_live_server(&new_address, "server-new").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-old", &old_address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");
        shutdown_live_server(&old_server).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-new", &new_address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            state.get_address("grid").as_deref(),
            Some(new_address.as_str())
        );

        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn concurrent_calls_after_idle_eviction_do_not_report_unreachable() {
        let state = test_state();
        let address = format!(
            "ipc://relay_concurrent_reconnect_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        state.evict_connection("grid");

        let app = build_router(state.clone());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                let response = app
                    .oneshot(
                        add_expected_echo_headers(
                            Request::builder()
                                .method("POST")
                                .uri("/grid/ping")
                                .header("content-type", "application/octet-stream"),
                        )
                        .body(Body::from(Vec::new()))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                response.status()
            }));
        }

        let mut statuses = Vec::new();
        for task in tasks {
            statuses.push(task.await.unwrap());
        }

        assert!(
            statuses.iter().all(|status| *status == StatusCode::OK),
            "all concurrent requests should succeed after one task reconnects, got {statuses:?}"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_late_route_survives_existing_endpoint_and_idle_eviction() {
        let state = test_state();
        let address = format!(
            "ipc://relay_late_route_idle_reconnect_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_routes(&address, "server-grid", &["manager"]).await;

        assert_eq!(
            post_register(state.clone(), "manager", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            post_call(state.clone(), "manager", "ping").await,
            StatusCode::OK
        );

        register_echo_route(&server, "builder").await;
        assert_eq!(
            post_register(state.clone(), "builder", "server-grid", &address).await,
            StatusCode::CREATED
        );

        assert_eq!(
            post_call(state.clone(), "builder", "ping").await,
            StatusCode::OK,
            "existing endpoint connection must authoritative-acquire routes registered after handshake"
        );

        state.evict_connection("builder");
        assert_eq!(
            post_call(state.clone(), "builder", "ping").await,
            StatusCode::OK,
            "idle-evicted endpoint must reconnect and keep late route usable"
        );
        assert!(state.local_route("manager").is_some());
        assert!(state.local_route("builder").is_some());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_upstream_stale_snapshot_acquires_current_route_by_name() {
        let state = test_state_for_client();
        let address = format!(
            "ipc://relay_upstream_live_route_refresh_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server_with_routes(&address, "server-grid", &["manager"]).await;

        let mut stale_client = state
            .clients
            .manage(|| {
                c2_ipc::IpcClient::with_config(&address, c2_config::ClientIpcConfig::default())
            })
            .expect("stale snapshot client admission");
        stale_client.connect().await.expect("stale client connects");
        assert!(stale_client.has_route("manager"));
        assert!(!stale_client.has_route("builder"));

        match test_commit_registration!(
            &state,
            "builder".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            ServerIpcConfig::default().max_payload_size,
            "builder-server-grid-uid".into(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            RegisterCommitResult::SameOwner { .. } => panic!("unexpected same-owner result"),
            RegisterCommitResult::Duplicate { existing_address }
            | RegisterCommitResult::ConflictingOwner { existing_address } => {
                panic!("unexpected duplicate route at {existing_address}")
            }
            RegisterCommitResult::PeerRouteHeld { registrations } => {
                panic!("unexpected peer-held name: {registrations}")
            }
            RegisterCommitResult::Invalid { reason } => {
                panic!("unexpected invalid route in test: {reason}")
            }
        }
        state.reconnect("builder", stale_client.into_shared());
        register_echo_route(&server, "builder").await;

        let app = build_router(state.clone());
        let response = app
            .oneshot(
                add_expected_echo_headers(
                    Request::builder()
                        .method("POST")
                        .uri("/builder/ping")
                        .header("content-type", "application/octet-stream"),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"echo");
        assert!(state.local_route("builder").is_some());

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_upstream_watch_removes_local_route_without_data_plane_call() {
        let state = test_state_for_client();
        let address = format!(
            "ipc://relay_upstream_watch_remove_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert!(state.local_route("grid").is_some());

        assert!(server.unregister_route("grid").await);

        wait_for_local_route_removed(&state, "grid").await;

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn relay_probe_does_not_trust_cached_data_plane_after_control_watch_unavailable() {
        let state = test_state_for_client();
        let address = format!(
            "ipc://relay_watch_unavailable_cached_probe_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        let mut attested =
            c2_ipc::IpcClient::with_config(&address, c2_config::ClientIpcConfig::default())
                .unwrap();
        attested
            .connect()
            .await
            .expect("attestation client connects");
        let contract = attested
            .route_contract("grid")
            .expect("server exports grid");
        let table = attested
            .attest_route_for_registration(&contract)
            .await
            .unwrap();
        attested.close().await;

        match test_commit_registration!(
            &state,
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            table.max_payload_size,
            table.route_uid.clone(),
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            RegisterCommitResult::SameOwner { .. } => panic!("unexpected same-owner result"),
            RegisterCommitResult::Duplicate { existing_address }
            | RegisterCommitResult::ConflictingOwner { existing_address } => {
                panic!("unexpected duplicate route at {existing_address}")
            }
            RegisterCommitResult::PeerRouteHeld { registrations } => {
                panic!("unexpected peer-held name: {registrations}")
            }
            RegisterCommitResult::Invalid { reason } => {
                panic!("unexpected invalid route in test: {reason}")
            }
        }

        let mut stale_data_client = state
            .clients
            .manage(|| {
                c2_ipc::IpcClient::with_config(&address, c2_config::ClientIpcConfig::default())
            })
            .expect("stale data client admission");
        stale_data_client
            .connect()
            .await
            .expect("stale data-plane client connects");
        stale_data_client.close().await;
        state.reconnect("grid", stale_data_client.into_shared());

        let route = state.local_route("grid").expect("relay route registered");
        let key = crate::relay::upstream_control::owner_key_for_route(&route)
            .expect("local route has owner key");
        state.mark_upstream_control_watch_unavailable(&key, "watch stream closed");

        let (status, body) = get_probe_response(state.clone(), "grid").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = String::from_utf8(body).unwrap();
        assert!(
            body.contains("RouteWatchUnavailable"),
            "watch-unavailable route must not be reported as generic success or route removal: {body}"
        );
        assert!(
            state.local_route("grid").is_some(),
            "watch unavailability alone must not withdraw the route"
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn concurrent_register_different_addresses_keeps_single_owner() {
        let state = test_state();
        let first_address = format!(
            "ipc://relay_race_first_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let second_address = format!(
            "ipc://relay_race_second_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let first_server = start_live_server(&first_address, "server-first").await;
        let second_server = start_live_server(&second_address, "server-second").await;

        let first = {
            let state = state.clone();
            let first_address = first_address.clone();
            tokio::spawn(async move {
                post_register(state, "grid", "server-first", &first_address).await
            })
        };
        let second = {
            let state = state.clone();
            let second_address = second_address.clone();
            tokio::spawn(async move {
                post_register(state, "grid", "server-second", &second_address).await
            })
        };

        let statuses = vec![first.await.unwrap(), second.await.unwrap()];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CREATED)
                .count(),
            1,
            "exactly one registration should create the route, got {statuses:?}"
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count(),
            1,
            "exactly one registration should be rejected as duplicate, got {statuses:?}"
        );

        let final_address = state.get_address("grid").expect("route should exist");
        assert!(
            final_address == first_address || final_address == second_address,
            "final route owner should be one of the racing addresses, got {final_address}"
        );

        shutdown_live_server(&first_server).await;
        shutdown_live_server(&second_server).await;
    }

    #[tokio::test]
    async fn unregister_rejects_wrong_server_id_without_removing_route() {
        let state = test_state();
        let address = format!(
            "ipc://relay_unregister_owner_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );

        assert_eq!(
            post_unregister(state.clone(), "grid", "server-other").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(state.get_address("grid").as_deref(), Some(address.as_str()));

        assert_eq!(
            post_unregister(state.clone(), "grid", "server-grid").await,
            StatusCode::OK
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn unregister_is_idempotent_after_success_for_same_server_id() {
        let state = test_state();
        let address = format!(
            "ipc://relay_unregister_idempotent_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            post_unregister(state.clone(), "grid", "server-grid").await,
            StatusCode::OK
        );
        assert_eq!(
            post_unregister(state.clone(), "grid", "server-grid").await,
            StatusCode::OK
        );

        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn unregister_tombstone_does_not_authorize_different_server_id() {
        let state = test_state();
        let address = format!(
            "ipc://relay_unregister_wrong_idempotent_{}_{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-grid").await;

        assert_eq!(
            post_register(state.clone(), "grid", "server-grid", &address).await,
            StatusCode::CREATED
        );
        assert_eq!(
            post_unregister(state.clone(), "grid", "server-grid").await,
            StatusCode::OK
        );
        assert_eq!(
            post_unregister(state.clone(), "grid", "server-other").await,
            StatusCode::NOT_FOUND
        );

        shutdown_live_server(&server).await;
    }

    fn unique_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
    async fn post_registration_withdrawal(
        state: Arc<RelayState>,
        scope: &crate::client::RelayRegistrationScope,
    ) -> StatusCode {
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_unregister")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(scope).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        status
    }

    fn captured_scope(
        entry: &crate::relay::types::RouteEntry,
    ) -> crate::client::RelayRegistrationScope {
        crate::client::RelayRegistrationScope {
            name: entry.name.clone(),
            server_id: entry.server_id.clone().unwrap(),
            server_instance_id: entry.server_instance_id.clone().unwrap(),
            route_uid: entry.route_uid.clone().expect("local UID"),
        }
    }

    #[tokio::test]
    async fn captured_registration_withdrawal_cannot_remove_new_uid_in_same_server_instance() {
        let state = test_state();
        let address = format!(
            "ipc://scope-withdraw-{}-{}",
            std::process::id(),
            unique_suffix()
        );
        let server = start_live_server(&address, "server-scope").await;
        assert_eq!(
            post_register(state.clone(), "grid", "server-scope", &address).await,
            StatusCode::CREATED
        );
        let old = captured_scope(&state.route_table_write().local_route("grid").unwrap());
        assert!(server.unregister_route("grid").await);
        crate::relay::test_support::register_echo_route(&server, "grid").await;
        assert!(matches!(
            post_register(state.clone(), "grid", "server-scope", &address).await,
            StatusCode::CREATED | StatusCode::OK
        ));
        let new = captured_scope(&state.route_table_write().local_route("grid").unwrap());
        assert_eq!(old.server_instance_id, new.server_instance_id);
        assert_ne!(old.route_uid, new.route_uid);
        assert_eq!(
            post_registration_withdrawal(state.clone(), &old).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            state
                .route_table_write()
                .local_route("grid")
                .unwrap()
                .route_uid,
            Some(new.route_uid.clone())
        );
        let mut wrong_instance = new.clone();
        wrong_instance.server_instance_id.push_str("-old");
        assert_eq!(
            post_registration_withdrawal(state.clone(), &wrong_instance).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            post_registration_withdrawal(state.clone(), &new).await,
            StatusCode::OK
        );
        assert_eq!(
            post_registration_withdrawal(state.clone(), &new).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            post_registration_withdrawal(state.clone(), &old).await,
            StatusCode::NOT_FOUND
        );
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn ordinary_registration_withdrawal_refuses_name_only_body() {
        let response = build_router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_unregister")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"grid","server_id":"server-grid"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[cfg(test)]
mod forwarding_admission_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct BodyDrop(Arc<AtomicBool>);

    impl Drop for BodyDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn assert_capacity_response(response: Response) {
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["code"], 717);
        assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
        assert_eq!(error["message"], "relay forwarding capacity exceeded");
    }

    #[tokio::test]
    async fn rejected_body_expect_and_unknown_length_are_never_polled() {
        for (expect, length) in [(true, Some(4)), (false, None)] {
            let body = Body::from_stream(futures::stream::once(async {
                panic!("ineligible rejected body must not be polled");
                #[allow(unreachable_code)]
                Ok::<Bytes, std::io::Error>(Bytes::new())
            }));
            let response = finish_early_rejection(
                expect,
                length,
                body,
                forwarding_capacity_response("grid", "original refusal"),
            )
            .await;
            assert_capacity_response(response).await;
        }
    }

    #[tokio::test]
    async fn rejected_body_excess_short_and_source_fault_preserve_error() {
        for ending in ["excess", "fault", "short"] {
            let polls = Arc::new(AtomicUsize::new(0));
            let dropped = Arc::new(AtomicBool::new(false));
            let owner = BodyDrop(dropped.clone());
            let observed = polls.clone();
            let body = Body::from_stream(futures::stream::poll_fn(move |_| {
                let _keep_owner = &owner;
                let poll = observed.fetch_add(1, Ordering::SeqCst);
                let item = match poll {
                    0 => Some(Ok(Bytes::from_static(b"abc"))),
                    1 if ending == "fault" => Some(Err(std::io::Error::other("body source fault"))),
                    1 if ending == "short" => None,
                    1 => Some(Ok(Bytes::from_static(b"de"))),
                    _ => panic!("must stop at the first fault or byte-bound violation"),
                };
                std::task::Poll::Ready(item)
            }));
            let response = finish_early_rejection(
                false,
                Some(4),
                body,
                (StatusCode::NOT_FOUND, "original refusal").into_response(),
            )
            .await;
            assert_eq!(polls.load(Ordering::SeqCst), 2);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
                "original refusal"
            );
        }
    }

    #[tokio::test]
    async fn rejected_body_pending_and_ready_empty_streams_have_total_deadline() {
        for always_ready in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let owner = BodyDrop(dropped.clone());
            let body = Body::from_stream(futures::stream::poll_fn(move |_| {
                let _keep_owner = &owner;
                if always_ready {
                    std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(Bytes::new())))
                } else {
                    std::task::Poll::Pending
                }
            }));
            let response = tokio::time::timeout(
                REJECTED_BODY_DRAIN_TIMEOUT + Duration::from_secs(1),
                finish_early_rejection(
                    false,
                    Some(4),
                    body,
                    forwarding_capacity_response("grid", "original refusal"),
                ),
            )
            .await
            .expect("rejected transport cleanup must terminate");
            assert!(dropped.load(Ordering::SeqCst));
            assert_capacity_response(response).await;
        }
    }

    #[tokio::test]
    async fn rejected_body_cleanup_cancellation_drops_owner_without_background_task() {
        let dropped = Arc::new(AtomicBool::new(false));
        let owner = BodyDrop(dropped.clone());
        let body = Body::from_stream(futures::stream::poll_fn(move |_| {
            let _keep_owner = &owner;
            std::task::Poll::Pending::<Option<Result<Bytes, std::io::Error>>>
        }));
        let mut cleanup = Box::pin(finish_early_rejection(
            false,
            Some(4),
            body,
            forwarding_capacity_response("grid", "original refusal"),
        ));
        assert!(futures::poll!(&mut cleanup).is_pending());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(cleanup);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unknown_length_growth_above_b_rejects_and_refunds() {
        let budget = PayloadBufferBudget::new(5);
        let cancelled = CancellationToken::new();
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]));
        let response = read_unknown_length_body("grid", body, 100, &budget, &cancelled, 1)
            .await
            .err()
            .expect("growth must reject")
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let response = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
        assert_eq!(budget.snapshot().peak_bytes, 5);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn unknown_length_growth_can_reuse_unused_sending_capacity() {
        let budget = PayloadBufferBudget::new(1024);
        let cancelled = CancellationToken::new();
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![1; 100])),
            Ok(Bytes::from(vec![2; 100])),
        ]));
        let prepared = read_unknown_length_body("grid", body, 1000, &budget, &cancelled, 1024)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.len(), 200);
        assert!(prepared.upload.bytes() > 0);
        assert!(prepared.download.bytes() > 0);
        assert!(budget.snapshot().peak_bytes <= 1024);
        assert_eq!(budget.snapshot().rejected_allocations, 0);
        drop(prepared);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn unknown_length_two_chunks_admit_when_known_length_fits_same_budget() {
        let budget = PayloadBufferBudget::new(4);
        let known_plan = SendBufferPlan::new(4, 2, 2, 2).unwrap();
        let known = reserve_send_buffers(&budget, known_plan, None)
            .await
            .expect("Content-Length: 2 fits input and both sending directions");
        assert_eq!(known.input.bytes(), 2);
        assert_eq!(known.upload.bytes(), 1);
        assert_eq!(known.download.bytes(), 1);
        drop(known);
        assert_eq!(budget.snapshot().used_bytes, 0);

        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"a")),
            Ok(Bytes::from_static(b"b")),
        ]));
        let prepared =
            read_unknown_length_body("grid", body, 2, &budget, &CancellationToken::new(), 2)
                .await
                .expect("unknown length must admit the same two bytes")
                .expect("body read was not cancelled");
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared.upload.bytes(), 1);
        assert_eq!(prepared.download.bytes(), 1);
        assert_eq!(budget.snapshot().used_bytes, 4);
        drop(prepared);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn unknown_length_source_fault_stays_body_error_and_refunds_owner() {
        let budget = PayloadBufferBudget::new(1024);
        let cancelled = CancellationToken::new();
        let body = Body::from_stream(futures::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "real source failure",
            )),
        ]));
        let response = read_unknown_length_body("grid", body, 100, &budget, &cancelled, 1)
            .await
            .err()
            .expect("source fault must reject")
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(budget.snapshot().peak_bytes, 5);
        let response = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(error["error"], "RequestBodyReadError");
        drop(response);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn unknown_length_segments_fit_input_and_sending_budget() {
        let budget = PayloadBufferBudget::new(7);
        let cancelled = CancellationToken::new();
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::new()),
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
        ]));
        let input = tokio::time::timeout(
            Duration::from_secs(1),
            read_unknown_length_body("grid", body, 5, &budget, &cancelled, 1),
        )
        .await
        .expect("segments must not wait for a second full-size allocation")
        .unwrap()
        .unwrap();
        assert_eq!(input.len(), 5);
        let segments = input.into_segments();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].as_ref(), b"abc");
        assert_eq!(segments[1].as_ref(), b"de");
        assert_eq!(budget.snapshot().peak_bytes, 7);
        assert_eq!(budget.snapshot().used_bytes, 5);
        drop(segments);
        assert_eq!(budget.snapshot().used_bytes, 0);

        let error_budget = PayloadBufferBudget::new(1024);
        let response = read_unknown_length_body(
            "grid",
            Body::from("abcdef"),
            5,
            &error_budget,
            &cancelled,
            1,
        )
        .await
        .err()
        .expect("payload limit must reject")
        .into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error_budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn unknown_length_first_segment_wait_cancels_without_dispatch() {
        let budget = PayloadBufferBudget::new(5);
        let held = budget.try_reserve(5).unwrap();
        let cancelled = CancellationToken::new();
        let mut read = Box::pin(read_unknown_length_body(
            "grid",
            Body::from("abc"),
            5,
            &budget,
            &cancelled,
            1,
        ));
        assert!(futures::poll!(&mut read).is_pending());
        assert_eq!(budget.snapshot().used_bytes, 5);
        cancelled.cancel();
        assert!(read.await.unwrap().is_none());
        assert_eq!(budget.snapshot().used_bytes, 5);
        assert_eq!(budget.snapshot().rejected_allocations, 0);
        drop(held);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn concurrent_partial_unknown_uploads_do_not_wait_on_each_others_growth() {
        let budget = PayloadBufferBudget::new(10);
        let cancelled = CancellationToken::new();
        let (resume_a, paused_a) = tokio::sync::oneshot::channel::<()>();
        let (resume_b, paused_b) = tokio::sync::oneshot::channel::<()>();
        let body_a = Body::from_stream(
            futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"abc")) })
                .chain(futures::stream::once(async move {
                    paused_a.await.unwrap();
                    Ok(Bytes::from_static(b"d"))
                })),
        );
        let body_b = Body::from_stream(
            futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"abc")) })
                .chain(futures::stream::once(async move {
                    paused_b.await.unwrap();
                    Ok(Bytes::from_static(b"d"))
                })),
        );
        let mut read_a = Box::pin(read_unknown_length_body(
            "grid", body_a, 6, &budget, &cancelled, 1,
        ));
        let mut read_b = Box::pin(read_unknown_length_body(
            "grid", body_b, 6, &budget, &cancelled, 1,
        ));
        assert!(futures::poll!(&mut read_a).is_pending());
        assert!(futures::poll!(&mut read_b).is_pending());
        assert_eq!(budget.snapshot().used_bytes, 10);

        resume_a.send(()).unwrap();
        let rejected = tokio::time::timeout(Duration::from_secs(1), &mut read_a)
            .await
            .expect("later segment must reject immediately when both partials fill B")
            .err()
            .expect("later segment must reject")
            .into_response();
        assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(budget.snapshot().used_bytes, 5);

        resume_b.send(()).unwrap();
        let accepted = read_b.await.unwrap().unwrap();
        assert_eq!(accepted.len(), 4);
        assert_eq!(budget.snapshot().used_bytes, 6);
        drop(accepted);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn known_length_releases_reservation_after_final_real_chunk_owner() {
        let budget = PayloadBufferBudget::new(5);
        let reservation = budget.reserve(5).await.unwrap();
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de")),
        ]));
        let mut stream = Box::pin(known_length_body_stream(body, 5, reservation));
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(budget.snapshot().used_bytes, 5);
        drop(first);
        assert_eq!(budget.snapshot().used_bytes, 5);
        let alias = second.slice(..1);
        drop(second);
        assert_eq!(budget.snapshot().used_bytes, 5);
        drop(alias);
        assert_eq!(budget.snapshot().used_bytes, 0);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn generated_error_bytes_refund_after_last_slice() {
        use axum::body::HttpBody;

        let budget = PayloadBufferBudget::new(1024);
        let response = budgeted_json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "InvalidContentLength"}),
            &budget,
            false,
        );
        let advertised_length = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert_eq!(budget.snapshot().used_bytes, 0);
        let mut body = response.into_body();
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap();
        let bytes = frame.into_data().unwrap();
        let length = bytes.len() as u64;
        assert_eq!(advertised_length, length);
        assert_eq!(budget.snapshot().used_bytes, length);
        let alias = bytes.slice(..1);
        drop((body, bytes));
        assert_eq!(budget.snapshot().used_bytes, length);
        drop(alias);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn generated_error_that_cannot_fit_preserves_dispatch_uncertainty() {
        let budget = PayloadBufferBudget::new(0);
        let response = resource_unavailable_response_budgeted(
            "grid",
            "upstream failed after dispatch",
            "dispatch_uncertain",
            &budget,
        );
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["code"], 702);
        assert_eq!(envelope["details"]["dispatch_phase"], "dispatch_uncertain");
        assert_eq!(budget.snapshot().used_bytes, 0);
    }
}
