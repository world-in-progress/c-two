//! IPC transport configuration split into Base / Server / Client.
//!
//! `BaseIpcConfig` holds pool, chunk, and reassembly settings shared by both
//! sides.  `ServerIpcConfig` and `ClientIpcConfig` each wrap a `base` field
//! and add role-specific knobs.  Both implement `Deref<Target = BaseIpcConfig>`
//! for ergonomic field access.
//!
//! All size fields are in bytes; time fields are in seconds (with `_secs`
//! suffix). Defaults are canonical for Rust, CLI, and SDK config resolution.

use std::ops::Deref;
use std::time::Duration;

pub const MAX_EXECUTION_WORKERS: u32 = 64;

/// Code-level IPC override fields shared by both server and client roles.
pub const BASE_IPC_OVERRIDE_KEYS: &[&str] = &[
    "pool_enabled",
    "pool_segment_size",
    "max_pool_segments",
    "reassembly_segment_size",
    "reassembly_max_segments",
    "max_total_chunks",
    "chunk_gc_interval",
    "chunk_threshold_ratio",
    "chunk_assembler_timeout",
    "max_reassembly_bytes",
    "chunk_size",
];

/// Code-level IPC override fields accepted for server config resolution.
pub const SERVER_IPC_OVERRIDE_KEYS: &[&str] = &[
    "pool_enabled",
    "pool_segment_size",
    "max_pool_segments",
    "reassembly_segment_size",
    "reassembly_max_segments",
    "max_total_chunks",
    "chunk_gc_interval",
    "chunk_threshold_ratio",
    "chunk_assembler_timeout",
    "max_reassembly_bytes",
    "chunk_size",
    "max_frame_size",
    "max_payload_size",
    "max_pending_requests",
    "max_execution_workers",
    "pool_decay_seconds",
    "heartbeat_interval",
    "heartbeat_timeout",
];

/// Code-level IPC override fields accepted for client config resolution.
pub const CLIENT_IPC_OVERRIDE_KEYS: &[&str] = BASE_IPC_OVERRIDE_KEYS;

/// Global transport policy fields that are not valid role-level IPC overrides.
pub const FORBIDDEN_IPC_OVERRIDE_KEYS: &[&str] = &["shm_threshold"];

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
    pub max_execution_workers: u32,
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
            pool_segment_size: 268_435_456, // 256 MB
            max_pool_segments: 4,
            max_pool_memory: 1_073_741_824, // 1 GB

            reassembly_segment_size: 64 * 1024 * 1024, // 64 MB
            reassembly_max_segments: 4,

            max_total_chunks: 512,
            chunk_gc_interval_secs: 5.0,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout_secs: 60.0,
            max_reassembly_bytes: 8_589_934_592, // 8 GB
            chunk_size: 131_072,                 // 128 KB
        }
    }
}

impl Default for ServerIpcConfig {
    fn default() -> Self {
        Self {
            base: BaseIpcConfig::default(),

            shm_threshold: 4_096,
            max_frame_size: 2_147_483_648,    // 2 GB
            max_payload_size: 17_179_869_184, // 16 GB
            max_pending_requests: 1024,
            max_execution_workers: default_max_execution_workers(),
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
    fn deref(&self) -> &BaseIpcConfig {
        &self.base
    }
}

impl Deref for ClientIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig {
        &self.base
    }
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
                self.pool_segment_size,
                u32::MAX,
            ));
        }
        if !(1..=255).contains(&self.max_pool_segments) {
            return Err(format!(
                "max_pool_segments ({}) must be in 1..=255",
                self.max_pool_segments,
            ));
        }
        let expected_max_pool_memory = self
            .pool_segment_size
            .checked_mul(u64::from(self.max_pool_segments))
            .ok_or_else(|| "max_pool_memory derived value overflowed".to_string())?;
        if self.max_pool_memory != expected_max_pool_memory {
            return Err(format!(
                "max_pool_memory ({}) must equal pool_segment_size * max_pool_segments ({})",
                self.max_pool_memory, expected_max_pool_memory,
            ));
        }
        if self.reassembly_segment_size == 0 {
            return Err("reassembly_segment_size must be > 0".into());
        }
        if !(1..=255).contains(&self.reassembly_max_segments) {
            return Err(format!(
                "reassembly_max_segments ({}) must be in 1..=255",
                self.reassembly_max_segments,
            ));
        }
        if self.chunk_size == 0 {
            return Err("chunk_size must be > 0".into());
        }
        if !self.chunk_gc_interval_secs.is_finite() {
            return Err(format!(
                "chunk_gc_interval_secs ({}) must be finite",
                self.chunk_gc_interval_secs,
            ));
        }
        if self.chunk_gc_interval_secs <= 0.0 {
            return Err(format!(
                "chunk_gc_interval_secs ({}) must be > 0",
                self.chunk_gc_interval_secs,
            ));
        }
        validate_duration_secs("chunk_gc_interval_secs", self.chunk_gc_interval_secs)?;
        if !self.chunk_threshold_ratio.is_finite() {
            return Err(format!(
                "chunk_threshold_ratio ({}) must be finite",
                self.chunk_threshold_ratio,
            ));
        }
        if self.chunk_threshold_ratio <= 0.0 || self.chunk_threshold_ratio > 1.0 {
            return Err(format!(
                "chunk_threshold_ratio ({}) must be in (0, 1]",
                self.chunk_threshold_ratio,
            ));
        }
        if !self.chunk_assembler_timeout_secs.is_finite() {
            return Err(format!(
                "chunk_assembler_timeout_secs ({}) must be finite",
                self.chunk_assembler_timeout_secs,
            ));
        }
        if self.chunk_assembler_timeout_secs <= 0.0 {
            return Err(format!(
                "chunk_assembler_timeout_secs ({}) must be > 0",
                self.chunk_assembler_timeout_secs,
            ));
        }
        validate_duration_secs(
            "chunk_assembler_timeout_secs",
            self.chunk_assembler_timeout_secs,
        )?;
        if self.max_reassembly_bytes == 0 {
            return Err("max_reassembly_bytes must be > 0".into());
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
        if self.max_execution_workers == 0 {
            return Err("max_execution_workers must be > 0".into());
        }
        if self.max_execution_workers > MAX_EXECUTION_WORKERS {
            return Err(format!(
                "max_execution_workers ({}) must be <= {}",
                self.max_execution_workers, MAX_EXECUTION_WORKERS
            ));
        }
        if self.base.pool_segment_size > self.max_payload_size {
            return Err(format!(
                "pool_segment_size ({}) must not exceed max_payload_size ({})",
                self.base.pool_segment_size, self.max_payload_size,
            ));
        }
        if self.shm_threshold > self.max_frame_size {
            return Err(format!(
                "shm_threshold ({}) must not exceed max_frame_size ({})",
                self.shm_threshold, self.max_frame_size,
            ));
        }
        if !self.pool_decay_seconds.is_finite() {
            return Err(format!(
                "pool_decay_seconds ({}) must be finite",
                self.pool_decay_seconds,
            ));
        }
        validate_duration_secs("pool_decay_seconds", self.pool_decay_seconds)?;
        if !self.heartbeat_interval_secs.is_finite() {
            return Err(format!(
                "heartbeat_interval_secs ({}) must be finite",
                self.heartbeat_interval_secs,
            ));
        }
        if self.heartbeat_interval_secs < 0.0 {
            return Err(format!(
                "heartbeat_interval_secs ({}) must be >= 0",
                self.heartbeat_interval_secs,
            ));
        }
        validate_duration_secs("heartbeat_interval_secs", self.heartbeat_interval_secs)?;
        if !self.heartbeat_timeout_secs.is_finite() {
            return Err(format!(
                "heartbeat_timeout_secs ({}) must be finite",
                self.heartbeat_timeout_secs,
            ));
        }
        validate_duration_secs("heartbeat_timeout_secs", self.heartbeat_timeout_secs)?;
        if self.heartbeat_interval_secs > 0.0
            && self.heartbeat_timeout_secs <= self.heartbeat_interval_secs
        {
            return Err(format!(
                "heartbeat_timeout_secs ({}) must exceed heartbeat_interval_secs ({})",
                self.heartbeat_timeout_secs, self.heartbeat_interval_secs,
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

fn validate_duration_secs(name: &str, secs: f64) -> Result<(), String> {
    Duration::try_from_secs_f64(secs)
        .map(|_| ())
        .map_err(|_| format!("{name} ({secs}) must be a representable duration in seconds"))
}

fn default_max_execution_workers() -> u32 {
    let available = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4);
    available.clamp(4, MAX_EXECUTION_WORKERS as usize) as u32
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default validation ───────────────────────────────────────────────

    #[test]
    fn override_key_catalogs_exclude_derived_and_global_fields() {
        assert_eq!(CLIENT_IPC_OVERRIDE_KEYS, BASE_IPC_OVERRIDE_KEYS);
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"max_frame_size"));
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"max_execution_workers"));
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"heartbeat_timeout"));
        assert!(!CLIENT_IPC_OVERRIDE_KEYS.contains(&"max_frame_size"));

        for keys in [BASE_IPC_OVERRIDE_KEYS, SERVER_IPC_OVERRIDE_KEYS] {
            assert!(!keys.contains(&"max_pool_memory"));
            assert!(!keys.contains(&"shm_threshold"));
        }
        assert_eq!(FORBIDDEN_IPC_OVERRIDE_KEYS, &["shm_threshold"]);
    }

    #[test]
    fn server_default_validates() {
        assert!(ServerIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn server_default_execution_workers_uses_bounded_os_parallelism() {
        let cfg = ServerIpcConfig::default();

        assert!((4..=MAX_EXECUTION_WORKERS).contains(&cfg.max_execution_workers));
    }

    #[test]
    fn reject_zero_execution_workers() {
        let cfg = ServerIpcConfig {
            max_execution_workers: 0,
            ..ServerIpcConfig::default()
        };

        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("max_execution_workers")
        );
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
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("pool_segment_size must be > 0")
        );
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
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("chunk_size must be > 0")
        );
    }

    #[test]
    fn reject_bad_threshold_ratio() {
        let mut cfg = BaseIpcConfig {
            chunk_threshold_ratio: 0.0,
            ..BaseIpcConfig::default()
        };
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("chunk_threshold_ratio")
        );

        cfg.chunk_threshold_ratio = 1.1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("chunk_threshold_ratio")
        );

        cfg.chunk_threshold_ratio = 1.0; // boundary — valid
        assert!(cfg.validate().is_ok());
    }

    // ── Server-only validations ──────────────────────────────────────────

    #[test]
    fn reject_small_frame_size() {
        let cfg = ServerIpcConfig {
            max_frame_size: 16,
            ..ServerIpcConfig::default()
        };
        assert!(cfg.validate().unwrap_err().contains("max_frame_size"));
    }

    #[test]
    fn reject_zero_payload_size() {
        let cfg = ServerIpcConfig {
            max_payload_size: 0,
            ..ServerIpcConfig::default()
        };
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("max_payload_size must be > 0")
        );
    }

    #[test]
    fn reject_pool_segment_larger_than_payload_size() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_segment_size = 2 * 1024 * 1024;
        cfg.base.max_pool_memory =
            cfg.base.pool_segment_size * u64::from(cfg.base.max_pool_segments);
        cfg.max_payload_size = 1024 * 1024;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("pool_segment_size"));
        assert!(err.contains("max_payload_size"));
    }

    #[test]
    fn reject_threshold_exceeds_frame() {
        let cfg = ServerIpcConfig {
            shm_threshold: 1000,
            max_frame_size: 500,
            ..ServerIpcConfig::default()
        };
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
    fn server_reassembly_segment_is_64mb() {
        let cfg = ServerIpcConfig::default();
        assert_eq!(cfg.base.reassembly_segment_size, 64 * 1024 * 1024);
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
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("pool_segment_size must be > 0")
        );
    }
}
