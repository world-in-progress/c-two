use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use super::http_client::{HttpError, HttpRouteToken, runtime};
use c2_contract::ExpectedRouteContract;

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
}

impl ResolveCacheKey {
    fn route_name(&self) -> &str {
        &self.expected.route_name
    }
}

fn resolve_cache_key(expected: &ExpectedRouteContract) -> ResolveCacheKey {
    ResolveCacheKey {
        expected: expected.clone(),
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

#[derive(Debug, Clone, Serialize)]
struct UnregisterRequest<'a> {
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

#[derive(Debug, Clone)]
struct CacheEntry {
    routes: Vec<RelayRouteInfo>,
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
        })
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

    pub fn unregister(&self, name: &str, server_id: &str) -> Result<(), HttpError> {
        let request = UnregisterRequest { name, server_id };
        runtime().handle().block_on(self.post_json_with_retry(
            "/_unregister",
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
        self.resolve_with_query_async(expected).await
    }

    async fn resolve_with_query_async(
        &self,
        expected: &ExpectedRouteContract,
    ) -> Result<Vec<RelayRouteInfo>, HttpError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| HttpError::InvalidInput(err.to_string()))?;
        let cache_key = resolve_cache_key(expected);
        if let Some(routes) = self.cached(&cache_key) {
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
        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let routes = match status {
            200 => resp
                .json::<Vec<RelayRouteInfo>>()
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
        for route in &routes {
            validate_resolved_route(route)?;
        }

        self.cache.lock().insert(
            cache_key,
            CacheEntry {
                routes: routes.clone(),
                inserted_at: Instant::now(),
            },
        );
        Ok(routes)
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
        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(payload)
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

    fn cached(&self, key: &ResolveCacheKey) -> Option<Vec<RelayRouteInfo>> {
        let mut cache = self.cache.lock();
        let entry = cache.get(key)?;
        if entry.inserted_at.elapsed() >= self.config.cache_ttl {
            cache.remove(key);
            return None;
        }
        Some(entry.routes.clone())
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
