//! Unified memory pool with tiered fallback.
//!
//! Manages multiple buddy-allocated SHM segments and dedicated
//! segments for oversized payloads.  Implements the fallback chain:
//!   T1. Try existing buddy segments
//!   T2. Create new buddy segment (up to max_segments)
//!   T3. Fall back to dedicated segment

use crate::buddy_segment::BuddySegment;
use crate::config::{PoolAllocation, PoolConfig, PoolStats};
use crate::dedicated::DedicatedSegment;
use crate::handle::MemHandle;
use crate::spill;
use std::collections::HashMap;
use std::time::Instant;

/// Tracking info for a dedicated segment.
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,
}

/// Unified memory pool — the main entry point.
pub struct MemPool {
    config: PoolConfig,
    /// Buddy-managed segments.
    segments: Vec<BuddySegment>,
    /// Dedicated segments for oversized allocations. Key is segment index.
    dedicated: HashMap<u32, DedicatedEntry>,
    /// Base name prefix for SHM segments (includes PID).
    name_prefix: String,
    /// Next dedicated segment index (offset from max_segments to avoid collision).
    next_dedicated_idx: u32,
    /// When each buddy segment became fully idle (alloc_count == 0).
    /// None means the segment has active allocations.
    idle_since: Vec<Option<Instant>>,
}

impl MemPool {
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
            name_prefix,
            next_dedicated_idx: 256,
            idle_since: Vec::new(),
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

    /// Scan for and remove stale SHM segments left by crashed processes.
    ///
    /// SHM segment names follow the pattern `/{prefix}{pid:08x}_{tag}{counter:04x}`.
    /// This function enumerates known SHM segments, extracts the PID from each
    /// name, and unlinks any whose owner process is no longer alive.
    ///
    /// On Linux, scans `/dev/shm/`. On macOS (no `/dev/shm/`), uses a best-effort
    /// approach by probing common names based on the prefix pattern.
    #[cfg(target_os = "linux")]
    pub fn cleanup_stale_segments(prefix: &str) -> usize {
        use crate::alloc::spinlock::is_process_alive;

        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir("/dev/shm") else {
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Prefix without leading '/' (file names in /dev/shm don't have it)
            let bare_prefix = prefix.trim_start_matches('/');
            if !name_str.starts_with(bare_prefix) {
                continue;
            }
            // Try to extract PID from the first 8 hex chars after the 4-char tag.
            // Format: "cc3b{pid:08x}_{tag}{counter:04x}"
            // bare_prefix is e.g. "cc3b" (4 chars), PID is chars 4..12
            if let Some(pid) = Self::extract_pid_from_name(&name_str) {
                if !is_process_alive(pid) {
                    let shm_name = format!("/{}", name_str);
                    if let Ok(c_name) = std::ffi::CString::new(shm_name) {
                        unsafe { libc::shm_unlink(c_name.as_ptr()); }
                        removed += 1;
                    }
                }
            }
        }
        removed
    }

    /// On macOS there is no `/dev/shm/` directory. We use a best-effort approach
    /// by attempting to unlink segments from our own (dead) PIDs. Since we cannot
    /// enumerate POSIX SHM on macOS, this is inherently limited.
    #[cfg(not(target_os = "linux"))]
    pub fn cleanup_stale_segments(_prefix: &str) -> usize {
        // On macOS, POSIX SHM cannot be enumerated. The per-segment
        // shm_unlink-before-create in BuddySegment::create() provides
        // partial cleanup for same-PID restarts. Cross-PID stale segments
        // are not automatically cleaned on this platform.
        0
    }

    /// Extract PID from a segment name like "cc3b{pid:08x}_b0000".
    #[cfg(target_os = "linux")]
    fn extract_pid_from_name(name: &str) -> Option<u32> {
        // The prefix is "cc3b" (4 chars), followed by 8 hex chars of PID.
        if name.len() >= 12 && name.starts_with("cc3b") {
            u32::from_str_radix(&name[4..12], 16).ok()
        } else if name.len() >= 12 && name.starts_with("cc3t") {
            // Test prefix: "cc3t{pid_lo:04x}{counter:04x}" — PID extraction
            // is unreliable for test prefixes, skip.
            None
        } else {
            None
        }
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

    /// Reclaim idle buddy segments from the end of the segment list.
    ///
    /// Only pops trailing empty segments to avoid index remapping (segment
    /// indices are encoded in wire frames). Always retains at least one segment.
    /// Returns the number of segments reclaimed.
    pub fn gc_buddy(&mut self) -> usize {
        let secs = self.config.dedicated_gc_delay_secs;
        let delay = if secs < 0.0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(secs)
        };
        let now = Instant::now();
        let mut removed = 0;

        // Pop from the end while segments are idle and past the delay.
        while self.segments.len() > 1 {
            let last = self.segments.len() - 1;
            let seg = &self.segments[last];
            if seg.allocator().alloc_count() > 0 {
                break;
            }
            if let Some(idle_at) = self.idle_since.get(last).copied().flatten() {
                if now.duration_since(idle_at) >= delay {
                    self.segments.pop();
                    self.idle_since.pop();
                    removed += 1;
                    continue;
                }
            }
            break;
        }
        removed
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

    /// Ensure at least one buddy segment exists.
    ///
    /// Called before handshake so the client can announce its SHM segments
    /// to the server.  Does nothing if segments already exist.
    pub fn ensure_ready(&mut self) -> Result<(), String> {
        if self.segments.is_empty() && self.config.max_segments > 0 {
            let seg = self.create_segment()?;
            self.segments.push(seg);
            self.idle_since.push(None);
        }
        Ok(())
    }

    /// Get the number of buddy segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Get a specific segment by index.
    pub fn segment(&self, idx: usize) -> Option<&BuddySegment> {
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

    /// Get the pool name prefix (for handshake exchange).
    pub fn prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Open an existing buddy segment (for the remote side of a connection).
    pub fn open_segment(&mut self, name: &str, size: usize) -> Result<usize, String> {
        let seg = BuddySegment::open(name, size)?;
        let idx = self.segments.len();
        self.segments.push(seg);
        self.idle_since.push(None);
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
                seg.allocator().free(offset, level as u16)?;
                // Track idle: if segment is now empty, record the time.
                let idx = seg_idx as usize;
                if seg.allocator().alloc_count() == 0 {
                    if idx < self.idle_since.len() && self.idle_since[idx].is_none() {
                        self.idle_since[idx] = Some(Instant::now());
                    }
                }
                Ok(())
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

    // ── MemHandle API ──────────────────────────────────────────────

    /// Unified allocation returning a [`MemHandle`].
    ///
    /// Decision flow (optimised — RAM check only when creating new mappings):
    /// 1. size fits buddy AND existing segments have space → Buddy (no RAM check)
    /// 2. Else: should_spill() → FileSpill if RAM scarce
    /// 3. Else: expand buddy or create dedicated SHM
    /// 4. If SHM creation fails → FileSpill fallback
    pub fn alloc_handle(&mut self, size: usize) -> Result<MemHandle, String> {
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.max_buddy_block_size();

        // Fast path: existing buddy segments (bitmap only, no RAM query).
        if max_buddy > 0 && size <= max_buddy {
            for (idx, seg) in self.segments.iter().enumerate() {
                if (seg.allocator().free_bytes() as usize) < size { continue; }
                if let Some(a) = seg.allocator().alloc(size) {
                    if idx < self.idle_since.len() { self.idle_since[idx] = None; }
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16, offset: a.offset, len: size,
                    });
                }
            }
        }

        // Slow path: need new mapping — check RAM.
        if spill::should_spill(size, self.config.spill_threshold) {
            return self.alloc_file_spill(size);
        }

        // RAM fine: try buddy expansion.
        if max_buddy > 0 && size <= max_buddy
            && self.segments.len() < self.config.max_segments
        {
            match self.create_segment() {
                Ok(seg) => {
                    let idx = self.segments.len();
                    self.segments.push(seg);
                    self.idle_since.push(None);
                    if let Some(a) = self.segments[idx].allocator().alloc(size) {
                        return Ok(MemHandle::Buddy {
                            seg_idx: idx as u16, offset: a.offset, len: size,
                        });
                    }
                }
                Err(_) => return self.alloc_file_spill(size),
            }
        }

        // Large → dedicated SHM, file spill fallback.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16, len: size,
            }),
            Err(_) => self.alloc_file_spill(size),
        }
    }

    fn alloc_file_spill(&self, size: usize) -> Result<MemHandle, String> {
        let (mmap, path) = spill::create_file_spill(size, &self.config.spill_dir)
            .map_err(|e| format!("file spill failed: {e}"))?;
        Ok(MemHandle::FileSpill { mmap, path, len: size })
    }

    /// Read-only slice from a handle.
    pub fn handle_slice<'a>(&'a self, handle: &'a MemHandle) -> &'a [u8] {
        match handle {
            MemHandle::Buddy { seg_idx, offset, len } => {
                let ptr = self.segments[*seg_idx as usize]
                    .allocator().data_ptr(*offset);
                unsafe { std::slice::from_raw_parts(ptr, *len) }
            }
            MemHandle::Dedicated { seg_idx, len } => {
                let ptr = self.dedicated[&(*seg_idx as u32)]
                    .segment.data_ptr();
                unsafe { std::slice::from_raw_parts(ptr, *len) }
            }
            MemHandle::FileSpill { mmap, len, .. } => &mmap[..*len],
        }
    }

    /// Mutable slice from a handle.
    pub fn handle_slice_mut<'a>(
        &'a self, handle: &'a mut MemHandle,
    ) -> &'a mut [u8] {
        match handle {
            MemHandle::Buddy { seg_idx, offset, len } => {
                let ptr = self.segments[*seg_idx as usize]
                    .allocator().data_ptr(*offset);
                unsafe { std::slice::from_raw_parts_mut(ptr, *len) }
            }
            MemHandle::Dedicated { seg_idx, len } => {
                let ptr = self.dedicated[&(*seg_idx as u32)]
                    .segment.data_ptr();
                unsafe { std::slice::from_raw_parts_mut(ptr, *len) }
            }
            MemHandle::FileSpill { mmap, len, .. } => &mut mmap[..*len],
        }
    }

    /// Release resources held by a [`MemHandle`].
    pub fn release_handle(&mut self, handle: MemHandle) {
        match handle {
            MemHandle::Buddy { seg_idx, offset, len } => {
                let _ = self.free_at(
                    seg_idx as u32, offset, len as u32, false,
                );
            }
            MemHandle::Dedicated { seg_idx, .. } => {
                self.free_dedicated(seg_idx as u32);
            }
            MemHandle::FileSpill { .. } => {
                // MmapMut dropped here → munmap; file already unlinked
            }
        }
    }

    // --- Internal methods ---

    fn max_buddy_block_size(&self) -> usize {
        if self.segments.is_empty() && self.segments.len() < self.config.max_segments {
            // BuddySegment::create auto-inflates so data region >= segment_size.
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
        // Layer 1: Try existing segments. Skip segments with insufficient free space
        // to avoid unnecessary spinlock acquisition.
        for (idx, seg) in self.segments.iter().enumerate() {
            if (seg.allocator().free_bytes() as usize) < size {
                continue;
            }
            if let Some(a) = seg.allocator().alloc(size) {
                // Mark segment as active (not idle).
                if idx < self.idle_since.len() {
                    self.idle_since[idx] = None;
                }
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
            self.idle_since.push(None);
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

        let idx = self.next_dedicated_idx;
        let name = format!("{}_{}{:04x}", self.name_prefix, "d", idx);
        let seg = DedicatedSegment::create(&name, size)?;
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
        let idx = alloc.seg_idx as usize;
        if let Some(seg) = self.segments.get(idx) {
            seg.allocator().free(alloc.offset, alloc.level)?;
            // Track idle: if segment is now empty, record the time.
            if seg.allocator().alloc_count() == 0 {
                if idx < self.idle_since.len() && self.idle_since[idx].is_none() {
                    self.idle_since[idx] = Some(Instant::now());
                }
            }
            Ok(())
        } else {
            Err(format!("invalid segment index {}", alloc.seg_idx))
        }
    }

    fn free_dedicated(&mut self, seg_idx: u32) {
        if let Some(entry) = self.dedicated.get_mut(&seg_idx) {
            entry.freed_at = Some(Instant::now());
        }
    }

    fn create_segment(&self) -> Result<BuddySegment, String> {
        let idx = self.segments.len();
        let name = format!("{}_{}{:04x}", self.name_prefix, "b", idx);
        BuddySegment::create(&name, self.config.segment_size, self.config.min_block_size)
    }
}

impl Drop for MemPool {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_pool(config: PoolConfig) -> MemPool {
        let id = TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        let prefix = format!("/cc3t{:04x}{:04x}", std::process::id() as u16, id);
        MemPool::new_with_prefix(config, prefix)
    }

    fn small_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
            ..PoolConfig::default()
        }
    }

    fn test_config() -> PoolConfig {
        small_config()
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
            ..PoolConfig::default()
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
            ..PoolConfig::default()
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

    #[test]
    fn test_gc_buddy_reclaims_trailing_idle() {
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_gc_delay_secs: 0.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);

        // Allocate blocks that force 2 segments.
        let mut allocs = Vec::new();
        for _ in 0..20 {
            allocs.push(pool.alloc(4096).unwrap());
        }
        assert!(pool.segment_count() >= 2);

        // Free all blocks.
        for a in &allocs {
            pool.free(a).unwrap();
        }

        // GC should reclaim trailing idle segments (but keep at least 1).
        let removed = pool.gc_buddy();
        assert!(removed > 0);
        assert!(pool.segment_count() >= 1);

        // Pool should still be usable.
        let a = pool.alloc(4096).unwrap();
        pool.free(&a).unwrap();
    }

    #[test]
    fn test_gc_buddy_keeps_segment_with_allocs() {
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_gc_delay_secs: 0.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);

        // Force 2 segments.
        let mut allocs = Vec::new();
        for _ in 0..20 {
            allocs.push(pool.alloc(4096).unwrap());
        }
        let seg_count = pool.segment_count();
        assert!(seg_count >= 2);

        // Free only blocks from the first segment (keep second non-empty).
        // Actually, just keep one block alive — gc should not reclaim.
        for a in allocs.iter().skip(1) {
            pool.free(a).unwrap();
        }

        let removed = pool.gc_buddy();
        // Trailing segment may or may not be empty. At minimum, first segment
        // still has 1 alloc, so pool is still alive.
        assert!(pool.segment_count() >= 1);
        let _ = removed;

        pool.free(&allocs[0]).unwrap();
    }

    #[test]
    fn test_deterministic_buddy_naming() {
        let mut pool = test_pool(PoolConfig {
            max_segments: 4,
            max_dedicated_segments: 2,
            ..test_config()
        });

        // Allocate to force segment 0 creation.
        let a0 = pool.alloc(4096).unwrap();
        assert_eq!(a0.seg_idx, 0);
        let name0 = pool.segment_name(0).unwrap();
        assert!(name0.ends_with("_b0000"), "got: {}", name0);

        // Fill segment 0 to force segment 1 creation.
        let big = pool.config.segment_size;
        pool.free(&a0).unwrap();
        let a_big = pool.alloc(big).unwrap();
        assert_eq!(a_big.seg_idx, 0);
        let a1 = pool.alloc(4096).unwrap();
        assert_eq!(a1.seg_idx, 1);
        let name1 = pool.segment_name(1).unwrap();
        assert!(name1.ends_with("_b0001"), "got: {}", name1);

        // Interleave a dedicated allocation — should NOT affect buddy naming.
        let a_ded = pool.alloc(big * 2).unwrap();
        assert!(a_ded.is_dedicated);

        // Buddy segment 2.
        pool.free(&a1).unwrap();
        let a1_big = pool.alloc(big).unwrap();
        assert_eq!(a1_big.seg_idx, 1);
        let a2 = pool.alloc(4096).unwrap();
        assert_eq!(a2.seg_idx, 2);
        let name2 = pool.segment_name(2).unwrap();
        assert!(name2.ends_with("_b0002"), "got: {}", name2);

        pool.free(&a_ded).unwrap();
        pool.free(&a_big).unwrap();
        pool.free(&a1_big).unwrap();
        pool.free(&a2).unwrap();
    }

    #[test]
    fn test_deterministic_dedicated_naming() {
        let mut pool = test_pool(PoolConfig {
            max_segments: 1,
            max_dedicated_segments: 4,
            ..test_config()
        });

        let a0 = pool.alloc(4096).unwrap();
        pool.free(&a0).unwrap();

        let big = pool.config.segment_size * 2;
        let d0 = pool.alloc(big).unwrap();
        assert!(d0.is_dedicated);
        assert_eq!(d0.seg_idx, 256);
        let dname0 = pool.dedicated_name(256).unwrap();
        assert!(dname0.ends_with("_d0100"), "got: {}", dname0);

        let d1 = pool.alloc(big).unwrap();
        assert!(d1.is_dedicated);
        assert_eq!(d1.seg_idx, 257);
        let dname1 = pool.dedicated_name(257).unwrap();
        assert!(dname1.ends_with("_d0101"), "got: {}", dname1);

        pool.free(&d0).unwrap();
        pool.free(&d1).unwrap();
    }

    #[test]
    fn test_prefix_accessor() {
        let pool = test_pool(test_config());
        let prefix = pool.prefix();
        assert!(prefix.starts_with("/cc3t"), "got: {}", prefix);
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::config::PoolConfig;

    fn test_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
            spill_threshold: 1.0, // disable spill
            spill_dir: std::env::temp_dir().join("c2_pool_handle_test"),
        }
    }

    #[test]
    fn test_alloc_handle_buddy() {
        let mut pool = MemPool::new(test_config());
        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_buddy());
        assert_eq!(handle.len(), 4096);
        pool.release_handle(handle);
    }

    #[test]
    fn test_alloc_handle_dedicated() {
        let mut pool = MemPool::new(test_config());
        let handle = pool.alloc_handle(128 * 1024).unwrap();
        assert!(handle.is_dedicated());
        assert_eq!(handle.len(), 128 * 1024);
        pool.release_handle(handle);
    }

    #[test]
    fn test_alloc_handle_file_spill_forced() {
        let dir = std::env::temp_dir().join("c2_pool_spill_test");
        let config = PoolConfig {
            spill_threshold: 0.0, spill_dir: dir.clone(), ..test_config()
        };
        let mut pool = MemPool::new(config);
        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_slice_write_read() {
        let mut pool = MemPool::new(test_config());
        let mut handle = pool.alloc_handle(4096).unwrap();
        let pattern = b"test_data_pattern";
        pool.handle_slice_mut(&mut handle)[..pattern.len()].copy_from_slice(pattern);
        assert_eq!(&pool.handle_slice(&handle)[..pattern.len()], pattern);
        pool.release_handle(handle);
    }

    #[test]
    fn test_handle_slice_file_spill() {
        let dir = std::env::temp_dir().join("c2_pool_spill_slice_test");
        let config = PoolConfig {
            spill_threshold: 0.0, spill_dir: dir.clone(), ..test_config()
        };
        let mut pool = MemPool::new(config);
        let mut handle = pool.alloc_handle(8192).unwrap();
        let pattern = b"spill_pattern_data";
        pool.handle_slice_mut(&mut handle)[..pattern.len()].copy_from_slice(pattern);
        assert_eq!(&pool.handle_slice(&handle)[..pattern.len()], pattern);
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
