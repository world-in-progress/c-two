use std::{fmt, future::Future, sync::Arc, time::Duration};

use axum::body::{Body, to_bytes};
use c2_config::{RelayBridgeSpec, RelayConfig, ResolvedRelayClientConfig};
use futures::future::Either;
use http::{Method, Request, Response, StatusCode, header};
use http_body::Body as _;
use serde::{Serialize, de::DeserializeOwned};

use super::{
    iroh_transport::{IrohMeshTransport, MeshLane, PreparedIrohRequest},
    types::PeerAddress,
    url::peer_endpoint_url,
};

pub(super) const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// An outbound peer request failure, classified by whether dispatch could have begun.
#[derive(Debug)]
pub(crate) struct PeerRequestError {
    pre_dispatch: bool,
    message: String,
}

impl PeerRequestError {
    pub(super) fn before_dispatch(error: impl fmt::Display) -> Self {
        Self {
            pre_dispatch: true,
            message: error.to_string(),
        }
    }

    pub(super) fn after_dispatch(error: impl fmt::Display) -> Self {
        Self {
            pre_dispatch: false,
            message: error.to_string(),
        }
    }

    pub(crate) fn is_pre_dispatch(&self) -> bool {
        self.pre_dispatch
    }
}

impl fmt::Display for PeerRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PeerRequestError {}

/// The transport has prepared the request without handing it to the peer.
pub(super) enum PreparedPeerRequest<'a> {
    Http(&'a reqwest::Client, reqwest::Request),
    Iroh(PreparedIrohRequest),
}

impl PreparedPeerRequest<'_> {
    /// Hand ownership to the transport synchronously and return its reply future.
    pub(super) fn dispatch(self) -> impl Future<Output = Result<Response<Body>, PeerRequestError>> {
        match self {
            Self::Http(client, request) => {
                // Reqwest combines connection acquisition and sending. From
                // execute onward only an explicit connect error proves that
                // dispatch could not have occurred.
                let response = client.execute(request);
                Either::Left(async move {
                    let response = response.await.map_err(|error| {
                        if error.is_connect() {
                            PeerRequestError::before_dispatch(error)
                        } else {
                            PeerRequestError::after_dispatch(error)
                        }
                    })?;
                    let response: Response<reqwest::Body> = response.into();
                    Ok(response.map(Body::new))
                })
            }
            Self::Iroh(request) => Either::Right(request.dispatch()),
        }
    }
}

/// Shared transport for gossip and data-plane forwarding. Each lane has an
/// independent HTTP pool and QUIC connection, and sends a request at most once.
pub(crate) struct PeerHttpClient {
    control: reqwest::Client,
    data: reqwest::Client,
    iroh: Option<Arc<IrohMeshTransport>>,
    configured_bridge: Option<String>,
    max_control_response_bytes: usize,
    #[cfg(test)]
    dispatch_pause: parking_lot::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

impl PeerHttpClient {
    pub(super) fn new(
        config: &RelayConfig,
        iroh: Option<Arc<IrohMeshTransport>>,
        bridge: Option<&RelayBridgeSpec>,
    ) -> Result<Self, String> {
        let policy = ResolvedRelayClientConfig {
            use_proxy: config.use_proxy,
            ..Default::default()
        };
        let build = || -> Result<reqwest::Client, String> {
            crate::relay_client_builder(&policy)
                .map_err(|error| error.to_string())?
                .connect_timeout(CONTROL_TIMEOUT)
                .retry(reqwest::retry::never())
                .build()
                .map_err(|error| error.to_string())
        };
        Ok(Self {
            control: build()?,
            data: build()?,
            iroh,
            configured_bridge: bridge.map(RelayBridgeSpec::base_url),
            // Keep membership available when the relay closes business
            // admission. Larger tables grow with the configured role budget.
            max_control_response_bytes: usize::try_from(config.domain_limits.memory_budget_bytes)
                .unwrap_or(usize::MAX)
                .max(2 * 1024 * 1024),
            #[cfg(test)]
            dispatch_pause: parking_lot::Mutex::new(None),
        })
    }

    pub(super) fn iroh(&self) -> Option<&Arc<IrohMeshTransport>> {
        self.iroh.as_ref()
    }

    pub(super) fn local_address(&self, config: &RelayConfig) -> PeerAddress {
        match &self.iroh {
            Some(mesh) => PeerAddress::iroh(mesh.endpoint_addr(), self.configured_bridge.clone()),
            None => PeerAddress::http(config.mesh_url()),
        }
    }

    pub(crate) async fn send(
        &self,
        lane: MeshLane,
        address: &PeerAddress,
        request: Request<Body>,
    ) -> Result<Response<Body>, PeerRequestError> {
        self.prepare(lane, address, request).await?.dispatch().await
    }

    pub(super) async fn prepare(
        &self,
        lane: MeshLane,
        address: &PeerAddress,
        request: Request<Body>,
    ) -> Result<PreparedPeerRequest<'_>, PeerRequestError> {
        let prepared = if address.iroh.is_some() {
            let mesh = self.iroh.as_ref().ok_or_else(|| {
                PeerRequestError::before_dispatch("iroh mesh transport is disabled")
            })?;
            let remote = address
                .to_iroh_endpoint_addr(self.configured_bridge.as_deref())
                .ok_or_else(|| PeerRequestError::before_dispatch("invalid iroh peer address"))?;
            PreparedPeerRequest::Iroh(mesh.prepare_request(lane, &remote, request).await?)
        } else {
            self.prepare_http(lane, address, request)?
        };
        #[cfg(test)]
        self.wait_for_dispatch_pause().await;
        Ok(prepared)
    }

    fn prepare_http(
        &self,
        lane: MeshLane,
        address: &PeerAddress,
        request: Request<Body>,
    ) -> Result<PreparedPeerRequest<'_>, PeerRequestError> {
        let base = address
            .http_url
            .as_deref()
            .ok_or_else(|| PeerRequestError::before_dispatch("missing HTTP peer address"))?;
        let (parts, body) = request.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .ok_or_else(|| PeerRequestError::before_dispatch("missing peer request path"))?;
        let client = match lane {
            MeshLane::Control => &self.control,
            MeshLane::Data => &self.data,
        };
        let body = if body.is_end_stream() {
            reqwest::Body::from(bytes::Bytes::new())
        } else {
            reqwest::Body::wrap_stream(body.into_data_stream())
        };
        let request = client
            .request(parts.method, peer_endpoint_url(base, path.as_str()))
            .headers(parts.headers)
            .body(body)
            .build()
            .map_err(PeerRequestError::before_dispatch)?;
        Ok(PreparedPeerRequest::Http(client, request))
    }

    #[cfg(test)]
    pub(super) fn pause_before_dispatch(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (resume, wait) = tokio::sync::oneshot::channel();
        assert!(
            self.dispatch_pause
                .lock()
                .replace((entered, wait))
                .is_none()
        );
        (ready, resume)
    }

    #[cfg(test)]
    async fn wait_for_dispatch_pause(&self) {
        let pause = self.dispatch_pause.lock().take();
        if let Some((entered, wait)) = pause {
            let _ = entered.send(());
            let _ = wait.await;
        }
    }

    pub(super) async fn post_control(
        &self,
        address: &PeerAddress,
        path: &str,
        payload: &impl Serialize,
    ) -> Result<Response<Body>, PeerRequestError> {
        let payload = serde_json::to_vec(payload).map_err(PeerRequestError::before_dispatch)?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, payload.len())
            .body(Body::from(payload))
            .map_err(PeerRequestError::before_dispatch)?;
        self.send(MeshLane::Control, address, request).await
    }

    pub(super) async fn exchange_control<T: DeserializeOwned>(
        &self,
        address: &PeerAddress,
        path: &str,
        payload: &impl Serialize,
    ) -> Option<T> {
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            let response = self.post_control(address, path, payload).await.ok()?;
            if response.status() != StatusCode::OK {
                return None;
            }
            let bytes = to_bytes(response.into_body(), self.max_control_response_bytes)
                .await
                .ok()?;
            serde_json::from_slice(&bytes).ok()
        })
        .await
        .ok()
        .flatten()
    }
}
