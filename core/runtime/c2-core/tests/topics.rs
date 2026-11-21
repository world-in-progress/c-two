use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use c2_contract::{
    ContractRelease, ExpectedRouteContract, MethodAccess, derive_contract_fingerprints_json,
};
use c2_core::{
    EncodedService, Error, Host, HostOptions, LagPolicy, LocalEndpointOptions, MethodDefinition,
    Runtime, RuntimeOptions, RuntimeShutdownTarget, ServiceDefinition, SubscribeOptions,
    TopicStart,
};
use c2_error::{C2Error, ErrorCode};
use serde_json::{Value, json};

const BASE_DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct EchoService;

impl EncodedService for EchoService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected method index {other}"),
            )),
        }
    }
}

fn unique_server_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
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

fn service_def(release: &ContractRelease, route_name: &str) -> ServiceDefinition {
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

fn make_host(
    prefix: &str,
    memory_budget: Option<u64>,
    max_outstanding: Option<u64>,
) -> (tempfile::TempDir, Runtime, Host) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    #[cfg(unix)]
    runtime
        .set_local_endpoint(LocalEndpointOptions {
            unix_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
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
    (dir, runtime, host)
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
fn registration_scoped_publishers_isolate_forks_and_initial_state() {
    let (_dir, _rt, host) = make_host("topic-fork", None, None);
    let rel = release_with_topics(&["field", "telemetry"]);

    let mut reg_main = host
        .register(
            service_def(&rel, "sim_main")
                .with_initial_topic_bytes("field", b"init-main".to_vec())
                .unwrap(),
        )
        .unwrap();
    let mut reg_fork = host
        .register(
            service_def(&rel, "sim_fork")
                .with_initial_topic_bytes("field", b"init-fork".to_vec())
                .unwrap(),
        )
        .unwrap();

    let pub_main = reg_main.publisher("field").unwrap();
    let pub_fork = reg_fork.publisher("field").unwrap();
    assert_eq!(pub_main.head_seq(), 1);
    assert_eq!(pub_fork.head_seq(), 1);

    // `start = Latest` immediately sees the committed initial frame; `Next` starts at 2.
    let sub_latest = reg_main
        .subscribe_local(
            "field",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    let sub_next = reg_main
        .subscribe_local(
            "field",
            SubscribeOptions {
                start: TopicStart::Next,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(sub_latest.start_seq(), 1);
    assert_eq!(sub_next.start_seq(), 2);

    let d1 = sub_latest.next_delivery().unwrap();
    assert_eq!(d1.seq(), 1);
    assert_eq!(d1.dropped(), 0);
    assert_eq!(d1.bytes(), b"init-main");

    // Publishing on main never affects fork and vice versa.
    assert_eq!(pub_main.publish(b"main-2".to_vec()).unwrap(), 2);
    assert_eq!(pub_fork.head_seq(), 1);

    let d2 = sub_next.next_delivery().unwrap();
    assert_eq!(d2.seq(), 2);
    assert_eq!(d2.dropped(), 0);
    assert_eq!(d2.bytes(), b"main-2");

    let sub_fork = reg_fork
        .subscribe_local("field", SubscribeOptions::default())
        .unwrap();
    let df = sub_fork.next_delivery().unwrap();
    assert_eq!(df.seq(), 1);
    assert_eq!(df.bytes(), b"init-fork");

    // Zero-byte frames still advance sequence and deliver cleanly.
    assert_eq!(pub_fork.publish(Vec::new()).unwrap(), 2);
    let df_empty = sub_fork.next_delivery().unwrap();
    assert_eq!(df_empty.seq(), 2);
    assert!(df_empty.bytes().is_empty());

    sub_latest.close();
    sub_next.close();
    sub_fork.close();
    reg_fork.close().unwrap();
    reg_main.close().unwrap();
    host.shutdown();
}

#[test]
fn initial_state_rolls_back_cleanly_on_capacity_or_duplicate_route_failure() {
    let (_dir, _rt, host) = make_host("topic-rollback", Some(128), Some(8));
    let rel = release_with_topics(&["field"]);

    // 1. Initial frame exceeds server.M -> registration fails without opening route or leaking budget.
    let err = host
        .register(
            service_def(&rel, "too_big")
                .with_initial_topic_bytes("field", vec![7u8; 256])
                .unwrap(),
        )
        .unwrap_err();
    assert_semantic_code(err, ErrorCode::CallCapacityExceeded);
    assert_eq!(host.memory_stats().server.memory.unwrap().used_bytes, 0);

    // 2. Duplicate route registration rolls back staged initial frame memory.
    let mut reg1 = host
        .register(
            service_def(&rel, "dup")
                .with_initial_topic_bytes("field", vec![1u8; 32])
                .unwrap(),
        )
        .unwrap();
    let used_after_reg1 = host.memory_stats().server.memory.unwrap().used_bytes;
    assert!(used_after_reg1 >= 32);

    let dup_err = host.register(
        service_def(&rel, "dup")
            .with_initial_topic_bytes("field", vec![2u8; 32])
            .unwrap(),
    );
    assert!(dup_err.is_err());
    // Failed duplicate registration must not retain its staged 32 bytes.
    assert_eq!(
        host.memory_stats().server.memory.unwrap().used_bytes,
        used_after_reg1
    );

    reg1.close().unwrap();
    assert_eq!(host.memory_stats().server.memory.unwrap().used_bytes, 0);
    host.shutdown();
}

#[test]
fn capacity_failure_preserves_previous_frame_and_does_not_burn_seq() {
    // Allow 1 frame of 32 bytes (and room for the brief overlap of two 32-byte
    // frames during replacement), but reject a 256-byte frame.
    let (_dir, _rt, host) = make_host("topic-cap", Some(96), Some(8));
    let rel = release_with_topics(&["field"]);

    let mut reg = host.register(service_def(&rel, "sim")).unwrap();
    let publisher = reg.publisher("field").unwrap();

    assert_eq!(publisher.publish(vec![11u8; 32]).unwrap(), 1);
    let err = publisher.publish(vec![22u8; 256]).unwrap_err();
    let semantic = assert_semantic_code(err, ErrorCode::CallCapacityExceeded);
    assert_eq!(semantic.details.get("stage").map(String::as_str), Some("topic_publish"));

    // Previous frame is intact and sequence was NOT burned.
    assert_eq!(publisher.head_seq(), 1);
    let sub = reg
        .subscribe_local("field", SubscribeOptions::default())
        .unwrap();
    let d1 = sub.next_delivery().unwrap();
    assert_eq!(d1.seq(), 1);
    assert_eq!(d1.bytes(), vec![11u8; 32].as_slice());

    // Subsequent valid publish receives seq = 2.
    assert_eq!(publisher.publish(vec![33u8; 32]).unwrap(), 2);
    let d2 = sub.next_delivery().unwrap();
    assert_eq!(d2.seq(), 2);
    assert_eq!(d2.bytes(), vec![33u8; 32].as_slice());

    sub.close();
    reg.close().unwrap();
    host.shutdown();
}

#[test]
fn lag_skip_and_error_semantics_match_k_k1_k2_boundary() {
    let (_dir, _rt, host) = make_host("topic-lag", None, None);
    let rel = release_with_topics(&["burst", "field"]);

    let mut reg = host
        .register(
            service_def(&rel, "sim")
                .with_topic_capacity("field", 1)
                .unwrap()
                .with_topic_capacity("burst", 2)
                .unwrap(),
        )
        .unwrap();
    let pub_field = reg.publisher("field").unwrap();
    let pub_burst = reg.publisher("burst").unwrap();

    // capacity = 1: read k=1, then publish k+1=2 and k+2=3 before next read.
    let sub_skip = reg
        .subscribe_local(
            "field",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            },
        )
        .unwrap();
    let sub_err = reg
        .subscribe_local(
            "field",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Error,
            },
        )
        .unwrap();

    assert_eq!(pub_field.publish(b"f1".to_vec()).unwrap(), 1);
    let d1_skip = sub_skip.next_delivery().unwrap();
    let d1_err = sub_err.next_delivery().unwrap();
    assert_eq!((d1_skip.seq(), d1_skip.dropped()), (1, 0));
    assert_eq!((d1_err.seq(), d1_err.dropped()), (1, 0));

    assert_eq!(pub_field.publish(b"f2".to_vec()).unwrap(), 2);
    assert_eq!(pub_field.publish(b"f3".to_vec()).unwrap(), 3);

    // Skip jumps to 3 with dropped = 1; Error fails with 720 SubscriptionLagged.
    let d3_skip = sub_skip.next_delivery().unwrap();
    assert_eq!((d3_skip.seq(), d3_skip.dropped()), (3, 1));
    assert_eq!(d3_skip.bytes(), b"f3");

    let lag_err = sub_err.next_delivery().unwrap_err();
    let semantic = assert_semantic_code(lag_err, ErrorCode::SubscriptionLagged);
    assert_eq!(semantic.details.get("expected_seq").map(String::as_str), Some("2"));
    assert_eq!(
        semantic.details.get("available_first_seq").map(String::as_str),
        Some("3")
    );
    assert_eq!(
        semantic.details.get("available_last_seq").map(String::as_str),
        Some("3")
    );
    assert_eq!(semantic.details.get("dropped").map(String::as_str), Some("1"));

    // Previously delivered values remain immutable and readable after eviction and close.
    assert_eq!(d1_skip.bytes(), b"f1");
    assert_eq!(d1_err.bytes(), b"f1");

    // capacity = 2: k=1 read, then k+1=2 and k+2=3 published -> neither lags!
    let sub_burst = reg
        .subscribe_local(
            "burst",
            SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Error,
            },
        )
        .unwrap();
    assert_eq!(pub_burst.publish(b"b1".to_vec()).unwrap(), 1);
    assert_eq!(sub_burst.next_delivery().unwrap().seq(), 1);
    assert_eq!(pub_burst.publish(b"b2".to_vec()).unwrap(), 2);
    assert_eq!(pub_burst.publish(b"b3".to_vec()).unwrap(), 3);
    let b2 = sub_burst.next_delivery().unwrap();
    let b3 = sub_burst.next_delivery().unwrap();
    assert_eq!((b2.seq(), b2.dropped(), b2.bytes()), (2, 0, b"b2".as_slice()));
    assert_eq!((b3.seq(), b3.dropped(), b3.bytes()), (3, 0, b"b3".as_slice()));

    sub_skip.close();
    sub_burst.close();
    reg.close().unwrap();
    host.shutdown();
}

#[test]
fn unregister_and_reregister_isolate_old_handles_and_wake_waiters() {
    let (_dir, _rt, host) = make_host("topic-rereg", None, None);
    let rel = release_with_topics(&["field"]);

    let mut reg1 = host.register(service_def(&rel, "sim")).unwrap();
    let pub1 = reg1.publisher("field").unwrap();
    let sub1 = reg1
        .subscribe_local("field", SubscribeOptions::default())
        .unwrap();

    // Deliver one frame, then block waiting for seq=2 on another thread.
    assert_eq!(pub1.publish(b"v1".to_vec()).unwrap(), 1);
    let d1 = sub1.next_delivery().unwrap();
    assert_eq!(d1.bytes(), b"v1");

    let waiter = std::thread::spawn(move || sub1.next_delivery());
    std::thread::sleep(Duration::from_millis(30));

    reg1.close().unwrap();
    let wake_err = waiter.join().unwrap().unwrap_err();
    let semantic = assert_semantic_code(wake_err, ErrorCode::CallCancelled);
    assert_eq!(
        semantic.details.get("cause").map(String::as_str),
        Some("route_unregistered")
    );

    // Old publisher reports ResourceClosed (707) and cannot publish into a new registration.
    assert!(pub1.is_closed());
    assert_semantic_code(pub1.publish(b"stale".to_vec()).unwrap_err(), ErrorCode::ResourceClosed);

    // Re-registering the same route name starts a clean seq=1 space.
    let mut reg2 = host.register(service_def(&rel, "sim")).unwrap();
    let pub2 = reg2.publisher("field").unwrap();
    assert_eq!(pub2.head_seq(), 0);
    assert_semantic_code(pub1.publish(b"stale2".to_vec()).unwrap_err(), ErrorCode::ResourceClosed);
    assert_eq!(pub2.publish(b"fresh-1".to_vec()).unwrap(), 1);

    // Dropping old reg1 must not touch reg2.
    drop(reg1);
    assert!(!pub2.is_closed());
    let sub2 = reg2
        .subscribe_local("field", SubscribeOptions::default())
        .unwrap();
    let df = sub2.next_delivery().unwrap();
    assert_eq!((df.seq(), df.bytes()), (1, b"fresh-1".as_slice()));

    // Already-delivered d1 from reg1 is still valid.
    assert_eq!(d1.bytes(), b"v1");

    sub2.close();
    reg2.close().unwrap();
    host.shutdown();
}

#[test]
fn contract_and_topic_identity_errors_never_overload_701() {
    let (_dir, _rt, host) = make_host("topic-id", None, None);
    let rel = release_with_topics(&["field"]);
    let mut reg = host.register(service_def(&rel, "sim")).unwrap();
    let expected = rel.expected_route("sim").unwrap();

    // Missing route -> 701 ResourceNotFound.
    let missing = rel.expected_route("missing").unwrap();
    assert_semantic_code(
        host.subscribe_local(&missing, "field", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::ResourceNotFound,
    );

    // Wrong contract identity -> 709 ContractMismatch.
    let wrong_contract = ExpectedRouteContract {
        abi_hash: "f".repeat(64),
        ..expected.clone()
    };
    assert_semantic_code(
        host.subscribe_local(&wrong_contract, "field", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::ContractMismatch,
    );

    // Unknown topic on matched contract -> 713 ProtocolViolation (NEVER 701!).
    assert_semantic_code(
        host.subscribe_local(&expected, "no_such_topic", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::ProtocolViolation,
    );

    reg.close().unwrap();
    host.shutdown();
}

#[test]
fn tight_n2_reserves_one_slot_for_ordinary_calls_and_runtime_shutdown_cancels() {
    let (_dir, runtime, host) = make_host("topic-n2", Some(64 * 1024), Some(2));
    let rel = release_with_topics(&["field"]);
    let reg = host.register(service_def(&rel, "sim")).unwrap();
    let expected = rel.expected_route("sim").unwrap();

    // With N = 2, the first subscription enters and the second is rejected so
    // an ordinary control/unary call can still enter.
    let sub1 = host
        .subscribe_local(&expected, "field", SubscribeOptions::default())
        .unwrap();
    assert_semantic_code(
        host.subscribe_local(&expected, "field", SubscribeOptions::default())
            .unwrap_err(),
        ErrorCode::CallCapacityExceeded,
    );

    // An ordinary server-side unary slot on the same route still succeeds!
    let guard = reg.route_concurrency().blocking_acquire(0).unwrap();
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(2)
    );
    drop(guard);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(1)
    );

    // Runtime shutdown wakes the blocked subscription with host_shutdown and drains slots to 0.
    let waiter = std::thread::spawn(move || sub1.next_delivery());
    std::thread::sleep(Duration::from_millis(30));

    let receipt = runtime
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&host),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    let _ = receipt.observe_until(Instant::now() + Duration::from_secs(5));

    let wake_err = waiter.join().unwrap().unwrap_err();
    let semantic = assert_semantic_code(wake_err, ErrorCode::CallCancelled);
    assert_eq!(
        semantic.details.get("cause").map(String::as_str),
        Some("host_shutdown")
    );
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(0)
    );
    drop(reg);
}

#[test]
fn idle_local_subscription_settles_on_unregister_shutdown_and_cancel_without_polling() {
    // 1. Same-thread unregister completes while an idle local subscription stays alive;
    //    previously delivered values remain readable and subsequent polling sees
    //    718 route_unregistered (and re-registration works on the same thread).
    let (_dir, _rt, host) = make_host("topic-idle-unreg", None, Some(4));
    let rel = release_with_topics(&["field"]);
    let expected = rel.expected_route("sim").unwrap();
    let mut reg1 = host
        .register(
            service_def(&rel, "sim")
                .with_initial_topic_bytes("field", b"snap-1".to_vec())
                .unwrap(),
        )
        .unwrap();
    let sub1 = host
        .subscribe_local(&expected, "field", SubscribeOptions::default())
        .unwrap();
    let d1 = sub1.next_delivery().unwrap();
    assert_eq!(d1.bytes(), b"snap-1");
    assert_eq!(reg1.active_subscribers(), 1);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(1)
    );

    // No background thread polls `sub1`; closing `reg1` on the same thread MUST NOT deadlock.
    let unreg = reg1.close().unwrap();
    assert!(unreg.local_removed);
    assert_eq!(reg1.active_subscribers(), 0);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(0)
    );
    // Already-delivered value remains valid.
    assert_eq!(d1.bytes(), b"snap-1");
    // Re-register on the same thread while `sub1` is still alive.
    let reg2 = host
        .register(
            service_def(&rel, "sim")
                .with_initial_topic_bytes("field", b"snap-2".to_vec())
                .unwrap(),
        )
        .unwrap();
    // Subsequent read on old `sub1` observes exact first cause `route_unregistered` (718).
    let err1 = assert_semantic_code(sub1.next_delivery().unwrap_err(), ErrorCode::CallCancelled);
    assert_eq!(
        err1.details.get("cause").map(String::as_str),
        Some("route_unregistered")
    );

    // 2. Idle `cancel()` immediately releases business slot and drain guard.
    let sub_cancel = host
        .subscribe_local(&expected, "field", SubscribeOptions::default())
        .unwrap();
    assert_eq!(reg2.active_subscribers(), 1);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(1)
    );
    sub_cancel.cancel();
    assert_eq!(reg2.active_subscribers(), 0);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(0)
    );
    let cancel_err = assert_semantic_code(
        sub_cancel.next_delivery().unwrap_err(),
        ErrorCode::CallCancelled,
    );
    assert_eq!(
        cancel_err.details.get("cause").map(String::as_str),
        Some("consumer_cancelled")
    );

    // 3. Same-thread host shutdown completes while an idle local subscription stays alive.
    let sub_shutdown = host
        .subscribe_local(&expected, "field", SubscribeOptions::default())
        .unwrap();
    let d2 = sub_shutdown.next_delivery().unwrap();
    assert_eq!(d2.bytes(), b"snap-2");
    assert_eq!(reg2.active_subscribers(), 1);
    let outcome = host.shutdown();
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert_eq!(reg2.active_subscribers(), 0);
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(0)
    );
    assert_eq!(d2.bytes(), b"snap-2");
    let shut_err = assert_semantic_code(
        sub_shutdown.next_delivery().unwrap_err(),
        ErrorCode::CallCancelled,
    );
    assert_eq!(
        shut_err.details.get("cause").map(String::as_str),
        Some("host_shutdown")
    );
    drop(reg2);
}
