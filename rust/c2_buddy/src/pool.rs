//! Multi-segment buddy pool with dedicated segment fallback.
//!
//! The pool manages multiple buddy-allocated SHM segments and automatically
//! creates dedicated segments for oversized payloads. Implements the three-layer
//! fallback strategy from buddy.md:
//!   1. Try existing segments
//!   2. Create new segment (up to max_segments)
//!   3. Fall back to dedicated segment

use crate::segment::{DedicatedSegment, ShmSegment};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

/// Configuration for the buddy pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Size of each buddy segment (default 256MB).
    pub segment_size: usize,
    /// Minimum allocation block (default 4KB).
    pub min_block_size: usize,
    /// Maximum number of buddy segments (default 8).
    pub max_segments: usize,
    /// Maximum number of dedicated segments (default 4).
    pub max_dedicated_segments: usize,
    /// Delay before reclaiming empty dedicated segments (seconds).
    pub dedicated_gc_delay_secs: f64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            segment_size: 256 * 1024 * 1024,
            min_block_size: 4096,
            max_segments: 8,
            max_dedicated_segments: 4,
            dedicated_gc_delay_secs: 5.0,
        }
    }
}

/// Result of a pool allocation.
#[derive(Debug, Clone, Copy)]
pub struct PoolAllocation {
    /// Index of the segment (buddy or dedicated).
    pub seg_idx: u32,
    /// Offset within the segment's data region.
    pub offset: u32,
    /// Actual allocated size.
    pub actual_size: u32,
    /// Buddy level (only meaningful for buddy segments).
    pub level: u16,
    /// Whether this is a dedicated segment.
    pub is_dedicated: bool,
}

/// Statistics about the pool.
#[derive(Debug, Clone)]
pub struct PoolStats {
    pub total_segments: usize,
    pub dedicated_segments: usize,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub alloc_count: u32,
    pub fragmentation_ratio: f64,
}

/// Tracking info for a dedicated segment.
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,
}

/// Multi-segment buddy pool.
pub struct BuddyPool {
    config: PoolConfig,
    /// Buddy-managed segments.
    segments: Vec<ShmSegment>,
    /// Dedicated segments for oversized allocations. Key is segment index.
    dedicated: HashMap<u32, DedicatedEntry>,
    /// Counter for generating unique SHM names.
    name_counter: AtomicU32,
    /// Base name prefix for SHM segments (includes PID).
    name_prefix: String,
    /// Next dedicated segment index (offset from max_segments to avoid collision).
    next_dedicated_idx: u32,
}

impl BuddyPool {
    /// Create a new pool. Segments are lazily created on first alloc.
    pub fn new(config: PoolConfig) -> Self {
        let pid = std::process::id();
        let name_prefix = format!("/cc3b{:08x}", pid);
        Self::new_with_prefix(config, name_prefix)
    }

    /// Create a new pool with a custom name prefix (for testing / multi-pool).
    pub fn new_with_prefix(config: PoolConfig, name_prefix: String) -> Self {
        Self::validate_config(&config).expect("invalid PoolConfig");
        Self {
            config,
            segments: Vec::new(),
            dedicated: HashMap::new(),
            name_counter: AtomicU32::new(0),
            name_prefix,
            next_dedicated_idx: 256,
        }
    }

    /// Validate pool configuration, returning Err on invalid values.
    pub fn validate_config(config: &PoolConfig) -> Result<(), String> {
        if config.min_block_size == 0 || !config.min_block_size.is_power_of_two() {
            return Err(format!(
                "min_block_size must be a positive power of 2, got {}",
                config.min_block_size
            ));
        }
        if config.segment_size < 2 * config.min_block_size {
            return Err(format!(
                "segment_size ({}) must be >= 2 * min_block_size ({})",
                config.segment_size, config.min_block_size
            ));
        }
        if config.dedicated_gc_delay_secs.is_nan() {
            return Err("dedicated_gc_delay_secs must not be NaN".into());
        }
        Ok(())
    }

    /// Allocate memory from the pool.
    pub fn alloc(&mut self, size: usize) -> Result<PoolAllocation, String> {
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }

        let max_buddy_block = self.max_buddy_block_size();

        if max_buddy_block > 0 && size <= max_buddy_block {
            // Try buddy allocation.
            self.alloc_buddy(size)
        } else {
            // Too large for buddy → dedicated segment.
            self.alloc_dedicated(size)
        }
    }

    /// Free a previously allocated block.
    pub fn free(&mut self, alloc: &PoolAllocation) -> Result<(), String> {
        if alloc.is_dedicated {
            self.free_dedicated(alloc.seg_idx);
            Ok(())
        } else {
            self.free_buddy(alloc)
        }
    }

    /// Get a raw pointer to data for a given allocation.
    pub fn data_ptr(&self, alloc: &PoolAllocation) -> Result<*mut u8, String> {
        if alloc.is_dedicated {
            let entry = self
                .dedicated
                .get(&alloc.seg_idx)
                .ok_or("invalid dedicated segment index")?;
            Ok(entry.segment.data_ptr())
        } else {
            let seg = self
                .segments
                .get(alloc.seg_idx as usize)
                .ok_or("invalid segment index")?;
            Ok(seg.allocator().data_ptr(alloc.offset))
        }
    }

    /// Get pool statistics.
    pub fn stats(&self) -> PoolStats {
        let mut total_bytes = 0u64;
        let mut free_bytes = 0u64;
        let mut alloc_count = 0u32;

        for seg in &self.segments {
            let a = seg.allocator();
            total_bytes += a.data_size() as u64;
            free_bytes += a.free_bytes();
            alloc_count += a.alloc_count();
        }

        for entry in self.dedicated.values() {
            total_bytes += entry.segment.size() as u64;
            if entry.freed_at.is_none() {
                alloc_count += 1;
            }
        }

        let fragmentation_ratio = if total_bytes > 0 {
            1.0 - (free_bytes as f64 / total_bytes as f64)
        } else {
            0.0
        };

        PoolStats {
            total_segments: self.segments.len(),
            dedicated_segments: self.dedicated.len(),
            total_bytes,
            free_bytes,
            alloc_count,
            fragmentation_ratio,
        }
    }

    /// Run garbage collection on freed dedicated segments.
    pub fn gc_dedicated(&mut self) {
        let secs = self.config.dedicated_gc_delay_secs;
        let delay = if secs < 0.0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(secs)
        };
        let now = Instant::now();
        let to_remove: Vec<u32> = self
            .dedicated
            .iter()
            .filter_map(|(&idx, entry)| {
                if let Some(freed_at) = entry.freed_at {
                    if now.duration_since(freed_at) >= delay {
                        return Some(idx);
                    }
                }
                None
            })
            .collect();

        for idx in to_remove {
            self.dedicated.remove(&idx); // Drop triggers munmap + shm_unlink.
        }
    }

    /// Get the number of buddy segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Get a specific segment by index.
    pub fn segment(&self, idx: usize) -> Option<&ShmSegment> {
        self.segments.get(idx)
    }

    /// Destroy the pool, cleaning up all SHM segments.
    pub fn destroy(&mut self) {
        self.dedicated.clear();
        self.segments.clear(); // Drop triggers munmap + shm_unlink for owned segments.
    }

    /// Get the SHM name for a buddy segment.
    pub fn segment_name(&self, idx: usize) -> Option<&str> {
        self.segments.get(idx).map(|s| s.name())
    }

    /// Get the SHM name for a dedicated segment.
    pub fn dedicated_name(&self, idx: u32) -> Option<&str> {
        self.dedicated.get(&idx).map(|e| e.segment.name())
    }

    /// Open an existing buddy segment (for the remote side of a connection).
    pub fn open_segment(&mut self, name: &str, size: usize) -> Result<usize, String> {
        let seg = ShmSegment::open(name, size)?;
        let idx = self.segments.len();
        self.segments.push(seg);
        Ok(idx)
    }

    /// Open an existing dedicated segment.
    pub fn open_dedicated(&mut self, name: &str, size: usize) -> Result<u32, String> {
        let seg = DedicatedSegment::open(name, size)?;
        let idx = self.next_dedicated_idx;
        self.next_dedicated_idx = self.next_dedicated_idx.checked_add(1)
            .expect("dedicated segment index overflow");
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
            },
        );
        Ok(idx)
    }

    /// Free a block given its offset and the original requested data size.
    ///
    /// This recomputes the buddy level from the data size, enabling cross-process
    /// freeing where the remote side only knows (offset, data_size) from the wire.
    pub fn free_at(&mut self, seg_idx: u32, offset: u32, data_size: u32, is_dedicated: bool) -> Result<(), String> {
        if is_dedicated {
            self.free_dedicated(seg_idx);
            Ok(())
        } else if let Some(seg) = self.segments.get(seg_idx as usize) {
            let actual_size = (data_size as usize)
                .next_power_of_two()
                .max(self.config.min_block_size);
            if let Some(level) = seg.allocator().size_to_level(actual_size) {
                seg.allocator().free(offset, level as u16)
            } else {
                Err("could not determine buddy level for free_at".into())
            }
        } else {
            Err(format!("invalid segment index {}", seg_idx))
        }
    }

    /// Get the data region base address and size for a buddy segment.
    ///
    /// Returns (data_base_addr, data_region_size) where data_base_addr is the
    /// raw pointer to offset 0 within the data region.  Useful for creating
    /// a persistent memoryview covering the whole data region.
    pub fn seg_data_info(&self, seg_idx: u32) -> Result<(*mut u8, usize), String> {
        let seg = self
            .segments
            .get(seg_idx as usize)
            .ok_or("invalid segment index")?;
        let alloc = seg.allocator();
        Ok((alloc.data_ptr(0), alloc.data_size()))
    }

    /// Get a raw pointer to data at a specific (seg_idx, offset) without a PoolAllocation.
    ///
    /// Used by the remote side of a connection to read from SHM blocks allocated
    /// by the peer.
    pub fn data_ptr_at(
        &self,
        seg_idx: u32,
        offset: u32,
        is_dedicated: bool,
    ) -> Result<*mut u8, String> {
        if is_dedicated {
            let entry = self
                .dedicated
                .get(&seg_idx)
                .ok_or("invalid dedicated segment index")?;
            Ok(entry.segment.data_ptr())
        } else {
            let seg = self
                .segments
                .get(seg_idx as usize)
                .ok_or("invalid segment index")?;
            Ok(seg.allocator().data_ptr(offset))
        }
    }

    // --- Internal methods ---

    fn max_buddy_block_size(&self) -> usize {
        if self.segments.is_empty() && self.segments.len() < self.config.max_segments {
            // ShmSegment::create auto-inflates so data region >= segment_size.
            // The data region is segment_size.next_power_of_two().
            return self.config.segment_size.next_power_of_two();
        }
        // Return the data size of existing segments.
        self.segments
            .first()
            .map(|s| s.allocator().data_size())
            .unwrap_or(0)
    }

    fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, String> {
        // Layer 1: Try existing segments.
        for (idx, seg) in self.segments.iter().enumerate() {
            if let Some(a) = seg.allocator().alloc(size) {
                return Ok(PoolAllocation {
                    seg_idx: idx as u32,
                    offset: a.offset,
                    actual_size: a.actual_size,
                    level: a.level,
                    is_dedicated: false,
                });
            }
        }

        // Layer 2: Create new segment.
        if self.segments.len() < self.config.max_segments {
            let seg = self.create_segment()?;
            let idx = self.segments.len();
            self.segments.push(seg);
            if let Some(a) = self.segments[idx].allocator().alloc(size) {
                return Ok(PoolAllocation {
                    seg_idx: idx as u32,
                    offset: a.offset,
                    actual_size: a.actual_size,
                    level: a.level,
                    is_dedicated: false,
                });
            }
        }

        // Layer 3: Dedicated segment fallback.
        self.alloc_dedicated(size)
    }

    fn alloc_dedicated(&mut self, size: usize) -> Result<PoolAllocation, String> {
        // Check dedicated segment limit.
        let active_dedicated = self
            .dedicated
            .values()
            .filter(|e| e.freed_at.is_none())
            .count();
        if active_dedicated >= self.config.max_dedicated_segments {
            // Try GC first.
            self.gc_dedicated();
            let active_after = self
                .dedicated
                .values()
                .filter(|e| e.freed_at.is_none())
                .count();
            if active_after >= self.config.max_dedicated_segments {
                return Err(format!(
                    "dedicated segment limit reached ({} active)",
                    active_after
                ));
            }
        }

        let name = self.next_name("d");
        let seg = DedicatedSegment::create(&name, size)?;
        let idx = self.next_dedicated_idx;
        self.next_dedicated_idx = self.next_dedicated_idx.checked_add(1)
            .expect("dedicated segment index overflow");

        // R-I2: Guard against u32 truncation for dedicated segment size.
        if seg.size() > u32::MAX as usize {
            return Err(format!(
                "dedicated segment size {} exceeds 4GB limit", seg.size()
            ));
        }
        let alloc_size = seg.size() as u32;
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
            },
        );

        Ok(PoolAllocation {
            seg_idx: idx,
            offset: 0,
            actual_size: alloc_size,
            level: 0,
            is_dedicated: true,
        })
    }

    fn free_buddy(&mut self, alloc: &PoolAllocation) -> Result<(), String> {
        if let Some(seg) = self.segments.get(alloc.seg_idx as usize) {
            seg.allocator().free(alloc.offset, alloc.level)
        } else {
            Err(format!("invalid segment index {}", alloc.seg_idx))
        }
    }

    fn free_dedicated(&mut self, seg_idx: u32) {
        if let Some(entry) = self.dedicated.get_mut(&seg_idx) {
            entry.freed_at = Some(Instant::now());
        }
    }

    fn create_segment(&self) -> Result<ShmSegment, String> {
        let name = self.next_name("b");
        ShmSegment::create(&name, self.config.segment_size, self.config.min_block_size)
    }

    fn next_name(&self, tag: &str) -> String {
        let counter = self.name_counter.fetch_add(1, Ordering::Relaxed);
        format!("{}_{}{:04x}", self.name_prefix, tag, counter)
    }
}

impl Drop for BuddyPool {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_pool(config: PoolConfig) -> BuddyPool {
        let id = TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        let prefix = format!("/cc3t{:04x}{:04x}", std::process::id() as u16, id);
        BuddyPool::new_with_prefix(config, prefix)
    }

    fn small_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
        }
    }

    #[test]
    fn test_basic_pool_alloc_free() {
        let mut pool = test_pool(small_config());
        let a = pool.alloc(4096).unwrap();
        assert!(!a.is_dedicated);
        assert_eq!(pool.segment_count(), 1);

        pool.free(&a).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.alloc_count, 0);
    }

    #[test]
    fn test_multiple_allocs() {
        let mut pool = test_pool(small_config());
        let a = pool.alloc(4096).unwrap();
        let b = pool.alloc(4096).unwrap();
        assert_ne!(a.offset, b.offset);
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
    }

    #[test]
    fn test_segment_expansion() {
        // With auto-inflation, segment_size=64KB gives 64KB usable data.
        // Use a smaller segment so two 32KB allocs trigger expansion.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        assert_eq!(pool.segment_count(), 1);

        let b = pool.alloc(32 * 1024).unwrap();
        assert!(pool.segment_count() >= 2 || b.is_dedicated);

        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
    }

    #[test]
    fn test_dedicated_fallback() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
        };
        let mut pool = test_pool(config);

        let a = pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);

        pool.free(&a).unwrap();
        pool.gc_dedicated();
    }

    #[test]
    fn test_pool_stats() {
        let mut pool = test_pool(small_config());
        let a = pool.alloc(4096).unwrap();
        let stats = pool.stats();
        assert!(stats.total_bytes > 0);
        assert!(stats.alloc_count >= 1);
        pool.free(&a).unwrap();
    }

    #[test]
    fn test_data_ptr_read_write() {
        let mut pool = test_pool(small_config());
        let a = pool.alloc(4096).unwrap();
        let ptr = pool.data_ptr(&a).unwrap();
        unsafe {
            std::ptr::write_bytes(ptr, 0xAB, 100);
            assert_eq!(*ptr, 0xAB);
            assert_eq!(*ptr.add(99), 0xAB);
        }
        pool.free(&a).unwrap();
    }

    #[test]
    fn test_zero_alloc_fails() {
        let mut pool = test_pool(small_config());
        assert!(pool.alloc(0).is_err());
    }

    #[test]
    #[should_panic(expected = "invalid PoolConfig")]
    fn test_validate_zero_min_block() {
        let config = PoolConfig {
            min_block_size: 0,
            ..small_config()
        };
        test_pool(config);
    }

    #[test]
    #[should_panic(expected = "invalid PoolConfig")]
    fn test_validate_non_power_of_two_min_block() {
        let config = PoolConfig {
            min_block_size: 3000,
            ..small_config()
        };
        test_pool(config);
    }

    #[test]
    #[should_panic(expected = "invalid PoolConfig")]
    fn test_validate_segment_too_small() {
        let config = PoolConfig {
            segment_size: 4096,
            min_block_size: 4096,
            ..small_config()
        };
        test_pool(config);
    }

    #[test]
    #[should_panic(expected = "invalid PoolConfig")]
    fn test_validate_nan_gc_delay() {
        let config = PoolConfig {
            dedicated_gc_delay_secs: f64::NAN,
            ..small_config()
        };
        test_pool(config);
    }

    #[test]
    fn test_validate_negative_gc_delay_ok() {
        // Negative gc_delay is allowed (clamped to 0 at runtime)
        let config = PoolConfig {
            dedicated_gc_delay_secs: -1.0,
            ..small_config()
        };
        let _pool = test_pool(config);
    }
}
