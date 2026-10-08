use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::{
    BaseIpcConfig, CallExecutionLimits, ClientIpcConfig, LocalEndpointContext,
    LocalEndpointOptions, RelayConfig, ServerIpcConfig,
};

pub type EnvMap = BTreeMap<String, String>;
const MAX_RELAY_ROUTE_ATTEMPTS: u64 = 32;

#[derive(Debug, Clone)]
pub enum EnvFilePolicy {
    FromC2EnvFile,
    Disabled,
    Path(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ConfigSources {
    pub env_file: EnvFilePolicy,
    pub process_env: EnvMap,
}

impl ConfigSources {
    pub fn empty() -> Self {
        Self {
            env_file: EnvFilePolicy::Disabled,
            process_env: EnvMap::new(),
        }
    }

    pub fn from_process() -> Self {
        Self {
            env_file: EnvFilePolicy::FromC2EnvFile,
            process_env: std::env::vars().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvCatalog {
    env: EnvMap,
}

impl EnvCatalog {
    pub fn load(sources: ConfigSources) -> Result<Self, ConfigError> {
        Ok(Self {
            env: resolve_env(sources)?,
        })
    }

    fn optional_string(&self, key: &str) -> Option<String> {
        optional_string(&self.env, key)
    }

    /// Present-but-raw value lookup. Unlike `optional_string` this keeps
    /// empty values visible so callers can reject them explicitly instead of
    /// silently falling back to defaults.
    fn raw_optional_string(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    fn optional_bool(&self, key: &str) -> Option<Result<bool, ConfigError>> {
        parse_optional_bool(&self.env, key)
    }

    fn optional_u64(&self, key: &str) -> Option<Result<u64, ConfigError>> {
        parse_optional_u64(&self.env, key)
    }

    fn optional_u32(&self, key: &str) -> Option<Result<u32, ConfigError>> {
        parse_optional_u32(&self.env, key)
    }

    fn optional_f64(&self, key: &str) -> Option<Result<f64, ConfigError>> {
        parse_optional_f64(&self.env, key)
    }

    fn optional_list(&self, key: &str) -> Option<Vec<String>> {
        self.optional_string(key).map(|value| parse_list(&value))
    }
}

#[derive(Debug, Clone, Default)]
pub struct BaseIpcConfigOverrides {
    pub pool_enabled: Option<bool>,
    pub pool_segment_size: Option<u64>,
    pub max_pool_segments: Option<u32>,
    pub pool_prewarm_segments: Option<u32>,
    pub pool_min_retained_segments: Option<u32>,
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
    pub shm_backing_budget_bytes: Option<u64>,
    pub file_backing_budget_bytes: Option<u64>,
    pub live_reassembly_budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerIpcConfigOverrides {
    pub base: BaseIpcConfigOverrides,
    pub pool_enabled: Option<bool>,
    pub pool_segment_size: Option<u64>,
    pub max_pool_segments: Option<u32>,
    pub pool_prewarm_segments: Option<u32>,
    pub pool_min_retained_segments: Option<u32>,
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
    pub shm_backing_budget_bytes: Option<u64>,
    pub file_backing_budget_bytes: Option<u64>,
    pub live_reassembly_budget_bytes: Option<u64>,
    pub max_frame_size: Option<u64>,
    pub max_payload_size: Option<u64>,
    pub max_pending_requests: Option<u32>,
    pub max_execution_workers: Option<u32>,
    pub pool_decay_seconds: Option<f64>,
    pub heartbeat_interval_secs: Option<f64>,
    pub heartbeat_timeout_secs: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientIpcConfigOverrides {
    pub base: BaseIpcConfigOverrides,
    pub pool_enabled: Option<bool>,
    pub pool_segment_size: Option<u64>,
    pub max_pool_segments: Option<u32>,
    pub pool_prewarm_segments: Option<u32>,
    pub pool_min_retained_segments: Option<u32>,
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
    pub pool_decay_seconds: Option<f64>,
    pub shm_backing_budget_bytes: Option<u64>,
    pub file_backing_budget_bytes: Option<u64>,
    pub live_reassembly_budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayConfigOverrides {
    pub bind: Option<String>,
    pub relay_id: Option<String>,
    pub advertise_url: Option<String>,
    pub seeds: Option<Vec<String>>,
    pub idle_timeout_secs: Option<u64>,
    pub anti_entropy_interval_secs: Option<f64>,
}

/// Typed code-level overrides for [`CallExecutionLimits`] resolution.
///
/// Each dimension is independent: `None` falls through to the process
/// environment, then the `.env` file, then the canonical default, while
/// `Some(value)` is a finite `u64` admission limit used verbatim — `Some(0)`
/// closes that dimension and never means unset.
#[derive(Debug, Clone, Default)]
pub struct CallExecutionLimitsOverrides {
    pub max_outstanding_calls: Option<u64>,
    pub retained_input_budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeConfigOverrides {
    pub relay_anchor_address: Option<String>,
    pub relay: RelayConfigOverrides,
    pub server_ipc: ServerIpcConfigOverrides,
    pub client_ipc: ClientIpcConfigOverrides,
    pub shm_threshold: Option<u64>,
    pub relay_use_proxy: Option<bool>,
    pub remote_payload_chunk_size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRuntimeConfig {
    pub relay_anchor_address: Option<String>,
    pub relay: RelayConfig,
    pub server_ipc: ServerIpcConfig,
    pub client_ipc: ClientIpcConfig,
    pub shm_threshold: u64,
    pub relay_use_proxy: bool,
    pub remote_payload_chunk_size: u64,
}

#[derive(Debug, Clone)]
pub struct ResolvedRelayConfig {
    pub relay_anchor_address: Option<String>,
    pub relay: RelayConfig,
    pub relay_use_proxy: bool,
    pub remote_payload_chunk_size: u64,
}

#[derive(Debug, Clone)]
pub struct ResolvedRelayClientConfig {
    pub relay_anchor_address: Option<String>,
    pub relay_use_proxy: bool,
    pub relay_route_max_attempts: usize,
    pub relay_call_timeout_secs: f64,
    pub remote_payload_chunk_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

pub struct ConfigResolver;

impl ConfigResolver {
    pub fn resolve(
        overrides: RuntimeConfigOverrides,
        sources: ConfigSources,
    ) -> Result<ResolvedRuntimeConfig, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;

        let shm_threshold = resolve_shm_threshold(&catalog, overrides.shm_threshold)?;

        let relay = resolve_relay_server_config(&catalog, overrides.relay.clone())?;
        let relay_client = resolve_relay_client_config(&catalog, &overrides)?;
        let client_ipc = resolve_client_ipc_config(&catalog, overrides.client_ipc, shm_threshold)?;
        let relay = RelayConfig {
            upstream_ipc: client_ipc.clone(),
            use_proxy: relay_client.relay_use_proxy,
            remote_payload_chunk_size: relay_client.remote_payload_chunk_size,
            ..relay
        };
        let server_ipc = resolve_server_ipc_config(&catalog, overrides.server_ipc, shm_threshold)?;
        relay.validate().map_err(ConfigError::new)?;

        Ok(ResolvedRuntimeConfig {
            relay_anchor_address: relay_client.relay_anchor_address,
            relay,
            server_ipc,
            client_ipc,
            shm_threshold,
            relay_use_proxy: relay_client.relay_use_proxy,
            remote_payload_chunk_size: relay_client.remote_payload_chunk_size,
        })
    }

    pub fn resolve_relay_server(
        overrides: RuntimeConfigOverrides,
        sources: ConfigSources,
    ) -> Result<ResolvedRelayConfig, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        let relay = resolve_relay_server_config(&catalog, overrides.relay.clone())?;
        let relay_anchor_address = overrides
            .relay_anchor_address
            .clone()
            .or_else(|| catalog.optional_string("C2_RELAY_ANCHOR_ADDRESS"));
        let relay_use_proxy = resolve_relay_use_proxy(&catalog, &overrides)?;
        let remote_payload_chunk_size =
            resolve_remote_payload_chunk_size(&catalog, overrides.remote_payload_chunk_size)?;
        let shm_threshold = resolve_shm_threshold(&catalog, overrides.shm_threshold)?;
        let upstream_ipc =
            resolve_client_ipc_config(&catalog, overrides.client_ipc, shm_threshold)?;
        let relay = RelayConfig {
            upstream_ipc,
            use_proxy: relay_use_proxy,
            remote_payload_chunk_size,
            ..relay
        };

        relay.validate().map_err(ConfigError::new)?;

        Ok(ResolvedRelayConfig {
            relay_anchor_address,
            relay,
            relay_use_proxy,
            remote_payload_chunk_size,
        })
    }

    pub fn resolve_relay_anchor_address(
        sources: ConfigSources,
    ) -> Result<Option<String>, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        Ok(catalog.optional_string("C2_RELAY_ANCHOR_ADDRESS"))
    }

    pub fn resolve_relay_use_proxy(sources: ConfigSources) -> Result<bool, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        resolve_relay_use_proxy(&catalog, &RuntimeConfigOverrides::default())
    }

    pub fn resolve_relay_route_max_attempts(sources: ConfigSources) -> Result<usize, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        resolve_relay_route_max_attempts(&catalog)
    }

    pub fn resolve_relay_call_timeout_secs(sources: ConfigSources) -> Result<f64, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        resolve_relay_call_timeout_secs(&catalog)
    }

    pub fn resolve_remote_payload_chunk_size(
        override_value: Option<u64>,
        sources: ConfigSources,
    ) -> Result<u64, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        resolve_remote_payload_chunk_size(&catalog, override_value)
    }

    pub fn resolve_server_ipc(
        overrides: ServerIpcConfigOverrides,
        global_overrides: RuntimeConfigOverrides,
        sources: ConfigSources,
    ) -> Result<ServerIpcConfig, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        let shm_threshold = resolve_shm_threshold(&catalog, global_overrides.shm_threshold)?;
        resolve_server_ipc_config(&catalog, overrides, shm_threshold)
    }

    pub fn resolve_client_ipc(
        overrides: ClientIpcConfigOverrides,
        global_overrides: RuntimeConfigOverrides,
        sources: ConfigSources,
    ) -> Result<ClientIpcConfig, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        let shm_threshold = resolve_shm_threshold(&catalog, global_overrides.shm_threshold)?;
        resolve_client_ipc_config(&catalog, overrides, shm_threshold)
    }

    pub fn resolve_shm_threshold(
        override_value: Option<u64>,
        sources: ConfigSources,
    ) -> Result<u64, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;
        resolve_shm_threshold(&catalog, override_value)
    }

    /// Resolve the bounded deadline transaction admission limits from typed
    /// overrides and configuration sources: explicit code > process env >
    /// `.env` > the canonical [`CallExecutionLimits`] default. Both
    /// dimensions are finite `u64` admission limits — an explicit or
    /// environmental `0` rejects every positive request in that dimension
    /// and never means unlimited or unset. A present-but-empty or
    /// whitespace-only environment value is rejected instead of silently
    /// falling back to the default, and every other invalid value names its
    /// variable in the error.
    pub fn resolve_call_execution_limits(
        overrides: CallExecutionLimitsOverrides,
        sources: ConfigSources,
    ) -> Result<CallExecutionLimits, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;

        let max_outstanding_calls = match overrides.max_outstanding_calls {
            Some(value) => value,
            None => call_execution_limit_from_env(&catalog, "C2_CALL_MAX_OUTSTANDING")?
                .unwrap_or(crate::DEFAULT_MAX_OUTSTANDING_CALLS),
        };
        let retained_input_budget_bytes = match overrides.retained_input_budget_bytes {
            Some(value) => value,
            None => call_execution_limit_from_env(&catalog, "C2_CALL_RETAINED_INPUT_BUDGET_BYTES")?
                .unwrap_or(crate::DEFAULT_RETAINED_INPUT_BUDGET_BYTES),
        };

        Ok(CallExecutionLimits {
            max_outstanding_calls,
            retained_input_budget_bytes,
        })
    }

    /// Resolve the immutable local endpoint context from typed options and
    /// configuration sources: explicit code > process env > `.env` > platform
    /// default. The resolved root is validated without touching the
    /// filesystem; the root container must be pre-created by the application.
    /// An explicit empty, relative, `..`-bearing, NUL-bearing, or non-UTF-8
    /// root is rejected instead of silently falling back to the default.
    /// Environment values reach native validation verbatim — a trailing space
    /// is a legal Unix directory name and must resolve identically to the
    /// same code-level path, so nothing is trimmed into or out of validity.
    pub fn resolve_local_endpoint(
        options: LocalEndpointOptions,
        sources: ConfigSources,
    ) -> Result<LocalEndpointContext, ConfigError> {
        let catalog = EnvCatalog::load(sources)?;

        let (root, source_name) = match options.unix_root {
            Some(root) => (root, "local endpoint root option".to_owned()),
            None => match catalog.raw_optional_string("C2_IPC_ROOT") {
                // Only a present-but-empty value is rejected outright;
                // whitespace-only and leading-whitespace values keep their
                // raw bytes and fail the native absolute-path check.
                Some(raw) => {
                    if raw.is_empty() {
                        return Err(ConfigError::new("C2_IPC_ROOT cannot be empty"));
                    }
                    (PathBuf::from(raw), "C2_IPC_ROOT".to_owned())
                }
                None => {
                    return LocalEndpointContext::default_for_platform().map_err(|error| {
                        ConfigError::new(format!(
                            "default local endpoint context unavailable: {error}"
                        ))
                    });
                }
            },
        };

        LocalEndpointContext::with_unix_root(&root)
            .map_err(|error| ConfigError::new(format!("{source_name}: {error}")))
    }
}

fn resolve_relay_client_config(
    catalog: &EnvCatalog,
    overrides: &RuntimeConfigOverrides,
) -> Result<ResolvedRelayClientConfig, ConfigError> {
    let relay_anchor_address = overrides
        .relay_anchor_address
        .clone()
        .or_else(|| catalog.optional_string("C2_RELAY_ANCHOR_ADDRESS"));
    let relay_use_proxy = resolve_relay_use_proxy(catalog, overrides)?;
    let relay_route_max_attempts = resolve_relay_route_max_attempts(catalog)?;
    let relay_call_timeout_secs = resolve_relay_call_timeout_secs(catalog)?;
    let remote_payload_chunk_size =
        resolve_remote_payload_chunk_size(catalog, overrides.remote_payload_chunk_size)?;

    Ok(ResolvedRelayClientConfig {
        relay_anchor_address,
        relay_use_proxy,
        relay_route_max_attempts,
        relay_call_timeout_secs,
        remote_payload_chunk_size,
    })
}

fn resolve_relay_use_proxy(
    catalog: &EnvCatalog,
    overrides: &RuntimeConfigOverrides,
) -> Result<bool, ConfigError> {
    match overrides.relay_use_proxy {
        Some(value) => Ok(value),
        None => Ok(catalog
            .optional_bool("C2_RELAY_USE_PROXY")
            .transpose()?
            .unwrap_or(false)),
    }
}

fn resolve_relay_route_max_attempts(catalog: &EnvCatalog) -> Result<usize, ConfigError> {
    let attempts = catalog
        .optional_u64("C2_RELAY_ROUTE_MAX_ATTEMPTS")
        .transpose()?
        .unwrap_or(3)
        .max(1);
    if attempts > MAX_RELAY_ROUTE_ATTEMPTS {
        return Err(ConfigError::new(format!(
            "C2_RELAY_ROUTE_MAX_ATTEMPTS must be <= {MAX_RELAY_ROUTE_ATTEMPTS}"
        )));
    }
    Ok(attempts as usize)
}

fn resolve_relay_call_timeout_secs(catalog: &EnvCatalog) -> Result<f64, ConfigError> {
    let timeout = catalog
        .optional_f64("C2_RELAY_CALL_TIMEOUT")
        .transpose()?
        .unwrap_or(300.0);
    duration_from_secs("C2_RELAY_CALL_TIMEOUT", timeout)?;
    Ok(timeout)
}

fn resolve_remote_payload_chunk_size(
    catalog: &EnvCatalog,
    override_value: Option<u64>,
) -> Result<u64, ConfigError> {
    let value = match override_value {
        Some(value) => value,
        None => catalog
            .optional_u64("C2_REMOTE_PAYLOAD_CHUNK_SIZE")
            .transpose()?
            .unwrap_or(crate::DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE),
    };
    crate::validate_remote_payload_chunk_size(value)
        .map_err(|reason| ConfigError::new(format!("C2_REMOTE_PAYLOAD_CHUNK_SIZE {reason}")))?;
    Ok(value)
}

fn resolve_env(sources: ConfigSources) -> Result<EnvMap, ConfigError> {
    let mut env = EnvMap::new();

    let env_file = match sources.env_file {
        EnvFilePolicy::Disabled => None,
        EnvFilePolicy::Path(path) => Some(path),
        EnvFilePolicy::FromC2EnvFile => {
            match sources.process_env.get("C2_ENV_FILE").map(|v| v.trim()) {
                Some("") => None,
                Some(path) => Some(PathBuf::from(path)),
                None => Some(PathBuf::from(".env")),
            }
        }
    };

    if let Some(path) = env_file {
        match dotenvy::from_path_iter(&path) {
            Ok(iter) => {
                for item in iter {
                    let (key, value) = item.map_err(|e| {
                        ConfigError::new(format!(
                            "failed to parse env file {}: {e}",
                            path.display()
                        ))
                    })?;
                    env.insert(key, value);
                }
            }
            Err(dotenvy::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ConfigError::new(format!(
                    "failed to load env file {}: {e}",
                    path.display(),
                )));
            }
        }
    }

    for (key, value) in sources.process_env {
        env.insert(key, value);
    }

    Ok(env)
}

fn resolve_relay_server_config(
    catalog: &EnvCatalog,
    overrides: RelayConfigOverrides,
) -> Result<RelayConfig, ConfigError> {
    let mut cfg = RelayConfig::default();

    if let Some(v) = catalog.optional_string("C2_RELAY_BIND") {
        cfg.bind = v;
    }
    if let Some(v) = catalog.optional_string("C2_RELAY_ID") {
        cfg.relay_id = v;
    }
    if let Some(v) = catalog.optional_string("C2_RELAY_ADVERTISE_URL") {
        cfg.advertise_url = v;
    }
    if let Some(v) = catalog.optional_list("C2_RELAY_SEEDS") {
        cfg.seeds = v;
    }
    if let Some(v) = catalog.optional_u64("C2_RELAY_IDLE_TIMEOUT").transpose()? {
        cfg.idle_timeout_secs = validate_millis_timeout("C2_RELAY_IDLE_TIMEOUT", v)?;
    }
    if let Some(v) = catalog
        .optional_f64("C2_RELAY_ANTI_ENTROPY_INTERVAL")
        .transpose()?
    {
        cfg.anti_entropy_interval = duration_from_secs("C2_RELAY_ANTI_ENTROPY_INTERVAL", v)?;
    }

    if let Some(v) = clean_string(overrides.bind) {
        cfg.bind = v;
    }
    if let Some(v) = clean_string(overrides.relay_id) {
        cfg.relay_id = v;
    }
    if let Some(v) = clean_string(overrides.advertise_url) {
        cfg.advertise_url = v;
    }
    if let Some(v) = overrides.seeds {
        cfg.seeds = v;
    }
    if let Some(v) = overrides.idle_timeout_secs {
        cfg.idle_timeout_secs = validate_millis_timeout("idle_timeout_secs", v)?;
    }
    if let Some(v) = overrides.anti_entropy_interval_secs {
        cfg.anti_entropy_interval = duration_from_secs("anti_entropy_interval_secs", v)?;
    }

    cfg.validate().map_err(ConfigError::new)?;
    Ok(cfg)
}

fn resolve_server_ipc_config(
    catalog: &EnvCatalog,
    overrides: ServerIpcConfigOverrides,
    shm_threshold: u64,
) -> Result<ServerIpcConfig, ConfigError> {
    let mut cfg = ServerIpcConfig {
        shm_threshold,
        ..ServerIpcConfig::default()
    };

    apply_base_env(&mut cfg.base, catalog)?;
    if let Some(v) = catalog.optional_u64("C2_IPC_MAX_FRAME_SIZE").transpose()? {
        cfg.max_frame_size = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_MAX_PAYLOAD_SIZE")
        .transpose()?
    {
        cfg.max_payload_size = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_MAX_PENDING_REQUESTS")
        .transpose()?
    {
        cfg.max_pending_requests = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_MAX_EXECUTION_WORKERS")
        .transpose()?
    {
        cfg.max_execution_workers = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_POOL_DECAY_SECONDS")
        .transpose()?
    {
        cfg.pool_decay_seconds = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_HEARTBEAT_INTERVAL")
        .transpose()?
    {
        cfg.heartbeat_interval_secs = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_HEARTBEAT_TIMEOUT")
        .transpose()?
    {
        cfg.heartbeat_timeout_secs = v;
    }

    apply_base_overrides(&mut cfg.base, &overrides.base);
    apply_flat_base_overrides_to_server(&mut cfg, &overrides);
    if let Some(v) = overrides.max_frame_size {
        cfg.max_frame_size = v;
    }
    if let Some(v) = overrides.max_payload_size {
        cfg.max_payload_size = v;
    }
    if let Some(v) = overrides.max_pending_requests {
        cfg.max_pending_requests = v;
    }
    if let Some(v) = overrides.max_execution_workers {
        cfg.max_execution_workers = v;
    }
    if let Some(v) = overrides.pool_decay_seconds {
        cfg.pool_decay_seconds = v;
    }
    if let Some(v) = overrides.heartbeat_interval_secs {
        cfg.heartbeat_interval_secs = v;
    }
    if let Some(v) = overrides.heartbeat_timeout_secs {
        cfg.heartbeat_timeout_secs = v;
    }

    derive_base(&mut cfg.base)?;
    cfg.validate().map_err(ConfigError::new)?;
    Ok(cfg)
}

fn resolve_client_ipc_config(
    catalog: &EnvCatalog,
    overrides: ClientIpcConfigOverrides,
    shm_threshold: u64,
) -> Result<ClientIpcConfig, ConfigError> {
    let mut cfg = ClientIpcConfig {
        shm_threshold,
        ..ClientIpcConfig::default()
    };

    apply_base_env(&mut cfg.base, catalog)?;
    if let Some(v) = catalog
        .optional_f64("C2_IPC_POOL_DECAY_SECONDS")
        .transpose()?
    {
        cfg.pool_decay_seconds = v;
    }
    apply_base_overrides(&mut cfg.base, &overrides.base);
    apply_flat_base_overrides_to_client(&mut cfg, &overrides);
    if let Some(v) = overrides.pool_decay_seconds {
        cfg.pool_decay_seconds = v;
    }

    derive_base(&mut cfg.base)?;
    cfg.validate().map_err(ConfigError::new)?;
    Ok(cfg)
}

fn apply_base_env(cfg: &mut BaseIpcConfig, catalog: &EnvCatalog) -> Result<(), ConfigError> {
    if let Some(v) = catalog.optional_bool("C2_IPC_POOL_ENABLED").transpose()? {
        cfg.pool_enabled = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_POOL_SEGMENT_SIZE")
        .transpose()?
    {
        cfg.pool_segment_size = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_MAX_POOL_SEGMENTS")
        .transpose()?
    {
        cfg.max_pool_segments = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_POOL_PREWARM_SEGMENTS")
        .transpose()?
    {
        cfg.pool_prewarm_segments = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_POOL_MIN_RETAINED_SEGMENTS")
        .transpose()?
    {
        cfg.pool_min_retained_segments = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_REASSEMBLY_SEGMENT_SIZE")
        .transpose()?
    {
        cfg.reassembly_segment_size = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_REASSEMBLY_MAX_SEGMENTS")
        .transpose()?
    {
        cfg.reassembly_max_segments = v;
    }
    if let Some(v) = catalog
        .optional_u32("C2_IPC_MAX_TOTAL_CHUNKS")
        .transpose()?
    {
        cfg.max_total_chunks = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_CHUNK_GC_INTERVAL")
        .transpose()?
    {
        cfg.chunk_gc_interval_secs = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_CHUNK_THRESHOLD_RATIO")
        .transpose()?
    {
        cfg.chunk_threshold_ratio = v;
    }
    if let Some(v) = catalog
        .optional_f64("C2_IPC_CHUNK_ASSEMBLER_TIMEOUT")
        .transpose()?
    {
        cfg.chunk_assembler_timeout_secs = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_MAX_REASSEMBLY_BYTES")
        .transpose()?
    {
        cfg.max_reassembly_bytes = v;
    }
    if let Some(v) = catalog.optional_u64("C2_IPC_CHUNK_SIZE").transpose()? {
        cfg.chunk_size = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_SHM_BACKING_BUDGET_BYTES")
        .transpose()?
    {
        cfg.shm_backing_budget_bytes = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_FILE_BACKING_BUDGET_BYTES")
        .transpose()?
    {
        cfg.file_backing_budget_bytes = v;
    }
    if let Some(v) = catalog
        .optional_u64("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES")
        .transpose()?
    {
        cfg.live_reassembly_budget_bytes = v;
    }
    Ok(())
}

fn apply_base_overrides(cfg: &mut BaseIpcConfig, overrides: &BaseIpcConfigOverrides) {
    if let Some(v) = overrides.pool_enabled {
        cfg.pool_enabled = v;
    }
    if let Some(v) = overrides.pool_segment_size {
        cfg.pool_segment_size = v;
    }
    if let Some(v) = overrides.max_pool_segments {
        cfg.max_pool_segments = v;
    }
    if let Some(v) = overrides.pool_prewarm_segments {
        cfg.pool_prewarm_segments = v;
    }
    if let Some(v) = overrides.pool_min_retained_segments {
        cfg.pool_min_retained_segments = v;
    }
    if let Some(v) = overrides.reassembly_segment_size {
        cfg.reassembly_segment_size = v;
    }
    if let Some(v) = overrides.reassembly_max_segments {
        cfg.reassembly_max_segments = v;
    }
    if let Some(v) = overrides.max_total_chunks {
        cfg.max_total_chunks = v;
    }
    if let Some(v) = overrides.chunk_gc_interval_secs {
        cfg.chunk_gc_interval_secs = v;
    }
    if let Some(v) = overrides.chunk_threshold_ratio {
        cfg.chunk_threshold_ratio = v;
    }
    if let Some(v) = overrides.chunk_assembler_timeout_secs {
        cfg.chunk_assembler_timeout_secs = v;
    }
    if let Some(v) = overrides.max_reassembly_bytes {
        cfg.max_reassembly_bytes = v;
    }
    if let Some(v) = overrides.chunk_size {
        cfg.chunk_size = v;
    }
    if let Some(v) = overrides.shm_backing_budget_bytes {
        cfg.shm_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.file_backing_budget_bytes {
        cfg.file_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.live_reassembly_budget_bytes {
        cfg.live_reassembly_budget_bytes = v;
    }
}

fn resolve_shm_threshold(
    catalog: &EnvCatalog,
    override_value: Option<u64>,
) -> Result<u64, ConfigError> {
    let shm_threshold = match override_value {
        Some(value) => value,
        None => catalog
            .optional_u64("C2_SHM_THRESHOLD")
            .transpose()?
            .unwrap_or(4096),
    };
    if shm_threshold == 0 {
        return Err(ConfigError::new("shm_threshold must be > 0"));
    }
    Ok(shm_threshold)
}

/// Present-but-raw finite-`u64` lookup for one call execution limit.
///
/// An absent key is `None`; a present value must carry non-empty unsigned
/// integer text. An empty or whitespace-only value is rejected instead of
/// silently becoming the default, and negative, non-numeric, or overflowing
/// text fails with the variable named. `0` is a present finite value, never
/// unset.
fn call_execution_limit_from_env(
    catalog: &EnvCatalog,
    key: &str,
) -> Result<Option<u64>, ConfigError> {
    let raw = match catalog.raw_optional_string(key) {
        Some(raw) => raw,
        None => return Ok(None),
    };
    let value = raw.trim();
    if value.is_empty() {
        return Err(ConfigError::new(format!("{key} cannot be empty")));
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|e| ConfigError::new(format!("{key} must be an unsigned integer: {e}")))
}

fn apply_flat_base_overrides_to_server(
    cfg: &mut ServerIpcConfig,
    overrides: &ServerIpcConfigOverrides,
) {
    if let Some(v) = overrides.pool_enabled {
        cfg.base.pool_enabled = v;
    }
    if let Some(v) = overrides.pool_segment_size {
        cfg.base.pool_segment_size = v;
    }
    if let Some(v) = overrides.max_pool_segments {
        cfg.base.max_pool_segments = v;
    }
    if let Some(v) = overrides.pool_prewarm_segments {
        cfg.base.pool_prewarm_segments = v;
    }
    if let Some(v) = overrides.pool_min_retained_segments {
        cfg.base.pool_min_retained_segments = v;
    }
    if let Some(v) = overrides.reassembly_segment_size {
        cfg.base.reassembly_segment_size = v;
    }
    if let Some(v) = overrides.reassembly_max_segments {
        cfg.base.reassembly_max_segments = v;
    }
    if let Some(v) = overrides.max_total_chunks {
        cfg.base.max_total_chunks = v;
    }
    if let Some(v) = overrides.chunk_gc_interval_secs {
        cfg.base.chunk_gc_interval_secs = v;
    }
    if let Some(v) = overrides.chunk_threshold_ratio {
        cfg.base.chunk_threshold_ratio = v;
    }
    if let Some(v) = overrides.chunk_assembler_timeout_secs {
        cfg.base.chunk_assembler_timeout_secs = v;
    }
    if let Some(v) = overrides.max_reassembly_bytes {
        cfg.base.max_reassembly_bytes = v;
    }
    if let Some(v) = overrides.chunk_size {
        cfg.base.chunk_size = v;
    }
    if let Some(v) = overrides.shm_backing_budget_bytes {
        cfg.base.shm_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.file_backing_budget_bytes {
        cfg.base.file_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.live_reassembly_budget_bytes {
        cfg.base.live_reassembly_budget_bytes = v;
    }
}

fn apply_flat_base_overrides_to_client(
    cfg: &mut ClientIpcConfig,
    overrides: &ClientIpcConfigOverrides,
) {
    if let Some(v) = overrides.pool_enabled {
        cfg.base.pool_enabled = v;
    }
    if let Some(v) = overrides.pool_segment_size {
        cfg.base.pool_segment_size = v;
    }
    if let Some(v) = overrides.max_pool_segments {
        cfg.base.max_pool_segments = v;
    }
    if let Some(v) = overrides.pool_prewarm_segments {
        cfg.base.pool_prewarm_segments = v;
    }
    if let Some(v) = overrides.pool_min_retained_segments {
        cfg.base.pool_min_retained_segments = v;
    }
    if let Some(v) = overrides.reassembly_segment_size {
        cfg.base.reassembly_segment_size = v;
    }
    if let Some(v) = overrides.reassembly_max_segments {
        cfg.base.reassembly_max_segments = v;
    }
    if let Some(v) = overrides.max_total_chunks {
        cfg.base.max_total_chunks = v;
    }
    if let Some(v) = overrides.chunk_gc_interval_secs {
        cfg.base.chunk_gc_interval_secs = v;
    }
    if let Some(v) = overrides.chunk_threshold_ratio {
        cfg.base.chunk_threshold_ratio = v;
    }
    if let Some(v) = overrides.chunk_assembler_timeout_secs {
        cfg.base.chunk_assembler_timeout_secs = v;
    }
    if let Some(v) = overrides.max_reassembly_bytes {
        cfg.base.max_reassembly_bytes = v;
    }
    if let Some(v) = overrides.chunk_size {
        cfg.base.chunk_size = v;
    }
    if let Some(v) = overrides.shm_backing_budget_bytes {
        cfg.base.shm_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.file_backing_budget_bytes {
        cfg.base.file_backing_budget_bytes = v;
    }
    if let Some(v) = overrides.live_reassembly_budget_bytes {
        cfg.base.live_reassembly_budget_bytes = v;
    }
}

fn derive_base(cfg: &mut BaseIpcConfig) -> Result<(), ConfigError> {
    cfg.max_pool_memory = cfg
        .pool_segment_size
        .checked_mul(u64::from(cfg.max_pool_segments))
        .ok_or_else(|| ConfigError::new("max_pool_memory derived value overflowed"))?;
    Ok(())
}

fn optional_string(env: &EnvMap, key: &str) -> Option<String> {
    env.get(key).and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn clean_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        if value.is_empty() { None } else { Some(value) }
    })
}

fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_optional_u64(env: &EnvMap, key: &str) -> Option<Result<u64, ConfigError>> {
    optional_string(env, key).map(|value| {
        value
            .parse::<u64>()
            .map_err(|e| ConfigError::new(format!("{key} must be an unsigned integer: {e}")))
    })
}

fn parse_optional_u32(env: &EnvMap, key: &str) -> Option<Result<u32, ConfigError>> {
    optional_string(env, key).map(|value| {
        value
            .parse::<u32>()
            .map_err(|e| ConfigError::new(format!("{key} must be an unsigned integer: {e}")))
    })
}

fn parse_optional_f64(env: &EnvMap, key: &str) -> Option<Result<f64, ConfigError>> {
    optional_string(env, key).map(|value| {
        let parsed = value
            .parse::<f64>()
            .map_err(|e| ConfigError::new(format!("{key} must be a number: {e}")))?;
        validate_finite(key, parsed)
    })
}

fn parse_optional_bool(env: &EnvMap, key: &str) -> Option<Result<bool, ConfigError>> {
    optional_string(env, key).map(|value| match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(ConfigError::new(format!(
            "{key} must be a boolean (1/0, true/false, yes/no)"
        ))),
    })
}

fn duration_from_secs(name: &str, secs: f64) -> Result<Duration, ConfigError> {
    let secs = validate_finite(name, secs)?;
    if secs < 0.0 {
        return Err(ConfigError::new(format!("{name} must be >= 0")));
    }
    Duration::try_from_secs_f64(secs).map_err(|_| {
        ConfigError::new(format!(
            "{name} must be a representable duration in seconds"
        ))
    })
}

fn validate_millis_timeout(name: &str, secs: u64) -> Result<u64, ConfigError> {
    secs.checked_mul(1000)
        .ok_or_else(|| ConfigError::new(format!("{name} must fit in milliseconds")))?;
    Ok(secs)
}

fn validate_finite(name: &str, value: f64) -> Result<f64, ConfigError> {
    if !value.is_finite() {
        return Err(ConfigError::new(format!("{name} must be finite")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn env(entries: &[(&str, &str)]) -> EnvMap {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn resolver_uses_canonical_defaults() {
        let resolved =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), ConfigSources::empty())
                .expect("defaults should resolve");

        assert_eq!(resolved.shm_threshold, 4096);
        assert_eq!(
            resolved.server_ipc.reassembly_segment_size,
            64 * 1024 * 1024
        );
        assert_eq!(
            resolved.client_ipc.reassembly_segment_size,
            64 * 1024 * 1024
        );
        assert_eq!(resolved.server_ipc.max_pool_memory, 268_435_456 * 4);
        assert_eq!(resolved.client_ipc.max_pool_memory, 268_435_456 * 4);
        assert_eq!(resolved.relay.bind, "0.0.0.0:8080");
        assert_eq!(resolved.relay.idle_timeout_secs, 60);
        assert!(!resolved.relay_use_proxy);
    }

    #[test]
    fn code_overrides_beat_process_env_which_beats_env_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let env_file = tempdir.path().join(".env");
        fs::write(
            &env_file,
            [
                "C2_SHM_THRESHOLD=8192",
                "C2_IPC_POOL_SEGMENT_SIZE=1048576",
                "C2_RELAY_BIND=127.0.0.1:7000",
            ]
            .join("\n"),
        )
        .expect("write env file");

        let overrides = RuntimeConfigOverrides {
            shm_threshold: Some(16_384),
            server_ipc: ServerIpcConfigOverrides {
                pool_segment_size: Some(4_194_304),
                ..ServerIpcConfigOverrides::default()
            },
            ..RuntimeConfigOverrides::default()
        };

        let sources = ConfigSources {
            env_file: EnvFilePolicy::Path(env_file),
            process_env: env(&[
                ("C2_SHM_THRESHOLD", "12288"),
                ("C2_IPC_POOL_SEGMENT_SIZE", "2097152"),
                ("C2_RELAY_BIND", "127.0.0.1:8000"),
            ]),
        };

        let resolved = ConfigResolver::resolve(overrides, sources).expect("resolve");

        assert_eq!(resolved.shm_threshold, 16_384);
        assert_eq!(resolved.server_ipc.pool_segment_size, 4_194_304);
        assert_eq!(resolved.server_ipc.max_pool_memory, 4_194_304 * 4);
        assert_eq!(resolved.relay.bind, "127.0.0.1:8000");
    }

    #[test]
    fn env_file_empty_string_disables_env_file_loading() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        fs::write(tempdir.path().join(".env"), "C2_SHM_THRESHOLD=8192\n").expect("write env file");

        let sources = ConfigSources {
            env_file: EnvFilePolicy::FromC2EnvFile,
            process_env: env(&[("C2_ENV_FILE", "")]),
        };

        let resolved =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources).expect("resolve");

        assert_eq!(resolved.shm_threshold, 4096);
    }

    #[test]
    fn server_pool_segment_capacity_is_independent_of_payload_limit() {
        // Pool segment capacity and the per-message limit are independent
        // dimensions (0.7.1 design section 3): the default 256 MiB segment
        // resolves beside a 32 MiB message cap with the buddy pool enabled
        // and disabled. Oversized individual payloads stay a dispatch-level
        // check, not a resolver validation rule.
        let mut overrides = RuntimeConfigOverrides::default();
        overrides.server_ipc.max_payload_size = Some(32 * 1024 * 1024);

        let resolved = ConfigResolver::resolve(overrides, ConfigSources::empty())
            .expect("large pool segment with a smaller message cap should resolve");
        assert_eq!(resolved.server_ipc.pool_segment_size, 256 * 1024 * 1024);
        assert_eq!(resolved.server_ipc.max_pool_memory, 1024 * 1024 * 1024);
        assert_eq!(resolved.server_ipc.max_payload_size, 32 * 1024 * 1024);
        assert!(resolved.server_ipc.pool_enabled);

        let mut overrides = RuntimeConfigOverrides::default();
        overrides.server_ipc.max_payload_size = Some(32 * 1024 * 1024);
        overrides.server_ipc.pool_enabled = Some(false);

        let resolved = ConfigResolver::resolve(overrides, ConfigSources::empty())
            .expect("buddy-off resolution must also allow segment > message cap");
        assert!(!resolved.server_ipc.pool_enabled);
        assert_eq!(resolved.server_ipc.pool_segment_size, 256 * 1024 * 1024);
        assert_eq!(resolved.server_ipc.max_payload_size, 32 * 1024 * 1024);
    }

    #[test]
    fn invalid_env_value_names_variable() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_POOL_SEGMENT_SIZE", "not-a-number")]),
        };

        let err = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
            .expect_err("invalid value should fail");

        assert!(err.to_string().contains("C2_IPC_POOL_SEGMENT_SIZE"));
    }

    #[test]
    fn non_finite_float_env_value_is_rejected() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_ANTI_ENTROPY_INTERVAL", "NaN")]),
        };

        let err = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
            .expect_err("non-finite floats should fail");

        assert!(err.to_string().contains("C2_RELAY_ANTI_ENTROPY_INTERVAL"));
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn oversized_relay_duration_env_value_is_rejected() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_ANTI_ENTROPY_INTERVAL", "1e100")]),
        };

        let err = ConfigResolver::resolve_relay_server(RuntimeConfigOverrides::default(), sources)
            .expect_err("oversized relay duration should fail without panicking");

        assert!(err.to_string().contains("C2_RELAY_ANTI_ENTROPY_INTERVAL"));
        assert!(err.to_string().contains("representable duration"));
    }

    #[test]
    fn oversized_ipc_duration_env_value_is_rejected() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_CHUNK_GC_INTERVAL", "1e100")]),
        };

        let err = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            sources,
        )
        .expect_err("oversized IPC duration should fail without panicking");

        assert!(err.to_string().contains("chunk_gc_interval_secs"));
        assert!(err.to_string().contains("representable duration"));
    }

    #[test]
    fn oversized_ipc_duration_override_is_rejected() {
        let overrides = ServerIpcConfigOverrides {
            heartbeat_interval_secs: Some(1e100),
            heartbeat_timeout_secs: Some(2e100),
            ..Default::default()
        };

        let err = ConfigResolver::resolve_server_ipc(
            overrides,
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect_err("oversized IPC duration override should fail without panicking");

        assert!(err.to_string().contains("heartbeat_interval_secs"));
        assert!(err.to_string().contains("representable duration"));
    }

    #[test]
    fn server_ipc_execution_workers_env_and_override_are_resolved() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_MAX_EXECUTION_WORKERS", "9")]),
        };

        let from_env = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            sources,
        )
        .expect("env execution worker override should resolve");
        assert_eq!(from_env.max_execution_workers, 9);

        let from_override = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides {
                max_execution_workers: Some(7),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect("explicit execution worker override should resolve");
        assert_eq!(from_override.max_execution_workers, 7);
    }

    #[test]
    fn server_ipc_execution_workers_rejects_zero_env_and_override() {
        let env_sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_MAX_EXECUTION_WORKERS", "0")]),
        };
        let env_err = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            env_sources,
        )
        .expect_err("zero env execution worker override should fail");
        assert!(env_err.to_string().contains("max_execution_workers"));

        let override_err = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides {
                max_execution_workers: Some(0),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect_err("zero explicit execution worker override should fail");
        assert!(override_err.to_string().contains("max_execution_workers"));
    }

    #[test]
    fn server_ipc_execution_workers_rejects_values_above_hard_limit() {
        let env_sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_MAX_EXECUTION_WORKERS", "65")]),
        };
        let env_err = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            env_sources,
        )
        .expect_err("oversized env execution worker override should fail");
        assert!(env_err.to_string().contains("max_execution_workers"));
        assert!(env_err.to_string().contains("64"));

        let override_err = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides {
                max_execution_workers: Some(65),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect_err("oversized explicit execution worker override should fail");
        assert!(override_err.to_string().contains("max_execution_workers"));
        assert!(override_err.to_string().contains("64"));
    }

    #[test]
    fn client_ipc_pool_decay_env_and_override_are_resolved() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_POOL_DECAY_SECONDS", "12.5")]),
        };

        let from_env = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            sources,
        )
        .expect("env pool decay should resolve");
        assert_eq!(from_env.pool_decay_seconds, 12.5);

        // Zero is the explicit immediate-retirement window and stays valid.
        let immediate = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides {
                pool_decay_seconds: Some(0.0),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect("zero pool decay should resolve as immediate retirement");
        assert_eq!(immediate.pool_decay_seconds, 0.0);

        // Explicit code-level overrides beat the shared environment variable.
        let from_override = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides {
                pool_decay_seconds: Some(7.5),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_POOL_DECAY_SECONDS", "12.5")]),
            },
        )
        .expect("explicit pool decay override should beat env");
        assert_eq!(from_override.pool_decay_seconds, 7.5);
    }

    #[test]
    fn client_ipc_pool_decay_rejects_negative_env_and_override() {
        let env_err = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides::default(),
            RuntimeConfigOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_POOL_DECAY_SECONDS", "-1")]),
            },
        )
        .expect_err("negative env pool decay should fail like the server");
        assert!(env_err.to_string().contains("pool_decay_seconds"));

        let override_err = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides {
                pool_decay_seconds: Some(-1.0),
                ..Default::default()
            },
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .expect_err("negative explicit pool decay should fail like the server");
        assert!(override_err.to_string().contains("pool_decay_seconds"));
    }

    #[test]
    fn relay_idle_timeout_must_fit_millisecond_sweeper_interval() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_IDLE_TIMEOUT", "18446744073709551615")]),
        };

        let err = ConfigResolver::resolve_relay_server(RuntimeConfigOverrides::default(), sources)
            .expect_err("idle timeout should reject millisecond overflow");

        assert!(err.to_string().contains("C2_RELAY_IDLE_TIMEOUT"));
        assert!(err.to_string().contains("milliseconds"));
    }

    #[test]
    fn non_finite_float_override_is_rejected() {
        let mut overrides = RuntimeConfigOverrides::default();
        overrides.server_ipc.chunk_gc_interval_secs = Some(f64::INFINITY);

        let err = ConfigResolver::resolve(overrides, ConfigSources::empty())
            .expect_err("non-finite override should fail");

        assert!(err.to_string().contains("chunk_gc_interval_secs"));
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn scoped_shm_threshold_ignores_relay_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_SHM_THRESHOLD", "8192"),
                ("C2_RELAY_ANTI_ENTROPY_INTERVAL", "not-a-number"),
            ]),
        };

        let threshold =
            ConfigResolver::resolve_shm_threshold(None, sources).expect("threshold should resolve");

        assert_eq!(threshold, 8192);
    }

    #[test]
    fn scoped_shm_threshold_ignores_ipc_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_SHM_THRESHOLD", "8192"),
                ("C2_IPC_MAX_FRAME_SIZE", "not-a-number"),
            ]),
        };

        let threshold =
            ConfigResolver::resolve_shm_threshold(None, sources).expect("threshold should resolve");

        assert_eq!(threshold, 8192);
    }

    #[test]
    fn global_shm_threshold_is_injected_into_scoped_ipc_configs() {
        let global = RuntimeConfigOverrides {
            shm_threshold: Some(16_384),
            ..RuntimeConfigOverrides::default()
        };

        let server = ConfigResolver::resolve_server_ipc(
            ServerIpcConfigOverrides::default(),
            global.clone(),
            ConfigSources::empty(),
        )
        .expect("server IPC should resolve");
        let client = ConfigResolver::resolve_client_ipc(
            ClientIpcConfigOverrides::default(),
            global,
            ConfigSources::empty(),
        )
        .expect("client IPC should resolve");

        assert_eq!(server.shm_threshold, 16_384);
        assert_eq!(client.shm_threshold, 16_384);
    }

    #[test]
    fn relay_seeds_and_proxy_are_parsed() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_RELAY_SEEDS", " http://a:8080, ,http://b:8080 "),
                ("C2_RELAY_USE_PROXY", "yes"),
            ]),
        };

        let resolved =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources).expect("resolve");

        assert_eq!(
            resolved.relay.seeds,
            vec!["http://a:8080".to_string(), "http://b:8080".to_string()]
        );
        assert!(resolved.relay_use_proxy);
        assert!(resolved.relay.use_proxy);
    }

    #[test]
    fn relay_anchor_address_resolution_ignores_relay_server_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_RELAY_ANCHOR_ADDRESS", "http://127.0.0.1:8080"),
                ("C2_RELAY_IDLE_TIMEOUT", "not-a-number"),
            ]),
        };

        let resolved = ConfigResolver::resolve_relay_anchor_address(sources)
            .expect("relay address should not parse relay server-only env");

        assert_eq!(resolved.as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn relay_use_proxy_resolution_ignores_relay_anchor_address_and_ipc_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_RELAY_ANCHOR_ADDRESS", "http://127.0.0.1:8080"),
                ("C2_RELAY_USE_PROXY", "yes"),
                ("C2_IPC_POOL_SEGMENT_SIZE", "not-a-number"),
            ]),
        };

        let use_proxy = ConfigResolver::resolve_relay_use_proxy(sources)
            .expect("relay proxy policy should not parse IPC env");

        assert!(use_proxy);
    }

    #[test]
    fn relay_route_max_attempts_resolves_from_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_ROUTE_MAX_ATTEMPTS", "5")]),
        };

        let attempts =
            ConfigResolver::resolve_relay_route_max_attempts(sources).expect("resolve attempts");

        assert_eq!(attempts, 5);
    }

    #[test]
    fn relay_call_timeout_resolves_from_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_CALL_TIMEOUT", "900.5")]),
        };

        let timeout = ConfigResolver::resolve_relay_call_timeout_secs(sources)
            .expect("resolve relay call timeout");

        assert_eq!(timeout, 900.5);
    }

    #[test]
    fn relay_call_timeout_zero_disables_total_timeout() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_CALL_TIMEOUT", "0")]),
        };

        let timeout = ConfigResolver::resolve_relay_call_timeout_secs(sources)
            .expect("resolve disabled relay call timeout");

        assert_eq!(timeout, 0.0);
    }

    #[test]
    fn remote_payload_chunk_size_resolves_from_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_REMOTE_PAYLOAD_CHUNK_SIZE", "1048576")]),
        };

        let chunk_size = ConfigResolver::resolve_remote_payload_chunk_size(None, sources)
            .expect("resolve remote payload chunk size");

        assert_eq!(chunk_size, 1_048_576);
    }

    #[test]
    fn remote_payload_chunk_size_rejects_zero() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_REMOTE_PAYLOAD_CHUNK_SIZE", "0")]),
        };

        let err = ConfigResolver::resolve_remote_payload_chunk_size(None, sources)
            .expect_err("zero remote payload chunk size should fail");

        assert!(err.to_string().contains("C2_REMOTE_PAYLOAD_CHUNK_SIZE"));
        assert!(err.to_string().contains("> 0"));
    }

    #[test]
    fn relay_client_resolution_carries_remote_payload_chunk_size() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_REMOTE_PAYLOAD_CHUNK_SIZE", "2097152")]),
        };

        let resolved =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources).expect("resolve");

        assert_eq!(resolved.remote_payload_chunk_size, 2_097_152);
    }

    #[test]
    fn relay_route_max_attempts_is_bounded() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_ROUTE_MAX_ATTEMPTS", "33")]),
        };

        let err = ConfigResolver::resolve_relay_route_max_attempts(sources)
            .expect_err("unbounded relay route attempts should fail");

        assert!(err.to_string().contains("C2_RELAY_ROUTE_MAX_ATTEMPTS"));
        assert!(err.to_string().contains("32"));
    }

    #[test]
    fn relay_server_resolution_rejects_bad_relay_server_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_IDLE_TIMEOUT", "not-a-number")]),
        };

        let err = ConfigResolver::resolve_relay_server(RuntimeConfigOverrides::default(), sources)
            .expect_err("relay server config should parse idle timeout");

        assert!(err.to_string().contains("C2_RELAY_IDLE_TIMEOUT"));
    }

    #[test]
    fn relay_server_resolution_ignores_relay_route_attempt_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_RELAY_ROUTE_MAX_ATTEMPTS", "not-a-number")]),
        };

        ConfigResolver::resolve_relay_server(RuntimeConfigOverrides::default(), sources)
            .expect("relay server config should not parse client route attempts");
    }

    #[test]
    fn relay_server_resolution_uses_single_runtime_override_source() {
        let mut overrides = RuntimeConfigOverrides::default();
        overrides.relay.idle_timeout_secs = Some(5);
        overrides.relay_anchor_address = Some("http://127.0.0.1:8080".to_string());

        let resolved = ConfigResolver::resolve_relay_server(overrides, ConfigSources::empty())
            .expect("relay server should resolve from one override source");

        assert_eq!(resolved.relay.idle_timeout_secs, 5);
        assert_eq!(
            resolved.relay_anchor_address.as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    // ── Memory budget limits ─────────────────────────────────────────────

    #[test]
    fn resolver_budget_defaults_come_from_memory_budget_limits() {
        let resolved =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), ConfigSources::empty())
                .expect("defaults should resolve");

        assert_eq!(
            resolved.server_ipc.memory_budget_limits(),
            crate::MemoryBudgetLimits::default()
        );
        assert_eq!(
            resolved.client_ipc.memory_budget_limits(),
            crate::MemoryBudgetLimits::default()
        );
        assert_eq!(
            resolved.server_ipc.shm_backing_budget_bytes,
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(
            resolved.server_ipc.file_backing_budget_bytes,
            16 * 1024 * 1024 * 1024
        );
        assert_eq!(
            resolved.server_ipc.live_reassembly_budget_bytes,
            8 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn budget_limits_resolve_from_env_for_both_roles() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "1073741824"),
                ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "2147483648"),
                ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "536870912"),
            ]),
        };

        let resolved = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
            .expect("env budget limits should resolve");

        assert_eq!(resolved.server_ipc.shm_backing_budget_bytes, 1_073_741_824);
        assert_eq!(resolved.server_ipc.file_backing_budget_bytes, 2_147_483_648);
        assert_eq!(
            resolved.server_ipc.live_reassembly_budget_bytes,
            536_870_912
        );
        assert_eq!(resolved.client_ipc.shm_backing_budget_bytes, 1_073_741_824);
        assert_eq!(resolved.client_ipc.file_backing_budget_bytes, 2_147_483_648);
        assert_eq!(
            resolved.client_ipc.live_reassembly_budget_bytes,
            536_870_912
        );
    }

    #[test]
    fn budget_limits_explicit_overrides_beat_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "1073741824"),
                ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "2147483648"),
                ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "536870912"),
            ]),
        };
        let mut overrides = RuntimeConfigOverrides::default();
        overrides.server_ipc.shm_backing_budget_bytes = Some(3_221_225_472);
        overrides.client_ipc.file_backing_budget_bytes = Some(0);
        overrides.client_ipc.base.live_reassembly_budget_bytes = Some(2_147_483_648);

        let resolved = ConfigResolver::resolve(overrides, sources).expect("resolve");

        assert_eq!(resolved.server_ipc.shm_backing_budget_bytes, 3_221_225_472);
        assert_eq!(resolved.server_ipc.file_backing_budget_bytes, 2_147_483_648);
        assert_eq!(resolved.client_ipc.shm_backing_budget_bytes, 1_073_741_824);
        assert_eq!(resolved.client_ipc.file_backing_budget_bytes, 0);
        assert_eq!(
            resolved.client_ipc.live_reassembly_budget_bytes,
            2_147_483_648
        );
    }

    #[test]
    fn budget_limits_accept_zero_and_full_u64_range() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "0"),
                ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "0"),
                ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "0"),
            ]),
        };

        let from_zero_env = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
            .expect("zero budget limits are valid finite configuration");
        assert_eq!(
            from_zero_env.server_ipc.memory_budget_limits(),
            crate::MemoryBudgetLimits::zeroed()
        );

        let overrides = RuntimeConfigOverrides {
            server_ipc: ServerIpcConfigOverrides {
                shm_backing_budget_bytes: Some(u64::MAX),
                file_backing_budget_bytes: Some(u64::MAX),
                live_reassembly_budget_bytes: Some(u64::MAX),
                ..Default::default()
            },
            client_ipc: ClientIpcConfigOverrides {
                shm_backing_budget_bytes: Some(u64::MAX),
                file_backing_budget_bytes: Some(u64::MAX),
                live_reassembly_budget_bytes: Some(u64::MAX),
                ..Default::default()
            },
            ..RuntimeConfigOverrides::default()
        };
        let from_max_override = ConfigResolver::resolve(overrides, ConfigSources::empty())
            .expect("full-range budget limits are valid");
        assert_eq!(
            from_max_override.server_ipc.shm_backing_budget_bytes,
            u64::MAX
        );
        assert_eq!(
            from_max_override.client_ipc.file_backing_budget_bytes,
            u64::MAX
        );
    }

    #[test]
    fn budget_limits_env_rejects_negative_and_overflow() {
        for (key, value) in [
            ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "-1"),
            ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "18446744073709551616"),
            ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "not-a-number"),
        ] {
            let sources = ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[(key, value)]),
            };

            let err = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
                .expect_err("invalid budget env value should fail");

            assert!(
                err.to_string().contains(key),
                "error should name {key}: {err}"
            );
        }
    }

    // ── Call execution limits ────────────────────────────────────────────

    #[test]
    fn call_execution_limits_resolve_canonical_defaults() {
        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            ConfigSources::empty(),
        )
        .expect("defaults should resolve");

        assert_eq!(limits, CallExecutionLimits::default());
        assert_eq!(limits.max_outstanding_calls, 1024);
        assert_eq!(limits.retained_input_budget_bytes, 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn call_execution_limits_resolve_from_env_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let env_file = tempdir.path().join(".env");
        fs::write(
            &env_file,
            [
                "C2_CALL_MAX_OUTSTANDING=64",
                "C2_CALL_RETAINED_INPUT_BUDGET_BYTES=1073741824",
            ]
            .join("\n"),
        )
        .expect("write env file");

        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file),
                process_env: env(&[]),
            },
        )
        .expect("env-file limits should resolve");

        assert_eq!(limits.max_outstanding_calls, 64);
        assert_eq!(limits.retained_input_budget_bytes, 1_073_741_824);
    }

    #[test]
    fn call_execution_limits_process_env_overrides_env_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let env_file = tempdir.path().join(".env");
        fs::write(
            &env_file,
            [
                "C2_CALL_MAX_OUTSTANDING=64",
                "C2_CALL_RETAINED_INPUT_BUDGET_BYTES=1073741824",
            ]
            .join("\n"),
        )
        .expect("write env file");

        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file),
                process_env: env(&[
                    ("C2_CALL_MAX_OUTSTANDING", "128"),
                    ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "2147483648"),
                ]),
            },
        )
        .expect("process env should win over the env file");

        assert_eq!(limits.max_outstanding_calls, 128);
        assert_eq!(limits.retained_input_budget_bytes, 2_147_483_648);
    }

    #[test]
    fn call_execution_limits_explicit_overrides_beat_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_CALL_MAX_OUTSTANDING", "64"),
                ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "1073741824"),
            ]),
        };

        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(7),
                retained_input_budget_bytes: Some(4096),
            },
            sources,
        )
        .expect("explicit limits should beat env");

        assert_eq!(limits.max_outstanding_calls, 7);
        assert_eq!(limits.retained_input_budget_bytes, 4096);
    }

    #[test]
    fn call_execution_limits_explicit_overrides_ignore_invalid_env() {
        // A present explicit value means the environment for that dimension
        // is never consulted, so invalid text there cannot fail resolution.
        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(7),
                retained_input_budget_bytes: Some(4096),
            },
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[
                    ("C2_CALL_MAX_OUTSTANDING", "not-a-number"),
                    ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "-1"),
                ]),
            },
        )
        .expect("explicit limits should bypass env parsing entirely");

        assert_eq!(limits.max_outstanding_calls, 7);
        assert_eq!(limits.retained_input_budget_bytes, 4096);
    }

    #[test]
    fn call_execution_limits_zero_is_finite_per_dimension() {
        // An explicit zero closes only its own dimension; the other one
        // still falls through to the canonical default.
        let zeroed_max = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(0),
                retained_input_budget_bytes: None,
            },
            ConfigSources::empty(),
        )
        .expect("explicit zero outstanding calls should resolve");
        assert_eq!(zeroed_max.max_outstanding_calls, 0);
        assert_eq!(
            zeroed_max.retained_input_budget_bytes,
            crate::DEFAULT_RETAINED_INPUT_BUDGET_BYTES
        );

        // An environmental zero is a present value in exactly the same way:
        // it must never fall through to the default via truthiness.
        let zero_env = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "0")]),
            },
        )
        .expect("zero retained budget should resolve as finite and rejecting");
        assert_eq!(
            zero_env.max_outstanding_calls,
            crate::DEFAULT_MAX_OUTSTANDING_CALLS
        );
        assert_eq!(zero_env.retained_input_budget_bytes, 0);
    }

    #[test]
    fn call_execution_limits_accept_full_u64_range() {
        let from_env = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[
                    ("C2_CALL_MAX_OUTSTANDING", "18446744073709551615"),
                    (
                        "C2_CALL_RETAINED_INPUT_BUDGET_BYTES",
                        "18446744073709551615",
                    ),
                ]),
            },
        )
        .expect("u64::MAX is a legal finite limit");
        assert_eq!(from_env.max_outstanding_calls, u64::MAX);
        assert_eq!(from_env.retained_input_budget_bytes, u64::MAX);

        let from_override = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(u64::MAX),
                retained_input_budget_bytes: Some(u64::MAX),
            },
            ConfigSources::empty(),
        )
        .expect("u64::MAX overrides are legal");
        assert_eq!(from_override.max_outstanding_calls, u64::MAX);
        assert_eq!(from_override.retained_input_budget_bytes, u64::MAX);
    }

    #[test]
    fn call_execution_limits_env_rejects_invalid_values() {
        for (key, value) in [
            ("C2_CALL_MAX_OUTSTANDING", "-1"),
            ("C2_CALL_MAX_OUTSTANDING", ""),
            ("C2_CALL_MAX_OUTSTANDING", "   "),
            ("C2_CALL_MAX_OUTSTANDING", "not-a-number"),
            ("C2_CALL_MAX_OUTSTANDING", "18446744073709551616"),
            ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "-1"),
            ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", ""),
            ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "   "),
            ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "not-a-number"),
            (
                "C2_CALL_RETAINED_INPUT_BUDGET_BYTES",
                "18446744073709551616",
            ),
        ] {
            let sources = ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[(key, value)]),
            };

            let err = ConfigResolver::resolve_call_execution_limits(
                CallExecutionLimitsOverrides::default(),
                sources,
            )
            .expect_err(&format!("invalid value {value:?} for {key} should fail"));

            assert!(
                err.to_string().contains(key),
                "error should name {key} for {value:?}: {err}"
            );
        }
    }

    #[test]
    fn call_execution_limits_ignore_other_scopes_invalid_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_CALL_MAX_OUTSTANDING", "32"),
                ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "65536"),
                ("C2_IPC_MAX_FRAME_SIZE", "not-a-number"),
                ("C2_IPC_POOL_SEGMENT_SIZE", "-1"),
                ("C2_RELAY_IDLE_TIMEOUT", "not-a-number"),
                ("C2_SHM_THRESHOLD", ""),
                ("C2_IPC_ROOT", "   "),
            ]),
        };

        let limits = ConfigResolver::resolve_call_execution_limits(
            CallExecutionLimitsOverrides::default(),
            sources,
        )
        .expect("call limit resolution must not parse other scopes");

        assert_eq!(limits.max_outstanding_calls, 32);
        assert_eq!(limits.retained_input_budget_bytes, 65_536);
    }

    #[test]
    fn other_scope_resolutions_ignore_call_limit_env() {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[
                ("C2_CALL_MAX_OUTSTANDING", "not-a-number"),
                ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES", "-1"),
            ]),
        };

        let threshold = ConfigResolver::resolve_shm_threshold(None, sources.clone())
            .expect("shm threshold resolution must not parse call limit env");
        assert_eq!(threshold, 4096);

        let attempts = ConfigResolver::resolve_relay_route_max_attempts(sources.clone())
            .expect("relay route attempts resolution must not parse call limit env");
        assert_eq!(attempts, 3);

        let runtime = ConfigResolver::resolve(RuntimeConfigOverrides::default(), sources)
            .expect("runtime resolution must not parse call limit env");
        assert_eq!(runtime.shm_threshold, 4096);
    }

    #[test]
    fn relay_upstream_ipc_rejects_invalid_configuration() {
        for (key, value) in [
            ("C2_IPC_POOL_ENABLED", "invalid"),
            ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "-1"),
            ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "18446744073709551616"),
            ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "invalid"),
            ("C2_SHM_THRESHOLD", "invalid"),
        ] {
            let sources = ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[(key, value)]),
            };
            let err =
                ConfigResolver::resolve_relay_server(RuntimeConfigOverrides::default(), sources)
                    .expect_err("relay must reject malformed upstream IPC policy before listening");
            assert!(err.to_string().contains(key), "{err}");
        }

        let mut overrides = RuntimeConfigOverrides::default();
        overrides.client_ipc.pool_enabled = Some(false);
        overrides.client_ipc.pool_prewarm_segments = Some(1);
        let err = ConfigResolver::resolve_relay_server(overrides, ConfigSources::empty())
            .expect_err("disabled buddy cannot be prewarmed");
        assert!(err.to_string().contains("pool_prewarm_segments"), "{err}");
    }

    #[test]
    fn relay_upstream_ipc_defaults_match_client_and_both_entry_points() {
        let runtime =
            ConfigResolver::resolve(RuntimeConfigOverrides::default(), ConfigSources::empty())
                .unwrap();
        let relay = ConfigResolver::resolve_relay_server(
            RuntimeConfigOverrides::default(),
            ConfigSources::empty(),
        )
        .unwrap();
        assert_eq!(relay.relay.upstream_ipc, ClientIpcConfig::default());
        assert_eq!(runtime.relay.upstream_ipc, runtime.client_ipc);
        assert_eq!(runtime.client_ipc, relay.relay.upstream_ipc);
    }

    #[test]
    fn relay_upstream_ipc_resolves_full_policy_with_shared_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.env");
        fs::write(&path, "C2_IPC_POOL_ENABLED=false\nC2_IPC_POOL_SEGMENT_SIZE=65536\nC2_IPC_MAX_POOL_SEGMENTS=3\nC2_IPC_POOL_PREWARM_SEGMENTS=0\nC2_IPC_POOL_DECAY_SECONDS=10\nC2_IPC_SHM_BACKING_BUDGET_BYTES=100\nC2_IPC_FILE_BACKING_BUDGET_BYTES=200\nC2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES=300\nC2_SHM_THRESHOLD=1024\n").unwrap();
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Path(path),
            process_env: env(&[
                ("C2_IPC_POOL_ENABLED", "true"),
                ("C2_IPC_POOL_PREWARM_SEGMENTS", "1"),
                ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "400"),
                ("C2_IPC_POOL_DECAY_SECONDS", "20"),
                ("C2_SHM_THRESHOLD", "2048"),
            ]),
        };
        let from_env = ConfigResolver::resolve_relay_server(
            RuntimeConfigOverrides::default(),
            sources.clone(),
        )
        .unwrap();
        let ipc = from_env.relay.upstream_ipc;
        assert!(ipc.pool_enabled);
        assert_eq!(ipc.pool_prewarm_segments, 1);
        assert_eq!(ipc.pool_segment_size, 65536);
        assert_eq!(ipc.max_pool_memory, 65536 * 3);
        assert_eq!(ipc.shm_backing_budget_bytes, 400);
        assert_eq!(ipc.file_backing_budget_bytes, 200);
        assert_eq!(ipc.live_reassembly_budget_bytes, 300);
        assert_eq!(ipc.pool_decay_seconds, 20.0);
        assert_eq!(ipc.shm_threshold, 2048);

        let mut overrides = RuntimeConfigOverrides::default();
        overrides.client_ipc.base.pool_enabled = Some(false);
        overrides.client_ipc.pool_prewarm_segments = Some(0);
        overrides.client_ipc.shm_backing_budget_bytes = Some(0);
        overrides.client_ipc.base.file_backing_budget_bytes = Some(0);
        overrides.client_ipc.live_reassembly_budget_bytes = Some(0);
        overrides.client_ipc.pool_decay_seconds = Some(30.0);
        overrides.shm_threshold = Some(4096);
        let runtime = ConfigResolver::resolve(overrides.clone(), sources.clone()).unwrap();
        let relay = ConfigResolver::resolve_relay_server(overrides, sources).unwrap();
        assert_eq!(relay.relay.upstream_ipc, runtime.client_ipc);
        assert_eq!(runtime.relay.upstream_ipc, runtime.client_ipc);
        let ipc = relay.relay.upstream_ipc;
        assert!(!ipc.pool_enabled);
        assert_eq!(ipc.pool_prewarm_segments, 0);
        assert_eq!(
            ipc.memory_budget_limits(),
            crate::MemoryBudgetLimits::zeroed()
        );
        assert_eq!(ipc.pool_decay_seconds, 30.0);
        assert_eq!(ipc.shm_threshold, 4096);
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_resolution_follows_code_env_file_default_precedence() {
        use crate::{LocalEndpointContext, LocalEndpointOptions};

        let tempdir = tempfile::tempdir().expect("tempdir");
        let env_file = tempdir.path().join(".env");
        fs::write(&env_file, "C2_IPC_ROOT=/tmp/c2-file-root\n").expect("write env file");

        // Default: no code option, no env, no env file.
        let default_ctx = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources::empty(),
        )
        .expect("default context should resolve");
        assert_eq!(
            default_ctx.unix_root(),
            Some(std::path::Path::new("/tmp")),
            "the platform default root must stay /tmp"
        );
        assert_eq!(
            default_ctx,
            ConfigResolver::resolve_local_endpoint(
                LocalEndpointOptions::default(),
                ConfigSources::empty()
            )
            .expect("default context is stable")
        );

        // .env only.
        let from_file = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file.clone()),
                process_env: env(&[]),
            },
        )
        .expect("env-file context should resolve");
        assert_eq!(
            from_file.unix_root(),
            Some(std::path::Path::new("/tmp/c2-file-root"))
        );

        // Process env beats the env file.
        let from_env = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file.clone()),
                process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-env-root")]),
            },
        )
        .expect("env context should resolve");
        assert_eq!(
            from_env.unix_root(),
            Some(std::path::Path::new("/tmp/c2-env-root"))
        );

        // Explicit code beats process env.
        let from_code = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions {
                unix_root: Some(std::path::PathBuf::from("/tmp/c2-code-root")),
            },
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file),
                process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-env-root")]),
            },
        )
        .expect("code context should resolve");
        assert_eq!(
            from_code.unix_root(),
            Some(std::path::Path::new("/tmp/c2-code-root"))
        );

        // Distinct roots keep the same logical address isolated and give it
        // distinct namespace identities.
        let address = "ipc://resolver-name";
        let env_endpoint = from_env.endpoint(address).expect("env endpoint");
        let code_endpoint = from_code.endpoint(address).expect("code endpoint");
        assert_ne!(env_endpoint, code_endpoint);
        assert_ne!(
            env_endpoint.context().namespace_id(),
            code_endpoint.context().namespace_id()
        );
        assert_eq!(
            from_env.namespace_id(),
            LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/c2-env-root"))
                .expect("same root is one identity")
                .namespace_id()
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_resolver_rejects_invalid_root_values() {
        use crate::LocalEndpointOptions;

        // "" is the only value rejected as explicitly empty; "   " fails the
        // native absolute-path check instead (never trimmed into validity).
        let empty_sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_ROOT", "")]),
        };
        let empty_err =
            ConfigResolver::resolve_local_endpoint(LocalEndpointOptions::default(), empty_sources)
                .expect_err("empty root should fail");
        assert!(
            empty_err.to_string().contains("cannot be empty"),
            "{empty_err}"
        );

        for value in ["   ", "relative/root", "/tmp/../escape", "/tmp/a\0b"] {
            let sources = ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_ROOT", value)]),
            };
            let err =
                ConfigResolver::resolve_local_endpoint(LocalEndpointOptions::default(), sources)
                    .expect_err(&format!("invalid root {value:?} should fail"));
            assert!(
                err.to_string().contains("C2_IPC_ROOT"),
                "error should name C2_IPC_ROOT for {value:?}: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_env_root_is_preserved_verbatim() {
        use crate::LocalEndpointOptions;

        // A trailing space is a legal Unix directory name: the env value must
        // resolve to exactly the same context/endpoint/namespace id as the
        // identical code-level path, and stay distinct from the name without
        // the trailing space.
        let spaced = "/tmp/c2-r ";
        let from_env = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_ROOT", spaced)]),
            },
        )
        .expect("spaced env root should resolve");
        let from_code = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions {
                unix_root: Some(std::path::PathBuf::from(spaced)),
            },
            ConfigSources::empty(),
        )
        .expect("spaced code root should resolve");
        assert_eq!(from_env, from_code);
        assert_eq!(from_env.unix_root(), Some(std::path::Path::new(spaced)));
        assert_eq!(from_env.namespace_id(), from_code.namespace_id());

        let endpoint = from_env.endpoint("ipc://verbatim").expect("endpoint");
        assert_eq!(
            endpoint,
            from_code.endpoint("ipc://verbatim").expect("endpoint")
        );
        assert!(
            endpoint
                .os_name()
                .to_str()
                .unwrap()
                .starts_with("/tmp/c2-r /c2-"),
            "{endpoint:?}"
        );

        let plain = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-r")]),
            },
        )
        .expect("plain env root should resolve");
        assert_ne!(from_env, plain);
        assert_ne!(from_env.namespace_id(), plain.namespace_id());
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_env_root_whitespace_is_rejected_without_trimming() {
        use crate::LocalEndpointOptions;

        // Whitespace-only and leading-whitespace values keep their raw bytes
        // and fail the native absolute-path check; they must never be
        // trimmed into valid paths.
        for value in [" ", "  /tmp/c2-r", "\t/tmp/c2-r"] {
            let sources = ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env(&[("C2_IPC_ROOT", value)]),
            };
            let err =
                ConfigResolver::resolve_local_endpoint(LocalEndpointOptions::default(), sources)
                    .expect_err(&format!("whitespace root {value:?} must not become valid"));
            assert!(
                err.to_string().contains("C2_IPC_ROOT"),
                "error should name C2_IPC_ROOT for {value:?}: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_env_file_quoted_spaces_are_preserved() {
        use crate::LocalEndpointOptions;

        // Quoted .env values legally carry spaces; the parser must keep them
        // verbatim so the resolved context matches the same code-level path.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let env_file = tempdir.path().join(".env");
        fs::write(&env_file, "C2_IPC_ROOT=\"/tmp/c2-r quoted\"\n").expect("write env file");

        let from_file = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Path(env_file),
                process_env: env(&[]),
            },
        )
        .expect("quoted env-file root should resolve");
        assert_eq!(
            from_file.unix_root(),
            Some(std::path::Path::new("/tmp/c2-r quoted"))
        );

        let from_code = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions {
                unix_root: Some(std::path::PathBuf::from("/tmp/c2-r quoted")),
            },
            ConfigSources::empty(),
        )
        .expect("quoted code root should resolve");
        assert_eq!(from_file, from_code);
        assert_eq!(from_file.namespace_id(), from_code.namespace_id());
    }
}
