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
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Result of a free operation — signals whether the segment became fully idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeResult {
    /// Block freed normally, segment still has active allocations.
    Normal,
    /// Block freed and the segment is now completely idle (zero active allocations).
    SegmentIdle { seg_idx: u16 },
    /// Dedicated segment freed — caller may schedule delayed GC.
    DedicatedFreed { seg_idx: u16 },
}

/// Tracking info for a dedicated segment.
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,
    /// `true` if this process created the segment (owns shm_unlink).
    /// `false` if opened from a peer (reader side, munmap-only on drop).
    is_creator: bool,
}

/// Unified memory pool — the main entry point.
pub struct MemPool {
    config: PoolConfig,
    /// Buddy-managed segments.
    segments: Vec<Option<BuddySegment>>,
    /// Never truncated when an owner slot is reclaimed or a peer view closes.
    generations: Vec<u32>,
    is_peer: bool,
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

    /// Prefixes travel in the handshake; OS object names are bounded digests.
    const MAX_SHM_PREFIX_LEN: usize = 255;

    /// Create a new pool with a custom name prefix (for testing / multi-pool).
    pub fn new_with_prefix(config: PoolConfig, name_prefix: String) -> Self {
        let name_prefix = format!(
            "{name_prefix}_{:08x}{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Self::with_identity(config, name_prefix, false)
    }

    /// Receive-side cache for one producer's advertised pool incarnation.
    pub fn open_peer(config: PoolConfig, name_prefix: String) -> Self {
        Self::with_identity(config, name_prefix, true)
    }

    fn with_identity(config: PoolConfig, name_prefix: String, is_peer: bool) -> Self {
        assert!(
            name_prefix.len() <= Self::MAX_SHM_PREFIX_LEN,
            "SHM prefix '{}' is {} bytes, exceeds handshake maximum {}",
            name_prefix,
            name_prefix.len(),
            Self::MAX_SHM_PREFIX_LEN,
        );
        Self::validate_config(&config).expect("invalid PoolConfig");
        Self {
            config,
            segments: Vec::new(),
            generations: Vec::new(),
            is_peer,
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
        if config.max_segments > u16::MAX as usize + 1 {
            return Err("max_segments exceeds the wire segment-index range".into());
        }
        validate_duration_secs(
            "dedicated_crash_timeout_secs",
            config.dedicated_crash_timeout_secs,
        )?;
        validate_duration_secs("buddy_idle_decay_secs", config.buddy_idle_decay_secs)?;
        Ok(())
    }

    /// Scan for and remove stale SHM segments left by crashed processes.
    ///
    /// Only the fixed C-Two namespace, exact encoded name length and valid
    /// creator PID are accepted. Live or inaccessible owners are retained.
    ///
    /// On Linux, scans `/dev/shm/`. On macOS (no `/dev/shm/`), uses a best-effort
    /// approach by probing common names based on the prefix pattern.
    #[cfg(target_os = "linux")]
    pub fn cleanup_stale_segments() -> usize {
        use crate::alloc::spinlock::is_process_alive;

        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir("/dev/shm") else {
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(pid) = Self::extract_pid_from_name(&name_str) {
                if !is_process_alive(pid) {
                    let shm_name = format!("/{}", name_str);
                    if let Ok(c_name) = std::ffi::CString::new(shm_name) {
                        if unsafe { libc::shm_unlink(c_name.as_ptr()) } == 0 {
                            removed += 1;
                        }
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
    pub fn cleanup_stale_segments() -> usize {
        // macOS has no enumerable SHM directory. Windows kernel objects
        // disappear after the last mapping/handle closes, including crashes.
        0
    }

    /// Parse only current C-Two names; never remove another namespace.
    #[cfg(target_os = "linux")]
    fn extract_pid_from_name(name: &str) -> Option<u32> {
        if name.len() != 28 || !name.starts_with("c2") || !name.is_ascii() {
            return None;
        }
        let codec = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let pid_bytes: [u8; 4] = codec.decode(&name[2..8]).ok()?.try_into().ok()?;
        let digest = codec.decode(&name[8..]).ok()?;
        let pid = u32::from_le_bytes(pid_bytes);
        (digest.len() == 15 && pid != 0).then_some(pid)
    }

    /// Allocate memory from the pool.
    pub fn alloc(&mut self, size: usize) -> Result<PoolAllocation, String> {
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        self.gc_dedicated();
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
        self.validate_generation(alloc.seg_idx, alloc.generation, alloc.is_dedicated)?;
        self.gc_dedicated();
        if alloc.is_dedicated {
            self.free_dedicated(alloc.seg_idx);
            Ok(())
        } else {
            self.free_buddy(alloc)
        }
    }

    /// Get a raw pointer to data for a given allocation.
    pub fn data_ptr(&self, alloc: &PoolAllocation) -> Result<*mut u8, String> {
        self.validate_generation(alloc.seg_idx, alloc.generation, alloc.is_dedicated)?;
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
                .and_then(Option::as_ref)
                .ok_or("invalid segment index")?;
            Ok(seg.allocator().data_ptr(alloc.offset))
        }
    }

    /// Get pool statistics.
    pub fn stats(&self) -> PoolStats {
        let mut total_bytes = 0u64;
        let mut free_bytes = 0u64;
        let mut alloc_count = 0u32;

        for seg in self.segments.iter().flatten() {
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
            total_segments: self.segments.iter().flatten().count(),
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
        if self.is_peer {
            let mut removed = 0;
            for slot in &mut self.segments {
                if slot
                    .as_ref()
                    .is_some_and(|segment| segment.allocator().can_retire())
                {
                    slot.take();
                    removed += 1;
                }
            }
            return removed;
        }
        let secs = self.config.buddy_idle_decay_secs;
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
            let seg = self.segments[last].as_ref().expect("owner segment slot");
            if !seg.allocator().can_retire() {
                self.idle_since[last] = None;
                break;
            }
            // A remote participant can perform the last free. The owner must
            // observe that transition rather than require a local free call.
            let idle_at = *self.idle_since[last].get_or_insert(now);
            if now.duration_since(idle_at) >= delay {
                self.segments.pop();
                self.idle_since.pop();
                removed += 1;
                continue;
            }
            break;
        }
        removed
    }

    /// Run garbage collection on freed dedicated segments.
    ///
    /// For creator entries: reclaim when the reader has set `read_done = 1`
    /// in the SHM header, or after a crash-recovery timeout.
    /// For reader entries: reclaim immediately (munmap only, no shm_unlink).
    pub fn gc_dedicated(&mut self) {
        let crash_timeout = {
            let secs = self.config.dedicated_crash_timeout_secs;
            if secs < 0.0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs_f64(secs)
            }
        };
        let now = Instant::now();
        let to_remove: Vec<u32> = self
            .dedicated
            .iter()
            .filter_map(|(&idx, entry)| {
                if let Some(freed_at) = entry.freed_at {
                    if entry.is_creator {
                        // Primary: peer set read_done in SHM header
                        if entry.segment.is_read_done() {
                            return Some(idx);
                        }
                        // Fallback: peer likely crashed — safety net
                        if now.duration_since(freed_at) >= crash_timeout {
                            return Some(idx);
                        }
                    } else {
                        // Reader side: can drop immediately (munmap only)
                        return Some(idx);
                    }
                }
                None
            })
            .collect();

        for idx in to_remove {
            self.dedicated.remove(&idx);
        }
    }

    /// Ensure at least one buddy segment exists.
    ///
    /// Called before handshake so the client can announce its SHM segments
    /// to the server.  Does nothing if segments already exist.
    pub fn ensure_ready(&mut self) -> Result<(), String> {
        if self.config.max_segments > 0 {
            self.ensure_buddy_segments(1)?;
        }
        Ok(())
    }

    /// Ensure a stable set of buddy segments exists before peer advertisement.
    ///
    /// Runtime adapters that advertise their client buddy pool in an IPC
    /// handshake should call this before exposing segment metadata. This avoids
    /// later lazy allocation from returning a segment index the peer never saw.
    pub fn ensure_buddy_segments(&mut self, count: usize) -> Result<(), String> {
        if self.is_peer {
            return Err("cannot create backings in a peer pool".into());
        }
        if count > self.config.max_segments {
            return Err(format!(
                "requested {} buddy segments exceeds configured max_segments {}",
                count, self.config.max_segments
            ));
        }
        while self.segments.len() < count {
            let seg = self.create_segment()?;
            self.segments.push(Some(seg));
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
        self.segments.get(idx).and_then(Option::as_ref)
    }

    /// Destroy the pool, cleaning up all SHM segments.
    pub fn destroy(&mut self) {
        self.dedicated.clear();
        self.segments.clear(); // Drop triggers munmap + shm_unlink for owned segments.
        self.idle_since.clear();
    }

    /// Get the SHM name for a buddy segment.
    pub fn segment_name(&self, idx: usize) -> Option<&str> {
        self.segment(idx).map(|s| s.name())
    }

    /// Get the SHM name for a dedicated segment.
    pub fn dedicated_name(&self, idx: u32) -> Option<&str> {
        self.dedicated.get(&idx).map(|e| e.segment.name())
    }

    /// Get the pool name prefix (for handshake exchange).
    pub fn prefix(&self) -> &str {
        &self.name_prefix
    }

    pub fn segment_generation(&self, idx: usize) -> Option<u32> {
        self.segment(idx)?;
        self.generations.get(idx).copied()
    }

    pub fn buddy_segment_name(prefix: &str, idx: u32, generation: u32) -> String {
        Self::backing_name(prefix, b'b', idx, generation)
    }

    pub fn dedicated_segment_name(prefix: &str, idx: u32) -> String {
        Self::backing_name(prefix, b'd', idx, 0)
    }

    fn backing_name(prefix: &str, kind: u8, idx: u32, generation: u32) -> String {
        let mut digest = Sha256::new();
        digest.update(b"c-two.shm.v2\0");
        digest.update([kind]);
        digest.update((prefix.len() as u64).to_le_bytes());
        digest.update(prefix.as_bytes());
        digest.update(idx.to_le_bytes());
        digest.update(generation.to_le_bytes());
        let hash = digest.finalize();
        let identity = prefix.rsplit('_').next().unwrap_or("");
        let pid = if identity.len() == 40 && identity.is_ascii() {
            u32::from_str_radix(&identity[..8], 16).unwrap_or(0)
        } else {
            0
        };
        let codec = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        // 29 ASCII bytes including a 120-bit digest and explicit owner PID.
        // The PID permits scoped Linux crash cleanup without mapping bytes.
        format!(
            "/c2{}{}",
            codec.encode(pid.to_le_bytes()),
            codec.encode(&hash[..15])
        )
    }

    /// Open exactly the backing carried by the frame, never a cached older one.
    pub fn ensure_peer_segment(
        &mut self,
        seg_idx: u32,
        generation: u32,
        min_size: usize,
    ) -> Result<(), String> {
        if !self.is_peer {
            return Err("cannot open peer backings in an owner pool".into());
        }
        let idx = seg_idx as usize;
        if generation == 0 || seg_idx > u16::MAX as u32 || idx >= self.config.max_segments {
            return Err("invalid peer backing identity".into());
        }
        let known = self.generations.get(idx).copied().unwrap_or(0);
        if generation < known {
            return Err("stale shared-memory backing generation".into());
        }
        if let Some(segment) = self.segment(idx) {
            if generation == known {
                if segment.allocator().data_size() < min_size {
                    return Err("peer backing data capacity is too small".into());
                }
                return Ok(());
            }
            if !segment.allocator().can_retire() {
                return Err(
                    "cannot retire a backing with live allocations or unavailable allocator state"
                        .into(),
                );
            }
        }
        let name = Self::buddy_segment_name(&self.name_prefix, seg_idx, generation);
        let segment = BuddySegment::open(&name, min_size)?;
        if segment.allocator().data_size() < min_size {
            return Err("peer backing data capacity is too small".into());
        }
        if self.segments.len() <= idx {
            self.segments.resize_with(idx + 1, || None);
            self.idle_since.resize(idx + 1, None);
        }
        if self.generations.len() <= idx {
            self.generations.resize(idx + 1, 0);
        }
        self.segments[idx] = Some(segment);
        self.generations[idx] = generation;
        Ok(())
    }

    /// Open an oversized peer allocation using the same native naming owner.
    pub fn ensure_peer_dedicated(&mut self, seg_idx: u32, min_size: usize) -> Result<(), String> {
        if !self.is_peer {
            return Err("cannot open peer backings in an owner pool".into());
        }
        self.gc_dedicated();
        let name = Self::dedicated_segment_name(&self.name_prefix, seg_idx);
        self.open_dedicated_at(seg_idx, &name, min_size)
    }

    fn validate_generation(
        &self,
        seg_idx: u32,
        generation: u32,
        dedicated: bool,
    ) -> Result<(), String> {
        if seg_idx > u16::MAX as u32 {
            return Err("segment index exceeds wire range".into());
        }
        if dedicated {
            if generation != 0 {
                return Err("dedicated generation must be zero".into());
            }
        } else if generation == 0 || self.segment_generation(seg_idx as usize) != Some(generation) {
            return Err("stale or unavailable shared-memory backing generation".into());
        }
        Ok(())
    }

    /// Open an existing dedicated segment at a specific index.
    ///
    /// Used by the server to lazy-open peer dedicated segments, where the
    /// index must match the producer's `seg_idx` from the wire frame.
    /// No-op if the index already exists in the map.
    pub fn open_dedicated_at(
        &mut self,
        idx: u32,
        name: &str,
        min_size: usize,
    ) -> Result<(), String> {
        if idx > u16::MAX as u32 {
            return Err("dedicated segment index exceeds wire range".into());
        }
        if let Some(entry) = self.dedicated.get(&idx) {
            if entry.freed_at.is_some() || entry.segment.is_read_done() {
                return Err("dedicated backing has already been released".into());
            }
            if entry.segment.data_size() < min_size {
                return Err("dedicated backing data capacity is too small".into());
            }
            return Ok(());
        }
        let seg = DedicatedSegment::open(name, min_size)?;
        if seg.is_read_done() {
            return Err("dedicated backing has already been released".into());
        }
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
                is_creator: false,
            },
        );
        // Keep next_dedicated_idx ahead of all known keys to avoid collision.
        if idx >= self.next_dedicated_idx {
            self.next_dedicated_idx = idx
                .checked_add(1)
                .expect("dedicated segment index overflow");
        }
        Ok(())
    }

    /// Free a block given its offset and the original requested data size.
    ///
    /// This recomputes the buddy level from the data size, enabling cross-process
    /// freeing where the remote side only knows (offset, data_size) from the wire.
    pub fn free_at(
        &mut self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<FreeResult, String> {
        self.validate_generation(seg_idx, generation, is_dedicated)?;
        self.gc_dedicated();
        if is_dedicated {
            let entry = self
                .dedicated
                .get(&seg_idx)
                .ok_or("invalid dedicated segment index")?;
            if entry.freed_at.is_some() || (!entry.is_creator && entry.segment.is_read_done()) {
                return Err("dedicated backing has already been released".into());
            }
            self.free_dedicated(seg_idx);
            Ok(FreeResult::DedicatedFreed {
                seg_idx: seg_idx as u16,
            })
        } else if let Some(seg) = self.segment(seg_idx as usize) {
            let actual_size = (data_size as usize)
                .next_power_of_two()
                .max(self.config.min_block_size);
            if let Some(level) = seg.allocator().size_to_level(actual_size) {
                seg.allocator().free(offset, level as u16)?;
                let idx = seg_idx as usize;
                if seg.allocator().can_retire() {
                    if idx < self.idle_since.len() && self.idle_since[idx].is_none() {
                        self.idle_since[idx] = Some(Instant::now());
                    }
                    if self.is_peer {
                        self.segments[idx].take();
                    }
                    return Ok(FreeResult::SegmentIdle {
                        seg_idx: seg_idx as u16,
                    });
                }
                Ok(FreeResult::Normal)
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
    pub fn seg_data_info(&self, seg_idx: u32, generation: u32) -> Result<(*mut u8, usize), String> {
        self.validate_generation(seg_idx, generation, false)?;
        let seg = self
            .segments
            .get(seg_idx as usize)
            .and_then(Option::as_ref)
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
        generation: u32,
        offset: u32,
        is_dedicated: bool,
    ) -> Result<*mut u8, String> {
        self.validate_generation(seg_idx, generation, is_dedicated)?;
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
                .and_then(Option::as_ref)
                .ok_or("invalid segment index")?;
            Ok(seg.allocator().data_ptr(offset))
        }
    }

    /// Copy bytes from mapped shared-memory coordinates after validating that
    /// the complete span belongs to the selected segment's data region.
    pub fn copy_data_at(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<Vec<u8>, String> {
        if data_size == 0 {
            return Err("shared-memory copy span must not be empty".into());
        }
        let (pointer, len) =
            self.checked_data_pointer(seg_idx, generation, offset, data_size, is_dedicated)?;

        // SAFETY: `checked_data_pointer` proves that the non-empty span is
        // entirely inside the mapped data region and no longer than
        // `isize::MAX`.
        Ok(unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec())
    }

    /// Validate shared-memory coordinates without copying their contents.
    ///
    /// Checked release paths use this before deriving a buddy allocation level.
    pub fn validate_data_at(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(), String> {
        if data_size == 0 {
            return Err("shared-memory copy span must not be empty".into());
        }
        self.checked_data_pointer(seg_idx, generation, offset, data_size, is_dedicated)
            .map(|_| ())
    }

    fn checked_data_pointer(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(*mut u8, usize), String> {
        self.validate_generation(seg_idx, generation, is_dedicated)?;
        let len = usize::try_from(data_size)
            .map_err(|_| "shared-memory copy size is not addressable on this platform")?;
        if len > isize::MAX as usize {
            return Err("shared-memory copy size exceeds the maximum Rust slice length".into());
        }

        let pointer = if is_dedicated {
            if offset != 0 {
                return Err("dedicated shared-memory offset must be zero".into());
            }
            let entry = self
                .dedicated
                .get(&seg_idx)
                .ok_or("invalid dedicated segment index")?;
            if len > entry.segment.data_size() {
                return Err(format!(
                    "shared-memory copy span of {len} bytes is outside dedicated segment {seg_idx}"
                ));
            }
            entry.segment.data_ptr()
        } else {
            let segment = self
                .segments
                .get(seg_idx as usize)
                .and_then(Option::as_ref)
                .ok_or("invalid segment index")?;
            let start = usize::try_from(offset)
                .map_err(|_| "shared-memory copy offset is not addressable on this platform")?;
            let end = start
                .checked_add(len)
                .ok_or("shared-memory copy span overflows the platform address space")?;
            if end > segment.allocator().data_size() {
                return Err(format!(
                    "shared-memory copy span {start}..{end} is outside buddy segment {seg_idx}"
                ));
            }
            segment.allocator().data_ptr(offset)
        };

        Ok((pointer, len))
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
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.max_buddy_block_size();

        // Fast path: existing buddy segments (bitmap only, no RAM query).
        if max_buddy > 0 && size <= max_buddy {
            for (idx, seg) in self.segments.iter().enumerate() {
                let Some(seg) = seg else {
                    continue;
                };
                if (seg.allocator().free_bytes() as usize) < size {
                    continue;
                }
                if let Some(a) = seg.allocator().alloc(size) {
                    if idx < self.idle_since.len() {
                        self.idle_since[idx] = None;
                    }
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16,
                        generation: self.generations[idx],
                        offset: a.offset,
                        allocation_size: a.actual_size,
                        len: size,
                    });
                }
            }
        }

        // Slow path: need new mapping — check RAM.
        if spill::should_spill(size, self.config.spill_threshold) {
            return self.alloc_file_spill(size);
        }

        // RAM fine: try buddy expansion.
        if max_buddy > 0 && size <= max_buddy && self.segments.len() < self.config.max_segments {
            // GC before expanding — may free trailing idle segments
            let reclaimed = self.gc_buddy();
            if reclaimed > 0 {
                // Retry existing segments after GC
                for (idx, seg) in self.segments.iter().enumerate() {
                    let Some(seg) = seg else {
                        continue;
                    };
                    if (seg.allocator().free_bytes() as usize) < size {
                        continue;
                    }
                    if let Some(a) = seg.allocator().alloc(size) {
                        if idx < self.idle_since.len() {
                            self.idle_since[idx] = None;
                        }
                        return Ok(MemHandle::Buddy {
                            seg_idx: idx as u16,
                            generation: self.generations[idx],
                            offset: a.offset,
                            allocation_size: a.actual_size,
                            len: size,
                        });
                    }
                }
            }

            match self.create_segment() {
                Ok(seg) => {
                    let idx = self.segments.len();
                    self.segments.push(Some(seg));
                    self.idle_since.push(None);
                    if let Some(a) = self
                        .segment(idx)
                        .expect("owner segment slot")
                        .allocator()
                        .alloc(size)
                    {
                        return Ok(MemHandle::Buddy {
                            seg_idx: idx as u16,
                            generation: self.generations[idx],
                            offset: a.offset,
                            allocation_size: a.actual_size,
                            len: size,
                        });
                    }
                }
                Err(_) => return self.alloc_file_spill(size),
            }
        }

        // Large → dedicated SHM, file spill fallback.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16,
                len: size,
            }),
            Err(_) => self.alloc_file_spill(size),
        }
    }

    /// Allocate from Buddy or Dedicated SHM only — no FileSpill fallback.
    ///
    /// Used by `promote_to_shm` when upgrading a FileSpill handle.
    /// Returns `Err` if neither buddy nor dedicated has capacity.
    pub fn try_alloc_shm(&mut self, size: usize) -> Result<MemHandle, String> {
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.max_buddy_block_size();

        // Try existing buddy segments.
        if max_buddy > 0 && size <= max_buddy {
            for (idx, seg) in self.segments.iter().enumerate() {
                let Some(seg) = seg else {
                    continue;
                };
                if (seg.allocator().free_bytes() as usize) < size {
                    continue;
                }
                if let Some(a) = seg.allocator().alloc(size) {
                    if idx < self.idle_since.len() {
                        self.idle_since[idx] = None;
                    }
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16,
                        generation: self.generations[idx],
                        offset: a.offset,
                        allocation_size: a.actual_size,
                        len: size,
                    });
                }
            }
        }

        // Try buddy expansion (no RAM check — caller already has data in RAM).
        if max_buddy > 0 && size <= max_buddy && self.segments.len() < self.config.max_segments {
            let reclaimed = self.gc_buddy();
            if reclaimed > 0 {
                for (idx, seg) in self.segments.iter().enumerate() {
                    let Some(seg) = seg else {
                        continue;
                    };
                    if (seg.allocator().free_bytes() as usize) < size {
                        continue;
                    }
                    if let Some(a) = seg.allocator().alloc(size) {
                        if idx < self.idle_since.len() {
                            self.idle_since[idx] = None;
                        }
                        return Ok(MemHandle::Buddy {
                            seg_idx: idx as u16,
                            generation: self.generations[idx],
                            offset: a.offset,
                            allocation_size: a.actual_size,
                            len: size,
                        });
                    }
                }
            }
            if let Ok(seg) = self.create_segment() {
                let idx = self.segments.len();
                self.segments.push(Some(seg));
                self.idle_since.push(None);
                if let Some(a) = self
                    .segment(idx)
                    .expect("owner segment slot")
                    .allocator()
                    .alloc(size)
                {
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16,
                        generation: self.generations[idx],
                        offset: a.offset,
                        allocation_size: a.actual_size,
                        len: size,
                    });
                }
            }
        }

        // Try dedicated SHM — NO FileSpill fallback.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16,
                len: size,
            }),
            Err(_) => Err(format!("try_alloc_shm: no SHM capacity for {size} bytes")),
        }
    }

    fn alloc_file_spill(&self, size: usize) -> Result<MemHandle, String> {
        let (mmap, path) = spill::create_file_spill(size, &self.config.spill_dir)
            .map_err(|e| format!("file spill failed: {e}"))?;
        Ok(MemHandle::FileSpill {
            mmap,
            path,
            len: size,
        })
    }

    /// Read-only slice from a handle.
    pub fn handle_slice<'a>(&'a self, handle: &'a MemHandle) -> &'a [u8] {
        self.validate_handle(handle).expect("invalid memory handle");
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation: _,
                offset,
                len,
                ..
            } => {
                let ptr = self
                    .segment(*seg_idx as usize)
                    .expect("validated backing")
                    .allocator()
                    .data_ptr(*offset);
                unsafe { std::slice::from_raw_parts(ptr, *len) }
            }
            MemHandle::Dedicated { seg_idx, len } => {
                let ptr = self.dedicated[&(*seg_idx as u32)].segment.data_ptr();
                unsafe { std::slice::from_raw_parts(ptr, *len) }
            }
            MemHandle::FileSpill { mmap, len, .. } => &mmap[..*len],
        }
    }

    /// Copy a handle's logical bytes after validating its public coordinates
    /// against the mapped backing region.
    pub fn copy_handle_data(&self, handle: &MemHandle) -> Result<Vec<u8>, String> {
        self.validate_handle(handle)?;
        if handle.is_empty() {
            self.validate_handle(handle)?;
            return Ok(Vec::new());
        }
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                len,
                ..
            } => self.copy_data_at(
                u32::from(*seg_idx),
                *generation,
                *offset,
                u32::try_from(*len)
                    .map_err(|_| "buddy handle length exceeds the wire address space")?,
                false,
            ),
            MemHandle::Dedicated { seg_idx, len } => self.copy_data_at(
                u32::from(*seg_idx),
                0,
                0,
                u32::try_from(*len)
                    .map_err(|_| "dedicated handle length exceeds the wire address space")?,
                true,
            ),
            MemHandle::FileSpill { mmap, len, .. } => {
                if *len == 0 {
                    return Err("file-spill copy span must not be empty".into());
                }
                let bytes = mmap.get(..*len).ok_or_else(|| {
                    format!(
                        "file-spill copy span of {} bytes is outside file-spill mapping of {} bytes",
                        len,
                        mmap.len()
                    )
                })?;
                Ok(bytes.to_vec())
            }
        }
    }

    /// Validate a handle's complete logical span without copying its contents.
    pub fn validate_handle(&self, handle: &MemHandle) -> Result<(), String> {
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                len,
                allocation_size,
                ..
            } => {
                if *len > *allocation_size as usize {
                    return Err("logical length is outside the buddy allocation".into());
                }
                self.checked_data_pointer(
                    u32::from(*seg_idx),
                    *generation,
                    *offset,
                    u32::try_from(*len)
                        .map_err(|_| "buddy handle length exceeds the wire address space")?,
                    false,
                )
                .map(|_| ())
            }
            MemHandle::Dedicated { seg_idx, len } => self
                .checked_data_pointer(
                    u32::from(*seg_idx),
                    0,
                    0,
                    u32::try_from(*len)
                        .map_err(|_| "dedicated handle length exceeds the wire address space")?,
                    true,
                )
                .map(|_| ()),
            MemHandle::FileSpill { mmap, len, .. } => {
                mmap.get(..*len).ok_or_else(|| {
                    format!(
                        "file-spill copy span of {} bytes is outside file-spill mapping of {} bytes",
                        len,
                        mmap.len()
                    )
                })?;
                Ok(())
            }
        }
    }

    /// Mutable slice from a handle.
    pub fn handle_slice_mut<'a>(&'a self, handle: &'a mut MemHandle) -> &'a mut [u8] {
        self.validate_handle(handle).expect("invalid memory handle");
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation: _,
                offset,
                len,
                ..
            } => {
                let ptr = self
                    .segment(*seg_idx as usize)
                    .expect("validated backing")
                    .allocator()
                    .data_ptr(*offset);
                unsafe { std::slice::from_raw_parts_mut(ptr, *len) }
            }
            MemHandle::Dedicated { seg_idx, len } => {
                let ptr = self.dedicated[&(*seg_idx as u32)].segment.data_ptr();
                unsafe { std::slice::from_raw_parts_mut(ptr, *len) }
            }
            MemHandle::FileSpill { mmap, len, .. } => &mut mmap[..*len],
        }
    }

    /// Release resources held by a [`MemHandle`].
    ///
    /// Returns `FreeResult` so callers can trigger deferred GC.
    /// For `FileSpill`, always returns `Normal` (OS handles cleanup via Drop).
    pub fn release_handle(&mut self, handle: MemHandle) -> FreeResult {
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                allocation_size,
                ..
            } => self
                .free_at(seg_idx as u32, generation, offset, allocation_size, false)
                .unwrap_or(FreeResult::Normal),
            MemHandle::Dedicated { seg_idx, .. } => {
                self.free_dedicated(seg_idx as u32);
                FreeResult::DedicatedFreed { seg_idx }
            }
            MemHandle::FileSpill { .. } => {
                // MmapMut dropped here → munmap; file already unlinked
                FreeResult::Normal
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
            .and_then(Option::as_ref)
            .map(|s| s.allocator().data_size())
            .unwrap_or(0)
    }

    fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, String> {
        // Layer 1: Try existing segments. Skip segments with insufficient free space
        // to avoid unnecessary spinlock acquisition.
        for (idx, seg) in self.segments.iter().enumerate() {
            let Some(seg) = seg else {
                continue;
            };
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
                    generation: self.generations[idx],
                    offset: a.offset,
                    actual_size: a.actual_size,
                    level: a.level,
                    is_dedicated: false,
                });
            }
        }

        // Layer 1.5: GC before expansion — reclaim idle trailing segments
        let reclaimed = self.gc_buddy();
        if reclaimed > 0 {
            // Retry existing segments after GC freed some
            for (idx, seg) in self.segments.iter().enumerate() {
                let Some(seg) = seg else {
                    continue;
                };
                if (seg.allocator().free_bytes() as usize) < size {
                    continue;
                }
                if let Some(a) = seg.allocator().alloc(size) {
                    if idx < self.idle_since.len() {
                        self.idle_since[idx] = None;
                    }
                    return Ok(PoolAllocation {
                        seg_idx: idx as u32,
                        generation: self.generations[idx],
                        offset: a.offset,
                        actual_size: a.actual_size,
                        level: a.level,
                        is_dedicated: false,
                    });
                }
            }
        }

        // Layer 2: Create new segment.
        if self.segments.len() < self.config.max_segments {
            let seg = self.create_segment()?;
            let idx = self.segments.len();
            self.segments.push(Some(seg));
            self.idle_since.push(None);
            if let Some(a) = self
                .segment(idx)
                .expect("owner segment slot")
                .allocator()
                .alloc(size)
            {
                return Ok(PoolAllocation {
                    seg_idx: idx as u32,
                    generation: self.generations[idx],
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
        // Always GC expired dedicated segments to reclaim SHM resources.
        // This must run unconditionally because freed-but-not-GC'd segments
        // still hold mapped memory even though they don't count as "active".
        self.gc_dedicated();

        let active_dedicated = self
            .dedicated
            .values()
            .filter(|e| e.freed_at.is_none())
            .count();
        if active_dedicated >= self.config.max_dedicated_segments {
            return Err(format!(
                "dedicated segment limit reached ({} active)",
                active_dedicated
            ));
        }

        let idx = self.next_dedicated_idx;
        if idx > u16::MAX as u32 {
            return Err("dedicated segment index exhausted".into());
        }
        let name = Self::dedicated_segment_name(&self.name_prefix, idx);
        let seg = DedicatedSegment::create(&name, size)?;
        self.next_dedicated_idx = self
            .next_dedicated_idx
            .checked_add(1)
            .expect("dedicated segment index overflow");

        // R-I2: Guard against u32 truncation for dedicated segment size.
        if seg.size() > u32::MAX as usize {
            return Err(format!(
                "dedicated segment size {} exceeds 4GB limit",
                seg.size()
            ));
        }
        let alloc_size = seg.size() as u32;
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
                is_creator: true,
            },
        );

        Ok(PoolAllocation {
            seg_idx: idx,
            generation: 0,
            offset: 0,
            actual_size: alloc_size,
            level: 0,
            is_dedicated: true,
        })
    }

    fn free_buddy(&mut self, alloc: &PoolAllocation) -> Result<(), String> {
        let idx = alloc.seg_idx as usize;
        if let Some(seg) = self.segment(idx) {
            seg.allocator().free(alloc.offset, alloc.level)?;
            // Track idle: if segment is now empty, record the time.
            if seg.allocator().alloc_count() == 0
                && idx < self.idle_since.len()
                && self.idle_since[idx].is_none()
            {
                self.idle_since[idx] = Some(Instant::now());
            }
            Ok(())
        } else {
            Err(format!("invalid segment index {}", alloc.seg_idx))
        }
    }

    fn free_dedicated(&mut self, seg_idx: u32) {
        if let Some(entry) = self.dedicated.get_mut(&seg_idx) {
            if !entry.is_creator {
                entry.segment.mark_read_done();
            }
            entry.freed_at = Some(Instant::now());
        }
    }

    fn create_segment(&mut self) -> Result<BuddySegment, String> {
        if self.is_peer {
            return Err("cannot create peer backing".into());
        }
        let idx = self.segments.len();
        let generation = self
            .generations
            .get(idx)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("buddy backing generation exhausted")?;
        let name = Self::buddy_segment_name(&self.name_prefix, idx as u32, generation);
        let segment =
            BuddySegment::create(&name, self.config.segment_size, self.config.min_block_size)?;
        if self.generations.len() <= idx {
            self.generations.resize(idx + 1, 0);
        }
        self.generations[idx] = generation;
        Ok(segment)
    }
}

fn validate_duration_secs(name: &str, secs: f64) -> Result<(), String> {
    if !secs.is_finite() {
        return Err(format!("{name} must be finite"));
    }
    if secs < 0.0 {
        return Ok(());
    }
    Duration::try_from_secs_f64(secs)
        .map(|_| ())
        .map_err(|_| format!("{name} must be a representable duration in seconds"))
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
            dedicated_crash_timeout_secs: 0.0,
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
    fn test_ensure_buddy_segments_precreates_advertised_segments() {
        let mut pool = test_pool(small_config());
        pool.ensure_buddy_segments(3).unwrap();
        assert_eq!(pool.segment_count(), 3);
        for idx in 0..3 {
            let name = pool.segment_name(idx).unwrap();
            assert_eq!(
                name,
                MemPool::buddy_segment_name(
                    pool.prefix(),
                    idx as u32,
                    pool.segment_generation(idx).unwrap()
                )
            );
            assert!(BuddySegment::open(name, small_config().segment_size).is_ok());
        }
        assert!(pool.ensure_buddy_segments(5).is_err());
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
            dedicated_crash_timeout_secs: 0.0,
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
            dedicated_crash_timeout_secs: 0.0,
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
    fn test_copy_data_at_reads_buddy_and_dedicated_allocations() {
        let mut pool = test_pool(small_config());
        for payload in [b"buddy".repeat(32), b"dedicated".repeat(20_000)] {
            let allocation = pool.alloc(payload.len()).unwrap();
            let ptr = pool.data_ptr(&allocation).unwrap();
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
            }

            assert_eq!(
                pool.copy_data_at(
                    allocation.seg_idx,
                    allocation.generation,
                    allocation.offset,
                    u32::try_from(payload.len()).unwrap(),
                    allocation.is_dedicated,
                )
                .unwrap(),
                payload,
            );
            pool.free(&allocation).unwrap();
        }
    }

    #[test]
    fn test_copy_data_at_rejects_spans_outside_mapped_data() {
        let mut pool = test_pool(small_config());
        let buddy = pool.alloc(4096).unwrap();
        let buddy_capacity = pool
            .segment(buddy.seg_idx as usize)
            .unwrap()
            .allocator()
            .data_size();
        assert!(
            pool.copy_data_at(
                buddy.seg_idx,
                buddy.generation,
                u32::try_from(buddy_capacity - 1).unwrap(),
                2,
                false,
            )
            .unwrap_err()
            .contains("outside buddy segment")
        );
        assert!(
            pool.validate_data_at(
                buddy.seg_idx,
                buddy.generation,
                u32::try_from(buddy_capacity - 1).unwrap(),
                2,
                false,
            )
            .unwrap_err()
            .contains("outside buddy segment")
        );
        assert!(
            pool.copy_data_at(buddy.seg_idx, buddy.generation, buddy.offset, 0, false)
                .unwrap_err()
                .contains("must not be empty")
        );
        pool.free(&buddy).unwrap();

        let dedicated = pool.alloc(128 * 1024).unwrap();
        assert!(dedicated.is_dedicated);
        assert!(
            pool.copy_data_at(dedicated.seg_idx, 0, 1, 1, true)
                .unwrap_err()
                .contains("offset must be zero")
        );
        assert!(
            pool.copy_data_at(dedicated.seg_idx, 0, 0, u32::MAX, true)
                .unwrap_err()
                .contains("outside dedicated segment")
        );
        pool.free(&dedicated).unwrap();
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
            dedicated_crash_timeout_secs: f64::NAN,
            ..small_config()
        };
        test_pool(config);
    }

    #[test]
    fn test_validate_rejects_non_finite_gc_delays() {
        let mut config = small_config();
        config.dedicated_crash_timeout_secs = f64::INFINITY;
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("dedicated_crash_timeout_secs")
        );

        let mut config = small_config();
        config.buddy_idle_decay_secs = f64::INFINITY;
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("buddy_idle_decay_secs")
        );
    }

    #[test]
    fn test_validate_rejects_unrepresentable_gc_delays() {
        let mut config = small_config();
        config.dedicated_crash_timeout_secs = 1e100;
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("representable duration")
        );

        let mut config = small_config();
        config.buddy_idle_decay_secs = 1e100;
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("representable duration")
        );
    }

    #[test]
    fn test_validate_negative_gc_delay_ok() {
        // Negative gc_delay is allowed (clamped to 0 at runtime)
        let config = PoolConfig {
            dedicated_crash_timeout_secs: -1.0,
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
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
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
            dedicated_crash_timeout_secs: 0.0,
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
        assert_eq!(
            name0,
            MemPool::buddy_segment_name(pool.prefix(), 0, a0.generation)
        );
        assert!(name0.len() <= 30);

        // Fill segment 0 to force segment 1 creation.
        let big = pool.config.segment_size;
        pool.free(&a0).unwrap();
        let a_big = pool.alloc(big).unwrap();
        assert_eq!(a_big.seg_idx, 0);
        let a1 = pool.alloc(4096).unwrap();
        assert_eq!(a1.seg_idx, 1);
        let name1 = pool.segment_name(1).unwrap();
        assert_eq!(
            name1,
            MemPool::buddy_segment_name(pool.prefix(), 1, a1.generation)
        );

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
        assert_eq!(
            name2,
            MemPool::buddy_segment_name(pool.prefix(), 2, a2.generation)
        );

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
        assert_eq!(dname0, MemPool::dedicated_segment_name(pool.prefix(), 256));

        let d1 = pool.alloc(big).unwrap();
        assert!(d1.is_dedicated);
        assert_eq!(d1.seg_idx, 257);
        let dname1 = pool.dedicated_name(257).unwrap();
        assert_eq!(dname1, MemPool::dedicated_segment_name(pool.prefix(), 257));

        pool.free(&d0).unwrap();
        pool.free(&d1).unwrap();
    }

    #[test]
    fn test_prefix_accessor() {
        let pool = test_pool(test_config());
        let prefix = pool.prefix();
        assert!(prefix.starts_with("/cc3t"), "got: {}", prefix);
    }

    #[test]
    fn test_prefix_max_valid_length() {
        let prefix = "x".repeat(214); // 41 bytes of PID/UUID incarnation follow.
        let mut pool = MemPool::new_with_prefix(test_config(), prefix);
        assert_eq!(pool.prefix().len(), 255);
        pool.ensure_ready().unwrap();
        assert!(pool.segment_name(0).unwrap().len() <= 30);
    }

    #[test]
    #[should_panic(expected = "exceeds handshake maximum")]
    fn test_prefix_too_long_panics() {
        let prefix = "x".repeat(215);
        let _ = MemPool::new_with_prefix(test_config(), prefix);
    }

    #[test]
    fn test_gc_buddy_respects_decay_window() {
        // With a large decay window, idle trailing segments should NOT be reclaimed.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 3600.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        let b = pool.alloc(32 * 1024).unwrap();
        assert!(pool.segment_count() >= 2);
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
        // Despite idleness, decay window prevents reclamation.
        let reclaimed = pool.gc_buddy();
        assert_eq!(reclaimed, 0);
        assert!(pool.segment_count() >= 2);
    }

    #[test]
    fn test_gc_buddy_reclaims_with_zero_decay() {
        // With zero decay, idle trailing segments should be reclaimed immediately.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        let b = pool.alloc(32 * 1024).unwrap();
        let seg_before = pool.segment_count();
        assert!(seg_before >= 2);
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
        let reclaimed = pool.gc_buddy();
        assert!(
            reclaimed >= 1,
            "expected at least one idle segment to be reclaimed"
        );
        assert!(pool.segment_count() < seg_before);
        // gc_buddy always retains at least one segment.
        assert!(pool.segment_count() >= 1);
    }

    #[test]
    fn reclaimed_buddy_slot_gets_a_new_backing_identity() {
        let mut pool = test_pool(PoolConfig {
            segment_size: 32 * 1024,
            max_segments: 2,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        });
        let first = pool.alloc(32 * 1024).unwrap();
        let old = pool.alloc(32 * 1024).unwrap();
        let old_name = pool.segment_name(old.seg_idx as usize).unwrap().to_owned();
        let old_view = BuddySegment::open(&old_name, 32 * 1024).unwrap();
        pool.free(&old).unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        let new = pool.alloc(32 * 1024).unwrap();
        assert_eq!(new.seg_idx, old.seg_idx);
        assert_ne!(pool.segment_name(new.seg_idx as usize).unwrap(), old_name);
        assert_eq!(old_view.allocator().alloc_count(), 0);
        assert!(new.generation > old.generation);
        assert!(pool.data_ptr(&old).is_err());
        assert!(pool.free(&old).is_err());
        assert_eq!(
            pool.segment(new.seg_idx as usize)
                .unwrap()
                .allocator()
                .alloc_count(),
            1
        );
        pool.free(&new).unwrap();
        pool.free(&first).unwrap();
    }

    #[test]
    fn peer_cache_retires_idle_views_and_rejects_older_generations() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            max_segments: 2,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        };
        let mut owner = test_pool(config.clone());
        let first = owner.alloc(32 * 1024).unwrap();
        let old = owner.alloc(32 * 1024).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned());
        peer.ensure_peer_segment(old.seg_idx, old.generation, 32 * 1024)
            .unwrap();
        unsafe {
            *owner.data_ptr(&old).unwrap() = 0x21;
        }
        assert_eq!(
            peer.copy_data_at(old.seg_idx, old.generation, old.offset, 1, false)
                .unwrap(),
            [0x21]
        );
        assert!(
            peer.ensure_peer_segment(old.seg_idx, old.generation + 1, 32 * 1024)
                .unwrap_err()
                .contains("live allocations")
        );
        assert_eq!(owner.gc_buddy(), 0);
        peer.free_at(old.seg_idx, old.generation, old.offset, 32 * 1024, false)
            .unwrap();
        assert!(peer.segment(old.seg_idx as usize).is_none());
        assert_eq!(owner.gc_buddy(), 1);
        let fresh = owner.alloc(32 * 1024).unwrap();
        assert!(fresh.generation > old.generation);
        peer.ensure_peer_segment(fresh.seg_idx, fresh.generation, 32 * 1024)
            .unwrap();
        unsafe {
            *owner.data_ptr(&fresh).unwrap() = 0x63;
        }
        assert_eq!(
            peer.copy_data_at(fresh.seg_idx, fresh.generation, fresh.offset, 1, false)
                .unwrap(),
            [0x63]
        );
        assert!(
            peer.ensure_peer_segment(old.seg_idx, old.generation, 1)
                .unwrap_err()
                .contains("stale")
        );
        assert!(
            peer.free_at(old.seg_idx, old.generation, old.offset, 32 * 1024, false)
                .is_err()
        );
        assert_eq!(
            owner
                .segment(fresh.seg_idx as usize)
                .unwrap()
                .allocator()
                .alloc_count(),
            1
        );
        peer.free_at(
            fresh.seg_idx,
            fresh.generation,
            fresh.offset,
            32 * 1024,
            false,
        )
        .unwrap();
        owner.free(&first).unwrap();
    }

    #[test]
    fn backing_generation_and_dedicated_index_never_wrap() {
        let mut owner = test_pool(PoolConfig {
            max_segments: 2,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        });
        let size = owner.config.segment_size;
        let first = owner.alloc(size).unwrap();
        let old = owner.alloc(size).unwrap();
        owner.free(&old).unwrap();
        owner.gc_buddy();
        owner.generations[1] = u32::MAX;
        assert!(
            owner
                .alloc(size)
                .unwrap_err()
                .contains("generation exhausted")
        );
        owner.next_dedicated_idx = u16::MAX as u32;
        let last = owner.alloc(size * 2).unwrap();
        assert_eq!(last.seg_idx, u16::MAX as u32);
        assert!(
            owner
                .alloc(size * 2)
                .unwrap_err()
                .contains("index exhausted")
        );
        owner.free(&last).unwrap();
        owner.free(&first).unwrap();
    }

    #[test]
    fn owner_incarnations_do_not_reuse_identical_labels() {
        let mut first = MemPool::new_with_prefix(test_config(), "same-label".into());
        let mut second = MemPool::new_with_prefix(test_config(), "same-label".into());
        first.ensure_ready().unwrap();
        second.ensure_ready().unwrap();
        assert_ne!(first.prefix(), second.prefix());
        assert_ne!(first.segment_name(0), second.segment_name(0));
    }

    #[test]
    fn opening_and_releasing_lower_slot_keeps_higher_live_mapping() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            max_segments: 2,
            ..test_config()
        };
        let mut owner = test_pool(config.clone());
        let low = owner.alloc(32 * 1024).unwrap();
        let high = owner.alloc(32 * 1024).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned());
        peer.ensure_peer_segment(high.seg_idx, high.generation, 32 * 1024)
            .unwrap();
        let high_ptr = peer
            .data_ptr_at(high.seg_idx, high.generation, high.offset, false)
            .unwrap();
        unsafe {
            *owner.data_ptr(&high).unwrap() = 0x56;
        }
        peer.ensure_peer_segment(low.seg_idx, low.generation, 32 * 1024)
            .unwrap();
        peer.free_at(low.seg_idx, low.generation, low.offset, 32 * 1024, false)
            .unwrap();
        assert_eq!(
            peer.data_ptr_at(high.seg_idx, high.generation, high.offset, false)
                .unwrap(),
            high_ptr
        );
        assert_eq!(unsafe { *high_ptr }, 0x56);
        assert_eq!(peer.stats().total_segments, 1);
        peer.free_at(high.seg_idx, high.generation, high.offset, 32 * 1024, false)
            .unwrap();
    }

    #[test]
    fn busy_zero_counters_never_authorize_retirement() {
        let config = PoolConfig {
            max_segments: 2,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        };
        let mut owner = test_pool(config.clone());
        owner.ensure_buddy_segments(2).unwrap();
        let generation = owner.segment_generation(1).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned());
        peer.ensure_peer_segment(1, generation, 4096).unwrap();
        let word = unsafe {
            owner
                .segment(1)
                .unwrap()
                .base_ptr()
                .add(std::mem::offset_of!(crate::SegmentHeader, spinlock))
        };
        let lock = unsafe { crate::ShmSpinlock::new(word) };
        lock.try_lock().unwrap();
        // The last free publishes a zero count before merging all bitmaps.
        // While that critical section is still live, none of the three
        // retirement paths may treat the zero count as a completed transition.
        assert_eq!(owner.segment(1).unwrap().allocator().alloc_count(), 0);
        assert_eq!(owner.gc_buddy(), 0);
        assert_eq!(peer.gc_buddy(), 0);
        assert!(
            peer.ensure_peer_segment(1, generation + 1, 4096)
                .unwrap_err()
                .contains("cannot retire")
        );
        assert!(owner.segment(1).is_some());
        assert!(peer.segment(1).is_some());
        lock.unlock();
        assert_eq!(peer.gc_buddy(), 1);
        assert_eq!(owner.gc_buddy(), 1);
    }

    #[test]
    fn poisoned_empty_counters_never_authorize_retirement() {
        let config = PoolConfig {
            max_segments: 2,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        };
        let mut owner = test_pool(config.clone());
        owner.ensure_buddy_segments(2).unwrap();
        let generation = owner.segment_generation(1).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned());
        peer.ensure_peer_segment(1, generation, 4096).unwrap();
        let word = unsafe {
            owner
                .segment(1)
                .unwrap()
                .base_ptr()
                .add(std::mem::offset_of!(crate::SegmentHeader, spinlock))
        };
        let lock = unsafe { crate::ShmSpinlock::new(word) };
        lock.try_lock().unwrap();
        lock.poison_panicked(std::process::id());
        assert!(owner.segment(1).unwrap().allocator().is_poisoned());
        assert_eq!(owner.gc_buddy(), 0);
        assert_eq!(peer.gc_buddy(), 0);
        assert!(
            peer.ensure_peer_segment(1, generation + 1, 4096)
                .unwrap_err()
                .contains("cannot retire")
        );
        assert!(owner.segment(1).is_some());
        assert!(peer.segment(1).is_some());
    }

    #[test]
    fn released_dedicated_mapping_cannot_be_reopened_before_owner_gc() {
        let config = test_config();
        let mut owner = test_pool(config.clone());
        let allocation = owner.alloc(config.segment_size * 2).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned());
        peer.ensure_peer_dedicated(allocation.seg_idx, 4096)
            .unwrap();
        peer.free_at(allocation.seg_idx, 0, 0, 4096, true).unwrap();
        peer.gc_dedicated();
        assert!(
            peer.ensure_peer_dedicated(allocation.seg_idx, 4096)
                .unwrap_err()
                .contains("already been released")
        );
        assert!(peer.free_at(allocation.seg_idx, 0, 0, 4096, true).is_err());
        owner.free(&allocation).unwrap();
        owner.gc_dedicated();
        assert!(owner.dedicated_name(allocation.seg_idx).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_name_parser_accepts_only_current_bounded_namespace() {
        let prefix = format!(
            "label_{:08x}{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let name = MemPool::buddy_segment_name(&prefix, 2, 3);
        assert_eq!(
            MemPool::extract_pid_from_name(name.trim_start_matches('/')),
            Some(std::process::id())
        );
        for invalid in [
            "c2",
            "c2not-a-pid-and-not-a-valid-digest",
            "other-library",
            "c2\u{e9}",
        ] {
            assert_eq!(MemPool::extract_pid_from_name(invalid), None);
        }
        let mut bad = name.trim_start_matches('/').as_bytes().to_vec();
        bad[2] = b'!';
        assert_eq!(
            MemPool::extract_pid_from_name(std::str::from_utf8(&bad).unwrap()),
            None
        );
        let zero_owner = MemPool::buddy_segment_name("not-an-owner-prefix", 0, 1);
        assert_eq!(
            MemPool::extract_pid_from_name(zero_owner.trim_start_matches('/')),
            None
        );
    }

    #[test]
    fn test_dedicated_gc_flag_based() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 60.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);

        let a = pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);
        let seg_idx = a.seg_idx;

        // Creator frees → freed_at set, but read_done still 0
        pool.free(&a).unwrap();

        // GC should NOT remove (read_done == 0, timeout not reached)
        pool.gc_dedicated();
        assert!(pool.dedicated.contains_key(&seg_idx));

        // Simulate reader: set read_done via the segment
        pool.dedicated
            .get(&seg_idx)
            .unwrap()
            .segment
            .mark_read_done();

        // Now GC should remove
        pool.gc_dedicated();
        assert!(!pool.dedicated.contains_key(&seg_idx));
    }

    #[test]
    fn test_dedicated_reader_gc_immediate() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 60.0,
            ..PoolConfig::default()
        };
        let mut creator_pool = test_pool(config.clone());
        let a = creator_pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);
        let seg_name = creator_pool
            .dedicated
            .get(&a.seg_idx)
            .unwrap()
            .segment
            .name()
            .to_string();
        let data_size = 64 * 1024;

        // Open in a second pool (simulates reader/peer side)
        let mut reader_pool = test_pool(config);
        reader_pool
            .open_dedicated_at(a.seg_idx, &seg_name, data_size)
            .unwrap();
        assert!(reader_pool.dedicated.contains_key(&a.seg_idx));

        // Reader frees → should mark read_done + be immediately removable
        reader_pool
            .free_at(a.seg_idx, 0, 0, data_size as u32, true)
            .unwrap();
        reader_pool.gc_dedicated();
        assert!(!reader_pool.dedicated.contains_key(&a.seg_idx));

        // Creator sees read_done
        assert!(
            creator_pool
                .dedicated
                .get(&a.seg_idx)
                .unwrap()
                .segment
                .is_read_done()
        );

        creator_pool.free(&a).unwrap();
        creator_pool.gc_dedicated();
        assert!(!creator_pool.dedicated.contains_key(&a.seg_idx));
    }

    #[test]
    fn try_alloc_shm_returns_buddy() {
        let mut pool = test_pool(small_config());
        let h = pool.try_alloc_shm(4096).unwrap();
        assert!(h.is_buddy());
        pool.release_handle(h);
    }

    #[test]
    fn try_alloc_shm_never_file_spills() {
        // Pool with 1 small buddy segment, no dedicated, spill disabled.
        let cfg = PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_try_shm_test"),
        };
        let mut pool = test_pool(cfg);
        // Fill the single buddy segment completely
        let mut held = Vec::new();
        loop {
            match pool.alloc_handle(4096) {
                Ok(h) if h.is_buddy() => held.push(h),
                _ => break,
            }
        }
        assert!(
            !held.is_empty(),
            "should have allocated at least one buddy block"
        );
        // Now try_alloc_shm should fail — NOT fall back to FileSpill
        let result = pool.try_alloc_shm(4096);
        assert!(result.is_err());
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
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
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
        let result = pool.release_handle(handle);
        assert!(matches!(result, FreeResult::DedicatedFreed { .. }));
    }

    #[test]
    fn test_alloc_handle_file_spill_forced() {
        let dir = std::env::temp_dir().join("c2_pool_spill_test");
        let config = PoolConfig {
            spill_threshold: 0.0,
            spill_dir: dir.clone(),
            ..test_config()
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
        assert_eq!(
            &pool.copy_handle_data(&handle).unwrap()[..pattern.len()],
            pattern
        );
        pool.release_handle(handle);
    }

    #[test]
    fn test_handle_slice_file_spill() {
        let dir = std::env::temp_dir().join("c2_pool_spill_slice_test");
        let config = PoolConfig {
            spill_threshold: 0.0,
            spill_dir: dir.clone(),
            ..test_config()
        };
        let mut pool = MemPool::new(config);
        let mut handle = pool.alloc_handle(8192).unwrap();
        let pattern = b"spill_pattern_data";
        pool.handle_slice_mut(&mut handle)[..pattern.len()].copy_from_slice(pattern);
        assert_eq!(&pool.handle_slice(&handle)[..pattern.len()], pattern);
        assert_eq!(
            &pool.copy_handle_data(&handle).unwrap()[..pattern.len()],
            pattern
        );
        handle.set_len(8193);
        assert!(
            pool.copy_handle_data(&handle)
                .unwrap_err()
                .contains("outside file-spill mapping")
        );
        handle.set_len(8192);
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_copy_handle_data_rejects_publicly_constructed_out_of_bounds_handle() {
        let mut pool = MemPool::new(test_config());
        pool.ensure_buddy_segments(1).unwrap();
        let capacity = pool.segment(0).unwrap().allocator().data_size();
        let handle = MemHandle::Buddy {
            seg_idx: 0,
            generation: pool.segment_generation(0).unwrap(),
            offset: u32::try_from(capacity - 1).unwrap(),
            allocation_size: 4096,
            len: 2,
        };
        assert!(
            pool.copy_handle_data(&handle)
                .unwrap_err()
                .contains("outside buddy segment")
        );
        assert!(
            pool.validate_handle(&handle)
                .unwrap_err()
                .contains("outside buddy segment")
        );
    }

    #[test]
    fn trimmed_handles_release_the_original_allocation() {
        for length in [0, 1, 4096] {
            let mut pool = MemPool::new(test_config());
            let initial = 32 * 1024;
            let mut handle = pool.try_alloc_shm(initial).unwrap();
            let capacity = pool.segment(0).unwrap().allocator().data_size() as u64;
            handle.set_len(length);
            assert_eq!(pool.handle_slice(&handle).len(), length);
            assert_eq!(pool.copy_handle_data(&handle).unwrap().len(), length);
            pool.release_handle(handle);
            assert_eq!(pool.stats().alloc_count, 0);
            assert_eq!(pool.stats().free_bytes, capacity);
        }
    }
}
