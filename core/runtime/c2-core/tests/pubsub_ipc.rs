//! End-to-end IPC 17 direct cross-process/connection publish/subscribe tests
//! (ADR 0005 Batch 4).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use c2_config::{CallOptions, CallTimeout};
use c2_contract::{
    ContractRelease, MethodAccess, derive_contract_fingerprints_json,
};
use c2_core::{
    Connect, EncodedClient, EncodedService, EncodedStream, EncodedStreamItem,
    EncodedStreamService, Error, Host, HostOptions, LagPolicy, LocalEndpointOptions,
    MethodDefinition, Runtime, RuntimeOptions, ServiceDefinition, SubscribeOptions, TopicStart,
    TransportPhase,
};
use c2_error::{C2Error, ErrorCode};
use c2_local::{DEFAULT_CONNECT_TIMEOUT, LocalStream};
use c2_wire::buddy::BuddyPayload;
use c2_wire::flags::{FLAG_HANDSHAKE, FLAG_RESPONSE, FLAG_SIGNAL};
use c2_wire::frame::{self, FrameHeader};
use c2_wire::handshake::{
    CAP_SHM_RESPONSE, HANDSHAKE_VERSION, VersionPreambleKind, decode_handshake,
    decode_version_preamble_payload, encode_client_handshake, encode_version_preamble,
    validate_version_preamble_header,
};
use c2_wire::release::{ReleaseBatch, encode_release_frame};
use c2_wire::rpc::{self, CallMode, CallStatus, RouteCallIdentity, RpcRecord, ValueStorage};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BASE_DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const STREAM_DESCRIPTOR: &str =
    include_str!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn unique_server_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

struct EchoService;

impl EncodedService for EchoService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(b"pong".to_vec()),
            1 => Ok(request),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected method index {other}"),
            )),
        }
    }
}

struct MixedStreamService;

struct BytesItem(Vec<u8>);

impl EncodedStreamItem for BytesItem {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        destination.copy_from_slice(&self.0[offset..offset + destination.len()]);
        Ok(())
    }
}

struct FixedStream {
    items: std::collections::VecDeque<Vec<u8>>,
}

impl EncodedStream for FixedStream {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        Ok(self
            .items
            .pop_front()
            .map(|item| Box::new(BytesItem(item)) as Box<dyn EncodedStreamItem>))
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Ok(())
    }
}

impl EncodedService for MixedStreamService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(request),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected unary method index {other}"),
            )),
        }
    }
}

impl EncodedStreamService for MixedStreamService {
    fn open_stream(
        &self,
        method_index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn EncodedStream>, C2Error> {
        match method_index {
            1 => Ok(Box::new(FixedStream {
                items: std::collections::VecDeque::from([
                    b"stream-1".to_vec(),
                    request,
                    b"stream-3".to_vec(),
                ]),
            })),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected stream method index {other}"),
            )),
        }
    }
}

fn release_with_topics(topic_names: &[&str]) -> ContractRelease {
    let mut value: Value = serde_json::from_str(BASE_DESCRIPTOR).unwrap();
    let sample_binding = value["methods"][1]["bindings"]["output"].clone();
    if !topic_names.is_empty() {
        value["topics"] = Value::Array(
            topic_names
                .iter()
                .map(|name| {
                    json!({
                        "name": name,
                        "output": {"kind": "payload"},
                        "binding": sample_binding.clone(),
                    })
                })
                .collect(),
        );
    }
    let fp = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(fp.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] = Value::String(fp.signature_hash().to_string());
    ContractRelease::from_descriptor_json(value.to_string().as_bytes()).unwrap()
}

fn stream_release_with_topics(topic_names: &[&str]) -> ContractRelease {
    let mut value: Value = serde_json::from_str(STREAM_DESCRIPTOR).unwrap();
    let sample_binding = value["methods"][1]["bindings"]["output"].clone();
    value["topics"] = Value::Array(
        topic_names
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "output": {"kind": "payload"},
                    "binding": sample_binding.clone(),
                })
            })
            .collect(),
    );
    let fp = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(fp.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] = Value::String(fp.signature_hash().to_string());
    ContractRelease::from_descriptor_json(value.to_string().as_bytes()).unwrap()
}

fn topic_only_release(topic_names: &[&str]) -> ContractRelease {
    let base: Value = serde_json::from_str(BASE_DESCRIPTOR).unwrap();
    let sample_binding = base["methods"][1]["bindings"]["output"].clone();
    let mut value = base.clone();
    value["methods"] = Value::Array(Vec::new());
    value["topics"] = Value::Array(
        topic_names
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "output": {"kind": "payload"},
                    "binding": sample_binding.clone(),
                })
            })
            .collect(),
    );
    let fp = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(fp.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] = Value::String(fp.signature_hash().to_string());
    ContractRelease::from_descriptor_json(value.to_string().as_bytes()).unwrap()
}

fn echo_def(release: &ContractRelease, route_name: &str) -> ServiceDefinition {
    ServiceDefinition::new(
        release,
        release.reference(),
        route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".into(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".into(),
                access: MethodAccess::Write,
            },
        ],
        Arc::new(EchoService),
    )
    .unwrap()
}

fn make_runtime(dir: &tempfile::TempDir, client_max_outstanding: Option<u64>) -> Runtime {
    let mut opts = RuntimeOptions::default();
    if let Some(calls) = client_max_outstanding {
        opts.client_limits.max_outstanding_calls = Some(calls);
    }
    let runtime = Runtime::new(opts).unwrap();
    #[cfg(unix)]
    runtime
        .set_local_endpoint(LocalEndpointOptions {
            unix_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
    runtime
}

fn make_host(
    dir: &tempfile::TempDir,
    prefix: &str,
    memory_budget: Option<u64>,
    max_outstanding: Option<u64>,
) -> (Runtime, Host) {
    let runtime = make_runtime(dir, None);
    let mut opts = HostOptions::default()
        .with_server_id(unique_server_id(prefix))
        .without_relay()
        .with_shutdown_timeout(Duration::from_secs(5));
    if let Some(bytes) = memory_budget {
        opts = opts.with_memory_budget_bytes(bytes);
    }
    if let Some(calls) = max_outstanding {
        opts = opts.with_max_outstanding_calls(calls);
    }
    let host = runtime.host(opts).unwrap();
    (runtime, host)
}

fn assert_semantic_code(err: Error, expected: ErrorCode) -> C2Error {
    match err {
        Error::Semantic(semantic) => {
            assert_eq!(semantic.code, expected, "unexpected error: {semantic:?}");
            semantic
        }
        other => panic!("expected semantic {expected:?}, got {other:?}"),
    }
}

#[test]
fn ipc16_peer_is_rejected_with_protocol_version_mismatch_by_ipc17_host() {
    assert_eq!(HANDSHAKE_VERSION, 17);
    let dir = tempfile::tempdir().unwrap();
    let (_rt, host) = make_host(&dir, "ipc16-reject", None, None);
    let endpoint = host.local_endpoint().clone();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut stream = LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT)
            .await
            .unwrap();
        let mut hello = encode_version_preamble(VersionPreambleKind::Hello);
        hello[21..25].copy_from_slice(&16u32.to_le_bytes());
        stream.write_all(&hello).await.unwrap();
        stream.flush().await.unwrap();

        let mut header = [0u8; 16];
        stream.read_exact(&mut header).await.unwrap();
        validate_version_preamble_header(&header, true).unwrap();
        let mut payload = [0u8; 9];
        stream.read_exact(&mut payload).await.unwrap();
        let (kind, version) = decode_version_preamble_payload(&payload).unwrap();
        assert_eq!(kind, VersionPreambleKind::VersionMismatch);
        assert_eq!(version, 17);
    });
    let _ = host.shutdown();
}

#[test]
fn remote_subscribe_start_latest_on_empty_and_populated_topic_and_start_next() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-start", None, None);
    let release = release_with_topics(&["state"]);
    let reg = host
        .register(echo_def(&release, "sim").with_topic_capacity("state", 4).unwrap())
        .unwrap();
    let pub_state = reg.publisher("state").unwrap();

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();

    // Empty topic confirms attachment immediately with READY(1).
    let sub_empty = client
        .subscribe(
            "state",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(sub_empty.start_seq(), 1);
    assert_eq!(sub_empty.topic_name(), "state");

    // Publish empty (0-byte) and non-empty inline frames.
    assert_eq!(pub_state.publish(Vec::new()).unwrap(), 1);
    assert_eq!(pub_state.publish(b"frame-2".to_vec()).unwrap(), 2);

    let d1 = sub_empty.next_delivery().unwrap();
    assert_eq!(d1.seq(), 1);
    assert_eq!(d1.dropped(), 0);
    assert!(d1.bytes().is_empty());

    let d2 = sub_empty.next_delivery().unwrap();
    assert_eq!(d2.seq(), 2);
    assert_eq!(d2.dropped(), 0);
    assert_eq!(d2.bytes(), b"frame-2");

    // Late joiner with start=Latest gets READY(2) and receives frame 2.
    let sub_latest = client
        .subscribe(
            "state",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(sub_latest.start_seq(), 2);
    let dl = sub_latest.next_delivery().unwrap();
    assert_eq!(dl.seq(), 2);
    assert_eq!(dl.dropped(), 0);
    assert_eq!(dl.bytes(), b"frame-2");

    // Late joiner with start=Next gets READY(3) and waits for seq 3.
    let sub_next = client
        .subscribe(
            "state",
            SubscribeOptions {
                start: TopicStart::Next,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(sub_next.start_seq(), 3);
    assert_eq!(pub_state.publish(b"frame-3".to_vec()).unwrap(), 3);
    let dn = sub_next.next_delivery().unwrap();
    assert_eq!(dn.seq(), 3);
    assert_eq!(dn.dropped(), 0);
    assert_eq!(dn.bytes(), b"frame-3");

    let r1 = sub_empty.close();
    assert!(r1.completed);
    let r2 = sub_latest.close();
    assert!(r2.completed);
    let r3 = sub_next.close();
    assert!(r3.completed);
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn multi_subscriber_single_write_shared_shm_buddy_and_dedicated() {
    let dir = tempfile::tempdir().unwrap();
    // 16 MiB server memory budget yields a 1 MiB buddy segment so a 64 KiB
    // payload takes buddy SHM and a 5 MiB payload takes dedicated SHM.
    let (_host_rt, host) = make_host(&dir, "ipc-shm-fanout", Some(16 * 1024 * 1024), None);
    let release = release_with_topics(&["field"]);
    let reg = host
        .register(echo_def(&release, "sim").with_topic_capacity("field", 1).unwrap())
        .unwrap();
    let pub_field = reg.publisher("field").unwrap();

    // Two independent client runtimes -> two distinct physical IPC connections.
    let client_rt_a = make_runtime(&dir, None);
    let client_rt_b = make_runtime(&dir, None);
    let addr = host.local_endpoint().address().to_string();
    let client_a = client_rt_a
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: addr.clone(),
            },
        )
        .unwrap();
    let client_b = client_rt_b
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc { address: addr },
        )
        .unwrap();

    let sub_a = client_a
        .subscribe("field", SubscribeOptions::default())
        .unwrap();
    let sub_b = client_b
        .subscribe("field", SubscribeOptions::default())
        .unwrap();

    // 1. Buddy SHM payload (64 KiB > SHM_THRESHOLD).
    let buddy_payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
    assert_eq!(pub_field.publish(buddy_payload.clone()).unwrap(), 1);
    let da = sub_a.next_delivery().unwrap();
    let db = sub_b.next_delivery().unwrap();
    assert_eq!(da.seq(), 1);
    assert_eq!(db.seq(), 1);
    assert_eq!(da.bytes(), buddy_payload.as_slice());
    assert_eq!(db.bytes(), buddy_payload.as_slice());

    // 2. Large payload (5 MiB > buddy segment size on 16 MiB budget -> dedicated SHM).
    // Even with two concurrent remote subscribers, the Host stores only ONE
    // sealed SHM block (< 16 MiB, whereas 2× dedicated + copies would exceed budget
    // or double server memory).
    let large_payload: Vec<u8> = (0..(5 * 1024 * 1024)).map(|i| ((i * 7) % 253) as u8).collect();
    assert_eq!(pub_field.publish(large_payload.clone()).unwrap(), 2);

    // Receive from both subscribers independently; both get exact bytes.
    let da2 = sub_a.next_delivery().unwrap();
    let db2 = sub_b.next_delivery().unwrap();
    assert_eq!(da2.seq(), 2);
    assert_eq!(db2.seq(), 2);
    assert_eq!(da2.bytes().len(), large_payload.len());
    assert_eq!(db2.bytes().len(), large_payload.len());
    assert_eq!(da2.bytes(), large_payload.as_slice());
    assert_eq!(db2.bytes(), large_payload.as_slice());

    let a_ptr = da2.bytes().as_ptr();
    let b_ptr = db2.bytes().as_ptr();
    drop((da2, db2));
    let next_payload: Vec<u8> = large_payload.iter().map(|byte| byte ^ 0xff).collect();
    assert_eq!(pub_field.publish(next_payload.clone()).unwrap(), 3);
    let da3 = sub_a.next_delivery().unwrap();
    let db3 = sub_b.next_delivery().unwrap();
    assert_eq!(da3.bytes().as_ptr(), a_ptr);
    assert_eq!(db3.bytes().as_ptr(), b_ptr);
    assert_eq!(da3.bytes(), next_payload.as_slice());
    assert_eq!(db3.bytes(), next_payload.as_slice());

    // Publishing a small inline frame evicts the dedicated SHM block.
    assert_eq!(pub_field.publish(b"tiny".to_vec()).unwrap(), 4);
    assert_eq!(sub_a.next_delivery().unwrap().bytes(), b"tiny");
    assert_eq!(sub_b.next_delivery().unwrap().bytes(), b"tiny");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let used = host
            .memory_stats()
            .server
            .memory
            .map(|s| s.used_bytes)
            .unwrap_or(0);
        if used < 5 * 1024 * 1024 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "evicted dedicated SHM block was not reclaimed after both peers released: used={used}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(sub_a.close().completed);
    assert!(sub_b.close().completed);
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn lag_policy_skip_and_error_exact_boundaries_over_ipc17() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-lag", None, None);
    let release = release_with_topics(&["burst"]);
    // Ring depth k = 2.
    let reg = host
        .register(echo_def(&release, "sim").with_topic_capacity("burst", 2).unwrap())
        .unwrap();
    let pub_burst = reg.publisher("burst").unwrap();

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();

    // Start two subscribers at seq 1 (on empty topic), then consume seq 1 so
    // the client capacity-1 channel + in-flight demand lets us fill the ring
    // deterministically while both subscribers hold seq 1 unread or after seq 1.
    // Even simpler: publish 1, attach with start=Latest (start_seq=1), read seq 1,
    // wait until server has sent seq 2 into client's capacity-1 channel, then
    // burst seq 3, 4, 5, 6 so next_expected = 3 is evicted from ring [5, 6]!
    let sub_skip = client
        .subscribe(
            "burst",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    let sub_err = client
        .subscribe(
            "burst",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Error,
            },
        )
        .unwrap();

    // Publish seq 1 and receive on both so both are synchronized at next_expected = 2.
    assert_eq!(pub_burst.publish(b"v1".to_vec()).unwrap(), 1);
    assert_eq!(sub_skip.next_delivery().unwrap().seq(), 1);
    assert_eq!(sub_err.next_delivery().unwrap().seq(), 1);

    // Publish seq 2 -> both client receivers fill their capacity-1 channel with
    // seq 2 and immediately grant demand=1 for seq 3; then seq 3 fills the next
    // in-flight slot; wait briefly so both receivers buffer seq 2 without us
    // calling next_delivery(), then publish 3, 4, 5, 6, 7 so intermediate seqs
    // are guaranteed evicted from the capacity=2 ring [6, 7].
    assert_eq!(pub_burst.publish(b"v2".to_vec()).unwrap(), 2);
    std::thread::sleep(Duration::from_millis(50));
    for seq in 3..=7u64 {
        assert_eq!(pub_burst.publish(format!("v{seq}").into_bytes()).unwrap(), seq);
    }

    // Drain buffered items from sub_skip until we observe the skipped jump to seq 7.
    let mut saw_skip = false;
    let mut last_seq = 1u64;
    for _ in 0..4 {
        let d = sub_skip.next_delivery().unwrap();
        assert!(d.seq() > last_seq);
        if d.dropped() > 0 {
            assert_eq!(d.seq() - (last_seq + 1), d.dropped());
            assert_eq!(d.seq(), 7);
            assert_eq!(d.bytes(), b"v7");
            saw_skip = true;
            break;
        }
        last_seq = d.seq();
    }
    assert!(saw_skip, "skip subscriber must report dropped > 0 and jump to latest");

    // Drain any already-buffered items from sub_err until it terminates with 720.
    let mut lag_err = None;
    for _ in 0..4 {
        match sub_err.next_delivery() {
            Ok(d) => {
                assert_eq!(d.dropped(), 0);
            }
            Err(err) => {
                lag_err = Some(assert_semantic_code(err, ErrorCode::SubscriptionLagged));
                break;
            }
        }
    }
    let lag_err = lag_err.expect("on_lag=error subscriber must terminate with 720");
    assert_eq!(lag_err.details.get("topic").map(String::as_str), Some("burst"));
    assert!(lag_err.details.contains_key("expected_seq"));
    assert!(lag_err.details.contains_key("available_first_seq"));
    assert!(lag_err.details.contains_key("available_last_seq"));
    assert!(lag_err.details.contains_key("dropped"));

    assert!(sub_skip.close().completed);
    assert!(sub_err.close().completed);
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn mixed_unary_server_stream_and_subscription_multiplexed_on_one_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-mixed", None, None);
    let release = stream_release_with_topics(&["events"]);
    let methods = release
        .descriptor()
        .methods()
        .iter()
        .enumerate()
        .map(|(index, m)| MethodDefinition {
            index: index as u16,
            name: m.name().into(),
            access: m.access(),
        });
    let reg = host
        .register(
            ServiceDefinition::new_stream(
                &release,
                release.reference(),
                "mixed",
                methods,
                Arc::new(MixedStreamService),
            )
            .unwrap()
            .with_topic_capacity("events", 4)
            .unwrap(),
        )
        .unwrap();
    let pub_events = reg.publisher("events").unwrap();

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("mixed").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();

    // 1. Open topic subscription first.
    let sub = client
        .subscribe("events", SubscribeOptions::default())
        .unwrap();
    assert_eq!(sub.start_seq(), 1);

    // 2. Open server stream on the exact same physical connection.
    let stream = client
        .begin_stream("echo")
        .unwrap()
        .encode_slice(b"mid-item")
        .unwrap()
        .stream()
        .unwrap();

    // 3. Interleave unary call, stream items, and topic publications.
    let first_stream_item = stream.next_item().unwrap().unwrap();
    assert_eq!(first_stream_item.bytes(), b"stream-1");

    assert_eq!(pub_events.publish(b"ev-1".to_vec()).unwrap(), 1);
    let ev1 = sub.next_delivery().unwrap();
    assert_eq!(ev1.seq(), 1);
    assert_eq!(ev1.bytes(), b"ev-1");

    let unary_reply = client.call_owned("ping", b"hello-unary").unwrap();
    assert_eq!(unary_reply, b"hello-unary");

    let second_stream_item = stream.next_item().unwrap().unwrap();
    assert_eq!(second_stream_item.bytes(), b"mid-item");
    let third_stream_item = stream.next_item().unwrap().unwrap();
    assert_eq!(third_stream_item.bytes(), b"stream-3");
    assert!(stream.next_item().unwrap().is_none());

    assert_eq!(pub_events.publish(b"ev-2".to_vec()).unwrap(), 2);
    let ev2 = sub.next_delivery().unwrap();
    assert_eq!(ev2.seq(), 2);
    assert_eq!(ev2.bytes(), b"ev-2");

    assert!(sub.close().completed);
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn unknown_topic_returns_713_without_poisoning_route_or_blocking_reregister() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-713", None, None);
    let release = release_with_topics(&["state"]);
    let mut reg1 = host.register(echo_def(&release, "sim")).unwrap();
    let pub1 = reg1.publisher("state").unwrap();
    assert_eq!(pub1.publish(b"r1".to_vec()).unwrap(), 1);

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();

    let err = client
        .subscribe("does_not_exist", SubscribeOptions::default())
        .unwrap_err();
    assert_eq!(err.transport_phase(), Some(TransportPhase::PreDispatch));
    let semantic = assert_semantic_code(err, ErrorCode::ProtocolViolation);
    assert_eq!(
        semantic.details.get("reason").map(String::as_str),
        Some("unknown_topic")
    );

    // Same client still works cleanly on the valid topic & unary calls.
    assert_eq!(client.call_owned("echo", b"ok").unwrap(), b"ok");
    let sub1 = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    assert_eq!(sub1.next_delivery().unwrap().bytes(), b"r1");

    // Unregister reg1 -> sub1 terminates with 718 route_unregistered.
    let close_thread = std::thread::spawn(move || reg1.close().unwrap());
    let term = assert_semantic_code(sub1.next_delivery().unwrap_err(), ErrorCode::CallCancelled);
    assert_eq!(
        term.details.get("cause").map(String::as_str),
        Some("route_unregistered")
    );
    let unreg_outcome = close_thread.join().unwrap();
    assert!(unreg_outcome.local_removed);

    // Re-register the same route name -> because 713 did NOT poison TERMINAL_REFUSAL
    // and 718(route_unregistered) marked NEEDS_ACQUIRE, the same client follows
    // the new registration with seq restarting at 1!
    let reg2 = host
        .register(
            echo_def(&release, "sim")
                .with_initial_topic_bytes("state", b"r2-init".to_vec())
                .unwrap(),
        )
        .unwrap();
    let sub2 = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    assert_eq!(sub2.start_seq(), 1);
    let d2 = sub2.next_delivery().unwrap();
    assert_eq!(d2.seq(), 1);
    assert_eq!(d2.bytes(), b"r2-init");

    assert!(sub2.close().completed);
    drop(reg2);
    let _ = host.shutdown();
}

#[test]
fn host_shutdown_drains_remote_subscriptions_with_718_host_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-shutdown", None, None);
    let release = release_with_topics(&["state"]);
    let reg = host
        .register(
            echo_def(&release, "sim")
                .with_initial_topic_bytes("state", b"init".to_vec())
                .unwrap(),
        )
        .unwrap();

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    let sub = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    assert_eq!(sub.next_delivery().unwrap().bytes(), b"init");

    let shutdown_host = host.clone();
    let shutdown_thread = std::thread::spawn(move || shutdown_host.shutdown());
    let term = assert_semantic_code(sub.next_delivery().unwrap_err(), ErrorCode::CallCancelled);
    assert_eq!(
        term.details.get("cause").map(String::as_str),
        Some("host_shutdown")
    );
    let outcome = shutdown_thread.join().unwrap();
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    drop(reg);
}

#[test]
fn subscription_slot_guard_n2_preserves_unary_headroom_and_n1_n0_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    // Server N = 2, Client N = 2 -> at most 1 active subscription, 1 slot
    // always kept free for normal unary control calls!
    let (_host_rt, host) = make_host(&dir, "ipc-n2", None, Some(2));
    let release = release_with_topics(&["state"]);
    let reg = host.register(echo_def(&release, "sim")).unwrap();

    let client_rt = make_runtime(&dir, Some(2));
    let client = client_rt
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();

    let sub1 = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    // 2nd subscription on N=2 is rejected with 717 (client N-1 guard).
    assert_semantic_code(
        client
            .subscribe("state", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::CallCapacityExceeded,
    );
    // Even from a second client with spacious N, server's own N=2 guard rejects
    // a 2nd subscription with 717 while leaving 1 server slot for unary calls!
    let client_rt2 = make_runtime(&dir, None);
    let client2 = client_rt2
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    assert_semantic_code(
        client2
            .subscribe("state", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::CallCapacityExceeded,
    );
    // Unary control calls succeed on BOTH clients while sub1 is active!
    assert_eq!(client.call_owned("echo", b"ctrl-1").unwrap(), b"ctrl-1");
    assert_eq!(client2.call_owned("echo", b"ctrl-2").unwrap(), b"ctrl-2");

    // Closing sub1 frees both client and server slots -> new subscription succeeds.
    assert!(sub1.close().completed);
    let sub2 = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    assert!(sub2.close().completed);
    drop(reg);
    let _ = host.shutdown();

    // N = 1 allows 1 subscription; N = 0 rejects all.
    let (_host_rt1, host1) = make_host(&dir, "ipc-n1", None, Some(1));
    let reg1 = host1.register(echo_def(&release, "sim")).unwrap();
    let client_rt_n1 = make_runtime(&dir, Some(1));
    let c1 = client_rt_n1
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host1.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    let s1 = c1.subscribe("state", SubscribeOptions::default()).unwrap();
    assert_semantic_code(
        c1.subscribe("state", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::CallCapacityExceeded,
    );
    assert!(s1.close().completed);
    drop(reg1);
    let _ = host1.shutdown();

    let (_host_rt0, host0) = make_host(&dir, "ipc-n0", None, Some(0));
    let reg0 = host0.register(echo_def(&release, "sim")).unwrap();
    let client_rt_spacious = make_runtime(&dir, None);
    let c0 = client_rt_spacious
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host0.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    assert_semantic_code(
        c0.subscribe("state", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::CallCapacityExceeded,
    );
    drop(reg0);
    let _ = host0.shutdown();
}

#[test]
fn topic_only_contract_works_end_to_end_over_ipc17() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-topic-only", None, None);
    let release = topic_only_release(&["telemetry"]);
    let def = ServiceDefinition::new(
        &release,
        release.reference(),
        "sensor",
        std::iter::empty(),
        Arc::new(EchoService),
    )
    .unwrap()
    .with_initial_topic_bytes("telemetry", b"boot".to_vec())
    .unwrap();
    let reg = host.register(def).unwrap();
    let pub_tel = reg.publisher("telemetry").unwrap();

    let client_rt = make_runtime(&dir, None);
    let client = client_rt
        .connect(
            release.expected_route("sensor").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    let sub = client
        .subscribe("telemetry", SubscribeOptions::default())
        .unwrap();
    assert_eq!(sub.start_seq(), 1);
    assert_eq!(sub.next_delivery().unwrap().bytes(), b"boot");
    assert_eq!(pub_tel.publish(b"reading-2".to_vec()).unwrap(), 2);
    let d2 = sub.next_delivery().unwrap();
    assert_eq!(d2.seq(), 2);
    assert_eq!(d2.bytes(), b"reading-2");

    assert!(sub.close().completed);
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn consumer_cancel_and_abrupt_client_drop_release_server_subscription_resources() {
    let dir = tempfile::tempdir().unwrap();
    let (_host_rt, host) = make_host(&dir, "ipc-drop", None, None);
    let release = release_with_topics(&["state"]);
    let reg = host
        .register(echo_def(&release, "sim").with_topic_capacity("state", 2).unwrap())
        .unwrap();
    let pub_state = reg.publisher("state").unwrap();

    // Sub 1: explicit cancel + close receipt reports exact complete_items.
    let client_rt1 = make_runtime(&dir, None);
    let client1 = client_rt1
        .connect(
            release.expected_route("sim").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap();
    let sub1 = client1
        .subscribe_with_options(
            "state",
            SubscribeOptions::default(),
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
        )
        .unwrap();
    assert_eq!(reg.active_subscribers(), 1);
    assert_eq!(pub_state.publish(b"one".to_vec()).unwrap(), 1);
    assert_eq!(sub1.next_delivery().unwrap().seq(), 1);
    let receipt = sub1.close();
    assert!(receipt.completed);
    assert_eq!(receipt.complete_items, Some(1));
    assert_semantic_code(
        receipt.terminal.unwrap().unwrap_err(),
        ErrorCode::CallCancelled,
    );

    // Sub 2: drop entire client runtime while a large SHM frame is unread!
    {
        let client_rt2 = make_runtime(&dir, None);
        let client2 = client_rt2
            .connect(
                release.expected_route("sim").unwrap(),
                Connect::DirectIpc {
                    address: host.local_endpoint().address().to_string(),
                },
            )
            .unwrap();
        let _sub2 = client2
            .subscribe("state", SubscribeOptions::default())
            .unwrap();
        assert_eq!(reg.active_subscribers(), 1);
        let big = vec![0xABu8; 128 * 1024];
        assert_eq!(pub_state.publish(big).unwrap(), 2);
        // Drop _sub2, client2, and client_rt2 abruptly without calling close().
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while reg.active_subscribers() != 0 {
        assert!(
            Instant::now() < deadline,
            "server did not clean up abruptly dropped remote subscription"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    drop(reg);
    let outcome = host.shutdown();
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
}

const RAW_PEER_WAIT: Duration = Duration::from_secs(5);

async fn read_ipc_frame(peer: &mut LocalStream) -> (FrameHeader, Vec<u8>) {
    tokio::time::timeout(RAW_PEER_WAIT, async {
        let mut len = [0; 4];
        peer.read_exact(&mut len).await.unwrap();
        let mut body = vec![0; u32::from_le_bytes(len) as usize];
        peer.read_exact(&mut body).await.unwrap();
        let (header, payload) = frame::decode_frame_body(&body, u32::from_le_bytes(len)).unwrap();
        (header, payload.to_vec())
    })
    .await
    .expect("IPC reply within test deadline")
}

async fn topic_peer(
    host: &Host,
    release: &ContractRelease,
    capability_flags: u16,
) -> (LocalStream, RouteCallIdentity) {
    let mut peer = LocalStream::connect(host.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    peer.write_all(&encode_version_preamble(VersionPreambleKind::Hello))
        .await
        .unwrap();
    let mut accepted = [0; 25];
    tokio::time::timeout(RAW_PEER_WAIT, peer.read_exact(&mut accepted))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        accepted,
        encode_version_preamble(VersionPreambleKind::Accepted)
    );
    let hello = encode_client_handshake(capability_flags, "").unwrap();
    peer.write_all(&frame::encode_frame(0, FLAG_HANDSHAKE, &hello))
        .await
        .unwrap();
    let (header, ack) = read_ipc_frame(&mut peer).await;
    assert_eq!(header.flags, FLAG_HANDSHAKE | FLAG_RESPONSE);
    assert_eq!(
        decode_handshake(&ack, true).unwrap().capability_flags & capability_flags,
        capability_flags
    );

    let route = release.expected_route("sim").unwrap();
    let identity = RouteCallIdentity {
        route_name: route.route_name,
        crm_ns: route.crm_ns,
        crm_name: route.crm_name,
        crm_ver: route.crm_ver,
        abi_hash: route.abi_hash,
        signature_hash: route.signature_hash,
    };
    let open = rpc::encode_open_subscribe_frame(
        1,
        &identity,
        "field",
        rpc::WireSubscribeOptions::default(),
    )
    .unwrap();
    peer.write_all(&open).await.unwrap();
    let (header, payload) = read_ipc_frame(&mut peer).await;
    assert_eq!(header.request_id, 1);
    assert!(matches!(
        rpc::decode_rpc_record(&header, &payload, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::Ready { start_seq: 1 }
    ));
    (peer, identity)
}

async fn read_topic_begin(peer: &mut LocalStream, bytes: usize) {
    let (header, payload) = read_ipc_frame(peer).await;
    assert_eq!(header.request_id, 1);
    assert!(matches!(
        rpc::decode_rpc_record(&header, &payload, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::ItemBegin { total_bytes, .. } if total_bytes == bytes as u64
    ));
}

async fn read_topic_loan(peer: &mut LocalStream, bytes: usize) -> BuddyPayload {
    peer.write_all(&rpc::encode_credit_frame(1, bytes as u64, 0).unwrap())
        .await
        .unwrap();
    let (header, payload) = read_ipc_frame(peer).await;
    assert_eq!(header.request_id, 1);
    let RpcRecord::ItemData { value, .. } =
        rpc::decode_rpc_record(&header, &payload, Some(CallMode::Subscribe)).unwrap()
    else {
        panic!("expected topic ITEM_DATA");
    };
    let ValueStorage::Shm(loan) = value.storage else {
        panic!("expected one shared-memory descriptor");
    };
    assert!(loan.is_dedicated);
    assert_eq!(loan.data_size as usize, bytes);
    loan
}

async fn release_topic_loan(peer: &mut LocalStream, loan: BuddyPayload) {
    let batch = ReleaseBatch::new(&[loan.loan_id]).unwrap();
    peer.write_all(&encode_release_frame(&batch, false))
        .await
        .unwrap();
    peer.write_all(&frame::encode_frame(
        0,
        FLAG_SIGNAL,
        &c2_wire::msg_type::PING_BYTES,
    ))
    .await
    .unwrap();
    let (header, pong) = read_ipc_frame(peer).await;
    assert_eq!(header.flags, FLAG_SIGNAL | FLAG_RESPONSE);
    assert_eq!(pong.as_slice(), c2_wire::msg_type::PONG_BYTES.as_slice());
}

#[test]
fn dedicated_topic_backing_is_charged_once_until_both_peers_release() {
    let dir = tempfile::tempdir().unwrap();
    let (_runtime, host) = make_host(&dir, "ipc-held-loans", Some(16 * 1024 * 1024), None);
    let release = release_with_topics(&["field"]);
    let reg = host.register(echo_def(&release, "sim")).unwrap();
    let publisher = reg.publisher("field").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (mut a, _) = topic_peer(&host, &release, CAP_SHM_RESPONSE).await;
            let (mut b, _) = topic_peer(&host, &release, CAP_SHM_RESPONSE).await;

            let used = || host.memory_stats().server.memory.unwrap().used_bytes;
            let baseline = used();
            let bytes = 5 * 1024 * 1024;
            assert_eq!(publisher.publish(vec![0xA5; bytes]).unwrap(), 1);
            let single_allocation = used();
            assert!(single_allocation - baseline >= bytes as u64);
            assert!(single_allocation - baseline < 2 * bytes as u64);
            read_topic_begin(&mut a, bytes).await;
            read_topic_begin(&mut b, bytes).await;
            let loan_a = read_topic_loan(&mut a, bytes).await;
            let loan_b = read_topic_loan(&mut b, bytes).await;
            assert_ne!(loan_a.loan_id, loan_b.loan_id);
            assert_eq!(
                (loan_a.seg_idx, loan_a.generation, loan_a.offset),
                (loan_b.seg_idx, loan_b.generation, loan_b.offset)
            );
            assert_eq!(used(), single_allocation);

            assert_eq!(publisher.publish(Vec::new()).unwrap(), 2);
            assert_eq!(used(), single_allocation);
            release_topic_loan(&mut a, loan_a).await;
            assert_eq!(used(), single_allocation);
            release_topic_loan(&mut b, loan_b).await;
            tokio::time::timeout(RAW_PEER_WAIT, async {
                while used() != baseline {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("evicted SHM backing must release its charge after both loans settle");

            drop((a, b));
        });
    drop(reg);
    let _ = host.shutdown();
}

#[test]
fn unregister_rejects_writes_while_a_selected_topic_frame_waits_for_credit() {
    let dir = tempfile::tempdir().unwrap();
    let (_runtime, host) = make_host(&dir, "ipc-unregister-credit", None, None);
    let release = release_with_topics(&["field"]);
    let mut reg = host.register(echo_def(&release, "sim")).unwrap();
    let observer = reg.observer();
    let publisher = reg.publisher("field").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (mut peer, identity) = topic_peer(&host, &release, 0).await;

            publisher.publish(b"pending".to_vec()).unwrap();
            read_topic_begin(&mut peer, 7).await;
            let closing = std::thread::spawn(move || reg.close());
            tokio::time::timeout(RAW_PEER_WAIT, async {
                while !observer.is_closed() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(observer.active_subscribers(), 1);
            assert!(!closing.is_finished());
            assert_semantic_code(
                publisher.publish(b"late".to_vec()).unwrap_err(),
                ErrorCode::ResourceClosed,
            );

            let write = rpc::encode_open_frame(
                2,
                &identity,
                1,
                CallMode::Unary,
                rpc::ValuePart::whole(ValueStorage::Inline(b"late")),
            )
            .unwrap();
            peer.write_all(&write).await.unwrap();
            let (header, payload) = read_ipc_frame(&mut peer).await;
            assert_eq!(header.request_id, 2);
            assert!(matches!(
                rpc::decode_rpc_record(&header, &payload, Some(CallMode::Unary)).unwrap(),
                RpcRecord::CallEnd { status: CallStatus::NotExecuted, error: Some(error), .. }
                    if error.code == ErrorCode::ResourceClosed
            ));
            assert!(!closing.is_finished());

            peer.write_all(&rpc::encode_credit_frame(1, 7, 0).unwrap())
                .await
                .unwrap();
            let (header, payload) = read_ipc_frame(&mut peer).await;
            assert_eq!(header.request_id, 1);
            assert!(matches!(
                rpc::decode_rpc_record(&header, &payload, Some(CallMode::Subscribe)).unwrap(),
                RpcRecord::ItemData { value, .. } if value.storage == ValueStorage::Inline(b"pending")
            ));
            let (header, payload) = read_ipc_frame(&mut peer).await;
            assert_eq!(header.request_id, 1);
            assert!(matches!(
                rpc::decode_rpc_record(&header, &payload, Some(CallMode::Subscribe)).unwrap(),
                RpcRecord::CallEnd { status: CallStatus::Failed, complete_items: Some(1), error: Some(error), .. }
                    if error.code == ErrorCode::CallCancelled
            ));
            let outcome = closing.join().unwrap().unwrap();
            assert!(outcome.local_removed && outcome.close.active_drained);
            assert_eq!(observer.active_subscribers(), 0);

            drop(peer);
        });
    let _ = host.shutdown();
}
