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
use crate::handle::{HandleBacking, MemHandle, PoolIncarnation};
use crate::pressure::{PressureDecision, PressureEngine};
use crate::spill;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

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

struct DedicatedEntry {
    segment: DedicatedSegment,
    generation: u32,
}

/// Unified memory pool — the main entry point. Retain the original pool
/// alongside every allocation handle and connection loan until release;
/// dropping its last owner drops the remaining cached backings.
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
    incarnation: PoolIncarnation,
    /// Dedicated slot generations persist across local release and slot reuse.
    dedicated_generations: Vec<u32>,
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
    pub fn new(config: PoolConfig) -> Result<Self, String> {
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
    pub fn new_with_prefix(config: PoolConfig, name_prefix: String) -> Result<Self, String> {
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
    ) -> Result<Self, String> {
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
    pub fn open_peer(config: PoolConfig, name_prefix: String) -> Result<Self, String> {
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
    ) -> Result<Self, String> {
        if name_prefix.len() > Self::MAX_SHM_PREFIX_LEN {
            return Err(format!(
                "SHM prefix is {} bytes, exceeds handshake maximum {}",
                name_prefix.len(),
                Self::MAX_SHM_PREFIX_LEN,
            ));
        }
        Self::validate_config(&config)?;
        let pressure = PressureEngine::new(config.spill_threshold);
        Ok(Self {
            config,
            segments: Vec::new(),
            generations: Vec::new(),
            is_peer,
            dedicated: HashMap::new(),
            name_prefix,
            incarnation: PoolIncarnation::new(),
            dedicated_generations: Vec::new(),
            idle_since: Vec::new(),
            budget,
            pressure,
            counters: TierCounters::default(),
        })
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
        BuddyAllocator::checked_layout(config.segment_size, config.min_block_size).ok_or_else(
            || {
                format!(
                    "buddy backing geometry unsupported: segment_size {} with min_block {} exceeds the u32 data capacity or the addressable mapping span",
                    config.segment_size, config.min_block_size
                )
            },
        )?;
        if config.max_segments > u16::MAX as usize + 1 {
            return Err("max_segments exceeds the wire segment-index range".into());
        }
        if config.min_retained_segments > config.max_segments {
            return Err(format!(
                "min_retained_segments ({}) must not exceed max_segments ({})",
                config.min_retained_segments, config.max_segments
            ));
        }
        if config.max_dedicated_segments > u16::MAX as usize + 1 {
            return Err("max_dedicated_segments exceeds the wire segment-index range".into());
        }
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
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir("/dev/shm") else {
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(pid) = Self::extract_pid_from_name(&name_str) {
                if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
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
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }

        self.alloc_shm(size).map_err(BackingError::into_message)
    }

    /// Allocate a unique SHM owner. Move it into the connection loan book before publication.
    pub fn alloc_shm_handle(&mut self, size: usize) -> Result<MemHandle, String> {
        self.alloc(size)
            .map(|a| a.into_handle(size, self.incarnation))
    }

    /// Free a local allocation after the caller has ended every payload read.
    ///
    /// # Safety
    /// Own this exact allocation and its original level. No local or borrowed
    /// peer access may remain, and no other owner may release it.
    pub unsafe fn free(&mut self, alloc: &PoolAllocation) -> Result<(), String> {
        if self.is_peer {
            return Err("cannot free allocations in a peer pool".into());
        }
        self.validate_generation(alloc.seg_idx, alloc.generation, alloc.is_dedicated)?;
        if alloc.is_dedicated {
            self.dedicated.remove(&alloc.seg_idx);
            Ok(())
        } else {
            self.free_buddy(alloc);
            Ok(())
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
            Ok(seg.base_ptr().wrapping_add(alloc.offset as usize))
        }
    }

    /// Observe this pool's mapped backing. Peer views expose capacities,
    /// not the owner's local allocation counters.
    pub fn stats(&self) -> PoolStats {
        let mut buddy_data = 0u64;
        let mut buddy_idle = 0u64;
        let mut alloc_count = 0u32;
        for segment in self.segments.iter().flatten() {
            buddy_data += segment.size() as u64;
            if !self.is_peer {
                buddy_idle += segment.allocator().free_bytes();
                alloc_count += segment.allocator().alloc_count();
            }
        }
        let dedicated_mapped: u64 = self
            .dedicated
            .values()
            .map(|e| e.segment.size() as u64)
            .sum();
        let dedicated_active_count = if self.is_peer {
            0
        } else {
            self.dedicated.len()
        };
        alloc_count += dedicated_active_count as u32;
        let buddy_occupied = if self.is_peer {
            0
        } else {
            buddy_data - buddy_idle
        };
        PoolStats {
            total_segments: self.segment_count(),
            dedicated_segments: self.dedicated.len(),
            alloc_count,
            buddy_data_bytes: buddy_data,
            buddy_occupied_bytes: buddy_occupied,
            buddy_idle_bytes: buddy_idle,
            dedicated_mapped_bytes: dedicated_mapped,
            dedicated_active_count,
            dedicated_active_bytes: if self.is_peer { 0 } else { dedicated_mapped },
            buddy_reused_allocs: self.counters.buddy_reused_allocs,
            buddy_expanded_allocs: self.counters.buddy_expanded_allocs,
            dedicated_allocs: self.counters.dedicated_allocs,
            file_spill_allocs: self.counters.file_spill_allocs,
            pressure_denied_backings: self.pressure.denials(),
            utilization_ratio: if buddy_data == 0 {
                0.0
            } else {
                buddy_occupied as f64 / buddy_data as f64
            },
        }
    }

    /// Reclaim trailing idle owner backings down to the retained minimum.
    /// Allocation handles retained by a loan book keep the local count live.
    /// Slot generations are never truncated or reset. Peer caches have no
    /// allocator state to inspect and are released by their mapping owner.
    pub fn gc_buddy(&mut self) -> usize {
        if self.is_peer {
            return 0;
        }
        let delay = if self.config.buddy_idle_decay_secs < 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.config.buddy_idle_decay_secs)
        };
        let now = Instant::now();
        let mut live = self.segment_count();
        let mut removed = 0;
        for index in (0..self.segments.len()).rev() {
            if live <= self.config.min_retained_segments {
                break;
            }
            let Some(segment) = self.segments[index].as_ref() else {
                continue;
            };
            if segment.allocator().alloc_count() != 0 {
                break;
            }
            let idle_at = *self.idle_since[index].get_or_insert(now);
            if now.duration_since(idle_at) < delay {
                break;
            }
            self.segments[index].take();
            self.idle_since[index] = None;
            live -= 1;
            removed += 1;
        }
        removed
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
        while self.segment_count() < count {
            let (idx, segment) = self.create_segment().map_err(BackingError::into_message)?;
            self.install_segment(idx, segment);
        }
        Ok(())
    }

    /// Get the number of buddy segments.
    pub fn segment_count(&self) -> usize {
        self.segments.iter().flatten().count()
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

    pub fn dedicated_segment_name(prefix: &str, idx: u32, generation: u32) -> String {
        Self::backing_name(prefix, b'd', idx, generation)
    }

    fn backing_name(prefix: &str, kind: u8, idx: u32, generation: u32) -> String {
        let mut digest = Sha256::new();
        digest.update(b"c-two.shm.v4\0");
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

    /// Open exactly the advertised buddy generation, read-only and without
    /// attaching an allocator. `min_size` bounds the admitted descriptor's end.
    pub fn ensure_peer_segment(
        &mut self,
        seg_idx: u32,
        generation: u32,
        min_size: usize,
    ) -> Result<(), String> {
        self.check_peer_identity(seg_idx, generation, self.config.max_segments)?;
        let idx = seg_idx as usize;
        let known = self.generations.get(idx).copied().unwrap_or(0);
        if generation < known {
            return Err("stale shared-memory backing generation".into());
        }
        if generation == known
            && let Some(segment) = self.segment(idx)
        {
            return check_span(segment.size(), 0, min_size).map(|_| ());
        }
        let name = Self::buddy_segment_name(&self.name_prefix, seg_idx, generation);
        let segment = BuddySegment::open(&name, min_size)?;
        if self.generations.len() <= idx {
            self.generations.resize(idx + 1, 0);
        }
        self.generations[idx] = generation;
        self.install_segment(idx, segment);
        Ok(())
    }

    pub fn ensure_peer_dedicated(
        &mut self,
        seg_idx: u32,
        generation: u32,
        min_size: usize,
    ) -> Result<(), String> {
        let name = Self::dedicated_segment_name(&self.name_prefix, seg_idx, generation);
        // SAFETY: the name was derived from this peer pool's identity.
        unsafe { self.open_dedicated_at(seg_idx, generation, &name, min_size) }
    }

    fn check_peer_identity(&self, slot: u32, generation: u32, limit: usize) -> Result<(), String> {
        if !self.is_peer {
            return Err("cannot open peer backings in an owner pool".into());
        }
        if generation == 0 || slot > u16::MAX as u32 || slot as usize >= limit {
            return Err("invalid peer backing identity".into());
        }
        Ok(())
    }

    fn validate_generation(
        &self,
        seg_idx: u32,
        generation: u32,
        dedicated: bool,
    ) -> Result<(), String> {
        let current = if dedicated {
            self.dedicated.get(&seg_idx).map(|entry| entry.generation)
        } else {
            self.segment_generation(seg_idx as usize)
        };
        if seg_idx > u16::MAX as u32 || generation == 0 || current != Some(generation) {
            return Err("stale or unavailable shared-memory backing generation".into());
        }
        Ok(())
    }

    /// Raw peer entry retained for the Python boundary until its removal.
    ///
    /// # Safety
    /// `name` must identify this peer pool's exact kind, slot and generation.
    /// No read may outlive the cached mapping when it is replaced or dropped.
    pub unsafe fn open_dedicated_at(
        &mut self,
        idx: u32,
        generation: u32,
        name: &str,
        min_size: usize,
    ) -> Result<(), String> {
        self.check_peer_identity(idx, generation, self.config.max_dedicated_segments)?;
        let known = self
            .dedicated_generations
            .get(idx as usize)
            .copied()
            .unwrap_or(0);
        if generation < known {
            return Err("stale shared-memory backing generation".into());
        }
        if generation == known
            && let Some(entry) = self.dedicated.get(&idx)
        {
            return check_span(entry.segment.size(), 0, min_size).map(|_| ());
        }
        let segment = DedicatedSegment::open(name, min_size)?;
        self.dedicated_generations
            .resize(self.dedicated_generations.len().max(idx as usize + 1), 0);
        self.dedicated_generations[idx as usize] = generation;
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment,
                generation,
            },
        );
        Ok(())
    }

    /// Raw local release boundary. Peer mappings grant no release authority.
    ///
    /// # Safety
    /// Uniquely own the local allocation and pass its original buddy level.
    /// All local and peer reads must have ended. Coordinates cannot establish
    /// ownership; normal callers consume the move-only `MemHandle` instead.
    pub unsafe fn free_at(
        &mut self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        level: u16,
        is_dedicated: bool,
    ) -> Result<(), String> {
        unsafe {
            self.free(&PoolAllocation {
                seg_idx,
                generation,
                offset,
                actual_size: 0,
                level,
                is_dedicated,
            })
        }
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
            Ok(seg.base_ptr().wrapping_add(offset as usize))
        }
    }

    /// Copy bytes from mapped shared-memory coordinates after validating that
    /// the complete span belongs to the selected segment's data region.
    /// # Safety
    /// The span belongs to a live published allocation. No writer or release may race the copy through any mapping.
    pub unsafe fn copy_data_at(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<Vec<u8>, String> {
        let (pointer, len) =
            self.checked_data_pointer(seg_idx, generation, offset, data_size, is_dedicated)?;

        // SAFETY: `checked_data_pointer` proves that the non-empty span is
        // entirely inside the mapped data region and no longer than
        // `isize::MAX`.
        Ok(unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec())
    }

    /// Validate shared-memory coordinates without copying their contents.
    ///
    /// Raw descriptor admission checks the actual OS view, without allocator geometry.
    pub fn validate_data_at(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(), String> {
        self.checked_data_pointer(seg_idx, generation, offset, data_size, is_dedicated)
            .map(|_| ())
    }

    fn checked_data_pointer(
        &self,
        seg_idx: u32,
        generation: u32,
        offset: u32,
        data_size: u32,
        dedicated: bool,
    ) -> Result<(*mut u8, usize), String> {
        self.validate_generation(seg_idx, generation, dedicated)?;
        let (base, span) = if dedicated {
            if offset != 0 {
                return Err("dedicated shared-memory offset must be zero".into());
            }
            let segment = &self
                .dedicated
                .get(&seg_idx)
                .expect("validated generation")
                .segment;
            (segment.data_ptr(), segment.size())
        } else {
            let segment = self
                .segment(seg_idx as usize)
                .expect("validated generation");
            (segment.base_ptr(), segment.size())
        };
        let start = offset as usize;
        let len = data_size as usize;
        check_span(span, start, len)?;
        Ok((base.wrapping_add(start), len))
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
        match self.alloc_shm(size) {
            Ok(alloc) => Ok(alloc.into_handle(size, self.incarnation)),
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
        self.alloc_shm(size)
            .map(|alloc| alloc.into_handle(size, self.incarnation))
            .map_err(|_| format!("try_alloc_shm: no SHM capacity for {size} bytes"))
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
        Ok(MemHandle {
            backing: HandleBacking::FileSpill {
                mmap: mmap.with_budget_guard(guard),
                path,
                len: size,
            },
        })
    }

    /// Read a private, move-only owner's logical bytes. Creation and trim
    /// establish the length; accesses check only pool/backing identity.
    pub fn handle_slice<'a>(&'a self, handle: &'a MemHandle) -> &'a [u8] {
        self.checked_handle_slice(handle)
            .expect("invalid memory handle")
    }

    pub fn copy_handle_data(&self, handle: &MemHandle) -> Result<Vec<u8>, String> {
        Ok(self.checked_handle_slice(handle)?.to_vec())
    }

    fn checked_handle_slice<'a>(&'a self, handle: &'a MemHandle) -> Result<&'a [u8], String> {
        self.validate_handle_owner(handle)?;
        match &handle.backing {
            HandleBacking::Shm {
                allocation, len, ..
            } => {
                let pointer = self.data_ptr(allocation)?;
                // SAFETY: private creation/trim prove the span. Both the
                // unique owner and its matching live pool remain borrowed.
                Ok(unsafe { std::slice::from_raw_parts(pointer, *len) })
            }
            HandleBacking::FileSpill { mmap, len, .. } => Ok(&mmap[..*len]),
        }
    }

    /// Check local identity, without proving private lengths again.
    pub fn validate_handle(&self, handle: &MemHandle) -> Result<(), String> {
        self.validate_handle_owner(handle)?;
        if let HandleBacking::Shm { allocation, .. } = &handle.backing {
            self.data_ptr(allocation)?;
        }
        Ok(())
    }

    fn validate_handle_owner(&self, handle: &MemHandle) -> Result<(), String> {
        if let HandleBacking::Shm { owner, .. } = &handle.backing {
            if self.is_peer {
                return Err("peer pools never own allocation handles".into());
            }
            if *owner != self.incarnation {
                return Err("pool owner incarnation mismatch".into());
            }
        }
        Ok(())
    }

    pub fn handle_slice_mut<'a>(&'a self, handle: &'a mut MemHandle) -> &'a mut [u8] {
        self.validate_handle_owner(handle)
            .expect("invalid memory handle");
        match &mut handle.backing {
            HandleBacking::Shm {
                allocation, len, ..
            } => {
                let pointer = self.data_ptr(allocation).expect("invalid backing identity");
                // SAFETY: the exclusive unforgeable owner establishes the
                // span and excludes other safe accesses to this allocation.
                unsafe { std::slice::from_raw_parts_mut(pointer, *len) }
            }
            HandleBacking::FileSpill { mmap, len, .. } => &mut mmap[..*len],
        }
    }

    /// Consume the allocation owner. The caller retains this original pool
    /// alongside every local handle or connection loan until release.
    ///
    /// # Panics
    /// A wrong pool or stale backing breaks the local ownership invariant.
    /// Local allocator failures also panic; there is no recoverable release
    /// or retry path. File owners release their own backing on drop.
    pub fn release_handle(&mut self, handle: MemHandle) {
        self.validate_handle_owner(&handle)
            .expect("memory handle pool identity invariant violated");
        if let HandleBacking::Shm { allocation, .. } = &handle.backing {
            // SAFETY: moving the private owner ends its local borrows. The
            // loan book must end all peer reads before consuming this owner.
            unsafe { self.free(allocation) }
                .expect("memory handle backing identity invariant violated");
        }
    }

    // --- Internal methods ---

    fn buddy_block_limit(&self) -> usize {
        if !self.config.buddy_enabled || self.config.max_segments == 0 {
            return 0;
        }
        BuddyAllocator::checked_layout(self.config.segment_size, self.config.min_block_size)
            .expect("validated buddy geometry")
            .data_size
    }

    fn alloc_shm(&mut self, size: usize) -> Result<PoolAllocation, BackingError> {
        let max_buddy = self.buddy_block_limit();
        if max_buddy > 0 && size <= max_buddy {
            // Layer 1: Reuse an already-mapped segment — permitted under
            // pressure, with no new backing charge and no OS query.
            if let Some(a) = self.try_buddy_reuse(size) {
                return Ok(a);
            }

            // Decay may release a backing charge before expansion. Reuse
            // was already exhausted; reclamation cannot create free blocks.
            self.gc_buddy();

            // Layer 2: Create a new segment (the pressure/budget-enforced seam).
            // A creation failure (for example the SHM object cannot be mapped or
            // pressure denies the full segment) falls through to Layer 3
            // dedicated storage instead of failing the allocation outright.
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
        let (idx, segment) = self.create_segment().ok()?;
        let allocation = segment
            .allocator()
            .alloc(size)
            .expect("size fits new buddy backing");
        self.install_segment(idx, segment);
        self.counters.buddy_expanded_allocs = self.counters.buddy_expanded_allocs.saturating_add(1);
        Some(PoolAllocation {
            seg_idx: idx as u32,
            generation: self.generations[idx],
            offset: allocation.offset,
            actual_size: allocation.actual_size,
            level: allocation.level,
            is_dedicated: false,
        })
    }

    fn install_segment(&mut self, idx: usize, segment: BuddySegment) {
        if self.segments.len() <= idx {
            self.segments.resize_with(idx + 1, || None);
            self.idle_since.resize(idx + 1, None);
        }
        self.segments[idx] = Some(segment);
        self.idle_since[idx] = None;
    }

    fn alloc_dedicated(&mut self, size: usize) -> Result<PoolAllocation, BackingError> {
        // Each kind has its own u16 slot space. Only empty slots advance;
        // checked generation exhaustion retires that slot permanently.
        let (idx, generation) = (0..self.config.max_dedicated_segments)
            .find_map(|idx| {
                if self.dedicated.contains_key(&(idx as u32)) {
                    return None;
                }
                self.dedicated_generations
                    .get(idx)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .map(|generation| (idx as u32, generation))
            })
            .ok_or_else(|| {
                BackingError::Other(
                    "dedicated segment capacity exhausted (live or retired slots)".into(),
                )
            })?;
        let backing = DedicatedSegment::required_shm_size(size)
            .filter(|size| *size <= u32::MAX as usize)
            .ok_or_else(|| {
                BackingError::Other(format!(
                    "dedicated segment size {size} exceeds the addressable wire backing range"
                ))
            })?;
        let backing_bytes = backing as u64;
        if let PressureDecision::Deny { reason } = self
            .pressure
            .evaluate(backing_bytes, "dedicated shared-memory backing")
        {
            return Err(BackingError::Pressure(reason));
        }
        let guard = self
            .reserve_backing(BudgetKind::Shm, backing_bytes)
            .map_err(BackingError::Budget)?;
        let name = Self::dedicated_segment_name(&self.name_prefix, idx, generation);
        let segment = DedicatedSegment::create(&name, size)
            .map_err(BackingError::Other)?
            .with_budget_guard(guard);
        self.dedicated_generations
            .resize(self.dedicated_generations.len().max(idx as usize + 1), 0);
        self.dedicated_generations[idx as usize] = generation;
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment,
                generation,
            },
        );
        self.counters.dedicated_allocs = self.counters.dedicated_allocs.saturating_add(1);
        Ok(PoolAllocation {
            seg_idx: idx,
            generation,
            offset: 0,
            actual_size: backing as u32,
            level: 0,
            is_dedicated: true,
        })
    }

    fn free_buddy(&mut self, allocation: &PoolAllocation) {
        let idx = allocation.seg_idx as usize;
        let allocator = self.segment(idx).expect("validated generation").allocator();
        // SAFETY: exact level/offset came from the unique local allocation.
        unsafe { allocator.free(allocation.offset, allocation.level) };
        if allocator.alloc_count() == 0 {
            self.idle_since[idx] = Some(Instant::now());
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

    fn create_segment(&mut self) -> Result<(usize, BuddySegment), BackingError> {
        if self.is_peer {
            return Err(BackingError::Other("cannot create peer backing".into()));
        }
        let (idx, generation) = (0..self.config.max_segments)
            .find_map(|idx| {
                if self.segment(idx).is_some() {
                    return None;
                }
                self.generations
                    .get(idx)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .map(|generation| (idx, generation))
            })
            .ok_or_else(|| {
                BackingError::Other(
                    "buddy segment capacity exhausted (live or retired slots)".into(),
                )
            })?;

        let name = Self::buddy_segment_name(&self.name_prefix, idx as u32, generation);
        // Constructor validation guarantees this layout before reservation.
        let layout =
            BuddyAllocator::checked_layout(self.config.segment_size, self.config.min_block_size)
                .expect("validated buddy geometry");
        // SHM contains payload bytes only; metadata stays in local memory.
        // Reusing blocks within charged backing takes no new reservation.
        let backing_bytes = layout.total_size as u64;
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
        Ok((idx, segment))
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
    fn into_handle(self, len: usize, owner: PoolIncarnation) -> MemHandle {
        MemHandle {
            backing: HandleBacking::Shm {
                allocation: self,
                owner,
                len,
            },
        }
    }
}

fn check_span(span: usize, start: usize, len: usize) -> Result<(), String> {
    if len == 0 {
        return Err("shared-memory copy span must not be empty".into());
    }
    let end = start
        .checked_add(len)
        .ok_or("shared-memory copy span overflows the platform address space")?;
    if end > span || end > isize::MAX as usize {
        return Err(format!(
            "shared-memory copy span {start}..{end} is outside mapped segment span {span}"
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_pool(config: PoolConfig) -> MemPool {
        let id = TEST_COUNTER.fetch_add(1, AtOrd::Relaxed);
        let prefix = format!("/cc3t{:04x}{:04x}", std::process::id() as u16, id);
        MemPool::new_with_prefix(config, prefix).unwrap()
    }

    fn small_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 2,

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

        unsafe { pool.free(&a) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
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

            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        assert_eq!(pool.segment_count(), 1);

        let b = pool.alloc(32 * 1024).unwrap();
        assert!(pool.segment_count() >= 2 || b.is_dedicated);

        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
    }

    #[test]
    fn test_dedicated_fallback() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,

            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);

        let a = pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);

        unsafe { pool.free(&a) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();
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
                unsafe {
                    pool.copy_data_at(
                        allocation.seg_idx,
                        allocation.generation,
                        allocation.offset,
                        u32::try_from(payload.len()).unwrap(),
                        allocation.is_dedicated,
                    )
                }
                .unwrap(),
                payload,
            );
            unsafe { pool.free(&allocation) }.unwrap();
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
            unsafe {
                pool.copy_data_at(
                    buddy.seg_idx,
                    buddy.generation,
                    u32::try_from(buddy_capacity - 1).unwrap(),
                    2,
                    false,
                )
            }
            .unwrap_err()
            .contains("outside mapped segment")
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
            .contains("outside mapped segment")
        );
        assert!(
            unsafe { pool.copy_data_at(buddy.seg_idx, buddy.generation, buddy.offset, 0, false) }
                .unwrap_err()
                .contains("must not be empty")
        );
        unsafe { pool.free(&buddy) }.unwrap();

        let dedicated = pool.alloc(128 * 1024).unwrap();
        assert!(dedicated.is_dedicated);
        assert!(
            unsafe { pool.copy_data_at(dedicated.seg_idx, dedicated.generation, 1, 1, true) }
                .unwrap_err()
                .contains("offset must be zero")
        );
        assert!(
            unsafe {
                pool.copy_data_at(dedicated.seg_idx, dedicated.generation, 0, u32::MAX, true)
            }
            .unwrap_err()
            .contains("outside mapped segment")
        );
        unsafe { pool.free(&dedicated) }.unwrap();
    }

    #[test]
    fn test_zero_alloc_fails() {
        let mut pool = test_pool(small_config());
        assert!(pool.alloc(0).is_err());
    }

    #[test]
    fn test_validate_zero_min_block() {
        let config = PoolConfig {
            min_block_size: 0,
            ..small_config()
        };
        assert!(MemPool::validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_non_power_of_two_min_block() {
        let config = PoolConfig {
            min_block_size: 3000,
            ..small_config()
        };
        assert!(MemPool::validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_segment_too_small() {
        let config = PoolConfig {
            segment_size: 4096,
            min_block_size: 4096,
            ..small_config()
        };
        assert!(MemPool::validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_nan_idle_decay() {
        let config = PoolConfig {
            buddy_idle_decay_secs: f64::NAN,
            ..small_config()
        };
        assert!(MemPool::validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_non_finite_gc_delays() {
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
        config.buddy_idle_decay_secs = 1e100;
        assert!(
            MemPool::validate_config(&config)
                .unwrap_err()
                .contains("representable duration")
        );
    }

    #[test]
    fn test_validate_negative_gc_delay_ok() {
        let config = PoolConfig {
            buddy_idle_decay_secs: -1.0,
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
            unsafe { pool.free(a) }.unwrap();
        }

        // GC should reclaim trailing idle segments (but keep at least 1).
        let removed = pool.gc_buddy();
        assert!(removed > 0);
        assert!(pool.segment_count() >= 1);

        // Pool should still be usable.
        let a = pool.alloc(4096).unwrap();
        unsafe { pool.free(&a) }.unwrap();
    }

    #[test]
    fn test_gc_buddy_keeps_segment_with_allocs() {
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,

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
            unsafe { pool.free(a) }.unwrap();
        }

        let removed = pool.gc_buddy();
        // Trailing segment may or may not be empty. At minimum, first segment
        // still has 1 alloc, so pool is still alive.
        assert!(pool.segment_count() >= 1);
        let _ = removed;

        unsafe { pool.free(&allocs[0]) }.unwrap();
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
        unsafe { pool.free(&a0) }.unwrap();
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
        unsafe { pool.free(&a1) }.unwrap();
        let a1_big = pool.alloc(big).unwrap();
        assert_eq!(a1_big.seg_idx, 1);
        let a2 = pool.alloc(4096).unwrap();
        assert_eq!(a2.seg_idx, 2);
        let name2 = pool.segment_name(2).unwrap();
        assert_eq!(
            name2,
            MemPool::buddy_segment_name(pool.prefix(), 2, a2.generation)
        );

        unsafe { pool.free(&a_ded) }.unwrap();
        unsafe { pool.free(&a_big) }.unwrap();
        unsafe { pool.free(&a1_big) }.unwrap();
        unsafe { pool.free(&a2) }.unwrap();
    }

    #[test]
    fn test_deterministic_dedicated_naming() {
        let mut pool = test_pool(PoolConfig {
            max_segments: 1,
            max_dedicated_segments: 4,
            ..test_config()
        });

        let a0 = pool.alloc(4096).unwrap();
        unsafe { pool.free(&a0) }.unwrap();

        let big = pool.config.segment_size * 2;
        let d0 = pool.alloc(big).unwrap();
        assert!(d0.is_dedicated);
        assert_eq!(d0.seg_idx, 0);
        let dname0 = pool.dedicated_name(0).unwrap();
        assert_eq!(
            dname0,
            MemPool::dedicated_segment_name(pool.prefix(), 0, d0.generation)
        );

        let d1 = pool.alloc(big).unwrap();
        assert!(d1.is_dedicated);
        assert_eq!(d1.seg_idx, 1);
        let dname1 = pool.dedicated_name(1).unwrap();
        assert_eq!(
            dname1,
            MemPool::dedicated_segment_name(pool.prefix(), 1, d1.generation)
        );

        unsafe { pool.free(&d0) }.unwrap();
        unsafe { pool.free(&d1) }.unwrap();
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
        let mut pool = MemPool::new_with_prefix(test_config(), prefix).unwrap();
        assert_eq!(pool.prefix().len(), 255);
        pool.ensure_buddy_segments(1).unwrap();
        assert!(pool.segment_name(0).unwrap().len() <= 30);
    }

    #[test]
    fn test_prefix_too_long_is_rejected() {
        let prefix = "x".repeat(215);
        let error = MemPool::new_with_prefix(test_config(), prefix)
            .err()
            .unwrap();
        assert!(error.contains("exceeds handshake maximum"));
    }

    #[test]
    fn test_gc_buddy_respects_decay_window() {
        // With a large decay window, idle trailing segments should NOT be reclaimed.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,

            buddy_idle_decay_secs: 3600.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        let b = pool.alloc(32 * 1024).unwrap();
        assert!(pool.segment_count() >= 2);
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
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

            buddy_idle_decay_secs: 0.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);
        let a = pool.alloc(32 * 1024).unwrap();
        let b = pool.alloc(32 * 1024).unwrap();
        let seg_before = pool.segment_count();
        assert!(seg_before >= 2);
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
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
        unsafe { pool.free(&old) }.unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        let new = pool.alloc(32 * 1024).unwrap();
        assert_eq!(new.seg_idx, old.seg_idx);
        assert_ne!(pool.segment_name(new.seg_idx as usize).unwrap(), old_name);
        assert!(!old_view.is_owner());
        assert!(new.generation > old.generation);
        assert!(pool.data_ptr(&old).is_err());
        assert!(unsafe { pool.free(&old) }.is_err());
        assert_eq!(
            pool.segment(new.seg_idx as usize)
                .unwrap()
                .allocator()
                .alloc_count(),
            1
        );
        unsafe { pool.free(&new) }.unwrap();
        unsafe { pool.free(&first) }.unwrap();
    }

    #[test]
    fn exhausted_slots_do_not_disable_other_slots_or_replace_the_pool() {
        let mut pool = test_pool(PoolConfig {
            max_segments: 3,
            max_dedicated_segments: 2,
            min_retained_segments: 0,
            buddy_idle_decay_secs: 0.0,
            ..test_config()
        });
        let prefix = pool.prefix().to_owned();
        let size = pool.config.segment_size;
        pool.generations = vec![u32::MAX, u32::MAX - 1];
        let last_generation = pool.alloc(size).unwrap();
        assert_eq!(
            (last_generation.seg_idx, last_generation.generation),
            (1, u32::MAX)
        );
        unsafe { pool.free(&last_generation) }.unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        let next_slot = pool.alloc(size).unwrap();
        assert_eq!((next_slot.seg_idx, next_slot.generation), (2, 1));
        unsafe { pool.free(&next_slot) }.unwrap();
        pool.gc_buddy();
        pool.generations[2] = u32::MAX;
        pool.dedicated_generations = vec![u32::MAX, u32::MAX - 1];
        let dedicated = pool.alloc(size).unwrap();
        assert!(dedicated.is_dedicated);
        assert_eq!((dedicated.seg_idx, dedicated.generation), (1, u32::MAX));
        unsafe { pool.free(&dedicated) }.unwrap();
        assert!(pool.try_alloc_shm(size).is_err());
        let file = pool.alloc_handle(size).unwrap();
        assert!(file.is_file_spill());
        pool.release_handle(file);
        assert_eq!(pool.prefix(), prefix);
        assert_eq!(pool.segment_count(), 0);
        assert_eq!(pool.stats().alloc_count, 0);
    }

    #[test]
    fn owner_incarnations_do_not_reuse_identical_labels() {
        let mut first = MemPool::new_with_prefix(test_config(), "same-label".into()).unwrap();
        let mut second = MemPool::new_with_prefix(test_config(), "same-label".into()).unwrap();
        first.ensure_buddy_segments(1).unwrap();
        second.ensure_buddy_segments(1).unwrap();
        assert_ne!(first.prefix(), second.prefix());
        assert_ne!(first.segment_name(0), second.segment_name(0));
    }

    #[test]
    fn opening_lower_slot_keeps_higher_peer_mapping() {
        let config = test_config();
        let mut owner = test_pool(config.clone());
        let low = owner.alloc(config.segment_size).unwrap();
        let high = owner.alloc(config.segment_size).unwrap();
        let mut peer = MemPool::open_peer(config, owner.prefix().to_owned()).unwrap();
        peer.ensure_peer_segment(high.seg_idx, high.generation, 1)
            .unwrap();
        let pointer = peer
            .data_ptr_at(high.seg_idx, high.generation, 0, false)
            .unwrap();
        unsafe {
            *owner.data_ptr(&high).unwrap() = 0x56;
        }
        peer.ensure_peer_segment(low.seg_idx, low.generation, 1)
            .unwrap();
        assert_eq!(unsafe { *pointer }, 0x56);
        assert_eq!(peer.stats().total_segments, 2);
        assert_eq!(peer.stats().alloc_count, 0);
        assert!(
            unsafe { peer.free_at(low.seg_idx, low.generation, low.offset, low.level, false) }
                .is_err()
        );
        unsafe { owner.free(&low) }.unwrap();
        unsafe { owner.free(&high) }.unwrap();
    }

    #[test]
    fn backing_names_preserve_sha256_and_base64_golden_values() {
        let prefix = "label_1234567800112233445566778899aabbccddeeff";
        assert_eq!(
            MemPool::buddy_segment_name(prefix, 2, 3),
            "/c2eFY0Egs27_wcABd3HcmKqqDh6p"
        );
        assert_eq!(
            MemPool::dedicated_segment_name(prefix, 2, 3),
            "/c2eFY0Egs4muW0QynaoG_cr5ZUTU"
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
        assert_eq!(pool.segment_count(), 0);
        assert!(
            pool.ensure_buddy_segments(1)
                .unwrap_err()
                .contains("buddy pool is disabled")
        );
        assert_eq!(pool.segment_count(), 0);

        unsafe { pool.free(&a) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();

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
        assert!(unsafe { pool.free(&a) }.is_err());

        unsafe { pool.free(&b) }.unwrap();
    }

    #[test]
    fn gc_buddy_retires_only_down_to_configured_min() {
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 0,

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
            unsafe { pool.free(a) }.unwrap();
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
        unsafe { pool.free(&filler) }.unwrap();
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

        unsafe { pool.free(&held) }.unwrap();
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
        // A payload whose page-aligned region exceeds
        // u32::MAX must be rejected before any mapping is created.
        let oversized = u32::MAX as usize;
        let err = pool.alloc(oversized).unwrap_err();
        assert!(err.contains("wire backing range"), "got: {err}");
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

            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0, // disable spill
            spill_dir: std::env::temp_dir().join("c2_pool_handle_test"),
            ..PoolConfig::default()
        }
    }

    #[test]
    fn test_alloc_handle_backing_kind_and_length() {
        for (size, is_buddy) in [(4096, true), (128 * 1024, false)] {
            let mut pool = MemPool::new(test_config()).unwrap();
            let handle = pool.alloc_handle(size).unwrap();
            assert_eq!(handle.is_buddy(), is_buddy);
            assert_eq!(handle.is_dedicated(), !is_buddy);
            assert_eq!(handle.len(), size);
            pool.release_handle(handle);
            assert_eq!(pool.stats().alloc_count, 0);
        }
    }

    #[test]
    fn test_alloc_handle_file_spill_forced() {
        let dir = std::env::temp_dir().join("c2_pool_spill_test");
        let config = PoolConfig {
            spill_threshold: 0.0,
            spill_dir: dir.clone(),
            ..test_config()
        };
        let mut pool = MemPool::new(config).unwrap();
        let handle = pool.alloc_handle(4096).unwrap();
        assert!(handle.is_file_spill());
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_slice_write_read() {
        let mut pool = MemPool::new(test_config()).unwrap();
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
        let mut pool = MemPool::new(config).unwrap();
        let mut handle = pool.alloc_handle(8192).unwrap();
        let pattern = b"spill_pattern_data";
        pool.handle_slice_mut(&mut handle)[..pattern.len()].copy_from_slice(pattern);
        assert_eq!(&pool.handle_slice(&handle)[..pattern.len()], pattern);
        assert_eq!(
            &pool.copy_handle_data(&handle).unwrap()[..pattern.len()],
            pattern
        );
        assert!(handle.trim_to(8193).unwrap_err().contains("cannot grow"));
        assert_eq!(handle.len(), 8192);
        assert_eq!(
            &pool.copy_handle_data(&handle).unwrap()[..pattern.len()],
            pattern
        );
        pool.release_handle(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_access_rejects_foreign_and_peer_pools() {
        for size in [4096, 128 * 1024] {
            let mut owner = MemPool::new(test_config()).unwrap();
            let mut handle = owner.alloc_shm_handle(size).unwrap();
            owner.handle_slice_mut(&mut handle)[0] = 0x11;
            let mut other = MemPool::new(test_config()).unwrap();
            let mut survivor = other.alloc_shm_handle(size).unwrap();
            other.handle_slice_mut(&mut survivor)[0] = 0x22;
            let peer = MemPool::open_peer(test_config(), owner.prefix().to_owned()).unwrap();
            assert!(peer.validate_handle(&handle).is_err());
            assert!(other.copy_handle_data(&handle).is_err());
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    other.handle_slice_mut(&mut handle)[0] = 0x33;
                }))
                .is_err()
            );
            assert_eq!(owner.handle_slice(&handle)[0], 0x11);
            assert_eq!(other.handle_slice(&survivor)[0], 0x22);
            owner.release_handle(handle);
            assert_eq!(owner.stats().alloc_count, 0);
            other.release_handle(survivor);
        }
    }

    fn local_handle(owner: PoolIncarnation) -> MemHandle {
        MemHandle {
            backing: HandleBacking::Shm {
                allocation: PoolAllocation {
                    seg_idx: 0,
                    generation: 1,
                    offset: 0,
                    actual_size: 4096,
                    level: 0,
                    is_dedicated: false,
                },
                owner,
                len: 4096,
            },
        }
    }

    #[test]
    #[should_panic(expected = "pool owner incarnation mismatch")]
    fn release_handle_panics_on_wrong_pool() {
        let owner = MemPool::new(test_config()).unwrap();
        let handle = local_handle(owner.incarnation);
        let mut other = MemPool::new(test_config()).unwrap();
        other.release_handle(handle);
    }

    #[test]
    #[should_panic(expected = "stale or unavailable shared-memory backing generation")]
    fn release_handle_panics_on_stale_backing_identity() {
        let mut pool = MemPool::new(test_config()).unwrap();
        // A private handle naming an absent backing simulates a violated
        // local ownership invariant; the test never touches payload memory.
        let handle = local_handle(pool.incarnation);
        pool.release_handle(handle);
    }

    #[test]
    fn trimmed_handles_release_the_original_allocation() {
        for length in [0, 1, 4096] {
            let mut pool = MemPool::new(test_config()).unwrap();
            let initial = 32 * 1024;
            let mut handle = pool.try_alloc_shm(initial).unwrap();
            let capacity = pool.segment(0).unwrap().allocator().data_size() as u64;
            handle.trim_to(length).unwrap();
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
    /// dedicated backing is rounded to the platform's real page size.
    fn budget_config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,

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
        MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget).unwrap()
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
            pool.ensure_buddy_segments(1)
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

        // Buddy backing contains only the rounded payload region.
        let block = pool.alloc(4096).unwrap();
        assert!(!block.is_dedicated);
        assert_eq!(budget.snapshot().shm.used_bytes, buddy_backing(&config));

        // Dedicated: page-aligned payload, exactly the mapped size.
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
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
        let reused = pool.alloc(4096).unwrap();
        assert_eq!(pool.segment_count(), 1);
        assert_eq!(budget.snapshot().shm.used_bytes, backing);
        assert_eq!(budget.snapshot().shm.peak_bytes, backing);
        unsafe { pool.free(&reused) }.unwrap();
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
        unsafe { pool_a.free(&a) }.unwrap();
        unsafe { pool_b.free(&b) }.unwrap();

        // Existing owner constructors carry distinct private canonical budgets.
        let mut private_a = MemPool::new_with_prefix(config.clone(), unique_prefix()).unwrap();
        let mut private_b = MemPool::new_with_prefix(config.clone(), unique_prefix()).unwrap();
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
        unsafe { private_a.free(&a) }.unwrap();
        unsafe { private_b.free(&b) }.unwrap();

        // Peer pools never charge owner-creation budget.
        let peer = MemPool::open_peer(config, unique_prefix()).unwrap();
        assert!(peer.budget().is_none());
    }

    #[test]
    fn owner_constructors_carry_canonical_private_limits() {
        let config = budget_config();
        for pool in [
            MemPool::new(config.clone()).unwrap(),
            MemPool::new_with_prefix(config.clone(), unique_prefix()).unwrap(),
        ] {
            let budget = pool.budget().expect("owner pools carry a budget");
            let snap = budget.snapshot();
            assert_eq!(snap.shm.limit_bytes, 8 * 1024 * 1024 * 1024);
            assert_eq!(snap.file.limit_bytes, 16 * 1024 * 1024 * 1024);
            assert_eq!(snap.reassembly.limit_bytes, 8 * 1024 * 1024 * 1024);
        }
    }

    #[test]
    fn dedicated_release_returns_backing_charge_immediately() {
        let backing = DedicatedSegment::required_shm_size(20_000).unwrap() as u64;
        let budget = MemoryBudget::new(backing, 0, 0);
        let mut pool = budget_pool(budget_config(), budget.clone());
        let handle = pool.alloc_shm_handle(20_000).unwrap();
        assert!(handle.is_dedicated());
        assert_eq!(budget.snapshot().shm.used_bytes, backing);
        pool.release_handle(handle);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
        assert_eq!(pool.stats().dedicated_segments, 0);
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
        handle.trim_to(4).unwrap();
        assert_eq!(budget.snapshot().file.used_bytes, 8192);

        // Move the handle, then drop the pool entirely.
        let moved = handle;
        drop(pool);
        assert_eq!(
            budget.snapshot().file.used_bytes,
            8192,
            "file charge must persist while the mapping is live"
        );

        // File spill owns its mapping, independently of either owner or peer pool identity.
        let mut peer = MemPool::open_peer(budget_config(), "unused-peer-prefix".into()).unwrap();
        assert_eq!(peer.copy_handle_data(&moved).unwrap(), b"hold");
        peer.release_handle(moved);
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
        let mut config = budget_config();
        let dedicated_backing = DedicatedSegment::required_shm_size(4096).unwrap() as u64;
        config.segment_size = dedicated_backing as usize * 2;
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
        unsafe { pool.free(&alloc) }.unwrap();
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
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
        assert_eq!(pool.gc_buddy(), 1);
        assert_eq!(budget.snapshot().shm.used_bytes, backing);

        // The last owner dropping the pool also releases retained backings.
        drop(pool);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);

        // Dedicated release refunds immediately; a file handle can outlive
        // the pool itself and stays charged until its mapping drops.
        let mut pool = budget_pool(config.clone(), budget.clone());
        let d = pool.alloc(20000).unwrap();
        let dedicated_backing = DedicatedSegment::required_shm_size(20000).unwrap() as u64;
        assert_eq!(budget.snapshot().shm.used_bytes, dedicated_backing);
        unsafe { pool.free(&d) }.unwrap();
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
    #[cfg(target_pointer_width = "64")]
    fn invalid_buddy_geometry_cannot_fall_back_to_dedicated() {
        for segment_size in [(1usize << 31) + 4096, (1usize << 32) + 4096] {
            let mut config = geometry_config();
            config.segment_size = segment_size;
            let result = MemPool::new_with_prefix(config, unique_prefix()).err();
            assert!(
                result
                    .as_ref()
                    .is_some_and(|error| error.contains("buddy backing geometry unsupported")),
                "invalid buddy geometry must fail before selecting another backing: {result:?}"
            );
        }
    }

    #[test]
    fn unsupported_buddy_geometry_rejects_before_any_mapping() {
        // Construction rejects before taking a budget charge or creating a
        // named region.
        let mut sizes = vec![usize::MAX];
        if usize::BITS >= 64 {
            sizes.extend([
                (1usize << 63) + 1,
                (1usize << 32) + 12345,
                (1usize << 31) + 12345,
            ]);
        }
        for segment_size in sizes {
            let mut config = geometry_config();
            config.segment_size = segment_size;
            let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
            let error =
                MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone())
                    .err()
                    .unwrap();
            assert!(
                error.contains("buddy backing geometry unsupported"),
                "segment_size {segment_size}: {error}"
            );
            // No charge was taken and no budget rejection was recorded.
            let snap = budget.snapshot();
            assert_eq!(snap.shm.used_bytes, 0, "segment_size {segment_size}");
            assert_eq!(
                snap.shm.rejected_allocations, 0,
                "segment_size {segment_size}"
            );
        }
    }

    #[test]
    fn owner_backing_names_disappear_after_pool_teardown() {
        // OS-level probe on this platform: creator teardown must actually
        // remove the named POSIX SHM objects, not just the pool's containers.
        let config = geometry_config();
        let budget = MemoryBudget::from_limits(&c2_config::MemoryBudgetLimits::default());
        let mut pool =
            MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone()).unwrap();

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

    /// Small buddy data region with the pressure heuristic active. Tests
    /// comparing tiers size their own buddy backing above a native page.
    fn pressure_config() -> PoolConfig {
        PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,

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
        let mut config = pressure_config();
        config.segment_size = DedicatedSegment::required_shm_size(4096).unwrap() * 2;
        let budget = default_budget();
        let mut pool =
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone())
                .unwrap();
        let buddy = buddy_total(&config);
        let dedicated = dedicated_backing(4096);
        assert!(
            dedicated < buddy,
            "test geometry needs a smaller dedicated tier"
        );
        // Availability admits one page-rounded dedicated backing at the
        // 50% threshold, while refusing the larger buddy backing.
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
        unsafe { pool.free(&alloc) }.unwrap();
    }

    #[test]
    fn entrypoints_agree_on_backing_policy_under_pressure() {
        let mut config = pressure_config();
        let dedicated = dedicated_backing(4096);
        config.segment_size = dedicated as usize * 2;

        // Pressure denies the buddy segment but a dedicated backing fits:
        // every entry point selects dedicated, none errors or spills.
        let budget = default_budget();
        let mut pool =
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget).unwrap();
        Script::install(&mut pool, 2 * dedicated);
        assert!(
            pool.ensure_buddy_segments(1)
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
        unsafe { pool.free(&a) }.unwrap();
        pool.release_handle(h);
        pool.release_handle(s);

        // Forced threshold zero: SHM creation always denied. alloc_handle
        // takes file backing; SHM-only entry points return capacity errors.
        let mut forced_config = config;
        forced_config.spill_threshold = 0.0;
        let mut forced =
            MemPool::new_with_prefix_and_budget(forced_config, unique_prefix(), default_budget())
                .unwrap();
        Script::install(&mut forced, u64::MAX);
        assert!(
            forced
                .ensure_buddy_segments(1)
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
        // ensure_buddy_segments(1) + alloc(buddy+dedicated) + try_alloc_shm(2) +
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
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone())
                .unwrap();

        // Ample availability maps one buddy segment holding two blocks.
        Script::install(&mut pool, 1 << 40);
        let a = pool.alloc(4096).unwrap();
        let b = pool.alloc(4096).unwrap();
        assert!(!a.is_dedicated && !b.is_dedicated);
        unsafe { pool.free(&a) }.unwrap();
        unsafe { pool.free(&b) }.unwrap();
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
        unsafe { pool.free(&reused) }.unwrap();
    }

    #[test]
    fn cached_observation_bounds_os_sampling_at_the_seams() {
        let mut pool = MemPool::new_with_prefix_and_budget(
            pressure_config(),
            unique_prefix(),
            default_budget(),
        )
        .unwrap();
        // Zero availability denies every creation seam deterministically.
        let script = Script::install(&mut pool, 0);

        for _ in 0..5 {
            assert!(
                pool.ensure_buddy_segments(1)
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
            pool.ensure_buddy_segments(1)
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
            MemPool::new_with_prefix_and_budget(config, unique_prefix(), default_budget()).unwrap();
        // Direct bar: buddy needs 2*buddy observed bytes at threshold 0.5.
        let script = Script::install(&mut pool, 2 * buddy - 1);
        assert!(
            pool.ensure_buddy_segments(1)
                .unwrap_err()
                .contains("OS memory pressure")
        );

        // Just past the direct bar but inside the recovery band (which needs
        // 2.5*buddy): without hysteresis this would flip to admitting.
        script.advance_ms(1_001);
        script.set_available(2 * buddy + 2);
        assert!(
            pool.ensure_buddy_segments(1)
                .unwrap_err()
                .contains("OS memory pressure"),
            "availability inside the recovery band must stay denied"
        );
        assert_eq!(pool.segment_count(), 0);

        // Genuine recovery admits the same tier again.
        script.advance_ms(1_001);
        script.set_available(3 * buddy);
        pool.ensure_buddy_segments(1).unwrap();
        assert_eq!(pool.segment_count(), 1);
    }

    #[test]
    fn pressure_denied_shm_escapes_to_file_backing() {
        let config = pressure_config();
        let budget = MemoryBudget::new(u64::MAX, u64::MAX, u64::MAX);
        let mut pool =
            MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget.clone()).unwrap();
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
        let mut pool =
            MemPool::new_with_prefix_and_budget(config, unique_prefix(), budget).unwrap();
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
            MemPool::new_with_prefix_and_budget(config.clone(), unique_prefix(), budget.clone())
                .unwrap();
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
        unsafe { pool.free(&block) }.unwrap();
        assert_eq!(pool.stats().utilization_ratio, 0.0);

        let reused = pool.alloc(4096).unwrap();
        assert_eq!(pool.stats().buddy_reused_allocs, 1);
        unsafe { pool.free(&reused) }.unwrap();

        // Dedicated capacity remains charged until its local owner releases.
        let dedicated_alloc = pool.alloc(20000).unwrap();
        assert!(dedicated_alloc.is_dedicated);
        let backing = dedicated_backing(20000);
        stats = pool.stats();
        assert_eq!(stats.dedicated_allocs, 1);
        assert_eq!(stats.dedicated_active_count, 1);
        assert_eq!(stats.dedicated_active_bytes, backing);
        assert_eq!(stats.dedicated_mapped_bytes, backing);
        assert_eq!(stats.alloc_count, 1);

        unsafe { pool.free(&dedicated_alloc) }.unwrap();
        stats = pool.stats();
        assert_eq!(stats.dedicated_active_count, 0);
        assert_eq!(stats.dedicated_active_bytes, 0);
        assert_eq!(stats.dedicated_mapped_bytes, 0);
        assert_eq!(stats.alloc_count, 0);

        // File tier through the forced-threshold route.
        let mut forced_config = config;
        forced_config.spill_threshold = 0.0;
        let spill_dir =
            std::env::temp_dir().join(format!("c2_pressure_count_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&spill_dir);
        forced_config.spill_dir = spill_dir.clone();
        let mut spiller =
            MemPool::new_with_prefix_and_budget(forced_config, unique_prefix(), budget.clone())
                .unwrap();
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
