//! Real local-stream deadline regressions. Run on Unix and Windows; the peer
//! pauses control replies rather than using process suspension or signals.

use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use c2_config::{ConnectDeadline, ConnectOptions, LocalEndpoint};
use c2_contract::ExpectedRouteContract;
use c2_error::ErrorCode;
use c2_ipc::{ClientPool, IpcError};
use c2_local::{LocalListener, LocalStream};
use c2_wire::flags;
use c2_wire::frame::{FrameHeader, decode_frame_body, encode_frame};
use c2_wire::handshake::{
    CAP_CALL_V2, CAP_CHUNKED, CAP_METHOD_IDX, MethodEntry, RouteInfo, ServerIdentity,
    encode_server_handshake,
};
use c2_wire::route_catalog_control::{
    RouteContractWire, RouteLookupResponse, RouteMethodWire, RouteRecordWire, RouteStateWire,
    decode_route_lookup_request, encode_route_lookup_response,
};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

const ROUTE_UID: &str = "connect-timeout-grid-0001";
const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SIGNATURE_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
static ADDRESS_SEQ: AtomicU64 = AtomicU64::new(0);

fn expected() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test.connect_timeout".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: ABI_HASH.into(),
        signature_hash: SIGNATURE_HASH.into(),
    }
}

fn handshake() -> Vec<u8> {
    let expected = expected();
    let route = RouteInfo {
        name: expected.route_name,
        route_uid: ROUTE_UID.into(),
        route_revision: 1,
        crm_ns: expected.crm_ns,
        crm_name: expected.crm_name,
        crm_ver: expected.crm_ver,
        abi_hash: expected.abi_hash,
        signature_hash: expected.signature_hash,
        max_payload_size: 1024,
        methods: vec![MethodEntry {
            name: "ping".into(),
            index: 0,
        }],
    };
    let payload = encode_server_handshake(
        &[],
        CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED,
        &[route],
        "",
        &ServerIdentity {
            server_id: "deadline-server".into(),
            server_instance_id: "deadline-instance".into(),
        },
    )
    .unwrap();
    encode_frame(0, flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE, &payload)
}

fn ready_reply() -> Vec<u8> {
    let expected = expected();
    encode_route_lookup_response(&RouteLookupResponse::Ready {
        current: RouteRecordWire {
            route_name: expected.route_name.clone(),
            route_uid: ROUTE_UID.into(),
            route_revision: 1,
            catalog_revision: 1,
            owner_server_id: "deadline-server".into(),
            owner_server_instance_id: "deadline-instance".into(),
            owner_epoch: 1,
            contract: RouteContractWire {
                route_name: expected.route_name,
                crm_ns: expected.crm_ns,
                crm_name: expected.crm_name,
                crm_ver: expected.crm_ver,
                abi_hash: expected.abi_hash,
                signature_hash: expected.signature_hash,
            },
            methods: vec![RouteMethodWire {
                name: "ping".into(),
                index: 0,
            }],
            max_payload_size: 1024,
            state: RouteStateWire::Ready,
            state_reason: None,
            lease_deadline_ms: None,
        },
    })
    .unwrap()
}

async fn read_frame(stream: &mut LocalStream) -> (FrameHeader, Vec<u8>) {
    let mut len = [0_u8; 4];
    stream
        .read_exact(&mut len)
        .await
        .expect("read frame length");
    let total_len = u32::from_le_bytes(len);
    let mut body = vec![0; total_len as usize];
    stream.read_exact(&mut body).await.expect("read frame body");
    let (header, payload) = decode_frame_body(&body, total_len).expect("decode frame");
    (header, payload.to_vec())
}

async fn read_handshake(stream: &mut LocalStream) {
    let (header, _) = read_frame(stream).await;
    assert!(header.is_handshake());
}

async fn read_lookup(stream: &mut LocalStream) -> FrameHeader {
    let (header, payload) = read_frame(stream).await;
    assert!(
        header.is_ctrl(),
        "connect must send control frames, never business calls"
    );
    let request = decode_route_lookup_request(&payload).expect("contract-scoped route lookup");
    assert_eq!(request.expected.route_name, "grid");
    assert_eq!(request.expected.abi_hash, ABI_HASH);
    header
}

async fn reply(stream: &mut LocalStream, header: FrameHeader, payload: &[u8]) {
    stream
        .write_all(&encode_frame(
            header.request_id,
            flags::FLAG_CTRL | flags::FLAG_RESPONSE,
            payload,
        ))
        .await
        .expect("control reply");
}

// Teardown cancels and joins the fixture, including when an assertion fails.
// No endpoint sweep or cleanup of addresses belonging to other tests is used.
struct Peer {
    address: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Peer {
    fn spawn<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(LocalListener) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let address = format!(
            "ipc://connect_deadline_{}_{}",
            std::process::id(),
            ADDRESS_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let endpoint = LocalEndpoint::from_address(&address).expect("fixture endpoint");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop, stopped) = oneshot::channel();
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = LocalListener::bind(&endpoint).expect("fixture listener");
                    ready_tx.send(()).unwrap();
                    tokio::select! {
                        _ = stopped => {},
                        result = tokio::time::timeout(Duration::from_secs(10), serve(listener)) => {
                            result.expect("fixture must finish within its guard");
                        },
                    }
                });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener readiness");
        Self {
            address,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn finish(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().expect("peer task");
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn deadline(timeout: Duration) -> ConnectDeadline {
    ConnectDeadline::start(ConnectOptions::new().with_timeout(timeout)).unwrap()
}

fn assert_deadline(error: IpcError, stage: &str) {
    let IpcError::LocalCallRejected(error) = error else {
        panic!("expected canonical connect deadline, got {error:?}")
    };
    assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
    assert_eq!(
        error.details.get("operation").map(String::as_str),
        Some("connect")
    );
    assert_eq!(
        error.details.get("transport_phase").map(String::as_str),
        Some("pre_dispatch")
    );
    assert_eq!(error.details.get("stage").map(String::as_str), Some(stage));
    assert_eq!(
        error.details.get("fallback_eligible").map(String::as_str),
        Some("false")
    );
}

#[test]
fn zero_timeout_expires_before_endpoint_resolution_or_cache_mutation() {
    let pool = ClientPool::new(Duration::from_secs(60));
    // A malformed address would fail endpoint resolution if any OS acquisition
    // began. The immediate deadline must take precedence without freezing the
    // first-connect memory domain or adding a cache reference.
    let error = pool
        .acquire_lease_with_deadline("not-an-ipc-address", None, deadline(Duration::ZERO))
        .err()
        .expect("zero timeout must expire immediately");
    assert_deadline(error, "pool_wait");
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.refcount("not-an-ipc-address"), 0);
    assert!(pool.memory_budget_observer().is_none());
}

#[test]
fn first_handshake_timeout_cannot_publish_late_client_and_next_connect_recovers() {
    let (resume, resumed) = oneshot::channel();
    let (late_tx, late_rx) = mpsc::channel();
    let peer = Peer::spawn(move |mut listener| async move {
        let mut stalled = listener.accept().await.unwrap();
        read_handshake(&mut stalled).await;
        let _ = resumed.await;
        // Returning after the caller expired must not publish this connection.
        let _ = stalled.write_all(&handshake()).await;
        drop(stalled);
        late_tx.send(()).unwrap();
        let mut healthy = listener.accept().await.unwrap();
        read_handshake(&mut healthy).await;
        healthy.write_all(&handshake()).await.unwrap();
        std::future::pending::<()>().await;
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let started = Instant::now();
    let error = pool
        .acquire_with_deadline(&peer.address, None, deadline(Duration::from_millis(100)))
        .err()
        .expect("nonresponding handshake must expire");
    assert_deadline(error, "handshake");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "caller budget must precede the 5s phase guard"
    );
    assert_eq!(pool.refcount(&peer.address), 0);
    assert!(!pool.has_client(&peer.address));
    resume.send(()).unwrap();
    late_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !pool.has_client(&peer.address),
        "late handshake must not enter cache"
    );
    let recovered = pool
        .acquire_with_deadline(&peer.address, None, deadline(Duration::from_secs(2)))
        .unwrap();
    assert!(recovered.is_connected());
    assert_eq!(pool.refcount(&peer.address), 1);
    assert!(pool.release_if_same(&peer.address, &recovered));
    assert_eq!(pool.refcount(&peer.address), 0);
    pool.close_all(Duration::from_secs(2));
    peer.finish();
}

#[test]
fn cached_client_route_timeout_removes_pending_and_preserves_healthy_stream() {
    let (resume, resumed) = oneshot::channel();
    let (lookup_tx, lookup_rx) = mpsc::channel();
    let (late_tx, late_rx) = mpsc::channel();
    let peer = Peer::spawn(move |mut listener| async move {
        let mut stream = listener.accept().await.unwrap();
        read_handshake(&mut stream).await;
        stream.write_all(&handshake()).await.unwrap();
        let first = read_lookup(&mut stream).await;
        lookup_tx.send(()).unwrap();
        let _ = resumed.await;
        reply(&mut stream, first, &ready_reply()).await;
        late_tx.send(()).unwrap();
        let second = read_lookup(&mut stream).await;
        reply(&mut stream, second, &ready_reply()).await;
        std::future::pending::<()>().await;
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let original = pool.acquire(&peer.address, None).unwrap();
    assert!(pool.release_if_same(&peer.address, &original));
    let cached = pool
        .acquire_with_deadline(&peer.address, None, deadline(Duration::from_secs(2)))
        .unwrap();
    assert!(
        Arc::ptr_eq(&original, &cached),
        "fixture must use an already connected cached client"
    );
    let error = cached
        .acquire_route_with_deadline(&expected(), deadline(Duration::from_millis(100)))
        .err()
        .expect("paused live route lookup must expire");
    lookup_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_deadline(error, "route_acquire");
    #[cfg(feature = "test-support")]
    assert_eq!(cached.pending_len_for_test(), 0);
    assert!(
        cached.is_connected(),
        "cancelling an awaiting reply must preserve the complete stream"
    );
    assert!(pool.release_if_same(&peer.address, &cached));
    assert_eq!(pool.refcount(&peer.address), 0);
    resume.send(()).unwrap();
    late_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let recovered = pool
        .acquire_with_deadline(&peer.address, None, deadline(Duration::from_secs(2)))
        .unwrap();
    assert!(Arc::ptr_eq(&recovered, &original));
    let binding = recovered
        .acquire_route_with_deadline(&expected(), deadline(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(binding.route_uid(), ROUTE_UID);
    #[cfg(feature = "test-support")]
    assert_eq!(recovered.pending_len_for_test(), 0);
    assert!(pool.release_if_same(&peer.address, &recovered));
    assert_eq!(pool.refcount(&peer.address), 0);
    pool.close_all(Duration::from_secs(2));
    peer.finish();
}

#[test]
fn handshake_and_route_lookup_consume_one_absolute_budget() {
    let peer = Peer::spawn(move |mut listener| async move {
        let mut stream = listener.accept().await.unwrap();
        read_handshake(&mut stream).await;
        tokio::time::sleep(Duration::from_millis(160)).await;
        stream.write_all(&handshake()).await.unwrap();
        read_lookup(&mut stream).await;
        std::future::pending::<()>().await;
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let budget = deadline(Duration::from_millis(350));
    let client = pool
        .acquire_with_deadline(&peer.address, None, budget)
        .unwrap();
    let started = Instant::now();
    let error = client
        .acquire_route_with_deadline(&expected(), budget)
        .err()
        .expect("remaining total budget must expire");
    assert_deadline(error, "route_acquire");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "route lookup must consume the remaining budget, not reset it"
    );
    assert!(budget.remaining("test").is_err());
    #[cfg(feature = "test-support")]
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(pool.release_if_same(&peer.address, &client));
    assert_eq!(pool.refcount(&peer.address), 0);
    pool.close_all(Duration::from_secs(2));
    peer.finish();
}

#[test]
fn transient_handshake_retry_keeps_original_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let peer = Peer::spawn(move |mut listener| async move {
        let mut first = listener.accept().await.unwrap();
        read_handshake(&mut first).await;
        observed.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(160)).await;
        drop(first); // A transient EOF is safe to retry before dispatch.
        let mut second = listener.accept().await.unwrap();
        read_handshake(&mut second).await;
        observed.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let budget = deadline(Duration::from_millis(350));
    let started = Instant::now();
    let error = pool
        .acquire_with_deadline(&peer.address, None, budget)
        .err()
        .expect("retry shares the original deadline");
    assert_deadline(error, "handshake");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() < Duration::from_millis(430),
        "each retry must not receive a fresh timeout"
    );
    assert!(budget.remaining("test").is_err());
    assert_eq!(pool.refcount(&peer.address), 0);
    assert!(!pool.has_client(&peer.address));
    peer.finish();
}

#[test]
fn route_publication_retry_sleep_is_inside_connect_budget() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&lookups);
    let peer = Peer::spawn(move |mut listener| async move {
        let mut stream = listener.accept().await.unwrap();
        read_handshake(&mut stream).await;
        stream.write_all(&handshake()).await.unwrap();
        let not_found = encode_route_lookup_response(&RouteLookupResponse::NotFound {
            route_name: "grid".into(),
        })
        .unwrap();
        loop {
            let header = read_lookup(&mut stream).await;
            observed.fetch_add(1, Ordering::SeqCst);
            reply(&mut stream, header, &not_found).await;
        }
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let client = pool.acquire(&peer.address, None).unwrap();
    let started = Instant::now();
    let error = client
        .acquire_route_with_deadline(&expected(), deadline(Duration::from_millis(80)))
        .err()
        .expect("publication retry must respect caller deadline");
    assert_deadline(error, "route_acquire");
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(
        lookups.load(Ordering::SeqCst) >= 2,
        "fixture must exercise retries"
    );
    #[cfg(feature = "test-support")]
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(pool.release_if_same(&peer.address, &client));
    assert_eq!(pool.refcount(&peer.address), 0);
    peer.finish();
    pool.close_all(Duration::from_secs(2));
}

// This is intentionally a composite T2/T9 gate: a matching cached exact token
// must still await the live authoritative lookup covered by the deadline.
#[test]
fn matching_cached_route_token_still_has_live_lookup_deadline() {
    let (lookup_tx, lookup_rx) = mpsc::channel();
    let peer = Peer::spawn(move |mut listener| async move {
        let mut stream = listener.accept().await.unwrap();
        read_handshake(&mut stream).await;
        stream.write_all(&handshake()).await.unwrap();
        read_lookup(&mut stream).await;
        lookup_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let pool = ClientPool::new(Duration::from_secs(60));
    let client = pool.acquire(&peer.address, None).unwrap();
    let error = client
        .acquire_route_token_with_deadline(
            &expected(),
            ROUTE_UID,
            1,
            deadline(Duration::from_millis(100)),
        )
        .err()
        .expect("matching cached token must not bypass the live lookup");
    lookup_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_deadline(error, "route_acquire");
    #[cfg(feature = "test-support")]
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(pool.release_if_same(&peer.address, &client));
    assert_eq!(pool.refcount(&peer.address), 0);
    pool.close_all(Duration::from_secs(2));
    peer.finish();
}
