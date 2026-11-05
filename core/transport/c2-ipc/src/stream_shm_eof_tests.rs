//! A physical peer end during an admitted SHM item copy preserves call order.
use super::*;
use c2_mem::retention::PayloadBufferBudget;
use c2_wire::rpc::{RpcRecord, decode_rpc_record, encode_credit_frame, encode_item_begin_frame};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

struct OperationOwner(Arc<AtomicBool>);

impl Drop for OperationOwner {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eof_during_whole_shm_copy_waits_for_item_and_loan_settlement() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let shm = Arc::new(ShmPeer::new(endpoint._root.path()));
    let (allow_begin, begin_allowed) = oneshot::channel();
    let (drop_peer, drop_allowed) = oneshot::channel();
    let (dropped, peer_dropped) = oneshot::channel();
    let peer = tokio::spawn({
        let shm = shm.clone();
        async move {
            let mut socket = shm.accept(&mut listener).await;
            let (open, payload) = read_frame(&mut socket).await;
            assert!(matches!(
                decode_rpc_record(&open, &payload, None).unwrap(),
                RpcRecord::Open { method_idx: 0, .. }
            ));
            let id = open.request_id;
            begin_allowed.await.unwrap();
            socket
                .write_all(&encode_item_begin_frame(id, 64).unwrap())
                .await
                .unwrap();
            let (credit, payload) = read_frame(&mut socket).await;
            assert_eq!(credit.request_id, id);
            assert_eq!(
                payload,
                encode_credit_frame(id, 64, 0).unwrap()[HEADER_SIZE..],
                "the client must reserve the whole item before admitting SHM"
            );
            socket
                .write_all(&shm.data_frame(id, &[7; 64]))
                .await
                .unwrap();
            drop_allowed.await.unwrap();
            // Closing the complete native socket yields a real read-side end on
            // Unix and Windows; no synthetic failure or half-close is involved.
            drop(socket);
            dropped.send(()).unwrap();
            crate::tests::close_listener(listener).await;
        }
    });

    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let observation = client.connection_observation().unwrap();
    let budget = PayloadBufferBudget::new(64);
    let owner_dropped = Arc::new(AtomicBool::new(false));
    let mut response = client
        .stream_bound_owned_controlled_phased(
            &binding(),
            "items",
            Vec::new(),
            budget.clone(),
            Box::new(OperationOwner(owner_dropped.clone())),
            crate::IpcStreamCancellation::new(),
            &IpcCallControl::new(|| Ok(())),
        )
        .await
        .unwrap();
    let (copy_entered, copy_enter) = std::sync::mpsc::channel();
    let (release_copy, copy_released) = std::sync::mpsc::channel();
    response
        .receive_call_for_test()
        .set_shm_copy_gate_for_test(copy_entered, copy_released);
    allow_begin.send(()).unwrap();
    tokio::task::spawn_blocking(move || copy_enter.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the real SHM copy did not reach the gate");
    assert_eq!(budget.snapshot().used_bytes, 64);
    assert_eq!(shm.pool.read().stats().alloc_count, 1);

    drop_peer.send(()).unwrap();
    peer_dropped.await.unwrap();
    timeout(WAIT, async {
        while client.pending_len_for_test() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("read-side EOF did not settle the pending entry");
    assert!(response.try_completion().is_none());
    assert!(!owner_dropped.load(Ordering::Acquire));
    assert_eq!(budget.snapshot().used_bytes, 64);
    assert!(
        observation.snapshot().is_none(),
        "the physical connection closed while its SHM read was active"
    );

    release_copy.send(()).unwrap();
    let terminal = timeout(WAIT, response.completion())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(terminal.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(owner_dropped.load(Ordering::Acquire));
    // The copy may win or lose the delivery race with EOF. Either outcome
    // preserves a whole item and the same dispatch-uncertain terminal.
    match timeout(WAIT, response.recv()).await.unwrap() {
        Ok(Some(item)) => {
            assert_eq!(item.as_ref(), &[7; 64]);
            drop(item);
            let error = timeout(WAIT, response.recv()).await.unwrap().unwrap_err();
            assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
        }
        Err(error) => assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain),
        Ok(None) => panic!("EOF cannot produce natural stream completion"),
    }
    assert_eq!(budget.snapshot().used_bytes, 0);
    observation
        .wait_closed(Some(WAIT))
        .await
        .expect("the physical connection did not close after the SHM read ended");
    peer.await.unwrap();
    assert_eq!(shm.pool.read().stats().alloc_count, 1);
    // The fake peer kept its original loan book even after closing its socket.
    // Its own peer-end proof, rather than the client's local EOF, reclaims L.
    drop(shm.core.peer_end());
    assert_eq!(shm.pool.read().stats().alloc_count, 0);
    client.close_shared().await;
}
