/// Configuration for the IPC transport layer.
///
/// Mirrors the Python `IPCConfig` dataclass from `c_two.transport.ipc.frame`.
/// All size fields are in bytes; time fields are in seconds.
#[derive(Debug, Clone)]
pub struct IpcConfig {
    // Transport limits
    pub shm_threshold: u64,
    pub max_frame_size: u64,
    pub max_payload_size: u64,
    pub max_pending_requests: u32,

    // Pool SHM settings
    pub pool_enabled: bool,
    pub pool_decay_seconds: f64,
    pub max_pool_memory: u64,
    pub max_pool_segments: u32,
    pub pool_segment_size: u64,

    // Reassembly pool settings
    pub reassembly_segment_size: u64,
    pub reassembly_max_segments: u32,

    // Chunked transfer settings
    pub max_total_chunks: u32,
    pub chunk_gc_interval: u32,
    pub chunk_threshold_ratio: f64,
    pub chunk_assembler_timeout: f64,
    pub max_reassembly_bytes: u64,
    pub reply_chunk_size: usize,

    // Heartbeat settings
    pub heartbeat_interval: f64,
    pub heartbeat_timeout: f64,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            // Transport limits
            shm_threshold: 4_096,              // 4 KB
            max_frame_size: 2_147_483_648,     // 2 GB
            max_payload_size: 17_179_869_184,  // 16 GB
            max_pending_requests: 1024,

            // Pool SHM settings
            pool_enabled: true,
            pool_decay_seconds: 60.0,
            max_pool_memory: 1_073_741_824,    // 1 GB
            max_pool_segments: 4,
            pool_segment_size: 268_435_456,    // 256 MB

            // Reassembly pool
            reassembly_segment_size: 64 * 1024 * 1024,  // 64 MB
            reassembly_max_segments: 4,

            // Chunked transfer settings
            max_total_chunks: 512,
            chunk_gc_interval: 100,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout: 60.0,
            max_reassembly_bytes: 8_589_934_592, // 8 GB
            reply_chunk_size: 131_072,             // 128 KB

            // Heartbeat settings
            heartbeat_interval: 15.0,
            heartbeat_timeout: 30.0,
        }
    }
}

impl IpcConfig {
    /// Validate configuration invariants, matching Python `IPCConfig.__post_init__`.
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
}
