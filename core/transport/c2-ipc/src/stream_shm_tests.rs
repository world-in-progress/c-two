//! Whole SHM stream admission, original-connection settlement and EOF.
use super::*;
use c2_wire::buddy::BuddyPayload;
use c2_wire::connection::{ConnectionCore, ConnectionSide, WriteRequest};

#[path = "stream_shm_eof_tests.rs"]
mod eof;

struct ShmPeer {
    pool: Arc<RwLock<MemPool>>,
    core: ConnectionCore,
}

impl ShmPeer {
    fn new(directory: &std::path::Path) -> Self {
        let prefix = format!(
            "/s4{:x}{:x}",
            std::process::id(),
            CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix(
                c2_mem::PoolConfig {
                    segment_size: 64 * 1024,
                    max_segments: 2,
                    spill_dir: directory.join("shm-peer"),
                    ..Default::default()
                },
                prefix,
            )
            .unwrap(),
        ));
        let core = ConnectionCore::new(ConnectionSide::Server, || {});
        core.finish_handshake().unwrap();
        Self { pool, core }
    }

    async fn accept(&self, listener: &mut LocalListener) -> LocalStream {
        let mut socket = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
        crate::tests::accept_version_preamble(&mut socket).await;
        let (_, metadata) = read_frame(&mut socket).await;
        let hello = decode_handshake(&metadata, false).unwrap();
        let metadata = encode_server_handshake(
            hello.capability_flags,
            &[] as &[RouteInfo],
            self.pool.read().prefix(),
            &ServerIdentity {
                server_id: "whole-shm-peer".to_string(),
                server_instance_id: "whole-shm-instance".to_string(),
            },
        )
        .unwrap();
        socket
            .write_all(&frame::encode_frame(
                0,
                flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE,
                &metadata,
            ))
            .await
            .unwrap();
        socket
    }

    fn data_frame(&self, id: u64, bytes: &[u8]) -> Vec<u8> {
        let mut handle = self.pool.write().alloc_shm_handle(bytes.len()).unwrap();
        self.pool
            .read()
            .handle_slice_mut(&mut handle)
            .copy_from_slice(bytes);
        let allocation = handle.shm_allocation().unwrap();
        let loan = self.core.prepare_loan(self.pool.clone(), handle).unwrap();
        let descriptor = BuddyPayload {
            seg_idx: allocation.seg_idx as u16,
            generation: allocation.generation,
            offset: allocation.offset,
            data_size: bytes.len() as u32,
            is_dedicated: allocation.is_dedicated,
            loan_id: loan.id(),
        };
        let mut batch = self
            .core
            .begin_write(WriteRequest::Business(vec![loan]))
            .unwrap()
            .unwrap();
        batch.begin_io().unwrap();
        batch.frame_completed();
        batch.flush_completed();
        encode_item_data_frame(id, ValueStorage::Shm(descriptor)).unwrap()
    }

    fn settle_release(&self, header: &FrameHeader, payload: &[u8]) -> bool {
        if payload.first() != Some(&(RpcRecordKind::Release as u8)) {
            return false;
        }
        let outcome = self.core.receive_release(header, payload);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        drop(outcome.reclaims);
        true
    }
}

#[derive(Clone, Copy)]
enum InvalidShm {
    Partial,
    InlineThenShm,
    ShmThenInline,
    WithoutStorage,
}

async fn rejected_shm_settles_on_original_connection(fault: InvalidShm) {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let shm = Arc::new(ShmPeer::new(endpoint._root.path()));
    let (send_data, data_allowed) = oneshot::channel();
    let (released, release_seen) = oneshot::channel();
    let peer = tokio::spawn({
        let shm = shm.clone();
        async move {
            let mut socket = shm.accept(&mut listener).await;
            let (open, _) = read_frame(&mut socket).await;
            let id = open.request_id;
            socket
                .write_all(&encode_item_begin_frame(id, 64).unwrap())
                .await
                .unwrap();
            let mut unary = None;
            let mut credited = matches!(fault, InvalidShm::WithoutStorage);
            while unary.is_none() || !credited {
                let (header, payload) = read_frame(&mut socket).await;
                match decode_rpc_record(&header, &payload, Some(CallMode::ServerStream)).unwrap() {
                    RpcRecord::Credit { storage_bytes, .. } => {
                        assert_eq!(storage_bytes, 64);
                        credited = true;
                    }
                    RpcRecord::Open { .. } => unary = Some(header.request_id),
                    other => panic!("unexpected client record: {other:?}"),
                }
            }
            data_allowed.await.unwrap();
            match fault {
                InvalidShm::Partial => socket
                    .write_all(&shm.data_frame(id, &[7; 32]))
                    .await
                    .unwrap(),
                InvalidShm::InlineThenShm => {
                    socket
                        .write_all(
                            &encode_item_data_frame(id, ValueStorage::Inline(&[1; 32])).unwrap(),
                        )
                        .await
                        .unwrap();
                    socket
                        .write_all(&shm.data_frame(id, &[7; 32]))
                        .await
                        .unwrap();
                }
                InvalidShm::ShmThenInline => {
                    socket
                        .write_all(&shm.data_frame(id, &[7; 64]))
                        .await
                        .unwrap();
                    socket
                        .write_all(&encode_item_data_frame(id, ValueStorage::Inline(&[1])).unwrap())
                        .await
                        .unwrap();
                }
                InvalidShm::WithoutStorage => socket
                    .write_all(&shm.data_frame(id, &[7; 64]))
                    .await
                    .unwrap(),
            }
            // The malformed stream cannot erase an already-dispatched unary.
            reply(&mut socket, unary.unwrap(), b"survived").await;
            let mut cancelled = false;
            while !cancelled || shm.pool.read().stats().alloc_count != 0 {
                let (header, payload) = read_frame(&mut socket).await;
                if shm.settle_release(&header, &payload) {
                    continue;
                }
                match decode_rpc_record(&header, &payload, Some(CallMode::ServerStream)).unwrap() {
                    RpcRecord::Cancel => {
                        assert_eq!(header.request_id, id);
                        cancelled = true;
                    }
                    RpcRecord::Credit { .. } => {}
                    other => panic!("unexpected settlement record: {other:?}"),
                }
            }
            let error = C2Error::new(ErrorCode::CallCancelled, "consumer abandoned invalid item")
                .to_wire_bytes();
            socket
                .write_all(
                    &encode_call_end_frame(
                        id,
                        CallMode::ServerStream,
                        CallStatus::Failed,
                        u64::from(matches!(fault, InvalidShm::ShmThenInline)),
                        ValuePart::whole(ValueStorage::Inline(&error)),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            released.send(()).unwrap();
            expect_disconnect_or_eof(&mut socket).await;
            drop(socket);
            crate::tests::close_listener(listener).await;
        }
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let budget = PayloadBufferBudget::new(64);
    let occupied =
        matches!(fault, InvalidShm::WithoutStorage).then(|| budget.try_reserve(64).unwrap());
    let mut stream = client
        .stream_bound(&binding(), "items", b"", budget.clone())
        .await
        .unwrap();
    let mut copying = None;
    let mut resume = None;
    if matches!(fault, InvalidShm::ShmThenInline) {
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        stream
            .receive_call_for_test()
            .set_shm_copy_gate_for_test(entered, released);
        copying = Some(waiting);
        resume = Some(release);
    }
    let unary = tokio::spawn({
        let client = client.clone();
        async move { client.call_bound(&binding(), "items", b"").await }
    });
    send_data.send(()).unwrap();
    if let Some(entered) = copying {
        tokio::task::spawn_blocking(move || entered.recv_timeout(WAIT))
            .await
            .unwrap()
            .unwrap();
    }
    let stopped = timeout(WAIT, async {
        while client.is_connected() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Some(release) = resume {
        release.send(()).unwrap();
    }
    let error = timeout(WAIT, stream.recv()).await.unwrap().unwrap_err();
    assert!(
        format!("{error:?}").contains("ProtocolViolation"),
        "{error:?}"
    );
    assert_eq!(
        unary.await.unwrap().unwrap().into_owned_bytes().unwrap(),
        b"survived"
    );
    timeout(WAIT, release_seen).await.unwrap().unwrap();
    assert_eq!(shm.pool.read().stats().alloc_count, 0);
    timeout(WAIT, stream.completion()).await.unwrap().unwrap();
    drop(occupied);
    assert_eq!(budget.snapshot().used_bytes, 0);
    client.close_shared().await;
    peer.await.unwrap();
    assert!(
        stopped.is_ok(),
        "invalid SHM did not deactivate its connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_shm_deactivates_and_releases_on_original_connection() {
    rejected_shm_settles_on_original_connection(InvalidShm::Partial).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_then_shm_deactivates_and_releases_on_original_connection() {
    rejected_shm_settles_on_original_connection(InvalidShm::InlineThenShm).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_then_inline_deactivates_and_releases_on_original_connection() {
    rejected_shm_settles_on_original_connection(InvalidShm::ShmThenInline).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncredited_shm_deactivates_and_releases_on_original_connection() {
    rejected_shm_settles_on_original_connection(InvalidShm::WithoutStorage).await;
}
