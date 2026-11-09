use super::*;

const STALLED_ITEM_BYTES: usize = 8 * 1024 * 1024;

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected state did not become observable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_local_reads_bound_calls_and_preserve_half_frame_close_truth() {
    // Block 9b3 requirement 3: a real local peer stops reading a large frame.
    let observed = Arc::new(Observed::default());
    observed
        .item_bytes
        .store(STALLED_ITEM_BYTES, Ordering::SeqCst);
    let (unary_entered_tx, unary_entered_rx) = oneshot::channel();
    let (release_unary_tx, release_unary_rx) = mpsc::channel();
    *observed.unary_entered.lock().unwrap() = Some(unary_entered_tx);
    *observed.release_unary.lock().unwrap() = Some(release_unary_rx);
    let mut config = ServerIpcConfig::default();
    config.max_execution_workers = 2;
    config.max_pending_requests = 2;
    config.base.write_timeout_secs = 0.01;
    config.base.chunk_size = STALLED_ITEM_BYTES as u64;
    config.max_frame_size = STALLED_ITEM_BYTES as u64 + 1024;
    let mut harness = Harness::start(observed.clone(), config, 0).await;

    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(n) if n == STALLED_ITEM_BYTES as u64
    ));
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                2,
                &harness.identity,
                0,
                CallMode::Unary,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    unary_entered_rx.await.unwrap();
    let before_stall = harness
        .server
        .route_scheduler_snapshot("portable")
        .await
        .unwrap();
    assert_eq!((before_stall.pending, before_stall.active_workers), (2, 1));
    let conn = harness
        .server
        .active_connection(harness.server.active_connection_ids()[0])
        .unwrap();
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, STALLED_ITEM_BYTES as u64, 0).unwrap())
        .await
        .unwrap();
    // The peer now performs no reads. A full-size real local-socket write
    // exceeds its deadline only after the OS stops accepting the frame.
    until(|| conn.is_deactivated()).await;
    assert!(conn.core.stream_state(1).is_some());
    assert_eq!(conn.calls.lock().active().count(), 2);
    assert!((0..=2).contains(&conn.inflight_count()));
    assert_eq!(harness.server.pending_requests.available_permits(), 0);
    assert_eq!(
        observed.encoded_ranges.lock().unwrap().as_slice(),
        &[(0, STALLED_ITEM_BYTES)]
    );
    assert!(
        !conn.try_close(),
        "the unread half-frame is not a close proof"
    );
    assert!(
        conn.connection_observation()
            .wait_closed(Some(Duration::ZERO))
            .await
            .is_none(),
        "the physical connection cannot yet report a completed close"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), conn.wait_idle())
            .await
            .is_err(),
        "a stalled physical write must keep the original work unfinished"
    );

    // The second call had already reserved the same physical connection.
    // It can finish resource execution while the first response cannot flush.
    release_unary_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = harness
                .server
                .route_scheduler_snapshot("portable")
                .await
                .unwrap();
            if snapshot.active_workers == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(observed.entries.load(Ordering::SeqCst), 2);
    assert_eq!(observed.steps.load(Ordering::SeqCst), 1);
    assert!(conn.calls.lock().active().count() <= 2);
    assert!(conn.inflight_count() <= 2);

    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(bytes) if bytes.len() == STALLED_ITEM_BYTES
    ));
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    tokio::time::timeout(Duration::from_secs(5), conn.wait_idle())
        .await
        .unwrap();
    assert_eq!(conn.calls.lock().active().count(), 0);
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    drop(conn);
    harness.stop().await;
}

// Windows uses stopped_local_reads_bound_calls_and_preserve_half_frame_close_truth:
// its real local stream exercises the same bounded call records, OS write
// blockage, truthful incomplete close, and read-resume settlement. SIGSTOP is
// a POSIX process-control addition, not a separate transport mechanism.
#[cfg(unix)]
mod posix {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};

    const ROOT_ENV: &str = "C2_STALLED_PEER_ROOT";

    struct PeerProcess {
        child: Child,
        stdin: ChildStdin,
        stdout: Option<BufReader<std::process::ChildStdout>>,
    }

    impl Drop for PeerProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl PeerProcess {
        fn signal(&self, signal: &str) {
            let status = Command::new("kill")
                .arg(signal)
                .arg(self.child.id().to_string())
                .status()
                .unwrap();
            assert!(status.success(), "kill {signal} failed: {status}");
        }

        async fn line(&mut self) -> String {
            let mut stdout = self.stdout.take().unwrap();
            let (stdout, line) = tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                loop {
                    let read = stdout.read_line(&mut line).unwrap();
                    assert!(read > 0, "peer exited before its synchronization marker");
                    if line.contains("C2_PEER_") {
                        break;
                    }
                    line.clear();
                }
                (stdout, line)
            })
            .await
            .unwrap();
            self.stdout = Some(stdout);
            line
        }
    }

    async fn read_tagged_record(peer: &mut LocalStream) -> (u64, SeenRecord) {
        let mut len = [0; 4];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut len))
            .await
            .unwrap()
            .unwrap();
        let mut body = vec![0; u32::from_le_bytes(len) as usize];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut body))
            .await
            .unwrap()
            .unwrap();
        let mut bytes = len.to_vec();
        bytes.extend_from_slice(&body);
        let (header, payload) = frame::decode_frame(&bytes).unwrap();
        let id = header.request_id;
        let mode = if id == 1 {
            CallMode::ServerStream
        } else {
            assert_eq!(id, 2);
            CallMode::Unary
        };
        let seen = match rpc::decode_rpc_record(&header, payload, Some(mode)).unwrap() {
            RpcRecord::ItemData {
                value:
                    ValuePart {
                        storage: ValueStorage::Inline(data),
                        ..
                    },
            } => SeenRecord::Data(data.to_vec()),
            RpcRecord::CallEnd {
                status,
                complete_items,
                error,
                ..
            } => SeenRecord::End(status, complete_items, error.map(|e| e.code)),
            record => panic!("unexpected resumed record for call {id}: {record:?}"),
        };
        (id, seen)
    }

    #[test]
    #[ignore = "run only as the controlled child of sigstop_peer_resumes_and_settles"]
    fn stalled_peer_child() {
        let root = std::env::var(ROOT_ENV).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let endpoint =
                c2_config::LocalEndpointContext::with_unix_root(std::path::Path::new(&root))
                    .unwrap()
                    .endpoint("ipc://stalled-peer")
                    .unwrap();
            let mut peer = LocalStream::connect(&endpoint, c2_local::DEFAULT_CONNECT_TIMEOUT)
                .await
                .unwrap();
            super::super::super::tests::handshake_test_client_with(&mut peer, 0, "").await;
            let descriptor = ValidatedContractDescriptor::from_json(STREAM_DESCRIPTOR).unwrap();
            let identity: RouteCallIdentity = RouteCallIdentity {
                route_name: "portable".into(),
                crm_ns: descriptor.crm_namespace().into(),
                crm_name: descriptor.crm_name().into(),
                crm_ver: descriptor.crm_version().into(),
                abi_hash: descriptor.abi_hash().into(),
                signature_hash: descriptor.signature_hash().into(),
            };
            peer.write_all(
                &rpc::encode_open_frame(
                    1,
                    &identity,
                    1,
                    CallMode::ServerStream,
                    ValuePart::whole(ValueStorage::Inline(&[1])),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            assert!(matches!(
                read_record(&mut peer, CallMode::ServerStream).await,
                SeenRecord::Begin(n) if n == STALLED_ITEM_BYTES as u64
            ));
            peer.write_all(&rpc::encode_credit_frame(1, STALLED_ITEM_BYTES as u64, 0).unwrap())
                .await
                .unwrap();
            peer.write_all(
                &rpc::encode_open_frame(
                    2,
                    &identity,
                    0,
                    CallMode::Unary,
                    ValuePart::whole(ValueStorage::Inline(&[])),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            println!("C2_PEER_READY");
            std::io::stdout().flush().unwrap();
            let mut command = String::new();
            std::io::stdin().read_line(&mut command).unwrap();
            assert_eq!(command, "read\n");
            let mut saw_data = false;
            let mut saw_unary = false;
            let mut saw_stream_end = false;
            for _ in 0..3 {
                match read_tagged_record(&mut peer).await {
                    (1, SeenRecord::Data(bytes)) => {
                        assert!(!saw_data);
                        assert_eq!(bytes.len(), STALLED_ITEM_BYTES);
                        saw_data = true;
                    }
                    (2, SeenRecord::End(CallStatus::Success, None, None)) => {
                        assert!(!saw_unary);
                        saw_unary = true;
                    }
                    (
                        1,
                        SeenRecord::End(
                            CallStatus::Failed,
                            Some(1),
                            Some(ErrorCode::CallCancelled),
                        ),
                    ) => {
                        assert!(saw_data, "CALL_END cannot overtake its item");
                        saw_stream_end = true;
                    }
                    other => panic!("unexpected resumed record: {other:?}"),
                }
            }
            assert!(saw_data && saw_unary && saw_stream_end);
            println!("C2_PEER_SETTLED");
            std::io::stdout().flush().unwrap();
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sigstop_peer_resumes_and_settles() {
        // Block 9b3 requirement 4: a genuine POSIX peer process is stopped
        // after reserving two calls on one real local connection.
        let root = tempfile::Builder::new()
            .prefix("c2-stalled-peer-")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        let endpoint = c2_config::LocalEndpointContext::with_unix_root(root.path())
            .unwrap()
            .endpoint("ipc://stalled-peer")
            .unwrap();
        let mut config = ServerIpcConfig::default();
        config.base.write_timeout_secs = 30.0;
        config.base.chunk_size = STALLED_ITEM_BYTES as u64;
        config.max_frame_size = STALLED_ITEM_BYTES as u64 + 1024;
        let server = Arc::new(Server::new_with_endpoint(endpoint, config).unwrap());
        let observed = Arc::new(Observed::default());
        observed
            .item_bytes
            .store(STALLED_ITEM_BYTES, Ordering::SeqCst);
        let (unary_entered_tx, unary_entered_rx) = oneshot::channel();
        let (release_unary_tx, release_unary_rx) = mpsc::channel();
        *observed.unary_entered.lock().unwrap() = Some(unary_entered_tx);
        *observed.release_unary.lock().unwrap() = Some(release_unary_rx);
        let descriptor = ValidatedContractDescriptor::from_json(STREAM_DESCRIPTOR).unwrap();
        let built = server
            .build_stream_route(
                RouteBuildSpec {
                    name: "portable".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: descriptor
                        .methods()
                        .iter()
                        .map(|m| m.name().into())
                        .collect(),
                    access_map: HashMap::new(),
                    concurrency_mode: crate::scheduler::ConcurrencyMode::Exclusive,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(CounterCallback(observed.clone())),
                &descriptor,
            )
            .unwrap();
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        server.begin_start_attempt().unwrap();
        let run_server = server.clone();
        let run = tokio::spawn(async move { run_server.run().await });
        server
            .wait_until_ready(Duration::from_secs(5))
            .await
            .unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--nocapture", "stalled_peer_child"])
            .env(ROOT_ENV, root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut peer = PeerProcess {
            stdin: child.stdin.take().unwrap(),
            stdout: Some(BufReader::new(child.stdout.take().unwrap())),
            child,
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(5), peer.line())
                .await
                .expect("peer did not reach the stop barrier")
                .contains("C2_PEER_READY")
        );
        tokio::time::timeout(Duration::from_secs(5), unary_entered_rx)
            .await
            .unwrap()
            .unwrap();
        peer.signal("-STOP");
        let child_pid = peer.child.id().to_string();
        until(|| {
            Command::new("ps")
                .args(["-o", "stat=", "-p", &child_pid])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout)
                            .trim()
                            .starts_with('T')
                })
        })
        .await;
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        until(|| !observed.encoded_ranges.lock().unwrap().is_empty()).await;
        let stream = conn.core.stream_state(1).unwrap();
        assert_eq!(
            stream.storage_bytes + stream.admitted_bytes,
            STALLED_ITEM_BYTES as u64
        );
        assert_eq!(conn.calls.lock().active().count(), 2);
        assert!(conn.inflight_count() <= 2);
        assert_eq!(
            observed.encoded_ranges.lock().unwrap().as_slice(),
            &[(0, STALLED_ITEM_BYTES)]
        );
        assert!(!conn.try_close());
        release_unary_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = server.route_scheduler_snapshot("portable").await.unwrap();
                if snapshot.active_workers == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(observed.entries.load(Ordering::SeqCst), 2);
        // Unary execution claims its terminal and retires its call record
        // before its original response can pass the blocked stream frame.
        until(|| conn.calls.lock().get(2).is_none()).await;
        assert_eq!(conn.calls.lock().active().count(), 1);
        assert!(conn.calls.lock().get(1).is_some());
        server.request_shutdown_signal();
        until(|| matches!(server.lifecycle_state(), ServerLifecycleState::Stopping)).await;
        // A stopped process cannot consume the frame or its terminal. The
        // original server transaction must remain visibly incomplete.
        assert!(
            conn.connection_observation()
                .wait_closed(Some(Duration::ZERO))
                .await
                .is_none()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                server.wait_until_stopped(Duration::from_secs(5))
            )
            .await
            .is_err()
        );
        assert!(!run.is_finished());
        peer.signal("-CONT");
        peer.stdin.write_all(b"read\n").unwrap();
        peer.stdin.flush().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), peer.line())
                .await
                .expect("resumed peer did not settle the original calls")
                .contains("C2_PEER_SETTLED")
        );
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
        assert_eq!(conn.calls.lock().active().count(), 0);
        drop(conn);
    }
}
