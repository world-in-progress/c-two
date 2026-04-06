//! IPC transport configuration split into Base / Server / Client.
//!
//! `BaseIpcConfig` holds pool, chunk, and reassembly settings shared by both
//! sides.  `ServerIpcConfig` and `ClientIpcConfig` each wrap a `base` field
//! and add role-specific knobs.  Both implement `Deref<Target = BaseIpcConfig>`
//! for ergonomic field access.
//!
//! All size fields are in bytes; time fields are in seconds (with `_secs`
//! suffix).  `Default` impls exist for Rust-side testing only — Python is
//! authoritative at runtime.

use std::ops::Deref;

// ─── Base ────────────────────────────────────────────────────────────────────

/// Fields shared by both server and client IPC configs.
#[derive(Debug, Clone)]
pub struct BaseIpcConfig {
    // ── Pool SHM settings ────────────────────────────────────────────────
    pub pool_enabled: bool,
    pub pool_segment_size: u64,
    pub max_pool_segments: u32,
    pub max_pool_memory: u64,

    // ── Reassembly pool settings ─────────────────────────────────────────
    pub reassembly_segment_size: u64,
    pub reassembly_max_segments: u32,

    // ── Chunked transfer settings ────────────────────────────────────────
    pub max_total_chunks: u32,
    pub chunk_gc_interval_secs: f64,
    pub chunk_threshold_ratio: f64,
    pub chunk_assembler_timeout_secs: f64,
    pub max_reassembly_bytes: u64,
    pub chunk_size: u64,
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// Server-side IPC configuration.
#[derive(Debug, Clone)]
pub struct ServerIpcConfig {
    pub base: BaseIpcConfig,

    pub shm_threshold: u64,
    pub max_frame_size: u64,
    pub max_payload_size: u64,
    pub max_pending_requests: u32,
    pub pool_decay_seconds: f64,
    pub heartbeat_interval_secs: f64,
    pub heartbeat_timeout_secs: f64,
}

// ─── Client ──────────────────────────────────────────────────────────────────

/// Client-side IPC configuration.
#[derive(Debug, Clone)]
pub struct ClientIpcConfig {
    pub base: BaseIpcConfig,
    pub shm_threshold: u64,
}

// ─── Default impls ───────────────────────────────────────────────────────────

impl Default for BaseIpcConfig {
    fn default() -> Self {
        Self {
            pool_enabled: true,
            pool_segment_size: 268_435_456,       // 256 MB
            max_pool_segments: 4,
            max_pool_memory: 1_073_741_824,        // 1 GB

            reassembly_segment_size: 268_435_456,  // 256 MB (server default)
            reassembly_max_segments: 4,

            max_total_chunks: 512,
            chunk_gc_interval_secs: 5.0,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout_secs: 60.0,
            max_reassembly_bytes: 8_589_934_592,   // 8 GB
            chunk_size: 131_072,                   // 128 KB
        }
    }
}

impl Default for ServerIpcConfig {
    fn default() -> Self {
        Self {
            base: BaseIpcConfig::default(),

            shm_threshold: 4_096,
            max_frame_size: 2_147_483_648,         // 2 GB
            max_payload_size: 17_179_869_184,      // 16 GB
            max_pending_requests: 1024,
            pool_decay_seconds: 60.0,
            heartbeat_interval_secs: 15.0,
            heartbeat_timeout_secs: 30.0,
        }
    }
}

impl Default for ClientIpcConfig {
    fn default() -> Self {
        Self {
            base: BaseIpcConfig {
                reassembly_segment_size: 64 * 1024 * 1024, // 64 MB override
                ..BaseIpcConfig::default()
            },
            shm_threshold: 4_096,
        }
    }
}

// ─── Deref impls ─────────────────────────────────────────────────────────────

impl Deref for ServerIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}

impl Deref for ClientIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}

// ─── Validation ──────────────────────────────────────────────────────────────

impl BaseIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.pool_segment_size == 0 {
            return Err("pool_segment_size must be > 0".into());
        }
        if self.pool_segment_size > u32::MAX as u64 {
            return Err(format!(
                "pool_segment_size ({}) must be <= u32::MAX ({}) for wire format compatibility",
                self.pool_segment_size, u32::MAX,
            ));
        }
        if self.chunk_size == 0 {
            return Err("chunk_size must be > 0".into());
        }
        if self.chunk_threshold_ratio <= 0.0 || self.chunk_threshold_ratio > 1.0 {
            return Err(format!(
                "chunk_threshold_ratio ({}) must be in (0, 1]",
                self.chunk_threshold_ratio,
            ));
        }
        Ok(())
    }
}

impl ServerIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.base.validate()?;
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

impl ClientIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.base.validate()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default validation ───────────────────────────────────────────────

    #[test]
    fn server_default_validates() {
        assert!(ServerIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn client_default_validates() {
        assert!(ClientIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn base_default_validates() {
        assert!(BaseIpcConfig::default().validate().is_ok());
    }

    // ── Base validations (via ServerIpcConfig) ───────────────────────────

    #[test]
    fn reject_zero_segment_size() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_segment_size = 0;
        assert!(cfg.validate().unwrap_err().contains("pool_segment_size must be > 0"));
    }

    #[test]
    fn reject_oversized_segment() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_segment_size = u64::from(u32::MAX) + 1;
        assert!(cfg.validate().unwrap_err().contains("u32::MAX"));
    }

    #[test]
    fn reject_zero_chunk_size() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.chunk_size = 0;
        assert!(cfg.validate().unwrap_err().contains("chunk_size must be > 0"));
    }

    #[test]
    fn reject_bad_threshold_ratio() {
        let mut cfg = BaseIpcConfig::default();
        cfg.chunk_threshold_ratio = 0.0;
        assert!(cfg.validate().unwrap_err().contains("chunk_threshold_ratio"));

        cfg.chunk_threshold_ratio = 1.1;
        assert!(cfg.validate().unwrap_err().contains("chunk_threshold_ratio"));

        cfg.chunk_threshold_ratio = 1.0; // boundary — valid
        assert!(cfg.validate().is_ok());
    }

    // ── Server-only validations ──────────────────────────────────────────

    #[test]
    fn reject_small_frame_size() {
        let mut cfg = ServerIpcConfig::default();
        cfg.max_frame_size = 16;
        assert!(cfg.validate().unwrap_err().contains("max_frame_size"));
    }

    #[test]
    fn reject_zero_payload_size() {
        let mut cfg = ServerIpcConfig::default();
        cfg.max_payload_size = 0;
        assert!(cfg.validate().unwrap_err().contains("max_payload_size must be > 0"));
    }

    #[test]
    fn reject_threshold_exceeds_frame() {
        let mut cfg = ServerIpcConfig::default();
        cfg.shm_threshold = 1000;
        cfg.max_frame_size = 500;
        assert!(cfg.validate().unwrap_err().contains("shm_threshold"));
    }

    // ── Deref ────────────────────────────────────────────────────────────

    #[test]
    fn server_deref_accesses_base_fields() {
        let cfg = ServerIpcConfig::default();
        assert!(cfg.pool_enabled);
        assert_eq!(cfg.pool_segment_size, 268_435_456);
    }

    #[test]
    fn client_deref_accesses_base_fields() {
        let cfg = ClientIpcConfig::default();
        assert!(cfg.pool_enabled);
        assert_eq!(cfg.chunk_size, 131_072);
    }

    // ── Client-specific defaults ─────────────────────────────────────────

    #[test]
    fn client_reassembly_segment_is_64mb() {
        let cfg = ClientIpcConfig::default();
        assert_eq!(cfg.base.reassembly_segment_size, 64 * 1024 * 1024);
    }

    #[test]
    fn server_reassembly_segment_is_256mb() {
        let cfg = ServerIpcConfig::default();
        assert_eq!(cfg.base.reassembly_segment_size, 268_435_456);
    }

    // ── Type checks ──────────────────────────────────────────────────────

    #[test]
    fn chunk_gc_interval_secs_is_f64() {
        let cfg = ServerIpcConfig::default();
        let _secs: f64 = cfg.chunk_gc_interval_secs;
        assert!((cfg.chunk_gc_interval_secs - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn chunk_size_is_u64() {
        let cfg = BaseIpcConfig::default();
        let _v: u64 = cfg.chunk_size;
        assert_eq!(_v, 131_072);
    }

    // ── Client validate delegates to base ────────────────────────────────

    #[test]
    fn client_rejects_zero_segment_size() {
        let mut cfg = ClientIpcConfig::default();
        cfg.base.pool_segment_size = 0;
        assert!(cfg.validate().unwrap_err().contains("pool_segment_size must be > 0"));
    }
}
