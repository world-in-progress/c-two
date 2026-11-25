//! End-to-end two-relay topic fan-out: Host → source relay A → anchor relay B
//! → R clients on B. Proves the cross-machine link carries the payload once,
//! the anchor's local fan-out carries it R times, and ADR 0004 resolution,
//! the merged late-join confirmation, budget isolation and every lifecycle
//! ending behave through both relays.

use c2_config::{CallOptions, CallTimeout, ConfigSources, DomainLimits};
use c2_contract::{ContractRelease, MethodAccess, derive_contract_fingerprints_json};
use c2_core::{
    AdmissionContext, AdmissionDecision, AdmissionHook, AdmissionOperation, AdmissionTransport,
    Connect, EncodedService, HostOptions, LocalEndpointOptions, MethodDefinition, Runtime,
    RuntimeOptions, ServiceDefinition, SubscribeOptions, TopicStart,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::relay::{RelayConfig, RelayServer};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const FRAME: usize = 4 * 1024 * 1024;

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
    let until = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < until, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
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

/// One counting TCP proxy. Bytes are recorded per direction and per
/// connection; the relay-side target can be attached late so a joining relay
/// keeps retrying until the test releases it, and `sever` simulates a
/// machine-level link failure by resetting every connection.
struct ProxyLink {
    address: SocketAddr,
    to_relay: Arc<AtomicU64>,
    from_relay: Arc<AtomicU64>,
    per_connection: Arc<StdMutex<Vec<(Arc<AtomicU64>, Arc<AtomicU64>)>>>,
    severed: Arc<AtomicBool>,
    sever: Arc<tokio::sync::Notify>,
    target: Arc<StdMutex<Option<SocketAddr>>>,
    target_changed: Arc<tokio::sync::Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A write sink that counts every byte it forwards, so live connections are
/// accounted while they are still open.
struct CountingSink<W> {
    inner: W,
    counter: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingSink<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_write(cx, buf)
            .map(|result| {
                if let Ok(count) = &result {
                    self.counter.fetch_add(*count as u64, Ordering::SeqCst);
                    self.total.fetch_add(*count as u64, Ordering::SeqCst);
                }
                result
            })
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl ProxyLink {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut link = Self {
            address,
            to_relay: Arc::new(AtomicU64::new(0)),
            from_relay: Arc::new(AtomicU64::new(0)),
            per_connection: Arc::new(StdMutex::new(Vec::new())),
            severed: Arc::new(AtomicBool::new(false)),
            sever: Arc::new(tokio::sync::Notify::new()),
            target: Arc::new(StdMutex::new(None)),
            target_changed: Arc::new(tokio::sync::Notify::new()),
            thread: None,
        };
        let to_relay = link.to_relay.clone();
        let from_relay = link.from_relay.clone();
        let per_connection = link.per_connection.clone();
        let severed = link.severed.clone();
        let sever = link.sever.clone();
        let target = link.target.clone();
        let target_changed = link.target_changed.clone();
        link.thread = Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let (client, _) = tokio::select! {
                        accepted = listener.accept() => match accepted {
                            Ok(value) => value,
                            Err(_) => continue,
                        },
                        _ = sever.notified() => break,
                    };
                    if severed.load(Ordering::Acquire) {
                        drop(client);
                        continue;
                    }
                    let target = loop {
                        if let Some(target) = *target.lock().unwrap() {
                            break target;
                        }
                        target_changed.notified().await;
                    };
                    if severed.load(Ordering::Acquire) {
                        drop(client);
                        continue;
                    }
                    let to_relay = to_relay.clone();
                    let from_relay = from_relay.clone();
                    let per_connection = per_connection.clone();
                    let sever = sever.clone();
                    tokio::spawn(async move {
                        let server = match tokio::net::TcpStream::connect(target).await {
                            Ok(server) => server,
                            Err(_) => return,
                        };
                        // Bytes are counted as they are forwarded so live
                        // topic connections are observable immediately; the
                        // per-connection record stays readable for the link's
                        // whole lifetime.
                        let conn_up = Arc::new(AtomicU64::new(0));
                        let conn_down = Arc::new(AtomicU64::new(0));
                        per_connection
                            .lock()
                            .unwrap()
                            .push((conn_up.clone(), conn_down.clone()));
                        let (mut client_read, client_write) = client.into_split();
                        let (mut server_read, server_write) = server.into_split();
                        let up_total = to_relay.clone();
                        let up_counter = conn_up.clone();
                        let up = tokio::spawn(async move {
                            let mut sink = CountingSink {
                                inner: server_write,
                                counter: up_counter,
                                total: up_total,
                            };
                            let _ = tokio::io::copy(&mut client_read, &mut sink).await;
                        });
                        let mut down_sink = CountingSink {
                            inner: client_write,
                            counter: conn_down.clone(),
                            total: from_relay.clone(),
                        };
                        let down = tokio::io::copy(&mut server_read, &mut down_sink);
                        tokio::pin!(down);
                        tokio::select! {
                            _ = &mut down => {},
                            // An abrupt link failure: the futures are dropped
                            // without a graceful EOF in either direction.
                            _ = sever.notified() => return,
                        }
                        let _ = up.await;
                    });
                }
            });
            rt.shutdown_timeout(Duration::from_secs(2));
        }));
        link
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn set_target(&self, target: SocketAddr) {
        *self.target.lock().unwrap() = Some(target);
        self.target_changed.notify_waiters();
    }

    fn sever(&self) {
        self.severed.store(true, Ordering::SeqCst);
        self.sever.notify_waiters();
    }

    fn from_relay_bytes(&self) -> u64 {
        self.from_relay.load(Ordering::SeqCst)
    }

    fn data_connections(&self, minimum: u64) -> usize {
        self.per_connection
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, down)| down.load(Ordering::SeqCst) >= minimum)
            .count()
    }

    fn stop(&mut self) {
        self.sever();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProxyLink {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unique_relay_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct DualRelay {
    _root: tempfile::TempDir,
    _host_rt: Runtime,
    host: c2_core::Host,
    registration: c2_core::Registration,
    relay_a: RelayServer,
    relay_b: RelayServer,
    /// The B↔A machine link: A advertises this address, so every cross-relay
    /// topic byte and confirmation travels through it.
    a: ProxyLink,
    /// The clients↔B machine link.
    b: ProxyLink,
    client_rt: Runtime,
}

struct DualRelayLimits {
    a: DomainLimits,
    b: DomainLimits,
    heartbeat_interval: Duration,
    heartbeat_miss_threshold: u32,
    join_deferred: bool,
}

impl Default for DualRelayLimits {
    fn default() -> Self {
        Self {
            a: DomainLimits::default(),
            b: DomainLimits::default(),
            heartbeat_interval: Duration::ZERO,
            heartbeat_miss_threshold: 3,
            join_deferred: false,
        }
    }
}

impl DualRelay {
    fn new() -> Self {
        Self::with_limits(DualRelayLimits::default())
    }

    fn with_limits(limits: DualRelayLimits) -> Self {
        Self::with_limits_and_hook(limits, None)
    }

    fn with_limits_and_hook(limits: DualRelayLimits, hook: Option<Arc<dyn AdmissionHook>>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2-dual-relay-")
            .tempdir()
            .unwrap();
        let host_rt = runtime(&root);
        let context = host_rt.local_endpoint_context().unwrap();
        let a = ProxyLink::start();
        let relay_a = RelayServer::start_resolved(
            RelayConfig {
                bind: "127.0.0.1:0".into(),
                relay_id: unique_relay_id("dual-a"),
                advertise_url: a.url(),
                heartbeat_interval: limits.heartbeat_interval,
                heartbeat_miss_threshold: limits.heartbeat_miss_threshold,
                dead_peer_probe_interval: Duration::ZERO,
                seed_retry_interval: Duration::from_millis(20),
                domain_limits: limits.a,
                ..RelayConfig::default()
            },
            context.clone(),
        )
        .unwrap();
        if !limits.join_deferred {
            a.set_target(relay_a.local_addr());
        }
        let b = ProxyLink::start();
        let relay_b = RelayServer::start_resolved(
            RelayConfig {
                bind: "127.0.0.1:0".into(),
                relay_id: unique_relay_id("dual-b"),
                advertise_url: b.url(),
                // The seed targets A through the counting link; a deferred
                // target keeps B unaware of A until the test releases it.
                seeds: vec![a.url()],
                heartbeat_interval: limits.heartbeat_interval,
                heartbeat_miss_threshold: limits.heartbeat_miss_threshold,
                dead_peer_probe_interval: Duration::ZERO,
                seed_retry_interval: Duration::from_millis(20),
                domain_limits: limits.b,
                ..RelayConfig::default()
            },
            context,
        )
        .unwrap();
        b.set_target(relay_b.local_addr());
        let options = HostOptions::default()
            .with_relay_anchor_address(format!("http://{}", relay_a.local_addr()));
        let options = match hook {
            Some(hook) => options.with_admission_hook(hook),
            None => options,
        };
        let host = host_rt.host(options).unwrap();
        let registration = Self::register_route(&host, "topic-route");
        let client_rt = runtime(&root);
        Self {
            _root: root,
            _host_rt: host_rt,
            host,
            registration,
            relay_a,
            relay_b,
            a,
            b,
            client_rt,
        }
    }

    fn register_route(host: &c2_core::Host, name: &str) -> c2_core::Registration {
        let contract = release();
        host.register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                name,
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
        .unwrap()
    }

    /// One client anchored at B through the counting link. Retries until the
    /// route announcement has propagated into B's mesh view.
    fn client(&self) -> c2_core::Client {
        let expected = release().expected_route("topic-route").unwrap();
        let anchor = self.b.url();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.client_rt.connect(
                expected.clone(),
                Connect::ExplicitRelay {
                    relay_url: anchor.clone(),
                },
            ) {
                Ok(client) => return client,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "connect through B failed: {error:?}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn source_client(&self) -> c2_core::Client {
        self.client_rt
            .connect(
                release().expected_route("topic-route").unwrap(),
                Connect::ExplicitRelay {
                    relay_url: format!("http://{}", self.relay_a.local_addr()),
                },
            )
            .unwrap()
    }

    fn sub(&self, client: &c2_core::Client) -> c2_core::CoreTopicSubscription {
        client
            .subscribe_with_options(
                "state",
                SubscribeOptions::default(),
                CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
            )
            .unwrap()
    }

    fn close_all(&self, subs: &mut [c2_core::CoreTopicSubscription]) {
        for sub in subs {
            let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
        }
    }
}

impl Drop for DualRelay {
    fn drop(&mut self) {
        let _ = self.registration.close();
        let _ = self.host.shutdown();
        let _ = self.relay_a.stop();
        let _ = self.relay_b.stop();
    }
}

/// A raw HTTP/1.1 topic request against one relay, for headers the SDK never
/// sends (loop prevention, mismatched contracts).
/// The default contract headers borrow from a temporary; leak them so the
/// request template can mix default and overridden headers uniformly.
fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn raw_topic_request_bytes(path: &str, extra_headers: &[(&str, &str)]) -> String {
    let expected = release().expected_route("topic-route").unwrap();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\nAccept: {}\r\nx-c2-topic-start: latest\r\nx-c2-topic-on-lag: skip\r\nx-c2-expected-crm-ns: {}\r\nx-c2-expected-crm-name: {}\r\nx-c2-expected-crm-ver: {}\r\n",
        c2_http::topic_wire::MEDIA_TYPE,
        expected.crm_ns,
        expected.crm_name,
        expected.crm_ver,
    );
    let mut extra_headers: Vec<(&str, &str)> = extra_headers.to_vec();
    if !extra_headers
        .iter()
        .any(|(name, _)| *name == "x-c2-expected-abi-hash")
    {
        extra_headers.push((
            "x-c2-expected-abi-hash",
            leak_str(expected.abi_hash.to_string()),
        ));
    }
    if !extra_headers
        .iter()
        .any(|(name, _)| *name == "x-c2-expected-signature-hash")
    {
        extra_headers.push((
            "x-c2-expected-signature-hash",
            leak_str(expected.signature_hash.to_string()),
        ));
    }
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Content-Length: 0\r\n\r\n");
    request
}

fn raw_get_json(address: SocketAddr, path: &str) -> Value {
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP headers");
    serde_json::from_slice(&bytes[header_end + 4..]).expect("valid JSON response")
}

fn raw_topic_request(
    address: SocketAddr,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<u8>) {
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = raw_topic_request_bytes(path, extra_headers);
    stream.write_all(request.as_bytes()).unwrap();
    let mut header_bytes = Vec::new();
    while !header_bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).unwrap();
        header_bytes.push(byte[0]);
    }
    let status: u16 = std::str::from_utf8(&header_bytes[..12])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers_lower = std::str::from_utf8(&header_bytes)
        .unwrap()
        .to_ascii_lowercase();
    // Streaming 200 topic responses never terminate; fail immediately instead of
    // blocking read_to_end or silently returning an empty body inside retry loops.
    // Finite 200 responses (such as /_c2/topics/confirm) do not carry the stream
    // media type and continue through read_to_end below.
    assert!(
        !headers_lower.contains("application/vnd.c-two.topic-stream"),
        "raw_topic_request expects a finite response, got streaming topic response: {}",
        String::from_utf8_lossy(&header_bytes)
    );
    let mut body = Vec::new();
    stream.read_to_end(&mut body).unwrap();
    // A refusal has no chunked body; strip the terminating chunk if present.
    let body = body
        .strip_suffix(b"0\r\n\r\n")
        .map(<[u8]>::to_vec)
        .unwrap_or(body);
    (status, body)
}

/// Send one raw topic subscription and read only the response headers, then
/// drop the connection: a 200 streams indefinitely.
fn raw_topic_response_headers(
    address: SocketAddr,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, String) {
    let (_, headers) = open_raw_topic(address, path, extra_headers);
    (200, headers)
}

fn open_raw_topic(
    address: SocketAddr,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (std::net::TcpStream, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(raw_topic_request_bytes(path, extra_headers).as_bytes())
            .unwrap();
        let mut headers = Vec::new();
        let mut ok = true;
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            match stream.read_exact(&mut byte) {
                Ok(()) => headers.push(byte[0]),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&headers).to_string();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        if ok && status == 200 {
            return (stream, text);
        }
        assert!(
            Instant::now() < deadline,
            "no topic response, status {status}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn response_header<'a>(headers: &'a str, name: &str) -> &'a str {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .unwrap()
        .1
        .trim()
}

#[test]
fn cross_relay_fanout_transfers_payload_once_across_and_r_times_locally() {
    let fixture = DualRelay::new();
    let clients: Vec<_> = (0..3).map(|_| fixture.client()).collect();
    let mut subs: Vec<_> = clients.iter().map(|client| fixture.sub(client)).collect();
    // One upstream subscription serves every B-side reader.
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let before = fixture.a.from_relay_bytes();
    let publisher = fixture.registration.publisher("state").unwrap();
    let frames = [vec![1u8; FRAME], vec![2u8; FRAME]];
    for frame in &frames {
        publisher.publish(frame.clone()).unwrap();
    }
    for sub in &mut subs {
        for frame in &frames {
            let item = sub.next_delivery().unwrap();
            assert_eq!(item.bytes(), frame);
        }
    }
    let across = fixture.a.from_relay_bytes() - before;
    let local = fixture.b.from_relay_bytes();
    let payload = (2 * FRAME) as u64;
    assert!(
        across >= payload && across < payload + 256 * 1024,
        "A→B carried {across} bytes for {payload} bytes of payload"
    );
    assert!(
        local >= 3 * payload && local < 3 * payload + 512 * 1024,
        "B→R carried {local} bytes for 3×{payload} bytes of payload"
    );
    // Exactly one data connection crossed the machine link.
    assert_eq!(fixture.a.data_connections(1024 * 1024), 1);
    fixture.close_all(&mut subs);
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn late_joiners_confirm_in_causal_batches_and_keep_causal_start_points() {
    let fixture = DualRelay::new();
    let first_client = fixture.client();
    let first = fixture.sub(&first_client);
    let publisher = fixture.registration.publisher("state").unwrap();
    publisher.publish(b"one".to_vec()).unwrap();
    publisher.publish(b"two".to_vec()).unwrap();
    assert_eq!(first.next_delivery().unwrap().seq(), 1);
    assert_eq!(first.next_delivery().unwrap().seq(), 2);
    let confirms_before = fixture.relay_a.topic_confirm_count_for_test();
    let across_before = fixture.a.from_relay_bytes();
    // Hold the first confirmation at the source: the second late joiner then
    // arrives while that request is on the wire and must queue for the next
    // causal batch instead of observing the older request's head.
    let (entered, release_confirm) = fixture.relay_a.pause_next_topic_confirm_for_test();
    let clients: Vec<_> = (0..2).map(|_| fixture.client()).collect();
    let mut late = std::thread::scope(|scope| {
        let workers: Vec<_> = clients
            .iter()
            .enumerate()
            .map(|(index, client)| {
                let start = if index == 0 {
                    c2_core::TopicStart::Latest
                } else {
                    c2_core::TopicStart::Next
                };
                scope.spawn(move || {
                    client
                        .subscribe_with_options(
                            "state",
                            SubscribeOptions {
                                start,
                                on_lag: c2_core::LagPolicy::Skip,
                            },
                            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                        )
                        .unwrap()
                })
            })
            .collect();
        entered.blocking_recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        release_confirm.send(()).unwrap();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    // Each batch's own acceptance moment reports head 2 (no frame was
    // published in between): `latest` resumes at frame 2, `next` waits for
    // frame 3. Nothing before it is re-sent.
    assert_eq!(late[0].start_seq(), 2);
    assert_eq!(late[1].start_seq(), 3);
    assert_eq!(
        fixture
            .relay_a
            .topic_confirm_count_for_test()
            .saturating_sub(confirms_before),
        2,
        "one confirmation per causal batch: the leader plus the joiner that arrived in flight"
    );
    let across = fixture.a.from_relay_bytes() - across_before;
    assert!(
        across < 4096,
        "the confirmation must not re-transfer payload bytes, saw {across}"
    );
    assert_eq!(late[0].next_delivery().unwrap().bytes(), b"two");
    publisher.publish(b"three".to_vec()).unwrap();
    assert_eq!(late[1].next_delivery().unwrap().bytes(), b"three");
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let _ = first.close_with_timeout(Some(Duration::from_secs(5)));
    for sub in &mut late {
        let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
    }
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn admission_denied_to_one_subscriber_preserves_local_and_remote_hubs() {
    for at_anchor in [false, true] {
        let (leader_entered, entered) = std::sync::mpsc::channel();
        let (release, leader_release) = std::sync::mpsc::channel();
        let leader_release = StdMutex::new(leader_release);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook = Arc::new(move |context: &AdmissionContext<'_>| {
            assert_eq!(context.operation(), AdmissionOperation::Subscribe);
            assert_eq!(context.transport(), AdmissionTransport::Ipc);
            assert_eq!(context.target(), Some("state"));
            let credential = context.metadata().get("credential").unwrap();
            hook_seen.lock().unwrap().push(credential.to_string());
            match credential {
                "denied-leader" => {
                    leader_entered.send(()).unwrap();
                    leader_release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    AdmissionDecision::deny("leader denied")
                }
                "denied-follower" => AdmissionDecision::deny("follower denied"),
                _ => AdmissionDecision::allow(),
            }
        });
        let fixture = DualRelay::with_limits_and_hook(DualRelayLimits::default(), Some(hook));
        let relay = if at_anchor {
            &fixture.relay_b
        } else {
            &fixture.relay_a
        };
        let client = if at_anchor {
            fixture.client()
        } else {
            fixture.source_client()
        };
        let publisher = fixture.registration.publisher("state").unwrap();
        publisher.publish(b"one".to_vec()).unwrap();
        let subscribe = |credential, start| {
            client
                .with_call_options(
                    CallOptions::new()
                        .with_metadata_entry("credential", credential)
                        .unwrap(),
                )
                .subscribe_with_options(
                    "state",
                    SubscribeOptions {
                        start,
                        ..Default::default()
                    },
                    CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                )
        };
        let mut first = std::thread::scope(|scope| {
            let leader = scope.spawn(|| subscribe("denied-leader", TopicStart::Latest));
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            let waiter = scope.spawn(|| subscribe("allowed-waiter", TopicStart::Next));
            wait_until(|| relay.call_execution_snapshot().used_operations == 2);
            release.send(()).unwrap();
            let error = leader.join().unwrap().unwrap_err();
            assert!(matches!(error, c2_core::Error::Semantic(ref e)
                if e.code == ErrorCode::AdmissionDenied && e.message == "leader denied"));
            waiter.join().unwrap().unwrap()
        });
        assert_eq!(first.start_seq(), 2);
        assert_eq!(fixture.registration.active_subscribers(), 1);
        assert_eq!(fixture.relay_a.topic_confirm_count_for_test(), 0);
        assert_eq!(&*seen.lock().unwrap(), &["denied-leader", "allowed-waiter"]);

        let error = subscribe("denied-follower", TopicStart::Latest).unwrap_err();
        assert!(matches!(error, c2_core::Error::Semantic(ref e)
            if e.code == ErrorCode::AdmissionDenied && e.message == "follower denied"));
        publisher.publish(b"two".to_vec()).unwrap();
        assert_eq!(first.next_delivery().unwrap().bytes(), b"two");
        let mut late = subscribe("allowed-follower", TopicStart::Latest).unwrap();
        assert_eq!(late.start_seq(), 2);
        assert_eq!(late.next_delivery().unwrap().bytes(), b"two");
        assert_eq!(fixture.registration.active_subscribers(), 1);
        assert_eq!(
            fixture.relay_a.topic_confirm_count_for_test(),
            if at_anchor { 2 } else { 0 }
        );
        assert_eq!(
            &*seen.lock().unwrap(),
            &[
                "denied-leader",
                "allowed-waiter",
                "denied-follower",
                "allowed-follower"
            ]
        );
        fixture.close_all(std::slice::from_mut(&mut first));
        fixture.close_all(std::slice::from_mut(&mut late));
        wait_until(|| fixture.registration.active_subscribers() == 0);
    }
}

#[test]
fn deleting_origin_token_while_host_confirms_preserves_source_relay_subscribers() {
    let (hook_entered, entered) = std::sync::mpsc::channel();
    let (release, hook_release) = std::sync::mpsc::channel();
    let hook_release = StdMutex::new(hook_release);
    let hook = Arc::new(move |context: &AdmissionContext<'_>| {
        if context.metadata().get("credential") == Some("blocked") {
            hook_entered.send(()).unwrap();
            hook_release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        AdmissionDecision::allow()
    });
    let fixture = DualRelay::with_limits_and_hook(DualRelayLimits::default(), Some(hook));
    let ordinary = fixture.sub(&fixture.source_client());
    let addr = fixture.relay_a.local_addr();
    let subscribe_path = "/_c2/topics/subscribe?route=topic-route&topic=state";
    let confirm_path = "/_c2/topics/confirm?route=topic-route&topic=state";
    let origin_headers = [("x-c2-relay-origin-only", "1")];
    let (origin, headers) = open_raw_topic(addr, subscribe_path, &origin_headers);
    assert_eq!(
        response_header(&headers, "x-c2-topic-admission-required"),
        "1"
    );
    assert_eq!(response_header(&headers, "x-c2-topic-head-seq"), "0");
    let token = response_header(&headers, "x-c2-call-token");
    let registration = response_header(&headers, "x-c2-topic-registration");
    let wrong_topic = "/_c2/topics/confirm?route=topic-route&topic=other";
    let confirm_headers = [
        ("x-c2-relay-origin-only", "1"),
        ("x-c2-expected-registration", registration),
        ("x-c2-topic-upstream-token", token),
    ];
    let assert_closed = |(status, body): (u16, Vec<u8>)| {
        assert_eq!(status, 503);
        let error = c2_http::decode_control_error(status, &body).unwrap();
        assert_eq!(error.code, ErrorCode::ResourceClosed);
    };
    assert_closed(raw_topic_request(addr, wrong_topic, &confirm_headers));
    std::thread::scope(|scope| {
        let in_flight = scope.spawn(|| {
            let mut headers = confirm_headers.to_vec();
            headers.push(("x-c2-meta-credential", "blocked"));
            raw_topic_request(addr, confirm_path, &headers)
        });
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut delete = std::net::TcpStream::connect(addr).unwrap();
        delete
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            delete,
            "DELETE /_calls/{token} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        delete.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert_closed(raw_topic_request(addr, confirm_path, &confirm_headers));
        release.send(()).unwrap();
        assert_closed(in_flight.join().unwrap());
    });
    drop(origin);

    let (origin, headers) = open_raw_topic(addr, subscribe_path, &origin_headers);
    let mut new_headers = confirm_headers;
    new_headers[2].1 = response_header(&headers, "x-c2-call-token");
    assert_ne!(new_headers[2].1, token);
    assert_eq!(raw_topic_request(addr, confirm_path, &new_headers).0, 200);
    drop(origin);
    wait_until(|| fixture.relay_a.call_execution_snapshot().used_operations == 1);
    assert_closed(raw_topic_request(addr, confirm_path, &new_headers));
    let publisher = fixture.registration.publisher("state").unwrap();
    publisher.publish(b"still live".to_vec()).unwrap();
    assert_eq!(ordinary.next_delivery().unwrap().bytes(), b"still live");
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let _ = ordinary.close_with_timeout(Some(Duration::from_secs(5)));
}

#[test]
fn re_registration_on_the_source_gives_late_joiners_a_fresh_remote_hub() {
    let mut fixture = DualRelay::new();
    let client = fixture.client();
    let old = fixture.sub(&client);
    let old_publisher = fixture.registration.publisher("state").unwrap();
    old_publisher.publish(b"old".to_vec()).unwrap();
    assert_eq!(old.next_delivery().unwrap().bytes(), b"old");
    fixture.registration.close().unwrap();
    cancelled_cause(old.next_delivery().unwrap_err(), "route_unregistered");
    let mut replacement = DualRelay::register_route(&fixture.host, "topic-route");
    // A late joiner must never attach the isolated hub: the client follows the
    // name to the new registration and its sequence restarts at 1.
    let deadline = Instant::now() + Duration::from_secs(10);
    let fresh = loop {
        match client.subscribe("state", SubscribeOptions::default()) {
            Ok(sub) => break sub,
            Err(error) => assert!(
                Instant::now() < deadline,
                "subscribe did not follow the re-registration: {error:?}"
            ),
        }
    };
    assert_eq!(fresh.start_seq(), 1);
    let publisher = replacement.publisher("state").unwrap();
    publisher.publish(b"new".to_vec()).unwrap();
    assert_eq!(fresh.next_delivery().unwrap().bytes(), b"new");
    assert_eq!(replacement.active_subscribers(), 1);
    let _ = fresh.close_with_timeout(Some(Duration::from_secs(5)));
    wait_until(|| replacement.active_subscribers() == 0);
    replacement.close().unwrap();
}

#[test]
fn dual_relay_refusals_keep_canonical_codes_through_the_anchor() {
    let fixture = DualRelay::new();
    let client = fixture.client();
    let error = client
        .subscribe("unknown", SubscribeOptions::default())
        .unwrap_err();
    assert!(
        matches!(error, c2_core::Error::Semantic(ref e) if e.code == c2_error::ErrorCode::ProtocolViolation
            && e.details.get("reason").map(String::as_str) == Some("unknown_topic")),
        "unknown topic must report 713/unknown_topic through the anchor: {error:?}"
    );
    let sub = fixture.sub(&client);
    assert_eq!(sub.start_seq(), 1);
    let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
    // A mismatched contract is refused by B itself, before any forwarding.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = raw_topic_request(
            fixture.relay_b.local_addr(),
            "/_c2/topics/subscribe?route=topic-route&topic=state",
            &[(
                "x-c2-expected-signature-hash",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )],
        );
        if status == 409 {
            let json: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["code"], 709);
            assert_eq!(json["details"]["dispatch_phase"], "pre_dispatch");
            break;
        }
        assert!(Instant::now() < deadline, "no 709 refusal, status {status}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Ordinary clients never see the inter-relay registration fingerprint:
    // neither a direct subscriber on the source relay A nor an anchor-side
    // subscriber on B receives it in the subscription response headers.
    let (_, headers) = raw_topic_response_headers(
        fixture.relay_b.local_addr(),
        "/_c2/topics/subscribe?route=topic-route&topic=state",
        &[],
    );
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("x-c2-topic-registration"),
        "anchor response leaked the registration fingerprint: {headers}"
    );
    let (_, headers) = raw_topic_response_headers(
        fixture.relay_a.local_addr(),
        "/_c2/topics/subscribe?route=topic-route&topic=state",
        &[],
    );
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("x-c2-topic-registration"),
        "source relay response leaked the registration fingerprint: {headers}"
    );
}

#[test]
fn same_host_reregistration_confirm_never_leaks_the_new_head() {
    let mut fixture = DualRelay::new();
    let client_b = fixture.client();
    let old = fixture.sub(&client_b);
    let old_publisher = fixture.registration.publisher("state").unwrap();
    old_publisher.publish(b"old".to_vec()).unwrap();
    assert_eq!(old.next_delivery().unwrap().bytes(), b"old");

    // A late joiner reaches B while its table still describes the original
    // registration; hold its source confirmation at A across a same-Host
    // re-registration (same server instance, new route UID).
    let (entered, release_confirm) = fixture.relay_a.pause_next_topic_confirm_for_test();
    let joiner_client = fixture.client();
    let joiner = std::thread::spawn(move || {
        joiner_client.subscribe_with_options(
            "state",
            SubscribeOptions::default(),
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
        )
    });
    entered.blocking_recv().unwrap();

    fixture.registration.close().unwrap();
    let mut replacement = DualRelay::register_route(&fixture.host, "topic-route");
    // A direct client activates the replacement registration's hub on A and
    // publishes, so a mismatched confirmation has a live head to leak.
    let direct_rt = runtime(&fixture._root);
    let direct = direct_rt
        .connect(
            release().expected_route("topic-route").unwrap(),
            Connect::ExplicitRelay {
                relay_url: format!("http://{}", fixture.relay_a.local_addr()),
            },
        )
        .unwrap();
    let u2 = direct
        .subscribe_with_options(
            "state",
            SubscribeOptions::default(),
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
        )
        .unwrap();
    let u2_publisher = replacement.publisher("state").unwrap();
    u2_publisher.publish(b"new".to_vec()).unwrap();
    assert_eq!(u2.next_delivery().unwrap().bytes(), b"new");

    // The old subscription observes its real Host terminal. Its held
    // confirmation is then refused independently of the replacement's head.
    cancelled_cause(old.next_delivery().unwrap_err(), "route_unregistered");
    release_confirm.send(()).unwrap();
    match joiner.join().unwrap() {
        Err(c2_core::Error::Semantic(error)) => {
            assert_eq!(error.code, ErrorCode::ResourceClosed);
        }
        other => panic!("the stale confirmation must refuse, got {other:?}"),
    }

    // New subscribers follow the name cleanly onto the replacement.
    let newcomer_client = fixture.client();
    let deadline = Instant::now() + Duration::from_secs(10);
    let fresh = loop {
        match newcomer_client.subscribe("state", SubscribeOptions::default()) {
            Ok(sub) => break sub,
            Err(error) => assert!(
                Instant::now() < deadline,
                "subscribe did not follow the replacement: {error:?}"
            ),
        }
    };
    // `latest` starts at the replacement registration's retained frame and
    // follows it from there.
    assert_eq!(fresh.start_seq(), 1);
    assert_eq!(fresh.next_delivery().unwrap().bytes(), b"new");
    u2_publisher.publish(b"second".to_vec()).unwrap();
    assert_eq!(fresh.next_delivery().unwrap().bytes(), b"second");
    assert_eq!(replacement.active_subscribers(), 1);
    let _ = fresh.close_with_timeout(Some(Duration::from_secs(5)));
    let _ = u2.close_with_timeout(Some(Duration::from_secs(5)));
    wait_until(|| replacement.active_subscribers() == 0);
    replacement.close().unwrap();
}

#[test]
fn relay_aware_clients_fan_out_through_the_anchor() {
    let fixture = DualRelay::new();
    // An isolated endpoint root keeps the clients off the Host's local IPC,
    // exactly as cross-machine clients would be, so relay-aware selection
    // enters through the anchor relay B.
    let remote_root = tempfile::Builder::new()
        .prefix("c2-relay-aware-dual-")
        .tempdir()
        .unwrap();
    let rt = Runtime::new(RuntimeOptions {
        relay_anchor_address: Some(fixture.b.url()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    rt.set_local_endpoint_with_sources(
        LocalEndpointOptions {
            #[cfg(unix)]
            unix_root: Some(remote_root.path().to_path_buf()),
            ..Default::default()
        },
        ConfigSources::empty(),
    )
    .unwrap();
    let expected = release().expected_route("topic-route").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let clients: Vec<_> = (0..2)
        .map(|_| {
            loop {
                match rt.connect(expected.clone(), Connect::RelayAware) {
                    Ok(client) => return client,
                    Err(error) => assert!(
                        Instant::now() < deadline,
                        "relay-aware connect through B failed: {error:?}"
                    ),
                }
            }
        })
        .collect();
    let mut subs: Vec<_> = clients.iter().map(|client| fixture.sub(client)).collect();
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let before = fixture.a.from_relay_bytes();
    let publisher = fixture.registration.publisher("state").unwrap();
    let frames = [vec![7u8; FRAME], vec![8u8; FRAME]];
    for frame in &frames {
        publisher.publish(frame.clone()).unwrap();
    }
    for sub in &mut subs {
        for frame in &frames {
            assert_eq!(sub.next_delivery().unwrap().bytes(), frame);
        }
    }
    let across = fixture.a.from_relay_bytes() - before;
    let local = fixture.b.from_relay_bytes();
    let payload = (2 * FRAME) as u64;
    assert!(
        across >= payload && across < payload + 256 * 1024,
        "A→B carried {across} bytes for {payload} bytes of payload"
    );
    assert!(
        local >= 2 * payload && local < 2 * payload + 512 * 1024,
        "B→R carried {local} bytes for 2×{payload} bytes of payload"
    );
    assert_eq!(fixture.a.data_connections(1024 * 1024), 1);
    fixture.close_all(&mut subs);
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn mesh_route_conflict_refuses_remote_subscriptions_pre_dispatch() {
    let fixture = DualRelay::with_limits(DualRelayLimits {
        join_deferred: true,
        ..DualRelayLimits::default()
    });
    // While the relays are still unacquainted, register the same route name on
    // B as well. Joining the mesh afterwards exposes two live registrations.
    let b_host_rt = runtime(&fixture._root);
    let b_host = b_host_rt
        .host(
            HostOptions::default()
                .with_relay_anchor_address(format!("http://{}", fixture.relay_b.local_addr())),
        )
        .unwrap();
    let mut b_registration = DualRelay::register_route(&b_host, "topic-route");
    // Release the join: wait until B's mesh view observes the conflict before
    // sending the topic subscribe request (otherwise an early request before
    // gossip completes would legitimately open a live 200 topic stream on B).
    fixture.a.set_target(fixture.relay_a.local_addr());
    wait_until(|| {
        let json = raw_get_json(fixture.relay_b.local_addr(), "/_routes");
        json["routes"].as_array().is_some_and(|routes| {
            routes
                .iter()
                .any(|route| route["name"] == "topic-route" && route["conflict"] == true)
        })
    });
    let (status, body) = raw_topic_request(
        fixture.relay_b.local_addr(),
        "/_c2/topics/subscribe?route=topic-route&topic=state",
        &[],
    );
    assert_eq!(status, 409);
    let json: Value = serde_json::from_slice(&body).expect("conflict error JSON");
    assert_eq!(json["code"], 719);
    assert_eq!(json["details"]["dispatch_phase"], "pre_dispatch");
    assert_eq!(fixture.registration.active_subscribers(), 0);
    assert_eq!(b_registration.active_subscribers(), 0);
    let _ = b_registration.close();
    let _ = b_host.shutdown();
}

#[test]
fn source_relay_loss_reports_the_route_unavailable() {
    let fixture = DualRelay::with_limits(DualRelayLimits {
        heartbeat_interval: Duration::from_millis(25),
        heartbeat_miss_threshold: 1,
        ..DualRelayLimits::default()
    });
    let client = fixture.client();
    let sub = fixture.sub(&client);
    assert_eq!(sub.start_seq(), 1);
    // Sever the machine link: contact loss, not a graceful leave.
    fixture.a.sever();
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        match client.subscribe("state", SubscribeOptions::default()) {
            Ok(sub) => {
                let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
                panic!("subscriptions must not be accepted while the source is unreachable");
            }
            Err(error) => {
                if matches!(error, c2_core::Error::Semantic(ref e) if e.code == c2_error::ErrorCode::ResourceUnavailable)
                {
                    break error;
                }
                assert!(Instant::now() < deadline, "expected 702, got {error:?}");
            }
        }
    };
    let _ = outcome;
    let _ = sub.close_with_timeout(Some(Duration::from_secs(5)));
}

#[test]
fn source_relay_shutdown_propagates_relay_shutdown_and_drains_the_host() {
    let mut fixture = DualRelay::new();
    let clients: Vec<_> = (0..2).map(|_| fixture.client()).collect();
    let mut subs: Vec<_> = clients.iter().map(|client| fixture.sub(client)).collect();
    fixture.relay_a.stop().unwrap();
    for sub in &mut subs {
        cancelled_cause(sub.next_delivery().unwrap_err(), "relay_shutdown");
    }
    wait_until(|| fixture.registration.active_subscribers() == 0);
    let drained = fixture.relay_a.domain_snapshot();
    assert_eq!(drained.operations.used_operations, 0);
    assert_eq!(drained.memory.used_bytes, 0);
}

#[test]
fn anchor_relay_shutdown_drains_the_source_and_the_host() {
    let mut fixture = DualRelay::new();
    let clients: Vec<_> = (0..2).map(|_| fixture.client()).collect();
    let mut subs: Vec<_> = clients.iter().map(|client| fixture.sub(client)).collect();
    fixture.relay_b.stop().unwrap();
    for sub in &mut subs {
        cancelled_cause(sub.next_delivery().unwrap_err(), "relay_shutdown");
    }
    // B cancelled its cross-relay upstream immediately: A and the Host drain
    // without waiting for B's own stopped downstream readers.
    wait_until(|| fixture.registration.active_subscribers() == 0);
    wait_until(|| {
        let snapshot = fixture.relay_a.domain_snapshot();
        snapshot.operations.used_operations == 0 && snapshot.memory.used_bytes == 0
    });
}

#[test]
fn host_lifecycle_endings_reach_anchor_subscribers() {
    for host_shutdown in [false, true] {
        let mut fixture = DualRelay::new();
        let clients: Vec<_> = (0..2).map(|_| fixture.client()).collect();
        let mut subs: Vec<_> = clients.iter().map(|client| fixture.sub(client)).collect();
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
        for sub in &mut subs {
            cancelled_cause(sub.next_delivery().unwrap_err(), cause);
        }
        wait_until(|| fixture.registration.active_subscribers() == 0);
        for relay in [&fixture.relay_a, &fixture.relay_b] {
            let drained = relay.domain_snapshot();
            assert_eq!(drained.operations.used_operations, 0);
            assert_eq!(drained.memory.used_bytes, 0);
        }
    }
}

#[test]
fn anchor_side_lag_isolation_and_shared_upstream_lifetime() {
    let fixture = DualRelay::with_limits(DualRelayLimits {
        b: DomainLimits {
            memory_budget_bytes: 4 * 1024 * 1024,
            file_spill_budget_bytes: 0,
            max_outstanding_calls: 8,
        },
        ..DualRelayLimits::default()
    });
    let fast_client = fixture.client();
    let slow_client = fixture.client();
    let fast = fixture.sub(&fast_client);
    let slow = slow_client
        .subscribe_with_options(
            "state",
            SubscribeOptions {
                start: c2_core::TopicStart::Latest,
                on_lag: c2_core::LagPolicy::Error,
            },
            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(20))),
        )
        .unwrap();
    let publisher = fixture.registration.publisher("state").unwrap();
    for seq in 1..=30u8 {
        publisher.publish(vec![seq; 1024 * 1024]).unwrap();
        assert_eq!(fast.next_delivery().unwrap().seq(), seq as u64);
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
    assert!(lagged, "the isolated slow subscriber should report 720");
    // The isolated reader never took down the shared cross-relay upstream.
    assert_eq!(fixture.registration.active_subscribers(), 1);
    // Cancelling one reader keeps the upstream for the others.
    let _ = slow.close_with_timeout(Some(Duration::from_secs(5)));
    assert_eq!(fixture.registration.active_subscribers(), 1);
    publisher.publish(b"still running".to_vec()).unwrap();
    assert_eq!(fast.next_delivery().unwrap().bytes(), b"still running");
    let _ = fast.close_with_timeout(Some(Duration::from_secs(5)));
    wait_until(|| fixture.registration.active_subscribers() == 0);
}

#[test]
fn anchor_stopped_tcp_reader_is_evicted_and_the_fast_reader_advances() {
    let frame_size = 4 * 1024 * 1024;
    let fixture = DualRelay::with_limits(DualRelayLimits {
        b: DomainLimits {
            memory_budget_bytes: frame_size + 256 * 1024,
            file_spill_budget_bytes: 0,
            max_outstanding_calls: 4,
        },
        ..DualRelayLimits::default()
    });
    let expected = release().expected_route("topic-route").unwrap();
    let request = format!(
        "POST /_c2/topics/subscribe?route=topic-route&topic=state HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {}\r\nx-c2-topic-start: latest\r\nx-c2-topic-on-lag: skip\r\nx-c2-expected-crm-ns: {}\r\nx-c2-expected-crm-name: {}\r\nx-c2-expected-crm-ver: {}\r\nx-c2-expected-abi-hash: {}\r\nx-c2-expected-signature-hash: {}\r\nContent-Length: 0\r\n\r\n",
        fixture.relay_b.local_addr(),
        c2_http::topic_wire::MEDIA_TYPE,
        expected.crm_ns,
        expected.crm_name,
        expected.crm_ver,
        expected.abi_hash,
        expected.signature_hash
    );
    let headers;
    let mut slow = loop {
        let mut stream = std::net::TcpStream::connect(fixture.relay_b.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut received = Vec::new();
        while !received.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            match stream.read_exact(&mut byte) {
                Ok(()) => received.push(byte[0]),
                Err(_) => break,
            }
        }
        if received.starts_with(b"HTTP/1.1 200") {
            headers = received;
            break stream;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    let fast_client = fixture.client();
    let fast = fixture.sub(&fast_client);
    let publisher = fixture.registration.publisher("state").unwrap();
    publisher.publish(vec![1u8; frame_size as usize]).unwrap();
    // Consume only the READY and ITEM_BEGIN chunks, then stop reading.
    fn http_chunk(stream: &mut std::net::TcpStream) -> usize {
        let mut line = Vec::new();
        while !line.ends_with(b"\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        let size = usize::from_str_radix(std::str::from_utf8(&line[..line.len() - 2]).unwrap(), 16)
            .unwrap();
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).unwrap();
        let mut crlf = [0; 2];
        stream.read_exact(&mut crlf).unwrap();
        assert_eq!(&crlf, b"\r\n");
        size
    }
    http_chunk(&mut slow);
    http_chunk(&mut slow);
    assert_eq!(fast.next_delivery().unwrap().seq(), 1);
    wait_until(|| fixture.relay_b.blocked_topic_subscribers_for_test() > 0);
    for seq in 2..=3u8 {
        publisher.publish(vec![seq; frame_size as usize]).unwrap();
        let item = fast.next_delivery().unwrap();
        assert_eq!(item.seq(), seq as u64);
        assert!(item.bytes().iter().all(|byte| *byte == seq));
    }
    let mut scratch = [0u8; 65536];
    let mut received = Vec::new();
    loop {
        match slow.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&scratch[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(error) => panic!("stalled anchor reader was not terminated: {error}"),
        }
    }
    assert!(!received.is_empty());
    let _ = fast.close_with_timeout(Some(Duration::from_secs(5)));
    wait_until(|| fixture.registration.active_subscribers() == 0);
    wait_until(|| {
        let snapshot = fixture.relay_b.domain_snapshot();
        snapshot.operations.used_operations == 0
    });
}

#[test]
fn anchor_single_slot_boundary_holds_until_the_upstream_cleans_up() {
    let fixture = DualRelay::with_limits(DualRelayLimits {
        b: DomainLimits {
            max_outstanding_calls: 1,
            ..DomainLimits::default()
        },
        ..DualRelayLimits::default()
    });
    let first_client = fixture.client();
    let second_client = fixture.client();
    let sub = fixture.sub(&first_client);
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let error = second_client
        .subscribe("state", SubscribeOptions::default())
        .unwrap_err();
    assert!(
        matches!(error, c2_core::Error::Semantic(ref e) if e.code == c2_error::ErrorCode::CallCapacityExceeded),
        "the single anchor slot must refuse a second subscription: {error:?}"
    );
    assert!(
        sub.close_with_timeout(Some(Duration::from_secs(5)))
            .completed
    );
    wait_until(|| fixture.registration.active_subscribers() == 0);
    // The slot returns only after the cross-relay upstream is cleaned up.
    let replacement = fixture.sub(&second_client);
    assert_eq!(fixture.registration.active_subscribers(), 1);
    let _ = replacement.close_with_timeout(Some(Duration::from_secs(5)));
    wait_until(|| fixture.registration.active_subscribers() == 0);
}
