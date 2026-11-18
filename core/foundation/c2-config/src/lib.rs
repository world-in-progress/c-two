//! Shared configuration and transport policy for C-Two.

mod call;
mod connect_options;
mod domain;
mod identity;
mod ipc;
mod local;
mod pool;
mod relay;
mod resolver;
mod timeout;

pub use call::{CallOptions, CallTimeout};
pub use connect_options::{
    ConnectDeadline, ConnectDeadlineExceeded, ConnectOptions, ConnectTimeoutError,
};
pub use domain::{
    ConfigField, DEFAULT_FILE_SPILL_BUDGET_BYTES, DEFAULT_MEMORY_BUDGET_BYTES, DomainLimits,
    DomainLimitsOverrides, DomainLimitsPatch, default_max_outstanding_calls,
};
pub use identity::{validate_relay_id, validate_server_id};
pub use ipc::{
    CHUNK_ASSEMBLER_TIMEOUT, CHUNK_GC_INTERVAL, CHUNK_SIZE, DEFAULT_MAX_PAYLOAD_SIZE,
    DEFAULT_SERVER_STARTUP_TIMEOUT, IDLE_CONNECTION_TIMEOUT, IPC_RELEASE_BATCH_CAPACITY,
    MAX_HANDSHAKE_FRAME_BYTES, MAX_RPC_FRAME_BYTES, RELAY_ANTI_ENTROPY_INTERVAL,
    REMOTE_PAYLOAD_CHUNK_SIZE, SHM_THRESHOLD, ServerIpcConfig, WRITE_TIMEOUT, WRITE_TIMEOUT_SECS,
    default_max_execution_workers,
};
pub use local::{
    LocalEndpoint, LocalEndpointContext, LocalEndpointNamespace, LocalEndpointOptions,
    UNIX_NAMESPACE_FORMAT_VERSION,
};
pub use pool::{
    MAX_BUDDY_SEGMENT_BYTES, MAX_DEDICATED_SEGMENTS, MAX_IPC_POOL_SEGMENTS,
    MIN_BUDDY_SEGMENT_BYTES, POOL_DECAY_SECONDS, PRIMARY_MAX_POOL_SEGMENTS, PoolConfig,
    REASSEMBLY_MAX_POOL_SEGMENTS, SPILL_THRESHOLD, default_spill_dir,
};
pub use relay::RelayConfig;
pub use resolver::{
    ConfigError, ConfigResolver, ConfigSources, EnvCatalog, EnvFilePolicy, EnvMap,
    RelayConfigOverrides, ResolvedRelayClientConfig, ResolvedServerConfig, ServerConfigOverrides,
    ServerConfigPatch,
};
pub use timeout::{TimeoutError, deadline_after, timeout_from_seconds, validate_timeout};
