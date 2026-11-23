//! ADR 0006 acceptance tests: keyed unary submit, recover, non-blocking
//! query, outcome retention/eviction and the shared sealed-block delivery.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use c2_contract::ValidatedContractDescriptor;
use c2_error::ErrorCode;
use c2_local::LocalStream;
use c2_mem::DomainBudget;
use c2_wire::buddy::BuddyPayload;
use c2_wire::frame::FrameHeader;
use c2_wire::handshake::CAP_SHM_RESPONSE;
use c2_wire::rpc::{
    self, CallMode, CallStatus, RouteCallIdentity, RpcRecord, ValuePart, ValueStorage,
    WireCallOutcome, WireCallOutcomeStatus, WireQueryCallResult, WireResultUnavailableReason,
};

use crate::dispatcher::{CrmCallback, CrmError, ResponseMeta, RouteBuildSpec};
use crate::scheduler::{ConcurrencyMode, SchedulerLimits};
use crate::server::{Server, ServerIdentity, ServerIpcConfig, handle_connection};
use c2_config::DomainLimits;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

/// Unary callback with an entry gate: every invocation publishes one entry
/// token and blocks until one release token arrives, then returns the
/// configured payload.
struct GatedCallback {
    entries: Arc<AtomicUsize>,
    response: Vec<u8>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
}

impl CrmCallback for GatedCallback {
    fn invoke(
        &self,
        _route: &str,
        _method: u16,
        request: Vec<u8>,
        _pool: Arc<crate::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.entries.fetch_add(1, Ordering::SeqCst);
        let _ = self.entered.send(());
        let mut release = self.release.lock().unwrap();
        if let Some(release) = release.as_mut() {
            // Consume exactly one release token per invocation.
            let _ = release.blocking_recv();
        }
        drop(release);
        assert_ne!(request, b"panic", "gated callback panic");
        Ok(ResponseMeta::Inline(self.response.clone()))
    }
}

#[derive(Clone)]
struct CallbackHandle {
    entries: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>>,
    release: tokio::sync::mpsc::UnboundedSender<()>,
}

impl CallbackHandle {
    fn executed(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }

    async fn wait_entry(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.lock().await.recv())
            .await
            .expect("resource entry before timeout")
            .expect("callback");
    }

    fn release_one(&self) {
        let _ = self.release.send(());
    }
}

struct OutcomeHarness {
    server: Arc<Server>,
    peer: Option<LocalStream>,
    handler: tokio::task::JoinHandle<()>,
    identity: RouteCallIdentity,
    callback: CallbackHandle,
    next_id: u64,
    capabilities: u16,
}

impl OutcomeHarness {
    async fn start(
        config: ServerIpcConfig,
        limits: DomainLimits,
        response: Vec<u8>,
    ) -> OutcomeHarness {
        Self::start_with_caps(config, limits, response, 0).await
    }

    async fn start_with_caps(
        config: ServerIpcConfig,
        limits: DomainLimits,
        response: Vec<u8>,
        capabilities: u16,
    ) -> OutcomeHarness {
        let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
        let server_id = format!("outcome-test-{}", uuid::Uuid::new_v4().simple());
        let server = Server::new_with_identity_endpoint_and_budget(
            c2_config::LocalEndpointContext::default_for_platform()
                .unwrap()
                .endpoint(&format!("ipc://{server_id}"))
                .unwrap(),
            config,
            ServerIdentity {
                server_id,
                server_instance_id: uuid::Uuid::new_v4().simple().to_string(),
            },
            DomainBudget::from_limits(&limits),
        )
        .unwrap();
        let server = Arc::new(server);
        let (entered_tx, entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
        let entries = Arc::new(AtomicUsize::new(0));
        let callback = Arc::new(GatedCallback {
            entries: entries.clone(),
            response,
            entered: entered_tx,
            release: std::sync::Mutex::new(Some(release_rx)),
        });
        let handle = CallbackHandle {
            entries,
            entered: Arc::new(tokio::sync::Mutex::new(entered_rx)),
            release: release_tx,
        };
        let built = server
            .build_route(
                RouteBuildSpec {
                    name: "keyed".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: descriptor
                        .methods()
                        .iter()
                        .map(|method| method.name().to_string())
                        .collect(),
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                callback,
            )
            .unwrap();
        let identity = RouteCallIdentity {
            route_name: built.name().to_string(),
            crm_ns: built.crm_ns().to_string(),
            crm_name: built.crm_name().to_string(),
            crm_ver: built.crm_ver().to_string(),
            abi_hash: built.abi_hash().to_string(),
            signature_hash: built.signature_hash().to_string(),
        };
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        OutcomeHarness {
            server,
            peer: None,
            handler: tokio::spawn(std::future::pending()),
            identity,
            callback: handle,
            next_id: 1,
            capabilities,
        }
    }

    fn peer(&mut self) -> &mut LocalStream {
        self.peer.as_mut().expect("connect() before using the peer")
    }

    /// Attach the harness' primary submitter connection.
    async fn connect(&mut self) {
        let (mut peer, stream) = crate::test_support::isolated_pair().await;
        let handler = tokio::spawn(handle_connection(self.server.clone(), stream));
        super::tests::handshake_test_client_with(&mut peer, self.capabilities, "").await;
        self.peer = Some(peer);
        self.handler = handler;
    }

    /// A second, independent connection to the same server.
    async fn extra_conn(&self) -> LocalStream {
        let (mut peer, stream) = crate::test_support::isolated_pair().await;
        let _handler = tokio::spawn(handle_connection(self.server.clone(), stream));
        super::tests::handshake_test_client_with(&mut peer, self.capabilities, "").await;
        peer
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn keyed_open(&mut self, call_id: &str, method_idx: u16, input: &[u8]) -> u64 {
        let id = self.next_request_id();
        let identity = self.identity.clone();
        self.peer()
            .write_all(
                &rpc::encode_open_keyed_frame(
                    id,
                    &identity,
                    method_idx,
                    call_id,
                    ValuePart::whole(ValueStorage::Inline(input)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        id
    }

    async fn recover_on(
        peer: &mut LocalStream,
        identity: &RouteCallIdentity,
        id: u64,
        call_id: &str,
        method_idx: u16,
    ) {
        peer.write_all(&rpc::encode_open_recover_frame(id, identity, method_idx, call_id).unwrap())
            .await
            .unwrap();
    }

    async fn query_on(
        peer: &mut LocalStream,
        identity: &RouteCallIdentity,
        id: u64,
        call_id: &str,
    ) {
        peer.write_all(&rpc::encode_query_call_frame(id, identity, call_id).unwrap())
            .await
            .unwrap();
    }

    async fn stop(mut self) {
        self.callback.release_one();
        self.peer = None;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handler).await;
        self.server.request_shutdown_signal();
    }
}

async fn read_frame(peer: &mut LocalStream) -> (u64, u32, Vec<u8>) {
    super::tests::read_reply_frame(peer).await
}

/// Wait until the server's role domain holds `count` operation permits: the
/// original keyed call plus `count - 1` attached waiters each hold one slot,
/// so reaching the count proves the attach completed server-side.
async fn wait_used_operations(server: &Arc<Server>, count: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.domain_snapshot().operations.used_operations >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("operation permits reached the expected count");
}

async fn read_query_result(peer: &mut LocalStream) -> WireCallOutcome {
    let (rid, flags, payload) = read_frame(peer).await;
    let header = FrameHeader {
        total_len: 12 + payload.len() as u32,
        request_id: rid,
        flags,
    };
    match rpc::decode_rpc_record(&header, &payload, None).unwrap() {
        RpcRecord::QueryCallResult(WireQueryCallResult::Outcome(outcome)) => outcome,
        RpcRecord::QueryCallResult(WireQueryCallResult::Error(error)) => {
            panic!("unexpected query error: {:?}", error.code)
        }
        other => panic!("unexpected record: {other:?}"),
    }
}

#[derive(Debug)]
enum CallEnd {
    Success(Vec<u8>),
    Status(CallStatus, Option<ErrorCode>),
    Shm(BuddyPayload),
}

async fn read_call_end(peer: &mut LocalStream) -> (u64, CallEnd) {
    let (rid, flags, payload) = read_frame(peer).await;
    let header = FrameHeader {
        total_len: 12 + payload.len() as u32,
        request_id: rid,
        flags,
    };
    match rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap() {
        RpcRecord::CallEnd {
            status,
            value,
            error,
            ..
        } => {
            let end = match status {
                CallStatus::Success => match value.storage {
                    ValueStorage::Inline(bytes) => CallEnd::Success(bytes.to_vec()),
                    ValueStorage::Shm(descriptor) => CallEnd::Shm(descriptor),
                },
                other => CallEnd::Status(other, error.map(|error| error.code)),
            };
            (rid, end)
        }
        other => panic!("unexpected record: {other:?}"),
    }
}

/// Group 1: disconnect racing the unique `enter_resource` adjudication can
/// never leave an `InFlight` record whose resource never executed, and an
/// executing call outlives its original connection.
#[tokio::test]
async fn disconnect_race_never_leaves_inflight_without_execution() {
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits::default(),
        b"pong".to_vec(),
    )
    .await;
    harness.connect().await;

    // Pause after commit and input materialization. Read-side retirement must
    // win the CAS even though the blocking executor already owns both slots.
    let (at_cas, entered) = tokio::sync::oneshot::channel();
    let (resume, paused) = tokio::sync::oneshot::channel();
    *harness.server.keyed_entry_pause.lock() = Some((at_cas, paused));
    let conn = harness
        .server
        .active_connections
        .lock()
        .values()
        .next()
        .unwrap()
        .clone();
    let id = harness.keyed_open("race-early", 0, b"in").await;
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(harness.callback.executed(), 0);

    let mut observer = harness.extra_conn().await;
    let recover_id = harness.next_request_id();
    OutcomeHarness::recover_on(
        &mut observer,
        &harness.identity,
        recover_id,
        "race-early",
        0,
    )
    .await;
    wait_used_operations(&harness.server, 2).await;
    harness.peer = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while conn.calls.lock().get(id).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect retired original before entry CAS");
    resume.send(()).unwrap();
    let (rid, end) = read_call_end(&mut observer).await;
    assert_eq!(rid, recover_id);
    assert!(matches!(
        end,
        CallEnd::Status(CallStatus::NotExecuted, Some(ErrorCode::CallOutcomeUnknown))
    ));
    assert_eq!(
        query(&mut harness, &mut observer, "race-early")
            .await
            .status,
        WireCallOutcomeStatus::Unknown
    );
    assert_eq!(harness.callback.executed(), 0);

    // Phase B: the resource is executing (enter_resource won) when its
    // original connection dies. The record stays truthfully observable,
    // then settles `succeeded` after the callback returns.
    harness.connect().await;
    let _id3 = harness.keyed_open("race-late", 0, b"in").await;
    harness.callback.wait_entry().await;
    assert_eq!(harness.callback.executed(), 1);
    let mut probe = harness.extra_conn().await;
    let outcome = query(&mut harness, &mut probe, "race-late").await;
    assert_eq!(outcome.status, WireCallOutcomeStatus::InFlight);
    // Kill the original connection mid-execution.
    harness.peer = None;
    // Release the blocked callback.
    harness.callback.release_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let outcome = query(&mut harness, &mut probe, "race-late").await;
            if outcome.status != WireCallOutcomeStatus::InFlight {
                break outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached original settled");
    assert!(
        matches!(outcome.status, WireCallOutcomeStatus::Succeeded),
        "execution independent of the original connection must settle"
    );
    assert_eq!(harness.callback.executed(), 1);
    harness.stop().await;
}

async fn query(
    harness: &mut OutcomeHarness,
    peer: &mut LocalStream,
    call_id: &str,
) -> WireCallOutcome {
    let id = harness.next_request_id();
    let identity = harness.identity.clone();
    OutcomeHarness::query_on(peer, &identity, id, call_id).await;
    read_query_result(peer).await
}

/// Group 2: with `outcome_max_retained_bytes = 0` and a two-entry cap, a
/// recover attached to the `InFlight` call still receives the full result;
/// later observers get `722 not_retained`, and after the entry cap evicts
/// the record, `723`.
#[tokio::test]
async fn zero_retention_late_attach_receives_result_then_722_and_cap_723() {
    let config = ServerIpcConfig {
        outcome_max_entries: 2,
        outcome_max_retained_bytes: Some(0),
        ..ServerIpcConfig::default()
    };
    let mut harness =
        OutcomeHarness::start(config, DomainLimits::default(), b"pong-data".to_vec()).await;
    harness.connect().await;

    let _id = harness.keyed_open("late", 0, b"in").await;
    harness.callback.wait_entry().await;
    let mut observer = harness.extra_conn().await;
    let recover_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut observer, &harness.identity, recover_id, "late", 0).await;
    wait_used_operations(&harness.server, 2).await;

    // Release the callback: the attached recover must receive the FULL
    // result even though the store retains nothing.
    harness.callback.release_one();
    let (rid, end) = read_call_end(&mut observer).await;
    assert_eq!(rid, recover_id);
    match end {
        CallEnd::Success(bytes) => assert_eq!(bytes, b"pong-data"),
        other => panic!("attached recover must replay the full result: {other:?}"),
    }
    // The original submitter also receives its reply.
    let (_rid, end) = read_call_end(harness.peer()).await;
    assert!(matches!(end, CallEnd::Success(_)));

    // A later recover on the settled, not-retained record gets 722.
    let mut later = harness.extra_conn().await;
    let later_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut later, &harness.identity, later_id, "late", 0).await;
    let (rid, end) = read_call_end(&mut later).await;
    assert_eq!(rid, later_id);
    match end {
        CallEnd::Status(CallStatus::Failed, Some(code)) => {
            assert_eq!(code, ErrorCode::CallResultUnavailable);
        }
        other => panic!("expected 722, got {other:?}"),
    }
    let outcome = query(&mut harness, &mut later, "late").await;
    assert!(matches!(outcome.status, WireCallOutcomeStatus::Succeeded));
    assert!(!outcome.result_available);
    assert_eq!(
        outcome.result_unavailable_reason,
        Some(WireResultUnavailableReason::NotRetained)
    );

    // Settle two more keyed calls: the two-entry cap evicts "late"'s
    // metadata, so a final recover observes a miss → 723.
    for call_id in ["fill-1", "fill-2"] {
        let _ = harness.keyed_open(call_id, 0, b"in").await;
        harness.callback.wait_entry().await;
        harness.callback.release_one();
        let (_rid, end) = read_call_end(harness.peer()).await;
        assert!(matches!(end, CallEnd::Success(_)));
    }
    let mut final_probe = harness.extra_conn().await;
    let final_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut final_probe, &harness.identity, final_id, "late", 0).await;
    let (rid, end) = read_call_end(&mut final_probe).await;
    assert_eq!(rid, final_id);
    match end {
        CallEnd::Status(CallStatus::NotExecuted, Some(code)) => {
            assert_eq!(code, ErrorCode::CallOutcomeUnknown);
        }
        other => panic!("expected 723 after metadata eviction, got {other:?}"),
    }
    harness.stop().await;
}

/// Group 3: with `N_server = 1` and the single slot held by a blocked keyed
/// call, a query on the same connection still answers `in_flight`
/// immediately (bypassing the business slots); after release the original
/// delivers and the query reports `succeeded`.
#[tokio::test]
async fn n1_saturation_query_stays_nonblocking_on_the_same_connection() {
    let limits = DomainLimits {
        max_outstanding_calls: 1,
        ..DomainLimits::default()
    };
    let mut harness =
        OutcomeHarness::start(ServerIpcConfig::default(), limits, b"n1".to_vec()).await;
    harness.connect().await;

    let _id = harness.keyed_open("n1", 0, b"in").await;
    harness.callback.wait_entry().await;

    // The single N slot is held by the executing call. A second business
    // call on this connection would be refused; the query must answer
    // without blocking.
    let started = std::time::Instant::now();
    let query_id = harness.next_request_id();
    let query_identity = harness.identity.clone();
    OutcomeHarness::query_on(harness.peer(), &query_identity, query_id, "n1").await;
    let outcome = read_query_result(harness.peer()).await;
    assert!(
        matches!(outcome.status, WireCallOutcomeStatus::InFlight),
        "query must bypass the saturated business slots"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "query must not wait behind the blocked business call"
    );

    // Release: the original delivers and the query flips to succeeded.
    harness.callback.release_one();
    let (_rid, end) = read_call_end(harness.peer()).await;
    assert!(matches!(end, CallEnd::Success(_)));
    let query_id = harness.next_request_id();
    OutcomeHarness::query_on(harness.peer(), &query_identity, query_id, "n1").await;
    let outcome = read_query_result(harness.peer()).await;
    assert!(matches!(outcome.status, WireCallOutcomeStatus::Succeeded));
    assert!(outcome.result_available);
    harness.stop().await;
}

/// Group 4: conflict refusals, PendingInput revocation, route purge, and
/// the shared sealed-block multi-waiter delivery with per-connection
/// isolation.
#[tokio::test]
async fn conflicts_revocation_and_shared_shm_multi_waiter_delivery() {
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits::default(),
        vec![0x5a; 64 * 1024],
    )
    .await;
    harness.connect().await;

    // Same call_id with a different method_idx → 721 NotExecuted.
    let id_a = harness.keyed_open("conflict", 0, b"a").await;
    harness.callback.wait_entry().await;
    harness.callback.release_one();
    let (rid, end) = read_call_end(harness.peer()).await;
    assert_eq!(rid, id_a);
    assert!(matches!(end, CallEnd::Success(_)));
    let id_wrong_method = harness.next_request_id();
    let identity = harness.identity.clone();
    harness
        .peer()
        .write_all(
            &rpc::encode_open_keyed_frame(
                id_wrong_method,
                &identity,
                1,
                "conflict",
                ValuePart::whole(ValueStorage::Inline(b"a")),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (rid, end) = read_call_end(harness.peer()).await;
    assert_eq!(rid, id_wrong_method);
    match end {
        CallEnd::Status(CallStatus::NotExecuted, Some(code)) => {
            assert_eq!(code, ErrorCode::CallIdConflict)
        }
        other => panic!("expected 721 method conflict, got {other:?}"),
    }

    // Digest conflict on the settled record: different input → 721.
    let id_b = harness.keyed_open("conflict", 0, b"different-input").await;
    let (rid, end) = read_call_end(harness.peer()).await;
    assert_eq!(rid, id_b);
    match end {
        CallEnd::Status(CallStatus::NotExecuted, Some(code)) => {
            assert_eq!(code, ErrorCode::CallIdConflict)
        }
        other => panic!("expected 721 digest conflict, got {other:?}"),
    }

    // PendingInput revocation: a chunked keyed first frame that never
    // completes, a recover and a duplicate submit attached to it, then the
    // submitter's connection dies. The recover wakes with 723, the
    // duplicate submit with NotExecuted.
    let truncated_id = harness.next_request_id();
    let first = b"0123456789abcdef".to_vec();
    let identity = harness.identity.clone();
    harness
        .peer()
        .write_all(
            &rpc::encode_open_keyed_frame(
                truncated_id,
                &identity,
                0,
                "truncated",
                ValuePart::first(1024, ValueStorage::Inline(&first)),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let mut observer = harness.extra_conn().await;
    let recover_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut observer, &harness.identity, recover_id, "truncated", 0).await;
    let mut duplicate = harness.extra_conn().await;
    let duplicate_id = harness.next_request_id();
    duplicate
        .write_all(
            &rpc::encode_open_keyed_frame(
                duplicate_id,
                &harness.identity,
                0,
                "truncated",
                ValuePart::whole(ValueStorage::Inline(b"eventual-duplicate")),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    wait_used_operations(&harness.server, 3).await;
    // Kill the truncated submitter: the chunked input can never complete.
    harness.peer = None;
    let (rid, end) = read_call_end(&mut observer).await;
    assert_eq!(rid, recover_id);
    match end {
        CallEnd::Status(CallStatus::NotExecuted, Some(code)) => {
            assert_eq!(code, ErrorCode::CallOutcomeUnknown)
        }
        other => panic!("attached recover must wake with 723, got {other:?}"),
    }
    let (rid, end) = read_call_end(&mut duplicate).await;
    assert_eq!(rid, duplicate_id);
    assert!(
        matches!(end, CallEnd::Status(CallStatus::NotExecuted, _)),
        "waiting duplicate submit must wake NotExecuted"
    );
    drop(observer);
    drop(duplicate);

    // Route unregister drains and purges the outcome records: the query
    // then answers a route error instead of an outcome.
    harness.connect().await;
    let _kept = harness.keyed_open("purge-me", 0, b"in").await;
    harness.callback.wait_entry().await;
    harness.callback.release_one();
    let (_rid, end) = read_call_end(harness.peer()).await;
    assert!(matches!(end, CallEnd::Success(_)));
    assert!(harness.server.unregister_route("keyed").await);
    let mut probe = harness.extra_conn().await;
    let id_q = harness.next_request_id();
    OutcomeHarness::query_on(&mut probe, &harness.identity, id_q, "purge-me").await;
    let (rid, flags, payload) = read_frame(&mut probe).await;
    let header = FrameHeader {
        total_len: 12 + payload.len() as u32,
        request_id: rid,
        flags,
    };
    match rpc::decode_rpc_record(&header, &payload, None).unwrap() {
        RpcRecord::QueryCallResult(WireQueryCallResult::Error(error)) => {
            assert_eq!(error.code, ErrorCode::ResourceRemoved);
        }
        other => panic!("expected route-removed query error, got {other:?}"),
    }
    drop(probe);

    // Shared sealed SHM block: two waiters on two SHM-capable connections
    // each receive their own CALL_END loan of the same block; dropping one
    // connection before its peer reads leaves the other's delivery and the
    // published outcome intact.
    let mut shm_harness = OutcomeHarness::start_with_caps(
        ServerIpcConfig::default(),
        DomainLimits::default(),
        vec![0xa5; 64 * 1024],
        CAP_SHM_RESPONSE,
    )
    .await;
    shm_harness.connect().await;
    let _id = shm_harness.keyed_open("shared", 0, b"in").await;
    shm_harness.callback.wait_entry().await;
    let mut waiter_a = shm_harness.extra_conn().await;
    let mut waiter_b = shm_harness.extra_conn().await;
    let rid_a = shm_harness.next_request_id();
    OutcomeHarness::recover_on(&mut waiter_a, &shm_harness.identity, rid_a, "shared", 0).await;
    let rid_b = shm_harness.next_request_id();
    OutcomeHarness::recover_on(&mut waiter_b, &shm_harness.identity, rid_b, "shared", 0).await;
    wait_used_operations(&shm_harness.server, 3).await;
    shm_harness.callback.release_one();

    let (rid, end) = read_call_end(&mut waiter_a).await;
    assert_eq!(rid, rid_a);
    let CallEnd::Shm(first) = end else {
        panic!("waiter A requires a real SHM loan, got {end:?}");
    };
    // The physical peer end of A is sufficient to retire its loan, while B
    // retains its independent loan of the very same immutable allocation.
    let conn_a = shm_harness
        .server
        .active_connections
        .lock()
        .values()
        .find(|conn| conn.core.is_loan_live(first.loan_id))
        .unwrap()
        .clone();
    drop(waiter_a);

    let (rid, end) = read_call_end(&mut waiter_b).await;
    assert_eq!(rid, rid_b);
    let CallEnd::Shm(second) = end else {
        panic!("waiter B requires a real SHM loan, got {end:?}");
    };
    assert_eq!(
        (
            first.seg_idx,
            first.generation,
            first.offset,
            first.data_size
        ),
        (
            second.seg_idx,
            second.generation,
            second.offset,
            second.data_size
        ),
    );
    assert_ne!(first.loan_id, second.loan_id);
    let conn_b = shm_harness
        .server
        .active_connections
        .lock()
        .values()
        .find(|conn| conn.core.is_loan_live(second.loan_id))
        .unwrap()
        .clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while conn_a.core.is_loan_live(first.loan_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("read-side peer end releases only the first loan");
    assert!(conn_b.core.is_loan_live(second.loan_id));

    let release = c2_wire::release::ReleaseBatch::new(&[second.loan_id]).unwrap();
    waiter_b
        .write_all(&c2_wire::release::encode_release_frame(&release, false))
        .await
        .unwrap();
    assert_eq!(
        query(&mut shm_harness, &mut waiter_b, "shared")
            .await
            .status,
        WireCallOutcomeStatus::Succeeded
    );
    assert!(!conn_b.core.is_loan_live(second.loan_id));
    let mut probe = shm_harness.extra_conn().await;
    let outcome = query(&mut shm_harness, &mut probe, "shared").await;
    assert!(matches!(outcome.status, WireCallOutcomeStatus::Succeeded));
    drop(waiter_b);
    drop(probe);
    shm_harness.stop().await;
    harness.stop().await;
}

#[tokio::test]
async fn panic_is_a_failed_outcome_for_original_attached_and_recovery() {
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits::default(),
        b"unreachable".to_vec(),
    )
    .await;
    harness.connect().await;
    let original_id = harness.keyed_open("panic", 0, b"panic").await;
    harness.callback.wait_entry().await;

    let mut observer = harness.extra_conn().await;
    let attached_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut observer, &harness.identity, attached_id, "panic", 0).await;
    wait_used_operations(&harness.server, 2).await;
    harness.callback.release_one();

    for (expected_id, peer) in [(original_id, harness.peer()), (attached_id, &mut observer)] {
        let (id, end) = read_call_end(peer).await;
        assert_eq!(id, expected_id);
        assert!(matches!(
            end,
            CallEnd::Status(
                CallStatus::Failed,
                Some(ErrorCode::ResourceFunctionExecuting)
            )
        ));
    }
    let snapshot = query(&mut harness, &mut observer, "panic").await;
    assert_eq!(snapshot.status, WireCallOutcomeStatus::Failed);
    assert_eq!(
        snapshot.error_code,
        Some(ErrorCode::ResourceFunctionExecuting)
    );
    let recovered_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut observer, &harness.identity, recovered_id, "panic", 0).await;
    let (id, end) = read_call_end(&mut observer).await;
    assert_eq!(id, recovered_id);
    assert!(matches!(
        end,
        CallEnd::Status(
            CallStatus::Failed,
            Some(ErrorCode::ResourceFunctionExecuting)
        )
    ));
    assert_eq!(harness.callback.executed(), 1);
    harness.stop().await;
}

#[tokio::test]
async fn unregister_waits_for_detached_execution_and_revokes_partial_input() {
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits::default(),
        b"retained".to_vec(),
    )
    .await;
    harness.connect().await;
    let route = harness.server.dispatcher.read().resolve("keyed").unwrap();
    let route_uid: Arc<str> = Arc::from(route.route_uid.as_str());
    let _ = harness.keyed_open("executing", 0, b"in").await;
    harness.callback.wait_entry().await;
    harness.peer = None;

    let mut partial = harness.extra_conn().await;
    let partial_id = harness.next_request_id();
    partial
        .write_all(
            &rpc::encode_open_keyed_frame(
                partial_id,
                &harness.identity,
                0,
                "partial",
                ValuePart::first(32, ValueStorage::Inline(b"never completed")),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        query(&mut harness, &mut partial, "partial").await.status,
        WireCallOutcomeStatus::InFlight
    );

    let mut observer = harness.extra_conn().await;
    let observer_id = harness.next_request_id();
    OutcomeHarness::recover_on(
        &mut observer,
        &harness.identity,
        observer_id,
        "executing",
        0,
    )
    .await;
    assert_eq!(
        query(&mut harness, &mut observer, "executing").await.status,
        WireCallOutcomeStatus::InFlight,
    );
    let server = harness.server.clone();
    let unregister = tokio::spawn(async move { server.unregister_route("keyed").await });

    let (id, end) = read_call_end(&mut partial).await;
    assert_eq!(id, partial_id);
    assert!(matches!(
        end,
        CallEnd::Status(CallStatus::NotExecuted, Some(ErrorCode::ResourceClosed))
    ));
    assert!(
        !unregister.is_finished(),
        "unregister waits for the executing call"
    );

    // New attachments must be closed while the prior execution is draining.
    let rejected_id = harness.next_request_id();
    OutcomeHarness::recover_on(&mut partial, &harness.identity, rejected_id, "executing", 0).await;
    let (id, end) = read_call_end(&mut partial).await;
    assert_eq!(id, rejected_id);
    assert!(matches!(
        end,
        CallEnd::Status(CallStatus::NotExecuted, Some(ErrorCode::ResourceClosed))
    ));

    harness.callback.release_one();
    let (id, end) = read_call_end(&mut observer).await;
    assert_eq!(id, observer_id);
    assert!(matches!(end, CallEnd::Success(bytes) if bytes == b"retained"));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), unregister)
            .await
            .unwrap()
            .unwrap()
    );
    for name in ["partial", "executing"] {
        assert_eq!(
            harness
                .server
                .outcome_store
                .query(&route_uid, name, |_| Some("method".into()))
                .status,
            WireCallOutcomeStatus::Unknown,
        );
    }
    assert_eq!(harness.callback.executed(), 1);
    harness.stop().await;
}

#[tokio::test]
async fn query_overload_and_contract_mismatch_are_control_errors() {
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits {
            max_outstanding_calls: 1,
            ..DomainLimits::default()
        },
        b"result".to_vec(),
    )
    .await;
    harness.connect().await;
    let occupied = harness
        .server
        .query_replies
        .clone()
        .try_acquire_owned()
        .unwrap();
    let mut peer = harness.extra_conn().await;
    let rid = harness.next_request_id();
    OutcomeHarness::query_on(&mut peer, &harness.identity, rid, "any").await;
    assert_query_error(&mut peer, rid, ErrorCode::CallCapacityExceeded).await;
    drop(occupied);

    let mut wrong_identity = harness.identity.clone();
    wrong_identity.crm_ns.push('x');
    let rid = harness.next_request_id();
    OutcomeHarness::query_on(&mut peer, &wrong_identity, rid, "any").await;
    assert_query_error(&mut peer, rid, ErrorCode::ContractMismatch).await;

    assert_eq!(
        query(&mut harness, &mut peer, "any").await.status,
        WireCallOutcomeStatus::Unknown
    );
    let id = harness.next_request_id();
    peer.write_all(
        &rpc::encode_open_keyed_frame(
            id,
            &harness.identity,
            0,
            "still-open",
            ValuePart::whole(ValueStorage::Inline(b"in")),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    harness.callback.wait_entry().await;
    harness.callback.release_one();
    assert!(
        matches!(read_call_end(&mut peer).await, (_, CallEnd::Success(bytes)) if bytes == b"result")
    );
    harness.stop().await;
}

async fn assert_query_error(peer: &mut LocalStream, expected_id: u64, code: ErrorCode) {
    let (request_id, flags, payload) = read_frame(peer).await;
    assert_eq!(request_id, expected_id);
    let header = FrameHeader {
        total_len: 12 + payload.len() as u32,
        request_id,
        flags,
    };
    match rpc::decode_rpc_record(&header, &payload, None).unwrap() {
        RpcRecord::QueryCallResult(WireQueryCallResult::Error(error)) => {
            assert_eq!(error.code, code)
        }
        record => panic!("expected query control error {code:?}, got {record:?}"),
    }
}

#[tokio::test]
async fn reassembly_retries_after_memory_shortfall_without_shedding_for_permanent_size() {
    const MEMORY: u64 = 256 * 1024;
    const PART: usize = 32 * 1024;
    const RETAINED: usize = 8192;
    let mut harness = OutcomeHarness::start(
        ServerIpcConfig::default(),
        DomainLimits {
            memory_budget_bytes: MEMORY,
            file_spill_budget_bytes: 0,
            ..DomainLimits::default()
        },
        vec![0x42; RETAINED],
    )
    .await;
    harness.connect().await;
    let mut observer = harness.extra_conn().await;
    for call_id in ["oldest", "second"] {
        let id = harness.keyed_open(call_id, 0, b"seed").await;
        harness.callback.wait_entry().await;
        harness.callback.release_one();
        assert!(matches!(read_call_end(harness.peer()).await,
            (rid, CallEnd::Success(bytes)) if rid == id && bytes.len() == RETAINED));
    }
    assert!(
        query(&mut harness, &mut observer, "oldest")
            .await
            .result_available
    );
    assert!(
        query(&mut harness, &mut observer, "second")
            .await
            .result_available
    );

    let part = vec![0x37; PART];
    let identity = harness.identity.clone();
    // The mapped span fits M alone, but its chunk metadata makes the
    // complete allocation impossible at this budget.
    let impossible = harness.next_request_id();
    harness
        .peer()
        .write_all(
            &rpc::encode_open_keyed_frame(
                impossible,
                &identity,
                0,
                "cannot-fit",
                ValuePart::first(MEMORY, ValueStorage::Inline(&part)),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(read_call_end(harness.peer()).await,
        (id, CallEnd::Status(CallStatus::NotExecuted, Some(ErrorCode::ResourceUnavailable)))
            if id == impossible));
    assert_eq!(harness.callback.executed(), 2);
    assert!(
        query(&mut harness, &mut observer, "oldest")
            .await
            .result_available
    );
    assert!(
        query(&mut harness, &mut observer, "second")
            .await
            .result_available
    );

    let budget = &harness.server.domain_budget;
    let before = budget.snapshot().memory;
    let frame_charge = (PART + super::PENDING_FRAME_OVERHEAD) as u64;
    let backing_charge = 2 * PART as u64 + 2;
    let available = frame_charge + backing_charge - RETAINED as u64 / 2;
    let hold = budget
        .try_reserve_memory(MEMORY - before.used_bytes - available)
        .unwrap();
    let request_id = harness.next_request_id();
    harness
        .peer()
        .write_all(
            &rpc::encode_open_keyed_frame(
                request_id,
                &identity,
                0,
                "fits-after-shedding",
                ValuePart::first(2 * PART as u64, ValueStorage::Inline(&part)),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while harness.server.chunk_registry.active_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reassembly published after one pressure retry");
    let after = harness.server.domain_snapshot().memory;
    assert_eq!(after.rejected_allocations - before.rejected_allocations, 1);
    assert_eq!(
        query(&mut harness, &mut observer, "oldest")
            .await
            .result_unavailable_reason,
        Some(WireResultUnavailableReason::Evicted),
    );
    assert!(
        query(&mut harness, &mut observer, "second")
            .await
            .result_available
    );
    drop(hold);
    harness
        .peer()
        .write_all(
            &rpc::encode_value_continuation_frame(
                request_id,
                rpc::RpcRecordKind::Open,
                ValueStorage::Inline(&part),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    harness.callback.wait_entry().await;
    harness.callback.release_one();
    assert!(matches!(read_call_end(harness.peer()).await,
        (rid, CallEnd::Success(bytes)) if rid == request_id && bytes.len() == RETAINED));
    assert_eq!(harness.callback.executed(), 3);
    harness.stop().await;
}
