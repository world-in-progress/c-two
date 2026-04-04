//! Unified IPC transport configuration.
//!
//! Single source of truth for both server and client sides.
//! Mirrors the Python `IPCConfig` dataclass from `c_two.transport.ipc.frame`.
//! All size fields are in bytes; time fields are in seconds.

/// Configuration for the IPC transport layer.
#[derive(Debug, Clone)]
pub struct IpcConfig {
    // ── Transport limits ─────────────────────────────────────────────────
    /// Payload size cutover from inline to SHM (default 4 KB).
    pub shm_threshold: u64,
    /// Maximum inline frame size (default 2 GB).
    pub max_frame_size: u64,
    /// Maximum SHM payload size (default 16 GB).
    pub max_payload_size: u64,
    /// Server-side concurrent request limit (default 1024).
    pub max_pending_requests: u32,

    // ── Pool SHM settings ────────────────────────────────────────────────
    /// Enable pre-allocated pool SHM (default true).
    pub pool_enabled: bool,
    /// Idle seconds before pool teardown (default 60.0).
    pub pool_decay_seconds: f64,
    /// Memory budget per pool direction (default 1 GB).
    pub max_pool_memory: u64,
    /// Max number of pool segments, 1–255 (default 4).
    pub max_pool_segments: u32,
    /// Size of each pool SHM segment (default 256 MB).
    pub pool_segment_size: u64,

    // ── Reassembly pool settings ─────────────────────────────────────────
    /// Size of reassembly pool segment (default 64 MB).
    pub reassembly_segment_size: u64,
    /// Max reassembly pool segments, 1–255 (default 4).
    pub reassembly_max_segments: u32,

    // ── Chunked transfer settings ────────────────────────────────────────
    /// Max concurrent in-flight chunked transfers (default 512).
    pub max_total_chunks: u32,
    /// GC sweep interval in seconds (default 5.0).
    pub chunk_gc_interval: f64,
    /// Ratio of pool capacity triggering chunked transfer (default 0.9).
    pub chunk_threshold_ratio: f64,
    /// Timeout for incomplete chunked assemblies in seconds (default 60.0).
    pub chunk_assembler_timeout: f64,
    /// Max total reassembly bytes across all in-flight chunks (default 8 GB).
    pub max_reassembly_bytes: u64,
    /// Chunk size for chunked transfer (default 128 KB).
    pub chunk_size: usize,

    // ── Heartbeat settings ───────────────────────────────────────────────
    /// Seconds between PING probes; ≤0 disables heartbeat (default 15.0).
    pub heartbeat_interval: f64,
    /// Seconds with no activity before declaring connection dead (default 30.0).
    pub heartbeat_timeout: f64,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            shm_threshold: 4_096,
            max_frame_size: 2_147_483_648,
            max_payload_size: 17_179_869_184,
            max_pending_requests: 1024,

            pool_enabled: true,
            pool_decay_seconds: 60.0,
            max_pool_memory: 1_073_741_824,
            max_pool_segments: 4,
            pool_segment_size: 268_435_456,

            reassembly_segment_size: 64 * 1024 * 1024,
            reassembly_max_segments: 4,

            max_total_chunks: 512,
            chunk_gc_interval: 5.0,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout: 60.0,
            max_reassembly_bytes: 8_589_934_592,
            chunk_size: 131_072,

            heartbeat_interval: 15.0,
            heartbeat_timeout: 30.0,
        }
    }
}

impl IpcConfig {
    /// Validate configuration invariants.
    pub fn validate(&self) -> Result<(), String> {
        if self.pool_segment_size > u32::MAX as u64 {
            return Err(format!(
                "pool_segment_size ({}) must be <= u32::MAX ({}) for wire format compatibility",
                self.pool_segment_size,
                u32::MAX,
            ));
        }
        if self.pool_segment_size == 0 {
            return Err("pool_segment_size must be > 0".into());
        }
        if self.max_frame_size <= 16 {
            return Err(format!(
                "max_frame_size ({}) must be > 16 (header size)",
                self.max_frame_size,
            ));
        }
        if self.max_payload_size == 0 {
            return Err("max_payload_size must be > 0".into());
        }
        if self.shm_threshold > self.max_frame_size {
            return Err(format!(
                "shm_threshold ({}) must not exceed max_frame_size ({})",
                self.shm_threshold, self.max_frame_size,
            ));
        }
        if self.chunk_size == 0 {
            return Err("chunk_size must be > 0".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_validates() {
        let cfg = IpcConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn reject_zero_segment_size() {
        let cfg = IpcConfig { pool_segment_size: 0, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("pool_segment_size must be > 0"));
    }

    #[test]
    fn reject_oversized_segment() {
        let cfg = IpcConfig {
            pool_segment_size: u64::from(u32::MAX) + 1,
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("u32::MAX"));
    }

    #[test]
    fn reject_small_frame_size() {
        let cfg = IpcConfig { max_frame_size: 16, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("max_frame_size"));
    }

    #[test]
    fn reject_zero_payload_size() {
        let cfg = IpcConfig { max_payload_size: 0, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("max_payload_size must be > 0"));
    }

    #[test]
    fn reject_threshold_exceeds_frame() {
        let cfg = IpcConfig {
            shm_threshold: 1000,
            max_frame_size: 500,
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("shm_threshold"));
    }

    #[test]
    fn reject_zero_chunk_size() {
        let cfg = IpcConfig { chunk_size: 0, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("chunk_size must be > 0"));
    }

    #[test]
    fn chunk_gc_interval_is_f64() {
        let cfg = IpcConfig::default();
        let _secs: f64 = cfg.chunk_gc_interval; // compile-time type check
        assert!((cfg.chunk_gc_interval - 5.0).abs() < f64::EPSILON);
    }
}
