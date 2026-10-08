use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use super::http_client::{HttpError, HttpRouteToken, runtime};
use c2_config::LocalEndpointContext;
use c2_contract::ExpectedRouteContract;

/// Optional discovery hint; it grants no permission to open an endpoint.
pub const LOCAL_ENDPOINT_NAMESPACE_HEADER: &str = "x-c2-local-namespace";

pub(crate) fn validate_local_endpoint_namespace(value: &str) -> Result<(), HttpError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(HttpError::InvalidInput(
            "invalid local endpoint namespace (expected v1 lowercase SHA-256 identity)".into(),
        ));
    }
    Ok(())
}

const CONTROL_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RETRY_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30);

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, CONTROL_SEGMENT).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolveCacheKey {
    expected: ExpectedRouteContract,
    local_endpoint_namespace: Option<String>,
}

impl ResolveCacheKey {
    fn route_name(&self) -> &str {
        &self.expected.route_name
    }
}

fn resolve_cache_key(expected: &ExpectedRouteContract) -> ResolveCacheKey {
    ResolveCacheKey {
        expected: expected.clone(),
        local_endpoint_namespace: None,
    }
}

fn canonical_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_owned()
}

#[derive(Debug, Clone, Serialize)]
struct RegisterRequest<'a> {
    name: &'a str,
    server_id: &'a str,
    server_instance_id: &'a str,
    address: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    registration_token: Option<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    prepare_only: bool,
    crm_ns: &'a str,
    crm_name: &'a str,
    crm_ver: &'a str,
    abi_hash: &'a str,
    signature_hash: &'a str,
    max_payload_size: u64,
}

/// One contract-scoped route registration projected to a relay.
#[derive(Debug, Clone, Copy)]
pub struct RelayRegistration<'a> {
    pub expected: &'a ExpectedRouteContract,
    pub server_id: &'a str,
    pub server_instance_id: &'a str,
    pub address: &'a str,
    pub max_payload_size: u64,
}

impl<'a> RelayRegistration<'a> {
    fn request<'b>(
        &'b self,
        registration_token: Option<&'b str>,
        prepare_only: bool,
    ) -> RegisterRequest<'b> {
        RegisterRequest {
            name: &self.expected.route_name,
            server_id: self.server_id,
            server_instance_id: self.server_instance_id,
            address: self.address,
            registration_token,
            prepare_only,
            crm_ns: &self.expected.crm_ns,
            crm_name: &self.expected.crm_name,
            crm_ver: &self.expected.crm_ver,
            abi_hash: &self.expected.abi_hash,
            signature_hash: &self.expected.signature_hash,
            max_payload_size: self.max_payload_size,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Identity of one concrete registration, captured before beginning retirement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelayRegistrationScope {
    pub name: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub route_uid: String,
    pub route_revision: u64,
}

#[derive(Debug, Clone, Serialize)]
struct AdminUnregisterRequest<'a> {
    name: &'a str,
    server_id: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRouteInfo {
    pub name: String,
    pub relay_url: String,
    pub route_uid: String,
    pub route_revision: u64,
    pub ipc_address: Option<String>,
    pub server_id: Option<String>,
    pub server_instance_id: Option<String>,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
}

impl RelayRouteInfo {
    pub(crate) fn route_token(&self) -> HttpRouteToken {
        HttpRouteToken {
            route_uid: self.route_uid.clone(),
            route_revision: self.route_revision,
        }
    }
}

#[derive(Deserialize)]
struct ResolvedRouteWithNamespace {
    #[serde(flatten)]
    route: RelayRouteInfo,
    #[serde(default)]
    local_endpoint_namespace: Option<String>,
}

fn resolved_routes_with_namespace(
    records: Vec<ResolvedRouteWithNamespace>,
    header_namespace: Option<String>,
) -> Result<RelayResolvedRoutes, HttpError> {
    // Header and additive JSON metadata describe the same relay domain. A
    // disagreement removes only IPC hints; the HTTP route remains usable.
    let has_header = header_namespace.is_some();
    let mut namespace = header_namespace;
    let mut disagreement = false;
    let mut missing_ipc_metadata = false;
    for record in &records {
        if let Some(value) = &record.local_endpoint_namespace {
            validate_local_endpoint_namespace(value)
                .map_err(|error| HttpError::Transport(error.to_string()))?;
            match &namespace {
                Some(namespace) if namespace != value => disagreement = true,
                None => namespace = Some(value.clone()),
                _ => {}
            }
        } else if record.route.ipc_address.is_some() {
            missing_ipc_metadata = true;
        }
    }
    // With no response header, partial JSON metadata must not reinterpret
    // unlabelled legacy IPC records as custom-root endpoints.
    let suppress_ipc = disagreement || (!has_header && namespace.is_some() && missing_ipc_metadata);
    let routes = records
        .into_iter()
        .map(|record| {
            let mut route = record.route;
            if suppress_ipc {
                route.ipc_address = None;
                route.server_id = None;
                route.server_instance_id = None;
            }
            route
        })
        .collect();
    Ok(RelayResolvedRoutes {
        routes,
        local_endpoint_namespace: namespace,
    })
}

/// Discovery metadata kept separately so existing `RelayRouteInfo` literals remain valid.
#[derive(Debug, Clone)]
pub struct RelayResolvedRoutes {
    pub routes: Vec<RelayRouteInfo>,
    pub local_endpoint_namespace: Option<String>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    routes: Vec<RelayRouteInfo>,
    local_endpoint_namespace: Option<String>,
    inserted_at: Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct RelayControlClientConfig {
    pub timeout: Duration,
    pub retry_attempts: usize,
    pub retry_delay: Duration,
    pub cache_ttl: Duration,
}

impl Default for RelayControlClientConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_delay: DEFAULT_RETRY_DELAY,
            cache_ttl: DEFAULT_CACHE_TTL,
        }
    }
}

pub struct RelayControlClient {
    client: reqwest::Client,
    base_url: String,
    config: RelayControlClientConfig,
    cache: Mutex<HashMap<ResolveCacheKey, CacheEntry>>,
    local_endpoint_context: Option<LocalEndpointContext>,
}

impl RelayControlClient {
    pub fn new(base_url: &str, use_proxy: bool) -> Result<Self, HttpError> {
        Self::new_with_config(base_url, use_proxy, RelayControlClientConfig::default())
    }

    pub fn new_with_config(
        base_url: &str,
        use_proxy: bool,
        config: RelayControlClientConfig,
    ) -> Result<Self, HttpError> {
        let client = crate::relay_client_builder_with_proxy(use_proxy)
            .timeout(config.timeout)
            .build()
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            base_url: canonical_base_url(base_url),
            config,
            cache: Mutex::new(HashMap::new()),
            local_endpoint_context: None,
        })
    }

    /// Freeze the resource owner's native context for registration and discovery.
    /// Custom-root owners must explicitly project their context before registering.
    /// Legacy constructors emit no header and retain default-namespace behavior.
    pub fn with_local_endpoint_context(mut self, context: &LocalEndpointContext) -> Self {
        self.local_endpoint_context = Some(context.clone());
        self.cache.get_mut().clear();
        self
    }

    pub fn register(&self, registration: RelayRegistration<'_>) -> Result<(), HttpError> {
        let route_name = registration.expected.route_name.clone();
        let request = registration.request(None, false);
        runtime().handle().block_on(self.post_json_with_retry(
            "/_register",
            &request,
            &[200, 201],
        ))?;
        self.invalidate(&route_name);
        Ok(())
    }

    pub fn prepare_register(
        &self,
        registration: RelayRegistration<'_>,
        registration_token: &str,
    ) -> Result<(), HttpError> {
        let request = registration.request(Some(registration_token), true);
        runtime()
            .handle()
            .block_on(self.post_json_with_retry("/_register", &request, &[202]))?;
        Ok(())
    }

    /// Compare-remove one registration. A superseded or already absent target is a safe no-op.
    pub fn unregister_registration(&self, scope: &RelayRegistrationScope) -> Result<(), HttpError> {
        runtime().handle().block_on(self.post_json_with_retry(
            "/_unregister",
            scope,
            &[200, 202, 204, 404],
        ))?;
        self.invalidate(&scope.name);
        Ok(())
    }

    /// Explicit administrative name/server-id removal; never used for ordinary Runtime teardown.
    pub fn admin_unregister(&self, name: &str, server_id: &str) -> Result<(), HttpError> {
        let request = AdminUnregisterRequest { name, server_id };
        runtime().handle().block_on(self.post_json_with_retry(
            "/_admin/unregister",
            &request,
            &[200, 204],
        ))?;
        self.invalidate(name);
        Ok(())
    }

    pub async fn resolve_matching_async(
        &self,
        expected: &ExpectedRouteContract,
    ) -> Result<Vec<RelayRouteInfo>, HttpError> {
        Ok(self
            .resolve_matching_with_namespace_async(expected)
            .await?
            .routes)
    }

    /// Resolve with optional response namespace without changing the legacy route surface.
    pub async fn resolve_matching_with_namespace_async(
        &self,
        expected: &ExpectedRouteContract,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        self.resolve_with_query_async(expected, self.local_endpoint_context.as_ref())
            .await
    }

    /// Project an already frozen Runtime context; never resolve process configuration here.
    pub async fn resolve_matching_with_context_async(
        &self,
        expected: &ExpectedRouteContract,
        context: &LocalEndpointContext,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        self.resolve_with_query_async(expected, Some(context)).await
    }

    async fn resolve_with_query_async(
        &self,
        expected: &ExpectedRouteContract,
        context: Option<&LocalEndpointContext>,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| HttpError::InvalidInput(err.to_string()))?;
        let mut cache_key = resolve_cache_key(expected);
        cache_key.local_endpoint_namespace =
            context.map(|context| context.namespace_id().to_owned());
        if let Some(routes) = self.cached_with_namespace(&cache_key) {
            return Ok(routes);
        }

        let path = format!(
            "/_resolve/{}?crm_ns={}&crm_name={}&crm_ver={}&abi_hash={}&signature_hash={}",
            encode_segment(&expected.route_name),
            encode_segment(&expected.crm_ns),
            encode_segment(&expected.crm_name),
            encode_segment(&expected.crm_ver),
            encode_segment(&expected.abi_hash),
            encode_segment(&expected.signature_hash),
        );
        let request = self.client.get(format!("{}{}", self.base_url, path));
        let request = match context {
            Some(context) => {
                request.header(LOCAL_ENDPOINT_NAMESPACE_HEADER, context.namespace_id())
            }
            None => request,
        };
        let resp = request
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if resp
            .headers()
            .get_all(LOCAL_ENDPOINT_NAMESPACE_HEADER)
            .iter()
            .count()
            > 1
        {
            return Err(HttpError::Transport(
                "repeated local endpoint namespace header".into(),
            ));
        }
        let local_endpoint_namespace = resp
            .headers()
            .get(LOCAL_ENDPOINT_NAMESPACE_HEADER)
            .map(|value| {
                let value = value.to_str().map_err(|_| {
                    HttpError::Transport("invalid local endpoint namespace header".into())
                })?;
                validate_local_endpoint_namespace(value)
                    .map_err(|error| HttpError::Transport(error.to_string()))?;
                Ok::<_, HttpError>(value.to_owned())
            })
            .transpose()?;
        let records = match status {
            200 => resp
                .json::<Vec<ResolvedRouteWithNamespace>>()
                .await
                .map_err(|e| HttpError::Transport(e.to_string()))?,
            404 => {
                let text = resp.text().await.unwrap_or_default();
                return Err(HttpError::ServerError(404, text));
            }
            code => {
                let text = resp.text().await.unwrap_or_default();
                return Err(HttpError::ServerError(code, text));
            }
        };
        let resolved = resolved_routes_with_namespace(records, local_endpoint_namespace)?;
        for route in &resolved.routes {
            validate_resolved_route(route)?;
        }

        self.cache.lock().insert(
            cache_key,
            CacheEntry {
                routes: resolved.routes.clone(),
                local_endpoint_namespace: resolved.local_endpoint_namespace.clone(),
                inserted_at: Instant::now(),
            },
        );
        Ok(resolved)
    }

    pub fn clear_cache(&self) {
        self.cache.lock().clear();
    }

    pub fn invalidate(&self, name: &str) {
        self.cache.lock().retain(|key, _| key.route_name() != name);
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn post_json_with_retry<T>(
        &self,
        path: &str,
        payload: &T,
        success_codes: &[u16],
    ) -> Result<(), HttpError>
    where
        T: Serialize + ?Sized,
    {
        let attempts = self.config.retry_attempts.max(1);
        let mut last_server_error = None;
        for attempt in 0..attempts {
            match self.post_json_once(path, payload, success_codes).await {
                Ok(()) => return Ok(()),
                Err(HttpError::ServerError(code, body)) if code >= 500 => {
                    last_server_error = Some(HttpError::ServerError(code, body));
                }
                Err(HttpError::Transport(message)) => {
                    last_server_error = Some(HttpError::Transport(message));
                }
                Err(err) => return Err(err),
            }

            if attempt + 1 < attempts {
                tokio::time::sleep(self.config.retry_delay).await;
            }
        }
        Err(last_server_error
            .unwrap_or_else(|| HttpError::Transport("relay control request failed".to_string())))
    }

    async fn post_json_once<T>(
        &self,
        path: &str,
        payload: &T,
        success_codes: &[u16],
    ) -> Result<(), HttpError>
    where
        T: Serialize + ?Sized,
    {
        let request = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(payload);
        let request = match &self.local_endpoint_context {
            Some(context) => {
                request.header(LOCAL_ENDPOINT_NAMESPACE_HEADER, context.namespace_id())
            }
            None => request,
        };
        let resp = request
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if success_codes.contains(&status) {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(HttpError::ServerError(status, text))
    }

    #[cfg(test)]
    fn cached(&self, key: &ResolveCacheKey) -> Option<Vec<RelayRouteInfo>> {
        self.cached_with_namespace(key)
            .map(|resolved| resolved.routes)
    }

    fn cached_with_namespace(&self, key: &ResolveCacheKey) -> Option<RelayResolvedRoutes> {
        let mut cache = self.cache.lock();
        let entry = cache.get(key)?;
        if entry.inserted_at.elapsed() >= self.config.cache_ttl {
            cache.remove(key);
            return None;
        }
        Some(RelayResolvedRoutes {
            routes: entry.routes.clone(),
            local_endpoint_namespace: entry.local_endpoint_namespace.clone(),
        })
    }
}

fn validate_resolved_route(route: &RelayRouteInfo) -> Result<(), HttpError> {
    c2_contract::validate_call_route_key("route_uid", &route.route_uid)
        .map_err(|err| HttpError::Transport(format!("malformed relay route token: {err}")))?;
    if route.route_revision == 0 {
        return Err(HttpError::Transport(
            "malformed relay route token: route_revision must be > 0".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_contract() -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.ns".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
        }
    }

    #[test]
    fn namespace_metadata_validation_is_strict() {
        let context = LocalEndpointContext::default_for_platform().unwrap();
        validate_local_endpoint_namespace(context.namespace_id()).unwrap();
        for invalid in [
            "",
            "root=/tmp",
            "v2:abc",
            &"A".repeat(64),
            &"a".repeat(63),
            &"a".repeat(65),
        ] {
            assert!(
                validate_local_endpoint_namespace(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn typed_registration_context_sends_only_namespace_and_legacy_sends_none() {
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::post,
        };
        use std::sync::Arc;
        let seen = Arc::new(Mutex::new(Vec::new()));
        async fn capture(
            State(seen): State<Arc<Mutex<Vec<Option<String>>>>>,
            headers: HeaderMap,
        ) -> StatusCode {
            seen.lock().push(
                headers
                    .get(LOCAL_ENDPOINT_NAMESPACE_HEADER)
                    .map(|value| value.to_str().unwrap().to_owned()),
            );
            StatusCode::OK
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/_register", post(capture))
            .with_state(seen.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let legacy = RelayControlClient::new(&url, false).unwrap();
        legacy
            .post_json_once("/_register", &serde_json::json!({}), &[200])
            .await
            .unwrap();
        let typed = RelayControlClient::new(&url, false)
            .unwrap()
            .with_local_endpoint_context(&context);
        typed
            .post_json_once("/_register", &serde_json::json!({}), &[200])
            .await
            .unwrap();
        assert_eq!(
            *seen.lock(),
            vec![None, Some(context.namespace_id().to_owned())]
        );
        handle.abort();
    }

    #[test]
    fn canonicalizes_base_url() {
        let client = RelayControlClient::new(" http://relay.test// ", false).unwrap();
        assert_eq!(client.base_url(), "http://relay.test");
    }

    #[test]
    fn relay_control_client_does_not_expose_name_only_resolve() {
        let source = include_str!("control.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("control.rs must contain production section");
        for forbidden in [
            "pub fn resolve(&self, name: &str)",
            "pub async fn resolve_async(&self, name: &str)",
            "expected: Option<&ExpectedRouteContract>",
            "ResolveCacheKey::Name",
            "None => format!(\"/_resolve/{}\"",
        ] {
            assert!(
                !production.contains(forbidden),
                "RelayControlClient must not keep name-only runtime resolve surface: {forbidden}",
            );
        }
    }

    #[test]
    fn route_cache_key_distinguishes_contract_hashes() {
        let left = expected_contract();
        let mut right = left.clone();
        right.signature_hash =
            "1111111111111111111111111111111111111111111111111111111111111111".to_string();

        assert_ne!(resolve_cache_key(&left), resolve_cache_key(&right));
    }

    #[test]
    fn resolve_matching_rejects_malformed_expected_contract_before_request() {
        let client = RelayControlClient::new_with_config(
            "http://127.0.0.1:9",
            false,
            RelayControlClientConfig {
                timeout: Duration::from_millis(50),
                retry_attempts: 1,
                retry_delay: Duration::from_millis(0),
                ..RelayControlClientConfig::default()
            },
        )
        .unwrap();
        let mut malformed = expected_contract();
        malformed.abi_hash = "not-a-sha256".to_string();

        let err = runtime()
            .handle()
            .block_on(client.resolve_matching_async(&malformed))
            .expect_err("malformed expected contract must be rejected locally");

        match err {
            HttpError::InvalidInput(message) => assert!(message.contains("abi_hash"), "{message}"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn namespace_response_cache_is_scoped_and_keeps_metadata() {
        let client = RelayControlClient::new("http://relay.test", false).unwrap();
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let legacy = resolve_cache_key(&expected_contract());
        let mut typed = legacy.clone();
        typed.local_endpoint_namespace = Some(context.namespace_id().into());
        client.cache.lock().insert(
            typed.clone(),
            CacheEntry {
                routes: vec![],
                local_endpoint_namespace: Some(context.namespace_id().into()),
                inserted_at: Instant::now(),
            },
        );
        assert!(client.cached_with_namespace(&legacy).is_none());
        assert_eq!(
            client
                .cached_with_namespace(&typed)
                .unwrap()
                .local_endpoint_namespace
                .as_deref(),
            Some(context.namespace_id())
        );
        client.invalidate("grid");
        assert!(client.cached_with_namespace(&typed).is_none());
    }

    #[test]
    fn additive_json_namespace_keeps_legacy_records_and_conflicts_keep_http() {
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let base = serde_json::json!({
            "name": "grid", "relay_url": "http://relay.test", "route_uid": "grid-route-uid-0001", "route_revision": 1,
            "ipc_address": "ipc://grid", "server_id": "grid", "server_instance_id": "grid-instance",
            "crm_ns": "test.ns", "crm_name": "Grid", "crm_ver": "0.1.0", "abi_hash": expected_contract().abi_hash,
            "signature_hash": expected_contract().signature_hash, "max_payload_size": 1024
        });
        let records =
            serde_json::from_value::<Vec<ResolvedRouteWithNamespace>>(serde_json::json!([base]))
                .unwrap();
        let legacy = resolved_routes_with_namespace(records, None).unwrap();
        assert!(legacy.local_endpoint_namespace.is_none());
        assert!(legacy.routes[0].ipc_address.is_some());
        let mut typed = base.clone();
        typed["local_endpoint_namespace"] = serde_json::json!(context.namespace_id());
        let records = serde_json::from_value(serde_json::json!([typed])).unwrap();
        let metadata_only = resolved_routes_with_namespace(records, None).unwrap();
        assert_eq!(
            metadata_only.local_endpoint_namespace.as_deref(),
            Some(context.namespace_id())
        );
        assert!(metadata_only.routes[0].ipc_address.is_some());
        typed["local_endpoint_namespace"] = serde_json::json!("a".repeat(64));
        let records = serde_json::from_value(serde_json::json!([typed])).unwrap();
        let conflict = resolved_routes_with_namespace(records, Some("b".repeat(64))).unwrap();
        assert!(conflict.routes[0].ipc_address.is_none());
        assert_eq!(conflict.routes[0].relay_url, "http://relay.test");
        assert_eq!(conflict.routes[0].route_uid, "grid-route-uid-0001");
    }

    #[test]
    fn route_cache_can_be_invalidated() {
        let client = RelayControlClient::new("http://relay.test", false).unwrap();
        let expected = expected_contract();
        client.cache.lock().insert(
            resolve_cache_key(&expected),
            CacheEntry {
                routes: vec![RelayRouteInfo {
                    name: "grid".into(),
                    relay_url: "http://relay-a.test".into(),
                    route_uid: "grid-route-uid-0001".into(),
                    route_revision: 1,
                    ipc_address: None,
                    server_id: None,
                    server_instance_id: None,
                    crm_ns: "".into(),
                    crm_name: "".into(),
                    crm_ver: "".into(),
                    abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .into(),
                    signature_hash:
                        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
                    max_payload_size: 1024,
                }],
                local_endpoint_namespace: None,
                inserted_at: Instant::now(),
            },
        );

        assert_eq!(
            client.cached(&resolve_cache_key(&expected)).unwrap()[0].relay_url,
            "http://relay-a.test"
        );
        client.invalidate("grid");
        assert!(client.cached(&resolve_cache_key(&expected)).is_none());
    }

    #[cfg(feature = "relay")]
    #[test]
    fn resolve_maps_404_status_from_real_relay_router() {
        let (addr, server) = runtime().handle().block_on(async {
            let state = crate::relay::test_support::test_state_for_client();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = crate::relay::router::build_router(state);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap();
            });
            (addr, server)
        });

        let client = RelayControlClient::new(&format!("http://{addr}"), false).unwrap();
        let mut expected = expected_contract();
        expected.route_name = "missing/name".to_string();
        let err = match runtime()
            .handle()
            .block_on(client.resolve_matching_async(&expected))
        {
            Ok(_) => panic!("missing route should return a status error"),
            Err(err) => err,
        };

        server.abort();
        match err {
            HttpError::ServerError(404, body) => {
                assert!(
                    body.contains("ResourceNotFound"),
                    "unexpected 404 body: {body}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
