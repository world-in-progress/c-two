//! Chunk lifecycle configuration.

use std::time::Duration;

/// Configuration for the [`ChunkRegistry`](super::ChunkRegistry).
///
/// Typically derived from [`c2_config::BaseIpcConfig`] chunk-related fields.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Timeout for incomplete input assemblies (default 60s). Active replies do not expire.
    pub assembler_timeout: Duration,
    /// GC sweep interval (default 5s). Callers use this to drive their timer.
    pub gc_interval: Duration,
    /// Early GC threshold derived from `max_inflight_chunk_frames`; it never
    /// refuses a reassembly by count.
    pub soft_limit: u32,
    /// Soft limit on in-flight reassembly bytes and hard limit per assembly (default 8 GB).
    pub max_reassembly_bytes: u64,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self::from_base(&c2_config::BaseIpcConfig::default())
    }
}

impl ChunkConfig {
    /// Build a `ChunkConfig` from a [`BaseIpcConfig`](c2_config::BaseIpcConfig).
    pub fn from_base(cfg: &c2_config::BaseIpcConfig) -> Self {
        Self {
            assembler_timeout: Duration::from_secs_f64(cfg.chunk_assembler_timeout_secs),
            gc_interval: Duration::from_secs_f64(cfg.chunk_gc_interval_secs),
            soft_limit: cfg.max_inflight_chunk_frames,
            max_reassembly_bytes: cfg.max_reassembly_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_sane() {
        let cfg = ChunkConfig::default();
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
    }

    #[test]
    fn from_base_config() {
        let base = c2_config::BaseIpcConfig::default();
        let cfg = ChunkConfig::from_base(&base);
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
        assert_eq!(cfg.max_reassembly_bytes, 8_589_934_592);
        let base = c2_config::BaseIpcConfig {
            max_inflight_chunk_frames: 7,
            ..base
        };
        assert_eq!(ChunkConfig::from_base(&base).soft_limit, 7);
    }
}
