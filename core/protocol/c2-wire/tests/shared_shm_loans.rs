use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use c2_config::DomainLimits;
use c2_mem::{DomainBudget, MemPool, PeerMappingCache, PoolConfig, SealedShmBlock};
use c2_wire::connection::{ConnectionCore, ConnectionSide, WriteRequest};
use c2_wire::frame::decode_frame;
use c2_wire::release::{ReleaseBatch, encode_release_frame};
use parking_lot::RwLock;

static POOL_SEQ: AtomicU64 = AtomicU64::new(1);

fn make_pool(
    segment_size: usize,
    budget_bytes: u64,
) -> (DomainBudget, PoolConfig, String, Arc<RwLock<MemPool>>) {
    let _id = POOL_SEQ.fetch_add(1, Ordering::Relaxed);
    let config = PoolConfig {
        segment_size,
        max_segments: 2,
        max_dedicated_segments: 4,
        buddy_idle_decay_secs: 0.0,
        ..PoolConfig::default()
    };
    let limits = DomainLimits {
        memory_budget_bytes: budget_bytes,
        file_spill_budget_bytes: 0,
        max_outstanding_calls: 64,
    };
    let budget = DomainBudget::from_limits(&limits);
    let raw_pool = MemPool::new_with_budget(config.clone(), budget.clone()).expect("valid pool");
    let prefix = raw_pool.prefix().to_string();
    let pool = Arc::new(RwLock::new(raw_pool));
    (budget, config, prefix, pool)
}

fn server_connection() -> ConnectionCore {
    let conn = ConnectionCore::new(ConnectionSide::Server, || {});
    conn.finish_handshake().unwrap();
    conn
}

fn publish_shared(conn: &ConnectionCore, block: &SealedShmBlock, settled: Arc<AtomicUsize>) -> u64 {
    let loan = block.mint_loan().with_on_settle(move || {
        settled.fetch_add(1, Ordering::AcqRel);
    });
    let prepared = conn.prepare_shared_loan(loan).unwrap();
    let loan_id = prepared.id();
    let mut batch = conn
        .begin_write(WriteRequest::Business(vec![prepared]))
        .unwrap()
        .unwrap();
    batch.begin_io().unwrap();
    batch.frame_completed();
    batch.flush_completed();
    loan_id
}

fn send_client_release(conn: &ConnectionCore, loan_id: u64) {
    let raw = encode_release_frame(&ReleaseBatch::new(&[loan_id]).unwrap(), false);
    let (header, payload) = decode_frame(&raw).unwrap();
    let outcome = conn.receive_release(&header, payload);
    assert!(outcome.error.is_none(), "unexpected release error: {:?}", outcome.error);
    outcome.reclaims.release();
}

#[test]
fn buddy_and_dedicated_multi_loans_stay_readable_until_last_proven_release() {
    // Segment size = 64 KiB:
    // - 16 KiB allocates in buddy pool;
    // - 128 KiB overflows buddy and allocates a dedicated segment.
    for (label, payload_len, expect_dedicated) in [
        ("buddy", 16 * 1024, false),
        ("dedicated", 128 * 1024, true),
    ] {
        let (budget, config, prefix, pool) = make_pool(64 * 1024, 4 * 1024 * 1024);
        let payload: Vec<u8> = (0usize..payload_len)
            .map(|i| ((i.wrapping_mul(31) ^ 0x5A) & 0xFF) as u8)
            .collect();

        let block = SealedShmBlock::from_bytes(Arc::clone(&pool), &payload).expect(label);
        assert_eq!(block.is_dedicated(), expect_dedicated, "{label}");
        assert_eq!(block.as_slice(), payload.as_slice(), "{label}");
        assert_eq!(block.active_refs(), 1);
        let alloc = block.allocation();

        let conn_a = server_connection();
        let conn_b = server_connection();
        let settled_a = Arc::new(AtomicUsize::new(0));
        let settled_b = Arc::new(AtomicUsize::new(0));

        let loan_a = publish_shared(&conn_a, &block, Arc::clone(&settled_a));
        let loan_b = publish_shared(&conn_b, &block, Arc::clone(&settled_b));
        assert_ne!(loan_a, loan_b);
        assert_eq!(block.active_refs(), 3);
        assert!(conn_a.is_loan_live(loan_a));
        assert!(conn_b.is_loan_live(loan_b));

        // Drop the topic ring's reference while both peers still hold loans.
        drop(block);

        // Both peers map the exact same SHM backing read-only through PeerMappingCache.
        let cache_a = PeerMappingCache::new(config.clone(), prefix.clone()).unwrap();
        let cache_b = PeerMappingCache::new(config.clone(), prefix.clone()).unwrap();
        let read_a = cache_a
            .admit(
                alloc.seg_idx as u16,
                alloc.generation,
                alloc.offset,
                payload_len as u32,
                alloc.is_dedicated,
            )
            .unwrap();
        let read_b = cache_b
            .admit(
                alloc.seg_idx as u16,
                alloc.generation,
                alloc.offset,
                payload_len as u32,
                alloc.is_dedicated,
            )
            .unwrap();

        let mut buf_a = vec![0u8; payload_len];
        // SAFETY: conn_a's published loan pins the sealed immutable block.
        unsafe { read_a.copy_into(&mut buf_a) }.unwrap();
        assert_eq!(buf_a, payload, "{label} peer A bytes");
        drop(read_a);

        // Peer A releases first -> peer A settles, peer B STILL reads original bytes untouched!
        send_client_release(&conn_a, loan_a);
        assert!(!conn_a.is_loan_live(loan_a));
        assert_eq!(settled_a.load(Ordering::Acquire), 1);
        assert_eq!(settled_b.load(Ordering::Acquire), 0);

        // Allocate/overwrite another block in the pool while peer B still holds its loan:
        // must NOT touch peer B's allocation!
        let other = SealedShmBlock::from_bytes(Arc::clone(&pool), &vec![0xFFu8; payload_len])
            .expect("second allocation");
        let mut buf_b = vec![0u8; payload_len];
        // SAFETY: conn_b's published loan pins the sealed immutable block.
        unsafe { read_b.copy_into(&mut buf_b) }.unwrap();
        assert_eq!(buf_b, payload, "{label} peer B bytes after peer A release");
        drop(read_b);
        drop(other);

        // Dedicated charge must still be positive until peer B releases.
        if expect_dedicated {
            assert!(budget.snapshot().memory.used_bytes >= payload_len as u64);
        }

        // Peer B releases last -> allocation reclaimed and dedicated budget returns to 0!
        send_client_release(&conn_b, loan_b);
        assert!(!conn_b.is_loan_live(loan_b));
        assert_eq!(settled_b.load(Ordering::Acquire), 1);
        if expect_dedicated {
            assert_eq!(budget.snapshot().memory.used_bytes, 0);
        } else {
            let stats = pool.read().stats();
            assert_eq!(stats.dedicated_segments, 0);
            // Buddy segment can now be reclaimed cleanly by idle GC (decay = 0s).
            pool.write().gc_buddy();
            assert_eq!(budget.snapshot().memory.used_bytes, 0);
        }
    }
}

#[test]
fn pre_io_cancellation_settles_without_affecting_other_readers() {
    let (budget, _config, _prefix, pool) = make_pool(64 * 1024, 4 * 1024 * 1024);
    let payload = vec![42u8; 128 * 1024];
    let block = SealedShmBlock::from_bytes(Arc::clone(&pool), &payload).unwrap();
    assert!(block.is_dedicated());

    let conn_a = server_connection();
    let conn_b = server_connection();
    let settled_a = Arc::new(AtomicUsize::new(0));
    let settled_b = Arc::new(AtomicUsize::new(0));

    // Conn A prepares a loan, then drops/cancels BEFORE begin_io().
    let sa = Arc::clone(&settled_a);
    let loan_a = block.mint_loan().with_on_settle(move || {
        sa.fetch_add(1, Ordering::AcqRel);
    });
    let prepared_a = conn_a.prepare_shared_loan(loan_a).unwrap();
    let id_a = prepared_a.id();
    assert_eq!(block.active_refs(), 2);
    drop(prepared_a);
    assert!(!conn_a.is_loan_live(id_a));
    assert_eq!(settled_a.load(Ordering::Acquire), 1);
    assert_eq!(block.active_refs(), 1);

    // Conn B publishes normally and later settles on release.
    let id_b = publish_shared(&conn_b, &block, Arc::clone(&settled_b));
    assert_eq!(block.active_refs(), 2);
    drop(block);
    assert!(budget.snapshot().memory.used_bytes >= 128 * 1024);

    send_client_release(&conn_b, id_b);
    assert_eq!(settled_b.load(Ordering::Acquire), 1);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn post_io_write_failure_and_drop_without_peer_end_never_reclaims_shared_block() {
    let (budget, _config, _prefix, pool) = make_pool(64 * 1024, 4 * 1024 * 1024);
    let payload = vec![9u8; 128 * 1024];
    let block = SealedShmBlock::from_bytes(Arc::clone(&pool), &payload).unwrap();
    assert!(block.is_dedicated());

    let settled_a = Arc::new(AtomicUsize::new(0));
    let settled_b = Arc::new(AtomicUsize::new(0));
    let conn_b = server_connection();
    let loan_b = publish_shared(&conn_b, &block, Arc::clone(&settled_b));

    {
        let conn_a = server_connection();
        let sa = Arc::clone(&settled_a);
        let prepared = conn_a
            .prepare_shared_loan(block.mint_loan().with_on_settle(move || {
                sa.fetch_add(1, Ordering::AcqRel);
            }))
            .unwrap();
        let mut batch = conn_a
            .begin_write(WriteRequest::Business(vec![prepared]))
            .unwrap()
            .unwrap();
        // Enter I/O, then drop WriteBatch (simulating mid-write I/O failure)
        // AND drop conn_a WITHOUT receiving RELEASE or peer_end!
        batch.begin_io().unwrap();
        drop(batch);
        conn_a.deactivate().release();
    }

    // Peer B settles cleanly; `block` (ring ref = 1) + conn_a's abandoned
    // published loan (ref = 1) = 2 before `block` drops!
    send_client_release(&conn_b, loan_b);
    assert_eq!(settled_b.load(Ordering::Acquire), 1);
    assert_eq!(settled_a.load(Ordering::Acquire), 0);
    assert_eq!(block.active_refs(), 2);
    drop(block);

    // ADR 0001 invariant: unconfirmed published loan on conn_a keeps the
    // dedicated allocation pinned and charged rather than reusing it!
    assert!(budget.snapshot().memory.used_bytes >= 128 * 1024);
    assert_eq!(pool.read().stats().dedicated_segments, 1);
}

#[test]
fn stalled_peer_settled_only_by_real_peer_end_reclaims_shared_block() {
    let (budget, _config, _prefix, pool) = make_pool(64 * 1024, 4 * 1024 * 1024);
    let payload = vec![77u8; 128 * 1024];
    let block = SealedShmBlock::from_bytes(Arc::clone(&pool), &payload).unwrap();

    let conn_a = server_connection();
    let conn_b = server_connection();
    let settled_a = Arc::new(AtomicUsize::new(0));
    let settled_b = Arc::new(AtomicUsize::new(0));

    let _loan_a = publish_shared(&conn_a, &block, Arc::clone(&settled_a));
    let loan_b = publish_shared(&conn_b, &block, Arc::clone(&settled_b));
    drop(block);

    // Peer B releases normally, peer A is stalled (no RELEASE).
    send_client_release(&conn_b, loan_b);
    assert_eq!(settled_b.load(Ordering::Acquire), 1);
    assert_eq!(settled_a.load(Ordering::Acquire), 0);
    assert!(budget.snapshot().memory.used_bytes >= 128 * 1024);

    // Deactivation alone does NOT reclaim peer A's published loan.
    conn_a.deactivate().release();
    assert_eq!(settled_a.load(Ordering::Acquire), 0);
    assert!(budget.snapshot().memory.used_bytes >= 128 * 1024);

    // Real read-side peer_end settles peer A's loan and refunds server.M!
    conn_a.peer_end().release();
    assert_eq!(settled_a.load(Ordering::Acquire), 1);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn dual_concurrent_reads_on_same_dedicated_segment_stay_mapped_until_both_drop() {
    let (_budget, config, prefix, pool) = make_pool(64 * 1024, 4 * 1024 * 1024);
    let payload = vec![0x3Cu8; 128 * 1024];
    let block = SealedShmBlock::from_bytes(Arc::clone(&pool), &payload).unwrap();
    let alloc = block.allocation();
    assert!(alloc.is_dedicated);

    let cache = PeerMappingCache::new(config, prefix).unwrap();
    let r1 = cache
        .admit(alloc.seg_idx as u16, alloc.generation, 0, payload.len() as u32, true)
        .unwrap();
    let r2 = cache
        .admit(alloc.seg_idx as u16, alloc.generation, 0, payload.len() as u32, true)
        .unwrap();
    assert_eq!(cache.mapped_segments(), 1);

    drop(r1);
    assert_eq!(cache.mapped_segments(), 1);
    let mut out = vec![0u8; payload.len()];
    // SAFETY: `block` keeps the sealed immutable allocation alive.
    unsafe { r2.copy_into(&mut out) }.unwrap();
    assert_eq!(out, payload);
    drop(r2);
    assert_eq!(cache.mapped_segments(), 0);
    drop(block);
}
