//! Relay stop and stale acquisition through real HTTP and IPC connections.
use std::sync::Arc;
use std::time::Duration;

use c2_config::RelayConfig;
use c2_server::{Server, ServerIdentity, ServerIpcConfig};
use futures::FutureExt;
use tokio::sync::oneshot;

use super::server::RelayServer;

const STEP: Duration = Duration::from_secs(10);

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
    #[cfg(unix)]
    let root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = root.context();
    #[cfg(windows)]
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let server = Arc::new(
        Server::new_with_identity_and_endpoint(
            context.endpoint(&address).unwrap(),
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
        let running = tokio::task::spawn_blocking(move || {
            RelayServer::start_with_context(config, context).unwrap()
        })
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
        let namespace = state.endpoint_context().namespace_id().to_owned();
        registration_task = Some(tokio::spawn(async move {
            let response = reqwest::Client::builder()
                .no_proxy()
                .timeout(STEP)
                .build()
                .unwrap()
                .post(format!("http://{socket}/_register"))
                .header(crate::client::LOCAL_ENDPOINT_NAMESPACE_HEADER, namespace)
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
    #[cfg(unix)]
    let root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = root.context();
    #[cfg(windows)]
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let server = Arc::new(
        Server::new_with_identity_and_endpoint(
            context.endpoint(&address).unwrap(),
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
            let relay = RelayServer::start_with_context(config, context).unwrap();
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
        let (lease, _) = tokio::time::timeout(STEP, state.acquire_upstream_for_route(&old_entry))
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
        let new_uid = server.registered_route_identity("grid").await.unwrap();
        assert_ne!(new_uid, old_entry.route_uid);
        let mut new_entry = old_entry.clone();
        new_entry.route_uid = new_uid;
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
        let (replacement_lease, binding) =
            tokio::time::timeout(STEP, state.acquire_upstream_for_route(&new_entry))
                .await
                .unwrap()
                .unwrap_or_else(|_| panic!("same endpoint replacement acquisition"));
        let replacement = replacement_lease.client();
        assert!(!Arc::ptr_eq(&replacement, &old_client));
        assert_eq!(binding.route_uid(), new_entry.route_uid);
        let reply = tokio::time::timeout(STEP, replacement.call_bound(&binding, "ping", &[]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.into_owned_bytes().unwrap(), b"echo");
        // Re-observing the precise closing owner cannot target its replacement.
        state.clients.close(&old_client);
        assert!(old_client.close_shared_bounded(STEP).await);
        let (same_lease, same_binding) =
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
        assert_eq!(reply.into_owned_bytes().unwrap(), b"echo");
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
