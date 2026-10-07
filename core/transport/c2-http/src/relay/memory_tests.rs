//! Real relay-owned budget/lifetime regressions (phase 2).
//!
//! The first six tests cover RelayState -> IpcClient -> LocalStream -> Server,
//! not HTTP. Connections perform real identity/contract/token handshakes; no
//! force_connected, injected pools, synthetic reservations, or acquire bypass.
//! The last test uses an actual TCP listener and build_router, including an
//! incomplete HTTP Content-Length upload and a later complete HTTP call.
//! IPC endpoints and file carriers have unique owner incarnations. The spool
//! directory is the canonical config projection; tests inspect only their own
//! carrier's exact debug path, never scan/delete shared spool directories.
//! Unix unlinks a spill at creation; Windows owns an exclusive delete-on-close
//! file. Live ownership is proved by carrier contents and budget charges, not
//! path existence or the ability to reopen that path.
//! The HTTP EOF case requires pre_dispatch and the original healthy client;
//! it intentionally rejects the baseline's pre-publication classification bug.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use c2_config::{ClientIpcConfig, RelayConfig};
use c2_ipc::{IpcClient, IpcError, ResponseData, RouteBinding};
use c2_mem::{BudgetSnapshot, DedicatedSegment, MemPool};
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
    RouteBuildSpec, SchedulerLimits, Server, ServerIdentity, ServerIpcConfig,
};
use c2_wire::chunk::ReassemblyBacking;
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::conn_pool::{CachedClient, UpstreamLease};
use super::router::build_router;
use super::state::{RegisterCommitResult, RelayState};
use super::test_support::{NoopDisseminator, TEST_ABI_HASH, TEST_SIGNATURE_HASH};
use super::types::RouteEntry;

const PAYLOAD_LEN: usize = 8192;
const CHUNK_SIZE: u64 = 4096;
const DEADLINE: Duration = Duration::from_secs(10);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("real transport/lifecycle step exceeded 10s")
}

// Aborting a failed test's parked call drops its body future and real armed
// RequestBlock. The normal path still joins it and checks the actual result.
struct PendingCall<T>(JoinHandle<T>);

impl<T> PendingCall<T> {
    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl<T> Drop for PendingCall<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn unique_id() -> String {
    format!(
        "rm_{}_{}_{:x}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn payload(marker: u8) -> Vec<u8> {
    (0..PAYLOAD_LEN)
        .map(|i| marker.wrapping_add(i as u8))
        .collect()
}

fn policy(shm: u64, file: u64, reassembly: u64) -> ClientIpcConfig {
    let mut config = ClientIpcConfig::default();
    config.base.pool_enabled = false;
    config.base.pool_prewarm_segments = 0;
    config.base.chunk_size = CHUNK_SIZE;
    config.base.shm_backing_budget_bytes = shm;
    config.base.file_backing_budget_bytes = file;
    config.base.live_reassembly_budget_bytes = reassembly;
    config.shm_threshold = 1;
    config.validate().unwrap();
    config
}

fn state(policy: ClientIpcConfig) -> Arc<RelayState> {
    Arc::new(RelayState::new(
        Arc::new(RelayConfig {
            relay_id: unique_id(),
            // No wall-clock owner expiry is involved in these tests.
            idle_timeout_secs: 0,
            upstream_ipc: policy,
            ..RelayConfig::default()
        }),
        Arc::new(NoopDisseminator),
    ))
}

/// Checks content through the Rust request lease and releases the actual
/// transport owner on every path (including failed content assertions).
/// Small replies identify the observed request carrier; large replies force
/// the server's checked chunk fallback and the relay's real receive allocation.
struct Probe {
    expected: Vec<u8>,
    large_reply: bool,
    calls: AtomicUsize,
    shm_calls: AtomicUsize,
    handle_calls: AtomicUsize,
}

impl CrmCallback for Probe {
    fn invoke(
        &self,
        _route: &str,
        method: u16,
        request: RequestData,
        _response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        let kind = match &request {
            RequestData::Shm {
                is_dedicated: true, ..
            } => {
                self.shm_calls.fetch_add(1, Ordering::SeqCst);
                "dedicated"
            }
            RequestData::Shm { .. } => "buddy",
            RequestData::Handle(_) => {
                self.handle_calls.fetch_add(1, Ordering::SeqCst);
                "handle"
            }
            RequestData::Inline(_) => "inline",
        };
        let mut lease = RequestLease::new(request);
        // This is Rust-side validation, not a Python bytes conversion before
        // dispatch. The server really receives SHM coordinates/owned Handle.
        let bytes = lease.copy_bytes().unwrap();
        lease
            .release()
            .expect("callback must release its actual request");
        assert_ne!(
            kind, "buddy",
            "buddy was disabled; actual owner released first"
        );
        if method == 1 {
            assert!(
                bytes.is_empty(),
                "response-only method has an inline empty request"
            );
        } else {
            assert_eq!(bytes, self.expected);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResponseMeta::Inline(if self.large_reply || method == 1 {
            self.expected.clone()
        } else {
            kind.as_bytes().to_vec()
        }))
    }
}

struct Upstream {
    server: Arc<Server>,
    task: JoinHandle<Result<(), c2_server::server::ServerError>>,
    entry: RouteEntry,
    probe: Arc<Probe>,
}

impl Upstream {
    async fn start(state: &RelayState, name: &str, marker: u8, large_reply: bool) -> Self {
        let id = unique_id();
        let address = format!("ipc://{id}");
        let mut config = ServerIpcConfig::default();
        config.base.pool_enabled = false;
        config.base.pool_prewarm_segments = 0;
        config.base.chunk_size = CHUNK_SIZE;
        // Request receive storage belongs to the server, not to the relay.
        // Denying server response SHM forces real reply reassembly in relay.
        config.base.shm_backing_budget_bytes = 0;
        config.base.file_backing_budget_bytes = 4 * PAYLOAD_LEN as u64;
        config.base.live_reassembly_budget_bytes = 4 * PAYLOAD_LEN as u64;
        config.shm_threshold = 1;
        config.validate().unwrap();
        let server = Arc::new(
            Server::new_with_identity(
                &address,
                config,
                ServerIdentity {
                    server_id: id.clone(),
                    server_instance_id: format!("{id}-i"),
                },
            )
            .unwrap(),
        );
        let probe = Arc::new(Probe {
            expected: payload(marker),
            large_reply,
            calls: AtomicUsize::new(0),
            shm_calls: AtomicUsize::new(0),
            handle_calls: AtomicUsize::new(0),
        });
        let route = server
            .build_route(
                RouteBuildSpec {
                    name: name.into(),
                    crm_ns: "test.echo".into(),
                    crm_name: "Echo".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["probe".into(), "reply".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                probe.clone(),
            )
            .unwrap();
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let running = server.clone();
        let task = tokio::spawn(async move { running.run().await });
        bounded(server.wait_until_responsive(Duration::from_secs(2)))
            .await
            .unwrap();
        // Attestation uses its own real, lazy control-plane connection.
        let mut attestor = IpcClient::new(&address);
        bounded(attestor.connect()).await.unwrap();
        let contract = attestor.route_contract(name).unwrap();
        let attestation = bounded(attestor.attest_route_for_registration(&contract))
            .await
            .unwrap();
        let entry = match test_commit_registration!(
            state,
            name.into(),
            id.clone(),
            format!("{id}-i"),
            address,
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            attestation.max_payload_size(),
            attestation.route_uid().to_string(),
            attestation.route_revision(),
            None,
        ) {
            RegisterCommitResult::Registered { entry } => entry,
            _ => panic!("attested live upstream registration failed"),
        };
        bounded(attestor.close()).await;
        Self {
            server,
            task,
            entry,
            probe,
        }
    }

    async fn acquire(&self, state: &RelayState) -> (UpstreamLease, Arc<IpcClient>, RouteBinding) {
        let (lease, entry, binding) =
            match bounded(state.acquire_upstream_for_route(&self.entry)).await {
                Ok(value) => value,
                Err(_) => panic!("real attested upstream acquisition failed"),
            };
        assert_eq!(entry.route_uid, self.entry.route_uid);
        let client = lease.client();
        assert!(client.is_connected());
        assert_eq!(client.server_id(), self.entry.server_id.as_deref());
        assert_eq!(
            client.server_instance_id(),
            self.entry.server_instance_id.as_deref()
        );
        (lease, client, binding)
    }

    async fn stop(self, state: &RelayState) {
        if let Some(client) = state.evict_connection(&self.entry.name) {
            close(&client).await;
        }
        bounded(self.server.shutdown_and_wait(Duration::from_secs(2)))
            .await
            .unwrap();
        bounded(self.task).await.unwrap().unwrap();
        assert!(
            !self.server.is_running(),
            "listener/server task must finish normally"
        );
        let budget = self.server.memory_budget_snapshot().budget;
        eprintln!(
            "[relay-memory] {} route={} callbacks={} shm_requests={} handle_requests={} server-after-join={budget:?}",
            state.relay_id(),
            self.entry.name,
            self.probe.calls.load(Ordering::SeqCst),
            self.probe.shm_calls.load(Ordering::SeqCst),
            self.probe.handle_calls.load(Ordering::SeqCst),
        );
        assert_eq!(
            (
                budget.shm.used_bytes,
                budget.file.used_bytes,
                budget.reassembly.used_bytes
            ),
            (0, 0, 0),
            "callbacks and server shutdown must refund actual receive carriers"
        );
        #[cfg(unix)]
        assert!(!std::path::Path::new(self.server.local_endpoint().os_name()).exists());
    }
}

async fn close(client: &IpcClient) {
    assert!(
        bounded(client.close_shared_bounded(Duration::from_secs(2))).await,
        "close must confirm joins and pool detach, not silently time out"
    );
    assert!(!client.is_connected());
}

fn snapshot(state: &RelayState, label: &str) -> BudgetSnapshot {
    let s = state.upstream_memory_snapshot();
    eprintln!("[relay-memory] {} {label}: {s:?}", state.relay_id());
    for cell in [s.shm, s.file, s.reassembly] {
        assert!(cell.peak_bytes <= cell.limit_bytes);
        assert!(cell.used_bytes <= cell.limit_bytes);
    }
    s
}

fn refunded(state: &RelayState) {
    let s = snapshot(state, "refunded");
    assert_eq!(
        (s.shm.used_bytes, s.file.used_bytes, s.reassembly.used_bytes),
        (0, 0, 0)
    );
}

fn file_carrier(response: ResponseData, expected: &[u8]) -> ReassemblyBacking {
    let ResponseData::Handle(backing) = response else {
        panic!("real server chunk fallback must return ReassemblyBacking");
    };
    assert!(
        backing.is_file_spill(),
        "this receive allocation must use the file fallback"
    );
    assert_eq!(backing.capacity_bytes(), PAYLOAD_LEN as u64);
    assert_eq!(backing.copy_bytes().unwrap(), expected);
    assert_live_file_path_contract(&backing);
    backing
}

fn assert_live_file_path_contract(backing: &ReassemblyBacking) {
    // This is a debug path, not a handle. Unix removes the directory entry
    // before sizing/mapping the still-owned file; Windows share_mode(0) and
    // delete-on-close prohibit using reopen/exists as a live-owner proof.
    let path = backing
        .file_spill_path()
        .expect("file carrier records its debug path");
    #[cfg(unix)]
    assert!(
        !path.exists(),
        "Unix must unlink a spill at creation: {path:?}"
    );
    #[cfg(not(unix))]
    let _ = path;
}

fn release(backing: &mut ReassemblyBacking) {
    let path = backing.file_spill_path().unwrap();
    backing.release().unwrap();
    backing.release().unwrap();
    assert!(backing.is_released());
    assert!(backing.copy_bytes().is_err());
    assert!(
        !path.exists(),
        "released file carrier must leave no directory entry at its exact debug path"
    );
}

async fn call(client: &IpcClient, binding: &RouteBinding, upstream: &Upstream) -> ResponseData {
    bounded(client.call_bound(binding, "probe", &upstream.probe.expected))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_upstreams_share_request_budget_and_checked_chunk_fallback() {
    // One real dedicated allocation fits exactly; two cannot fit together.
    let charge = DedicatedSegment::required_shm_size(PAYLOAD_LEN).unwrap() as u64;
    let state = state(policy(charge, 0, 0));
    let a = Upstream::start(&state, "a", 17, false).await;
    let b = Upstream::start(&state, "b", 93, false).await;
    assert_ne!(a.entry.ipc_address, b.entry.ipc_address);
    // Both real endpoints independently succeed on the preferred SHM path.
    for upstream in [&a, &b] {
        let (lease, client, binding) = upstream.acquire(&state).await;
        assert_eq!(
            call(&client, &binding, upstream).await.into_inline_bytes(),
            b"dedicated"
        );
        drop(lease);
        close(&state.evict_connection(&upstream.entry.name).unwrap()).await;
        refunded(&state);
    }
    let (lease_a, client_a, binding_a) = a.acquire(&state).await;
    let (lease_b, client_b, binding_b) = b.acquire(&state).await;
    let before = snapshot(&state, "requests-before-overlap");
    let (ready_tx, ready_rx) = oneshot::channel();
    let (body_tx, body_rx) = oneshot::channel();
    let pending = PendingCall(tokio::spawn(async move {
        // IpcClient allocates its real SHM block before polling the body.
        // No frame is published until this channel provides the entire body.
        let stream = futures::stream::once(async move {
            let _ = ready_tx.send(());
            body_rx.await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "test body producer cancelled",
                )
            })
        });
        let response = client_a
            .call_bound_sized_stream(&binding_a, "probe", PAYLOAD_LEN as u64, stream)
            .await;
        drop(lease_a);
        response
    }));
    bounded(ready_rx).await.unwrap();
    let held = snapshot(&state, "request-a-unpublished");
    assert_eq!(held.shm.used_bytes, charge);
    assert!(
        state
            .evict_idle(0)
            .iter()
            .all(|(_, client)| client.is_none()),
        "active leases prevent idle eviction"
    );
    assert_eq!(
        call(&client_b, &binding_b, &b).await.into_inline_bytes(),
        b"handle"
    );
    let competed = snapshot(&state, "request-b-fallback");
    assert_eq!(competed.shm.peak_bytes, charge);
    assert!(competed.shm.rejected_allocations > before.shm.rejected_allocations);
    assert!(competed.shm.rejected_bytes >= before.shm.rejected_bytes + charge);
    body_tx.send(a.probe.expected.clone()).unwrap();
    assert_eq!(
        bounded(pending.join())
            .await
            .unwrap()
            .unwrap()
            .into_inline_bytes(),
        b"dedicated"
    );
    drop(lease_b);
    assert_eq!(a.probe.shm_calls.load(Ordering::SeqCst), 2);
    assert_eq!(b.probe.shm_calls.load(Ordering::SeqCst), 1);
    assert_eq!(b.probe.handle_calls.load(Ordering::SeqCst), 1);
    a.stop(&state).await;
    b.stop(&state).await;
    refunded(&state);
}

// Both request storage and response reassembly must charge the same SHM cell,
// in addition to each role sharing its limits across upstream connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpublished_request_and_reply_reassembly_compete_for_same_shm_budget() {
    let charge = DedicatedSegment::required_shm_size(PAYLOAD_LEN).unwrap() as u64;
    let state = state(policy(charge, PAYLOAD_LEN as u64, PAYLOAD_LEN as u64));
    let a = Upstream::start(&state, "request", 23, false).await;
    let b = Upstream::start(&state, "reply", 181, true).await;
    // Each role individually succeeds with real dedicated SHM storage.
    let (lease, client, binding) = a.acquire(&state).await;
    assert_eq!(
        call(&client, &binding, &a).await.into_inline_bytes(),
        b"dedicated"
    );
    drop(lease);
    close(&state.evict_connection(&a.entry.name).unwrap()).await;
    refunded(&state);
    let (lease, client, binding) = b.acquire(&state).await;
    let response = bounded(client.call_bound(&binding, "reply", &[]))
        .await
        .unwrap();
    let ResponseData::Handle(mut solo) = response else {
        panic!("expected real chunked response");
    };
    assert!(solo.is_dedicated());
    assert_eq!(solo.copy_bytes().unwrap(), b.probe.expected);
    let standalone = snapshot(&state, "reply-reassembly-alone-uses-SHM");
    assert_eq!(standalone.shm.used_bytes, charge);
    assert_eq!(standalone.file.used_bytes, 0);
    solo.release().unwrap();
    drop(solo);
    drop(lease);
    close(&state.evict_connection(&b.entry.name).unwrap()).await;
    refunded(&state);

    let (lease_a, client_a, binding_a) = a.acquire(&state).await;
    let (lease_b, client_b, binding_b) = b.acquire(&state).await;
    let (ready_tx, ready_rx) = oneshot::channel();
    let (body_tx, body_rx) = oneshot::channel();
    let pending = PendingCall(tokio::spawn(async move {
        let stream = futures::stream::once(async move {
            let _ = ready_tx.send(());
            body_rx.await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "test body producer cancelled",
                )
            })
        });
        let response = client_a
            .call_bound_sized_stream(&binding_a, "probe", PAYLOAD_LEN as u64, stream)
            .await;
        drop(lease_a);
        response
    }));
    bounded(ready_rx).await.unwrap();
    let held = snapshot(&state, "request-SHM-held-before-reply");
    assert_eq!(held.shm.used_bytes, charge);
    let response = bounded(client_b.call_bound(&binding_b, "reply", &[]))
        .await
        .unwrap();
    let mut fallback = file_carrier(response, &b.probe.expected);
    let overlap = snapshot(
        &state,
        "reply-reassembly-file-fallback-competes-with-request",
    );
    assert_eq!(overlap.shm.used_bytes, charge);
    assert_eq!(overlap.shm.peak_bytes, charge);
    assert_eq!(
        overlap.shm.rejected_allocations,
        held.shm.rejected_allocations + 1
    );
    assert_eq!(overlap.shm.rejected_bytes, held.shm.rejected_bytes + charge);
    assert_eq!(
        (overlap.file.used_bytes, overlap.reassembly.used_bytes),
        (PAYLOAD_LEN as u64, PAYLOAD_LEN as u64)
    );
    release(&mut fallback);
    body_tx.send(a.probe.expected.clone()).unwrap();
    assert_eq!(
        bounded(pending.join())
            .await
            .unwrap()
            .unwrap()
            .into_inline_bytes(),
        b"dedicated"
    );
    assert_eq!(a.probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(b.probe.calls.load(Ordering::SeqCst), 2);
    drop(lease_b);
    a.stop(&state).await;
    b.stop(&state).await;
    refunded(&state);
}

async fn shared_response_capacity(file_is_tighter: bool) {
    let one = PAYLOAD_LEN as u64;
    let limit = one + one / 2;
    let state = state(policy(
        0,
        if file_is_tighter { limit } else { 2 * one },
        if file_is_tighter { 2 * one } else { limit },
    ));
    let a = Upstream::start(&state, "a", 31, true).await;
    let b = Upstream::start(&state, "b", 149, true).await;
    let (lease_a, client_a, binding_a) = a.acquire(&state).await;
    let (lease_b, client_b, binding_b) = b.acquire(&state).await;
    // Each independently admits a real chunked reply and removes its file.
    for (upstream, client, binding) in [(&a, &client_a, &binding_a), (&b, &client_b, &binding_b)] {
        let mut backing = file_carrier(
            call(client, binding, upstream).await,
            &upstream.probe.expected,
        );
        release(&mut backing);
        refunded(&state);
    }
    let mut old = file_carrier(call(&client_a, &binding_a, &a).await, &a.probe.expected);
    let before = snapshot(&state, "reply-a-held");
    assert_eq!(
        (before.file.used_bytes, before.reassembly.used_bytes),
        (one, one)
    );
    // The second allocation overlaps the first carrier's live receive scope.
    // Its callback executes exactly once; local capacity failure is not replayed.
    let error = bounded(client_b.call_bound(&binding_b, "probe", &b.probe.expected))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Chunk(c2_ipc::client::ChunkError::Capacity(_))
        ),
        "{error}"
    );
    let after = snapshot(&state, "reply-b-capacity-rejection");
    assert_eq!(
        (after.file.used_bytes, after.reassembly.used_bytes),
        (one, one)
    );
    let (before_cell, after_cell) = if file_is_tighter {
        (before.file, after.file)
    } else {
        (before.reassembly, after.reassembly)
    };
    assert_eq!(
        after_cell.rejected_allocations,
        before_cell.rejected_allocations + 1
    );
    assert_eq!(after_cell.rejected_bytes, before_cell.rejected_bytes + one);
    assert_eq!(b.probe.calls.load(Ordering::SeqCst), 2);
    assert!(client_a.is_connected() && client_b.is_connected());
    assert_eq!(old.copy_bytes().unwrap(), a.probe.expected);
    release(&mut old);
    refunded(&state);
    let mut retry = file_carrier(call(&client_b, &binding_b, &b).await, &b.probe.expected);
    release(&mut retry);
    assert_eq!(b.probe.calls.load(Ordering::SeqCst), 3);
    drop((lease_a, lease_b));
    a.stop(&state).await;
    b.stop(&state).await;
    refunded(&state);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_upstreams_share_live_reassembly_capacity() {
    shared_response_capacity(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_upstreams_share_file_capacity_and_refund_failed_admission() {
    shared_response_capacity(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_file_carrier_keeps_charge_across_idle_close_and_reconnect() {
    let one = PAYLOAD_LEN as u64;
    let state = state(policy(0, 2 * one, one + one / 2));
    let upstream = Upstream::start(&state, "held", 203, true).await;
    let (lease, old_client, binding) = upstream.acquire(&state).await;
    let mut old = file_carrier(
        call(&old_client, &binding, &upstream).await,
        &upstream.probe.expected,
    );
    let path = old.file_spill_path().unwrap();
    drop(lease);
    let idle = state.evict_idle(0);
    assert_eq!(idle.len(), 1);
    let detached = idle.into_iter().next().unwrap().1.unwrap();
    assert!(Arc::ptr_eq(&detached, &old_client));
    close(&detached).await;
    let held = snapshot(&state, "finished-carrier-after-confirmed-close");
    assert_eq!(
        (held.file.used_bytes, held.reassembly.used_bytes),
        (one, one)
    );
    assert_live_file_path_contract(&old);
    assert_eq!(old.copy_bytes().unwrap(), upstream.probe.expected);
    let (lease, new_client, binding) = upstream.acquire(&state).await;
    assert!(!Arc::ptr_eq(&old_client, &new_client));
    let reconnected = snapshot(&state, "finished-carrier-after-reconnect");
    assert_eq!(
        (
            reconnected.file.used_bytes,
            reconnected.reassembly.used_bytes
        ),
        (one, one)
    );
    let error = bounded(new_client.call_bound(&binding, "probe", &upstream.probe.expected))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Chunk(c2_ipc::client::ChunkError::Capacity(_))
        ),
        "{error}"
    );
    let rejected = snapshot(&state, "new-incarnation-competes-with-old-carrier");
    assert_eq!(
        rejected.reassembly.rejected_allocations,
        held.reassembly.rejected_allocations + 1
    );
    assert_eq!(old.copy_bytes().unwrap(), upstream.probe.expected);
    release(&mut old);
    refunded(&state);
    let mut fresh = file_carrier(
        call(&new_client, &binding, &upstream).await,
        &upstream.probe.expected,
    );
    assert_ne!(fresh.file_spill_path().unwrap(), path);
    release(&mut fresh);
    assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 3);
    drop(lease);
    upstream.stop(&state).await;
    refunded(&state);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_response_budgets_reject_real_allocation_without_charge_or_replay() {
    for file_zero in [false, true] {
        let one = PAYLOAD_LEN as u64;
        let state = state(policy(
            0,
            if file_zero { 0 } else { one },
            if file_zero { one } else { 0 },
        ));
        let upstream = Upstream::start(&state, "zero", 67, true).await;
        let (lease, client, binding) = upstream.acquire(&state).await;
        let before = snapshot(&state, "zero-before-real-call");
        let error = bounded(client.call_bound(&binding, "probe", &upstream.probe.expected))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                IpcError::Chunk(c2_ipc::client::ChunkError::Capacity(_))
            ),
            "{error}"
        );
        let after = snapshot(&state, "zero-after-real-call");
        let (before_cell, after_cell) = if file_zero {
            (before.file, after.file)
        } else {
            (before.reassembly, after.reassembly)
        };
        assert_eq!(after.shm.peak_bytes, 0);
        assert!(after.shm.rejected_allocations > before.shm.rejected_allocations);
        assert_eq!(after_cell.limit_bytes, 0);
        assert_eq!(
            after_cell.rejected_allocations,
            before_cell.rejected_allocations + 1
        );
        assert_eq!(after_cell.rejected_bytes, before_cell.rejected_bytes + one);
        assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.probe.handle_calls.load(Ordering::SeqCst), 1);
        assert!(client.is_connected());
        drop(lease);
        upstream.stop(&state).await;
        refunded(&state);
    }
}

fn http_headers(entry: &RouteEntry) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in [
        ("x-c2-expected-crm-ns", entry.crm_ns.as_str()),
        ("x-c2-expected-crm-name", entry.crm_name.as_str()),
        ("x-c2-expected-crm-ver", entry.crm_ver.as_str()),
        ("x-c2-expected-abi-hash", entry.abi_hash.as_str()),
        (
            "x-c2-expected-signature-hash",
            entry.signature_hash.as_str(),
        ),
        ("x-c2-route-uid", entry.route_uid.as_str()),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    headers.insert(
        "x-c2-route-revision",
        entry.route_revision.to_string().parse().unwrap(),
    );
    headers
}

// Real HTTP -> router -> RelayState -> LocalStream/IPC. The follow-up lookup
// exchanges control frames on the original client; a cached token binding
// alone is insufficient to prove liveness. Neither invokes a resource method.
async fn exercise_http_upload(
    state: &Arc<RelayState>,
    upstream: &Upstream,
    original_client: &Arc<IpcClient>,
    address: std::net::SocketAddr,
    charge: u64,
) {
    let mut upload = bounded(tokio::net::TcpStream::connect(address))
        .await
        .unwrap();
    let mut head = format!(
        "POST /http/probe HTTP/1.1\r\nHost: {address}\r\nContent-Length: {PAYLOAD_LEN}\r\nConnection: close\r\n"
    );
    for (key, value) in &http_headers(&upstream.entry) {
        head.push_str(&format!(
            "{}: {}\r\n",
            key.as_str(),
            value.to_str().unwrap()
        ));
    }
    head.push_str("\r\n");
    bounded(upload.write_all(head.as_bytes())).await.unwrap();
    // Explicit HTTP framing: 64 body bytes of declared 8192, then no more.
    bounded(upload.write_all(&upstream.probe.expected[..64]))
        .await
        .unwrap();
    bounded(async {
        while state.upstream_memory_snapshot().shm.used_bytes == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let live = snapshot(&state, "HTTP-upload-before-publication");
    assert_eq!(live.shm.used_bytes, charge);
    assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 0);
    assert!(
        state
            .evict_idle(0)
            .iter()
            .all(|(_, client)| client.is_none())
    );
    // A real TCP write EOF, not cancellation of an in-process handler future.
    bounded(upload.shutdown()).await.unwrap();
    let mut reply = Vec::new();
    bounded(upload.read_to_end(&mut reply)).await.unwrap();
    drop(upload);
    eprintln!(
        "[relay-memory] HTTP incomplete upload EOF: response={}",
        String::from_utf8_lossy(&reply)
    );
    assert_eq!(
        upstream.probe.calls.load(Ordering::SeqCst),
        0,
        "partial HTTP body must never publish IPC call",
    );
    let header_end = reply
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("actual HTTP EOF must return a framed error response");
    assert!(
        reply.starts_with(b"HTTP/1.1 502 "),
        "expected an HTTP body-read error, received {}",
        String::from_utf8_lossy(&reply)
    );
    let error: serde_json::Value = serde_json::from_slice(&reply[header_end + 4..]).unwrap();
    assert_eq!(error["code"], 702);
    assert_eq!(error["details"]["route"], "http");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("request body stream error")
    );
    assert_eq!(
        error["details"]["dispatch_phase"], "pre_dispatch",
        "HTTP EOF occurred while preparing SHM, before IPC publication; do not classify it as possible dispatch or evict the healthy client: {error}",
    );
    assert!(
        original_client.is_connected(),
        "pre-publication HTTP EOF closed the original healthy IPC client"
    );
    assert!(
        matches!(
            state.connection_lookup(&upstream.entry.name),
            CachedClient::Ready { .. }
        ),
        "pre-publication HTTP EOF lost the healthy Ready slot before idle eviction"
    );
    let expected = original_client
        .route_contract(&upstream.entry.name)
        .expect("original client must retain the attested contract");
    bounded(original_client.lookup_route(&expected))
        .await
        .expect("original HTTP upload client must still complete real IPC route lookup");
    // Pointer identity rejects a fresh reconnect that would otherwise conceal
    // the unwanted eviction. This acquire may bind a cached route token.
    let (lease, surviving_client, binding) = upstream.acquire(&state).await;
    assert!(
        Arc::ptr_eq(&original_client, &surviving_client),
        "HTTP EOF replaced the original client instead of preserving it"
    );
    assert_eq!(
        surviving_client.server_id(),
        upstream.entry.server_id.as_deref()
    );
    assert_eq!(
        surviving_client.server_instance_id(),
        upstream.entry.server_instance_id.as_deref()
    );
    assert_eq!(binding.route_uid(), upstream.entry.route_uid);
    assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 0);
    drop(lease);
    let mut idle = state.evict_idle(0);
    assert_eq!(
        idle.len(),
        1,
        "HTTP EOF response must already have returned the handler lease; no polling for an evicted client"
    );
    let detached = idle
        .pop()
        .unwrap()
        .1
        .expect("healthy original client must be idle, not already evicted");
    assert!(Arc::ptr_eq(&detached, &original_client));
    // An unpublished freed dedicated mapping may stay cached until the pool
    // closes. The backing charge must then refund on the same shared context.
    snapshot(&state, "HTTP-body-error-original-client-survived");
    close(&detached).await;
    refunded(&state);

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = bounded(
        http.post(format!("http://{address}/http/probe"))
            .headers(http_headers(&upstream.entry))
            .body(upstream.probe.expected.clone())
            .send(),
    )
    .await
    .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    assert_eq!(
        bounded(response.bytes()).await.unwrap().as_ref(),
        upstream.probe.expected
    );
    assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.probe.shm_calls.load(Ordering::SeqCst), 1);
    let complete = snapshot(&state, "HTTP-complete-call-consumed-response");
    assert!(complete.reassembly.peak_bytes >= PAYLOAD_LEN as u64);
    assert_eq!(
        (complete.file.used_bytes, complete.reassembly.used_bytes),
        (0, 0)
    );
    let idle = state.evict_idle(0);
    assert_eq!(idle.len(), 1);
    close(&idle.into_iter().next().unwrap().1.unwrap()).await;
    refunded(&state);
    // Reconnect after successful-call idle eviction, through HTTP again.
    let response = bounded(
        http.post(format!("http://{address}/http/probe"))
            .headers(http_headers(&upstream.entry))
            .body(upstream.probe.expected.clone())
            .send(),
    )
    .await
    .unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        bounded(response.bytes()).await.unwrap().as_ref(),
        upstream.probe.expected
    );
    assert_eq!(upstream.probe.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_http_incomplete_upload_and_completed_call_refund_after_idle_close() {
    let charge = DedicatedSegment::required_shm_size(PAYLOAD_LEN).unwrap() as u64;
    let state = state(policy(charge, PAYLOAD_LEN as u64, PAYLOAD_LEN as u64));
    let upstream = Upstream::start(&state, "http", 113, true).await;
    // Keep the original acquired client so an unnoticed eviction/replacement
    // cannot masquerade as survival of the HTTP upload's connection.
    let (lease, original_client, _) = upstream.acquire(&state).await;
    drop(lease);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = oneshot::channel();
    let app = build_router(state.clone());
    let mut http_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
    });
    // Preserve assertion failures while still closing/joining our listeners.
    // This also makes the expected baseline phase failure a narrow repro,
    // rather than a timeout followed by abandoned server tasks.
    let verification = AssertUnwindSafe(exercise_http_upload(
        &state,
        &upstream,
        &original_client,
        address,
        charge,
    ))
    .catch_unwind()
    .await;

    // Do all independent cleanup steps before rethrowing the primary failure.
    // A failed cleanup is recorded, never substituted for the phase assertion.
    let http_cleanup = AssertUnwindSafe(async {
        let _ = stop_tx.send(());
        match tokio::time::timeout(DEADLINE, &mut http_task).await {
            Ok(joined) => joined.unwrap().unwrap(),
            Err(_) => {
                http_task.abort();
                let _ = (&mut http_task).await;
                panic!("HTTP server failed graceful shutdown; test-owned task aborted and joined");
            }
        }
        assert!(
            tokio::net::TcpStream::connect(address).await.is_err(),
            "HTTP listener must be closed after normal task join"
        );
    })
    .catch_unwind()
    .await;
    let original_close = tokio::time::timeout(
        DEADLINE,
        original_client.close_shared_bounded(Duration::from_secs(2)),
    )
    .await;
    let server_cleanup = AssertUnwindSafe(upstream.stop(&state)).catch_unwind().await;
    if let Err(primary) = verification {
        eprintln!(
            "[relay-memory] HTTP failed-assertion cleanup: http_join_ok={} original_close={original_close:?} upstream_join_ok={}",
            http_cleanup.is_ok(),
            server_cleanup.is_ok()
        );
        std::panic::resume_unwind(primary);
    }
    if let Err(error) = http_cleanup {
        std::panic::resume_unwind(error);
    }
    assert!(
        matches!(original_close, Ok(true)),
        "original IPC close cleanup did not confirm: {original_close:?}"
    );
    if let Err(error) = server_cleanup {
        std::panic::resume_unwind(error);
    }
    refunded(&state);
}
