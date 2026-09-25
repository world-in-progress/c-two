use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::{BaseIpcConfig, ClientIpcConfig, RelayConfig, ServerIpcConfig};

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
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerIpcConfigOverrides {
    pub base: BaseIpcConfigOverrides,
    pub pool_enabled: Option<bool>,
    pub pool_segment_size: Option<u64>,
    pub max_pool_segments: Option<u32>,
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
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
    pub reassembly_segment_size: Option<u64>,
    pub reassembly_max_segments: Option<u32>,
    pub max_total_chunks: Option<u32>,
    pub chunk_gc_interval_secs: Option<f64>,
    pub chunk_threshold_ratio: Option<f64>,
    pub chunk_assembler_timeout_secs: Option<f64>,
    pub max_reassembly_bytes: Option<u64>,
    pub chunk_size: Option<u64>,
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
        let relay = RelayConfig {
            use_proxy: relay_client.relay_use_proxy,
            remote_payload_chunk_size: relay_client.remote_payload_chunk_size,
            ..relay
        };
        let server_ipc = resolve_server_ipc_config(&catalog, overrides.server_ipc, shm_threshold)?;
        let client_ipc = resolve_client_ipc_config(&catalog, overrides.client_ipc, shm_threshold)?;

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
        let relay = RelayConfig {
            use_proxy: relay_use_proxy,
            remote_payload_chunk_size,
            ..relay
        };

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
    apply_base_overrides(&mut cfg.base, &overrides.base);
    apply_flat_base_overrides_to_client(&mut cfg, &overrides);

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
    fn server_pool_segment_must_not_exceed_payload_limit() {
        let mut overrides = RuntimeConfigOverrides::default();
        overrides.server_ipc.pool_segment_size = Some(2 * 1024 * 1024);
        overrides.server_ipc.max_payload_size = Some(1024 * 1024);

        let err = ConfigResolver::resolve(overrides, ConfigSources::empty())
            .expect_err("oversized pool segment should fail");

        assert!(err.to_string().contains("pool_segment_size"));
        assert!(err.to_string().contains("max_payload_size"));
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
}
