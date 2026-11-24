use super::*;
use crate::dispatcher::{CrmStream, CrmStreamItem};
use crate::hook::{AdmissionContext, AdmissionDecision, AdmissionTransport, current_call_context};
use crate::scheduler::{AccessLevel, ConcurrencyMode};
use crate::topic::{TopicConfig, TopicPublisher};
use c2_contract::{ExpectedRouteContract, ValidatedContractDescriptor};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use tokio::time::{Instant, timeout};

const WAIT: Duration = Duration::from_secs(5);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

struct TestExtension(Arc<AtomicUsize>);

impl Drop for TestExtension {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ResourceEvent {
    phase: &'static str,
    target: String,
    operation: AdmissionOperation,
    call_id: Option<String>,
    metadata: Option<String>,
    has_extension: bool,
}

struct Callback(Arc<CallMutex<Vec<ResourceEvent>>>);

impl Callback {
    fn observe(&self, phase: &'static str) -> bool {
        let context = current_call_context().expect("bound resource call context");
        assert_eq!(context.route_name(), "test");
        self.0.lock().push(ResourceEvent {
            phase,
            target: context.target().to_owned(),
            operation: context.operation(),
            call_id: context.call_id().map(str::to_owned),
            metadata: context.metadata().get("decision").map(str::to_owned),
            has_extension: context.extension::<TestExtension>().is_some(),
        });
        context.metadata().get("decision") == Some("callback-denies")
    }
}

fn nested_denial() -> CrmError {
    CrmError::UserError(
        C2Error::new(ErrorCode::AdmissionDenied, "nested denial")
            .with_details(
                [
                    ("transport_phase".into(), "pre_dispatch".into()),
                    ("dispatch_phase".into(), "pre_dispatch".into()),
                ]
                .into(),
            )
            .to_wire_bytes(),
    )
}

impl CrmCallback for Callback {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        request: Vec<u8>,
        _: Arc<crate::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if self.observe("invoke") {
            Err(nested_denial())
        } else {
            Ok(ResponseMeta::Inline(request))
        }
    }
}

impl CrmStreamCallback for Callback {
    fn open_stream(
        &self,
        _: &str,
        _: u16,
        request: Vec<u8>,
        _: Arc<crate::ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        self.observe("open");
        Ok(Box::new(TestStream(
            Callback(self.0.clone()),
            Some(request),
        )))
    }
}

struct TestStream(Callback, Option<Vec<u8>>);

impl CrmStream for TestStream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        if self.0.observe("step") {
            Err(nested_denial())
        } else {
            Ok(self
                .1
                .take()
                .map(|bytes| Box::new(bytes) as Box<dyn CrmStreamItem>))
        }
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        self.0.observe("finish");
        Ok(())
    }
}

struct Harness {
    server: Arc<Server>,
    peer: c2_local::LocalStream,
    conn: Arc<Connection>,
    identity: RouteCallIdentity,
    publisher: TopicPublisher,
    events: Arc<CallMutex<Vec<ResourceEvent>>>,
}

impl Harness {
    async fn new(hook: Option<Arc<dyn AdmissionHook>>, concurrency: ConcurrencyMode) -> Self {
        Self::with_limits(hook, concurrency, DomainLimits::default(), false).await
    }

    async fn with_limits(
        hook: Option<Arc<dyn AdmissionHook>>,
        concurrency: ConcurrencyMode,
        limits: DomainLimits,
        run: bool,
    ) -> Self {
        let id = format!("admission-{}", uuid::Uuid::new_v4().simple());
        let endpoint = parse_local_endpoint(&format!("ipc://{id}")).unwrap();
        let mut server = Server::new_with_identity_endpoint_and_budget(
            endpoint,
            ServerIpcConfig::default(),
            ServerIdentity {
                server_id: id,
                server_instance_id: uuid::Uuid::new_v4().simple().to_string(),
            },
            DomainBudget::from_limits(&limits),
        )
        .unwrap();
        if let Some(hook) = hook {
            server = server.with_admission_hook(hook);
        }
        let server = Arc::new(server);
        let events = Arc::new(CallMutex::new(Vec::new()));
        let (identity, publisher) = register(&server, events.clone(), concurrency).await;
        if run {
            let server = server.clone();
            tokio::spawn(async move { server.run().await.unwrap() });
        }
        if run {
            server.wait_until_ready(WAIT).await.unwrap();
        }
        let (mut peer, stream) = crate::test_support::isolated_pair().await;
        // Keep the production connection driver and the production shutdown
        // transaction, while using an isolated native pair for scripted frames.
        tokio::spawn(handle_connection(server.clone(), stream));
        let ack = super::tests::handshake_test_client_with(&mut peer, 0, "").await;
        assert_eq!(
            ack.capability_flags & CAP_ADMISSION_HOOK != 0,
            server.has_admission_hook()
        );
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        Self {
            server,
            peer,
            conn,
            identity,
            publisher,
            events,
        }
    }

    async fn send(&mut self, frame: Vec<u8>) {
        timeout(WAIT, self.peer.write_all(&frame))
            .await
            .unwrap()
            .unwrap();
    }

    async fn ping(&mut self) {
        self.send(encode_frame(0, FLAG_SIGNAL, &[MsgType::Ping.as_byte()]))
            .await;
        assert_eq!(
            super::tests::read_reply_frame(&mut self.peer).await.2,
            PONG_BYTES
        );
    }

    async fn read(&mut self, mode: CallMode) -> (u64, RpcRecord<'static>) {
        let (id, flags, bytes) = super::tests::read_reply_frame(&mut self.peer).await;
        let header = frame::FrameHeader {
            request_id: id,
            flags,
            total_len: 12 + bytes.len() as u32,
        };
        // The tests retain only response status, control metadata and item
        // framing here. Tests that check bytes decode the original frame.
        let record = rpc::decode_rpc_record(&header, &bytes, Some(mode)).unwrap();
        let owned = match record {
            RpcRecord::CallEnd {
                status,
                complete_items,
                error,
                ..
            } => RpcRecord::CallEnd {
                status,
                complete_items,
                error,
                value: ValuePart::whole(ValueStorage::Inline(&[])),
            },
            RpcRecord::ItemData { .. } => RpcRecord::ItemData {
                value: ValuePart::whole(ValueStorage::Inline(&[])),
            },
            RpcRecord::ItemBegin { total_bytes, seq } => RpcRecord::ItemBegin { total_bytes, seq },
            RpcRecord::Ready {
                start_seq,
                head_seq,
            } => RpcRecord::Ready {
                start_seq,
                head_seq,
            },
            RpcRecord::QueryCallResult(result) => RpcRecord::QueryCallResult(result),
            RpcRecord::AdmitTopicResult(result) => RpcRecord::AdmitTopicResult(result),
            other => panic!("unexpected reply {other:?}"),
        };
        (id, owned)
    }

    async fn expect_error(&mut self, id: u64, mode: CallMode, code: ErrorCode) -> C2Error {
        let (actual, record) = self.read(mode).await;
        assert_eq!(actual, id);
        let error = match record {
            RpcRecord::CallEnd {
                status,
                error: Some(error),
                ..
            } => {
                let expected = if code == ErrorCode::ResourceFunctionExecuting
                    || code == ErrorCode::CallCancelled && status == CallStatus::Failed
                {
                    CallStatus::Failed
                } else {
                    CallStatus::NotExecuted
                };
                assert_eq!(status, expected);
                error
            }
            RpcRecord::QueryCallResult(rpc::WireQueryCallResult::Error(error))
            | RpcRecord::AdmitTopicResult(rpc::WireAdmitTopicResult::Error(error)) => error,
            other => panic!("expected refusal for {id}, got {other:?}"),
        };
        assert_eq!(error.code, code, "{error:?}");
        error
    }

    async fn success(&mut self, id: u64, mode: CallMode) {
        let (received, record) = self.read(mode).await;
        assert_eq!(received, id);
        assert!(
            matches!(
                record,
                RpcRecord::CallEnd {
                    status: CallStatus::Success,
                    ..
                }
            ),
            "{record:?}"
        );
    }

    async fn subscribe(&mut self, id: u64, metadata: &CallMetadata) {
        self.send(subscription(id, &self.identity, metadata)).await;
        assert_eq!(
            self.read(CallMode::Subscribe).await,
            (
                id,
                RpcRecord::Ready {
                    start_seq: self.publisher.head_seq() + 1,
                    head_seq: self.publisher.head_seq(),
                }
            )
        );
    }

    fn pending(&self, id: u64) -> bool {
        self.conn
            .calls
            .lock()
            .get(id)
            .is_some_and(|call| call.data.pending_admission.lock().is_some())
    }
}

async fn register(
    server: &Arc<Server>,
    events: Arc<CallMutex<Vec<ResourceEvent>>>,
    concurrency_mode: ConcurrencyMode,
) -> (RouteCallIdentity, TopicPublisher) {
    let contract = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
    let expected = ExpectedRouteContract {
        route_name: "test".to_owned(),
        crm_ns: contract.crm_namespace().to_owned(),
        crm_name: contract.crm_name().to_owned(),
        crm_ver: contract.crm_version().to_owned(),
        abi_hash: contract.abi_hash().to_owned(),
        signature_hash: contract.signature_hash().to_owned(),
    };
    let topics = server.create_route_topics(
        expected.clone(),
        &[("latest".into(), TopicConfig::default())].into(),
    );
    let publisher = topics.publisher("latest").unwrap();
    let spec = RouteBuildSpec {
        name: expected.route_name.clone(),
        crm_ns: expected.crm_ns.clone(),
        crm_name: expected.crm_name.clone(),
        crm_ver: expected.crm_ver.clone(),
        abi_hash: expected.abi_hash.clone(),
        signature_hash: expected.signature_hash.clone(),
        method_names: vec!["ping".into(), "echo".into()],
        access_map: [(0, AccessLevel::Read), (1, AccessLevel::Write)].into(),
        concurrency_mode,
        limits: SchedulerLimits::default(),
    };
    let route = server
        .build_stream_route(spec, Arc::new(Callback(events)), &contract)
        .unwrap()
        .with_topics(topics);
    let reserved = server.reserve_route(route).await.unwrap();
    server.commit_reserved_route(reserved).await.unwrap();
    (
        RouteCallIdentity {
            route_name: expected.route_name,
            crm_ns: expected.crm_ns,
            crm_name: expected.crm_name,
            crm_ver: expected.crm_ver,
            abi_hash: expected.abi_hash,
            signature_hash: expected.signature_hash,
        },
        publisher,
    )
}

fn metadata(decision: &str) -> CallMetadata {
    CallMetadata::from_entries([("decision", decision)]).unwrap()
}

fn unary(id: u64, route: &RouteCallIdentity, metadata: &CallMetadata, input: &[u8]) -> Vec<u8> {
    rpc::encode_open_frame(
        id,
        route,
        0,
        CallMode::Unary,
        metadata,
        ValuePart::whole(ValueStorage::Inline(input)),
    )
    .unwrap()
}

fn keyed(id: u64, route: &RouteCallIdentity, metadata: &CallMetadata, call_id: &str) -> Vec<u8> {
    rpc::encode_open_keyed_frame(
        id,
        route,
        0,
        call_id,
        metadata,
        ValuePart::whole(ValueStorage::Inline(b"echo")),
    )
    .unwrap()
}

fn subscription(id: u64, route: &RouteCallIdentity, metadata: &CallMetadata) -> Vec<u8> {
    rpc::encode_open_subscribe_frame(
        id,
        route,
        "latest",
        rpc::WireSubscribeOptions {
            start: rpc::WireTopicStart::Next,
            ..Default::default()
        },
        metadata,
    )
    .unwrap()
}

fn check_admission_context(ctx: &AdmissionContext<'_>) {
    assert_eq!(ctx.route_name(), "test");
    assert_eq!(ctx.crm_ns(), "test.contract-release");
    assert_eq!(ctx.crm_name(), "Portable");
    assert_eq!(ctx.crm_ver(), "0.1.0");
    assert_eq!(ctx.transport(), AdmissionTransport::Ipc);
    assert!(current_call_context().is_none());
    let (target, access) = match ctx.operation() {
        AdmissionOperation::Unary | AdmissionOperation::Recover => {
            (Some("ping"), Some(AccessLevel::Read))
        }
        AdmissionOperation::ServerStream => (Some("echo"), Some(AccessLevel::Write)),
        AdmissionOperation::Subscribe => (Some("latest"), None),
        AdmissionOperation::QueryOutcome => (None, None),
    };
    assert_eq!(ctx.target(), target);
    assert_eq!(ctx.access(), access);
}

fn cancel(id: u64) -> Vec<u8> {
    rpc::encode_cancel_frame(id, rpc::CancelCause::Consumer).unwrap()
}

async fn until(check: impl Fn() -> bool) {
    timeout(WAIT, async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("state reached within timeout");
}

#[tokio::test]
async fn all_operations_present_contract_context_and_fail_closed() {
    for mode in [ConcurrencyMode::Exclusive, ConcurrencyMode::Parallel] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let extension = dropped.clone();
        let hook = move |ctx: &AdmissionContext<'_>| {
            check_admission_context(ctx);
            match ctx.metadata().get("decision") {
                Some("deny") => AdmissionDecision::deny_with_details(
                    "denied",
                    BTreeMap::from([
                        ("transport".into(), "fake".into()),
                        ("transport_phase".into(), "fake".into()),
                        ("dispatch_phase".into(), "fake".into()),
                        ("stage".into(), "fake".into()),
                        ("call_id".into(), "fake".into()),
                        ("reason".into(), "policy".into()),
                    ]),
                ),
                Some("panic") => panic!("hook panicked"),
                _ => AdmissionDecision::allow_with_extension(Arc::new(TestExtension(
                    extension.clone(),
                ))),
            }
        };
        let mut harness = Harness::new(Some(Arc::new(hook)), mode).await;
        let route = harness.identity.clone();
        assert_eq!(harness.publisher.publish_bytes(b"historical").unwrap(), 1);
        harness.subscribe(1, &metadata("allow")).await;
        until(|| dropped.load(Ordering::Acquire) == 1).await;
        harness
            .send(keyed(2, &route, &metadata("allow"), "original"))
            .await;
        harness.success(2, CallMode::KeyedUnary).await;
        until(|| dropped.load(Ordering::Acquire) == 2).await;
        let mut id = 2;
        for (batch, decision) in ["allow", "deny", "panic"].into_iter().enumerate() {
            let metadata = &metadata(decision);
            for (index, mode) in [
                CallMode::Unary,
                CallMode::KeyedUnary,
                CallMode::ServerStream,
                CallMode::Recover,
                CallMode::Subscribe,
                CallMode::Unary,
                CallMode::Subscribe,
            ]
            .into_iter()
            .enumerate()
            {
                id += 1;
                let frame = match index {
                    0 => unary(id, &route, metadata, b"input"),
                    1 => keyed(id, &route, metadata, "original"),
                    2 => rpc::encode_open_frame(
                        id,
                        &route,
                        1,
                        mode,
                        metadata,
                        ValuePart::whole(ValueStorage::Inline(b"item")),
                    )
                    .unwrap(),
                    3 => {
                        rpc::encode_open_recover_frame(id, &route, 0, "original", metadata).unwrap()
                    }
                    4 => subscription(id, &route, metadata),
                    5 => rpc::encode_query_call_frame(id, &route, "original", metadata).unwrap(),
                    6 => rpc::encode_admit_topic_frame(id, 1, metadata).unwrap(),
                    _ => unreachable!(),
                };
                harness.send(frame).await;
                if batch > 0 {
                    let error = harness
                        .expect_error(id, mode, ErrorCode::AdmissionDenied)
                        .await;
                    assert_eq!(
                        error.details,
                        BTreeMap::from([(
                            "reason".into(),
                            if batch == 1 {
                                "policy".into()
                            } else {
                                "hook_error".into()
                            }
                        ),])
                    );
                    assert_eq!(
                        error.message,
                        if batch == 1 {
                            "denied"
                        } else {
                            "admission hook failed"
                        }
                    );
                    continue;
                }
                match index {
                    0 | 1 | 3 => harness.success(id, mode).await,
                    2 => {
                        assert_eq!(
                            harness.read(mode).await,
                            (
                                id,
                                RpcRecord::ItemBegin {
                                    total_bytes: 4,
                                    seq: None
                                }
                            )
                        );
                        harness
                            .send(rpc::encode_credit_frame(id, 4, 0).unwrap())
                            .await;
                        assert!(matches!(
                            harness.read(mode).await,
                            (_, RpcRecord::ItemData { .. })
                        ));
                        harness
                            .send(rpc::encode_credit_frame(id, 0, 1).unwrap())
                            .await;
                        harness.success(id, mode).await;
                    }
                    4 => {
                        assert_eq!(
                            harness.read(mode).await,
                            (
                                id,
                                RpcRecord::Ready {
                                    start_seq: 2,
                                    head_seq: 1
                                }
                            )
                        );
                        harness.send(cancel(id)).await;
                        harness
                            .expect_error(id, mode, ErrorCode::CallCancelled)
                            .await;
                    }
                    5 => {
                        let (received, reply) = harness.read(mode).await;
                        assert_eq!(received, id);
                        let RpcRecord::QueryCallResult(rpc::WireQueryCallResult::Outcome(outcome)) =
                            reply
                        else {
                            panic!("expected outcome, got {reply:?}");
                        };
                        assert_eq!(outcome.status, rpc::WireCallOutcomeStatus::Succeeded);
                    }
                    6 => assert_eq!(
                        harness.read(mode).await,
                        (
                            id,
                            RpcRecord::AdmitTopicResult(rpc::WireAdmitTopicResult::Allowed {
                                head_seq: 1
                            },)
                        )
                    ),
                    _ => unreachable!(),
                }
                until(|| dropped.load(Ordering::Acquire) == index + 3).await;
            }
        }
        let events = harness.events.lock();
        assert_eq!(
            events.iter().map(|event| event.phase).collect::<Vec<_>>(),
            ["invoke", "invoke", "open", "step", "step", "finish",]
        );
        assert_eq!(events[0].call_id.as_deref(), Some("original"));
        assert_eq!(events[1].call_id, None);
        assert!(
            events[2..].iter().all(|event| event.target == "echo"
                && event.operation == AdmissionOperation::ServerStream)
        );
        assert!(
            events
                .iter()
                .all(|event| event.has_extension && event.metadata.as_deref() == Some("allow"))
        );
    }
}

#[tokio::test]
async fn no_hook_binds_context_and_isolates_nested_denials_from_all_callback_paths() {
    let mut harness = Harness::new(None, ConcurrencyMode::Exclusive).await;
    let route = harness.identity.clone();
    let metadata = &metadata("callback-denies");
    let stream = rpc::encode_open_frame(
        3,
        &route,
        1,
        CallMode::ServerStream,
        metadata,
        ValuePart::whole(ValueStorage::Inline(b"bytes")),
    )
    .unwrap();
    for (id, mode, frame) in [
        (1, CallMode::Unary, unary(1, &route, metadata, b"input")),
        (
            2,
            CallMode::KeyedUnary,
            keyed(2, &route, metadata, "nested"),
        ),
        (3, CallMode::ServerStream, stream),
    ] {
        harness.send(frame).await;
        let error = harness
            .expect_error(id, mode, ErrorCode::ResourceFunctionExecuting)
            .await;
        assert_eq!(
            error.details.get("cause_code").map(String::as_str),
            Some("724")
        );
        assert_eq!(
            error.details.get("cause_message").map(String::as_str),
            Some("nested denial")
        );
        assert!(!error.details.contains_key("transport_phase"));
        assert!(!error.details.contains_key("dispatch_phase"));
    }
    {
        let events = harness.events.lock();
        assert_eq!(
            events.iter().map(|event| event.phase).collect::<Vec<_>>(),
            ["invoke", "invoke", "open", "step", "finish",]
        );
        assert!(
            events.iter().all(|event| !event.has_extension
                && event.metadata.as_deref() == Some("callback-denies"))
        );
    }
    harness.subscribe(4, metadata).await;
    harness
        .send(rpc::encode_admit_topic_frame(5, 4, metadata).unwrap())
        .await;
    assert_eq!(
        harness.read(CallMode::Subscribe).await,
        (
            5,
            RpcRecord::AdmitTopicResult(rpc::WireAdmitTopicResult::Allowed { head_seq: 0 },)
        )
    );
}

#[tokio::test]
async fn semantic_metadata_rejections_keep_the_connection_and_skip_the_hook() {
    for with_hook in [false, true] {
        let entered = Arc::new(AtomicUsize::new(0));
        let count = entered.clone();
        let hook = move |_: &AdmissionContext<'_>| {
            count.fetch_add(1, Ordering::Release);
            AdmissionDecision::allow()
        };
        let mut harness = Harness::new(
            with_hook.then(|| Arc::new(hook) as Arc<dyn AdmissionHook>),
            ConcurrencyMode::Exclusive,
        )
        .await;
        let route = harness.identity.clone();
        let metadata = &metadata("allow");
        for (id, mode, mut frame) in [
            (1, CallMode::Unary, unary(1, &route, metadata, b"input")),
            (
                2,
                CallMode::Unary,
                rpc::encode_query_call_frame(2, &route, "any", metadata).unwrap(),
            ),
            (
                3,
                CallMode::Subscribe,
                rpc::encode_admit_topic_frame(3, 9, metadata).unwrap(),
            ),
        ] {
            let offset = frame
                .windows(b"decision".len())
                .position(|w| w == b"decision")
                .unwrap();
            frame[offset] = b'D';
            harness.send(frame).await;
            let error = harness
                .expect_error(id, mode, ErrorCode::ProtocolViolation)
                .await;
            assert_eq!(
                error.details.get("stage").map(String::as_str),
                Some("call_metadata_validation")
            );
        }
        assert_eq!(entered.load(Ordering::Acquire), 0);
        harness.send(unary(4, &route, metadata, b"later")).await;
        harness.success(4, CallMode::Unary).await;
        assert_eq!(entered.load(Ordering::Acquire), usize::from(with_hook));
    }
}

struct Gate {
    observed: tokio::sync::mpsc::UnboundedReceiver<AdmissionOperation>,
    release: watch::Sender<bool>,
}

impl Gate {
    fn hook(denied: bool) -> (Arc<dyn AdmissionHook>, Self) {
        let (entered, observed) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = watch::channel(false);
        let hook = move |ctx: &AdmissionContext<'_>| {
            if ctx.metadata().get("decision") != Some("wait") {
                return AdmissionDecision::allow();
            }
            let _ = entered.send(ctx.operation());
            let mut released = released.clone();
            let _ = futures_executor::block_on(released.wait_for(|open| *open));
            if denied {
                AdmissionDecision::deny("gate denied")
            } else {
                AdmissionDecision::allow()
            }
        };
        (Arc::new(hook), Self { observed, release })
    }

    async fn entered(&mut self, operation: AdmissionOperation) {
        assert_eq!(
            timeout(WAIT, self.observed.recv()).await.unwrap(),
            Some(operation)
        );
    }

    fn open(&self) {
        self.release.send_replace(true);
    }
}

#[tokio::test]
async fn fifo_commits_key_before_recover_and_retires_queued_stream_and_subscription() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let route = harness.identity.clone();
    let wait = &metadata("wait");
    let allow = &metadata("allow");
    harness
        .send(
            rpc::encode_open_keyed_frame(
                1,
                &route,
                0,
                "fifo",
                wait,
                ValuePart::first(4, ValueStorage::Inline(b"ec")),
            )
            .unwrap(),
        )
        .await;
    gate.entered(AdmissionOperation::Unary).await;

    let stream = rpc::encode_open_frame(
        2,
        &route,
        1,
        CallMode::ServerStream,
        wait,
        ValuePart::whole(ValueStorage::Inline(b"item")),
    )
    .unwrap();
    harness.send(stream).await;
    harness
        .send(rpc::encode_open_recover_frame(3, &route, 0, "fifo", wait).unwrap())
        .await;
    harness.send(subscription(4, &route, wait)).await;
    gate.entered(AdmissionOperation::Subscribe).await;
    harness.ping().await;
    assert_eq!(
        harness.server.domain_snapshot().operations.used_operations,
        4
    );
    assert!(harness.pending(2));
    assert!(harness.pending(3));
    assert!(
        gate.observed.try_recv().is_err(),
        "Recover ran before the submitted key was admitted"
    );
    harness.send(cancel(2)).await;
    harness.send(cancel(4)).await;
    harness
        .expect_error(2, CallMode::ServerStream, ErrorCode::CallCancelled)
        .await;
    harness
        .expect_error(4, CallMode::Subscribe, ErrorCode::CallCancelled)
        .await;
    until(|| harness.server.domain_snapshot().operations.used_operations == 2).await;
    assert!(!harness.pending(2));
    assert!(!harness.pending(4));
    gate.open();
    gate.entered(AdmissionOperation::Recover).await;
    // Recover reached its hook before the keyed request's last fragment.
    // Its store attachment can now wait independently for the original.
    harness
        .send(continuation(1, ValueStorage::Inline(b"ho")))
        .await;
    let mut completed = Vec::new();
    for _ in 0..2 {
        let (id, reply) = harness.read(CallMode::Unary).await;
        assert!(matches!(
            reply,
            RpcRecord::CallEnd {
                status: CallStatus::Success,
                ..
            }
        ));
        completed.push(id);
    }
    completed.sort_unstable();
    assert_eq!(completed, [1, 3]);
    assert!(
        gate.observed.try_recv().is_err(),
        "cancelled stream reached its hook"
    );
    harness.send(unary(5, &route, allow, b"later")).await;
    harness.success(5, CallMode::Unary).await;
}

#[tokio::test]
async fn control_capacity_is_nonblocking_and_pins_the_registration() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let route = harness.identity.clone();
    let wait = &metadata("wait");
    let allow = &metadata("allow");
    harness.subscribe(1, allow).await;
    let parallelism = harness.server.admission_permits.available_permits();
    let mut peers = Vec::new();
    for _ in 0..parallelism {
        let (mut peer, stream) = crate::test_support::isolated_pair().await;
        tokio::spawn(handle_connection(harness.server.clone(), stream));
        super::tests::handshake_test_client(&mut peer).await;
        peer.write_all(&unary(1, &route, wait, b"busy"))
            .await
            .unwrap();
        peers.push(peer);
    }
    for _ in 0..parallelism {
        gate.entered(AdmissionOperation::Unary).await;
    }
    harness
        .send(rpc::encode_query_call_frame(2, &route, "any", wait).unwrap())
        .await;
    harness
        .expect_error(2, CallMode::Unary, ErrorCode::CallCapacityExceeded)
        .await;
    harness
        .send(rpc::encode_admit_topic_frame(3, 1, wait).unwrap())
        .await;
    harness
        .expect_error(3, CallMode::Subscribe, ErrorCode::CallCapacityExceeded)
        .await;
    assert!(gate.observed.try_recv().is_err());
    harness.send(subscription(4, &route, wait)).await;
    harness.ping().await;
    assert!(harness.pending(4));
    harness.send(cancel(4)).await;
    harness
        .expect_error(4, CallMode::Subscribe, ErrorCode::CallCancelled)
        .await;
    assert!(!harness.pending(4));
    assert!(gate.observed.try_recv().is_err());
    drop(peers);
    gate.open();
    until(|| harness.server.admission_permits.available_permits() == parallelism).await;

    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Exclusive).await;
    let route = harness.identity.clone();
    harness.subscribe(1, allow).await;
    harness
        .send(rpc::encode_query_call_frame(2, &route, "any", wait).unwrap())
        .await;
    harness
        .send(rpc::encode_admit_topic_frame(3, 1, wait).unwrap())
        .await;
    let entered = [
        timeout(WAIT, gate.observed.recv()).await.unwrap().unwrap(),
        timeout(WAIT, gate.observed.recv()).await.unwrap().unwrap(),
    ];
    assert!(entered.contains(&AdmissionOperation::Subscribe));
    assert!(entered.contains(&AdmissionOperation::QueryOutcome));
    assert!(
        timeout(WAIT, harness.server.unregister_route(&route.route_name))
            .await
            .unwrap()
    );
    let (reregistered, _) = register(
        &harness.server,
        harness.events.clone(),
        ConcurrencyMode::Exclusive,
    )
    .await;
    assert_eq!(reregistered, route);
    harness
        .expect_error(1, CallMode::Subscribe, ErrorCode::CallCancelled)
        .await;
    gate.open();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (id, reply) = harness.read(CallMode::Unary).await;
        let (RpcRecord::QueryCallResult(rpc::WireQueryCallResult::Error(error))
        | RpcRecord::AdmitTopicResult(rpc::WireAdmitTopicResult::Error(error))) = reply
        else {
            panic!("expected registration closed, got {reply:?}")
        };
        assert_eq!(error.code, ErrorCode::ResourceClosed);
        ids.push(id);
    }
    ids.sort_unstable();
    assert_eq!(ids, [2, 3]);
}

#[tokio::test]
async fn shutdown_waits_for_running_hook_and_terminal_reply() {
    let (hook, mut gate) = Gate::hook(true);
    let mut harness = Harness::with_limits(
        Some(hook),
        ConcurrencyMode::Parallel,
        DomainLimits::default(),
        true,
    )
    .await;
    let route = harness.identity.clone();
    harness
        .send(unary(1, &route, &metadata("wait"), b"slow"))
        .await;
    gate.entered(AdmissionOperation::Unary).await;
    let server = harness.server.clone();
    let shutdown = tokio::spawn(async move { server.shutdown_and_wait_unbounded().await.unwrap() });
    harness.server.wait_for_shutdown_requested().await;
    assert!(!shutdown.is_finished());
    gate.open();
    harness
        .expect_error(1, CallMode::Unary, ErrorCode::AdmissionDenied)
        .await;
    timeout(WAIT, shutdown).await.unwrap().unwrap();
    assert_eq!(
        harness.server.lifecycle_state(),
        ServerLifecycleState::Stopped
    );
}

#[tokio::test]
async fn disconnect_retires_pending_operation_but_shutdown_still_waits_for_hook() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::with_limits(
        Some(hook),
        ConcurrencyMode::Parallel,
        DomainLimits::default(),
        true,
    )
    .await;
    let route = harness.identity.clone();
    harness
        .send(unary(1, &route, &metadata("wait"), b"slow"))
        .await;
    gate.entered(AdmissionOperation::Unary).await;
    let server = harness.server.clone();
    drop(harness);
    until(|| {
        server.active_connection_count() == 0
            && server.domain_snapshot().operations.used_operations == 0
    })
    .await;
    let shutdown_server = server.clone();
    let shutdown =
        tokio::spawn(async move { shutdown_server.shutdown_and_wait_unbounded().await.unwrap() });
    server.wait_for_shutdown_requested().await;
    assert!(!shutdown.is_finished());
    gate.open();
    timeout(WAIT, shutdown).await.unwrap().unwrap();
}

fn first(id: u64, route: &RouteCallIdentity, metadata: &CallMetadata) -> Vec<u8> {
    rpc::encode_open_frame(
        id,
        route,
        0,
        CallMode::Unary,
        metadata,
        ValuePart::first(4, ValueStorage::Inline(b"ab")),
    )
    .unwrap()
}

fn continuation(id: u64, storage: ValueStorage<'_>) -> Vec<u8> {
    rpc::encode_value_continuation_frame(id, RpcRecordKind::Open, storage).unwrap()
}

#[tokio::test]
async fn gc_expires_incomplete_staging_only_and_complete_chunks_copy_in_order() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let route = harness.identity.clone();
    let wait = &metadata("wait");
    harness.send(first(1, &route, wait)).await;
    gate.entered(AdmissionOperation::Unary).await;
    harness
        .send(continuation(1, ValueStorage::Inline(b"cd")))
        .await;
    harness.send(first(2, &route, wait)).await;
    harness.ping().await;
    assert!(harness.pending(1) && harness.pending(2));
    assert!(harness.server.copy_scheduler.is_empty());
    let writer = harness
        .conn
        .rpc_writer
        .lock()
        .as_ref()
        .unwrap()
        .upgrade()
        .unwrap();
    expire_pending_admissions(
        &harness.conn,
        &writer,
        Instant::now() + harness.server.chunk_registry.config().assembler_timeout * 2,
    );
    harness
        .expect_error(2, CallMode::Unary, ErrorCode::ResourceUnavailable)
        .await;
    assert!(harness.pending(1));
    assert!(!harness.pending(2));
    gate.open();

    let (id, flags, payload) = super::tests::read_reply_frame(&mut harness.peer).await;
    let header = frame::FrameHeader {
        request_id: id,
        flags,
        total_len: 12 + payload.len() as u32,
    };
    assert_eq!(id, 1);
    let reply = rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap();
    assert!(
        matches!(
            reply,
            RpcRecord::CallEnd {
                status: CallStatus::Success,
                value: ValuePart {
                    storage: ValueStorage::Inline(b"abcd"),
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    assert!(
        gate.observed.try_recv().is_err(),
        "expired record reached its hook"
    );
}

#[tokio::test]
async fn pending_first_and_continuation_reservations_never_evict_keyed_result() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let route = harness.identity.clone();
    let allow = &metadata("allow");
    let wait = &metadata("wait");
    harness.send(keyed(1, &route, allow, "retained")).await;
    harness.success(1, CallMode::KeyedUnary).await;

    // Leave capacity for precisely one staged chunk and one byte less than
    // the next chunk needs. Evicting the retained four-byte result would
    // make either later reservation succeed.
    let memory = harness.server.domain_snapshot().memory;
    let charge = PENDING_FRAME_OVERHEAD as u64 + 2;
    let _occupied = harness
        .server
        .domain_budget
        .try_reserve_memory(memory.limit_bytes - memory.used_bytes - (charge * 2 - 1))
        .unwrap();
    harness.send(first(2, &route, wait)).await;
    gate.entered(AdmissionOperation::Unary).await;
    harness.send(first(3, &route, wait)).await;
    harness
        .expect_error(3, CallMode::Unary, ErrorCode::ResourceUnavailable)
        .await;
    harness
        .send(continuation(2, ValueStorage::Inline(b"cd")))
        .await;
    harness
        .expect_error(2, CallMode::Unary, ErrorCode::ResourceUnavailable)
        .await;
    assert!(harness.server.copy_scheduler.is_empty());
    assert!(!harness.pending(2) && !harness.pending(3));
    harness
        .send(rpc::encode_query_call_frame(4, &route, "retained", allow).unwrap())
        .await;
    let (id, reply) = harness.read(CallMode::Unary).await;
    assert_eq!(id, 4);
    let RpcRecord::QueryCallResult(rpc::WireQueryCallResult::Outcome(outcome)) = reply else {
        panic!("expected retained result, got {reply:?}");
    };
    assert!(outcome.result_available);
    gate.open();
}

#[tokio::test]
async fn denied_and_invalid_shm_frames_release_loan_without_closing_connection() {
    use c2_wire::connection::{ConnectionCore, ConnectionSide, WriteRequest};
    let (hook, mut gate) = Gate::hook(true);
    let harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let owner = Arc::new(RwLock::new(
        MemPool::new_with_prefix_and_budget(
            c2_mem::config::PoolConfig::primary_for_budget(64 * 1024 * 1024),
            format!(
                "/hook{:08x}{:08x}",
                std::process::id(),
                RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32
            ),
            DomainBudget::from_limits(&DomainLimits::default()),
        )
        .unwrap(),
    ));
    let (mut peer, stream) = crate::test_support::isolated_pair().await;
    tokio::spawn(handle_connection(harness.server.clone(), stream));
    let prefix = owner.read().prefix().to_owned();
    let ack = super::tests::handshake_test_client_with(&mut peer, CAP_SHM_REQUEST, &prefix).await;
    assert_ne!(ack.capability_flags & CAP_SHM_REQUEST, 0);
    let peer_core = ConnectionCore::new(ConnectionSide::Client, || {});
    peer_core.finish_handshake().unwrap();
    let loan = || {
        let handle = owner.write().alloc_shm_handle(2).unwrap();
        let allocation = handle.shm_allocation().unwrap();
        let owner = peer_core.prepare_loan(owner.clone(), handle).unwrap();
        let loan_id = owner.id();
        let mut write = peer_core
            .begin_write(WriteRequest::Business(vec![owner]))
            .unwrap()
            .unwrap();
        write.begin_io().unwrap();
        write.frame_completed();
        write.flush_completed();
        BuddyPayload {
            seg_idx: allocation.seg_idx as u16,
            generation: allocation.generation,
            offset: allocation.offset,
            data_size: 2,
            is_dedicated: allocation.is_dedicated,
            loan_id,
        }
    };
    async fn expect_release(peer: &mut c2_local::LocalStream, loan: BuddyPayload) {
        let (id, flags, payload) = super::tests::read_reply_frame(peer).await;
        let header = frame::FrameHeader {
            request_id: id,
            flags,
            total_len: 12 + payload.len() as u32,
        };
        let RpcRecord::Release(batch) = rpc::decode_rpc_record(&header, &payload, None).unwrap()
        else {
            panic!("expected loan release");
        };
        assert_eq!(batch.ids(), &[loan.loan_id]);
    }
    let route = &harness.identity;
    let shm_unary = |id, bytes, meta: &CallMetadata| {
        rpc::encode_open_frame(
            id,
            route,
            0,
            CallMode::Unary,
            meta,
            ValuePart::whole(ValueStorage::Shm(bytes)),
        )
        .unwrap()
    };

    let wait = &metadata("wait");
    peer.write_all(&first(1, route, wait)).await.unwrap();
    gate.entered(AdmissionOperation::Unary).await;
    gate.open();
    let (id, flags, payload) = super::tests::read_reply_frame(&mut peer).await;
    let header = frame::FrameHeader {
        request_id: id,
        flags,
        total_len: 12 + payload.len() as u32,
    };
    assert!(
        matches!(rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap(),
        RpcRecord::CallEnd { error: Some(error), .. } if error.code == ErrorCode::AdmissionDenied)
    );
    let late = loan();
    peer.write_all(&continuation(1, ValueStorage::Shm(late)))
        .await
        .unwrap();
    expect_release(&mut peer, late).await;

    let invalid = loan();
    let mut frame = shm_unary(2, invalid, wait);
    let key = frame
        .windows(b"decision".len())
        .position(|w| w == b"decision")
        .unwrap();
    frame[key] = b'D';
    peer.write_all(&frame).await.unwrap();
    // The writer may batch a queued RELEASE immediately before or after the
    // refusal, depending on whether it woke before CALL_END was queued.
    let mut seen_release = false;
    let mut seen_rejection = false;
    for _ in 0..2 {
        let (id, flags, payload) = super::tests::read_reply_frame(&mut peer).await;
        let header = frame::FrameHeader {
            request_id: id,
            flags,
            total_len: 12 + payload.len() as u32,
        };
        match rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap() {
            RpcRecord::Release(batch) => {
                assert_eq!(batch.ids(), &[invalid.loan_id]);
                seen_release = true;
            }
            RpcRecord::CallEnd {
                status: CallStatus::NotExecuted,
                error: Some(error),
                ..
            } => {
                assert_eq!(id, 2);
                assert_eq!(error.code, ErrorCode::ProtocolViolation);
                assert_eq!(
                    error.details.get("stage").map(String::as_str),
                    Some("call_metadata_validation")
                );
                seen_rejection = true;
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }
    assert!(seen_release && seen_rejection);
    peer.write_all(&unary(3, route, &metadata("allow"), b"later"))
        .await
        .unwrap();
    let (id, flags, payload) = super::tests::read_reply_frame(&mut peer).await;
    let header = frame::FrameHeader {
        request_id: id,
        flags,
        total_len: 12 + payload.len() as u32,
    };
    assert_eq!(id, 3);
    assert!(matches!(
        rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap(),
        RpcRecord::CallEnd {
            status: CallStatus::Success,
            ..
        }
    ));
}

#[tokio::test]
async fn connection_deactivation_preserves_pending_unary_in_its_admission_generation() {
    let (hook, mut gate) = Gate::hook(false);
    let mut harness = Harness::new(Some(hook), ConcurrencyMode::Parallel).await;
    let route = harness.identity.clone();
    harness
        .send(unary(1, &route, &metadata("wait"), b"accepted"))
        .await;
    gate.entered(AdmissionOperation::Unary).await;
    harness
        .conn
        .end(ConnectionEndReason::LocalClose, "test_deactivation");
    assert!(harness.conn.is_deactivated());
    gate.open();
    harness.success(1, CallMode::Unary).await;
    assert_eq!(harness.events.lock().len(), 1);
}
