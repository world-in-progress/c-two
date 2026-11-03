//! Read-only peer mappings, pinned only for an admitted remote read.
//!
//! IPC clients and servers keep one [`PeerMappingCache`] per physical
//! connection and advertised pool prefix. At frame admission, call `admit`
//! after admitting the connection's remote loan. Store the returned
//! [`PeerRead`] with that loan in the receive carrier; copy or discard the
//! read before dropping the loan and sending its release record. Neither
//! application inputs nor holds retain this carrier. The connection's
//! existing maintenance task calls `gc_idle`, including while no calls run.
//! Pass the resolved `pool_decay_seconds` as `buddy_idle_decay_secs`.

use crate::pool::check_span;
use crate::{MemPool, PoolConfig, ShmRegion};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

type Slot = (u16, bool);
type Slots = Mutex<HashMap<Slot, Entry>>;

struct Entry {
    // Retained even after unmapping, so retirement never resets identity.
    generation: u32,
    mapping: Option<Arc<ShmRegion>>,
    reads: usize,
    idle_since: Option<Instant>,
}

#[derive(Debug)]
pub enum PeerMappingError {
    InvalidDescriptor(String),
    Open(String),
}

impl std::fmt::Display for PeerMappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDescriptor(message) | Self::Open(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PeerMappingError {}

/// Cache for one peer pool incarnation. It never allocates or frees the
/// peer's storage and never charges local owner-backing budgets.
pub struct PeerMappingCache {
    prefix: String,
    config: PoolConfig,
    slots: Arc<Slots>,
}

impl PeerMappingCache {
    pub fn new(config: PoolConfig, prefix: String) -> Result<Self, String> {
        // Reuse the pool's boundary validation without opening any mapping.
        MemPool::open_peer(config.clone(), prefix.clone())?;
        Ok(Self {
            prefix,
            config,
            slots: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Validate one descriptor against its actual read-only OS mapping.
    /// A successful read pins that exact generation even if a newer one is
    /// admitted at the same slot. A failed admission does not pin a mapping.
    pub fn admit(
        &self,
        slot: u16,
        generation: u32,
        offset: u32,
        len: u32,
        dedicated: bool,
    ) -> Result<PeerRead, PeerMappingError> {
        let limit = if dedicated {
            self.config.max_dedicated_segments
        } else {
            self.config.max_segments
        };
        if usize::from(slot) >= limit || generation == 0 || len == 0 || (dedicated && offset != 0) {
            return Err(PeerMappingError::InvalidDescriptor(
                "invalid peer backing identity".into(),
            ));
        }
        let key = (slot, dedicated);
        let mut slots = self.slots.lock().unwrap();
        let entry = slots.entry(key).or_insert(Entry {
            generation: 0,
            mapping: None,
            reads: 0,
            idle_since: None,
        });
        if generation < entry.generation {
            return Err(PeerMappingError::InvalidDescriptor(
                "stale shared-memory backing generation".into(),
            ));
        }
        if generation != entry.generation || entry.mapping.is_none() {
            let name = if dedicated {
                MemPool::dedicated_segment_name(&self.prefix, slot.into(), generation)
            } else {
                MemPool::buddy_segment_name(&self.prefix, slot.into(), generation)
            };
            let mapping = ShmRegion::open(&name, 1).map_err(PeerMappingError::Open)?;
            entry.generation = generation;
            entry.mapping = Some(Arc::new(mapping));
            entry.reads = 0;
            entry.idle_since = Some(Instant::now());
        }
        let mapping = entry.mapping.as_ref().unwrap();
        let valid = check_span(mapping.size(), offset as usize, len as usize);
        if let Err(error) = valid {
            if dedicated && entry.reads == 0 {
                entry.mapping = None;
            }
            return Err(PeerMappingError::InvalidDescriptor(error));
        }
        entry.reads += 1;
        entry.idle_since = None;
        Ok(PeerRead {
            cache: Arc::downgrade(&self.slots),
            mapping: mapping.clone(),
            key,
            generation,
            offset: offset as usize,
            len: len as usize,
        })
    }

    /// Retire all idle buddy views after the configured decay window.
    /// Active reads are pinned independently of cache replacement. Receiver
    /// views have no retained minimum and no trailing-slot restriction.
    pub fn gc_idle(&self) -> usize {
        let delay = if self.config.buddy_idle_decay_secs < 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.config.buddy_idle_decay_secs)
        };
        let now = Instant::now();
        let mut slots = self.slots.lock().unwrap();
        let mut removed = 0;
        for ((_, dedicated), entry) in slots.iter_mut() {
            if !dedicated
                && entry.reads == 0
                && entry
                    .idle_since
                    .is_some_and(|idle| now.duration_since(idle) >= delay)
                && entry.mapping.is_some()
            {
                entry.mapping = None;
                entry.idle_since = None;
                removed += 1;
            }
        }
        removed
    }

    /// Number of live cached OS mappings; excludes replaced generations
    /// still pinned by a receive carrier.
    pub fn mapped_segments(&self) -> usize {
        self.slots
            .lock()
            .unwrap()
            .values()
            .filter(|entry| entry.mapping.is_some())
            .count()
    }
}

/// A move-only, validated read span bound to its original OS mapping.
/// Dropping the last read immediately closes a dedicated view, or starts a
/// buddy view's idle window. This grants no allocator or release authority.
pub struct PeerRead {
    cache: Weak<Slots>,
    mapping: Arc<ShmRegion>,
    key: Slot,
    generation: u32,
    offset: usize,
    len: usize,
}

impl PeerRead {
    /// # Safety
    /// The publishing peer must keep the span immutable until this read
    /// ends. Keep the connection's remote loan alive through this copy.
    pub unsafe fn copy_into(&self, destination: &mut [u8]) -> Result<(), String> {
        if destination.len() != self.len {
            return Err("destination length does not match the admitted peer span".into());
        }
        // SAFETY: admission checked the span and this owner pins the mapping;
        // the caller supplies the peer's read interval and nonaliasing output.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mapping.base_ptr().add(self.offset),
                destination.as_mut_ptr(),
                self.len,
            );
        }
        Ok(())
    }

    /// # Safety
    /// Same peer immutability and remote-loan contract as `copy_into`.
    pub unsafe fn copy(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(self.len)
            .map_err(|error| format!("peer copy allocation failed: {error}"))?;
        // SAFETY: admission checked the source span, this read pins it, and
        // reserve provided enough disjoint storage. Initialize directly with
        // the one payload copy before exposing the vector's length.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mapping.base_ptr().add(self.offset),
                bytes.as_mut_ptr(),
                self.len,
            );
            bytes.set_len(self.len);
        }
        Ok(bytes)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for PeerRead {
    fn drop(&mut self) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        let mut slots = cache.lock().unwrap();
        let entry = slots.get_mut(&self.key).unwrap();
        if entry.generation == self.generation
            && entry
                .mapping
                .as_ref()
                .is_some_and(|mapping| Arc::ptr_eq(mapping, &self.mapping))
        {
            // Arc fields drop after this destructor releases the cache lock.
            // Count carriers under that lock so simultaneous final drops
            // cannot both miss the transition to an idle mapping.
            entry.reads -= 1;
            if entry.reads == 0 {
                if self.key.1 {
                    entry.mapping = None;
                } else {
                    entry.idle_since = Some(Instant::now());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemHandle, PoolAllocation};

    fn config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            max_segments: 2,
            max_dedicated_segments: 2,
            buddy_idle_decay_secs: 0.0,
            min_retained_segments: 0,
            spill_threshold: 1.0,
            ..PoolConfig::default()
        }
    }

    fn payload(pool: &mut MemPool, len: usize, byte: u8) -> (MemHandle, PoolAllocation) {
        let mut handle = pool.alloc_shm_handle(len).unwrap();
        pool.handle_slice_mut(&mut handle).fill(byte);
        let allocation = handle.shm_allocation().unwrap();
        (handle, allocation)
    }

    fn admit(cache: &PeerMappingCache, allocation: PoolAllocation, len: u32) -> PeerRead {
        cache
            .admit(
                allocation.seg_idx as u16,
                allocation.generation,
                allocation.offset,
                len,
                allocation.is_dedicated,
            )
            .unwrap()
    }

    #[test]
    fn dedicated_copy_unmaps_after_last_carrier() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 1024 * 1024, 71);
        assert!(allocation.is_dedicated);
        let read = admit(&cache, allocation, 1024 * 1024);
        let mapping = Arc::downgrade(&read.mapping);
        let other = admit(&cache, allocation, 1024 * 1024);
        assert_eq!(unsafe { read.copy() }.unwrap(), vec![71; 1024 * 1024]);
        drop(read);
        assert_eq!(cache.mapped_segments(), 1);
        assert!(mapping.upgrade().is_some());
        drop(other);
        assert_eq!(cache.mapped_segments(), 0);
        assert!(
            mapping.upgrade().is_none(),
            "the OS view was dropped without waiting for GC"
        );
        owner.release_handle(handle);
    }

    #[test]
    fn owner_retired_buddy_unmaps_only_after_last_read_and_decay() {
        let mut owner = MemPool::new(config()).unwrap();
        let mut peer_config = config();
        peer_config.buddy_idle_decay_secs = 60.0;
        let cache = PeerMappingCache::new(peer_config, owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 4096, 39);
        let read = admit(&cache, allocation, 4096);
        let mapping = Arc::downgrade(&read.mapping);
        // No new carrier is allowed to lose its original mapping to GC.
        assert_eq!(cache.gc_idle(), 0);
        assert!(
            cache.slots.lock().unwrap()[&(allocation.seg_idx as u16, false)]
                .idle_since
                .is_none()
        );
        assert_eq!(unsafe { read.copy() }.unwrap(), vec![39; 4096]);
        drop(read);
        owner.release_handle(handle);
        assert_eq!(owner.gc_buddy(), 1);
        assert_eq!(
            cache.gc_idle(),
            0,
            "the decay starts at the final carrier's end"
        );
        cache
            .slots
            .lock()
            .unwrap()
            .get_mut(&(allocation.seg_idx as u16, false))
            .unwrap()
            .idle_since = Some(Instant::now() - Duration::from_secs(61));
        assert_eq!(cache.gc_idle(), 1);
        assert_eq!(cache.mapped_segments(), 0);
        assert!(mapping.upgrade().is_none());
    }

    #[test]
    fn buddy_inflight_carrier_survives_zero_decay_gc() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 4096, 13);
        let first = admit(&cache, allocation, 4096);
        let last = admit(&cache, allocation, 4096);
        drop(first);
        assert_eq!(cache.gc_idle(), 0);
        assert_eq!(unsafe { last.copy() }.unwrap(), vec![13; 4096]);
        drop(last);
        assert_eq!(cache.gc_idle(), 1);
        owner.release_handle(handle);
    }

    #[test]
    fn concurrent_final_carriers_leave_no_idle_mapping_pinned() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 4096, 13);
        for _ in 0..64 {
            let first = admit(&cache, allocation, 4096);
            let last = admit(&cache, allocation, 4096);
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    drop(first);
                });
                scope.spawn(|| {
                    barrier.wait();
                    drop(last);
                });
            });
            assert_eq!(cache.gc_idle(), 1);
            assert_eq!(cache.mapped_segments(), 0);
        }
        owner.release_handle(handle);
    }

    #[test]
    fn retirement_and_replacement_keep_generation_high_water() {
        for len in [4096, 16384] {
            let mut owner = MemPool::new(config()).unwrap();
            let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
            let (old_handle, old) = payload(&mut owner, len, 21);
            let prefix = owner.prefix().to_owned();
            let old_name = if old.is_dedicated {
                owner.dedicated_name(old.seg_idx).unwrap()
            } else {
                owner.segment_name(old.seg_idx as usize).unwrap()
            }
            .to_owned();
            let old_read = admit(&cache, old, len as u32);
            owner.release_handle(old_handle);
            owner.gc_buddy();
            let (new_handle, new) = payload(&mut owner, len, 42);
            assert_eq!(old.seg_idx, new.seg_idx);
            assert!(new.generation > old.generation);
            assert_eq!(owner.prefix(), prefix);
            let new_name = if new.is_dedicated {
                owner.dedicated_name(new.seg_idx).unwrap()
            } else {
                owner.segment_name(new.seg_idx as usize).unwrap()
            };
            assert_ne!(new_name, old_name);
            let new_read = admit(&cache, new, len as u32);
            assert_eq!(unsafe { old_read.copy() }.unwrap(), vec![21; len]);
            drop(old_read);
            assert_eq!(cache.gc_idle(), 0);
            assert_eq!(unsafe { new_read.copy() }.unwrap(), vec![42; len]);
            drop(new_read);
            cache.gc_idle();
            assert_eq!(cache.mapped_segments(), 0);
            assert!(matches!(
                cache.admit(
                    old.seg_idx as u16,
                    old.generation,
                    old.offset,
                    len as u32,
                    old.is_dedicated
                ),
                Err(PeerMappingError::InvalidDescriptor(_))
            ));
            // Retirement can reopen the current buddy generation while the
            // owner still has it. Its high-water mark must not reject it.
            drop(admit(&cache, new, len as u32));
            owner.release_handle(new_handle);
        }
    }

    #[test]
    fn failed_dedicated_span_does_not_leave_mapping_cached() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 16384, 7);
        assert!(matches!(
            cache.admit(
                allocation.seg_idx as u16,
                allocation.generation,
                0,
                u32::MAX,
                true
            ),
            Err(PeerMappingError::InvalidDescriptor(_))
        ));
        assert_eq!(cache.mapped_segments(), 0);
        owner.release_handle(handle);
    }

    #[test]
    fn admission_rejects_invalid_identity_and_spans() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        for len in [4096, 16384] {
            let (handle, allocation) = payload(&mut owner, len, 7);
            let slot = allocation.seg_idx as u16;
            let generation = allocation.generation;
            let dedicated = allocation.is_dedicated;
            let read = admit(&cache, allocation, len as u32);
            let span = read.mapping.size() as u32;
            let mut invalid = vec![
                (slot, 0, 0, 1, dedicated),
                (2, generation, 0, 1, dedicated),
                (slot, generation, 0, 0, dedicated),
                (slot, generation, 0, u32::MAX, dedicated),
            ];
            if dedicated {
                invalid.push((slot, generation, 1, 1, true));
            } else {
                invalid.push((slot, generation, span - 1, 2, false));
            }
            for (slot, generation, offset, len, dedicated) in invalid {
                assert!(matches!(
                    cache.admit(slot, generation, offset, len, dedicated),
                    Err(PeerMappingError::InvalidDescriptor(_))
                ));
            }
            assert_eq!(unsafe { read.copy() }.unwrap(), vec![7; len]);
            drop(read);
            owner.release_handle(handle);
        }
    }

    #[test]
    fn opening_lower_slot_keeps_higher_peer_mapping() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (low_handle, low) = payload(&mut owner, 8192, 0x12);
        let (high_handle, high) = payload(&mut owner, 8192, 0x56);
        assert!(low.seg_idx < high.seg_idx);
        let high_read = admit(&cache, high, 8192);
        let low_read = admit(&cache, low, 8192);
        assert_eq!(unsafe { high_read.copy() }.unwrap(), vec![0x56; 8192]);
        assert_eq!(unsafe { low_read.copy() }.unwrap(), vec![0x12; 8192]);
        assert_eq!(cache.mapped_segments(), 2);
        assert_eq!(owner.stats().alloc_count, 2);
        drop((high_read, low_read));
        owner.release_handle(high_handle);
        owner.release_handle(low_handle);
    }

    #[test]
    fn admitted_read_survives_cache_drop() {
        let mut owner = MemPool::new(config()).unwrap();
        let cache = PeerMappingCache::new(config(), owner.prefix().into()).unwrap();
        let (handle, allocation) = payload(&mut owner, 4096, 5);
        let read = admit(&cache, allocation, 4096);
        let mapping = Arc::downgrade(&read.mapping);
        drop(cache);
        assert_eq!(unsafe { read.copy() }.unwrap(), vec![5; 4096]);
        drop(read);
        assert!(mapping.upgrade().is_none());
        owner.release_handle(handle);
    }
}
