use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use super::http_client::{HttpError, runtime};
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

fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, CONTROL_SEGMENT).to_string()
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

/// Routes and the relay namespace advertised by the discovery response header.
#[derive(Debug, Clone)]
pub struct RelayResolvedRoutes {
    pub routes: Vec<RelayRouteInfo>,
    pub local_endpoint_namespace: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct RelayControlClientConfig {
    pub timeout: Duration,
    pub retry_attempts: usize,
    pub retry_delay: Duration,
}

impl Default for RelayControlClientConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }
}

pub struct RelayControlClient {
    client: reqwest::Client,
    base_url: String,
    config: RelayControlClientConfig,
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
            local_endpoint_context: None,
        })
    }

    /// Freeze the resource owner's native context for registration and discovery.
    /// Custom-root owners must explicitly project their context before registering.
    /// Without this projection, control requests carry no namespace header.
    pub fn with_local_endpoint_context(mut self, context: &LocalEndpointContext) -> Self {
        self.local_endpoint_context = Some(context.clone());
        self
    }

    pub fn register(&self, registration: RelayRegistration<'_>) -> Result<(), HttpError> {
        let request = registration.request(None, false);
        runtime().handle().block_on(self.post_json_with_retry(
            "/_register",
            &request,
            &[200, 201],
        ))?;
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

    /// Resolve routes with the response's optional namespace header.
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

    pub(crate) async fn resolve_matching_with_namespace_deadline_async(
        &self,
        expected: &ExpectedRouteContract,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        super::connect_deadline::run(
            deadline,
            "relay_resolve",
            self.resolve_with_query_async(expected, self.local_endpoint_context.as_ref()),
        )
        .await
    }

    pub(crate) async fn resolve_matching_with_context_deadline_async(
        &self,
        expected: &ExpectedRouteContract,
        context: &LocalEndpointContext,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        super::connect_deadline::run(
            deadline,
            "relay_resolve",
            self.resolve_with_query_async(expected, Some(context)),
        )
        .await
    }

    async fn resolve_with_query_async(
        &self,
        expected: &ExpectedRouteContract,
        context: Option<&LocalEndpointContext>,
    ) -> Result<RelayResolvedRoutes, HttpError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| HttpError::InvalidInput(err.to_string()))?;
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
        let resp = request.send().await.map_err(HttpError::Request)?;
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
        Ok(RelayResolvedRoutes {
            routes,
            local_endpoint_namespace,
        })
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
                Err(HttpError::Request(error)) => {
                    last_server_error = Some(HttpError::Request(error));
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
        let resp = request.send().await.map_err(HttpError::Request)?;
        let status = resp.status().as_u16();
        if success_codes.contains(&status) {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(HttpError::ServerError(status, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "relay")]
    use parking_lot::Mutex;

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn each_acquisition_observes_current_relay_resolution() {
        use axum::{Router, http::StatusCode, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let app = Router::new().route(
            "/_resolve/{name}",
            get(move || {
                let request = observed.fetch_add(1, Ordering::SeqCst);
                async move {
                    if request == 0 {
                        (StatusCode::OK, "[]")
                    } else {
                        (StatusCode::CONFLICT, "current route conflict")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = RelayControlClient::new(&url, false).unwrap();
        let expected = expected_contract();
        assert!(
            client
                .resolve_matching_async(&expected)
                .await
                .unwrap()
                .is_empty()
        );
        let second = client.resolve_matching_async(&expected).await;
        assert!(
            matches!(second, Err(HttpError::ServerError(409, _))),
            "{second:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

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
            "None => format!(\"/_resolve/{}\"",
        ] {
            assert!(
                !production.contains(forbidden),
                "RelayControlClient must not keep name-only runtime resolve surface: {forbidden}",
            );
        }
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
