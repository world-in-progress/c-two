//! Chunk lifecycle configuration.

use std::time::Duration;

/// Configuration for the [`ChunkRegistry`](super::ChunkRegistry).
///
/// Typically derived from [`c2_config::IpcConfig`] chunk-related fields.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Timeout for incomplete assemblies (default 60s).
    pub assembler_timeout: Duration,
    /// GC sweep interval (default 5s). Callers use this to drive their timer.
    pub gc_interval: Duration,
    /// Soft limit on concurrent in-flight assemblies (default 512).
    pub soft_limit: u32,
    /// Soft limit on total reassembly bytes (default 8 GB).
    pub max_reassembly_bytes: u64,
    /// Per-assembler: max chunks allowed (passed to ChunkAssembler::new).
    pub max_chunks_per_request: usize,
    /// Per-assembler: max bytes allowed (passed to ChunkAssembler::new).
    pub max_bytes_per_request: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            assembler_timeout: Duration::from_secs(60),
            gc_interval: Duration::from_secs(5),
            soft_limit: 512,
            max_reassembly_bytes: 8_589_934_592, // 8 GB
            max_chunks_per_request: 512,
            max_bytes_per_request: 8 * (1 << 30), // 8 GB
        }
    }
}

impl ChunkConfig {
    /// Build a `ChunkConfig` from an [`IpcConfig`](c2_config::IpcConfig).
    pub fn from_ipc(cfg: &c2_config::IpcConfig) -> Self {
        Self {
            assembler_timeout: Duration::from_secs_f64(cfg.chunk_assembler_timeout),
            gc_interval: Duration::from_secs_f64(cfg.chunk_gc_interval),
            soft_limit: cfg.max_total_chunks,
            max_reassembly_bytes: cfg.max_reassembly_bytes,
            max_chunks_per_request: cfg.max_total_chunks as usize,
            max_bytes_per_request: cfg.max_reassembly_bytes as usize,
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
    fn from_ipc_config() {
        let ipc = c2_config::IpcConfig::default();
        let cfg = ChunkConfig::from_ipc(&ipc);
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
        assert_eq!(cfg.max_reassembly_bytes, 8_589_934_592);
    }
}
