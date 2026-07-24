//! Relay-aware HTTP client with route failover.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;

use super::{HttpClient, HttpClientPool, HttpError, RelayControlClient, RelayRouteInfo};
use c2_contract::ExpectedRouteContract;

/// Whether an HTTP call failure is proven to precede CRM dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpCallPhase {
    PreDispatch,
    DispatchUncertain,
}

/// An HTTP call failure paired with its authoritative dispatch phase.
#[derive(Debug)]
pub struct HttpCallError {
    phase: HttpCallPhase,
    source: HttpError,
}

impl HttpCallError {
    const fn new(phase: HttpCallPhase, source: HttpError) -> Self {
        Self { phase, source }
    }

    pub const fn phase(&self) -> HttpCallPhase {
        self.phase
    }

    pub const fn is_retry_safe(&self) -> bool {
        matches!(self.phase, HttpCallPhase::PreDispatch)
    }

    pub const fn source_error(&self) -> &HttpError {
        &self.source
    }

    pub fn into_source(self) -> HttpError {
        self.source
    }
}

impl std::fmt::Display for HttpCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "HTTP call failed during {:?}: {}",
            self.phase, self.source
        )
    }
}

impl std::error::Error for HttpCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RelayAwareClientConfig {
    pub max_attempts: usize,
    pub call_timeout_secs: f64,
    pub remote_payload_chunk_size: u64,
}

impl Default for RelayAwareClientConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            call_timeout_secs: 300.0,
            remote_payload_chunk_size: c2_config::DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLocalIpcCandidate {
    pub address: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub route_uid: String,
    pub route_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayResolvedTarget {
    Ipc { candidate: RelayLocalIpcCandidate },
    Http { relay_url: String },
}

impl RelayResolvedTarget {
    pub fn as_url(&self) -> &str {
        match self {
            Self::Ipc { candidate } => &candidate.address,
            Self::Http { relay_url } => relay_url,
        }
    }
}

pub struct RelayAwareHttpClient {
    control: Arc<RelayControlClient>,
    pool: &'static HttpClientPool,
    expected: ExpectedRouteContract,
    use_proxy: bool,
    config: RelayAwareClientConfig,
    current: Mutex<Option<String>>,
    anchor_allows_local_ipc: bool,
}

impl RelayAwareHttpClient {
    pub fn new(
        relay_url: &str,
        expected: ExpectedRouteContract,
        use_proxy: bool,
        config: RelayAwareClientConfig,
    ) -> Result<Self, HttpError> {
        let control = Arc::new(RelayControlClient::new(relay_url, use_proxy)?);
        Self::new_with_control(control, expected, use_proxy, config)
    }

    pub fn new_with_control(
        control: Arc<RelayControlClient>,
        expected: ExpectedRouteContract,
        use_proxy: bool,
        config: RelayAwareClientConfig,
    ) -> Result<Self, HttpError> {
        c2_contract::validate_expected_route_contract(&expected)
            .map_err(|err| HttpError::InvalidInput(err.to_string()))?;
        Ok(Self {
            anchor_allows_local_ipc: relay_anchor_allows_local_ipc(control.base_url()),
            control,
            pool: HttpClientPool::instance(),
            expected,
            use_proxy,
            config,
            current: Mutex::new(None),
        })
    }

    pub fn route_name(&self) -> &str {
        &self.expected.route_name
    }

    pub fn call(&self, method_name: &str, data: &[u8]) -> Result<Vec<u8>, HttpError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.call_async(method_name, data))
    }

    /// Call through the selected relay path while retaining the dispatch
    /// phase of any failure.
    pub fn call_phased(&self, method_name: &str, data: &[u8]) -> Result<Vec<u8>, HttpCallError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.call_phased_async(method_name, data))
    }

    pub async fn call_phased_async(
        &self,
        method_name: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, HttpCallError> {
        self.call_async(method_name, data)
            .await
            .map_err(|source| HttpCallError::new(http_call_error_phase(&source), source))
    }

    pub fn connect(&self) -> Result<(), HttpError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.connect_async())
    }

    pub fn resolve_target(&self) -> Result<RelayResolvedTarget, HttpError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.resolve_target_async())
    }

    pub fn resolve_http_target(&self) -> Result<RelayResolvedTarget, HttpError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.resolve_http_target_async())
    }

    pub fn resolve_target_after_local_ipc_failures(
        &self,
        failed_candidates: &[RelayLocalIpcCandidate],
    ) -> Result<RelayResolvedTarget, HttpError> {
        super::http_client::runtime()
            .handle()
            .block_on(self.resolve_target_after_local_ipc_failures_async(failed_candidates))
    }

    async fn connect_async(&self) -> Result<(), HttpError> {
        match self.select_target_async(false).await? {
            RelayResolvedTarget::Http { .. } => Ok(()),
            RelayResolvedTarget::Ipc { .. } => unreachable!("HTTP selection cannot return IPC"),
        }
    }

    pub async fn resolve_target_async(&self) -> Result<RelayResolvedTarget, HttpError> {
        self.select_target_async(true).await
    }

    pub async fn resolve_http_target_async(&self) -> Result<RelayResolvedTarget, HttpError> {
        self.select_target_async(false).await
    }

    pub async fn resolve_target_after_local_ipc_failures_async(
        &self,
        failed_candidates: &[RelayLocalIpcCandidate],
    ) -> Result<RelayResolvedTarget, HttpError> {
        self.select_target_with_local_exclusions_async(true, failed_candidates, true)
            .await
    }

    async fn select_target_async(
        &self,
        prefer_local_ipc: bool,
    ) -> Result<RelayResolvedTarget, HttpError> {
        self.select_target_with_local_exclusions_async(prefer_local_ipc, &[], false)
            .await
    }

    async fn select_target_with_local_exclusions_async(
        &self,
        prefer_local_ipc: bool,
        excluded_local_ipc_candidates: &[RelayLocalIpcCandidate],
        fallback_denied_when_only_excluded: bool,
    ) -> Result<RelayResolvedTarget, HttpError> {
        let attempts = self.config.max_attempts.max(1);
        let mut last_error = None;
        let mut excluded_routes = HashSet::new();

        for attempt in 0..attempts {
            let routes = match self
                .resolve_routes_async(attempt > 0 || fallback_denied_when_only_excluded)
                .await
            {
                Ok(routes) if !routes.is_empty() => routes,
                Ok(_) => {
                    return Err(HttpError::ServerError(
                        404,
                        resource_not_found_body(self.route_name()),
                    ));
                }
                Err(err) => {
                    if resolve_retryable(&err) && attempt + 1 < attempts {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };
            let had_routes_before_local_exclusion = !routes.is_empty();
            let routes = filter_failed_local_ipc_candidates(routes, excluded_local_ipc_candidates);
            if routes.is_empty()
                && fallback_denied_when_only_excluded
                && had_routes_before_local_exclusion
            {
                return Err(HttpError::ServerError(
                    409,
                    fallback_denied_body(self.route_name(), excluded_local_ipc_candidates),
                ));
            }

            if let Some(candidate) = select_local_ipc_candidate(
                prefer_local_ipc,
                self.anchor_allows_local_ipc,
                &routes,
                &self.expected,
            ) {
                return Ok(RelayResolvedTarget::Ipc { candidate });
            }

            let ordered = self.order_routes(routes, &excluded_routes);
            if ordered.is_empty() {
                return Err(last_error.unwrap_or_else(|| {
                    HttpError::ServerError(404, resource_not_found_body(self.route_name()))
                }));
            }

            for route in ordered {
                let relay_url = route.relay_url.trim_end_matches('/').to_string();
                let client = match self.pool.acquire_with_options(
                    &relay_url,
                    self.use_proxy,
                    self.config.call_timeout_secs,
                    self.config.remote_payload_chunk_size,
                ) {
                    Ok(client) => RelayPoolGuard {
                        pool: self.pool,
                        relay_url: relay_url.clone(),
                        client,
                    },
                    Err(err) => {
                        last_error = Some(err);
                        continue;
                    }
                };

                match client
                    .client
                    .probe_route_with_token_async(&self.expected, &route.route_token())
                    .await
                {
                    Ok(()) => {
                        *self.current.lock() = Some(relay_url.clone());
                        return Ok(RelayResolvedTarget::Http { relay_url });
                    }
                    Err(err) if route_is_stale(&err) => {
                        self.control.invalidate(self.route_name());
                        *self.current.lock() = None;
                        excluded_routes.insert(relay_url);
                        last_error = Some(err);
                    }
                    Err(err) => {
                        last_error = Some(err);
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Err(last_error.unwrap_or_else(|| {
            HttpError::ServerError(404, resource_not_found_body(self.route_name()))
        }))
    }

    pub(crate) async fn call_async(
        &self,
        method_name: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, HttpError> {
        let attempts = self.config.max_attempts.max(1);
        let mut last_error = None;
        let mut excluded_routes = HashSet::new();

        for attempt in 0..attempts {
            let routes = match self.resolve_routes_async(attempt > 0).await {
                Ok(routes) if !routes.is_empty() => routes,
                Ok(_) => {
                    return Err(HttpError::ServerError(
                        404,
                        resource_not_found_body(self.route_name()),
                    ));
                }
                Err(err) => {
                    if resolve_retryable(&err) && attempt + 1 < attempts {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };

            let ordered = self.order_routes(routes, &excluded_routes);
            if ordered.is_empty() {
                return Err(last_error.unwrap_or_else(|| {
                    HttpError::ServerError(404, resource_not_found_body(self.route_name()))
                }));
            }
            for route in ordered {
                let relay_url = route.relay_url.trim_end_matches('/').to_string();
                let client = match self.pool.acquire_with_options(
                    &relay_url,
                    self.use_proxy,
                    self.config.call_timeout_secs,
                    self.config.remote_payload_chunk_size,
                ) {
                    Ok(client) => RelayPoolGuard {
                        pool: self.pool,
                        relay_url: relay_url.clone(),
                        client,
                    },
                    Err(err) => {
                        last_error = Some(err);
                        continue;
                    }
                };

                match client
                    .client
                    .call_with_route_token_async(
                        &self.expected,
                        &route.route_token(),
                        method_name,
                        data,
                    )
                    .await
                {
                    Ok(bytes) => {
                        *self.current.lock() = Some(relay_url);
                        return Ok(bytes);
                    }
                    Err(HttpError::CrmError(err)) => return Err(HttpError::CrmError(err)),
                    Err(err) if route_is_stale(&err) => {
                        self.control.invalidate(self.route_name());
                        *self.current.lock() = None;
                        excluded_routes.insert(relay_url);
                        last_error = Some(err);
                        break;
                    }
                    Err(err) => return Err(err),
                }
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Err(last_error.unwrap_or_else(|| {
            HttpError::ServerError(404, resource_not_found_body(self.route_name()))
        }))
    }

    async fn resolve_routes_async(
        &self,
        force_refresh: bool,
    ) -> Result<Vec<RelayRouteInfo>, HttpError> {
        if force_refresh {
            self.control.invalidate(self.route_name());
        }
        let routes = self.control.resolve_matching_async(&self.expected).await?;
        let raw_routes_non_empty = !routes.is_empty();
        let filtered = filter_routes_by_expected_contract(routes, &self.expected);
        if !force_refresh && raw_routes_non_empty && filtered.is_empty() {
            self.control.invalidate(self.route_name());
            let refreshed = self.control.resolve_matching_async(&self.expected).await?;
            let refreshed_routes_non_empty = !refreshed.is_empty();
            let refreshed_filtered = filter_routes_by_expected_contract(refreshed, &self.expected);
            if refreshed_routes_non_empty && refreshed_filtered.is_empty() {
                return Err(HttpError::ServerError(
                    409,
                    crm_contract_mismatch_body(self.route_name()),
                ));
            }
            return Ok(refreshed_filtered);
        }
        if force_refresh && raw_routes_non_empty && filtered.is_empty() {
            return Err(HttpError::ServerError(
                409,
                crm_contract_mismatch_body(self.route_name()),
            ));
        }
        Ok(filtered)
    }

    fn order_routes(
        &self,
        routes: Vec<RelayRouteInfo>,
        excluded_routes: &HashSet<String>,
    ) -> Vec<RelayRouteInfo> {
        let routes = routes
            .into_iter()
            .filter(|route| !excluded_routes.contains(route.relay_url.trim_end_matches('/')))
            .collect::<Vec<_>>();
        let current = self.current.lock().clone();
        let Some(current) = current else {
            return routes;
        };
        let mut preferred = Vec::new();
        let mut rest = Vec::new();
        for route in routes {
            if route.relay_url.trim_end_matches('/') == current {
                preferred.push(route);
            } else {
                rest.push(route);
            }
        }
        preferred.extend(rest);
        preferred
    }
}

struct RelayPoolGuard {
    pool: &'static HttpClientPool,
    relay_url: String,
    client: Arc<HttpClient>,
}

impl Drop for RelayPoolGuard {
    fn drop(&mut self) {
        self.pool.release(&self.relay_url);
    }
}

fn route_is_stale(err: &HttpError) -> bool {
    match err {
        HttpError::ServerError(404, body) => {
            relay_error_code_is(body, c2_error::ErrorCode::ResourceNotFound)
        }
        HttpError::ServerError(409, body) => {
            relay_error_code_is(body, c2_error::ErrorCode::RouteStale)
        }
        HttpError::ServerError(502, body) => canonical_semantic_error(body).is_some_and(|error| {
            error.code == c2_error::ErrorCode::ResourceUnavailable
                && error.details.get("dispatch_phase").map(String::as_str) == Some("pre_dispatch")
        }),
        _ => false,
    }
}

fn http_call_error_phase(error: &HttpError) -> HttpCallPhase {
    match error {
        HttpError::InvalidInput(_) => HttpCallPhase::PreDispatch,
        HttpError::ServerError(_, body)
            if canonical_semantic_error(body).is_some_and(|error| {
                error.code != c2_error::ErrorCode::ResourceUnavailable
                    || error.details.get("dispatch_phase").map(String::as_str)
                        == Some("pre_dispatch")
            }) =>
        {
            HttpCallPhase::PreDispatch
        }
        HttpError::CrmError(_) | HttpError::Transport(_) | HttpError::ServerError(_, _) => {
            HttpCallPhase::DispatchUncertain
        }
    }
}

fn canonical_semantic_error(body: &str) -> Option<c2_error::C2Error> {
    let envelope = serde_json::from_str::<c2_error::C2ErrorEnvelope>(body).ok()?;
    c2_error::C2Error::from_envelope(envelope).ok()
}

fn relay_error_code_is(body: &str, expected: c2_error::ErrorCode) -> bool {
    let Ok(envelope) = serde_json::from_str::<c2_error::C2ErrorEnvelope>(body) else {
        return false;
    };
    c2_error::C2Error::from_envelope(envelope).is_ok_and(|err| err.code == expected)
}

fn resource_not_found_body(route_name: &str) -> String {
    canonical_relay_error_body(
        c2_error::ErrorCode::ResourceNotFound,
        &format!("route not found: {route_name}"),
        route_name,
    )
}

fn crm_contract_mismatch_body(route_name: &str) -> String {
    canonical_relay_error_body(
        c2_error::ErrorCode::ContractMismatch,
        &format!("CRM contract mismatch for route {route_name}"),
        route_name,
    )
}

fn fallback_denied_body(route_name: &str, failed_candidates: &[RelayLocalIpcCandidate]) -> String {
    let last_failed = failed_candidates.last();
    let mut details = serde_json::Map::new();
    details.insert("route".to_string(), json!(route_name));
    details.insert(
        "reason".to_string(),
        json!("only_failed_local_ipc_candidates"),
    );
    details.insert(
        "failed_local_ipc_candidates".to_string(),
        json!(failed_candidates.len().to_string()),
    );
    if let Some(candidate) = last_failed {
        details.insert("ipc_address".to_string(), json!(candidate.address));
        details.insert("server_id".to_string(), json!(candidate.server_id));
        details.insert(
            "server_instance_id".to_string(),
            json!(candidate.server_instance_id),
        );
        details.insert("route_uid".to_string(), json!(candidate.route_uid));
        details.insert(
            "route_revision".to_string(),
            json!(candidate.route_revision.to_string()),
        );
    }
    json!({
        "version": c2_error::ERROR_WIRE_VERSION,
        "code": u16::from(c2_error::ErrorCode::FallbackDenied),
        "name": c2_error::ErrorCode::FallbackDenied.name(),
        "message": format!("same local relay HTTP fallback denied for route {route_name}"),
        "details": details,
    })
    .to_string()
}

fn canonical_relay_error_body(
    code: c2_error::ErrorCode,
    message: &str,
    route_name: &str,
) -> String {
    json!({
        "version": c2_error::ERROR_WIRE_VERSION,
        "code": u16::from(code),
        "name": code.name(),
        "message": message,
        "details": {
            "route": route_name,
        },
    })
    .to_string()
}

fn resolve_retryable(err: &HttpError) -> bool {
    matches!(
        err,
        HttpError::Transport(_) | HttpError::ServerError(500..=599, _)
    )
}

fn select_local_ipc_candidate(
    prefer_local_ipc: bool,
    anchor_allows_local_ipc: bool,
    routes: &[RelayRouteInfo],
    expected: &ExpectedRouteContract,
) -> Option<RelayLocalIpcCandidate> {
    if !prefer_local_ipc || !anchor_allows_local_ipc {
        return None;
    }
    routes.iter().find_map(|route| {
        if !route_matches_expected_contract(route, expected) {
            return None;
        }
        Some(RelayLocalIpcCandidate {
            address: route.ipc_address.clone()?,
            server_id: route.server_id.clone()?,
            server_instance_id: route.server_instance_id.clone()?,
            route_uid: route.route_uid.clone(),
            route_revision: route.route_revision,
        })
    })
}

fn filter_failed_local_ipc_candidates(
    routes: Vec<RelayRouteInfo>,
    failed_candidates: &[RelayLocalIpcCandidate],
) -> Vec<RelayRouteInfo> {
    if failed_candidates.is_empty() {
        return routes;
    }
    routes
        .into_iter()
        .filter(|route| {
            !failed_candidates
                .iter()
                .any(|failed| route_matches_local_ipc_candidate(route, failed))
        })
        .collect()
}

fn route_matches_local_ipc_candidate(
    route: &RelayRouteInfo,
    failed: &RelayLocalIpcCandidate,
) -> bool {
    // A same-endpoint local IPC retry with a newer token would silently bind a
    // replacement route. Exclude by endpoint identity, not by route token.
    route.ipc_address.as_deref() == Some(failed.address.as_str())
        && route.server_id.as_deref() == Some(failed.server_id.as_str())
        && route.server_instance_id.as_deref() == Some(failed.server_instance_id.as_str())
}

fn filter_routes_by_expected_contract(
    routes: Vec<RelayRouteInfo>,
    expected: &ExpectedRouteContract,
) -> Vec<RelayRouteInfo> {
    routes
        .into_iter()
        .filter(|route| route_matches_expected_contract(route, expected))
        .collect()
}

fn route_matches_expected_contract(
    route: &RelayRouteInfo,
    expected: &ExpectedRouteContract,
) -> bool {
    route.name == expected.route_name
        && route.crm_ns == expected.crm_ns
        && route.crm_name == expected.crm_name
        && route.crm_ver == expected.crm_ver
        && route.abi_hash == expected.abi_hash
        && route.signature_hash == expected.signature_hash
}

fn relay_anchor_allows_local_ipc(anchor_url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(anchor_url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || ip_host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(all(test, feature = "relay"))]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct RegistryState {
        stale_url: String,
        live_url: String,
        resolve_count: Arc<AtomicUsize>,
    }

    const TEST_ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_SIGNATURE_HASH: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn expected_contract() -> ExpectedRouteContract {
        ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: TEST_ABI_HASH.to_string(),
            signature_hash: TEST_SIGNATURE_HASH.to_string(),
        }
    }

    fn route_info(name: String, relay_url: String) -> RelayRouteInfo {
        RelayRouteInfo {
            name,
            relay_url,
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
            ipc_address: None,
            server_id: None,
            server_instance_id: None,
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: TEST_ABI_HASH.to_string(),
            signature_hash: TEST_SIGNATURE_HASH.to_string(),
            max_payload_size: 1024,
        }
    }

    async fn registry_resolve(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        state.resolve_count.fetch_add(1, Ordering::SeqCst);
        Json(vec![
            route_info(name.clone(), state.stale_url.clone()),
            route_info(name, state.live_url.clone()),
        ])
        .into_response()
    }

    fn canonical_error_json(
        code: u16,
        name: &str,
        message: &str,
        route: &str,
    ) -> serde_json::Value {
        json!({
            "version": 1,
            "code": code,
            "name": name,
            "message": message,
            "details": {
                "route": route,
            },
        })
    }

    async fn stale_call(Path((route, _method)): Path<(String, String)>) -> Response {
        let mut error = canonical_error_json(
            702,
            "ResourceUnavailable",
            "relay upstream unavailable",
            &route,
        );
        error["details"]["dispatch_phase"] = json!("pre_dispatch");
        (StatusCode::BAD_GATEWAY, Json(error)).into_response()
    }

    async fn generic_bad_gateway() -> Response {
        (StatusCode::BAD_GATEWAY, "proxy exploded").into_response()
    }

    async fn transient_unavailable() -> Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "relay temporarily unavailable",
        )
            .into_response()
    }

    async fn dispatch_uncertain_unavailable(
        Path((route, _method)): Path<(String, String)>,
    ) -> Response {
        let mut error = canonical_error_json(
            702,
            "ResourceUnavailable",
            "upstream connection lost after possible dispatch",
            &route,
        );
        error["details"]["dispatch_phase"] = json!("dispatch_uncertain");
        (StatusCode::BAD_GATEWAY, Json(error)).into_response()
    }

    async fn transient_resolve_error(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        let count = state.resolve_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "temporary registry error",
            )
                .into_response();
        }
        Json(vec![route_info(name, state.live_url.clone())]).into_response()
    }

    async fn misleading_bad_gateway() -> Response {
        (
            StatusCode::BAD_GATEWAY,
            "proxy exploded while mentioning UpstreamUnavailable",
        )
            .into_response()
    }

    async fn stale_not_found(Path((route, _method)): Path<(String, String)>) -> Response {
        (
            StatusCode::NOT_FOUND,
            Json(canonical_error_json(
                701,
                "ResourceNotFound",
                "relay route not found",
                &route,
            )),
        )
            .into_response()
    }

    async fn generic_not_found() -> Response {
        (StatusCode::NOT_FOUND, "missing ResourceNotFound marker").into_response()
    }

    async fn live_call(Path((_route, _method)): Path<(String, String)>, body: Bytes) -> Response {
        let mut out = b"ok:".to_vec();
        out.extend_from_slice(&body);
        (StatusCode::OK, out).into_response()
    }

    async fn registry_resolve_with_local_ipc(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        state.resolve_count.fetch_add(1, Ordering::SeqCst);
        Json(vec![
            route_info(name.clone(), state.live_url.clone()),
            RelayRouteInfo {
                relay_url: state.stale_url.clone(),
                ipc_address: Some("ipc://local-grid".to_string()),
                server_id: Some("local-grid".to_string()),
                server_instance_id: Some("inst-local-grid".to_string()),
                ..route_info(name, state.stale_url.clone())
            },
        ])
        .into_response()
    }

    async fn registry_resolve_only_failed_local_ipc(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        state.resolve_count.fetch_add(1, Ordering::SeqCst);
        Json(vec![RelayRouteInfo {
            relay_url: state.stale_url.clone(),
            ipc_address: Some("ipc://local-grid".to_string()),
            server_id: Some("local-grid".to_string()),
            server_instance_id: Some("inst-local-grid".to_string()),
            ..route_info(name, state.stale_url.clone())
        }])
        .into_response()
    }

    async fn registry_resolve_contract_changes(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        let count = state.resolve_count.fetch_add(1, Ordering::SeqCst);
        let (crm_ns, crm_ver) = if count == 0 {
            ("other.grid", "0.1.0")
        } else {
            ("test.grid", "0.1.0")
        };
        let mut route = route_info(name, state.live_url.clone());
        route.crm_ns = crm_ns.to_string();
        route.crm_ver = crm_ver.to_string();
        Json(vec![route]).into_response()
    }

    async fn registry_resolve_expected_contract(
        State(state): State<RegistryState>,
        Path(name): Path<String>,
    ) -> Response {
        state.resolve_count.fetch_add(1, Ordering::SeqCst);
        Json(vec![route_info(name, state.live_url.clone())]).into_response()
    }

    fn expected_crm_headers_match(headers: &HeaderMap) -> bool {
        headers
            .get("x-c2-expected-crm-ns")
            .is_some_and(|v| v == "test.grid")
            && headers
                .get("x-c2-expected-crm-name")
                .is_some_and(|v| v == "Grid")
            && headers
                .get("x-c2-expected-crm-ver")
                .is_some_and(|v| v == "0.1.0")
            && headers
                .get("x-c2-expected-abi-hash")
                .is_some_and(|v| v == TEST_ABI_HASH)
            && headers
                .get("x-c2-expected-signature-hash")
                .is_some_and(|v| v == TEST_SIGNATURE_HASH)
    }

    fn route_token_headers_match(headers: &HeaderMap) -> bool {
        headers
            .get("x-c2-route-uid")
            .is_some_and(|v| v == "grid-route-uid-0001")
            && headers.get("x-c2-route-revision").is_some_and(|v| v == "1")
    }

    async fn probe_requires_expected_crm_headers(headers: HeaderMap) -> Response {
        if expected_crm_headers_match(&headers) && route_token_headers_match(&headers) {
            StatusCode::OK.into_response()
        } else {
            (
                StatusCode::CONFLICT,
                Json(canonical_error_json(
                    709,
                    "ContractMismatch",
                    "CRM contract mismatch for route grid",
                    "grid",
                )),
            )
                .into_response()
        }
    }

    async fn call_requires_expected_crm_headers(
        headers: HeaderMap,
        Path((route, _method)): Path<(String, String)>,
        body: Bytes,
    ) -> Response {
        if expected_crm_headers_match(&headers) && route_token_headers_match(&headers) {
            let mut out = b"ok:".to_vec();
            out.extend_from_slice(&body);
            (StatusCode::OK, out).into_response()
        } else {
            (
                StatusCode::CONFLICT,
                Json(canonical_error_json(
                    709,
                    "ContractMismatch",
                    "CRM contract mismatch for route grid",
                    &route,
                )),
            )
                .into_response()
        }
    }

    async fn spawn_app(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, handle)
    }

    #[tokio::test]
    async fn call_re_resolves_and_tries_next_route_after_stale_route() {
        let (stale_url, stale_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(stale_call))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        let bytes = client.call_async("step", b"payload").await.unwrap();
        assert_eq!(bytes, b"ok:payload");
        assert!(
            resolve_count.load(Ordering::SeqCst) >= 2,
            "stale route should invalidate cache and re-resolve"
        );

        registry_handle.abort();
        stale_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn connect_selects_reachable_route_before_first_call() {
        let stale_url = "http://127.0.0.1:9".to_string();
        let (live_url, live_handle) = spawn_app(
            Router::new()
                .route("/_probe/{route}", get(|| async { StatusCode::OK }))
                .route("/{route}/{method}", post(live_call)),
        )
        .await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url,
            live_url: live_url.clone(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        client.connect_async().await.unwrap();
        assert_eq!(client.current.lock().as_deref(), Some(live_url.as_str()));
        let bytes = client.call_async("step", b"payload").await.unwrap();
        assert_eq!(bytes, b"ok:payload");

        registry_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn resolve_target_prefers_local_ipc_route_without_http_probe() {
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: "http://127.0.0.1:9".to_string(),
            live_url: "http://127.0.0.1:10".to_string(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve_with_local_ipc))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        let target = client.resolve_target_async().await.unwrap();
        assert_eq!(target.as_url(), "ipc://local-grid");
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            1,
            "local IPC target selection should not probe or iterate HTTP routes"
        );

        registry_handle.abort();
    }

    #[tokio::test]
    async fn local_ipc_failure_selects_distinct_http_candidate_only() {
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/_probe/{route}", get(|| async { StatusCode::OK })))
                .await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: "http://127.0.0.1:9".to_string(),
            live_url: live_url.clone(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve_with_local_ipc))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let failed = RelayLocalIpcCandidate {
            address: "ipc://local-grid".to_string(),
            server_id: "local-grid".to_string(),
            server_instance_id: "inst-local-grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
        };

        let target = client
            .resolve_target_after_local_ipc_failures_async(std::slice::from_ref(&failed))
            .await
            .unwrap();

        assert_eq!(
            target,
            RelayResolvedTarget::Http {
                relay_url: live_url
            }
        );
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);
        registry_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn local_ipc_failure_without_distinct_http_candidate_is_fallback_denied() {
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: "http://127.0.0.1:9".to_string(),
            live_url: String::new(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route(
                    "/_resolve/{name}",
                    get(registry_resolve_only_failed_local_ipc),
                )
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let failed = RelayLocalIpcCandidate {
            address: "ipc://local-grid".to_string(),
            server_id: "local-grid".to_string(),
            server_instance_id: "inst-local-grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
        };

        let err = client
            .resolve_target_after_local_ipc_failures_async(std::slice::from_ref(&failed))
            .await
            .unwrap_err();

        assert!(
            matches!(&err, HttpError::ServerError(409, body) if relay_error_code_is(body.as_str(), c2_error::ErrorCode::FallbackDenied))
        );
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);
        registry_handle.abort();
    }

    #[test]
    fn failed_local_ipc_exclusion_ignores_replacement_token_on_same_endpoint() {
        let failed = RelayLocalIpcCandidate {
            address: "ipc://local-grid".to_string(),
            server_id: "local-grid".to_string(),
            server_instance_id: "inst-local-grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
        };
        let refreshed_same_endpoint = RelayRouteInfo {
            ipc_address: Some("ipc://local-grid".to_string()),
            server_id: Some("local-grid".to_string()),
            server_instance_id: Some("inst-local-grid".to_string()),
            route_uid: "grid-route-uid-0002".to_string(),
            route_revision: 2,
            ..route_info("grid".to_string(), "http://127.0.0.1:8080".to_string())
        };

        assert!(
            filter_failed_local_ipc_candidates(vec![refreshed_same_endpoint], &[failed]).is_empty(),
            "same endpoint with a replacement token must stay excluded"
        );
    }

    #[tokio::test]
    async fn cached_wrong_contract_routes_force_fresh_resolve_before_404() {
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/_probe/{route}", get(|| async { StatusCode::OK })))
                .await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: String::new(),
            live_url: live_url.clone(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve_contract_changes))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();

        client.connect_async().await.unwrap();
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            2,
            "CRM-filtered cached routes should force one fresh resolve before reporting no compatible route"
        );

        registry_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn expected_crm_headers_are_sent_on_probe_and_call() {
        let (live_url, live_handle) = spawn_app(
            Router::new()
                .route("/_probe/{route}", get(probe_requires_expected_crm_headers))
                .route(
                    "/{route}/{method}",
                    post(call_requires_expected_crm_headers),
                ),
        )
        .await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: String::new(),
            live_url,
            resolve_count,
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve_expected_contract))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();

        client.connect_async().await.unwrap();
        let bytes = client.call_async("step", b"payload").await.unwrap();
        assert_eq!(bytes, b"ok:payload");

        registry_handle.abort();
        live_handle.abort();
    }

    #[test]
    fn construction_rejects_malformed_expected_contract_without_panic() {
        let mut malformed = expected_contract();
        malformed.abi_hash = "not-a-sha256".to_string();

        let result = RelayAwareHttpClient::new(
            "http://relay.example",
            malformed,
            false,
            RelayAwareClientConfig::default(),
        );
        let err = match result {
            Ok(_) => panic!("malformed expected contract must return an error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("abi_hash"), "{err}");
    }

    #[test]
    fn new_with_control_rejects_malformed_expected_contract_without_panic() {
        let mut malformed = expected_contract();
        malformed.signature_hash = String::new();
        let control = Arc::new(RelayControlClient::new("http://relay.example", false).unwrap());

        let result = RelayAwareHttpClient::new_with_control(
            control,
            malformed,
            false,
            RelayAwareClientConfig::default(),
        );
        let err = match result {
            Ok(_) => panic!("malformed expected contract must return an error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("signature_hash"), "{err}");
    }

    #[test]
    fn local_ipc_selection_requires_loopback_anchor() {
        let routes = vec![RelayRouteInfo {
            relay_url: "http://relay.example".to_string(),
            ipc_address: Some("ipc://local-grid".to_string()),
            server_id: Some("local-grid".to_string()),
            server_instance_id: Some("inst-local-grid".to_string()),
            ..route_info("grid".to_string(), "http://relay.example".to_string())
        }];

        assert_eq!(
            select_local_ipc_candidate(true, true, &routes, &expected_contract())
                .map(|candidate| candidate.address),
            Some("ipc://local-grid".to_string())
        );
        assert_eq!(
            select_local_ipc_candidate(true, false, &routes, &expected_contract()),
            None
        );
        assert_eq!(
            select_local_ipc_candidate(false, true, &routes, &expected_contract()),
            None
        );

        assert!(relay_anchor_allows_local_ipc("http://127.0.0.1:8080"));
        assert!(relay_anchor_allows_local_ipc("http://[::1]:8080"));
        assert!(relay_anchor_allows_local_ipc("http://localhost:8080"));
        assert!(!relay_anchor_allows_local_ipc("http://192.0.2.10:8080"));
        assert!(!relay_anchor_allows_local_ipc("http://relay.example:8080"));
    }

    #[test]
    fn local_ipc_selection_requires_identity_complete_route() {
        let complete = RelayRouteInfo {
            ipc_address: Some("ipc://grid-server".to_string()),
            server_id: Some("grid-server".to_string()),
            server_instance_id: Some("inst-a".to_string()),
            ..route_info("grid".to_string(), "http://127.0.0.1:8080".to_string())
        };
        let missing_instance = RelayRouteInfo {
            server_instance_id: None,
            ..complete.clone()
        };
        let missing_server_id = RelayRouteInfo {
            server_id: None,
            ..complete.clone()
        };

        assert_eq!(
            select_local_ipc_candidate(true, true, &[missing_instance], &expected_contract()),
            None,
        );
        assert_eq!(
            select_local_ipc_candidate(true, true, &[missing_server_id], &expected_contract()),
            None,
        );
        assert_eq!(
            select_local_ipc_candidate(
                true,
                true,
                std::slice::from_ref(&complete),
                &expected_contract(),
            ),
            Some(RelayLocalIpcCandidate {
                address: "ipc://grid-server".to_string(),
                server_id: "grid-server".to_string(),
                server_instance_id: "inst-a".to_string(),
                route_uid: "grid-route-uid-0001".to_string(),
                route_revision: 1,
            }),
        );
    }

    #[test]
    fn relay_resolved_ipc_target_carries_expected_identity() {
        let route = RelayRouteInfo {
            ipc_address: Some("ipc://grid-server".to_string()),
            server_id: Some("grid-server".to_string()),
            server_instance_id: Some("inst-a".to_string()),
            ..route_info("grid".to_string(), "http://127.0.0.1:8080".to_string())
        };

        let candidate = select_local_ipc_candidate(true, true, &[route], &expected_contract())
            .expect("candidate");
        assert_eq!(candidate.address, "ipc://grid-server");
        assert_eq!(candidate.server_id, "grid-server");
        assert_eq!(candidate.server_instance_id, "inst-a");
        assert_eq!(candidate.route_uid, "grid-route-uid-0001");
        assert_eq!(candidate.route_revision, 1);
    }

    #[test]
    fn crm_contract_filter_runs_before_local_ipc_selection() {
        let mismatched_local = RelayRouteInfo {
            ipc_address: Some("ipc://wrong-grid".to_string()),
            server_id: Some("wrong-grid".to_string()),
            server_instance_id: Some("inst-wrong".to_string()),
            crm_ns: "other.grid".to_string(),
            ..route_info("grid".to_string(), "http://127.0.0.1:8080".to_string())
        };
        let mismatched_crm_name = RelayRouteInfo {
            ipc_address: Some("ipc://wrong-model".to_string()),
            server_id: Some("wrong-model".to_string()),
            server_instance_id: Some("inst-wrong-model".to_string()),
            crm_name: "OtherGrid".to_string(),
            ..route_info("grid".to_string(), "http://127.0.0.1:8082".to_string())
        };
        let matched_http = route_info("grid".to_string(), "http://127.0.0.1:8081".to_string());

        let filtered = filter_routes_by_expected_contract(
            vec![mismatched_local, mismatched_crm_name, matched_http.clone()],
            &expected_contract(),
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].relay_url, matched_http.relay_url);
        assert_eq!(
            select_local_ipc_candidate(true, true, &filtered, &expected_contract()),
            None
        );
    }

    #[tokio::test]
    async fn generic_502_is_not_treated_as_stale_route() {
        let (bad_url, bad_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(generic_bad_gateway))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: bad_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let err = client.call_async("step", b"payload").await.unwrap_err();
        assert!(matches!(err, HttpError::ServerError(502, _)));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

        registry_handle.abort();
        bad_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn misleading_502_text_is_not_treated_as_stale_route() {
        let (bad_url, bad_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(misleading_bad_gateway))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: bad_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let err = client.call_async("step", b"payload").await.unwrap_err();
        assert!(matches!(err, HttpError::ServerError(502, _)));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

        registry_handle.abort();
        bad_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn structured_404_is_treated_as_stale_route() {
        let (stale_url, stale_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(stale_not_found))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        let bytes = client.call_async("step", b"payload").await.unwrap();
        assert_eq!(bytes, b"ok:payload");
        assert!(resolve_count.load(Ordering::SeqCst) >= 2);

        registry_handle.abort();
        stale_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn generic_404_is_not_treated_as_stale_route() {
        let (bad_url, bad_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(generic_not_found))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: bad_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let err = client.call_async("step", b"payload").await.unwrap_err();
        assert!(matches!(err, HttpError::ServerError(404, _)));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

        registry_handle.abort();
        bad_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn ambiguous_call_failure_is_not_replayed() {
        let (bad_url, bad_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(transient_unavailable))).await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: bad_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        let err = client.call_async("step", b"payload").await.unwrap_err();
        assert!(matches!(err, HttpError::ServerError(503, _)));
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            1,
            "ambiguous data-plane failures must not replay CRM calls"
        );

        registry_handle.abort();
        bad_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn canonical_dispatch_uncertain_failure_is_not_replayed() {
        let (bad_url, bad_handle) = spawn_app(
            Router::new().route("/{route}/{method}", post(dispatch_uncertain_unavailable)),
        )
        .await;
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/{route}/{method}", post(live_call))).await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: bad_url,
            live_url,
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(registry_resolve))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        let error = client
            .call_phased_async("step", b"payload")
            .await
            .expect_err("dispatch-uncertain relay failure");
        assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            1,
            "explicit dispatch uncertainty must never trigger a second data-plane attempt"
        );

        registry_handle.abort();
        bad_handle.abort();
        live_handle.abort();
    }

    #[tokio::test]
    async fn connect_retries_transient_resolve_5xx() {
        let (live_url, live_handle) =
            spawn_app(Router::new().route("/_probe/{route}", get(|| async { StatusCode::OK })))
                .await;
        let resolve_count = Arc::new(AtomicUsize::new(0));
        let registry_state = RegistryState {
            stale_url: String::new(),
            live_url: live_url.clone(),
            resolve_count: resolve_count.clone(),
        };
        let (registry_url, registry_handle) = spawn_app(
            Router::new()
                .route("/_resolve/{name}", get(transient_resolve_error))
                .with_state(registry_state),
        )
        .await;

        let client = RelayAwareHttpClient::new(
            &registry_url,
            expected_contract(),
            false,
            RelayAwareClientConfig {
                max_attempts: 3,
                ..Default::default()
            },
        )
        .unwrap();

        client.connect_async().await.unwrap();
        assert_eq!(client.current.lock().as_deref(), Some(live_url.as_str()));
        assert_eq!(resolve_count.load(Ordering::SeqCst), 2);

        registry_handle.abort();
        live_handle.abort();
    }
}
