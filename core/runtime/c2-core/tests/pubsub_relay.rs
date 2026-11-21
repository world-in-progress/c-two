//! End-to-end source-relay topic subscriptions.
use c2_config::{CallOptions, CallTimeout, ConfigSources};
use c2_contract::{ContractRelease, MethodAccess, derive_contract_fingerprints_json};
use c2_core::{
    Connect, EncodedService, HostOptions, LocalEndpointOptions, MethodDefinition, Runtime,
    RuntimeOptions, ServiceDefinition, SubscribeOptions,
};
use c2_error::C2Error;
use c2_http::relay::{RelayConfig, RelayServer};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

struct Echo;
impl EncodedService for Echo {
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(if index == 0 {
            b"pong".to_vec()
        } else {
            request
        })
    }
}
fn release() -> ContractRelease {
    release_names(&["state"])
}
fn release_names(names: &[&str]) -> ContractRelease {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/contracts/portable-release.contract.json"
    ))
    .unwrap();
    let binding = value["methods"][1]["bindings"]["output"].clone();
    value["topics"] = Value::Array(
        names
            .iter()
            .map(|name| json!({ "name": name, "output": {"kind": "payload"}, "binding": binding }))
            .collect(),
    );
    let fp = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(fp.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] = Value::String(fp.signature_hash().to_string());
    ContractRelease::from_descriptor_json(value.to_string().as_bytes()).unwrap()
}
fn runtime(root: &tempfile::TempDir) -> Runtime {
    let rt = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    rt.set_local_endpoint_with_sources(
        LocalEndpointOptions {
            #[cfg(unix)]
            unix_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
        ConfigSources::empty(),
    )
    .unwrap();
    rt
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "timed out");
        std::thread::yield_now();
    }
}
#[test]
fn concurrent_explicit_relay_subscribers_share_one_upstream_and_detach() {
    let root = tempfile::Builder::new()
        .prefix("c2-relay-topic-")
        .tempdir()
        .unwrap();
    let host_rt = runtime(&root);
    let mut relay = RelayServer::start_with_context(
        RelayConfig {
            bind: "127.0.0.1:0".into(),
            heartbeat_interval: Duration::ZERO,
            ..Default::default()
        },
        host_rt.local_endpoint_context().unwrap(),
    )
    .unwrap();
    let relay_url = format!("http://{}", relay.local_addr());
    let host = host_rt
        .host(HostOptions::default().with_relay_anchor_address(&relay_url))
        .unwrap();
    let contract = release();
    let mut registration = host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                "topic-route",
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
                Arc::new(Echo),
            )
            .unwrap()
            .with_topic_capacity("state", 2)
            .unwrap(),
        )
        .unwrap();
    let publisher = registration.publisher("state").unwrap();
    let client_rt = runtime(&root);
    let client = client_rt
        .connect(
            contract.expected_route("topic-route").unwrap(),
            Connect::ExplicitRelay { relay_url },
        )
        .unwrap();
    let subscriptions = std::thread::scope(|s| {
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let client = client.clone();
                s.spawn(move || {
                    client
                        .subscribe_with_options(
                            "state",
                            SubscribeOptions::default(),
                            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(5))),
                        )
                        .unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(registration.active_subscribers(), 1);
    assert_eq!(publisher.publish(b"one".to_vec()).unwrap(), 1);
    for sub in &subscriptions {
        let item = sub.next_delivery().unwrap();
        assert_eq!((item.seq(), item.bytes()), (1, b"one".as_slice()));
    }
    assert!(
        subscriptions[0]
            .close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    assert_eq!(registration.active_subscribers(), 1);
    assert_eq!(publisher.publish(b"two".to_vec()).unwrap(), 2);
    for sub in &subscriptions[1..] {
        let item = sub.next_delivery().unwrap();
        assert_eq!((item.seq(), item.bytes()), (2, b"two".as_slice()));
    }
    for sub in &subscriptions[1..] {
        assert!(
            sub.close_with_timeout(Some(Duration::from_secs(5)))
                .completed
        );
    }
    wait_until(|| registration.active_subscribers() == 0);
    registration.close().unwrap();
    let _ = host.shutdown();
    relay.stop().unwrap();
}

struct Fixture {
    _root: tempfile::TempDir,
    _host_rt: Runtime,
    host: c2_core::Host,
    registration: c2_core::Registration,
    relay: RelayServer,
    client: c2_core::Client,
}
impl Fixture {
    fn new(limits: c2_config::DomainLimits) -> Self {
        Self::with_topics(limits, &["state"])
    }
    fn with_topics(limits: c2_config::DomainLimits, names: &[&str]) -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2-relay-topic-")
            .tempdir()
            .unwrap();
        let host_rt = runtime(&root);
        let relay = RelayServer::start_resolved(
            RelayConfig {
                bind: "127.0.0.1:0".into(),
                heartbeat_interval: Duration::ZERO,
                domain_limits: limits,
                ..Default::default()
            },
            host_rt.local_endpoint_context().unwrap(),
        )
        .unwrap();
        let relay_url = format!("http://{}", relay.local_addr());
        let host = host_rt
            .host(HostOptions::default().with_relay_anchor_address(&relay_url))
            .unwrap();
        let contract = release_names(names);
        let registration = host
            .register(
                ServiceDefinition::new(
                    &contract,
                    contract.reference(),
                    "topic-route",
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
                    Arc::new(Echo),
                )
                .unwrap()
                .with_topic_capacity("state", 2)
                .unwrap(),
            )
            .unwrap();
        let client_rt = runtime(&root);
        let client = client_rt
            .connect(
                contract.expected_route("topic-route").unwrap(),
                Connect::ExplicitRelay { relay_url },
            )
            .unwrap();
        Self {
            _root: root,
            _host_rt: host_rt,
            host,
            registration,
            relay,
            client,
        }
    }
    fn sub(&self) -> c2_core::CoreTopicSubscription {
        self.client
            .subscribe_with_options(
                "state",
                SubscribeOptions::default(),
                CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(5))),
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.registration.close();
        let _ = self.host.shutdown();
        let _ = self.relay.stop();
    }
}

#[test]
fn relay_subscription_reserves_the_documented_n_boundaries() {
    for n in [1, 2] {
        let fixture = Fixture::new(c2_config::DomainLimits {
            max_outstanding_calls: n,
            ..Default::default()
        });
        let sub = fixture.sub();
        assert_eq!(fixture.registration.active_subscribers(), 1);
        let error = fixture
            .client
            .subscribe("state", SubscribeOptions::default())
            .unwrap_err();
        assert!(
            matches!(error, c2_core::Error::Semantic(ref e) if e.code == c2_error::ErrorCode::CallCapacityExceeded),
            "{error:?}"
        );
        if n == 2 {
            let response = fixture
                .client
                .begin_call("ping")
                .unwrap()
                .encode_vec(Vec::new())
                .unwrap()
                .call_held()
                .unwrap();
            assert_eq!(response.bytes(), b"pong");
        }
        assert!(
            sub.close_with_timeout(Some(Duration::from_secs(5)))
                .completed
        );
        wait_until(|| fixture.registration.active_subscribers() == 0);
    }
}

#[test]
fn unknown_topic_does_not_poison_the_existing_route() {
    let fixture = Fixture::new(Default::default());
    let lookups = fixture.relay.route_lookup_count_for_test();
    assert!(lookups > 0, "initial acquisition must be observable");
    let error = fixture
        .client
        .subscribe("unknown", SubscribeOptions::default())
        .unwrap_err();
    assert!(
        matches!(error, c2_core::Error::Semantic(ref e) if e.code == c2_error::ErrorCode::ProtocolViolation && e.details.get("reason").map(String::as_str) == Some("unknown_topic")),
        "{error:?}"
    );
    let sub = fixture.sub();
    assert_eq!(sub.start_seq(), 1);
    assert_eq!(fixture.relay.route_lookup_count_for_test(), lookups);
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

fn cancelled_cause(error: c2_core::Error, cause: &str) {
    match error {
        c2_core::Error::Semantic(error) => {
            assert_eq!(error.code, c2_error::ErrorCode::CallCancelled);
            assert_eq!(error.details.get("cause").map(String::as_str), Some(cause));
        }
        other => panic!("expected {cause} cancellation, got {other:?}"),
    }
}

#[test]
fn route_unregistration_and_host_shutdown_reach_all_subscribers() {
    for host_shutdown in [false, true] {
        let mut fixture = Fixture::new(Default::default());
        let first = fixture.sub();
        let second = fixture.sub();
        if host_shutdown {
            let outcome = fixture.host.shutdown();
            assert!(outcome.route_close_error.is_none(), "{outcome:?}");
        } else {
            fixture.registration.close().unwrap();
        }
        let cause = if host_shutdown {
            "host_shutdown"
        } else {
            "route_unregistered"
        };
        cancelled_cause(first.next_delivery().unwrap_err(), cause);
        cancelled_cause(second.next_delivery().unwrap_err(), cause);
    }
}

#[test]
fn relay_shutdown_reaches_every_subscriber_after_upstream_cleanup() {
    let mut fixture = Fixture::new(Default::default());
    let first = fixture.sub();
    let second = fixture.sub();
    fixture.relay.stop().unwrap();
    cancelled_cause(first.next_delivery().unwrap_err(), "relay_shutdown");
    cancelled_cause(second.next_delivery().unwrap_err(), "relay_shutdown");
    assert_eq!(fixture.registration.active_subscribers(), 0);
}

#[test]
fn relay_shutdown_drains_host_before_an_unevicted_stopped_reader_resumes() {
    use std::io::{Read, Write};

    let mut fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: 8 * 1024 * 1024 + 64 * 1024,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 4,
    });
    let mut slow = std::net::TcpStream::connect(fixture.relay.local_addr()).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let expected = release().expected_route("topic-route").unwrap();
    write!(slow,
        "POST /topic-route/_topics/state HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {}\r\nx-c2-topic-start: latest\r\nx-c2-topic-on-lag: skip\r\nx-c2-expected-crm-ns: {}\r\nx-c2-expected-crm-name: {}\r\nx-c2-expected-crm-ver: {}\r\nx-c2-expected-abi-hash: {}\r\nx-c2-expected-signature-hash: {}\r\nContent-Length: 0\r\n\r\n",
        fixture.relay.local_addr(), c2_http::topic_wire::MEDIA_TYPE, expected.crm_ns,
        expected.crm_name, expected.crm_ver, expected.abi_hash, expected.signature_hash
    ).unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        slow.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    let frame_size = 8 * 1024 * 1024;
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(vec![42; frame_size])
        .unwrap();
    wait_until(|| fixture.relay.blocked_topic_subscribers_for_test() > 0);
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let (entered, release_frame) = fixture.relay.pause_next_topic_frame_for_test();
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(vec![43; frame_size])
        .unwrap();
    entered.blocking_recv().unwrap();

    let receipt = fixture
        .relay
        .shutdown_and_wait(Duration::from_millis(100))
        .unwrap();
    assert!(
        !receipt.completed,
        "an un-evicted stopped response remains pending"
    );
    release_frame.send(()).unwrap();
    wait_until(|| fixture.registration.active_subscribers() == 0);
    assert_eq!(
        fixture.relay.domain_snapshot().operations.used_operations,
        1
    );

    let mut bytes = Vec::new();
    slow.read_to_end(&mut bytes).unwrap();
    let mut cursor = bytes.as_slice();
    let mut wire = Vec::new();
    let mut http_ended = false;
    while let Some(end) = cursor.windows(2).position(|pair| pair == b"\r\n") {
        let size = usize::from_str_radix(std::str::from_utf8(&cursor[..end]).unwrap(), 16).unwrap();
        cursor = &cursor[end + 2..];
        assert!(cursor.len() >= size + 2, "truncated HTTP chunk");
        wire.extend_from_slice(&cursor[..size]);
        assert_eq!(&cursor[size..size + 2], b"\r\n");
        cursor = &cursor[size + 2..];
        if size == 0 {
            http_ended = true;
            break;
        }
    }
    assert!(http_ended && cursor.is_empty());
    let mut records = wire.as_slice();
    let mut seen_ready = false;
    let mut seen_begin = false;
    let mut payload_bytes = 0;
    let mut terminal = None;
    while !records.is_empty() {
        assert!(records.len() >= 9);
        let (kind, length) =
            c2_http::topic_wire::decode_header(records[..9].try_into().unwrap()).unwrap();
        let length = usize::try_from(length).unwrap();
        assert!(records.len() >= 9 + length);
        let payload = &records[9..9 + length];
        match kind {
            c2_http::topic_wire::RecordKind::Ready => {
                assert!(!seen_ready);
                assert_eq!(u64::from_le_bytes(payload.try_into().unwrap()), 1);
                seen_ready = true;
            }
            c2_http::topic_wire::RecordKind::ItemBegin => {
                assert!(seen_ready && !seen_begin);
                assert_eq!(u64::from_le_bytes(payload[..8].try_into().unwrap()), 1);
                assert_eq!(
                    u64::from_le_bytes(payload[8..].try_into().unwrap()),
                    frame_size as u64
                );
                seen_begin = true;
            }
            c2_http::topic_wire::RecordKind::ItemData => {
                assert!(seen_begin && terminal.is_none());
                assert!(payload.iter().all(|byte| *byte == 42));
                payload_bytes += payload.len();
            }
            c2_http::topic_wire::RecordKind::Terminal => {
                assert!(terminal.is_none());
                terminal = Some(c2_http::topic_wire::decode_terminal(payload).unwrap());
            }
        }
        records = &records[9 + length..];
    }
    assert_eq!(payload_bytes, frame_size);
    let terminal = terminal.expect("relay shutdown terminal");
    assert_eq!(terminal.complete_items, 1);
    assert_eq!(terminal.error.code, c2_error::ErrorCode::CallCancelled);
    assert_eq!(
        terminal.error.details.get("cause").map(String::as_str),
        Some("relay_shutdown")
    );
    assert!(
        fixture
            .relay
            .shutdown_and_wait(Duration::from_secs(5))
            .unwrap()
            .completed
    );
    assert!(
        fixture
            .relay
            .shutdown_and_wait(Duration::ZERO)
            .unwrap()
            .completed
    );
}

#[test]
fn large_frame_uses_file_spill_under_a_tight_relay_memory_budget() {
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: 1024 * 1024,
        file_spill_budget_bytes: 4 * 1024 * 1024,
        max_outstanding_calls: 8,
    });
    let sub = fixture.sub();
    let payload = vec![42; 3 * 1024 * 1024];
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(payload.clone())
        .unwrap();
    let delivery = sub.next_delivery().unwrap();
    assert_eq!(delivery.bytes(), payload);
    assert!(fixture.relay.domain_snapshot().file_spill.peak_bytes > 0);
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

#[test]
fn stopped_tcp_reader_does_not_block_a_fast_subscriber_under_pressure() {
    use std::io::{Read, Write};
    let frame_size = 8 * 1024 * 1024;
    let relay_memory = 9 * 1024 * 1024;
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: relay_memory,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 8,
    });
    let mut slow = std::net::TcpStream::connect(fixture.relay.local_addr()).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let expected = release().expected_route("topic-route").unwrap();
    let request = format!(
        "POST /topic-route/_topics/state HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {}\r\nx-c2-topic-start: latest\r\nx-c2-topic-on-lag: skip\r\nx-c2-expected-crm-ns: {}\r\nx-c2-expected-crm-name: {}\r\nx-c2-expected-crm-ver: {}\r\nx-c2-expected-abi-hash: {}\r\nx-c2-expected-signature-hash: {}\r\nContent-Length: 0\r\n\r\n",
        fixture.relay.local_addr(),
        c2_http::topic_wire::MEDIA_TYPE,
        expected.crm_ns,
        expected.crm_name,
        expected.crm_ver,
        expected.abi_hash,
        expected.signature_hash
    );
    slow.write_all(request.as_bytes()).unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut b = [0];
        slow.read_exact(&mut b).unwrap();
        headers.push(b[0]);
    }
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    let fast = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    assert_eq!(publisher.publish(vec![1; frame_size as usize]).unwrap(), 1);
    // Consume exactly the READY and ITEM_BEGIN HTTP chunks, then stop reading
    // while the first large topic item is still being written to this socket.
    fn http_chunk(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut line = Vec::new();
        while !line.ends_with(b"\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b).unwrap();
            line.push(b[0]);
        }
        let size = usize::from_str_radix(std::str::from_utf8(&line[..line.len() - 2]).unwrap(), 16)
            .unwrap();
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).unwrap();
        let mut crlf = [0; 2];
        stream.read_exact(&mut crlf).unwrap();
        assert_eq!(&crlf, b"\r\n");
        bytes
    }
    let ready = http_chunk(&mut slow);
    assert_eq!(
        c2_http::topic_wire::decode_header(ready[..9].try_into().unwrap())
            .unwrap()
            .0,
        c2_http::topic_wire::RecordKind::Ready
    );
    let begin = http_chunk(&mut slow);
    assert_eq!(
        c2_http::topic_wire::decode_header(begin[..9].try_into().unwrap())
            .unwrap()
            .0,
        c2_http::topic_wire::RecordKind::ItemBegin
    );
    assert_eq!(fast.next_delivery().unwrap().seq(), 1);
    for seq in 2..=3 {
        assert_eq!(
            publisher
                .publish(vec![seq as u8; frame_size as usize])
                .unwrap(),
            seq
        );
        let item = fast.next_delivery().unwrap();
        assert_eq!(item.seq(), seq);
        assert_eq!(item.bytes().len(), frame_size as usize);
        assert!(item.bytes().iter().all(|b| *b == seq as u8));
    }
    let mut scratch = [0u8; 65536];
    let mut received = Vec::new();
    loop {
        match slow.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&scratch[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!(
                "stalled reader was not terminated after {} bytes: {e}",
                received.len()
            ),
        }
    }
    // A blocked HTTP/1 connection can end in a partial chunk. Every complete
    // record must still belong to frame 1, and no terminal or later frame may
    // be appended to an incomplete record.
    let mut cursor = received.as_slice();
    let mut topic_data = Vec::new();
    let mut ended_normally = false;
    while let Some(end) = cursor.windows(2).position(|bytes| bytes == b"\r\n") {
        let Ok(line) = std::str::from_utf8(&cursor[..end]) else {
            panic!("invalid HTTP chunk header");
        };
        let size = usize::from_str_radix(line, 16).unwrap();
        cursor = &cursor[end + 2..];
        if cursor.len() < size + 2 {
            break;
        }
        if size == 0 {
            ended_normally = true;
        }
        topic_data.extend_from_slice(&cursor[..size]);
        assert_eq!(&cursor[size..size + 2], b"\r\n");
        cursor = &cursor[size + 2..];
    }
    let mut complete_bytes = 0;
    let mut records = topic_data.as_slice();
    while records.len() >= 9 {
        let (kind, len) =
            c2_http::topic_wire::decode_header(records[..9].try_into().unwrap()).unwrap();
        let len = len as usize;
        if records.len() < 9 + len {
            break;
        }
        match kind {
            c2_http::topic_wire::RecordKind::ItemData => {
                assert!(records[9..9 + len].iter().all(|byte| *byte == 1));
                complete_bytes += len;
            }
            other => panic!("unexpected topic record: {other:?}"),
        }
        records = &records[9 + len..];
    }
    assert!(complete_bytes > 0 && complete_bytes < frame_size as usize);
    assert!(
        !ended_normally,
        "aborted HTTP response must not look complete"
    );
    assert!(fixture.relay.domain_snapshot().memory.peak_bytes <= relay_memory);
    assert!(
        fast.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn stopped_tcp_reader_does_not_block_fast_subscriber_with_narrow_budget_headroom() {
    use std::io::{Read, Write};
    let frame_size = 8 * 1024 * 1024;
    let mut fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: frame_size + 64 * 1024,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 3,
    });
    let mut slow = std::net::TcpStream::connect(fixture.relay.local_addr()).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let expected = release().expected_route("topic-route").unwrap();
    write!(slow,
        "POST /topic-route/_topics/state HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {}\r\nx-c2-topic-start: latest\r\nx-c2-topic-on-lag: skip\r\nx-c2-expected-crm-ns: {}\r\nx-c2-expected-crm-name: {}\r\nx-c2-expected-crm-ver: {}\r\nx-c2-expected-abi-hash: {}\r\nx-c2-expected-signature-hash: {}\r\nContent-Length: 0\r\n\r\n",
        fixture.relay.local_addr(), c2_http::topic_wire::MEDIA_TYPE, expected.crm_ns,
        expected.crm_name, expected.crm_ver, expected.abi_hash, expected.signature_hash
    ).unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        slow.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    let fast = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    publisher.publish(vec![1; frame_size as usize]).unwrap();
    assert_eq!(fast.next_delivery().unwrap().seq(), 1);
    wait_until(|| fixture.relay.blocked_topic_subscribers_for_test() > 0);
    publisher.publish(vec![2; frame_size as usize]).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let second = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), fast.next_delivery_async()).await
    });
    wait_until(|| fixture.relay.domain_snapshot().operations.used_operations == 1);
    let operations = fixture.relay.domain_snapshot().operations;
    let newcomer = fixture.client.subscribe_with_options(
        "state",
        SubscribeOptions::default(),
        CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(5))),
    );
    if let Ok(ref sub) = newcomer {
        let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
    }
    assert_eq!(publisher.publish(vec![3; frame_size as usize]).unwrap(), 3);
    let third = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), fast.next_delivery_async()).await
    });
    let _ = fast.close_with_timeout(Some(Duration::from_secs(5)));
    let shutdown = fixture
        .relay
        .shutdown_and_wait(Duration::from_secs(5))
        .unwrap();
    let drained = fixture.relay.domain_snapshot();
    drop(slow);
    assert!(
        matches!(second, Ok(Ok(ref item)) if item.seq() == 2),
        "fast subscriber did not advance: {second:?}"
    );
    let refusal = newcomer.as_ref().err().map(ToString::to_string);
    assert!(
        newcomer.is_ok(),
        "stalled subscriber still occupies a relay slot: {operations:?}, refusal: {refusal:?}"
    );
    assert!(matches!(third, Ok(Ok(ref item)) if item.seq() == 3));
    assert!(
        shutdown.completed,
        "evicted connection prevented relay shutdown"
    );
    assert_eq!(drained.operations.used_operations, 0);
    assert_eq!(drained.memory.used_bytes, 0);
}

#[test]
fn evicted_http_client_reports_transport_failure_without_a_partial_delivery() {
    let frame_size = 8 * 1024 * 1024;
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: frame_size + 64 * 1024,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 4,
    });
    let slow = fixture.sub();
    let fast = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    for seq in 1..=2 {
        assert_eq!(
            publisher
                .publish(vec![seq as u8; frame_size as usize])
                .unwrap(),
            seq
        );
        let item = fast.next_delivery().unwrap();
        assert_eq!(item.seq(), seq);
        assert!(item.bytes().iter().all(|byte| *byte == seq as u8));
    }
    wait_until(|| {
        fixture.relay.blocked_topic_subscribers_for_test() > 0
            || fixture.relay.domain_snapshot().operations.used_operations == 1
    });
    assert_eq!(publisher.publish(vec![3; frame_size as usize]).unwrap(), 3);
    assert_eq!(fast.next_delivery().unwrap().seq(), 3);
    wait_until(|| fixture.relay.domain_snapshot().operations.used_operations == 1);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut last_seq = 0;
    let mut terminal = None;
    for _ in 0..4 {
        match runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(15), slow.next_delivery_async()).await
            })
            .expect("slow client did not observe connection termination")
        {
            Ok(item) => {
                assert!(item.seq() > last_seq);
                assert_eq!(item.bytes().len(), frame_size as usize);
                assert!(item.bytes().iter().all(|byte| *byte == item.seq() as u8));
                last_seq = item.seq();
            }
            Err(error) => {
                terminal = Some(error);
                break;
            }
        }
    }
    let error = terminal.expect("aborted connection must not end normally");
    assert_eq!(
        error.transport_phase(),
        Some(c2_core::TransportPhase::DispatchUncertain)
    );
    assert!(matches!(&error, c2_core::Error::Transport(_)), "{error:?}");
    let _ = slow.close_with_timeout(Some(Duration::from_secs(5)));
    assert!(
        fast.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

#[test]
fn lag_error_isolated_from_fast_skip_subscriber() {
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: 4 * 1024 * 1024,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 8,
    });
    let fast = fixture.sub();
    let slow = fixture
        .client
        .subscribe_with_options(
            "state",
            SubscribeOptions {
                start: c2_core::TopicStart::Latest,
                on_lag: c2_core::LagPolicy::Error,
            },
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(15))),
        )
        .unwrap();
    let publisher = fixture.registration.publisher("state").unwrap();
    for seq in 1..=30 {
        assert_eq!(
            publisher.publish(vec![seq as u8; 1024 * 1024]).unwrap(),
            seq
        );
        assert_eq!(fast.next_delivery().unwrap().seq(), seq);
    }
    let mut lagged = false;
    for _ in 0..30 {
        match slow.next_delivery() {
            Ok(_) => {}
            Err(c2_core::Error::Semantic(error))
                if error.code == c2_error::ErrorCode::SubscriptionLagged =>
            {
                lagged = true;
                break;
            }
            Err(error) => panic!("unexpected slow terminal: {error:?}"),
        }
    }
    assert!(lagged, "the slow subscriber should report 720");
    assert_eq!(fixture.registration.active_subscribers(), 1);
    assert_eq!(publisher.publish(b"still running".to_vec()).unwrap(), 31);
    assert_eq!(fast.next_delivery().unwrap().seq(), 31);
    assert!(
        fast.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

#[test]
fn relay_aware_local_ipc_subscriptions_remain_available() {
    let fixture = Fixture::new(Default::default());
    let peer = runtime(&fixture._root);
    peer.set_relay_anchor_address(Some(format!("http://{}", fixture.relay.local_addr())));
    let client = peer
        .connect(
            release().expected_route("topic-route").unwrap(),
            Connect::RelayAware,
        )
        .unwrap();
    assert_eq!(
        client.observed_path(),
        c2_core::ObservedPath::RelayAwareLocalIpc
    );
    let sub = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(b"local".to_vec())
        .unwrap();
    assert_eq!(sub.next_delivery().unwrap().bytes(), b"local");
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

#[cfg(unix)]
#[test]
fn relay_aware_different_namespace_subscribes_over_http() {
    let fixture = Fixture::new(Default::default());
    let other_root = tempfile::Builder::new()
        .prefix("c2-client-topic-")
        .tempdir()
        .unwrap();
    let peer = runtime(&other_root);
    peer.set_relay_anchor_address(Some(format!("http://{}", fixture.relay.local_addr())));
    let client = peer
        .connect(
            release().expected_route("topic-route").unwrap(),
            Connect::RelayAware,
        )
        .unwrap();
    assert_eq!(
        client.observed_path(),
        c2_core::ObservedPath::RelayAwareRelay
    );
    let sub = client
        .subscribe("state", SubscribeOptions::default())
        .unwrap();
    let publisher = fixture.registration.publisher("state").unwrap();
    assert_eq!(publisher.publish(b"relay-aware".to_vec()).unwrap(), 1);
    let delivery = sub.next_delivery().unwrap();
    assert_eq!((delivery.seq(), delivery.bytes()), (1, &b"relay-aware"[..]));
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn late_start_uses_source_head_while_earlier_frame_is_in_flight() {
    let fixture = Fixture::new(Default::default());
    let first = fixture.sub();
    let (entered, release_gate) = fixture.relay.pause_next_topic_frame_for_test();
    let publisher = fixture.registration.publisher("state").unwrap();
    assert_eq!(publisher.publish(b"one".to_vec()).unwrap(), 1);
    entered.blocking_recv().unwrap();
    assert_eq!(publisher.publish(b"two".to_vec()).unwrap(), 2);
    let latest = fixture
        .client
        .subscribe(
            "state",
            SubscribeOptions {
                start: c2_core::TopicStart::Latest,
                on_lag: c2_core::LagPolicy::Skip,
            },
        )
        .unwrap();
    let next = fixture
        .client
        .subscribe(
            "state",
            SubscribeOptions {
                start: c2_core::TopicStart::Next,
                on_lag: c2_core::LagPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(latest.start_seq(), 2);
    assert_eq!(next.start_seq(), 3);
    assert_eq!(fixture.registration.active_subscribers(), 1);
    release_gate.send(()).unwrap();
    assert_eq!(latest.next_delivery().unwrap().seq(), 2);
    assert_eq!(publisher.publish(b"three".to_vec()).unwrap(), 3);
    assert_eq!(next.next_delivery().unwrap().seq(), 3);
    assert!(
        first
            .close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    assert!(
        latest
            .close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    assert!(
        next.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}

#[test]
fn replacement_registration_gets_a_fresh_hub_and_sequence() {
    let mut fixture = Fixture::new(Default::default());
    let old_publisher = fixture.registration.publisher("state").unwrap();
    let old = fixture.sub();
    old_publisher.publish(b"old".to_vec()).unwrap();
    assert_eq!(old.next_delivery().unwrap().bytes(), b"old");
    fixture.registration.close().unwrap();
    cancelled_cause(old.next_delivery().unwrap_err(), "route_unregistered");
    let contract = release();
    let mut replacement = fixture
        .host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                "topic-route",
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
                Arc::new(Echo),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(old_publisher.publish(b"stale".to_vec()).is_err());
    let new = fixture.sub();
    assert_eq!(new.start_seq(), 1);
    assert_eq!(
        replacement
            .publisher("state")
            .unwrap()
            .publish(b"new".to_vec())
            .unwrap(),
        1
    );
    assert_eq!(new.next_delivery().unwrap().bytes(), b"new");
    assert_eq!(replacement.active_subscribers(), 1);
    assert!(
        new.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    wait_until(|| replacement.active_subscribers() == 0);
    replacement.close().unwrap();
}

#[test]
fn host_shutdown_drains_a_frame_already_selected_upstream() {
    let fixture = Fixture::new(Default::default());
    let sub = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    let (entered, release_gate) = fixture.relay.pause_next_topic_frame_for_test();
    publisher.publish(vec![9; 1024 * 1024]).unwrap();
    entered.blocking_recv().unwrap();
    let host = fixture.host.clone();
    let shutdown = std::thread::spawn(move || host.shutdown());
    wait_until(|| publisher.is_closed());
    release_gate.send(()).unwrap();
    let item = sub.next_delivery().unwrap();
    assert_eq!(item.seq(), 1);
    assert_eq!(item.bytes().len(), 1024 * 1024);
    cancelled_cause(sub.next_delivery().unwrap_err(), "host_shutdown");
    let outcome = shutdown.join().unwrap();
    assert!(outcome.route_close_error.is_none(), "{outcome:?}");
}

#[test]
fn relay_shutdown_drains_a_frame_already_selected_upstream() {
    let mut fixture = Fixture::new(Default::default());
    let sub = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    let (entered, release_gate) = fixture.relay.pause_next_topic_frame_for_test();
    let shutdown_signal = fixture.relay.topic_shutdown_signal_for_test();
    publisher.publish(vec![11; 1024 * 1024]).unwrap();
    entered.blocking_recv().unwrap();
    std::thread::scope(|scope| {
        let stop = scope.spawn(|| fixture.relay.stop());
        wait_until(|| shutdown_signal.is_cancelled());
        release_gate.send(()).unwrap();
        let item = sub.next_delivery().unwrap();
        assert_eq!(item.seq(), 1);
        assert_eq!(item.bytes().len(), 1024 * 1024);
        cancelled_cause(sub.next_delivery().unwrap_err(), "relay_shutdown");
        stop.join().unwrap().unwrap();
    });
    assert_eq!(fixture.registration.active_subscribers(), 0);
}

#[test]
fn independent_hub_capacity_pressure_does_not_wait_on_an_idle_latest_frame() {
    let fixture = Fixture::with_topics(
        c2_config::DomainLimits {
            memory_budget_bytes: 1024 * 1024,
            file_spill_budget_bytes: 0,
            max_outstanding_calls: 8,
        },
        &["aux", "state"],
    );
    let first = fixture.sub();
    let second = fixture
        .client
        .subscribe_with_options(
            "aux",
            SubscribeOptions::default(),
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(2))),
        )
        .unwrap();
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(vec![1; 600 * 1024])
        .unwrap();
    assert_eq!(first.next_delivery().unwrap().seq(), 1);
    fixture
        .registration
        .publisher("aux")
        .unwrap()
        .publish(vec![2; 600 * 1024])
        .unwrap();
    match second.next_delivery() {
        Ok(delivery) => assert_eq!(delivery.seq(), 1),
        Err(c2_core::Error::Semantic(error))
            if error.code == c2_error::ErrorCode::CallCapacityExceeded => {}
        Err(error) => panic!("capacity pressure must make progress or report 717: {error:?}"),
    }
    let _ = first.close_with_timeout(Some(Duration::from_secs(5)));
    let _ = second.close_with_timeout(Some(Duration::from_secs(5)));
}

#[test]
fn tiny_relay_budget_does_not_spin_when_chunk_metadata_cannot_fit() {
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: 512,
        file_spill_budget_bytes: 4096,
        max_outstanding_calls: 2,
    });
    let sub = fixture.sub();
    fixture
        .registration
        .publisher("state")
        .unwrap()
        .publish(vec![7; 2048])
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), sub.next_delivery_async()).await
    });
    match result.expect("relay spun while waiting for impossible chunk geometry") {
        Ok(delivery) => assert_eq!(delivery.bytes(), &[7; 2048]),
        Err(c2_core::Error::Semantic(error)) => {
            assert_eq!(error.code, c2_error::ErrorCode::CallCapacityExceeded)
        }
        Err(error) => panic!("unexpected topic outcome: {error:?}"),
    }
    let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
}

#[test]
fn empty_frames_still_charge_relay_history_memory() {
    let fixture = Fixture::new(c2_config::DomainLimits {
        memory_budget_bytes: 8192,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 2,
    });
    let sub = fixture.sub();
    let publisher = fixture.registration.publisher("state").unwrap();
    for seq in 1..=3 {
        assert_eq!(publisher.publish(Vec::new()).unwrap(), seq);
        let item = sub.next_delivery().unwrap();
        assert_eq!(item.seq(), seq);
        assert!(item.bytes().is_empty());
    }
    assert!(fixture.relay.domain_snapshot().memory.used_bytes > 0);
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
}
