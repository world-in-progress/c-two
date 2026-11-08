//! Shared configuration types for C-Two.
//!
//! This crate is the single source of truth for all configuration structs
//! used across the C-Two transport layer (IPC, relay, memory pool).

mod call;
mod connect_options;
mod identity;
mod ipc;
mod local;
mod memory;
mod pool;
mod relay;
mod remote;
mod resolver;
mod timeout;

pub use call::{
    CallExecutionLimits, CallOptions, CallTimeout, DEFAULT_MAX_OUTSTANDING_CALLS,
    DEFAULT_PAYLOAD_BUFFER_BUDGET_BYTES,
};
pub use connect_options::{
    ConnectDeadline, ConnectDeadlineExceeded, ConnectOptions, ConnectTimeoutError,
};
pub use identity::{validate_relay_id, validate_server_id};
pub use ipc::{
    BaseIpcConfig, CLIENT_IPC_OVERRIDE_KEYS, ClientIpcConfig, IPC_RELEASE_BATCH_CAPACITY,
    MAX_EXECUTION_WORKERS, MAX_IPC_POOL_SEGMENTS, PoolRoleTuning, SERVER_IPC_OVERRIDE_KEYS,
    ServerIpcConfig,
};
pub use local::{
    LocalEndpoint, LocalEndpointContext, LocalEndpointNamespace, LocalEndpointOptions,
    UNIX_NAMESPACE_FORMAT_VERSION,
};
pub use memory::MemoryBudgetLimits;
pub use pool::{PoolConfig, default_spill_dir};
pub use relay::RelayConfig;
pub use remote::{
    DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE, MAX_REMOTE_PAYLOAD_CHUNK_SIZE,
    validate_remote_payload_chunk_size,
};
pub use resolver::{
    CallExecutionLimitsOverrides, ClientIpcConfigOverrides, ConfigResolver, ConfigSources,
    EnvFilePolicy, EnvMap, RelayConfigOverrides, ResolvedRelayClientConfig, ResolvedRelayConfig,
    RuntimeConfigOverrides, ServerIpcConfigOverrides,
};
pub use timeout::{TimeoutError, deadline_after, timeout_from_seconds, validate_timeout};
