//! Chunk lifecycle configuration.

use std::time::Duration;

/// Configuration for the [`ChunkRegistry`](super::ChunkRegistry).
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Timeout for incomplete input assemblies (default 60s). Active replies do not expire.
    pub assembler_timeout: Duration,
    /// GC sweep interval (default 5s). Callers use this to drive their timer.
    pub gc_interval: Duration,
    /// Early GC scan soft threshold; it never refuses a reassembly by count.
    pub soft_limit: u32,
    /// Per-assembly byte cap. The server passes its own max_payload_size;
    /// clients additionally validate each reply against the route-advertised
    /// max_payload_size at insert_reply time.
    pub max_payload_size: u64,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            assembler_timeout: c2_config::CHUNK_ASSEMBLER_TIMEOUT,
            gc_interval: c2_config::CHUNK_GC_INTERVAL,
            soft_limit: 512,
            max_payload_size: c2_config::DEFAULT_MAX_PAYLOAD_SIZE,
        }
    }
}

impl ChunkConfig {
    /// Defaults with the given per-assembly byte cap (a server's
    /// max_payload_size or a route-advertised reply limit).
    pub fn with_max_payload_size(max_payload_size: u64) -> Self {
        Self {
            max_payload_size: max_payload_size,
            ..Self::default()
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
        assert_eq!(cfg.max_payload_size, 16 * (1 << 30));
    }

    #[test]
    fn with_max_payload_size_keeps_the_default_durations() {
        let cfg = ChunkConfig::with_max_payload_size(1024);
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
        assert_eq!(cfg.max_payload_size, 1024);
    }
}
