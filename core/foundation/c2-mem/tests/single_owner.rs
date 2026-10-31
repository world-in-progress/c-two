use c2_mem::{MemHandle, MemPool, PoolAllocation, PoolConfig};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SEGMENT_SIZE: usize = 32 * 1024;

fn config() -> PoolConfig {
    PoolConfig {
        segment_size: SEGMENT_SIZE,
        min_block_size: 4096,
        max_segments: 1,
        max_dedicated_segments: 1,
        buddy_idle_decay_secs: 0.0,
        spill_threshold: 1.0,
        min_retained_segments: 0,
        ..PoolConfig::default()
    }
}

fn owner(config: PoolConfig) -> MemPool {
    MemPool::new_with_prefix(
        config,
        format!("/c2_single_owner_test_{}", std::process::id()),
    )
    .unwrap()
}

fn allocation(handle: &MemHandle) -> PoolAllocation {
    handle.shm_allocation().expect("test requires SHM")
}

#[test]
fn moved_loan_retains_its_original_pool_until_local_release() {
    let mut no_fallback = config();
    no_fallback.max_dedicated_segments = 0;
    let pool = Arc::new(Mutex::new(owner(no_fallback)));
    let handle = pool.lock().unwrap().alloc_shm_handle(SEGMENT_SIZE).unwrap();
    assert!(handle.is_buddy());
    let pool_for_loan = Arc::clone(&pool);
    let weak = Arc::downgrade(&pool);
    drop(pool);
    assert!(
        weak.upgrade().is_some(),
        "the loan retains the original pool"
    );

    std::thread::spawn(move || {
        let mut loans: HashMap<u64, (Arc<Mutex<MemPool>>, MemHandle)> = HashMap::new();
        loans.insert(1, (pool_for_loan, handle));
        let (origin, _) = loans.get(&1).unwrap();
        let mut locked = origin.lock().unwrap();
        assert_eq!(
            locked.gc_buddy(),
            0,
            "a borrowed block keeps its backing live"
        );
        assert_eq!(locked.stats().alloc_count, 1);
        assert!(locked.alloc_shm_handle(SEGMENT_SIZE).is_err());
        drop(locked);

        let (origin, handle) = loans.remove(&1).unwrap();
        assert!(loans.is_empty());
        let mut locked = origin.lock().unwrap();
        locked.release_handle(handle);
        assert_eq!(locked.stats().alloc_count, 0);
        assert_eq!(locked.gc_buddy(), 1);
        assert_eq!(locked.stats().total_segments, 0);
        let next = locked.alloc_shm_handle(SEGMENT_SIZE).unwrap();
        assert!(next.is_buddy());
        locked.release_handle(next);
    })
    .join()
    .unwrap();

    assert!(
        weak.upgrade().is_none(),
        "the last loan owner dropped the pool"
    );
}

#[test]
fn buddy_slot_reuse_preserves_old_peer_mapping_and_pool_prefix() {
    let mut owner = owner(config());
    let original_prefix = owner.prefix().to_owned();
    let mut old = owner.alloc_shm_handle(SEGMENT_SIZE).unwrap();
    owner.handle_slice_mut(&mut old).fill(0x3a);
    let first = allocation(&old);
    assert_eq!(first.seg_idx, 0);
    assert_ne!(first.generation, 0);
    let first_name = owner.segment_name(0).unwrap().to_owned();

    let mut old_peer = MemPool::open_peer(config(), original_prefix.clone()).unwrap();
    old_peer
        .ensure_peer_segment(first.seg_idx, first.generation, SEGMENT_SIZE)
        .unwrap();
    owner.release_handle(old);
    assert_eq!(owner.gc_buddy(), 1);

    let mut fresh = owner.alloc_shm_handle(SEGMENT_SIZE).unwrap();
    owner.handle_slice_mut(&mut fresh).fill(0x9b);
    let second = allocation(&fresh);
    assert_eq!(second.seg_idx, first.seg_idx);
    assert!(second.generation > first.generation);
    assert_eq!(owner.prefix(), original_prefix);
    assert_ne!(owner.segment_name(0).unwrap(), first_name);
    assert_eq!(
        unsafe { old_peer.copy_data_at(first.seg_idx, first.generation, 0, 1, false) }.unwrap(),
        [0x3a]
    );
    let mut new_peer = MemPool::open_peer(config(), original_prefix).unwrap();
    new_peer
        .ensure_peer_segment(second.seg_idx, second.generation, SEGMENT_SIZE)
        .unwrap();
    assert!(
        new_peer
            .ensure_peer_segment(first.seg_idx, first.generation, SEGMENT_SIZE)
            .is_err()
    );
    assert!(
        old_peer
            .validate_data_at(first.seg_idx, second.generation, 0, 1, false)
            .is_err()
    );
    assert_eq!(
        unsafe { new_peer.copy_data_at(second.seg_idx, second.generation, 0, 1, false) }.unwrap(),
        [0x9b]
    );
    owner.release_handle(fresh);
}

#[test]
fn dedicated_slot_reuse_preserves_retained_old_peer_view() {
    let mut owner_config = config();
    owner_config.max_segments = 0;
    let mut owner = owner(owner_config.clone());
    let original_prefix = owner.prefix().to_owned();
    let mut old = owner.alloc_shm_handle(5000).unwrap();
    owner.handle_slice_mut(&mut old)[0] = 0x31;
    let first = allocation(&old);
    assert!(first.is_dedicated);
    assert_eq!(first.seg_idx, 0);
    assert_ne!(first.generation, 0);
    let first_name = owner.dedicated_name(0).unwrap().to_owned();

    let mut peer_config = owner_config.clone();
    peer_config.segment_size = 8 * 1024;
    let mut old_peer = MemPool::open_peer(peer_config.clone(), original_prefix.clone()).unwrap();
    old_peer
        .ensure_peer_dedicated(first.seg_idx, first.generation, 5000)
        .unwrap();
    let span = c2_mem::DedicatedSegment::required_shm_size(5000).unwrap() as u32;
    assert_eq!(first.actual_size, span);
    assert!(
        old_peer
            .validate_data_at(first.seg_idx, first.generation, 0, span, true)
            .is_ok()
    );
    assert!(
        old_peer
            .validate_data_at(first.seg_idx, first.generation, 0, span + 1, true)
            .is_err()
    );
    owner.release_handle(old);

    let mut fresh = owner.alloc_shm_handle(5000).unwrap();
    owner.handle_slice_mut(&mut fresh)[0] = 0xa4;
    let second = allocation(&fresh);
    assert_eq!(second.seg_idx, first.seg_idx);
    assert!(second.generation > first.generation);
    assert_eq!(owner.prefix(), original_prefix);
    assert_ne!(owner.dedicated_name(0).unwrap(), first_name);
    assert_eq!(
        unsafe { old_peer.copy_data_at(first.seg_idx, first.generation, 0, 1, true) }.unwrap(),
        [0x31]
    );

    let mut new_peer = MemPool::open_peer(peer_config, original_prefix).unwrap();
    new_peer
        .ensure_peer_dedicated(second.seg_idx, second.generation, 5000)
        .unwrap();
    assert_eq!(
        unsafe { new_peer.copy_data_at(second.seg_idx, second.generation, 0, 1, true) }.unwrap(),
        [0xa4]
    );
    assert!(
        new_peer
            .validate_data_at(first.seg_idx, first.generation, 0, 1, true)
            .is_err()
    );
    owner.release_handle(fresh);
}

#[test]
fn peer_reads_use_actual_os_span_and_reject_wrong_identity_or_kind() {
    let mut owner_config = config();
    owner_config.segment_size = 64 * 1024;
    let mut owner = owner(owner_config);
    let mut buddy = owner.alloc_shm_handle(64 * 1024).unwrap();
    owner.handle_slice_mut(&mut buddy)[64 * 1024 - 1] = 0x5e;
    let identity = allocation(&buddy);

    let mut peer_config = config();
    peer_config.segment_size = 8 * 1024;
    let mut peer = MemPool::open_peer(peer_config, owner.prefix().to_owned()).unwrap();
    peer.ensure_peer_segment(identity.seg_idx, identity.generation, 64 * 1024)
        .unwrap();
    assert_eq!(peer.segment(0).unwrap().size(), 64 * 1024);
    assert_eq!(
        unsafe {
            peer.copy_data_at(
                identity.seg_idx,
                identity.generation,
                64 * 1024 - 1,
                1,
                false,
            )
        }
        .unwrap(),
        [0x5e]
    );
    assert!(
        peer.validate_data_at(identity.seg_idx, identity.generation, 64 * 1024, 1, false)
            .is_err()
    );
    assert!(
        peer.validate_data_at(identity.seg_idx, identity.generation + 1, 0, 1, false)
            .is_err()
    );
    assert!(
        peer.validate_data_at(identity.seg_idx, identity.generation, 0, 1, true)
            .is_err()
    );
    assert!(
        peer.ensure_peer_dedicated(identity.seg_idx, identity.generation, 1)
            .is_err()
    );
    assert!(
        unsafe {
            peer.free_at(
                identity.seg_idx,
                identity.generation,
                identity.offset,
                identity.level,
                false,
            )
        }
        .is_err()
    );
    assert_eq!(owner.stats().alloc_count, 1);
    owner.release_handle(buddy);
}

#[test]
fn trimmed_handle_releases_full_buddy_capacity() {
    let mut retained_config = config();
    retained_config.min_retained_segments = 1;
    let mut owner = owner(retained_config);
    let mut handle = owner.alloc_shm_handle(SEGMENT_SIZE).unwrap();
    let first = allocation(&handle);
    handle.trim_to(1).unwrap();
    assert_eq!(handle.len(), 1);
    owner.release_handle(handle);

    let full = owner.alloc_shm_handle(SEGMENT_SIZE).unwrap();
    let second = allocation(&full);
    assert_eq!(
        (second.seg_idx, second.generation, second.offset),
        (first.seg_idx, first.generation, first.offset)
    );
    assert_eq!(second.actual_size as usize, SEGMENT_SIZE);
    owner.release_handle(full);
}
