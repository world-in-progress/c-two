//! Shared configuration and transport policy for C-Two.

mod bridge;
mod call;
mod connect_options;
mod domain;
mod identity;
mod ipc;
mod local;
mod metadata;
mod pool;
mod relay;
mod resolver;
mod timeout;

pub use bridge::{BridgeConfig, DEFAULT_BRIDGE_QUIC_PORT, IrohSeedAddr, RelayBridgeSpec};
pub use call::{CallOptions, CallTimeout};
pub use connect_options::{
    ConnectDeadline, ConnectDeadlineExceeded, ConnectOptions, ConnectTimeoutError,
};
pub use domain::{
    ConfigField, DEFAULT_FILE_SPILL_BUDGET_BYTES, DEFAULT_MEMORY_BUDGET_BYTES, DomainLimits,
    DomainLimitsOverrides, DomainLimitsPatch, default_max_outstanding_calls,
};
pub use identity::{MAX_CALL_ID_LEN, validate_call_id, validate_relay_id, validate_server_id};
pub use ipc::{
    CHUNK_ASSEMBLER_TIMEOUT, CHUNK_GC_INTERVAL, CHUNK_SIZE, DEFAULT_MAX_PAYLOAD_SIZE,
    DEFAULT_OUTCOME_MAX_ENTRIES, DEFAULT_OUTCOME_MAX_RETAINED_BYTES_CAP,
    DEFAULT_SERVER_STARTUP_TIMEOUT, IDLE_CONNECTION_TIMEOUT, IPC_RELEASE_BATCH_CAPACITY,
    MAX_HANDSHAKE_FRAME_BYTES, MAX_RPC_FRAME_BYTES, RELAY_ANTI_ENTROPY_INTERVAL,
    REMOTE_PAYLOAD_CHUNK_SIZE, SHM_THRESHOLD, ServerIpcConfig, WRITE_TIMEOUT, WRITE_TIMEOUT_SECS,
    default_max_execution_workers, default_outcome_max_retained_bytes,
};
pub use local::{
    LocalEndpoint, LocalEndpointContext, LocalEndpointNamespace, LocalEndpointOptions,
    UNIX_NAMESPACE_FORMAT_VERSION,
};
pub use metadata::{
    CallMetadata, CallMetadataError, MAX_CALL_METADATA_ENTRIES, MAX_CALL_METADATA_KEY_LEN,
    MAX_CALL_METADATA_TOTAL_BYTES, MAX_CALL_METADATA_VALUE_LEN, validate_call_metadata_key,
    validate_call_metadata_value,
};
pub use pool::{
    MAX_BUDDY_SEGMENT_BYTES, MAX_DEDICATED_SEGMENTS, MAX_IPC_POOL_SEGMENTS,
    MIN_BUDDY_SEGMENT_BYTES, POOL_DECAY_SECONDS, PRIMARY_MAX_POOL_SEGMENTS, PoolConfig,
    REASSEMBLY_MAX_POOL_SEGMENTS, SPILL_THRESHOLD, default_spill_dir,
};
pub use relay::{RelayConfig, RelayEdgeTlsMode};
pub use resolver::{
    BridgeConfigOverrides, ConfigError, ConfigResolver, ConfigSources, EnvCatalog, EnvFilePolicy,
    EnvMap, RelayConfigOverrides, RelayTlsTrust, ResolvedRelayClientConfig, ResolvedServerConfig,
    ServerConfigOverrides, ServerConfigPatch,
};
pub use timeout::{TimeoutError, deadline_after, timeout_from_seconds, validate_timeout};
