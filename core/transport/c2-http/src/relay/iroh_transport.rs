//! End-to-end mesh connections over direct QUIC or a private iroh bridge.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock, Weak},
    task::{Context, Poll},
    time::Duration,
};

use axum::{Router, body::Body};
use c2_config::{RelayBridgeSpec, RelayConfig, RelayTlsTrust};
use http::{Request, Response};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey,
    endpoint::{
        AfterHandshakeOutcome, BeforeConnectOutcome, Connection, EndpointHooks,
        QuicTransportConfig, VarInt, presets,
    },
};
use iroh_relay::{RelayQuicConfig, tls::CaTlsConfig};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::{sync::OnceCell, task::JoinSet, time::timeout};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use super::{peer_client::PeerRequestError, transport::IrohQuicBiStream};
use crate::{
    private_state::{constant_time_eq, persistent_hex32, state_dir},
    tls::relay_server_cert_verifier,
};

pub const MESH_AUTH_ALPN: &[u8] = b"c2/mesh-auth/9";
pub const MESH_CTRL_ALPN: &[u8] = b"c2/mesh-ctrl/9";
pub const MESH_DATA_ALPN: &[u8] = b"c2/mesh-data/9";
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_DENIED: VarInt = VarInt::from_u32(403);
const AUTH_DENIED_REASON: &[u8] = b"unauthenticated mesh peer";

type MeshError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeshLane {
    Control,
    Data,
}

impl MeshLane {
    fn alpn(self) -> &'static [u8] {
        match self {
            Self::Control => MESH_CTRL_ALPN,
            Self::Data => MESH_DATA_ALPN,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AuthenticatedIrohPeer {
    pub endpoint_id: EndpointId,
}

type LaneCell = Arc<tokio::sync::Mutex<Option<Connection>>>;

pub struct IrohMeshTransport {
    endpoint: Arc<Endpoint>,
    auth: Arc<MeshAuth>,
    lanes: Mutex<HashMap<(EndpointId, MeshLane), LaneCell>>,
}

pub(super) struct PreparedIrohRequest {
    sender: SendRequest<Body>,
    request: Request<Body>,
    driver: AbortOnDropHandle<()>,
}

impl PreparedIrohRequest {
    pub(super) fn dispatch(
        mut self,
    ) -> impl Future<Output = Result<Response<Body>, PeerRequestError>> {
        let response = self.sender.try_send_request(self.request);
        let driver = self.driver;
        async move {
            let response = response.await.map_err(|error| {
                if error.message().is_some() {
                    PeerRequestError::before_dispatch(error.error())
                } else {
                    PeerRequestError::after_dispatch(error.error())
                }
            })?;
            Ok(response.map(|body| {
                Body::new(MeshResponseBody {
                    body,
                    _driver: driver,
                })
            }))
        }
    }
}

impl IrohMeshTransport {
    pub(super) fn load_identity(config: &RelayConfig) -> Result<SecretKey, String> {
        let dir = state_dir(config.state_dir.as_deref(), ".c3-relay")?;
        let key = persistent_hex32(&dir, "mesh-key.ed25519")?;
        Ok(SecretKey::from_bytes(&key))
    }

    pub(super) async fn bind_with(
        config: &RelayConfig,
        key: SecretKey,
        bridge: Option<RelayBridgeSpec>,
        #[cfg(test)] relay_only: bool,
    ) -> Result<Self, MeshError> {
        let secret = config
            .cluster_secret
            .as_ref()
            .expect("validated mesh secret");
        let auth = Arc::new(MeshAuth::new(secret.as_bytes()));
        // Keep eight streams' worth of credit on each connection and reserve
        // separate send and receive windows. A small floor carries control and
        // refusal responses even when business capacity is zero.
        let window =
            (config.domain_limits.memory_budget_bytes / 16).clamp(4096, 1024 * 1024) as u32;
        let transport = QuicTransportConfig::builder()
            .stream_receive_window(VarInt::from_u32(window))
            .receive_window(VarInt::from_u32(window * 8))
            .send_window(u64::from(window) * 8)
            .build();
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .alpns(
                [MESH_AUTH_ALPN, MESH_CTRL_ALPN, MESH_DATA_ALPN]
                    .map(<[u8]>::to_vec)
                    .to_vec(),
            )
            .hooks(AuthHook(auth.clone()))
            .transport_config(transport);
        if let Some(spec) = bridge {
            let trust = spec
                .cert_sha256()
                .map_or(RelayTlsTrust::SystemDefault, RelayTlsTrust::CertSha256);
            builder = builder.ca_tls_config(CaTlsConfig::custom_server_cert_verifier(Arc::new(
                move |provider| {
                    relay_server_cert_verifier(&trust, provider).map_err(std::io::Error::other)
                },
            )));
            let bridge = iroh::RelayConfig::new(
                spec.base_url().parse()?,
                spec.quic_port().map(RelayQuicConfig::new),
            )
            .with_auth_token(
                config
                    .bridge_token
                    .as_ref()
                    .expect("validated bridge token"),
            );
            builder = builder.relay_mode(RelayMode::Custom(bridge.into()));
        } else {
            builder = builder.relay_mode(RelayMode::Disabled);
        }
        #[cfg(test)]
        if relay_only {
            builder = builder.clear_ip_transports();
        }
        let endpoint = Arc::new(builder.bind().await?);
        auth.endpoint
            .set(Arc::downgrade(&endpoint))
            .expect("new auth hook");
        Ok(Self {
            endpoint,
            auth,
            lanes: Mutex::default(),
        })
    }

    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    async fn connection(
        &self,
        lane: MeshLane,
        addr: &EndpointAddr,
    ) -> Result<Connection, MeshError> {
        let cell = self
            .lanes
            .lock()
            .entry((addr.id, lane))
            .or_default()
            .clone();
        let mut current = cell.lock().await;
        if let Some(conn) = current.take() {
            if conn.close_reason().is_none() {
                *current = Some(conn.clone());
                return Ok(conn);
            }
        }
        let conn = self.endpoint.connect(addr.clone(), lane.alpn()).await?;
        *current = Some(conn.clone());
        Ok(conn)
    }

    /// Open a lane stream and set up HTTP without submitting the request.
    pub(super) async fn prepare_request(
        &self,
        lane: MeshLane,
        remote_addr: &EndpointAddr,
        mut request: Request<Body>,
    ) -> Result<PreparedIrohRequest, PeerRequestError> {
        let conn = self
            .connection(lane, remote_addr)
            .await
            .map_err(PeerRequestError::before_dispatch)?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(PeerRequestError::before_dispatch)?;
        request
            .headers_mut()
            .entry(http::header::HOST)
            .or_insert(http::HeaderValue::from_static("mesh"));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(TokioIo::new(
            IrohQuicBiStream::new(send, recv, None),
        ))
        .await
        .map_err(PeerRequestError::before_dispatch)?;
        let driver = AbortOnDropHandle::new(tokio::spawn(async move {
            // Keep the QUIC connection live for the entire response even if
            // the transport removes its cached lane.
            let _conn = conn;
            let _ = driver.await;
        }));
        sender
            .ready()
            .await
            .map_err(PeerRequestError::before_dispatch)?;
        Ok(PreparedIrohRequest {
            sender,
            request,
            driver,
        })
    }

    #[cfg(test)]
    async fn send_request(
        &self,
        lane: MeshLane,
        remote_addr: &EndpointAddr,
        request: Request<Body>,
    ) -> Result<Response<Body>, PeerRequestError> {
        self.prepare_request(lane, remote_addr, request)
            .await?
            .dispatch()
            .await
    }

    pub(super) async fn serve(&self, control: Router, data: Router, stop: CancellationToken) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => {}
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    let (control, data, auth, stop) =
                        (control.clone(), data.clone(), self.auth.clone(), stop.clone());
                    connections.spawn(async move {
                        let connection = tokio::select! {
                            _ = stop.cancelled() => return,
                            result = incoming => match result {
                                Ok(conn) => conn,
                                Err(_) => return,
                            },
                        };
                        match connection.alpn() {
                            MESH_AUTH_ALPN => {
                                if !matches!(timeout(AUTH_TIMEOUT, auth.respond(&connection)).await, Ok(Ok(()))) {
                                    connection.close(AUTH_DENIED, AUTH_DENIED_REASON);
                                }
                            }
                            MESH_CTRL_ALPN => serve_connection(connection, control).await,
                            MESH_DATA_ALPN => serve_connection(connection, data).await,
                            _ => unreachable!("mesh ALPNs are fixed at bind time"),
                        }
                    });
                }
            }
        }
        self.endpoint.close().await;
        connections.join_all().await;
        self.lanes.lock().clear();
    }
}

struct MeshResponseBody {
    body: Incoming,
    // Dropping the response also drops the QUIC stream, even while Hyper's
    // driver is waiting for network data and has nothing to write.
    _driver: AbortOnDropHandle<()>,
}

impl http_body::Body for MeshResponseBody {
    type Data = bytes::Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

async fn serve_connection(connection: Connection, app: Router) {
    let peer = AuthenticatedIrohPeer {
        endpoint_id: connection.remote_id(),
    };
    let mut streams = JoinSet::new();
    loop {
        tokio::select! {
            _ = streams.join_next(), if !streams.is_empty() => {}
            stream = connection.accept_bi() => {
                let Ok((send, recv)) = stream else { break; };
                streams.spawn(IrohQuicBiStream::serve(send, recv, app.clone(), peer));
            }
        }
    }
    streams.join_all().await;
}

#[derive(Clone)]
struct AuthHook(Arc<MeshAuth>);

impl fmt::Debug for AuthHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeshAuth").finish_non_exhaustive()
    }
}

impl EndpointHooks for AuthHook {
    async fn before_connect(&self, addr: &EndpointAddr, alpn: &[u8]) -> BeforeConnectOutcome {
        if alpn == MESH_AUTH_ALPN || self.0.authenticate(addr).await {
            BeforeConnectOutcome::Accept
        } else {
            BeforeConnectOutcome::Reject
        }
    }

    async fn after_handshake(&self, conn: &Connection) -> AfterHandshakeOutcome {
        if conn.alpn() == MESH_AUTH_ALPN || self.0.peers.lock().contains(&conn.remote_id()) {
            AfterHandshakeOutcome::Accept
        } else {
            AfterHandshakeOutcome::Reject {
                error_code: AUTH_DENIED,
                reason: AUTH_DENIED_REASON.to_vec(),
            }
        }
    }
}

struct MeshAuth {
    endpoint: OnceLock<Weak<Endpoint>>,
    key: HmacSha256,
    peers: Mutex<HashSet<EndpointId>>,
    attempts: Mutex<HashMap<EndpointId, Arc<OnceCell<bool>>>>,
}

impl MeshAuth {
    fn new(secret: &[u8]) -> Self {
        Self {
            endpoint: OnceLock::new(),
            key: HmacSha256::new(secret),
            peers: Mutex::default(),
            attempts: Mutex::default(),
        }
    }

    async fn authenticate(&self, addr: &EndpointAddr) -> bool {
        // A new connection proves both sides again. A remote restart can
        // discard its authentication state without changing its endpoint ID.
        let attempt = self.attempts.lock().entry(addr.id).or_default().clone();
        let result = *attempt
            .get_or_init(|| async {
                let Some(endpoint) = self.endpoint.get().and_then(Weak::upgrade) else {
                    return false;
                };
                matches!(
                    timeout(AUTH_TIMEOUT, self.initiate(&endpoint, addr.clone())).await,
                    Ok(Ok(()))
                )
            })
            .await;
        let mut attempts = self.attempts.lock();
        if attempts
            .get(&addr.id)
            .is_some_and(|current| Arc::ptr_eq(current, &attempt))
        {
            attempts.remove(&addr.id);
        }
        result
    }

    async fn initiate(&self, endpoint: &Endpoint, addr: EndpointAddr) -> Result<(), MeshError> {
        let conn = endpoint.connect(addr, MESH_AUTH_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let mut nonce_a = [0; 32];
        getrandom::fill(&mut nonce_a)?;
        send.write_all(&nonce_a).await?;
        let mut nonce_b = [0; 32];
        let mut tag_b = [0; 32];
        recv.read_exact(&mut nonce_b).await?;
        recv.read_exact(&mut tag_b).await?;
        let (a, b) = (endpoint.id(), conn.remote_id());
        let expected = self
            .key
            .tag(b"c2-mesh-auth-v9-resp", b, a, &nonce_a, &nonce_b);
        if !constant_time_eq(&expected, &tag_b) {
            return Err("invalid mesh responder proof".into());
        }
        send.write_all(
            &self
                .key
                .tag(b"c2-mesh-auth-v9-init", a, b, &nonce_a, &nonce_b),
        )
        .await?;
        send.finish()?;
        if recv.read_to_end(1).await? != [1] {
            return Err("invalid mesh authentication acknowledgement".into());
        }
        self.peers.lock().insert(b);
        Ok(())
    }

    async fn respond(&self, conn: &Connection) -> Result<(), MeshError> {
        let endpoint = self
            .endpoint
            .get()
            .and_then(Weak::upgrade)
            .ok_or("mesh endpoint closed")?;
        let (a, b) = (conn.remote_id(), endpoint.id());
        let (mut send, mut recv) = conn.accept_bi().await?;
        let mut nonce_a = [0; 32];
        let mut nonce_b = [0; 32];
        recv.read_exact(&mut nonce_a).await?;
        getrandom::fill(&mut nonce_b)?;
        send.write_all(&nonce_b).await?;
        send.write_all(
            &self
                .key
                .tag(b"c2-mesh-auth-v9-resp", b, a, &nonce_a, &nonce_b),
        )
        .await?;
        let tag_a = recv.read_to_end(32).await?;
        let tag_a: [u8; 32] = tag_a
            .try_into()
            .map_err(|_| "invalid mesh initiator proof length")?;
        let expected = self
            .key
            .tag(b"c2-mesh-auth-v9-init", a, b, &nonce_a, &nonce_b);
        if !constant_time_eq(&expected, &tag_a) {
            return Err("invalid mesh initiator proof".into());
        }
        self.peers.lock().insert(a);
        send.write_all(&[1]).await?;
        send.finish()?;
        // Retain the connection until the peer has its acknowledgement.
        send.stopped().await?;
        Ok(())
    }
}

struct HmacSha256 {
    inner: Sha256,
    outer: Sha256,
}

impl HmacSha256 {
    fn new(secret: &[u8]) -> Self {
        let digest;
        let secret = if secret.len() > 64 {
            digest = Sha256::digest(secret);
            digest.as_slice()
        } else {
            secret
        };
        let mut inner = [0x36; 64];
        let mut outer = [0x5c; 64];
        for (i, &byte) in secret.iter().enumerate() {
            inner[i] ^= byte;
            outer[i] ^= byte;
        }
        Self {
            inner: Sha256::new_with_prefix(inner),
            outer: Sha256::new_with_prefix(outer),
        }
    }

    fn tag(
        &self,
        domain: &[u8],
        sender: EndpointId,
        recipient: EndpointId,
        a: &[u8; 32],
        b: &[u8; 32],
    ) -> [u8; 32] {
        self.sign(&[domain, sender.as_bytes(), recipient.as_bytes(), a, b])
    }

    fn sign(&self, message: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        for part in message {
            inner.update(part);
        }
        self.outer
            .clone()
            .chain_update(inner.finalize())
            .finalize()
            .into()
    }
}

#[cfg(test)]
mod tests;
