use super::*;
use crate::test_support::isolated_pair;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;

/// Deliver PONG bytes, but fail its flush only after the test releases it.
struct PongFlushGate {
    stream: c2_local::LocalWriteHalf,
    entered: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
}

impl AsyncWrite for PongFlushGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).expect("test observes PONG flush");
        }
        match Pin::new(&mut self.release).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(ready) => {
                ready.expect("test releases PONG flush");
                #[cfg(windows)]
                let error = io::Error::from_raw_os_error(232);
                #[cfg(not(windows))]
                let error = io::Error::from(io::ErrorKind::UnexpectedEof);
                Poll::Ready(Err(error))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn pong_flush_before_peer_read(disconnect: bool, loan: bool) {
    let conn = Arc::new(Connection::new(91));
    conn.set_handshake_done(true).unwrap();
    let pool = loan.then(|| {
        let pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix(
                PoolConfig {
                    segment_size: 65536,
                    max_segments: 1,
                    ..Default::default()
                },
                format!("disconnect-{}", uuid::Uuid::new_v4().simple()),
            )
            .unwrap(),
        ));
        let handle = pool.write().alloc_shm_handle(512).unwrap();
        let loan = conn.core.prepare_loan(pool.clone(), handle).unwrap();
        let mut batch = conn
            .core
            .begin_write(c2_wire::connection::WriteRequest::Business(vec![loan]))
            .unwrap()
            .unwrap();
        batch.begin_io().unwrap();
        batch.frame_completed();
        batch.flush_completed();
        pool
    });
    let (mut client, stream) = isolated_pair().await;
    let abort = stream.abort_handle();
    let (mut reader, stream) = stream.into_split();
    let (flush_entered, at_flush) = oneshot::channel();
    let (flush_release, flush_resume) = oneshot::channel();
    let (writer, driver) = ConnectionWriter::spawn_test_stream(
        PongFlushGate {
            stream,
            entered: Some(flush_entered),
            release: flush_resume,
        },
        conn.clone(),
    );
    let pong = encode_frame(0, FLAG_SIGNAL | FLAG_RESPONSE, &PONG_BYTES);
    writer.queue_control(pong.clone()).unwrap();
    at_flush.await.unwrap();
    let mut received = vec![0; pong.len()];
    client.read_exact(&mut received).await.unwrap();
    assert_eq!(received, pong);

    let (read_release, read_resume) = oneshot::channel();
    let pending_pool = pool.clone();
    let receive = tokio::spawn({
        let conn = conn.clone();
        let writer = writer.clone();
        async move {
            // Nothing in the receive path may run before PONG flush fails.
            read_resume.await.unwrap();
            let (_shutdown_sender, mut shutdown) = watch::channel(false);
            let mut length = [0; 4];
            if disconnect {
                assert!(
                    read_connection_exact(&mut reader, &mut length, &conn, &mut shutdown)
                        .await
                        .unwrap(),
                    "PONG flush failure must preserve the queued DISCONNECT read"
                );
                let total = u32::from_le_bytes(length);
                let mut body = vec![0; total as usize];
                assert!(
                    read_connection_exact(&mut reader, &mut body, &conn, &mut shutdown)
                        .await
                        .unwrap()
                );
                let (header, payload) = decode_frame_body(&body, total).unwrap();
                assert_eq!(payload, c2_wire::msg_type::DISCONNECT_BYTES);
                assert!(matches!(
                    handle_signal(payload, header.request_id, &writer, &conn).await,
                    SignalAction::Disconnect
                ));
                conn.end(ConnectionEndReason::PeerCloseMessage, "disconnect");
                if let Some(pool) = &pool {
                    assert_eq!(
                        pool.read().stats().alloc_count,
                        1,
                        "DISCONNECT is no loan proof"
                    );
                }
            }
            if !disconnect || pool.is_some() {
                let error = read_connection_exact(&mut reader, &mut length, &conn, &mut shutdown)
                    .await
                    .expect_err("reader observes the actual peer end");
                record_read_error(&conn, &abort, "read_length", &error);
                assert!(reader.peer_ended());
                conn.peer_end();
                if let Some(pool) = &pool {
                    assert_eq!(
                        pool.read().stats().alloc_count,
                        0,
                        "read-side proof settles the loan"
                    );
                }
            }
            assert!(conn.try_close());
            drop(reader);
            conn.publish_physical_close();
            conn.connection_observation().snapshot().unwrap()
        }
    });
    if disconnect {
        client
            .write_all(&encode_frame(
                0,
                FLAG_SIGNAL,
                &c2_wire::msg_type::DISCONNECT_BYTES,
            ))
            .await
            .unwrap();
    }
    drop(client);
    flush_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("writer retires after the controlled flush error")
        .unwrap();
    // The flush fact alone must neither close the reader nor settle its loan.
    if let Some(pool) = pending_pool {
        assert_eq!(
            pool.read().stats().alloc_count,
            1,
            "write-side close is no loan proof"
        );
    }
    assert!(!conn.try_close(), "reader still owns the closing decision");
    read_release.send(()).unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        closed.reason(),
        if disconnect {
            ConnectionEndReason::PeerCloseMessage
        } else {
            ConnectionEndReason::PeerEof
        },
        "{closed:?}"
    );
    assert_eq!(
        closed.operation(),
        if disconnect {
            "disconnect"
        } else {
            "read_length"
        }
    );
}

#[tokio::test]
async fn pong_flush_peer_close_preserves_queued_disconnect() {
    pong_flush_before_peer_read(true, false).await;
}

#[tokio::test]
async fn pong_flush_peer_close_preserves_disconnect_and_loan_until_read_end() {
    pong_flush_before_peer_read(true, true).await;
}

#[tokio::test]
async fn pong_flush_peer_close_leaves_eof_reason_and_loan_reclaim_to_reader() {
    pong_flush_before_peer_read(false, true).await;
}

#[test]
fn non_write_io_failure_keeps_its_original_record() {
    let conn = Connection::new(92);
    conn.end_io("handshake", &io::Error::from(io::ErrorKind::UnexpectedEof));
    assert!(
        conn.try_close(),
        "non-write failures must not await another read"
    );
    conn.publish_physical_close();
    let closed = conn.connection_observation().snapshot().unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::PeerEof);
    assert_eq!(closed.operation(), "handshake");
}

#[test]
fn genuine_write_io_failure_still_records_io_error() {
    let conn = Connection::new(93);
    conn.end_write("write", &io::Error::from(io::ErrorKind::PermissionDenied));
    assert!(conn.try_close());
    conn.publish_physical_close();
    let closed = conn.connection_observation().snapshot().unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::IoError);
    assert_eq!(closed.operation(), "write");
}

#[test]
fn parsed_disconnect_without_loans_does_not_add_an_eof_wait() {
    let conn = Connection::new(94);
    #[cfg(windows)]
    let error = io::Error::from_raw_os_error(232);
    #[cfg(not(windows))]
    let error = io::Error::from(io::ErrorKind::UnexpectedEof);
    conn.end_write("flush", &error);
    assert!(!conn.try_close(), "reader has not chosen a reason yet");
    conn.note_peer_close_message();
    conn.end(ConnectionEndReason::PeerCloseMessage, "disconnect");
    assert!(
        conn.try_close(),
        "a decoded DISCONNECT keeps its existing close behavior when no loan needs read proof"
    );
}

/// The receive loop is blocked on this write receipt until the peer's next
/// frame is fully delivered. Only the selected handshake flush fails.
struct HandshakeFlushGate {
    stream: c2_local::LocalWriteHalf,
    fail_flush: usize,
    completed: usize,
    entered: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
}

impl AsyncWrite for HandshakeFlushGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.completed + 1 != self.fail_flush {
            return match Pin::new(&mut self.stream).poll_flush(cx) {
                Poll::Ready(Ok(())) => {
                    self.completed += 1;
                    Poll::Ready(Ok(()))
                }
                result => result,
            };
        }
        if let Some(entered) = self.entered.take() {
            entered.send(()).expect("test observes handshake flush");
        }
        match Pin::new(&mut self.release).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(ready) => {
                ready.expect("test releases handshake flush");
                #[cfg(windows)]
                let error = io::Error::from_raw_os_error(232);
                #[cfg(not(windows))]
                let error = io::Error::from(io::ErrorKind::UnexpectedEof);
                Poll::Ready(Err(error))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn handshake_flush_before_following_frame(fail_flush: usize, disconnect: bool) {
    #[cfg(unix)]
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
    #[cfg(windows)]
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let server = Arc::new(
        Server::new_with_endpoint(
            context
                .endpoint(&format!(
                    "ipc://handshake-flush-{}",
                    uuid::Uuid::new_v4().simple()
                ))
                .unwrap(),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let conn = Arc::new(Connection::observed(c2_local::ConnectionPublisher::new(
        c2_local::ConnectionSide::Server,
    )));
    let (mut client, stream) = isolated_pair().await;
    let abort = stream.abort_handle();
    let (mut reader, stream) = stream.into_split();
    let (flush_entered, at_flush) = oneshot::channel();
    let (flush_release, flush_resume) = oneshot::channel();
    let (writer, driver) = ConnectionWriter::spawn_test_stream(
        HandshakeFlushGate {
            stream,
            fail_flush,
            completed: 0,
            entered: Some(flush_entered),
            release: flush_resume,
        },
        conn.clone(),
    );
    let receive = tokio::spawn({
        let conn = conn.clone();
        let writer = writer.clone();
        async move {
            let mut shutdown = server.shutdown_tx.subscribe();
            receive_connection(&server, &conn, &mut reader, &writer, &abort, &mut shutdown).await;
            assert!(
                !conn.handshake_done(),
                "failed flush cannot commit the handshake"
            );
            assert!(conn.write_peer_closed());
            assert!(conn.try_close());
            drop(reader);
        }
    });
    client
        .write_all(&encode_version_preamble(VersionPreambleKind::Hello))
        .await
        .unwrap();
    let mut accepted = [0; 25];
    client.read_exact(&mut accepted).await.unwrap();
    assert_eq!(
        accepted,
        encode_version_preamble(VersionPreambleKind::Accepted)
    );
    let metadata = c2_wire::handshake::encode_client_handshake(0, "").unwrap();
    if fail_flush == 2 {
        client
            .write_all(&encode_frame(0, FLAG_HANDSHAKE, &metadata))
            .await
            .unwrap();
        let mut length = [0; 4];
        client.read_exact(&mut length).await.unwrap();
        let total = u32::from_le_bytes(length);
        let mut body = vec![0; total as usize];
        client.read_exact(&mut body).await.unwrap();
        let (header, payload) = decode_frame_body(&body, total).unwrap();
        assert_eq!(header.flags, FLAG_HANDSHAKE | FLAG_RESPONSE);
        decode_handshake(payload, true).unwrap();
    }
    at_flush.await.unwrap();
    assert!(!conn.handshake_done());
    assert!(!conn.write_peer_closed());
    let following = if fail_flush == 1 {
        encode_frame(0, FLAG_HANDSHAKE, &metadata)
    } else {
        let identity = RouteCallIdentity {
            route_name: "unadmitted".to_string(),
            route_uid: "unadmitted-uid".to_string(),
            crm_ns: "test.handshake".to_string(),
            crm_name: "Unadmitted".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef".repeat(4),
            signature_hash: "abcdef0123456789".repeat(4),
        };
        rpc::encode_open_frame(
            1,
            &identity,
            0,
            c2_wire::rpc::CallMode::Unary,
            ValuePart::whole(ValueStorage::Inline(&[])),
        )
        .unwrap()
    };
    client.write_all(&following).await.unwrap();
    if disconnect {
        client
            .write_all(&encode_frame(
                0,
                FLAG_SIGNAL,
                &c2_wire::msg_type::DISCONNECT_BYTES,
            ))
            .await
            .unwrap();
    }
    drop(client);
    flush_release.send(()).unwrap();
    driver.await.unwrap();
    receive.await.unwrap();
    conn.publish_physical_close();
    let closed = conn.connection_observation().snapshot().unwrap();
    assert_eq!(
        closed.reason(),
        if disconnect {
            ConnectionEndReason::PeerCloseMessage
        } else {
            ConnectionEndReason::PeerEof
        },
        "flush={fail_flush}, disconnect={disconnect}: {closed:?}"
    );
}

#[tokio::test]
async fn handshake_accepted_flush_peer_close_drains_following_frames() {
    for disconnect in [false, true] {
        tokio::time::timeout(
            Duration::from_secs(5),
            handshake_flush_before_following_frame(1, disconnect),
        )
        .await
        .expect("Accepted flush failure reaches a read-side close");
    }
}

#[tokio::test]
async fn handshake_ack_flush_peer_close_drains_following_frames() {
    for disconnect in [false, true] {
        tokio::time::timeout(
            Duration::from_secs(5),
            handshake_flush_before_following_frame(2, disconnect),
        )
        .await
        .expect("final ACK flush failure reaches a read-side close");
    }
}
