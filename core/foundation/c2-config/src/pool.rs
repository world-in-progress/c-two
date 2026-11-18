//! Pool geometry derived from role-domain memory capacity.

use crate::DEFAULT_MEMORY_BUDGET_BYTES;
use std::path::PathBuf;

pub const MIN_BUDDY_SEGMENT_BYTES: u64 = 64 * 1024;
pub const MAX_BUDDY_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
pub const PRIMARY_MAX_POOL_SEGMENTS: usize = 16;
pub const REASSEMBLY_MAX_POOL_SEGMENTS: usize = 4;
/// The peer cache accepts buddy indices below this bound.
pub const MAX_IPC_POOL_SEGMENTS: u32 = 255;
/// Dedicated mappings can use every wire slot.
pub const MAX_DEDICATED_SEGMENTS: usize = u16::MAX as usize + 1;
pub const POOL_DECAY_SECONDS: f64 = 60.0;
pub const SPILL_THRESHOLD: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Whether the buddy pool can reuse and create segments.
    pub pool_enabled: bool,
    pub segment_size: usize,
    pub max_segments: usize,
    pub max_dedicated_segments: usize,
    /// A fraction of available OS memory. Values >= 1 bypass the heuristic.
    pub spill_threshold: f64,
    pub spill_dir: PathBuf,
    pub prewarm_segments: usize,
    pub min_retained_segments: usize,
    pub buddy_idle_decay_secs: f64,
}

impl PoolConfig {
    pub fn primary_for_budget(memory_budget_bytes: u64) -> Self {
        Self::for_budget_share(memory_budget_bytes / 4, PRIMARY_MAX_POOL_SEGMENTS)
    }

    pub fn reassembly_for_budget(memory_budget_bytes: u64) -> Self {
        Self::for_budget_share(memory_budget_bytes / 16, REASSEMBLY_MAX_POOL_SEGMENTS)
    }

    pub fn peer_cache_config() -> Self {
        Self {
            max_segments: MAX_IPC_POOL_SEGMENTS as usize,
            ..Self::default()
        }
    }

    fn for_budget_share(share: u64, segment_cap: usize) -> Self {
        let pool_enabled = share >= MIN_BUDDY_SEGMENT_BYTES;
        let max_segments = (share / MIN_BUDDY_SEGMENT_BYTES).clamp(1, segment_cap as u64);
        let segment_size = if pool_enabled {
            let per_segment = share / max_segments;
            (1u64 << per_segment.ilog2()).min(MAX_BUDDY_SEGMENT_BYTES)
        } else {
            MIN_BUDDY_SEGMENT_BYTES
        };
        Self {
            pool_enabled,
            segment_size: segment_size as usize,
            max_segments: max_segments as usize,
            max_dedicated_segments: MAX_DEDICATED_SEGMENTS,
            spill_threshold: SPILL_THRESHOLD,
            spill_dir: default_spill_dir(),
            prewarm_segments: 0,
            min_retained_segments: 0,
            buddy_idle_decay_secs: POOL_DECAY_SECONDS,
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::primary_for_budget(DEFAULT_MEMORY_BUDGET_BYTES)
    }
}

pub fn default_spill_dir() -> PathBuf {
    std::env::temp_dir().join("c_two_spill")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_crosses_pool_and_segment_boundaries() {
        let kib = 1024;
        let mib = kib * kib;
        for (budget, expected) in [
            (256 * kib - 1, (false, 64 * kib, 1)),
            (256 * kib, (true, 64 * kib, 1)),
            (mib, (true, 64 * kib, 4)),
            (4 * mib - 1, (true, 64 * kib, 15)),
            (4 * mib, (true, 64 * kib, 16)),
            (8 * mib, (true, 128 * kib, 16)),
            (DEFAULT_MEMORY_BUDGET_BYTES, (true, 64 * mib, 16)),
        ] {
            let config = PoolConfig::primary_for_budget(budget);
            assert_eq!(
                (
                    config.pool_enabled,
                    config.segment_size as u64,
                    config.max_segments
                ),
                expected
            );
        }
        for (budget, expected) in [
            (mib - 1, (false, 64 * kib, 1)),
            (mib, (true, 64 * kib, 1)),
            (2 * mib, (true, 64 * kib, 2)),
            (4 * mib, (true, 64 * kib, 4)),
            (DEFAULT_MEMORY_BUDGET_BYTES, (true, 64 * mib, 4)),
        ] {
            let config = PoolConfig::reassembly_for_budget(budget);
            assert_eq!(
                (
                    config.pool_enabled,
                    config.segment_size as u64,
                    config.max_segments
                ),
                expected
            );
        }
    }

    #[test]
    fn enabled_geometry_fits_its_share_at_rounding_boundaries() {
        // Each share is monotonic between consecutive segment-count or size transitions.
        for budget in (1..=16)
            .flat_map(|count| {
                (16..=58).flat_map(move |shift| {
                    let boundary = count * (1u64 << shift);
                    [boundary - 1, boundary, boundary + 1]
                })
            })
            .chain([0, u64::MAX])
        {
            for (config, share) in [
                (PoolConfig::primary_for_budget(budget), budget / 4),
                (PoolConfig::reassembly_for_budget(budget), budget / 16),
            ] {
                if config.pool_enabled {
                    assert!(config.segment_size.is_power_of_two());
                    assert!(
                        (MIN_BUDDY_SEGMENT_BYTES..=MAX_BUDDY_SEGMENT_BYTES)
                            .contains(&(config.segment_size as u64))
                    );
                    assert!((config.segment_size as u64) * (config.max_segments as u64) <= share);
                }
            }
        }
        let peer = PoolConfig::peer_cache_config();
        assert_eq!(peer.max_segments, 255);
        assert_eq!(peer.max_dedicated_segments, 65_536);
        assert_eq!(peer.buddy_idle_decay_secs, 60.0);
    }
}
