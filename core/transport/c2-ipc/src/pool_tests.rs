use super::*;
use c2_config::{ConnectOptions, LocalEndpointContext};
use c2_local::{ConnectionClosed, ConnectionEndReason};
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits,
};
use c2_server::{Server, ServerIpcConfig};
use std::collections::HashMap;
use std::thread;
use tokio::io::AsyncReadExt;

const WAIT: Duration = Duration::from_secs(5);

async fn read_handshake_request(stream: &mut c2_local::LocalStream) -> u16 {
    crate::tests::accept_version_preamble(stream).await;
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix).await.unwrap();
    let length = u32::from_le_bytes(prefix) as usize;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    let (_, payload) = c2_wire::frame::decode_frame_body(&body, length as u32).unwrap();
    c2_wire::handshake::decode_handshake(payload, false)
        .unwrap()
        .capability_flags
}

async fn write_handshake_reply(stream: &mut c2_local::LocalStream, capabilities: u16) {
    let response = c2_wire::handshake::encode_server_handshake(
        capabilities,
        &[] as &[c2_wire::handshake::RouteInfo],
        "/pool_peer",
        &c2_wire::handshake::ServerIdentity {
            server_id: String::from("pool-server"),
            server_instance_id: String::from("pool-instance"),
        },
    )
    .unwrap();
    stream
        .write_all(&c2_wire::frame::encode_frame(
            0,
            c2_wire::flags::FLAG_HANDSHAKE | c2_wire::flags::FLAG_RESPONSE,
            &response,
        ))
        .await
        .unwrap();
}

struct Serving {
    runtime: tokio::runtime::Runtime,
    servers: Vec<Arc<Server>>,
    context: LocalEndpointContext,
    _root: tempfile::TempDir,
}

impl Serving {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2pool")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        #[cfg(unix)]
        let context = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            LocalEndpointContext::with_unix_root(root.path()).unwrap()
        };
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
            servers: Vec::new(),
            context,
            _root: root,
        }
    }

    fn start(&mut self, name: &str) -> String {
        let address = format!("ipc://{name}");
        let endpoint = self.context.endpoint(&address).unwrap();
        let server =
            Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
        let runner = Arc::clone(&server);
        self.runtime
            .spawn(async move { runner.run().await.unwrap() });
        self.runtime
            .block_on(server.wait_until_responsive(WAIT))
            .unwrap();
        self.servers.push(server);
        address
    }

    fn stop(&mut self, index: usize) {
        let server = self.servers.remove(index);
        self.runtime
            .block_on(server.shutdown_and_wait(WAIT))
            .unwrap();
    }

    fn pool(&self, grace: Duration) -> ClientPool {
        ClientPool::with_endpoint_context(grace, self.context.clone())
    }

    fn closed(&self, client: &SyncClient) -> ConnectionClosed {
        self.runtime
            .block_on(
                client
                    .connection_observation()
                    .unwrap()
                    .wait_closed(Some(WAIT)),
            )
            .expect("native connection termination")
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        while !self.servers.is_empty() {
            self.stop(0);
        }
    }
}

#[test]
fn eviction_retires_physical_connection_after_pending_work() {
    let mut serving = Serving::new();
    let address = serving.start("eviction_disconnect");
    serving.runtime.block_on(async {
        tokio::time::timeout(WAIT, async {
            while serving.servers[0].active_connection_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    let events = serving.servers[0].connection_events();
    let pool = serving.pool(Duration::ZERO);
    let client = pool.acquire(&address, None).unwrap();
    let observation = serving.runtime.block_on(async {
        match tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            c2_local::ConnectionEvent::Opened(observation) => observation,
            event => panic!("expected accepted connection: {event:?}"),
        }
    });
    assert!(pool.release_if_same(&address, &client));
    pool.sweep_expired();
    let closed = serving
        .runtime
        .block_on(observation.wait_closed(Some(WAIT)))
        .unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::PeerEof);
    assert_eq!(
        serving.closed(&client).reason(),
        ConnectionEndReason::LocalClose
    );
}

#[test]
fn seal_guard_is_reversible_and_deadline_waits_are_bounded() {
    let pool = ClientPool::new(WAIT);
    let before = pool.state.lock().epoch;
    drop(pool.prepare_seal(Instant::now() + WAIT).unwrap());
    assert_eq!(pool.state.lock().epoch, before);
    assert!(matches!(
        pool.acquire("http://invalid", None),
        Err(IpcError::Config(_))
    ));

    let state_guard = pool.state.lock();
    assert!(pool.prepare_seal(Instant::now()).is_none());
    let deadline =
        ConnectDeadline::start(ConnectOptions::new().with_timeout(Duration::from_millis(15)))
            .unwrap();
    let start = Instant::now();
    let error = pool
        .acquire_lease_with_deadline("ipc://blocked", None, deadline)
        .err()
        .expect("state lock wait must expire");
    assert!(start.elapsed() < Duration::from_secs(1), "{error:?}");
    assert!(format!("{error:?}").contains("pool_wait"), "{error:?}");
    drop(state_guard);

    let seed = pool.prepare_seal(Instant::now() + WAIT).unwrap().commit();
    assert!(matches!(
        pool.acquire("ipc://sealed", None),
        Err(IpcError::PoolRetired)
    ));
    let mut observed = pool.observe_closes();
    observed.merge_seed(seed);
    let report = observed.close_all(Duration::from_millis(20));
    assert!(report.error.is_none(), "{report:?}");
    assert!(matches!(
        pool.acquire("ipc://sealed", None),
        Err(IpcError::PoolRetired)
    ));
}

#[test]
fn close_observation_seed_is_pool_bound_and_consumed_once() {
    let first = ClientPool::new(WAIT);
    let second = ClientPool::new(WAIT);
    let seed = first.prepare_seal(Instant::now() + WAIT).unwrap().commit();
    let mut wrong = second.observe_closes();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wrong.merge_seed(seed))).is_err()
    );
    let mut observed = first.observe_closes();
    assert!(
        observed
            .close_all(Duration::from_millis(20))
            .abnormal
            .is_empty()
    );
    assert!(
        observed
            .close_all(Duration::from_millis(20))
            .abnormal
            .is_empty()
    );
}

#[test]
fn original_native_abnormal_end_survives_timeout_and_late_seed_merge() {
    let serving = Serving::new();
    let address = "ipc://abnormal_end".to_string();
    let endpoint = serving.context.endpoint(&address).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (drop_tx, drop_rx) = tokio::sync::oneshot::channel::<()>();
    let peer = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut listener = c2_local::LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                let mut stream = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let capabilities = read_handshake_request(&mut stream).await;
                write_handshake_reply(&mut stream, capabilities).await;
                drop_rx.await.unwrap();
                drop(stream); // Real peer EOF, without a disconnect message.
                crate::tests::close_listener(listener).await;
            });
    });
    ready_rx.recv_timeout(WAIT).unwrap();
    let pool = serving.pool(WAIT);
    let client = pool.acquire(&address, None).unwrap();
    let native = client.connection_observation().unwrap();

    // Keep the original native observation while removing the business cache
    // entry. The peer remains open until after the first bounded observation.
    {
        let mut state = pool.state.lock();
        state.entries.remove(&address).unwrap();
        let record = state.record(&address);
        *record.state.lock() = CloseState::Closing {
            observation: Some(native),
            client: None,
        };
    }
    let seed = pool.prepare_seal(Instant::now() + WAIT).unwrap().commit();
    let duplicate_seed = ClientCloseSeed {
        state: seed.state.clone(),
        observed: seed.observed.clone(),
    };
    let mut observed = pool.observe_closes();
    let timed_out = observed.close_all(Duration::from_millis(20));
    assert_eq!(
        timed_out.unconfirmed,
        vec![address.clone()],
        "{timed_out:?}"
    );
    assert!(timed_out.abnormal.is_empty());

    drop_tx.send(()).unwrap();
    peer.join().unwrap();
    let closed = serving.closed(&client);
    assert_eq!(closed.reason(), ConnectionEndReason::PeerEof);
    observed.merge_seed(seed); // This seal seed arrives after native completion.
    let report = observed.close_all(WAIT);
    assert_eq!(report.abnormal.len(), 1, "{report:?}");
    assert_eq!(report.abnormal[0].address, address);
    assert_eq!(report.abnormal[0].reason, closed.operation());
    observed.merge_seed(duplicate_seed);
    assert!(observed.close_all(WAIT).abnormal.is_empty());
    assert!(pool.observe_closes().close_all(WAIT).abnormal.is_empty());
}

fn in_flight_connect_is_fenced(sealed: bool) {
    let serving = Serving::new();
    let address = format!("ipc://stalled_handshake_{sealed}");
    let endpoint = serving.context.endpoint(&address).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut listener = c2_local::LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                let mut stream = tokio::time::timeout(WAIT, listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let capabilities = read_handshake_request(&mut stream).await;
                request_tx.send(()).unwrap();
                release_rx.await.unwrap();
                write_handshake_reply(&mut stream, capabilities).await;
                let mut probe = [0_u8; 1];
                let ended = tokio::time::timeout(WAIT, stream.read(&mut probe))
                    .await
                    .is_ok();
                let retried = tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_ok();
                closed_tx.send((ended, retried)).unwrap();
                crate::tests::close_listener(listener).await;
            });
    });
    ready_rx.recv_timeout(WAIT).unwrap();
    let pool = Arc::new(serving.pool(WAIT));
    let acquiring = {
        let pool = pool.clone();
        let address = address.clone();
        thread::spawn(move || pool.acquire(&address, None))
    };
    request_rx.recv_timeout(WAIT).unwrap();
    let mut observed = pool.observe_closes();
    if sealed {
        let seed = pool.prepare_seal(Instant::now() + WAIT).unwrap().commit();
        observed.merge_seed(seed);
    }
    let report = observed.close_all(Duration::from_millis(100));
    assert_eq!(report.detached, 0, "{report:?}");
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(
        observed.observed.len(),
        1,
        "the original preparing record is retained"
    );
    assert!(observed.observed.values().all(|record| {
        matches!(
            &*record.as_ref().unwrap().state.lock(),
            CloseState::Preparing
        )
    }));
    if sealed {
        assert_eq!(report.unconfirmed, vec![address.clone()], "{report:?}");
    } else {
        assert!(
            report.unconfirmed.is_empty(),
            "reopenable drain must not wait on connect I/O: {report:?}"
        );
    }
    release_tx.send(()).unwrap();
    let error = acquiring
        .join()
        .unwrap()
        .err()
        .expect("old epoch cannot publish");
    assert!(
        matches!(error, IpcError::Pool(_) | IpcError::PoolRetired),
        "{error:?}"
    );
    let (ended, retried) = closed_rx.recv_timeout(WAIT).unwrap();
    assert!(ended, "losing connection must terminate");
    assert!(!retried, "fenced connect must not retry");
    assert!(observed.close_all(WAIT).unconfirmed.is_empty());
    assert_eq!(pool.active_count(), 0);
    peer.join().unwrap();
}

#[test]
fn in_flight_connect_cannot_publish_across_drain() {
    in_flight_connect_is_fenced(false);
}

#[test]
fn in_flight_connect_cannot_publish_across_permanent_seal() {
    in_flight_connect_is_fenced(true);
}

#[test]
fn same_address_race_closes_losing_native_connection() {
    let serving = Serving::new();
    let address = "ipc://same_address_race".to_string();
    let endpoint = serving.context.endpoint(&address).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut listener = c2_local::LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                let mut streams = Vec::new();
                for _ in 0..2 {
                    let mut stream = tokio::time::timeout(WAIT, listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                    let capabilities = read_handshake_request(&mut stream).await;
                    streams.push((stream, capabilities));
                }
                for (stream, capabilities) in &mut streams {
                    write_handshake_reply(stream, *capabilities).await;
                }
                let watchers: Vec<_> = streams
                    .into_iter()
                    .map(|(mut stream, _)| {
                        let sender = closed_tx.clone();
                        tokio::spawn(async move {
                            let mut prefix = [0_u8; 4];
                            let closed = tokio::time::timeout(
                                Duration::from_secs(2),
                                stream.read_exact(&mut prefix),
                            )
                            .await
                            .is_ok();
                            sender.send(closed).unwrap();
                        })
                    })
                    .collect();
                for watcher in watchers {
                    watcher.await.unwrap();
                }
                crate::tests::close_listener(listener).await;
            });
    });
    ready_rx.recv_timeout(WAIT).unwrap();
    let pool = Arc::new(serving.pool(WAIT));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut acquiring = Vec::new();
    for _ in 0..2 {
        let pool = Arc::clone(&pool);
        let barrier = Arc::clone(&barrier);
        let address = address.clone();
        acquiring.push(thread::spawn(move || {
            barrier.wait();
            pool.acquire(&address, None)
        }));
    }
    let first = acquiring.remove(0).join().unwrap().unwrap();
    let second = acquiring.remove(0).join().unwrap().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(pool.refcount(&address), 2);
    let closed = [
        closed_rx.recv_timeout(WAIT).unwrap(),
        closed_rx.recv_timeout(WAIT).unwrap(),
    ];
    assert_eq!(
        closed.into_iter().filter(|closed| *closed).count(),
        1,
        "{closed:?}"
    );
    peer.join().unwrap();
}

#[test]
fn concurrent_drains_serialize_and_reject_acquires_with_aggregate_deadline() {
    let mut serving = Serving::new();
    let a = serving.start("drain_a");
    let b = serving.start("drain_b");
    let pool = Arc::new(serving.pool(WAIT));
    let mut retained = Vec::new();
    for address in [&a, &b] {
        let client = pool.acquire(address, None).unwrap();
        let native = client.connection_observation().unwrap();
        let mut state = pool.state.lock();
        state.entries.remove(address).unwrap();
        let record = state.record(address);
        *record.state.lock() = CloseState::Closing {
            observation: Some(native),
            client: None,
        };
        retained.push(client);
    }
    let draining_pool = Arc::clone(&pool);
    let start = Instant::now();
    let first = thread::spawn(move || draining_pool.close_all(Duration::from_millis(200)));
    let until = Instant::now() + WAIT;
    while !pool.state.lock().draining && Instant::now() < until {
        thread::yield_now();
    }
    assert!(pool.state.lock().draining, "first drain started");
    assert!(matches!(pool.acquire(&a, None), Err(IpcError::Pool(_))));
    let second = pool.close_all(Duration::from_millis(20));
    assert!(
        second
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("concurrent cache drain"),
        "{second:?}"
    );
    let first = first.join().unwrap();
    assert_eq!(first.unconfirmed.len(), 2, "{first:?}");
    assert!(
        start.elapsed() < Duration::from_millis(380),
        "one deadline covers all records"
    );
    assert_eq!(pool.state.lock().epoch, Some(1));
    drop(retained);
}

#[test]
fn real_expiry_discard_and_drain_close_exact_connections() {
    let mut serving = Serving::new();
    let a = serving.start("pool_a");
    let b = serving.start("pool_b");
    let pool = serving.pool(Duration::from_millis(20));
    let first = pool
        .acquire_lease_with_deadline(&a, None, ConnectDeadline::default())
        .unwrap();
    let first_client = first.client().clone();
    let active = pool
        .acquire_lease_with_deadline(&b, None, ConnectDeadline::default())
        .unwrap();
    drop(first);
    thread::sleep(Duration::from_millis(30));
    let replacement = pool
        .acquire_lease_with_deadline(&a, None, ConnectDeadline::default())
        .unwrap();
    assert!(!Arc::ptr_eq(&first_client, replacement.client()));
    assert_eq!(
        serving.closed(&first_client).reason(),
        ConnectionEndReason::LocalClose
    );
    assert!(active.is_connected());
    assert!(!pool.discard_if_same(&a, &first_client));
    assert!(pool.discard_if_same(&a, replacement.client()));
    assert_eq!(
        serving.closed(replacement.client()).reason(),
        ConnectionEndReason::LocalClose
    );
    assert!(active.is_connected());

    let idle = pool
        .acquire_lease_with_deadline(&a, None, ConnectDeadline::default())
        .unwrap();
    let idle_client = idle.client().clone();
    drop(idle);
    thread::sleep(Duration::from_millis(30));
    pool.sweep_expired();
    assert_eq!(
        serving.closed(&idle_client).reason(),
        ConnectionEndReason::LocalClose
    );
    assert!(active.is_connected());

    let report = pool.close_all(WAIT);
    assert_eq!(report.detached, 1, "{report:?}");
    assert!(
        report.error.is_none() && report.unconfirmed.is_empty(),
        "{report:?}"
    );
    assert_eq!(
        serving.closed(&active).reason(),
        ConnectionEndReason::LocalClose
    );
    let fresh = pool
        .acquire_lease_with_deadline(&b, None, ConnectDeadline::default())
        .unwrap();
    assert!(!Arc::ptr_eq(active.client(), fresh.client()));
    assert!(pool.close_all(WAIT).error.is_none());
}

#[test]
fn close_all_closes_active_and_idle_connections_then_opens_fresh_epoch() {
    let mut serving = Serving::new();
    let a = serving.start("close_active");
    let b = serving.start("close_idle");
    let pool = serving.pool(WAIT);
    let active = pool
        .acquire_lease_with_deadline(&a, None, ConnectDeadline::default())
        .unwrap();
    let idle = pool
        .acquire_lease_with_deadline(&b, None, ConnectDeadline::default())
        .unwrap();
    let idle_client = idle.client().clone();
    drop(idle);
    assert_eq!(pool.refcount(&a), 1);
    assert_eq!(pool.refcount(&b), 0);
    let report = pool.close_all(WAIT);
    assert_eq!(report.detached, 2, "{report:?}");
    assert!(
        report.error.is_none() && report.unconfirmed.is_empty(),
        "{report:?}"
    );
    assert_eq!(
        serving.closed(&active).reason(),
        ConnectionEndReason::LocalClose
    );
    assert_eq!(
        serving.closed(&idle_client).reason(),
        ConnectionEndReason::LocalClose
    );
    let fresh = pool
        .acquire_lease_with_deadline(&a, None, ConnectDeadline::default())
        .unwrap();
    assert!(!Arc::ptr_eq(active.client(), fresh.client()));
    assert!(pool.close_all(WAIT).error.is_none());
}

#[test]
fn finite_budget_is_shared_within_one_cache_and_isolated_across_caches() {
    let mut serving = Serving::new();
    let a = serving.start("budget_a");
    let b = serving.start("budget_b");
    let config = ClientIpcConfig {
        base: c2_config::BaseIpcConfig {
            shm_backing_budget_bytes: 65_536,
            file_backing_budget_bytes: 32_768,
            live_reassembly_budget_bytes: 16_384,
            ..Default::default()
        },
        ..Default::default()
    };
    let pool = serving.pool(WAIT);
    let first = pool
        .acquire_lease_with_deadline(&a, Some(&config), ConnectDeadline::default())
        .unwrap();
    let second = pool
        .acquire_lease_with_deadline(&b, Some(&config), ConnectDeadline::default())
        .unwrap();
    assert_eq!(pool.active_count(), 2);
    let observer = pool.memory_budget_observer().unwrap();
    assert_eq!(observer.limits(), config.memory_budget_limits());
    assert!(first.is_connected() && second.is_connected());

    let other = serving.pool(WAIT);
    let independent = other
        .acquire_lease_with_deadline(&a, Some(&config), ConnectDeadline::default())
        .unwrap();
    assert!(independent.is_connected());
    assert!(
        !observer
            .downgrade()
            .ptr_eq(&other.memory_budget_observer().unwrap().downgrade())
    );
    let shared_budget = pool.state.lock().budget.clone().unwrap();
    let reservation = shared_budget
        .reserve(c2_mem::BudgetKind::File, 32_768)
        .unwrap();
    assert_eq!(observer.snapshot().file.used_bytes, 32_768);
    assert!(shared_budget.reserve(c2_mem::BudgetKind::File, 1).is_err());
    let independent_budget = other.state.lock().budget.clone().unwrap();
    let independent_reservation = independent_budget
        .reserve(c2_mem::BudgetKind::File, 32_768)
        .unwrap();
    drop(independent_reservation);
    drop(reservation);
    assert!(other.close_all(WAIT).error.is_none());

    let mut changed = config.clone();
    changed.base.shm_backing_budget_bytes += 1;
    assert!(pool.set_default_config(changed.clone()).is_err());
    assert!(pool.acquire(&a, Some(&changed)).is_err());
    let report = pool.close_all(WAIT);
    assert_eq!(report.detached, 2, "{report:?}");
    assert_eq!(
        pool.memory_budget_observer().unwrap().snapshot(),
        observer.snapshot()
    );
}

#[test]
fn cache_hit_requires_full_resolved_policy() {
    let mut serving = Serving::new();
    let address = serving.start("policy");
    let pool = serving.pool(WAIT);
    let config = ClientIpcConfig::default();
    let first = pool.acquire(&address, Some(&config)).unwrap();
    assert!(Arc::ptr_eq(
        &first,
        &pool.acquire(&address, Some(&config)).unwrap()
    ));
    let mut variants = Vec::new();
    let mut threshold = config.clone();
    threshold.shm_threshold += 1;
    variants.push(("shm_threshold", threshold));
    let mut chunk = config.clone();
    chunk.base.chunk_size += 1;
    variants.push(("chunk_size", chunk));
    let mut decay = config.clone();
    decay.pool_decay_seconds += 1.0;
    variants.push(("pool_decay_seconds", decay));
    for (field, requested) in variants {
        assert_eq!(
            requested.memory_budget_limits(),
            config.memory_budget_limits()
        );
        let error = pool.acquire(&address, Some(&requested)).err().unwrap();
        assert!(error.to_string().contains(field), "{error:?}");
        assert!(Arc::ptr_eq(
            &first,
            &pool.acquire(&address, Some(&config)).unwrap()
        ));
    }
    assert_eq!(pool.close_all(WAIT).detached, 1);
}

struct Echo;
impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::Inline(b"ok".to_vec()))
    }
}

#[test]
fn registration_attestation_accepts_committed_closed_route() {
    let mut serving = Serving::new();
    let address = "ipc://closed_route";
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test.pool".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "01".repeat(32),
        signature_hash: "02".repeat(32),
    };
    let endpoint = serving.context.endpoint(address).unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let built = server
        .build_route(
            RouteBuildSpec {
                name: expected.route_name.clone(),
                crm_ns: expected.crm_ns.clone(),
                crm_name: expected.crm_name.clone(),
                crm_ver: expected.crm_ver.clone(),
                abi_hash: expected.abi_hash.clone(),
                signature_hash: expected.signature_hash.clone(),
                method_names: vec!["ping".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::ReadParallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(Echo),
        )
        .unwrap();
    let admission = serving.runtime.block_on(async {
        let reservation = server.reserve_route(built).await.unwrap();
        server
            .commit_reserved_route_closed(reservation)
            .await
            .unwrap()
    });
    let runner = Arc::clone(&server);
    serving
        .runtime
        .spawn(async move { runner.run().await.unwrap() });
    serving
        .runtime
        .block_on(server.wait_until_responsive(WAIT))
        .unwrap();
    serving.servers.push(Arc::clone(&server));
    let mut client = crate::client::IpcClient::with_endpoint(
        serving.context.endpoint(address).unwrap(),
        ClientIpcConfig::default(),
    )
    .unwrap();
    serving.runtime.block_on(async {
        client.connect().await.unwrap();
        assert!(matches!(
            client.acquire_route(&expected).await,
            Err(IpcError::RouteClosed { .. })
        ));
        assert_eq!(
            client
                .attest_route_for_registration(&expected)
                .await
                .unwrap()
                .route_name(),
            "grid"
        );
        server.open_route_admission(admission).await.unwrap();
        client.close().await;
    });
}

struct PausedEcho {
    entered: std::sync::mpsc::Sender<()>,
    resume: parking_lot::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl CrmCallback for PausedEcho {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        input: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if input == b"old" {
            self.entered.send(()).unwrap();
            self.resume.lock().recv_timeout(WAIT).unwrap();
        }
        Ok(ResponseMeta::Inline(input))
    }
}

fn retired_pool_client_preserves_sent_reply(discard: bool, force_close: bool) {
    let mut serving = Serving::new();
    let address = if force_close {
        "ipc://pool_close_retired_sent_reply"
    } else if discard {
        "ipc://pool_discard_sent_reply"
    } else {
        "ipc://pool_retirement_sent_reply"
    };
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test.pool".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "01".repeat(32),
        signature_hash: "02".repeat(32),
    };
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let endpoint = serving.context.endpoint(address).unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let built = server
        .build_route(
            RouteBuildSpec {
                name: expected.route_name.clone(),
                crm_ns: expected.crm_ns.clone(),
                crm_name: expected.crm_name.clone(),
                crm_ver: expected.crm_ver.clone(),
                abi_hash: expected.abi_hash.clone(),
                signature_hash: expected.signature_hash.clone(),
                method_names: vec!["ping".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(PausedEcho {
                entered: entered_tx,
                resume: parking_lot::Mutex::new(resume_rx),
            }),
        )
        .unwrap();
    serving.runtime.block_on(async {
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
    });
    let runner = server.clone();
    serving
        .runtime
        .spawn(async move { runner.run().await.unwrap() });
    serving
        .runtime
        .block_on(server.wait_until_responsive(WAIT))
        .unwrap();
    serving.servers.push(server);

    let pool = serving.pool(WAIT);
    let old = pool.acquire(address, None).unwrap();
    let old_binding = old.acquire_route(&expected).unwrap();
    let old_observation = old.connection_observation().unwrap();
    let old_call = {
        let old = old.clone();
        thread::spawn(move || old.call_bound_phased(&old_binding, "ping", b"old"))
    };
    entered_rx.recv_timeout(WAIT).unwrap();
    if discard {
        assert!(pool.discard_if_same(address, &old));
    } else {
        old.retire();
    }
    assert!(!old.is_connected());
    let replacement = pool.acquire(address, None).unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    let new_binding = replacement.acquire_route(&expected).unwrap();
    assert_eq!(
        replacement
            .call_bound(&new_binding, "ping", b"new")
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"new"
    );
    assert!(old_observation.snapshot().is_none());
    if force_close {
        let report = pool.close_all(WAIT);
        assert!(
            report.error.is_none() && report.unconfirmed.is_empty(),
            "{report:?}"
        );
        let error = old_call.join().unwrap().unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
        assert!(matches!(error.source_error(), IpcError::Closed));
        let _ = resume_tx.send(());
    } else {
        resume_tx.send(()).unwrap();
        assert_eq!(
            old_call
                .join()
                .unwrap()
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"old"
        );
    }
    assert!(
        serving
            .runtime
            .block_on(old_observation.wait_closed(Some(WAIT)))
            .is_some()
    );
    if !force_close {
        assert!(pool.close_all(WAIT).unconfirmed.is_empty());
    }
}

#[test]
fn replacement_after_retirement_preserves_sent_reply_on_old_connection() {
    retired_pool_client_preserves_sent_reply(false, false);
}

#[test]
fn discard_preserves_sent_reply_on_old_connection() {
    retired_pool_client_preserves_sent_reply(true, false);
}

#[test]
fn close_all_aborts_sent_calls_on_previously_retired_clients() {
    retired_pool_client_preserves_sent_reply(false, true);
}
