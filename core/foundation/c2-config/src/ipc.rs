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
}

impl Default for ServerIpcConfig {
    fn default() -> Self {
        Self {
            max_payload_size: DEFAULT_MAX_PAYLOAD_SIZE,
            max_execution_workers: default_max_execution_workers(),
        }
    }
}

impl ServerIpcConfig {
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
        Ok(())
    }
}
