use c2_mem::{MemHandle, MemPool, PeerMappingCache, PeerMappingError, PoolAllocation, PoolConfig};
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
fn dedicated_reads_use_actual_os_span() {
    let mut owner_config = config();
    owner_config.max_segments = 0;
    let mut owner = owner(owner_config);
    let mut handle = owner.alloc_shm_handle(5000).unwrap();
    owner.handle_slice_mut(&mut handle).fill(0x31);
    let allocation = allocation(&handle);
    assert!(allocation.is_dedicated);

    let mut peer_config = config();
    peer_config.segment_size = 8 * 1024;
    let peer = PeerMappingCache::new(peer_config, owner.prefix().to_owned()).unwrap();
    let span = c2_mem::DedicatedSegment::required_shm_size(5000).unwrap() as u32;
    assert_eq!(allocation.actual_size, span);
    let full = peer.admit(0, allocation.generation, 0, span, true).unwrap();
    assert_eq!(full.len(), span as usize);
    assert!(matches!(
        peer.admit(0, allocation.generation, 0, span + 1, true),
        Err(PeerMappingError::InvalidDescriptor(_))
    ));
    drop(full);
    let read = peer.admit(0, allocation.generation, 0, 5000, true).unwrap();
    assert_eq!(unsafe { read.copy() }.unwrap(), vec![0x31; 5000]);
    drop(read);
    owner.release_handle(handle);
}

#[test]
fn peer_reads_use_actual_os_span_and_reject_wrong_identity_or_kind() {
    let mut owner_config = config();
    owner_config.segment_size = 64 * 1024;
    let mut owner = owner(owner_config);
    let mut buddy = owner.alloc_shm_handle(64 * 1024).unwrap();
    owner.handle_slice_mut(&mut buddy)[64 * 1024 - 1] = 0x5e;
    let identity = allocation(&buddy);
    let slot = identity.seg_idx as u16;

    let mut peer_config = config();
    peer_config.segment_size = 8 * 1024;
    let peer = PeerMappingCache::new(peer_config, owner.prefix().to_owned()).unwrap();
    let read = peer
        .admit(slot, identity.generation, 64 * 1024 - 1, 1, false)
        .unwrap();
    assert_eq!(unsafe { read.copy() }.unwrap(), [0x5e]);
    assert!(matches!(
        peer.admit(slot, identity.generation, 64 * 1024, 1, false),
        Err(PeerMappingError::InvalidDescriptor(_))
    ));
    assert!(matches!(
        peer.admit(slot, identity.generation + 1, 0, 1, false),
        Err(PeerMappingError::Open(_))
    ));
    assert!(matches!(
        peer.admit(slot, identity.generation, 0, 1, true),
        Err(PeerMappingError::Open(_))
    ));
    assert_eq!(owner.stats().alloc_count, 1);
    drop(read);
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
