//! Pool configuration, allocation results, and statistics.

/// Configuration for the memory pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Size of each buddy segment (default 256 MB).
    pub segment_size: usize,
    /// Minimum allocation block (default 4 KB).
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
