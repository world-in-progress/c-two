//! IPC and relay transport constants and Host capacity.

use std::time::Duration;

pub const SHM_THRESHOLD: u64 = 16 * 1024;
pub const CHUNK_SIZE: u64 = 128 * 1024;
pub const REMOTE_PAYLOAD_CHUNK_SIZE: u64 = 1024 * 1024;
pub const MAX_HANDSHAKE_FRAME_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_RPC_FRAME_BYTES: u64 = 1024 * 1024;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
pub const WRITE_TIMEOUT_SECS: f64 = WRITE_TIMEOUT.as_secs_f64();
pub const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);
pub const CHUNK_ASSEMBLER_TIMEOUT: Duration = Duration::from_secs(60);
pub const CHUNK_GC_INTERVAL: Duration = Duration::from_secs(5);
pub const RELAY_ANTI_ENTROPY_INTERVAL: Duration = Duration::from_secs(60);
/// Maximum loan IDs in one IPC release frame.
pub const IPC_RELEASE_BATCH_CAPACITY: usize = 64;

pub const DEFAULT_MAX_PAYLOAD_SIZE: u64 = 16 * (1 << 30);
pub const DEFAULT_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_OUTCOME_MAX_ENTRIES: usize = 4096;
pub const DEFAULT_OUTCOME_MAX_RETAINED_BYTES_CAP: u64 = 64 * 1024 * 1024;

/// Default retained byte budget for settled keyed-call outcomes:
/// `min(64 MiB, memory_budget_bytes / 4)`.
pub const fn default_outcome_max_retained_bytes(memory_budget_bytes: u64) -> u64 {
    let quarter = memory_budget_bytes / 4;
    if quarter < DEFAULT_OUTCOME_MAX_RETAINED_BYTES_CAP {
        quarter
    } else {
        DEFAULT_OUTCOME_MAX_RETAINED_BYTES_CAP
    }
}

/// Default execution and copy concurrency derived from host parallelism.
pub fn default_max_execution_workers() -> u32 {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(4, 64) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerIpcConfig {
    pub max_payload_size: u64,
    pub max_execution_workers: u32,
    pub outcome_max_entries: usize,
    /// Explicit byte cap for retained settled outcomes across all registrations
    /// on this Host. `None` resolves to `min(64 MiB, memory_budget_bytes / 4)`;
    /// `Some(0)` selects metadata-only retention.
    pub outcome_max_retained_bytes: Option<u64>,
}

impl Default for ServerIpcConfig {
    fn default() -> Self {
        Self {
            max_payload_size: DEFAULT_MAX_PAYLOAD_SIZE,
            max_execution_workers: default_max_execution_workers(),
            outcome_max_entries: DEFAULT_OUTCOME_MAX_ENTRIES,
            outcome_max_retained_bytes: None,
        }
    }
}

impl ServerIpcConfig {
    pub fn effective_outcome_max_retained_bytes(&self, memory_budget_bytes: u64) -> u64 {
        self.outcome_max_retained_bytes
            .unwrap_or_else(|| default_outcome_max_retained_bytes(memory_budget_bytes))
    }

    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        if self.max_payload_size == 0 {
            return Err(crate::ConfigError::new(
                "max_payload_size (C2_MAX_PAYLOAD_SIZE) must be > 0",
            ));
        }
        if self.max_execution_workers == 0 {
            return Err(crate::ConfigError::new(
                "max_execution_workers (C2_MAX_EXECUTION_WORKERS) must be >= 1",
            ));
        }
        if self.outcome_max_entries == 0 {
            return Err(crate::ConfigError::new(
                "outcome_max_entries (C2_OUTCOME_MAX_ENTRIES) must be >= 1",
            ));
        }
        Ok(())
    }

    pub fn validate_with_memory_budget(
        &self,
        memory_budget_bytes: u64,
    ) -> Result<(), crate::ConfigError> {
        self.validate()?;
        if let Some(bytes) = self.outcome_max_retained_bytes
            && bytes > memory_budget_bytes
        {
            return Err(crate::ConfigError::new(format!(
                "outcome_max_retained_bytes (C2_OUTCOME_MAX_RETAINED_BYTES) ({bytes}) must be <= memory_budget_bytes ({memory_budget_bytes})"
            )));
        }
        Ok(())
    }
}
