//! Pool configuration, allocation results, and statistics.

pub use c2_config::PoolConfig;

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
