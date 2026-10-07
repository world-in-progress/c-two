//! Pool configuration, allocation results, and statistics.

pub use c2_config::PoolConfig;

/// Result of a pool allocation.
#[derive(Debug, Clone, Copy)]
pub struct PoolAllocation {
    /// Index of the segment (buddy or dedicated).
    pub seg_idx: u32,
    /// Concrete backing incarnation within this segment slot; dedicated is 0.
    pub generation: u32,
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
///
/// Scope notes: byte fields describe this pool's own backing capacities,
/// never whole-process RSS or the OS commit charge. Buddy fields cover the
/// data region only (allocator metadata is excluded; the mapped backing is
/// larger and charged to the owner's memory budget). Dedicated fields count
/// mapped capacity including freed-but-pending-GC entries, which stay mapped
/// until the peer signals `read_done` or the crash timeout fires. File and
/// live-reassembly bytes belong to the budget cells, not to these fields.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Live buddy segments (mapped buddy backings).
    pub total_segments: usize,
    /// Dedicated entries, including freed-but-pending-GC ones.
    pub dedicated_segments: usize,
    /// Active allocations: live buddy blocks plus dedicated entries that are
    /// not pending-GC.
    pub alloc_count: u32,
    /// Sum of buddy data-region capacity across live segments.
    pub buddy_data_bytes: u64,
    /// Buddy data capacity currently backing live allocations.
    pub buddy_occupied_bytes: u64,
    /// Buddy data capacity free for reuse without any new mapping.
    pub buddy_idle_bytes: u64,
    /// Dedicated mapped capacity, including pending-free entries.
    pub dedicated_mapped_bytes: u64,
    /// Dedicated entries actively backing allocations (not pending-free).
    pub dedicated_active_count: usize,
    /// Mapped capacity of active dedicated entries.
    pub dedicated_active_bytes: u64,
    /// Mapped capacity of freed-but-pending-GC dedicated entries.
    pub dedicated_pending_free_bytes: u64,
    /// Successful allocations served from an already-mapped buddy segment.
    pub buddy_reused_allocs: u64,
    /// Successful allocations served by a newly created buddy segment.
    pub buddy_expanded_allocs: u64,
    /// Successful dedicated backing allocations.
    pub dedicated_allocs: u64,
    /// Successful file-spill allocations.
    pub file_spill_allocs: u64,
    /// Backing creations denied by the OS-memory pressure policy.
    pub pressure_denied_backings: u64,
    /// Occupied share of buddy data capacity (`0.0` when none is mapped).
    /// This is utilization of the buddy data region — not external
    /// fragmentation, and not a whole-process RSS claim. Dedicated and file
    /// storage are reported as absolute bytes above and through the budget
    /// snapshot.
    pub utilization_ratio: f64,
}
