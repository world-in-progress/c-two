//! Shared configuration types for C-Two.
//!
//! This crate is the single source of truth for all configuration structs
//! used across the C-Two transport layer (IPC, relay, memory pool).

mod identity;
mod ipc;
mod lifecycle;
mod local;
mod memory;
mod pool;
mod relay;
mod remote;
mod resolver;

pub use identity::{validate_ipc_region_id, validate_relay_id, validate_server_id};
pub use lifecycle::{
    MAX_OWNER_MISSING_GRACE, ServerLifecyclePolicy, validate_owner_missing_grace,
};
pub use ipc::{
    BASE_IPC_OVERRIDE_KEYS, BaseIpcConfig, CLIENT_IPC_OVERRIDE_KEYS, ClientIpcConfig,
    FORBIDDEN_IPC_OVERRIDE_KEYS, MAX_EXECUTION_WORKERS, MAX_IPC_POOL_SEGMENTS, PoolRoleTuning,
    SERVER_IPC_OVERRIDE_KEYS, ServerIpcConfig,
};
pub use local::{LocalEndpoint, LocalEndpointNamespace};
pub use memory::MemoryBudgetLimits;
pub use pool::{PoolConfig, default_spill_dir};
pub use relay::RelayConfig;
pub use remote::{
    DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE, MAX_REMOTE_PAYLOAD_CHUNK_SIZE,
    validate_remote_payload_chunk_size,
};
pub use resolver::{
    BaseIpcConfigOverrides, ClientIpcConfigOverrides, ConfigResolver, ConfigSources, EnvFilePolicy,
    EnvMap, RelayConfigOverrides, ResolvedRelayClientConfig, ResolvedRelayConfig,
    ResolvedRuntimeConfig, RuntimeConfigOverrides, ServerIpcConfigOverrides,
};
