//! Public Relay stop must observe native request-only cleanup after delivery.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c2_config::{ClientIpcConfig, RelayConfig};
use c2_mem::MemPool;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
    RouteBuildSpec, SchedulerLimits, Server, ServerIdentity, ServerIpcConfig,
};
use futures::FutureExt;
use tokio::sync::{Notify, oneshot};

use super::server::RelayServer;
use super::test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH};

const STEP: Duration = Duration::from_secs(10);

struct InlineReply {
    entered: Notify,
    release: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
impl CrmCallback for InlineReply {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        input: RequestData,
        _: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        assert!(matches!(
            &input,
            RequestData::Shm {
                is_dedicated: true,
                ..
            }
        ));
        let mut owner = RequestLease::new(input);
        assert_eq!(
            owner.copy_bytes().map_err(CrmError::InternalError)?,
            vec![7; 8192]
        );
        self.entered.notify_one();
        if let Some(release) = self.release.lock().take() {
            release
                .recv_timeout(STEP)
                .map_err(|e| CrmError::InternalError(e.to_string()))?;
        }
        owner.release().map_err(CrmError::InternalError)?;
        Ok(ResponseMeta::Inline(vec![9; 32]))
    }
}

// All lock-holder threads have a finite timeout and an unconditional release/join.
struct PoolHolder {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl PoolHolder {
    async fn start(pool: Arc<parking_lot::Mutex<MemPool>>) -> Self {
        let (ready, wait) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _guard = pool.lock();
            let _ = ready.send(());
            let _ = released.recv_timeout(STEP);
        });
        let holder = Self {
            release: Some(release),
            thread: Some(thread),
        };
        tokio::time::timeout(STEP, wait).await.unwrap().unwrap();
        holder
    }
}
impl Drop for PoolHolder {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

async fn public_stop_cleanup_case(acquire_failure: bool) {
    let socket = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let id = format!(
        "relay_stop_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let address = format!("ipc://{id}");
    let mut ipc = ClientIpcConfig::default();
    ipc.base.pool_enabled = false;
    ipc.base.pool_prewarm_segments = 0;
    ipc.base.shm_backing_budget_bytes = 1024 * 1024;
    ipc.shm_threshold = 1;
    ipc.validate().unwrap();
    let mut server_config = ServerIpcConfig::default();
    server_config.base = ipc.base.clone();
    server_config.max_execution_workers = 2;
    let server = Arc::new(
        Server::new_with_identity(
            &address,
            server_config,
            ServerIdentity {
                server_id: id.clone(),
                server_instance_id: format!("{id}-instance"),
            },
        )
        .unwrap(),
    );
    let (release, wait) = std::sync::mpsc::channel();
    let probe = Arc::new(InlineReply {
        entered: Notify::new(),
        release: parking_lot::Mutex::new(Some(wait)),
    });
    let route = server
        .build_route(
            RouteBuildSpec {
                name: "grid".into(),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                method_names: vec!["target".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            probe.clone(),
        )
        .unwrap();
    let route = server.reserve_route(route).await.unwrap();
    server.commit_reserved_route(route).await.unwrap();
    let ipc_task = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    let mut relay = None;
    let result = std::panic::AssertUnwindSafe(async {
        server
            .wait_until_responsive(Duration::from_secs(2))
            .await
            .unwrap();
        let config = RelayConfig {
            bind: socket.to_string(),
            advertise_url: format!("http://{socket}"),
            idle_timeout_secs: 0,
            upstream_ipc: ipc,
            ..RelayConfig::default()
        };
        let address = address.clone();
        let id = id.clone();
        let running = tokio::task::spawn_blocking(move || {
            let relay = RelayServer::start(config).unwrap();
            relay.register_upstream("grid", &id, &address).unwrap();
            relay
        })
        .await
        .unwrap();
        let state = running.state_for_test();
        relay = Some(running);
        let entry = state.local_route("grid").unwrap();
        let (lease, _, _binding) = state
            .acquire_upstream_for_route(&entry)
            .await
            .unwrap_or_else(|_| panic!("attested acquire"));
        let client = lease.client();
        drop(lease);
        let call_entry = entry.clone();
        let call = tokio::spawn(async move {
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("http://{socket}/grid/target"))
                .header("x-c2-expected-crm-ns", "test.echo")
                .header("x-c2-expected-crm-name", "Echo")
                .header("x-c2-expected-crm-ver", "0.1.0")
                .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
                .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
                .header("x-c2-route-uid", &call_entry.route_uid)
                .header("x-c2-route-revision", call_entry.route_revision.to_string())
                .body(vec![7; 8192])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            response.bytes().await.unwrap()
        });
        tokio::time::timeout(STEP, probe.entered.notified())
            .await
            .unwrap();
        let holder =
            PoolHolder::start(client.request_pool_for_test().expect("actual request pool")).await;
        release.send(()).unwrap();
        let response = tokio::time::timeout(STEP, call).await.unwrap().unwrap();
        assert_eq!(response.as_ref(), &[9; 32]);
        assert_eq!(
            client.pending_len_for_test(),
            1,
            "delivery is not settlement under busy request pool"
        );
        assert_eq!(state.forwarding.snapshot().used_operations, 0);
        if acquire_failure {
            // Remove only the actual upstream route, with watch stopped so the
            // Relay keeps its captured metadata. Route-token acquisition must
            // fail and evict this exact client while cleanup remains busy.
            state.stop_upstream_controls().await;
            assert!(server.unregister_route("grid").await);
            let key = super::upstream_control::owner_key_for_route(&entry).unwrap();
            state.mark_upstream_control_watch_unavailable(&key, "controlled watcher absence");
            assert!(state.acquire_upstream_for_route(&entry).await.is_err());
            assert!(state.local_route("grid").is_some());
            assert_eq!(client.pending_len_for_test(), 1);
            assert!(state.clients.outstanding() > 0);
        }
        let mut running = relay.take().unwrap();
        let mut stopping = tokio::task::spawn_blocking(move || {
            running.stop().unwrap();
            running
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut stopping)
                .await
                .is_err(),
            "public stop must retain native cleanup observer while the exact owner lock is busy"
        );
        assert!(state.clients.outstanding() > 0);
        drop(holder);
        let _stopped = tokio::time::timeout(STEP, stopping).await.unwrap().unwrap();
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(state.clients.outstanding(), 0);
        assert!(client.request_pool_for_test().is_none());
    })
    .catch_unwind()
    .await;
    // Callback waits and listener tasks are finite even on assertion failure.
    drop(probe);
    if let Some(mut running) = relay {
        tokio::task::spawn_blocking(move || running.stop())
            .await
            .unwrap()
            .unwrap();
    }
    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(STEP, ipc_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_stop_observes_successful_request_cleanup_under_exact_pool_lock() {
    public_stop_cleanup_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acquire_failure_unconfirmed_cleanup_remains_observed_through_public_stop() {
    public_stop_cleanup_case(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_stop_fences_real_http_registration_control_watch_publication() {
    let socket = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let id = format!(
        "relay_register_stop_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let address = format!("ipc://{id}");
    let server = Arc::new(
        Server::new_with_identity(
            &address,
            ServerIpcConfig::default(),
            ServerIdentity {
                server_id: id.clone(),
                server_instance_id: format!("{id}-instance"),
            },
        )
        .unwrap(),
    );
    super::test_support::register_echo_route(&server, "grid").await;
    let ipc_task = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    let mut relay = None;
    let mut cleanup_state = None;
    let mut registration_task = None;
    let mut stopping_task = None;
    let mut release_registration = None;
    let mut release_stop = None;
    let result = std::panic::AssertUnwindSafe(async {
        server
            .wait_until_responsive(Duration::from_secs(2))
            .await
            .unwrap();
        let config = RelayConfig {
            bind: socket.to_string(),
            advertise_url: format!("http://{socket}"),
            idle_timeout_secs: 0,
            ..RelayConfig::default()
        };
        let running = tokio::task::spawn_blocking(move || RelayServer::start(config).unwrap())
            .await
            .unwrap();
        let state = running.state_for_test();
        cleanup_state = Some(state.clone());
        relay = Some(running);
        let (registration_entered, registration_wait) = oneshot::channel();
        let (registration_resume, registration_resume_wait) = oneshot::channel();
        state.set_registration_watch_seam_for_test(registration_entered, registration_resume_wait);
        release_registration = Some(registration_resume);
        let (stop_entered, stop_wait) = oneshot::channel();
        let (stop_resume, stop_resume_wait) = oneshot::channel();
        state.set_controls_stop_seam_for_test(stop_entered, stop_resume_wait);
        release_stop = Some(stop_resume);
        let registration = serde_json::json!({
            "name": "grid",
            "address": address,
            "server_id": id,
            "server_instance_id": server.server_instance_id(),
            "max_payload_size": server.config().max_payload_size,
        });
        registration_task = Some(tokio::spawn(async move {
            let response = reqwest::Client::builder()
                .no_proxy()
                .timeout(STEP)
                .build()
                .unwrap()
                .post(format!("http://{socket}/_register"))
                .json(&registration)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::CREATED);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap(),
                serde_json::json!({"registered": "grid"})
            );
        }));
        // The actual HTTP handler has committed/attested its route, but has
        // not yet started the persistent control task. Do not infer handler
        // death from listener cancellation: explicitly resume and join it.
        tokio::time::timeout(STEP, registration_wait)
            .await
            .unwrap()
            .unwrap();
        assert!(state.local_route("grid").is_some());
        assert_eq!(state.controls_snapshot_for_test(), (false, 0));
        let mut running = relay.take().unwrap();
        stopping_task = Some(tokio::task::spawn_blocking(move || {
            running.stop().unwrap();
            running
        }));
        tokio::time::timeout(STEP, stop_wait)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.controls_snapshot_for_test(), (true, 0));
        release_registration.take().unwrap().send(()).unwrap();
        tokio::time::timeout(STEP, registration_task.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap();
        registration_task.take();
        assert_eq!(
            state.controls_snapshot_for_test(),
            (true, 0),
            "a completed real HTTP handler cannot publish after the stop fence"
        );
        assert!(!stopping_task.as_ref().unwrap().is_finished());
        release_stop.take().unwrap().send(()).unwrap();
        let _stopped = tokio::time::timeout(STEP, stopping_task.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap();
        stopping_task.take();
        assert_eq!(state.controls_snapshot_for_test(), (true, 0));
        assert_eq!(state.clients.outstanding(), 0);
    })
    .catch_unwind()
    .await;
    // Release both explicit synchronization points before cleanup, including
    // panic paths. Every callback/client operation and join has a deadline.
    if let Some(release) = release_registration.take() {
        let _ = release.send(());
    }
    if let Some(release) = release_stop.take() {
        let _ = release.send(());
    }
    if let Some(mut task) = registration_task.take() {
        if tokio::time::timeout(STEP, &mut task).await.is_err() {
            task.abort();
            let _ = tokio::time::timeout(STEP, &mut task).await.unwrap();
        }
    }
    // A buggy publication model can create a watcher after stop's first
    // snapshot. Explicitly reap it on the panic path before joining stop, so
    // failure demonstrates the assertion without stranding the stop thread.
    if result.is_err() {
        if let Some(state) = cleanup_state.as_ref() {
            tokio::time::timeout(STEP, state.stop_upstream_controls())
                .await
                .unwrap();
        }
    }
    if let Some(task) = stopping_task.take() {
        let _ = tokio::time::timeout(STEP, task).await.unwrap();
    }
    if let Some(mut running) = relay.take() {
        tokio::time::timeout(STEP, tokio::task::spawn_blocking(move || running.stop()))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(STEP, ipc_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_acquire_disconnects_exact_old_client_before_same_endpoint_replacement() {
    let socket = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let id = format!(
        "relay_acquire_stale_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let address = format!("ipc://{id}");
    let server = Arc::new(
        Server::new_with_identity(
            &address,
            ServerIpcConfig::default(),
            ServerIdentity {
                server_id: id.clone(),
                server_instance_id: format!("{id}-instance"),
            },
        )
        .unwrap(),
    );
    super::test_support::register_echo_route(&server, "grid").await;
    let ipc_task = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    let mut relay = None;
    let mut acquire_task = None;
    let mut resume_acquire = None;
    let result = std::panic::AssertUnwindSafe(async {
        server
            .wait_until_responsive(Duration::from_secs(2))
            .await
            .unwrap();
        let config = RelayConfig {
            bind: socket.to_string(),
            advertise_url: format!("http://{socket}"),
            idle_timeout_secs: 0,
            ..RelayConfig::default()
        };
        let register_address = address.clone();
        let running = tokio::task::spawn_blocking(move || {
            let relay = RelayServer::start(config).unwrap();
            relay
                .register_upstream("grid", &id, &register_address)
                .unwrap();
            relay
        })
        .await
        .unwrap();
        let state = running.state_for_test();
        relay = Some(running);
        // Keep the controlled table transition independent of watch delivery.
        tokio::time::timeout(STEP, state.stop_upstream_controls())
            .await
            .unwrap();
        let old_entry = state.local_route("grid").unwrap();
        let (lease, _, _) =
            tokio::time::timeout(STEP, state.acquire_upstream_for_route(&old_entry))
                .await
                .unwrap()
                .unwrap_or_else(|_| panic!("initial real acquisition"));
        let old_client = lease.client();
        drop(lease);
        assert!(old_client.is_connected());
        let (entered, wait) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        state.set_acquire_recheck_seam_for_test(entered, resumed);
        resume_acquire = Some(resume);
        let acquiring_state = state.clone();
        let acquiring_entry = old_entry.clone();
        acquire_task = Some(tokio::spawn(async move {
            acquiring_state
                .acquire_upstream_for_route(&acquiring_entry)
                .await
        }));
        tokio::time::timeout(STEP, wait).await.unwrap().unwrap();
        assert!(server.unregister_route("grid").await);
        super::test_support::register_echo_route(&server, "grid").await;
        let (new_uid, new_revision) = server.registered_route_identity("grid").unwrap();
        assert_ne!(
            (&new_uid, new_revision),
            (&old_entry.route_uid, old_entry.route_revision)
        );
        let mut new_entry = old_entry.clone();
        new_entry.route_uid = new_uid;
        new_entry.route_revision = new_revision;
        assert_eq!(new_entry.ipc_address.as_deref(), Some(address.as_str()));
        assert!(state.with_route_table_mut(|table| table.register_route(new_entry.clone())));
        resume_acquire.take().unwrap().send(()).unwrap();
        let stale = tokio::time::timeout(STEP, acquire_task.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap();
        acquire_task.take();
        assert!(matches!(
            stale,
            Err(super::state::UpstreamAcquireError::Stale { .. })
        ));
        assert!(
            !old_client.is_connected(),
            "stale acquire must initiate native disconnect before returning"
        );
        let (replacement_lease, _, binding) =
            tokio::time::timeout(STEP, state.acquire_upstream_for_route(&new_entry))
                .await
                .unwrap()
                .unwrap_or_else(|_| panic!("same endpoint replacement acquisition"));
        let replacement = replacement_lease.client();
        assert!(!Arc::ptr_eq(&replacement, &old_client));
        assert_eq!(binding.route_uid(), new_entry.route_uid);
        assert_eq!(binding.route_revision(), new_entry.route_revision);
        let reply = tokio::time::timeout(STEP, replacement.call_bound(&binding, "ping", &[]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reply
                .into_bytes_with_pool(replacement.server_pool_arc())
                .unwrap(),
            b"echo"
        );
        // Re-observing the precise closing owner cannot target its replacement.
        state.clients.close(&old_client);
        assert!(old_client.close_shared_bounded(STEP).await);
        let (same_lease, _, same_binding) =
            tokio::time::timeout(STEP, state.acquire_upstream_for_route(&new_entry))
                .await
                .unwrap()
                .unwrap_or_else(|_| panic!("replacement remains cached and healthy"));
        assert!(Arc::ptr_eq(&same_lease.client(), &replacement));
        assert!(replacement.is_connected());
        let reply = tokio::time::timeout(STEP, replacement.call_bound(&same_binding, "ping", &[]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reply
                .into_bytes_with_pool(replacement.server_pool_arc())
                .unwrap(),
            b"echo"
        );
        drop(same_lease);
        drop(replacement_lease);
        assert!(state.local_route("grid").is_some());
        let mut running = relay.take().unwrap();
        tokio::time::timeout(STEP, tokio::task::spawn_blocking(move || running.stop()))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.clients.outstanding(), 0);
    })
    .catch_unwind()
    .await;
    if let Some(resume) = resume_acquire.take() {
        let _ = resume.send(());
    }
    if let Some(task) = acquire_task.take() {
        let _ = tokio::time::timeout(STEP, task).await.unwrap();
    }
    if let Some(mut running) = relay.take() {
        tokio::time::timeout(STEP, tokio::task::spawn_blocking(move || running.stop()))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(STEP, ipc_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
