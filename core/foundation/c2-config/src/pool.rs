//! Memory pool configuration.

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
    /// Crash-recovery timeout for dedicated segments (seconds).
    /// Normal GC uses SHM read_done flag; this is a safety net for peer crashes.
    pub dedicated_crash_timeout_secs: f64,
    /// Idle decay window for buddy segments (seconds). A buddy segment whose
    /// `alloc_count` has been zero for at least this duration becomes eligible
    /// for reclamation by `gc_buddy()`. Only trailing idle segments are popped.
    pub buddy_idle_decay_secs: f64,
    /// Spill threshold ratio: when `requested > available_ram * threshold`,
    /// use file-backed mmap.  Default 0.8 (80%).
    pub spill_threshold: f64,
    /// Directory for spill files, beneath the platform's temporary directory.
    pub spill_dir: std::path::PathBuf,
    /// Whether buddy reuse and expansion are allowed. When `false`, every
    /// allocation API skips the buddy tiers entirely — including reuse of any
    /// already-cached segments — and goes straight to dedicated SHM (or the
    /// file-spill fallback where the API has one). Dedicated segments, chunked
    /// transfer, and file spill stay available; only the buddy tiers are
    /// policy-disabled. Peer (receive-side) caches ignore this flag because
    /// they never allocate.
    pub buddy_enabled: bool,
    /// Minimum number of trailing buddy segments `gc_buddy()` must retain even
    /// when they are fully idle (default 1 for direct native users). IPC
    /// transports project their own `pool_min_retained_segments` setting here,
    /// whose default 0 allows idle pools to retire back to zero mappings.
    /// Generation counters, live allocations, and peer safety are unaffected.
    pub min_retained_segments: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            segment_size: 256 * 1024 * 1024,
            min_block_size: 4096,
            max_segments: 8,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 60.0,
            buddy_idle_decay_secs: 60.0,
            spill_threshold: 0.8,
            spill_dir: default_spill_dir(),
            buddy_enabled: true,
            min_retained_segments: 1,
        }
    }
}

/// The single native default used by pool configuration and SDK projections.
pub fn default_spill_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("c_two_spill")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_default_uses_platform_temporary_directory() {
        assert_eq!(PoolConfig::default().spill_dir, std::env::temp_dir().join("c_two_spill"));
    }

    #[test]
    fn pool_default_enables_buddy_and_retains_one_segment() {
        let config = PoolConfig::default();
        assert!(config.buddy_enabled);
        assert_eq!(config.min_retained_segments, 1);
    }
}
