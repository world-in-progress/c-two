//! Unified memory pool with tiered fallback.
//!
//! Manages multiple buddy-allocated SHM segments and dedicated
//! segments for oversized payloads.  Implements the fallback chain:
//!   T1. Try existing buddy segments
//!   T2. Create new buddy segment (up to max_segments)
//!   T3. Fall back to dedicated segment

use crate::alloc::BuddyAllocator;
use crate::buddy_segment::BuddySegment;
use crate::budget::{BudgetError, BudgetKind, BudgetReservation, MemoryBudget};
use crate::config::{PoolAllocation, PoolConfig, PoolStats};
use crate::dedicated::DedicatedSegment;
use crate::handle::MemHandle;
use crate::pressure::{PressureDecision, PressureEngine};
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

/// Backing-creation failure at a policy-enforced seam.
///
/// Budget rejections and OS-memory pressure denials are separable from other
/// creation errors so the single fallback chain can still try a smaller
/// eligible tier (for example a dedicated backing that fits when a full buddy
/// segment does not). Creation failures may also continue through the same
/// eligible fallback tiers.
enum BackingError {
    Budget(BudgetError),
    Pressure(String),
    Other(String),
}

impl BackingError {
    fn into_message(self) -> String {
        match self {
            BackingError::Budget(e) => e.to_string(),
            BackingError::Pressure(message) => message,
            BackingError::Other(message) => message,
        }
    }
}

/// Lifetime tier-selection counters behind [`PoolStats`].
#[derive(Default, Debug, Clone, Copy)]
struct TierCounters {
    buddy_reused_allocs: u64,
    buddy_expanded_allocs: u64,
    dedicated_allocs: u64,
    file_spill_allocs: u64,
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
    /// Owner-creation budget context. Owner pools always carry one (shared or
    /// private); peer pools never charge owner-creation budget and carry none.
    budget: Option<MemoryBudget>,
    /// Creation-time OS-memory pressure decisions for owner backing seams.
    /// Peer pools never create backings, so theirs stays idle.
    pressure: PressureEngine,
    /// Lifetime tier-selection counters surfaced read-only through `stats()`.
    counters: TierCounters,
}

impl MemPool {
    /// Create a new pool. Segments are lazily created on first alloc.
    ///
    /// The pool carries a private finite budget built from the canonical
    /// [`c2_config::MemoryBudgetLimits`] defaults; share one budget across
    /// pools with [`MemPool::new_with_prefix_and_budget`].
    pub fn new(config: PoolConfig) -> Self {
        let pid = std::process::id();
        let name_prefix = format!("/cc3b{:08x}", pid);
        Self::new_with_prefix(config, name_prefix)
    }

    /// Prefixes travel in the handshake; OS object names are bounded digests.
    const MAX_SHM_PREFIX_LEN: usize = 255;

    /// Create a new pool with a custom name prefix (for testing / multi-pool).
    ///
    /// Each pool receives its own private finite budget from the canonical
    /// limits; pools constructed this way do not share accounting.
    pub fn new_with_prefix(config: PoolConfig, name_prefix: String) -> Self {
        let name_prefix = format!(
            "{name_prefix}_{:08x}{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Self::with_identity(config, name_prefix, false, Some(Self::private_budget()))
    }

    /// Create a new owner pool whose backing creation charges a shared budget.
    ///
    /// `budget` is shared by reference with every other pool or owner handed
    /// the same [`MemoryBudget`] clone: buddy expansion, dedicated creation,
    /// and file spill in this pool all charge the same cells. Existing owner
    /// constructors keep private budgets, so sharing is always explicit.
    pub fn new_with_prefix_and_budget(
        config: PoolConfig,
        name_prefix: String,
        budget: MemoryBudget,
    ) -> Self {
        let name_prefix = format!(
            "{name_prefix}_{:08x}{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Self::with_identity(config, name_prefix, false, Some(budget))
    }

    /// Receive-side cache for one producer's advertised pool incarnation.
    ///
    /// Peer pools open existing peer backings and never charge the
    /// owner-creation budget; they carry no budget context.
    pub fn open_peer(config: PoolConfig, name_prefix: String) -> Self {
        Self::with_identity(config, name_prefix, true, None)
    }

    fn private_budget() -> MemoryBudget {
        MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default())
    }

    /// The pool's backing budget context, if any.
    ///
    /// Owner pools always carry one (shared or private); peer pools return
    /// `None` because peer-opened mappings never acquire owner-creation
    /// charges.
    pub fn budget(&self) -> Option<&MemoryBudget> {
        self.budget.as_ref()
    }

    fn with_identity(
        config: PoolConfig,
        name_prefix: String,
        is_peer: bool,
        budget: Option<MemoryBudget>,
    ) -> Self {
        assert!(
            name_prefix.len() <= Self::MAX_SHM_PREFIX_LEN,
            "SHM prefix '{}' is {} bytes, exceeds handshake maximum {}",
            name_prefix,
            name_prefix.len(),
            Self::MAX_SHM_PREFIX_LEN,
        );
        Self::validate_config(&config).expect("invalid PoolConfig");
        let pressure = PressureEngine::new(config.spill_threshold);
        Self {
            config,
            segments: Vec::new(),
            generations: Vec::new(),
            is_peer,
            dedicated: HashMap::new(),
            name_prefix,
            next_dedicated_idx: 256,
            idle_since: Vec::new(),
            budget,
            pressure,
            counters: TierCounters::default(),
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
        // Checked doubling: a wrapped `2 * min_block_size` could otherwise
        // pass as a small bound and authorize an impossible layout.
        let min_segment_size = config.min_block_size.checked_mul(2).ok_or_else(|| {
            format!(
                "min_block_size ({}) is too large: doubling overflows the platform address space",
                config.min_block_size
            )
        })?;
        if config.segment_size < min_segment_size {
            return Err(format!(
                "segment_size ({}) must be >= 2 * min_block_size ({})",
                config.segment_size, config.min_block_size
            ));
        }
        if config.max_segments > u16::MAX as usize + 1 {
            return Err("max_segments exceeds the wire segment-index range".into());
        }
        if config.min_retained_segments > config.max_segments {
            return Err(format!(
                "min_retained_segments ({}) must not exceed max_segments ({})",
                config.min_retained_segments, config.max_segments
            ));
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
    ///
    /// Tier order: reuse an existing buddy segment, expand the buddy pool,
    /// then create a dedicated backing. Buddy expansion and dedicated
    /// creation are the policy-enforced seams: both validate their total
    /// mapped bytes and consult the pressure engine and the finite budget
    /// before mapping anything. Reusing already-mapped buddy blocks is
    /// always permitted and takes no new charge.
    pub fn alloc(&mut self, size: usize) -> Result<PoolAllocation, String> {
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        self.gc_dedicated();
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }

        let max_buddy_block = self.buddy_block_limit();

        if max_buddy_block > 0 && size <= max_buddy_block {
            // Try buddy allocation.
            self.alloc_buddy(size).map_err(BackingError::into_message)
        } else {
            // Too large for buddy or buddy disabled → dedicated segment.
            self.alloc_dedicated(size)
                .map_err(BackingError::into_message)
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
    ///
    /// Read-only observations over this pool's own backing: buddy data-region
    /// capacities, dedicated mapped capacities including pending-free
    /// entries, tier-selection counters, and the pressure-denial count. The
    /// values never claim whole-process RSS; file and live-reassembly bytes
    /// belong to the budget cells.
    pub fn stats(&self) -> PoolStats {
        let mut buddy_segments = 0usize;
        let mut buddy_data = 0u64;
        let mut buddy_idle = 0u64;
        let mut alloc_count = 0u32;

        for seg in self.segments.iter().flatten() {
            let a = seg.allocator();
            buddy_segments += 1;
            buddy_data += a.data_size() as u64;
            buddy_idle += a.free_bytes();
            alloc_count += a.alloc_count();
        }
        let buddy_occupied = buddy_data.saturating_sub(buddy_idle);

        let mut dedicated_mapped = 0u64;
        let mut dedicated_active_count = 0usize;
        let mut dedicated_active = 0u64;
        let mut dedicated_pending_free = 0u64;
        for entry in self.dedicated.values() {
            let bytes = entry.segment.size() as u64;
            dedicated_mapped += bytes;
            if entry.freed_at.is_none() {
                dedicated_active_count += 1;
                dedicated_active += bytes;
                alloc_count += 1;
            } else {
                dedicated_pending_free += bytes;
            }
        }

        let utilization_ratio = if buddy_data > 0 {
            buddy_occupied as f64 / buddy_data as f64
        } else {
            0.0
        };

        PoolStats {
            total_segments: buddy_segments,
            dedicated_segments: self.dedicated.len(),
            alloc_count,
            buddy_data_bytes: buddy_data,
            buddy_occupied_bytes: buddy_occupied,
            buddy_idle_bytes: buddy_idle,
            dedicated_mapped_bytes: dedicated_mapped,
            dedicated_active_count,
            dedicated_active_bytes: dedicated_active,
            dedicated_pending_free_bytes: dedicated_pending_free,
            buddy_reused_allocs: self.counters.buddy_reused_allocs,
            buddy_expanded_allocs: self.counters.buddy_expanded_allocs,
            dedicated_allocs: self.counters.dedicated_allocs,
            file_spill_allocs: self.counters.file_spill_allocs,
            pressure_denied_backings: self.pressure.denials(),
            utilization_ratio,
        }
    }

    /// Reclaim idle buddy segments from the end of the segment list.
    ///
    /// Only pops trailing empty segments to avoid index remapping (segment
    /// indices are encoded in wire frames). Retires down to the configured
    /// `min_retained_segments` (which may be zero); generation counters are
    /// never truncated, so a later re-created slot gets a fresh generation and
    /// stale peer references to retired backings are rejected. Returns the
    /// number of segments reclaimed.
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
        while self.segments.len() > self.config.min_retained_segments {
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

    /// Whether a freed dedicated backing for these coordinates still awaits
    /// retirement.
    ///
    /// `true` while the creator-side entry is present with `freed_at` set but
    /// not yet reclaimed — that is, while the peer's cross-process
    /// `read_done` signal or the configured crash-timeout policy has not yet
    /// retired it through [`MemPool::gc_dedicated`]. `false` once the entry
    /// is gone (retired, or the index was reused by a later live allocation).
    ///
    /// This is a generic pool status seam: it reports physical retirement
    /// state only and carries no transport or routing policy. Owners use it
    /// to keep the mapping (and its budget charge) alive until the peer is
    /// provably done reading — `PoolStats::alloc_count` already excludes
    /// pending-free entries, so it must never be used to infer retirement.
    pub fn dedicated_awaiting_retirement(&self, alloc: &PoolAllocation) -> bool {
        alloc.is_dedicated
            && self
                .dedicated
                .get(&alloc.seg_idx)
                .is_some_and(|entry| entry.freed_at.is_some())
    }

    /// Ensure at least one buddy segment exists.
    ///
    /// No-op when the buddy tiers are policy-disabled; transports prewarm only
    /// through [`MemPool::ensure_buddy_segments`] so laziness stays explicit.
    /// Does nothing if segments already exist.
    pub fn ensure_ready(&mut self) -> Result<(), String> {
        if self.config.buddy_enabled && self.config.max_segments > 0 {
            self.ensure_buddy_segments(1)?;
        }
        Ok(())
    }

    /// Ensure a stable set of buddy segments exists before peer advertisement.
    ///
    /// Runtime adapters that advertise their client buddy pool in an IPC
    /// handshake should call this before exposing segment metadata. This avoids
    /// later lazy allocation from returning a segment index the peer never saw.
    /// This is also the only prewarm entry point; it is rejected outright when
    /// the buddy tiers are disabled so a disabled pool never maps buddy memory.
    pub fn ensure_buddy_segments(&mut self, count: usize) -> Result<(), String> {
        if self.is_peer {
            return Err("cannot create backings in a peer pool".into());
        }
        if !self.config.buddy_enabled {
            return Err("cannot prewarm buddy segments while the buddy pool is disabled".into());
        }
        if count > self.config.max_segments {
            return Err(format!(
                "requested {} buddy segments exceeds configured max_segments {}",
                count, self.config.max_segments
            ));
        }
        while self.segments.len() < count {
            let seg = self.create_segment().map_err(BackingError::into_message)?;
            self.segments.push(Some(seg));
            self.idle_since.push(None);
        }
        Ok(())
    }

    /// Get the number of buddy segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Read-only view of the effective pool configuration, including the
    /// buddy policy projected from IPC config.
    pub fn config(&self) -> &PoolConfig {
        &self.config
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

    /// Check that this pool is the allocation owner with the captured
    /// incarnation, without opening, reading, or releasing any backing.
    ///
    /// Coordinates alone can collide across owner pools. Peer caches may
    /// carry an owner's advertised prefix but are not that owner authority.
    pub fn validate_owner_incarnation(&self, expected: &str) -> Result<(), String> {
        if self.is_peer {
            return Err("peer pool is not an owner authority".into());
        }
        if self.name_prefix != expected {
            return Err("pool owner incarnation mismatch".into());
        }
        Ok(())
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
    /// Decision flow (pressure decisions live at the backing-creation seams,
    /// on validated total mapped bytes — never on the payload size):
    /// 1. size fits buddy AND existing segments have space → Buddy (reuse,
    ///    no pressure query, no new backing charge)
    /// 2. Else: buddy expansion if policy permits (seam checks pressure and
    ///    budget with the full segment's total mapped bytes)
    /// 3. Else: dedicated backing (seam checks pressure and budget with its
    ///    own, smaller, mapped size — a pressure-denied buddy segment does
    ///    not suppress a dedicated backing that fits)
    /// 4. Else: FileSpill, if the file budget admits the requested length
    pub fn alloc_handle(&mut self, size: usize) -> Result<MemHandle, String> {
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.buddy_block_limit();

        if max_buddy > 0 && size <= max_buddy {
            if let Some(a) = self.try_buddy_reuse(size) {
                return Ok(a.into_buddy_handle(size));
            }
            if self.gc_buddy() > 0 {
                if let Some(a) = self.try_buddy_reuse(size) {
                    return Ok(a.into_buddy_handle(size));
                }
            }
            if self.segments.len() < self.config.max_segments {
                if let Some(a) = self.expand_buddy(size) {
                    return Ok(a.into_buddy_handle(size));
                }
            }
        }

        // Buddy ineligible, pressure-denied, or creation-failed → dedicated
        // SHM, with file spill only as the final fallback.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16,
                len: size,
            }),
            Err(_) => self
                .alloc_file_spill(size)
                .map_err(BackingError::into_message),
        }
    }

    /// Allocate from Buddy or Dedicated SHM only — no FileSpill fallback.
    ///
    /// For callers that require a shared-memory handle rather than file storage.
    /// Returns `Err` if neither buddy nor dedicated has capacity: SHM pressure
    /// denials and budget rejections surface as this error so transport can
    /// choose its existing checked chunk fallback. A failed buddy expansion
    /// still tries dedicated storage.
    pub fn try_alloc_shm(&mut self, size: usize) -> Result<MemHandle, String> {
        if self.is_peer {
            return Err("cannot allocate from a peer pool".into());
        }
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.buddy_block_limit();

        if max_buddy > 0 && size <= max_buddy {
            if let Some(a) = self.try_buddy_reuse(size) {
                return Ok(a.into_buddy_handle(size));
            }
            if self.gc_buddy() > 0 {
                if let Some(a) = self.try_buddy_reuse(size) {
                    return Ok(a.into_buddy_handle(size));
                }
            }
            if self.segments.len() < self.config.max_segments {
                if let Some(a) = self.expand_buddy(size) {
                    return Ok(a.into_buddy_handle(size));
                }
            }
        }

        // Dedicated SHM only — never FileSpill.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16,
                len: size,
            }),
            Err(_) => Err(format!("try_alloc_shm: no SHM capacity for {size} bytes")),
        }
    }

    fn alloc_file_spill(&mut self, size: usize) -> Result<MemHandle, BackingError> {
        // Charge the requested file backing length before creating anything;
        // a rejection must not touch the filesystem. File backing is the
        // pressure escape hatch: it is budget-gated but never pressure-denied.
        let backing_bytes = u64::try_from(size).map_err(|_| {
            BackingError::Other("file spill size exceeds budget accounting range".into())
        })?;
        let guard = self
            .reserve_backing(BudgetKind::File, backing_bytes)
            .map_err(BackingError::Budget)?;
        let (mmap, path) = match spill::create_file_spill(size, &self.config.spill_dir) {
            Ok(created) => created,
            // Guard drops here, releasing the reservation on creation failure.
            Err(e) => return Err(BackingError::Other(format!("file spill failed: {e}"))),
        };
        self.counters.file_spill_allocs = self.counters.file_spill_allocs.saturating_add(1);
        Ok(MemHandle::FileSpill {
            mmap: mmap.with_budget_guard(guard),
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
            // The data region is the checked layout's power-of-two capacity; an
            // unsupported geometry yields 0 (buddy creation disabled) instead
            // of wrapping or panicking. Normal allocation may then try
            // dedicated/file backing; explicit prewarm reports the geometry error.
            return BuddyAllocator::checked_layout(
                self.config.segment_size,
                self.config.min_block_size,
            )
            .map(|layout| layout.data_size)
            .unwrap_or(0);
        }
        // Return the data size of existing segments.
        self.segments
            .first()
            .and_then(Option::as_ref)
            .map(|s| s.allocator().data_size())
            .unwrap_or(0)
    }

    /// Policy-gated buddy capacity: `0` when the buddy tiers are disabled, so
    /// every allocation API — including reuse of cached existing segments —
    /// skips straight to dedicated SHM (or the caller's file-spill fallback).
    fn buddy_block_limit(&self) -> usize {
        if !self.config.buddy_enabled {
            return 0;
        }
        self.max_buddy_block_size()
    }

    fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, BackingError> {
        // Layer 1: Reuse an already-mapped segment — permitted under
        // pressure, with no new backing charge and no OS query.
        if let Some(a) = self.try_buddy_reuse(size) {
            return Ok(a);
        }

        // Layer 1.5: GC before expansion — reclaim idle trailing segments,
        // then retry reuse.
        if self.gc_buddy() > 0 {
            if let Some(a) = self.try_buddy_reuse(size) {
                return Ok(a);
            }
        }

        // Layer 2: Create a new segment (the pressure/budget-enforced seam).
        // A creation failure (for example the SHM object cannot be mapped or
        // pressure denies the full segment) falls through to Layer 3
        // dedicated storage instead of failing the allocation outright.
        if self.segments.len() < self.config.max_segments {
            if let Some(a) = self.expand_buddy(size) {
                return Ok(a);
            }
        }

        // Layer 3: Dedicated segment fallback (its own, smaller, seam checks).
        self.alloc_dedicated(size)
    }

    /// Try to serve `size` from an already-mapped buddy segment.
    ///
    /// Reuse is always permitted — including under OS-memory pressure —
    /// because it maps nothing and charges nothing.
    fn try_buddy_reuse(&mut self, size: usize) -> Option<PoolAllocation> {
        for (idx, seg) in self.segments.iter().enumerate() {
            let Some(seg) = seg else {
                continue;
            };
            // Skip segments with insufficient free space to avoid unnecessary
            // spinlock acquisition.
            if (seg.allocator().free_bytes() as usize) < size {
                continue;
            }
            if let Some(a) = seg.allocator().alloc(size) {
                // Mark segment as active (not idle).
                if idx < self.idle_since.len() {
                    self.idle_since[idx] = None;
                }
                self.counters.buddy_reused_allocs =
                    self.counters.buddy_reused_allocs.saturating_add(1);
                return Some(PoolAllocation {
                    seg_idx: idx as u32,
                    generation: self.generations[idx],
                    offset: a.offset,
                    actual_size: a.actual_size,
                    level: a.level,
                    is_dedicated: false,
                });
            }
        }
        None
    }

    /// Create one new buddy segment and allocate `size` from it.
    ///
    /// Returns `None` when creation fails for any reason (geometry,
    /// pressure, budget, or OS); callers fall through to the next tier.
    fn expand_buddy(&mut self, size: usize) -> Option<PoolAllocation> {
        let seg = self.create_segment().ok()?;
        let idx = self.segments.len();
        self.segments.push(Some(seg));
        self.idle_since.push(None);
        if let Some(a) = self
            .segment(idx)
            .expect("owner segment slot")
            .allocator()
            .alloc(size)
        {
            self.counters.buddy_expanded_allocs =
                self.counters.buddy_expanded_allocs.saturating_add(1);
            return Some(PoolAllocation {
                seg_idx: idx as u32,
                generation: self.generations[idx],
                offset: a.offset,
                actual_size: a.actual_size,
                level: a.level,
                is_dedicated: false,
            });
        }
        None
    }

    fn alloc_dedicated(&mut self, size: usize) -> Result<PoolAllocation, BackingError> {
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
            return Err(BackingError::Other(format!(
                "dedicated segment limit reached ({} active)",
                active_dedicated
            )));
        }

        let idx = self.next_dedicated_idx;
        if idx > u16::MAX as u32 {
            return Err(BackingError::Other(
                "dedicated segment index exhausted".into(),
            ));
        }

        // Checked geometry and wire-representability before any charge or
        // mapping: the charged bytes always equal the mapped bytes.
        let backing = DedicatedSegment::required_shm_size(size).ok_or_else(|| {
            BackingError::Other(format!(
                "dedicated segment size {size} exceeds the addressable backing range"
            ))
        })?;
        if backing > u32::MAX as usize {
            return Err(BackingError::Other(format!(
                "dedicated segment size {backing} exceeds 4GB limit"
            )));
        }
        let backing_bytes = u64::try_from(backing).expect("u32 range fits u64");
        // Creation seam: judge this backing's own validated mapped size
        // against OS-memory pressure before the budget reservation. A
        // pressure-denied buddy segment does not suppress this smaller
        // backing — the decision is per candidate, never a pool-global bit.
        if let PressureDecision::Deny { reason } = self
            .pressure
            .evaluate(backing_bytes, "dedicated shared-memory backing")
        {
            return Err(BackingError::Pressure(reason));
        }
        let guard = self
            .reserve_backing(BudgetKind::Shm, backing_bytes)
            .map_err(BackingError::Budget)?;

        let name = Self::dedicated_segment_name(&self.name_prefix, idx);
        let seg = match DedicatedSegment::create(&name, size) {
            Ok(seg) => seg.with_budget_guard(guard),
            // Guard drops here, releasing the reservation on creation failure.
            Err(e) => return Err(BackingError::Other(e)),
        };
        self.next_dedicated_idx = self
            .next_dedicated_idx
            .checked_add(1)
            .expect("dedicated segment index overflow");
        self.counters.dedicated_allocs = self.counters.dedicated_allocs.saturating_add(1);
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

    /// Reserve owner-creation backing bytes against this pool's budget.
    ///
    /// Owner pools always carry a budget context (shared or private); peer
    /// pools carry none and never reach a creation seam. The reservation
    /// happens before any mapping so a rejection never creates a backing.
    fn reserve_backing(
        &self,
        kind: BudgetKind,
        bytes: u64,
    ) -> Result<BudgetReservation, BudgetError> {
        let budget = self
            .budget
            .as_ref()
            .expect("owner pools carry a budget; peer pools never create backings");
        budget.reserve(kind, bytes)
    }

    fn create_segment(&mut self) -> Result<BuddySegment, BackingError> {
        if self.is_peer {
            return Err(BackingError::Other("cannot create peer backing".into()));
        }
        let idx = self.segments.len();
        let generation = self
            .generations
            .get(idx)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| BackingError::Other("buddy backing generation exhausted".into()))?;
        let name = Self::buddy_segment_name(&self.name_prefix, idx as u32, generation);
        // Preflight the geometry with the single checked-layout authority
        // before reserving or mapping: unsupported layouts (u32 data bound,
        // header/bitmap overflow, addressable-span overflow) must reject
        // here rather than map first and panic inside BuddyAllocator::init.
        let layout = BuddyAllocator::checked_layout(
            self.config.segment_size,
            self.config.min_block_size,
        )
        .ok_or_else(|| {
            BackingError::Other(format!(
                "buddy backing geometry unsupported: segment_size {} with min_block {} exceeds the u32 data capacity or the addressable mapping span",
                self.config.segment_size, self.config.min_block_size
            ))
        })?;
        // Charge the exact total BuddySegment::create maps — header and
        // bitmaps included, from the same validated layout — so reusing
        // blocks inside an already-charged segment never charges again.
        let backing_bytes = u64::try_from(layout.total_size).map_err(|_| {
            BackingError::Other("buddy backing size exceeds budget accounting range".into())
        })?;
        // Creation seam: judge the full segment's validated total mapped
        // bytes — never the payload size — against OS-memory pressure before
        // the budget reservation. Reuse of existing segments never reaches
        // this seam. A pressure denial here falls through to a smaller
        // dedicated backing when one fits.
        if let PressureDecision::Deny { reason } = self
            .pressure
            .evaluate(backing_bytes, "buddy segment backing")
        {
            return Err(BackingError::Pressure(reason));
        }
        let guard = self
            .reserve_backing(BudgetKind::Shm, backing_bytes)
            .map_err(BackingError::Budget)?;
        let segment =
            match BuddySegment::create(&name, self.config.segment_size, self.config.min_block_size)
            {
                Ok(segment) => segment.with_budget_guard(guard),
                // Guard drops here, releasing the reservation on creation failure.
                Err(e) => return Err(BackingError::Other(e)),
            };
        if self.generations.len() <= idx {
            self.generations.resize(idx + 1, 0);
        }
        self.generations[idx] = generation;
        Ok(segment)
    }

    /// Install deterministic availability/clock sources for pressure tests.
    ///
    /// Test-only: shipped builds always sample the raw OS availability query
    /// and the real clock. Installing hooks drops any cached observation.
    #[cfg(test)]
    pub(crate) fn install_pressure_hooks(&mut self, hooks: crate::pressure::PressureHooks) {
        self.pressure.install_hooks(hooks);
    }
}

impl PoolAllocation {
    fn into_buddy_handle(self, len: usize) -> MemHandle {
        debug_assert!(!self.is_dedicated);
        MemHandle::Buddy {
            seg_idx: self.seg_idx as u16,
            generation: self.generation,
            offset: self.offset,
            allocation_size: self.actual_size,
            len,
        }
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
    fn owner_incarnation_validation_is_non_destructive() {
        // Lazy pools: this check requires no SHM mapping or payload access.
        let owner = MemPool::new_with_prefix(small_config(), "same-label".into());
        let expected = owner.prefix().to_owned();
        let replacement = MemPool::new_with_prefix(small_config(), "same-label".into());
        let peer = MemPool::open_peer(small_config(), expected.clone());
        assert!(owner.validate_owner_incarnation(&expected).is_ok());
        assert!(
            replacement
                .validate_owner_incarnation(&expected)
                .unwrap_err()
                .contains("incarnation")
        );
        assert_eq!(peer.prefix(), expected);
        assert!(
            peer.validate_owner_incarnation(&expected)
                .unwrap_err()
                .contains("authority")
        );
        // Moving/restoring the actual owner preserves its identity.
        let restored = owner;
        assert!(restored.validate_owner_incarnation(&expected).is_ok());
        for pool in [&restored, &replacement, &peer] {
            assert_eq!(pool.stats().alloc_count, 0);
            assert_eq!(pool.stats().total_segments, 0);
        }
        let snapshot = restored.budget().unwrap().snapshot();
        assert_eq!(snapshot.shm.used_bytes, 0);
        assert_eq!(snapshot.file.used_bytes, 0);
        assert_eq!(snapshot.reassembly.used_bytes, 0);
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
        assert!(stats.buddy_data_bytes > 0);
        assert_eq!(
            stats.buddy_idle_bytes + stats.buddy_occupied_bytes,
            stats.buddy_data_bytes
        );
        assert!(stats.alloc_count >= 1);
        assert!(stats.utilization_ratio > 0.0);
        pool.free(&a).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.buddy_occupied_bytes, 0);
        assert_eq!(stats.utilization_ratio, 0.0);
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
        // Buddy generation exhaustion is a buddy-tier failure: the allocation
        // must fall through to dedicated storage rather than fail outright.
        let exhausted = owner.alloc(size).unwrap();
        assert!(exhausted.is_dedicated);
        owner.free(&exhausted).unwrap();
        owner.gc_dedicated();
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

    #[test]
    fn backing_names_preserve_sha256_and_base64_golden_values() {
        let prefix = "label_1234567800112233445566778899aabbccddeeff";
        assert_eq!(
            MemPool::buddy_segment_name(prefix, 2, 3),
            "/c2eFY0EgcB2GSVR4WiMn8x6vp-SH"
        );
        assert_eq!(
            MemPool::dedicated_segment_name(prefix, 2),
            "/c2eFY0Eg2SpSqOYgE0GThVNu4U5i"
        );
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
            ..PoolConfig::default()
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

    // ── Buddy policy enforcement (Phase 1A) ─────────────────────────────

    fn buddy_disabled_config() -> PoolConfig {
        PoolConfig {
            max_dedicated_segments: 4,
            spill_dir: std::env::temp_dir().join("c2_buddy_disabled_test"),
            buddy_enabled: false,
            min_retained_segments: 0,
            ..test_config()
        }
    }

    fn collision_test_config() -> PoolConfig {
        PoolConfig {
            max_segments: 2,
            max_dedicated_segments: 4,
            spill_dir: std::env::temp_dir().join("c2_collision_test"),
            ..test_config()
        }
    }

    #[test]
    fn disabled_buddy_skips_buddy_tiers_in_every_allocation_api() {
        let mut pool = test_pool(buddy_disabled_config());

        // Small request that would normally land in a buddy block.
        let a = pool.alloc(4096).unwrap();
        assert!(a.is_dedicated);

        let h = pool.alloc_handle(4096).unwrap();
        assert!(h.is_dedicated());

        let s = pool.try_alloc_shm(4096).unwrap();
        assert!(s.is_dedicated());

        // No buddy segment may ever be mapped, including through prewarm.
        assert_eq!(pool.segment_count(), 0);
        assert!(pool.ensure_ready().is_ok());
        assert_eq!(pool.segment_count(), 0);
        assert!(
            pool.ensure_buddy_segments(1)
                .unwrap_err()
                .contains("buddy pool is disabled")
        );
        assert_eq!(pool.segment_count(), 0);

        pool.free(&a).unwrap();
        pool.release_handle(h);
        pool.release_handle(s);
    }

    #[test]
    fn disabled_buddy_keeps_file_spill_fallback() {
        let dir = std::env::temp_dir().join("c2_disabled_spill_test");
        let config = PoolConfig {
            spill_threshold: 0.0, // force spill decisions
            spill_dir: dir.clone(),
            ..buddy_disabled_config()
        };
        let mut pool = test_pool(config);
        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        assert_eq!(pool.segment_count(), 0);
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buddy_backing_name_collision_falls_through_to_dedicated() {
        let mut pool = test_pool(collision_test_config());

        // Pre-create the exact backing name segment 0 / generation 1 will use,
        // so the pool's own create_segment fails with EEXIST. The allocation
        // must still succeed through the dedicated tier, not fail or spill.
        let name = MemPool::buddy_segment_name(pool.prefix(), 0, 1);
        let squatter = BuddySegment::create(&name, 64 * 1024, 4096).unwrap();

        let a = pool.alloc(4096).unwrap();
        assert!(a.is_dedicated);

        let h = pool.alloc_handle(4096).unwrap();
        assert!(h.is_dedicated());

        let s = pool.try_alloc_shm(4096).unwrap();
        assert!(s.is_dedicated());

        drop(squatter);
        pool.free(&a).unwrap();
        pool.release_handle(h);
        pool.release_handle(s);
    }

    #[test]
    fn gc_buddy_retires_to_zero_when_min_retained_is_zero() {
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_retire_zero_test"),
            buddy_enabled: true,
            min_retained_segments: 0,
        };
        let mut pool = test_pool(config);

        let a = pool.alloc(4096).unwrap();
        assert!(!a.is_dedicated);
        let old_generation = a.generation;
        let old_name = pool.segment_name(0).unwrap().to_owned();
        pool.free(&a).unwrap();

        // Idle decay 0 → GC may retire the last idle segment.
        assert_eq!(pool.gc_buddy(), 1);
        assert_eq!(pool.segment_count(), 0);

        // Re-creation reuses the slot with a fresh generation; stale
        // coordinates naming the retired backing must be rejected.
        let b = pool.alloc(4096).unwrap();
        assert_eq!(b.seg_idx, a.seg_idx);
        assert!(b.generation > old_generation);
        assert_ne!(pool.segment_name(0).unwrap(), old_name);
        assert!(pool.data_ptr(&a).is_err());
        assert!(pool.free(&a).is_err());

        pool.free(&b).unwrap();
    }

    #[test]
    fn gc_buddy_retires_only_down_to_configured_min() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_retire_min_test"),
            buddy_enabled: true,
            min_retained_segments: 2,
        };
        let mut pool = test_pool(config);

        let mut allocs = Vec::new();
        for _ in 0..6 {
            allocs.push(pool.alloc(16 * 1024).unwrap());
        }
        assert!(pool.segment_count() >= 3);
        for a in &allocs {
            pool.free(a).unwrap();
        }

        assert!(pool.gc_buddy() > 0);
        assert_eq!(pool.segment_count(), 2);
        // A second sweep cannot go below the floor.
        assert_eq!(pool.gc_buddy(), 0);
        assert_eq!(pool.segment_count(), 2);
    }

    #[test]
    fn live_allocations_survive_idle_gc() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_live_gc_test"),
            buddy_enabled: true,
            min_retained_segments: 0,
        };
        let mut pool = test_pool(config);

        let held = pool.alloc(32 * 1024).unwrap();
        let filler = pool.alloc(32 * 1024).unwrap();
        assert!(pool.segment_count() >= 2);

        // Free only the trailing segment's block; the first stays live.
        pool.free(&filler).unwrap();
        assert!(pool.gc_buddy() >= 1);
        // Segment 0 still backs a live allocation.
        assert!(pool.data_ptr(&held).is_ok());
        unsafe {
            *pool.data_ptr(&held).unwrap() = 0x7A;
        }

        // A fully-live segment is never retired even with min_retained 0.
        assert_eq!(pool.gc_buddy(), 0);
        assert_eq!(pool.segment_count(), 1);
        assert_eq!(unsafe { *pool.data_ptr(&held).unwrap() }, 0x7A);

        pool.free(&held).unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        assert_eq!(pool.segment_count(), 0);
    }

    #[test]
    fn validate_rejects_min_retained_above_max_segments() {
        let config = PoolConfig {
            max_segments: 2,
            min_retained_segments: 3,
            ..test_config()
        };
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("min_retained_segments")
        );
    }

    #[test]
    fn dedicated_size_representability_is_checked_before_mapping() {
        let mut pool = test_pool(test_config());
        // A payload whose page-aligned region (data + 64B header) exceeds
        // u32::MAX must be rejected before any mapping is created.
        let oversized = u32::MAX as usize - 4096;
        let err = pool.alloc(oversized).unwrap_err();
        assert!(err.contains("4GB limit"), "got: {err}");
        // No dedicated entry may exist after the rejection.
        assert_eq!(pool.stats().dedicated_segments, 0);
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
            ..PoolConfig::default()
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
            assert_eq!(pool.stats().buddy_idle_bytes, capacity);
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

    static BUDGET_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix() -> String {
        let id = BUDGET_TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        format!("/cc3bt{:04x}{:04x}", std::process::id() as u16, id)
    }

    /// Tiny geometry so every charge is exact and observable:
    /// buddy data region 8192 (one 8192 block or two 4096 blocks),
    /// dedicated backing for 4096 payload bytes is page_align(4096+64)=8192.
    fn budget_config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 60.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_budget_spill"),
            ..PoolConfig::default()
        }
    }

    fn buddy_backing(config: &PoolConfig) -> u64 {
        BuddyAllocator::checked_layout(config.segment_size, config.min_block_size)
            .expect("test config geometry is supported")
            .total_size as u64
    }

    fn budget_pool(config: PoolConfig, budget: MemoryBudget) -> MemPool {
        MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget)
    }

    #[test]
    fn zero_budget_rejects_before_any_mapping() {
        let config = budget_config();
        let mut pool = budget_pool(
            config.clone(),
            MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::zeroed()),
        );

        // Prewarm is a creation entry point and must reject before mapping.
        assert!(
            pool.ensure_ready()
                .unwrap_err()
                .contains("memory budget cell 'shm'")
        );
        assert_eq!(pool.segment_count(), 0);
        let snap = pool.budget().unwrap().snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.shm.rejected_allocations, 1);
        assert_eq!(snap.shm.rejected_bytes, buddy_backing(&config));

        // alloc falls buddy → dedicated; both charge the same zeroed cell.
        assert!(
            pool.alloc(4096)
                .unwrap_err()
                .contains("memory budget cell 'shm'")
        );
        assert_eq!(pool.segment_count(), 0);
        assert!(pool.dedicated.is_empty());
        assert_eq!(
            pool.budget().unwrap().snapshot().shm.rejected_allocations,
            3
        );

        // The zeroed file cell rejects before touching the filesystem.
        let mut spill_config = config.clone();
        spill_config.spill_threshold = 0.0; // force the file path
        let spill_dir = std::env::temp_dir().join(format!("c2_budget_zero_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&spill_dir);
        spill_config.spill_dir = spill_dir.clone();
        let mut spiller = budget_pool(
            spill_config,
            MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::zeroed()),
        );
        assert!(
            spiller
                .alloc_handle(4096)
                .unwrap_err()
                .contains("memory budget cell 'file'")
        );
        assert!(
            !spill_dir.exists(),
            "rejected file reservation must not create the spill directory"
        );
        let snap = spiller.budget().unwrap().snapshot();
        assert_eq!(snap.file.used_bytes, 0);
        assert_eq!(snap.file.rejected_allocations, 1);
        assert_eq!(snap.file.rejected_bytes, 4096);
    }

    #[test]
    fn charges_match_exact_backing_geometry() {
        let config = budget_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config.clone(), budget.clone());

        // Buddy: header + bitmap overhead included via checked_layout.
        let block = pool.alloc(4096).unwrap();
        assert!(!block.is_dedicated);
        assert_eq!(budget.snapshot().shm.used_bytes, buddy_backing(&config));

        // Dedicated: page-aligned header + payload, exactly the mapped size.
        let dedicated = pool.alloc(20000).unwrap();
        assert!(dedicated.is_dedicated);
        let dedicated_backing = DedicatedSegment::required_shm_size(20000).unwrap() as u64;
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            buddy_backing(&config) + dedicated_backing
        );

        // File: the requested backing length.
        let mut spill_config = config.clone();
        spill_config.spill_threshold = 0.0;
        let mut spiller = budget_pool(spill_config, budget.clone());
        let handle = spiller.alloc_handle(5000).unwrap();
        assert!(handle.is_file_spill());
        assert_eq!(budget.snapshot().file.used_bytes, 5000);
        drop(handle);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
    }

    #[test]
    fn reused_buddy_blocks_charge_no_new_backing() {
        let config = budget_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config.clone(), budget.clone());
        let backing = buddy_backing(&config);

        // Two 4096 blocks share one charged 8192-byte buddy segment.
        let a = pool.alloc(4096).unwrap();
        let b = pool.alloc(4096).unwrap();
        assert_eq!(pool.segment_count(), 1);
        assert_eq!(budget.snapshot().shm.used_bytes, backing);

        // Freeing then reusing blocks inside the existing segment adds no
        // charge; only a second mapped segment would.
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
        let reused = pool.alloc(4096).unwrap();
        assert_eq!(pool.segment_count(), 1);
        assert_eq!(budget.snapshot().shm.used_bytes, backing);
        assert_eq!(budget.snapshot().shm.peak_bytes, backing);
        pool.free(&reused).unwrap();
    }

    #[test]
    fn shared_budget_spans_owner_pools_and_private_budgets_do_not() {
        let config = budget_config();
        let backing = buddy_backing(&config);

        // One shared budget across two owner pools.
        let shared = MemoryBudget::new(backing * 2, u64::MAX, u64::MAX);
        let mut pool_a = budget_pool(config.clone(), shared.clone());
        let mut pool_b = budget_pool(config.clone(), shared.clone());

        let a = pool_a.alloc(8192).unwrap();
        assert_eq!(shared.snapshot().shm.used_bytes, backing);
        let b = pool_b.alloc(8192).unwrap();
        assert_eq!(shared.snapshot().shm.used_bytes, backing * 2);

        // Both cells exhausted: the third expansion is rejected in both pools.
        assert!(
            pool_a
                .alloc(8192)
                .unwrap_err()
                .contains("memory budget cell 'shm'")
        );
        assert!(
            pool_b
                .alloc(8192)
                .unwrap_err()
                .contains("memory budget cell 'shm'")
        );
        assert_eq!(shared.snapshot().shm.used_bytes, backing * 2);
        pool_a.free(&a).unwrap();
        pool_b.free(&b).unwrap();

        // Existing owner constructors carry distinct private canonical budgets.
        let mut private_a = MemPool::new_with_prefix(config.clone(), unique_prefix());
        let mut private_b = MemPool::new_with_prefix(config.clone(), unique_prefix());
        let a = private_a.alloc(8192).unwrap();
        assert_eq!(
            private_a.budget().unwrap().snapshot().shm.used_bytes,
            backing
        );
        assert_eq!(private_b.budget().unwrap().snapshot().shm.used_bytes, 0);
        let b = private_b.alloc(8192).unwrap();
        assert_eq!(
            private_a.budget().unwrap().snapshot().shm.used_bytes,
            backing
        );
        assert_eq!(
            private_b.budget().unwrap().snapshot().shm.used_bytes,
            backing
        );
        private_a.free(&a).unwrap();
        private_b.free(&b).unwrap();

        // Peer pools never charge owner-creation budget.
        let peer = MemPool::open_peer(config, unique_prefix());
        assert!(peer.budget().is_none());
    }

    #[test]
    fn owner_constructors_carry_canonical_private_limits() {
        let config = budget_config();
        for pool in [
            MemPool::new(config.clone()),
            MemPool::new_with_prefix(config.clone(), unique_prefix()),
        ] {
            let budget = pool.budget().expect("owner pools carry a budget");
            let snap = budget.snapshot();
            assert_eq!(snap.shm.limit_bytes, 8 * 1024 * 1024 * 1024);
            assert_eq!(snap.file.limit_bytes, 16 * 1024 * 1024 * 1024);
            assert_eq!(snap.reassembly.limit_bytes, 8 * 1024 * 1024 * 1024);
        }
    }

    #[test]
    fn dedicated_pending_gc_stays_charged_until_reclaim() {
        let config = budget_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config, budget.clone());

        // 20000 bytes exceeds the 8192 buddy ceiling: dedicated-only charge.
        let alloc = pool.alloc(20000).unwrap();
        assert!(alloc.is_dedicated);
        let dedicated_backing = DedicatedSegment::required_shm_size(20000).unwrap() as u64;
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);

        // Creator free marks freed_at but waits for peer read_done / GC.
        pool.free(&alloc).unwrap();
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            dedicated_backing,
            "freed-but-pending-GC dedicated backing must stay charged"
        );
        pool.gc_dedicated();
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);

        // Simulate the peer signalling read_done, then GC returns the charge.
        pool.dedicated
            .get(&alloc.seg_idx)
            .unwrap()
            .segment
            .mark_read_done();
        pool.gc_dedicated();
        assert!(pool.dedicated.get(&alloc.seg_idx).is_none());
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
    }

    #[test]
    fn dedicated_awaiting_retirement_reports_physical_pending_free_state() {
        let config = budget_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config, budget);

        let alloc = pool.alloc(20000).unwrap();
        assert!(alloc.is_dedicated);
        assert!(
            !pool.dedicated_awaiting_retirement(&alloc),
            "a live (unfreed) dedicated allocation is not awaiting retirement"
        );

        // Freed but unread: awaiting retirement until read_done or the
        // crash-timeout policy reclaims the entry.
        pool.free(&alloc).unwrap();
        assert!(pool.dedicated_awaiting_retirement(&alloc));
        pool.gc_dedicated();
        assert!(
            pool.dedicated_awaiting_retirement(&alloc),
            "a peer that has not signalled read_done keeps the entry pending"
        );

        pool.dedicated
            .get(&alloc.seg_idx)
            .unwrap()
            .segment
            .mark_read_done();
        pool.gc_dedicated();
        assert!(
            !pool.dedicated_awaiting_retirement(&alloc),
            "a retired entry no longer awaits retirement"
        );

        // A buddy allocation never reports awaiting retirement.
        let buddy = pool.alloc(4096).unwrap();
        assert!(!buddy.is_dedicated);
        pool.free(&buddy).unwrap();
        assert!(!pool.dedicated_awaiting_retirement(&buddy));
    }

    #[test]
    fn file_creation_failure_releases_reservation() {
        let config = budget_config();
        let not_a_dir =
            std::env::temp_dir().join(format!("c2_budget_not_a_dir_{}", std::process::id()));
        let _ = std::fs::remove_file(&not_a_dir);
        std::fs::write(&not_a_dir, b"occupies the path").unwrap();

        let mut spill_config = config;
        spill_config.spill_threshold = 0.0; // force the file path
        spill_config.spill_dir = not_a_dir.clone();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(spill_config, budget.clone());

        assert!(
            pool.alloc_handle(4096)
                .unwrap_err()
                .contains("file spill failed")
        );
        // The reservation was taken and then returned: used is zero, but the
        // persisted high-water mark proves the charge happened and rolled back.
        let snap = budget.snapshot();
        assert_eq!(snap.file.used_bytes, 0);
        assert_eq!(snap.file.peak_bytes, 4096);
        assert_eq!(snap.file.rejected_allocations, 0);

        let _ = std::fs::remove_file(&not_a_dir);
    }

    #[test]
    fn file_guard_survives_move_trim_and_pool_drop() {
        let dir = std::env::temp_dir().join(format!("c2_budget_hold_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = budget_config();
        config.spill_threshold = 0.0; // force the file path
        config.spill_dir = dir.clone();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config, budget.clone());

        let mut handle = pool.alloc_handle(8192).unwrap();
        assert!(handle.is_file_spill());
        assert_eq!(budget.snapshot().file.used_bytes, 8192);
        pool.handle_slice_mut(&mut handle)[..4].copy_from_slice(b"hold");

        // Logical trim must not release the mapping's charge.
        handle.set_len(4);
        assert_eq!(budget.snapshot().file.used_bytes, 8192);

        // Move the handle, then drop the pool entirely.
        let moved = handle;
        drop(pool);
        assert_eq!(
            budget.snapshot().file.used_bytes,
            8192,
            "file charge must persist while the mapping is live"
        );

        // The mapping is still readable after the pool is gone.
        match &moved {
            MemHandle::FileSpill { mmap, len, .. } => {
                assert_eq!(len, &4);
                assert_eq!(&mmap[..4], b"hold");
            }
            other => panic!("unexpected handle {other:?}"),
        }
        drop(moved);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_eligible_tiers_exhausted_returns_clear_capacity_error() {
        let dir = std::env::temp_dir().join(format!("c2_budget_exhaust_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = budget_config();
        config.spill_dir = dir.clone();
        // Tiny but nonzero budgets: smaller than any eligible backing.
        let budget = MemoryBudget::new(4095, 4095, 4095);
        let mut pool = budget_pool(config.clone(), budget.clone());

        let error = pool.alloc_handle(4096).unwrap_err();
        assert!(
            error.contains("memory budget cell 'file'"),
            "terminal capacity error should name the last exhausted cell: {error}"
        );
        // Every tier was attempted and none created a backing.
        assert_eq!(pool.segment_count(), 0);
        assert!(pool.dedicated.is_empty());
        assert!(
            !dir.exists(),
            "no spill file may be created by a rejected tier"
        );
        let snap = budget.snapshot();
        assert_eq!(snap.shm.rejected_allocations, 2); // buddy expansion + dedicated
        assert_eq!(snap.file.rejected_allocations, 1);
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.file.used_bytes, 0);

        // SHM-only allocation reports a capacity error for transport's
        // existing chunked fallback; it never silently spills.
        assert!(
            pool.try_alloc_shm(4096)
                .unwrap_err()
                .contains("no SHM capacity")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buddy_budget_short_but_dedicated_fits() {
        let config = budget_config();
        let dedicated_backing = DedicatedSegment::required_shm_size(4096).unwrap() as u64;
        // Large enough for a dedicated 4096-byte payload backing, too small
        // for a full buddy segment.
        assert!(dedicated_backing < buddy_backing(&config));
        let budget = MemoryBudget::new(dedicated_backing, u64::MAX, u64::MAX);
        let mut pool = budget_pool(config, budget.clone());

        let alloc = pool.alloc(4096).unwrap();
        assert!(
            alloc.is_dedicated,
            "rejected buddy expansion must fall through to a smaller dedicated backing"
        );
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);
        assert_eq!(pool.segment_count(), 0);
        pool.free(&alloc).unwrap();
    }

    #[test]
    fn shm_exhaustion_falls_to_file_when_allowed() {
        let dir = std::env::temp_dir().join(format!("c2_budget_fall_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = budget_config();
        config.spill_dir = dir.clone();
        // SHM fully disabled by zero limit; file allowed.
        let budget = MemoryBudget::new(0, 16 * 1024 * 1024 * 1024, 0);
        let mut pool = budget_pool(config, budget.clone());

        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.shm.rejected_allocations, 2); // buddy + dedicated
        assert_eq!(snap.file.used_bytes, 4096);
        assert_eq!(snap.file.rejected_allocations, 0);
        drop(handle);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn teardown_returns_all_charges_and_live_backings_are_not_retired_early() {
        let config = budget_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = budget_pool(config.clone(), budget.clone());
        let backing = buddy_backing(&config);

        // Live buddy block: GC cannot retire it and the charge stays.
        let a = pool.alloc(8192).unwrap();
        let b = pool.alloc(8192).unwrap();
        assert_eq!(pool.segment_count(), 2);
        assert_eq!(budget.snapshot().shm.used_bytes, backing * 2);
        assert_eq!(pool.gc_buddy(), 0);
        assert_eq!(budget.snapshot().shm.used_bytes, backing * 2);

        // Free + GC reclaims the trailing idle segment and returns its charge.
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        assert_eq!(budget.snapshot().shm.used_bytes, backing);

        // Destroy returns even the deliberately retained last segment.
        pool.destroy();
        assert_eq!(budget.snapshot().shm.used_bytes, 0);

        // Dedicated pending-GC then reclaimed, then a file handle outliving
        // the pool itself: the charge persists until the mapping drops.
        let d = pool.alloc(20000).unwrap();
        let dedicated_backing = DedicatedSegment::required_shm_size(20000).unwrap() as u64;
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);
        pool.free(&d).unwrap();
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);
        pool.dedicated
            .get(&d.seg_idx)
            .unwrap()
            .segment
            .mark_read_done();
        pool.gc_dedicated();
        assert_eq!(budget.snapshot().shm.used_bytes, 0);

        let mut spill_config = config;
        spill_config.spill_threshold = 0.0;
        spill_config.spill_dir =
            std::env::temp_dir().join(format!("c2_budget_teardown_{}", std::process::id()));
        let mut spiller = budget_pool(spill_config, budget.clone());
        let handle = spiller.alloc_handle(4096).unwrap();
        drop(spiller);
        assert_eq!(budget.snapshot().file.used_bytes, 4096);
        drop(handle);
        assert_eq!(budget.snapshot().file.used_bytes, 0);

        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.file.used_bytes, 0);
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert!(
            snap.shm.peak_bytes > 0,
            "peaks persist; counters are never reset"
        );
    }
}

#[cfg(test)]
mod geometry_and_guard_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

    static GEOMETRY_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix() -> String {
        let id = GEOMETRY_TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        format!("/cc3bg{:04x}{:04x}", std::process::id() as u16, id)
    }

    fn geometry_config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 60.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_geometry_spill"),
            ..PoolConfig::default()
        }
    }

    #[test]
    fn validate_config_rejects_min_block_doubling_overflow() {
        let mut config = geometry_config();
        // A power-of-two min block whose doubling wraps usize would previously
        // compare segment_size against a small wrapped bound.
        config.min_block_size = usize::MAX / 2 + 1;
        config.segment_size = usize::MAX;
        let error = MemPool::validate_config(&config).unwrap_err();
        assert!(
            error.contains("doubling overflows"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unsupported_buddy_geometry_rejects_before_any_mapping() {
        // validate_config deliberately accepts these (only doubling and shape
        // are checked there); the creation seam must preflight the geometry,
        // reject before the budget charge, and create no named region.
        let mut sizes = vec![usize::MAX];
        if usize::BITS >= 64 {
            sizes.extend([(1usize << 63) + 1, (1usize << 32) + 12345]);
        }
        for segment_size in sizes {
            let mut config = geometry_config();
            config.segment_size = segment_size;
            MemPool::validate_config(&config).expect("validation accepts; the seam preflights");

            let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
            let mut pool =
                MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone());
            let error = pool.ensure_ready().unwrap_err();
            assert!(
                error.contains("buddy backing geometry unsupported"),
                "segment_size {segment_size}: {error}"
            );
            assert_eq!(pool.segment_count(), 0);

            // No charge was taken and no budget rejection was recorded: the
            // geometry preflight rejects before the reservation.
            let snap = budget.snapshot();
            assert_eq!(snap.shm.used_bytes, 0, "segment_size {segment_size}");
            assert_eq!(
                snap.shm.rejected_allocations, 0,
                "segment_size {segment_size}"
            );

            // No named region exists: derive the name segment 0 would have
            // used and prove nothing is registered under it.
            let name = MemPool::buddy_segment_name(pool.prefix(), 0, 1);
            assert!(
                BuddySegment::open(&name, 4096).is_err(),
                "no SHM region may exist for {name} (segment_size {segment_size})"
            );
        }
    }

    #[test]
    #[should_panic(expected = "at most one owner-creation reservation")]
    fn budget_guard_attachment_is_single_shot() {
        let name = format!("/c2bgs_{}", std::process::id());
        let segment = BuddySegment::create(&name, 8192, 4096).unwrap();
        let budget = MemoryBudget::new(u64::MAX, u64::MAX, u64::MAX);
        let first = budget.reserve(BudgetKind::Shm, 1024).unwrap();
        let segment = segment.with_budget_guard(first);
        assert_eq!(budget.snapshot().shm.used_bytes, 1024);
        // A second attachment must not be able to replace and silently drop
        // the live charge.
        let second = budget.reserve(BudgetKind::Shm, 512).unwrap();
        let _ = segment.with_budget_guard(second);
    }

    #[test]
    fn owner_backing_names_disappear_after_pool_teardown() {
        // OS-level probe on this platform: creator teardown must actually
        // remove the named POSIX SHM objects, not just the pool's containers.
        let config = geometry_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool = MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone());

        let block = pool.alloc(4096).unwrap();
        let dedicated = pool.alloc(20000).unwrap();
        assert!(!block.is_dedicated && dedicated.is_dedicated);
        let buddy_name = pool
            .segment_name(block.seg_idx as usize)
            .unwrap()
            .to_owned();
        let dedicated_name = pool.dedicated_name(dedicated.seg_idx).unwrap().to_owned();

        drop(pool);

        // Neither name resolves after teardown: the mappings were unmapped
        // and the creator-owned shm objects unlinked.
        assert!(
            BuddySegment::open(&buddy_name, 4096).is_err(),
            "buddy region {buddy_name} must not survive owner teardown"
        );
        assert!(
            DedicatedSegment::open(&dedicated_name, 20000).is_err(),
            "dedicated region {dedicated_name} must not survive owner teardown"
        );
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
    }
}

#[cfg(test)]
mod pressure_seam_tests {
    use super::*;
    use crate::pressure::PressureHooks;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtOrd};

    static PRESSURE_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix() -> String {
        let id = PRESSURE_TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        format!("/cc3bp{:04x}{:04x}", std::process::id() as u16, id)
    }

    /// Tiny geometry with the heuristic active: buddy data region 8192,
    /// buddy total backing larger, dedicated backing for a 4096-byte payload
    /// is page_align(4096+64) = 8192.
    fn pressure_config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 60.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 0.5,
            spill_dir: std::env::temp_dir().join("c2_pressure_seam"),
            ..PoolConfig::default()
        }
    }

    fn buddy_total(config: &PoolConfig) -> u64 {
        BuddyAllocator::checked_layout(config.segment_size, config.min_block_size)
            .expect("test config geometry is supported")
            .total_size as u64
    }

    fn dedicated_backing(payload: usize) -> u64 {
        DedicatedSegment::required_shm_size(payload).unwrap() as u64
    }

    fn default_budget() -> MemoryBudget {
        MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default())
    }

    /// Scripted availability (bytes), sample counter, and clock (ms).
    struct Script {
        available: Arc<AtomicU64>,
        samples: Arc<AtomicU64>,
        clock_ms: Arc<AtomicU64>,
    }

    impl Script {
        fn install(pool: &mut MemPool, available: u64) -> Self {
            let script = Self {
                available: Arc::new(AtomicU64::new(available)),
                samples: Arc::new(AtomicU64::new(0)),
                clock_ms: Arc::new(AtomicU64::new(0)),
            };
            pool.install_pressure_hooks(PressureHooks::scripted(
                Arc::clone(&script.available),
                Arc::clone(&script.samples),
                Arc::clone(&script.clock_ms),
            ));
            script
        }

        fn set_available(&self, bytes: u64) {
            self.available.store(bytes, AtOrd::SeqCst);
        }

        fn advance_ms(&self, millis: u64) {
            self.clock_ms.fetch_add(millis, AtOrd::SeqCst);
        }

        fn samples(&self) -> u64 {
            self.samples.load(AtOrd::SeqCst)
        }
    }

    #[test]
    fn tiny_payload_is_pressure_denied_before_map_and_smaller_dedicated_fits() {
        let config = pressure_config();
        let budget = default_budget();
        let mut pool =
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone());
        let buddy = buddy_total(&config);
        let dedicated = dedicated_backing(4096);
        assert!(
            dedicated < buddy,
            "test geometry needs a smaller dedicated tier"
        );
        // Availability that admits an 8192-byte dedicated backing at the 50%
        // bar but not the larger full buddy segment.
        Script::install(&mut pool, 2 * dedicated);

        let alloc = pool.alloc(4096).unwrap();
        assert!(
            alloc.is_dedicated,
            "a tiny payload must not create the oversized buddy backing under pressure"
        );
        assert_eq!(pool.segment_count(), 0, "buddy backing denied before map");
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated);
        let stats = pool.stats();
        assert_eq!(stats.pressure_denied_backings, 1);
        assert_eq!(stats.dedicated_allocs, 1);
        assert_eq!(stats.buddy_expanded_allocs, 0);
        pool.free(&alloc).unwrap();
    }

    #[test]
    fn entrypoints_agree_on_backing_policy_under_pressure() {
        let config = pressure_config();
        let dedicated = dedicated_backing(4096);

        // Pressure denies the buddy segment but a dedicated backing fits:
        // every entry point selects dedicated, none errors or spills.
        let budget = default_budget();
        let mut pool = MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget);
        Script::install(&mut pool, 2 * dedicated);
        assert!(
            pool.ensure_ready()
                .unwrap_err()
                .contains("OS memory pressure"),
            "explicit prewarm obeys the same backing policy"
        );
        assert_eq!(pool.segment_count(), 0);
        let a = pool.alloc(4096).unwrap();
        assert!(a.is_dedicated);
        let h = pool.alloc_handle(4096).unwrap();
        assert!(
            h.is_dedicated(),
            "alloc_handle must not spill while SHM fits"
        );
        let s = pool.try_alloc_shm(4096).unwrap();
        assert!(s.is_dedicated());
        pool.free(&a).unwrap();
        pool.release_handle(h);
        pool.release_handle(s);

        // Forced threshold zero: SHM creation always denied. alloc_handle
        // takes file backing; SHM-only entry points return capacity errors.
        let mut forced_config = config;
        forced_config.spill_threshold = 0.0;
        let mut forced =
            MemPool::new_with_prefix_and_budget(forced_config, unique_prefix(), default_budget());
        Script::install(&mut forced, u64::MAX);
        assert!(
            forced
                .ensure_ready()
                .unwrap_err()
                .contains("forces new shared-memory backings")
        );
        assert!(
            forced
                .alloc(4096)
                .unwrap_err()
                .contains("forces new shared-memory backings")
        );
        assert!(
            forced
                .try_alloc_shm(4096)
                .unwrap_err()
                .contains("no SHM capacity")
        );
        let spilled = forced.alloc_handle(4096).unwrap();
        assert!(spilled.is_file_spill());
        assert_eq!(forced.segment_count(), 0);
        // ensure_ready(1) + alloc(buddy+dedicated) + try_alloc_shm(2) +
        // alloc_handle(2) pressure denials in total.
        assert_eq!(forced.stats().pressure_denied_backings, 7);
        assert_eq!(forced.stats().file_spill_allocs, 1);
        drop(spilled);
    }

    #[test]
    fn existing_buddy_reuse_succeeds_under_pressure_with_no_new_charge() {
        let config = pressure_config();
        let budget = default_budget();
        let mut pool =
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone());

        // Ample availability maps one buddy segment holding two blocks.
        Script::install(&mut pool, 1 << 40);
        let a = pool.alloc(4096).unwrap();
        let b = pool.alloc(4096).unwrap();
        assert!(!a.is_dedicated && !b.is_dedicated);
        pool.free(&a).unwrap();
        pool.free(&b).unwrap();
        let charged = budget.snapshot().shm.used_bytes;
        assert_eq!(charged, buddy_total(&config));

        // Zero observed availability: reuse of the already-mapped segment is
        // still permitted, takes no new charge, and never consults pressure.
        Script::install(&mut pool, 0);
        let reused = pool.alloc(4096).unwrap();
        assert!(!reused.is_dedicated);
        assert_eq!(budget.snapshot().shm.used_bytes, charged);
        let stats = pool.stats();
        assert_eq!(stats.buddy_reused_allocs, 2);
        assert_eq!(stats.buddy_expanded_allocs, 1);
        assert_eq!(stats.pressure_denied_backings, 0);
        pool.free(&reused).unwrap();
    }

    #[test]
    fn cached_observation_bounds_os_sampling_at_the_seams() {
        let mut pool = MemPool::new_with_prefix_and_budget(
            pressure_config(),
            unique_prefix(),
            default_budget(),
        );
        // Zero availability denies every creation seam deterministically.
        let script = Script::install(&mut pool, 0);

        for _ in 0..5 {
            assert!(
                pool.ensure_ready()
                    .unwrap_err()
                    .contains("OS memory pressure")
            );
        }
        assert_eq!(
            script.samples(),
            1,
            "repeated decisions inside one TTL must share a single OS sample"
        );

        // After the TTL a fresh sample is taken.
        script.advance_ms(1_001);
        assert!(
            pool.ensure_ready()
                .unwrap_err()
                .contains("OS memory pressure")
        );
        assert_eq!(script.samples(), 2);
    }

    #[test]
    fn recovery_band_gates_buddy_creation_until_genuine_recovery() {
        let config = pressure_config();
        let buddy = buddy_total(&config);
        let mut pool =
            MemPool::new_with_prefix_and_budget(config, unique_prefix(), default_budget());
        // Direct bar: buddy needs 2*buddy observed bytes at threshold 0.5.
        let script = Script::install(&mut pool, 2 * buddy - 1);
        assert!(
            pool.ensure_ready()
                .unwrap_err()
                .contains("OS memory pressure")
        );

        // Just past the direct bar but inside the recovery band (which needs
        // 2.5*buddy): without hysteresis this would flip to admitting.
        script.advance_ms(1_001);
        script.set_available(2 * buddy + 2);
        assert!(
            pool.ensure_ready()
                .unwrap_err()
                .contains("OS memory pressure"),
            "availability inside the recovery band must stay denied"
        );
        assert_eq!(pool.segment_count(), 0);

        // Genuine recovery admits the same tier again.
        script.advance_ms(1_001);
        script.set_available(3 * buddy);
        pool.ensure_ready().unwrap();
        assert_eq!(pool.segment_count(), 1);
    }

    #[test]
    fn pressure_denied_shm_escapes_to_file_backing() {
        let config = pressure_config();
        let budget = MemoryBudget::new(u64::MAX, u64::MAX, u64::MAX);
        let mut pool = MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone());
        // A failed (zero) availability observation denies every SHM seam.
        Script::install(&mut pool, 0);
        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        assert_eq!(pool.stats().pressure_denied_backings, 2);
        assert_eq!(budget.snapshot().file.used_bytes, 4096);
        drop(handle);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
    }

    #[test]
    fn exhausted_file_backing_yields_a_clear_terminal_failure() {
        let mut config = pressure_config();
        config.spill_threshold = 0.0;
        let budget = MemoryBudget::new(u64::MAX, 0, u64::MAX);
        let mut pool = MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget);
        Script::install(&mut pool, u64::MAX);

        let error = pool.alloc_handle(4096).unwrap_err();
        assert!(
            error.contains("memory budget cell 'file'"),
            "terminal failure should name the exhausted file cell: {error}"
        );
        assert_eq!(pool.segment_count(), 0);
        assert_eq!(pool.stats().dedicated_segments, 0);
        assert_eq!(pool.stats().pressure_denied_backings, 2);
    }

    #[test]
    fn counters_and_utilization_reflect_the_actually_selected_tiers() {
        let config = pressure_config();
        let budget = default_budget();
        let mut pool =
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone());
        Script::install(&mut pool, 1 << 40);

        // First small allocation expands the buddy pool; the next reuses it.
        let block = pool.alloc(4096).unwrap();
        let mut stats = pool.stats();
        assert_eq!(stats.buddy_expanded_allocs, 1);
        assert_eq!(stats.buddy_reused_allocs, 0);
        assert_eq!(stats.buddy_occupied_bytes, 4096);
        assert_eq!(stats.buddy_idle_bytes, 4096);
        assert_eq!(stats.buddy_data_bytes, 8192);
        assert!((stats.utilization_ratio - 0.5).abs() < 1e-9);
        pool.free(&block).unwrap();
        assert_eq!(pool.stats().utilization_ratio, 0.0);

        let reused = pool.alloc(4096).unwrap();
        assert_eq!(pool.stats().buddy_reused_allocs, 1);
        pool.free(&reused).unwrap();

        // Dedicated lifecycle: active, then pending-free (read_done unset),
        // then reclaimed — mapped capacity must include pending entries.
        let dedicated_alloc = pool.alloc(20000).unwrap();
        assert!(dedicated_alloc.is_dedicated);
        let backing = dedicated_backing(20000);
        stats = pool.stats();
        assert_eq!(stats.dedicated_allocs, 1);
        assert_eq!(stats.dedicated_active_count, 1);
        assert_eq!(stats.dedicated_active_bytes, backing);
        assert_eq!(stats.dedicated_pending_free_bytes, 0);
        assert_eq!(stats.dedicated_mapped_bytes, backing);
        assert_eq!(stats.alloc_count, 1);

        pool.free(&dedicated_alloc).unwrap();
        stats = pool.stats();
        assert_eq!(stats.dedicated_active_count, 0);
        assert_eq!(stats.dedicated_active_bytes, 0);
        assert_eq!(stats.dedicated_pending_free_bytes, backing);
        assert_eq!(
            stats.dedicated_mapped_bytes, backing,
            "pending-free dedicated entries stay mapped until read_done/GC"
        );
        assert_eq!(stats.alloc_count, 0);

        pool.dedicated
            .get(&dedicated_alloc.seg_idx)
            .unwrap()
            .segment
            .mark_read_done();
        pool.gc_dedicated();
        stats = pool.stats();
        assert_eq!(stats.dedicated_mapped_bytes, 0);
        assert_eq!(stats.dedicated_pending_free_bytes, 0);

        // File tier through the forced-threshold route.
        let mut forced_config = config;
        forced_config.spill_threshold = 0.0;
        let spill_dir =
            std::env::temp_dir().join(format!("c2_pressure_count_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&spill_dir);
        forced_config.spill_dir = spill_dir.clone();
        let mut spiller =
            MemPool::new_with_prefix_and_budget(forced_config, unique_prefix(), budget.clone());
        let handle = spiller.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        stats = spiller.stats();
        assert_eq!(stats.file_spill_allocs, 1);
        assert_eq!(stats.buddy_expanded_allocs, 0);
        assert_eq!(stats.buddy_reused_allocs, 0);
        assert_eq!(stats.dedicated_allocs, 0);
        drop(handle);
        let _ = std::fs::remove_dir_all(&spill_dir);
    }
}
