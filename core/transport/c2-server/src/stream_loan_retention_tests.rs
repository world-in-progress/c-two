use super::*;
use c2_wire::release::{ReleaseBatch, encode_release_frame};

async fn loan_one_item(harness: &mut Harness, id: u64) -> BuddyPayload {
    harness.open(id, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4096)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(id, 4096, 0).unwrap())
        .await
        .unwrap();
    let SeenRecord::ShmData(loan) = read_record(&mut harness.peer, CallMode::ServerStream).await
    else {
        panic!("expected a whole SHM item")
    };
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(id, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    loan
}

#[tokio::test]
async fn connected_peer_without_release_keeps_original_loan_past_maintenance_and_close() {
    // Block 9b3 requirement 2: elapsed timers and allocator maintenance cannot
    // reclaim a connected peer's loan, even after the stream's terminal. Only
    // its real RELEASE or read-side peer end settles the original allocation.
    let observed = Arc::new(Observed::default());
    observed.item_bytes.store(4096, Ordering::SeqCst);
    let mut config = ServerIpcConfig {
        shm_threshold: 1,
        pool_decay_seconds: 0.0,
        ..ServerIpcConfig::default()
    };
    config.base.pool_segment_size = 8192;
    config.base.max_pool_segments = 1;
    config.base.spill_threshold = 1.0;
    let mut harness = Harness::start(observed, config, CAP_SHM_RESPONSE).await;
    let first = loan_one_item(&mut harness, 1).await;
    let pool = harness.server.response_pool_arc();
    let conn = harness
        .server
        .active_connection(harness.server.active_connection_ids()[0])
        .unwrap();
    conn.wait_idle().await;
    assert_eq!(conn.calls.lock().active().count(), 0);

    tokio::time::pause();
    for elapsed in [Duration::from_secs(86_400), Duration::from_secs(31_536_000)] {
        tokio::time::advance(elapsed).await;
        // Pool decay uses the wall clock, so zero decay makes every GC pass
        // immediately eligible; virtual time separately exercises async waits.
        assert_eq!(pool.inner.write().gc_buddy(), 0);
        assert_eq!(harness.server.chunk_registry.gc_sweep().expired, 0);
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert_eq!(pool.read().stats().buddy_occupied_bytes, 4096);
        assert!(!conn.is_deactivated());
    }
    tokio::time::resume();

    let second = loan_one_item(&mut harness, 2).await;
    assert_eq!(pool.read().stats().alloc_count, 2);
    assert_ne!(
        (
            first.seg_idx,
            first.generation,
            first.offset,
            first.is_dedicated
        ),
        (
            second.seg_idx,
            second.generation,
            second.offset,
            second.is_dedicated
        ),
        "a new item must not overwrite the still-published first item"
    );
    harness
        .peer
        .write_all(&encode_release_frame(
            &ReleaseBatch::new(&[second.loan_id]).unwrap(),
            false,
        ))
        .await
        .unwrap();
    // A later OPEN and its BEGIN fence the original connection's RELEASE.
    harness.open(3, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4096)
    ));
    assert_eq!(pool.read().stats().alloc_count, 1);
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(3, rpc::CancelCause::Consumer).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Failed, Some(0), Some(ErrorCode::CallCancelled))
    ));
    conn.wait_idle().await;
    conn.end(ConnectionEndReason::LocalClose, "test_unreleased_loan");
    assert!(!conn.try_close(), "deactivation is no peer-end proof");
    assert!(
        conn.connection_observation()
            .wait_closed(Some(Duration::ZERO))
            .await
            .is_none()
    );
    assert_eq!(pool.read().stats().alloc_count, 1);
    drop(harness.peer);
    harness.handler.await.unwrap();
    assert_eq!(pool.read().stats().alloc_count, 0);
    assert_eq!(pool.read().stats().buddy_occupied_bytes, 0);
    assert!(conn.connection_observation().snapshot().is_some());
}
