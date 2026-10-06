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

use crate::LocalEndpointProtocol;

pub const MAX_EXECUTION_WORKERS: u32 = 64;

/// Canonical upper bound on configured buddy pool segments
/// (`max_pool_segments` / `reassembly_max_segments`). This is the actual
/// supported IPC segment-index space, not an arbitrary limit: both peers size
/// their receive-side peer caches to this bound so every segment index a
/// legitimately configured peer can reference is accepted, while indices
/// beyond it stay rejected (`MemPool::ensure_peer_segment` bounds checks
/// against the peer-cache `max_segments`).
pub const MAX_IPC_POOL_SEGMENTS: u32 = 255;

/// Code-level IPC override fields shared by both server and client roles.
pub const BASE_IPC_OVERRIDE_KEYS: &[&str] = &[
    "pool_enabled",
    "pool_segment_size",
    "max_pool_segments",
    "pool_prewarm_segments",
    "pool_min_retained_segments",
    "reassembly_segment_size",
    "reassembly_max_segments",
    "max_total_chunks",
    "chunk_gc_interval",
    "chunk_threshold_ratio",
    "chunk_assembler_timeout",
    "max_reassembly_bytes",
    "chunk_size",
    "shm_backing_budget_bytes",
    "file_backing_budget_bytes",
    "live_reassembly_budget_bytes",
    "endpoint_protocol",
];

/// Code-level IPC override fields accepted for server config resolution.
pub const SERVER_IPC_OVERRIDE_KEYS: &[&str] = &[
    "pool_enabled",
    "pool_segment_size",
    "max_pool_segments",
    "pool_prewarm_segments",
    "pool_min_retained_segments",
    "reassembly_segment_size",
    "reassembly_max_segments",
    "max_total_chunks",
    "chunk_gc_interval",
    "chunk_threshold_ratio",
    "chunk_assembler_timeout",
    "max_reassembly_bytes",
    "chunk_size",
    "shm_backing_budget_bytes",
    "file_backing_budget_bytes",
    "live_reassembly_budget_bytes",
    "endpoint_protocol",
    "max_frame_size",
    "max_payload_size",
    "max_pending_requests",
    "max_execution_workers",
    "pool_decay_seconds",
    "heartbeat_interval",
    "heartbeat_timeout",
];

/// Code-level IPC override fields accepted for client config resolution.
pub const CLIENT_IPC_OVERRIDE_KEYS: &[&str] = &[
    "pool_enabled",
    "pool_segment_size",
    "max_pool_segments",
    "pool_prewarm_segments",
    "pool_min_retained_segments",
    "reassembly_segment_size",
    "reassembly_max_segments",
    "max_total_chunks",
    "chunk_gc_interval",
    "chunk_threshold_ratio",
    "chunk_assembler_timeout",
    "max_reassembly_bytes",
    "chunk_size",
    "shm_backing_budget_bytes",
    "file_backing_budget_bytes",
    "live_reassembly_budget_bytes",
    "endpoint_protocol",
    "pool_decay_seconds",
];

/// Global transport policy fields that are not valid role-level IPC overrides.
pub const FORBIDDEN_IPC_OVERRIDE_KEYS: &[&str] = &["shm_threshold"];

// ─── Base ────────────────────────────────────────────────────────────────────

/// Fields shared by both server and client IPC configs.
///
/// `PartialEq` compares the complete resolved policy field by field. It is
/// the equality the transport uses to prove a cached connection was created
/// from the same resolved policy as a new request; float fields compare by
/// value, and every cache owner snapshots configs after resolution rather
/// than mutating a shared value.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseIpcConfig {
    /// Protocol used to derive the local OS endpoint from an IPC address.
    pub endpoint_protocol: LocalEndpointProtocol,
    // ── Pool SHM settings ────────────────────────────────────────────────
    /// Buddy-pool policy switch. Disabling it skips only the buddy tiers
    /// (reuse and expansion); dedicated SHM, chunked transfer, inline frames,
    /// and file spill remain available on every path that projects this config.
    pub pool_enabled: bool,
    pub pool_segment_size: u64,
    pub max_pool_segments: u32,
    pub max_pool_memory: u64,
    /// Buddy segments to map eagerly at connect/register (default 0 = fully
    /// lazy). Prewarming is an explicit transport action: without it, no buddy
    /// memory is mapped until the first allocation that needs it.
    pub pool_prewarm_segments: u32,
    /// Minimum idle buddy segments retained after `gc_buddy()` (default 0).
    /// Zero allows idle pools to retire back to zero mappings while segment
    /// generation counters and live allocations stay intact.
    pub pool_min_retained_segments: u32,

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

    // ── Finite memory budget limits ──────────────────────────────────────
    pub shm_backing_budget_bytes: u64,
    pub file_backing_budget_bytes: u64,
    pub live_reassembly_budget_bytes: u64,
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// Server-side IPC configuration.
#[derive(Debug, Clone, PartialEq)]
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
///
/// `PartialEq` is the complete resolved-policy equality used by the client
/// cache: a same-address hit is only valid when the cached client was created
/// from exactly this configuration. Budget-limit equality alone is not
/// sufficient because chunking, threshold, prewarm, and buddy-policy changes
/// also change how the cached client transfers data.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientIpcConfig {
    pub base: BaseIpcConfig,
    pub shm_threshold: u64,
    /// Idle window before the client's own request/reassembly buddy segments
    /// may be retired by the connection's periodic maintenance task while the
    /// connection stays open. `0` retires idle segments at the next maintenance
    /// tick; negative values are rejected, matching the server's
    /// `pool_decay_seconds` semantics.
    pub pool_decay_seconds: f64,
}

// ─── Default impls ───────────────────────────────────────────────────────────

impl Default for BaseIpcConfig {
    fn default() -> Self {
        let budget_limits = crate::MemoryBudgetLimits::default();
        Self {
            endpoint_protocol: LocalEndpointProtocol::LegacyV1,
            pool_enabled: true,
            pool_segment_size: 268_435_456, // 256 MB
            max_pool_segments: 4,
            max_pool_memory: 1_073_741_824, // 1 GB
            pool_prewarm_segments: 0,
            pool_min_retained_segments: 0,

            reassembly_segment_size: 64 * 1024 * 1024, // 64 MB
            reassembly_max_segments: 4,

            max_total_chunks: 512,
            chunk_gc_interval_secs: 5.0,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout_secs: 60.0,
            max_reassembly_bytes: 8_589_934_592, // 8 GB
            chunk_size: 131_072,                 // 128 KB

            shm_backing_budget_bytes: budget_limits.shm_backing_budget_bytes,
            file_backing_budget_bytes: budget_limits.file_backing_budget_bytes,
            live_reassembly_budget_bytes: budget_limits.live_reassembly_budget_bytes,
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
            pool_decay_seconds: 60.0,
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

// ─── PoolConfig projection ───────────────────────────────────────────────────

/// Role-specific tuning applied on top of [`BaseIpcConfig`] buddy policy when
/// projecting an owner [`PoolConfig`](crate::PoolConfig).
///
/// The base config owns the policy decisions (buddy enabled, segment geometry,
/// prewarm, minimum retained idle segments); the tuning carries only the
/// per-role values the calling transport already owned before centralization.
#[derive(Debug, Clone)]
pub struct PoolRoleTuning {
    /// Idle window before a fully idle buddy segment may be retired.
    pub buddy_idle_decay_secs: f64,
    /// Crash-recovery timeout for dedicated segments.
    pub dedicated_crash_timeout_secs: f64,
    /// Maximum simultaneously active dedicated segments.
    pub max_dedicated_segments: usize,
    /// File-spill directory for allocations that cannot stay in SHM.
    pub spill_dir: std::path::PathBuf,
}

impl Default for PoolRoleTuning {
    fn default() -> Self {
        Self {
            buddy_idle_decay_secs: 60.0,
            dedicated_crash_timeout_secs: 60.0,
            max_dedicated_segments: 4,
            spill_dir: crate::default_spill_dir(),
        }
    }
}

impl BaseIpcConfig {
    /// Project this config's buddy policy onto an owner pool config for the
    /// primary request/response pool role (`pool_segment_size` /
    /// `max_pool_segments`). Disabling `pool_enabled` disables only the buddy
    /// tiers of the returned pool; dedicated SHM and file spill remain.
    pub fn primary_pool_config(&self, tuning: &PoolRoleTuning) -> crate::PoolConfig {
        crate::PoolConfig {
            segment_size: self.pool_segment_size as usize,
            min_block_size: 4096,
            max_segments: self.max_pool_segments as usize,
            max_dedicated_segments: tuning.max_dedicated_segments,
            dedicated_crash_timeout_secs: tuning.dedicated_crash_timeout_secs,
            buddy_idle_decay_secs: tuning.buddy_idle_decay_secs,
            spill_threshold: 0.8,
            spill_dir: tuning.spill_dir.clone(),
            buddy_enabled: self.pool_enabled,
            min_retained_segments: self.pool_min_retained_segments as usize,
        }
    }

    /// Project this config's buddy policy onto an owner pool config for the
    /// chunk-reassembly storage role (`reassembly_segment_size` /
    /// `reassembly_max_segments`). The same buddy policy applies: a disabled
    /// buddy pool reassembles into dedicated SHM with file-spill fallback
    /// instead of skipping reassembly.
    pub fn reassembly_pool_config(&self, tuning: &PoolRoleTuning) -> crate::PoolConfig {
        crate::PoolConfig {
            segment_size: self.reassembly_segment_size as usize,
            min_block_size: 4096,
            max_segments: self.reassembly_max_segments as usize,
            max_dedicated_segments: tuning.max_dedicated_segments,
            dedicated_crash_timeout_secs: tuning.dedicated_crash_timeout_secs,
            buddy_idle_decay_secs: tuning.buddy_idle_decay_secs,
            spill_threshold: 0.8,
            spill_dir: tuning.spill_dir.clone(),
            buddy_enabled: self.pool_enabled,
            min_retained_segments: self.pool_min_retained_segments as usize,
        }
    }
}

impl ServerIpcConfig {
    /// Response-pool role tuning derived from server-only settings.
    pub fn response_pool_tuning(&self) -> PoolRoleTuning {
        PoolRoleTuning {
            buddy_idle_decay_secs: self.pool_decay_seconds,
            dedicated_crash_timeout_secs: 5.0,
            max_dedicated_segments: 4,
            spill_dir: std::path::PathBuf::from("/tmp/c_two_response_spill"),
        }
    }

    /// Reassembly-pool role tuning derived from server-only settings.
    pub fn reassembly_pool_tuning(&self) -> PoolRoleTuning {
        PoolRoleTuning {
            buddy_idle_decay_secs: self.pool_decay_seconds,
            dedicated_crash_timeout_secs: 5.0,
            max_dedicated_segments: 4,
            spill_dir: std::path::PathBuf::from("/tmp/c_two_reassembly"),
        }
    }
}

// ─── Validation ──────────────────────────────────────────────────────────────

impl BaseIpcConfig {
    /// Projects the resolved finite memory budget limits onto the canonical
    /// [`MemoryBudgetLimits`] value.
    ///
    /// Transport owners use this to build a `c2_mem::MemoryBudget` (for
    /// example through `MemoryBudget::from_limits`) without this config crate
    /// holding the reservation primitive or a second defaults table.
    pub fn memory_budget_limits(&self) -> crate::MemoryBudgetLimits {
        crate::MemoryBudgetLimits {
            shm_backing_budget_bytes: self.shm_backing_budget_bytes,
            file_backing_budget_bytes: self.file_backing_budget_bytes,
            live_reassembly_budget_bytes: self.live_reassembly_budget_bytes,
        }
    }

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
        if !(1..=MAX_IPC_POOL_SEGMENTS).contains(&self.max_pool_segments) {
            return Err(format!(
                "max_pool_segments ({}) must be in 1..={MAX_IPC_POOL_SEGMENTS}",
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
        if self.pool_prewarm_segments > self.max_pool_segments {
            return Err(format!(
                "pool_prewarm_segments ({}) must not exceed max_pool_segments ({})",
                self.pool_prewarm_segments, self.max_pool_segments,
            ));
        }
        if !self.pool_enabled && self.pool_prewarm_segments > 0 {
            return Err(format!(
                "pool_prewarm_segments ({}) requires pool_enabled; a disabled buddy pool must stay lazy",
                self.pool_prewarm_segments,
            ));
        }
        if self.pool_min_retained_segments > self.max_pool_segments {
            return Err(format!(
                "pool_min_retained_segments ({}) must not exceed max_pool_segments ({})",
                self.pool_min_retained_segments, self.max_pool_segments,
            ));
        }
        if self.pool_min_retained_segments > self.reassembly_max_segments {
            return Err(format!(
                "pool_min_retained_segments ({}) must not exceed reassembly_max_segments ({})",
                self.pool_min_retained_segments, self.reassembly_max_segments,
            ));
        }
        if self.reassembly_segment_size == 0 {
            return Err("reassembly_segment_size must be > 0".into());
        }
        if !(1..=MAX_IPC_POOL_SEGMENTS).contains(&self.reassembly_max_segments) {
            return Err(format!(
                "reassembly_max_segments ({}) must be in 1..={MAX_IPC_POOL_SEGMENTS}",
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
        // Memory budget limits are u64 and therefore cannot be negative; zero
        // is a valid finite limit that rejects positive reservations (it is
        // not unlimited), and the full u64 range is accepted. No cross-field
        // budget validation exists by design.
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
        self.base.validate()?;
        if !self.pool_decay_seconds.is_finite() {
            return Err(format!(
                "pool_decay_seconds ({}) must be finite",
                self.pool_decay_seconds
            ));
        }
        validate_duration_secs("pool_decay_seconds", self.pool_decay_seconds)?;
        Ok(())
    }

    /// Owner-pool role tuning for the client's own request and reassembly
    /// pools. The client-side idle window feeds both the pool projection
    /// (`buddy_idle_decay_secs`) and the connection maintenance cadence.
    pub fn pool_tuning(&self) -> PoolRoleTuning {
        PoolRoleTuning {
            buddy_idle_decay_secs: self.pool_decay_seconds,
            ..PoolRoleTuning::default()
        }
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
        // The client catalog is the shared base keys plus exactly one
        // role-specific key: the client-side idle decay window.
        assert_eq!(
            CLIENT_IPC_OVERRIDE_KEYS.len(),
            BASE_IPC_OVERRIDE_KEYS.len() + 1
        );
        for key in BASE_IPC_OVERRIDE_KEYS {
            assert!(CLIENT_IPC_OVERRIDE_KEYS.contains(key));
        }
        assert!(CLIENT_IPC_OVERRIDE_KEYS.contains(&"pool_decay_seconds"));
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"max_frame_size"));
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"max_execution_workers"));
        assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&"heartbeat_timeout"));
        assert!(!CLIENT_IPC_OVERRIDE_KEYS.contains(&"max_frame_size"));
        assert!(!CLIENT_IPC_OVERRIDE_KEYS.contains(&"heartbeat_interval"));

        for keys in [
            BASE_IPC_OVERRIDE_KEYS,
            SERVER_IPC_OVERRIDE_KEYS,
            CLIENT_IPC_OVERRIDE_KEYS,
        ] {
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

    // ── Memory budget limits ─────────────────────────────────────────────

    #[test]
    fn base_defaults_come_from_memory_budget_limits() {
        let cfg = BaseIpcConfig::default();
        let limits = crate::MemoryBudgetLimits::default();

        assert_eq!(
            cfg.shm_backing_budget_bytes,
            limits.shm_backing_budget_bytes
        );
        assert_eq!(
            cfg.file_backing_budget_bytes,
            limits.file_backing_budget_bytes
        );
        assert_eq!(
            cfg.live_reassembly_budget_bytes,
            limits.live_reassembly_budget_bytes
        );
        assert_eq!(cfg.shm_backing_budget_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(cfg.file_backing_budget_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(cfg.live_reassembly_budget_bytes, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn server_and_client_defaults_project_memory_budget_limits() {
        assert_eq!(
            ServerIpcConfig::default().memory_budget_limits(),
            crate::MemoryBudgetLimits::default()
        );
        assert_eq!(
            ClientIpcConfig::default().memory_budget_limits(),
            crate::MemoryBudgetLimits::default()
        );
    }

    #[test]
    fn memory_budget_limits_projection_is_pure_field_copy() {
        let cfg = BaseIpcConfig {
            shm_backing_budget_bytes: 1,
            file_backing_budget_bytes: u64::MAX,
            live_reassembly_budget_bytes: 0,
            ..BaseIpcConfig::default()
        };

        assert_eq!(
            cfg.memory_budget_limits(),
            crate::MemoryBudgetLimits {
                shm_backing_budget_bytes: 1,
                file_backing_budget_bytes: u64::MAX,
                live_reassembly_budget_bytes: 0,
            }
        );
    }

    #[test]
    fn zero_budget_limits_are_valid_finite_configuration() {
        let cfg = BaseIpcConfig {
            shm_backing_budget_bytes: 0,
            file_backing_budget_bytes: 0,
            live_reassembly_budget_bytes: 0,
            ..BaseIpcConfig::default()
        };
        assert!(cfg.validate().is_ok());
        assert_eq!(
            cfg.memory_budget_limits(),
            crate::MemoryBudgetLimits::zeroed()
        );
    }

    #[test]
    fn full_range_budget_limits_are_valid() {
        let cfg = BaseIpcConfig {
            shm_backing_budget_bytes: u64::MAX,
            file_backing_budget_bytes: u64::MAX,
            live_reassembly_budget_bytes: u64::MAX,
            ..BaseIpcConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn budget_override_keys_are_role_visible() {
        for key in [
            "shm_backing_budget_bytes",
            "file_backing_budget_bytes",
            "live_reassembly_budget_bytes",
        ] {
            assert!(BASE_IPC_OVERRIDE_KEYS.contains(&key));
            assert!(SERVER_IPC_OVERRIDE_KEYS.contains(&key));
            assert!(CLIENT_IPC_OVERRIDE_KEYS.contains(&key));
        }
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

    // ── Buddy policy fields ─────────────────────────────────────────────

    #[test]
    fn buddy_policy_defaults_are_lazy_and_retire_to_zero() {
        let cfg = BaseIpcConfig::default();
        assert!(cfg.pool_enabled);
        assert_eq!(cfg.pool_prewarm_segments, 0);
        assert_eq!(cfg.pool_min_retained_segments, 0);
        assert!(ServerIpcConfig::default().validate().is_ok());
        assert!(ClientIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn reject_prewarm_exceeding_max_pool_segments() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_prewarm_segments = cfg.base.max_pool_segments + 1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("pool_prewarm_segments")
        );
    }

    #[test]
    fn reject_prewarm_with_buddy_disabled() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_enabled = false;
        cfg.base.pool_prewarm_segments = 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("pool_prewarm_segments"));
        assert!(err.contains("pool_enabled"));

        // Disabled buddy with no prewarm stays valid.
        let mut lazy = ServerIpcConfig::default();
        lazy.base.pool_enabled = false;
        assert!(lazy.validate().is_ok());
    }

    #[test]
    fn reject_min_retained_exceeding_pool_or_reassembly_limits() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_min_retained_segments = cfg.base.max_pool_segments + 1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("pool_min_retained_segments")
        );

        let mut cfg = ServerIpcConfig::default();
        cfg.base.reassembly_max_segments = 2;
        cfg.base.pool_min_retained_segments = 3;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("reassembly_max_segments")
        );
    }

    #[test]
    fn valid_policy_boundaries_pass() {
        let mut cfg = ServerIpcConfig::default();
        cfg.base.pool_prewarm_segments = cfg.base.max_pool_segments;
        cfg.base.pool_min_retained_segments = cfg.base.max_pool_segments;
        cfg.base.reassembly_max_segments = cfg.base.max_pool_segments;
        cfg.base.max_pool_memory =
            cfg.base.pool_segment_size * u64::from(cfg.base.max_pool_segments);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn projection_maps_policy_to_primary_pool_config() {
        let mut cfg = BaseIpcConfig::default();
        cfg.pool_segment_size = 2 * 1024 * 1024;
        cfg.max_pool_segments = 3;
        cfg.pool_enabled = false;
        cfg.pool_min_retained_segments = 2;

        let tuning = PoolRoleTuning {
            buddy_idle_decay_secs: 12.0,
            dedicated_crash_timeout_secs: 5.0,
            max_dedicated_segments: 7,
            spill_dir: std::path::PathBuf::from("/tmp/projection_test"),
        };
        let pool = cfg.primary_pool_config(&tuning);

        assert_eq!(pool.segment_size, 2 * 1024 * 1024);
        assert_eq!(pool.max_segments, 3);
        assert!(!pool.buddy_enabled);
        assert_eq!(pool.min_retained_segments, 2);
        assert_eq!(pool.buddy_idle_decay_secs, 12.0);
        assert_eq!(pool.dedicated_crash_timeout_secs, 5.0);
        assert_eq!(pool.max_dedicated_segments, 7);
        assert_eq!(
            pool.spill_dir,
            std::path::PathBuf::from("/tmp/projection_test")
        );
    }

    #[test]
    fn projection_maps_reassembly_role_and_keeps_dedicated_alive() {
        let mut cfg = BaseIpcConfig::default();
        cfg.reassembly_segment_size = 1024 * 1024;
        cfg.reassembly_max_segments = 2;
        cfg.pool_enabled = false;

        let pool = cfg.reassembly_pool_config(&PoolRoleTuning::default());

        assert_eq!(pool.segment_size, 1024 * 1024);
        assert_eq!(pool.max_segments, 2);
        assert!(!pool.buddy_enabled);
        // Dedicated storage must stay reachable when buddy is disabled.
        assert!(pool.max_dedicated_segments > 0);
        assert_eq!(
            pool.min_retained_segments,
            cfg.pool_min_retained_segments as usize
        );
    }

    #[test]
    fn server_tuning_derives_from_pool_decay_seconds() {
        let mut cfg = ServerIpcConfig::default();
        cfg.pool_decay_seconds = 7.5;
        assert_eq!(cfg.response_pool_tuning().buddy_idle_decay_secs, 7.5);
        assert_eq!(cfg.reassembly_pool_tuning().buddy_idle_decay_secs, 7.5);
    }

    // ── Canonical peer-cache segment bound ──────────────────────────────

    #[test]
    fn max_ipc_pool_segments_bound_is_canonical_and_enforced() {
        assert_eq!(MAX_IPC_POOL_SEGMENTS, 255);

        let mut cfg = BaseIpcConfig {
            max_pool_segments: MAX_IPC_POOL_SEGMENTS,
            max_pool_memory: cfg_pool_memory(MAX_IPC_POOL_SEGMENTS),
            ..BaseIpcConfig::default()
        };
        assert!(cfg.validate().is_ok());

        cfg.max_pool_segments = MAX_IPC_POOL_SEGMENTS + 1;
        cfg.max_pool_memory = cfg_pool_memory(MAX_IPC_POOL_SEGMENTS + 1);
        assert!(cfg.validate().unwrap_err().contains("max_pool_segments"));

        cfg = BaseIpcConfig {
            reassembly_max_segments: MAX_IPC_POOL_SEGMENTS,
            ..BaseIpcConfig::default()
        };
        assert!(cfg.validate().is_ok());
        cfg.reassembly_max_segments = MAX_IPC_POOL_SEGMENTS + 1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("reassembly_max_segments")
        );
    }

    fn cfg_pool_memory(segments: u32) -> u64 {
        BaseIpcConfig::default().pool_segment_size * u64::from(segments)
    }

    // ── Client pool decay ───────────────────────────────────────────────

    #[test]
    fn client_pool_decay_defaults_and_validates_like_server_decay() {
        assert_eq!(ClientIpcConfig::default().pool_decay_seconds, 60.0);
        assert!(ClientIpcConfig::default().validate().is_ok());

        let mut cfg = ClientIpcConfig::default();
        cfg.pool_decay_seconds = 0.0; // zero means immediate retirement at the next tick
        assert!(cfg.validate().is_ok());

        cfg.pool_decay_seconds = -1.0; // negative is rejected like the server
        assert!(cfg.validate().unwrap_err().contains("pool_decay_seconds"));

        cfg.pool_decay_seconds = f64::NAN;
        assert!(cfg.validate().unwrap_err().contains("pool_decay_seconds"));

        cfg.pool_decay_seconds = f64::INFINITY;
        assert!(cfg.validate().unwrap_err().contains("pool_decay_seconds"));

        cfg.pool_decay_seconds = 1e100;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("representable duration")
        );
    }

    #[test]
    fn client_pool_tuning_projects_client_decay() {
        let mut cfg = ClientIpcConfig::default();
        cfg.pool_decay_seconds = 2.5;
        let tuning = cfg.pool_tuning();
        assert_eq!(tuning.buddy_idle_decay_secs, 2.5);
        // Role defaults for the non-decay fields stay intact.
        assert_eq!(
            tuning.dedicated_crash_timeout_secs,
            PoolRoleTuning::default().dedicated_crash_timeout_secs
        );
        assert_eq!(
            tuning.max_dedicated_segments,
            PoolRoleTuning::default().max_dedicated_segments
        );
    }

    // ── Complete resolved-config equality ────────────────────────────────

    /// The client cache rejects a same-address hit when the requested resolved
    /// policy differs, so `PartialEq` must cover every field a connection
    /// actually uses — not just the budget identity.
    #[test]
    fn client_config_equality_covers_every_resolved_policy_field() {
        let base = BaseIpcConfig::default();
        let pristine = ClientIpcConfig {
            base: base.clone(),
            ..ClientIpcConfig::default()
        };
        assert_eq!(pristine, pristine.clone());

        let base_mutations = [
            BaseIpcConfig {
                endpoint_protocol: crate::LocalEndpointProtocol::ManagedV2,
                ..base.clone()
            },
            BaseIpcConfig {
                pool_enabled: !base.pool_enabled,
                ..base.clone()
            },
            BaseIpcConfig {
                pool_segment_size: base.pool_segment_size + 4096,
                ..base.clone()
            },
            BaseIpcConfig {
                max_pool_segments: base.max_pool_segments + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                max_pool_memory: base.max_pool_memory + 4096,
                ..base.clone()
            },
            BaseIpcConfig {
                pool_prewarm_segments: base.pool_prewarm_segments + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                pool_min_retained_segments: base.pool_min_retained_segments + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                reassembly_segment_size: base.reassembly_segment_size + 4096,
                ..base.clone()
            },
            BaseIpcConfig {
                reassembly_max_segments: base.reassembly_max_segments + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                max_total_chunks: base.max_total_chunks + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                chunk_gc_interval_secs: base.chunk_gc_interval_secs + 1.0,
                ..base.clone()
            },
            BaseIpcConfig {
                chunk_threshold_ratio: base.chunk_threshold_ratio - 0.1,
                ..base.clone()
            },
            BaseIpcConfig {
                chunk_assembler_timeout_secs: base.chunk_assembler_timeout_secs + 1.0,
                ..base.clone()
            },
            BaseIpcConfig {
                max_reassembly_bytes: base.max_reassembly_bytes + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                chunk_size: base.chunk_size + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                shm_backing_budget_bytes: base.shm_backing_budget_bytes + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                file_backing_budget_bytes: base.file_backing_budget_bytes + 1,
                ..base.clone()
            },
            BaseIpcConfig {
                live_reassembly_budget_bytes: base.live_reassembly_budget_bytes + 1,
                ..base.clone()
            },
        ];
        for mutated in base_mutations {
            let cfg = ClientIpcConfig {
                base: mutated,
                ..pristine.clone()
            };
            assert_ne!(
                pristine, cfg,
                "every base policy field must participate in client config equality"
            );
        }

        for cfg in [
            ClientIpcConfig {
                shm_threshold: pristine.shm_threshold + 1,
                ..pristine.clone()
            },
            ClientIpcConfig {
                pool_decay_seconds: pristine.pool_decay_seconds + 1.0,
                ..pristine.clone()
            },
        ] {
            assert_ne!(
                pristine, cfg,
                "every client policy field must participate in client config equality"
            );
        }
    }
}
