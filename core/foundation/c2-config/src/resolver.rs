use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::{
    ConfigField, DEFAULT_FILE_SPILL_BUDGET_BYTES, DEFAULT_MEMORY_BUDGET_BYTES,
    DEFAULT_SERVER_STARTUP_TIMEOUT, DomainLimits, DomainLimitsOverrides, DomainLimitsPatch,
    LocalEndpointContext, LocalEndpointOptions, RelayConfig, ServerIpcConfig, deadline_after,
    default_max_outstanding_calls, validate_server_id,
};

const ENV_FILE: &str = "C2_ENV_FILE";
const RELAY_ANCHOR: &str = "C2_RELAY_ANCHOR_ADDRESS";
const IPC_ROOT: &str = "C2_IPC_ROOT";
const MEMORY_BUDGET: &str = "C2_MEMORY_BUDGET_BYTES";
const FILE_SPILL_BUDGET: &str = "C2_FILE_SPILL_BUDGET_BYTES";
const CALL_MAX_OUTSTANDING: &str = "C2_CALL_MAX_OUTSTANDING";
const MAX_PAYLOAD: &str = "C2_MAX_PAYLOAD_SIZE";
const EXECUTION_WORKERS: &str = "C2_MAX_EXECUTION_WORKERS";
const OUTCOME_MAX_ENTRIES: &str = "C2_OUTCOME_MAX_ENTRIES";
const OUTCOME_MAX_RETAINED_BYTES: &str = "C2_OUTCOME_MAX_RETAINED_BYTES";
const RELAY_BIND: &str = "C2_RELAY_BIND";
const RELAY_EDGE_BIND: &str = "C2_RELAY_EDGE_BIND";
const RELAY_TLS_CERT: &str = "C2_RELAY_TLS_CERT";
const RELAY_TLS_KEY: &str = "C2_RELAY_TLS_KEY";
const RELAY_TLS_AUTO: &str = "C2_RELAY_TLS_AUTO";
const RELAY_TLS_SAN: &str = "C2_RELAY_TLS_SAN";
const RELAY_STATE_DIR: &str = "C2_RELAY_STATE_DIR";
const RELAY_INSECURE_PLAINTEXT: &str = "C2_RELAY_INSECURE_PLAINTEXT";
const RELAY_ID: &str = "C2_RELAY_ID";
const RELAY_ADVERTISE_URL: &str = "C2_RELAY_ADVERTISE_URL";
const RELAY_SEEDS: &str = "C2_RELAY_SEEDS";
const RELAY_USE_PROXY: &str = "C2_RELAY_USE_PROXY";

const RELAY_CA_CERT: &str = "C2_RELAY_CA_CERT";
const RELAY_CERT_SHA256: &str = "C2_RELAY_CERT_SHA256";

const ENV_KEYS: [&str; 24] = [
    ENV_FILE,
    RELAY_ANCHOR,
    IPC_ROOT,
    MEMORY_BUDGET,
    FILE_SPILL_BUDGET,
    CALL_MAX_OUTSTANDING,
    MAX_PAYLOAD,
    EXECUTION_WORKERS,
    OUTCOME_MAX_ENTRIES,
    OUTCOME_MAX_RETAINED_BYTES,
    RELAY_BIND,
    RELAY_EDGE_BIND,
    RELAY_TLS_CERT,
    RELAY_TLS_KEY,
    RELAY_TLS_AUTO,
    RELAY_TLS_SAN,
    RELAY_STATE_DIR,
    RELAY_INSECURE_PLAINTEXT,
    RELAY_ID,
    RELAY_ADVERTISE_URL,
    RELAY_SEEDS,
    RELAY_USE_PROXY,
    RELAY_CA_CERT,
    RELAY_CERT_SHA256,
];

pub type EnvMap = BTreeMap<String, OsString>;

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
            process_env: ENV_KEYS
                .into_iter()
                .filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value)))
                .collect(),
        }
    }
}

/// Captured environment for one activation, shared across all its resolutions.
#[derive(Debug, Clone)]
pub struct EnvCatalog {
    env: EnvMap,
}

impl EnvCatalog {
    pub fn load(sources: ConfigSources) -> Result<Self, ConfigError> {
        let mut env = EnvMap::new();
        let env_file = match sources.env_file {
            EnvFilePolicy::Disabled => None,
            EnvFilePolicy::Path(path) => Some(path),
            EnvFilePolicy::FromC2EnvFile => match sources.process_env.get(ENV_FILE) {
                Some(path) if path.is_empty() => None,
                Some(path) => Some(PathBuf::from(path)),
                None => Some(PathBuf::from(".env")),
            },
        };
        if let Some(path) = env_file {
            #[cfg(test)]
            ENV_FILE_LOADS.with(|count| count.set(count.get() + 1));

            match dotenvy::from_path_iter(&path) {
                Ok(iter) => {
                    for item in iter {
                        let (key, value) = item.map_err(|error| {
                            ConfigError::new(format!(
                                "failed to parse env file {}: {error}",
                                path.display()
                            ))
                        })?;
                        if key != ENV_FILE && ENV_KEYS.contains(&key.as_str()) {
                            env.insert(key, value.into());
                        }
                    }
                }
                Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ConfigError::new(format!(
                        "failed to load env file {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        env.extend(
            sources
                .process_env
                .into_iter()
                .filter(|(key, _)| key != ENV_FILE && ENV_KEYS.contains(&key.as_str())),
        );
        Ok(Self { env })
    }

    fn raw(&self, key: &str) -> Result<Option<&str>, ConfigError> {
        self.env
            .get(key)
            .map(|value| {
                value
                    .to_str()
                    .ok_or_else(|| ConfigError::new(format!("{key} must be valid UTF-8")))
            })
            .transpose()
    }

    fn text(&self, key: &str) -> Result<Option<&str>, ConfigError> {
        Ok(self.raw(key)?.and_then(nonempty))
    }

    fn integer<T: FromStr>(
        &self,
        key: &str,
        explicit: Option<T>,
    ) -> Result<Option<T>, ConfigError> {
        if explicit.is_some() {
            return Ok(explicit);
        }
        self.raw(key)?
            .map(|value| {
                value.trim().parse().map_err(|_| {
                    ConfigError::new(format!("{key} must be an unsigned integer in range"))
                })
            })
            .transpose()
    }
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn catalog_bool(
    explicit: Option<bool>,
    key: &str,
    catalog: &EnvCatalog,
) -> Result<bool, ConfigError> {
    if let Some(value) = explicit {
        return Ok(value);
    }
    match catalog.text(key)?.map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "yes") => Ok(true),
        None | Some("0" | "false" | "no") => Ok(false),
        _ => Err(ConfigError::new(format!(
            "{key} must be a boolean (1/0, true/false, yes/no)"
        ))),
    }
}

fn catalog_path(
    explicit: Option<PathBuf>,
    key: &str,
    catalog: &EnvCatalog,
) -> Result<Option<PathBuf>, ConfigError> {
    match explicit {
        Some(path) => Ok(Some(path)),
        None => catalog
            .raw(key)?
            .map(|path| {
                nonempty(path)
                    .map(PathBuf::from)
                    .ok_or_else(|| ConfigError::new(format!("{key} must be a nonempty path")))
            })
            .transpose(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct RelayConfigOverrides {
    pub bind: Option<String>,
    pub edge_bind: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_auto: Option<bool>,
    pub tls_san: Option<Vec<String>>,
    pub state_dir: Option<PathBuf>,
    pub insecure_plaintext: Option<bool>,
    pub relay_id: Option<String>,
    pub advertise_url: Option<String>,
    pub seeds: Option<Vec<String>>,
    pub use_proxy: Option<bool>,
    pub domain_limits: DomainLimitsOverrides,
}

/// Trust policy for a relay HTTPS connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum RelayTlsTrust {
    #[default]
    SystemDefault,
    CustomCaCert(PathBuf),
    /// Exact SHA-256 digest of the leaf certificate's DER encoding.
    CertSha256([u8; 32]),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ResolvedRelayClientConfig {
    pub use_proxy: bool,
    pub tls_trust: RelayTlsTrust,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServerConfig {
    /// When absent, the Host supplies or reuses its automatic identity.
    pub server_id: Option<String>,
    pub startup_timeout: Duration,
    pub ipc: ServerIpcConfig,
    pub domain_limits: DomainLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerConfigPatch {
    pub server_id: ConfigField<String>,
    pub startup_timeout: ConfigField<Duration>,
    pub max_payload_size: ConfigField<u64>,
    pub max_execution_workers: ConfigField<u32>,
    pub outcome_max_entries: ConfigField<usize>,
    pub outcome_max_retained_bytes: ConfigField<u64>,
    pub domain_limits: DomainLimitsPatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerConfigOverrides {
    pub server_id: Option<String>,
    pub startup_timeout: Option<Duration>,
    pub max_payload_size: Option<u64>,
    pub max_execution_workers: Option<u32>,
    pub outcome_max_entries: Option<usize>,
    pub outcome_max_retained_bytes: Option<u64>,
    pub domain_limits: DomainLimitsOverrides,
}

impl ServerConfigOverrides {
    /// Form a candidate without changing the previously accepted overrides.
    pub fn patched(mut self, patch: ServerConfigPatch) -> Self {
        patch.server_id.apply(&mut self.server_id);
        patch.startup_timeout.apply(&mut self.startup_timeout);
        patch.max_payload_size.apply(&mut self.max_payload_size);
        patch
            .max_execution_workers
            .apply(&mut self.max_execution_workers);
        patch
            .outcome_max_entries
            .apply(&mut self.outcome_max_entries);
        patch
            .outcome_max_retained_bytes
            .apply(&mut self.outcome_max_retained_bytes);
        self.domain_limits = self.domain_limits.patched(patch.domain_limits);
        self
    }

    pub fn apply_patch(
        &mut self,
        patch: ServerConfigPatch,
        catalog: &EnvCatalog,
    ) -> Result<ResolvedServerConfig, ConfigError> {
        let candidate = self.clone().patched(patch);
        let resolved = ConfigResolver::resolve_server(&candidate, catalog)?;
        *self = candidate;
        Ok(resolved)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
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
    /// Resolve the capacity of a client, Host, or relay with the same rules.
    pub fn resolve_domain_limits(
        overrides: &DomainLimitsOverrides,
        catalog: &EnvCatalog,
    ) -> Result<DomainLimits, ConfigError> {
        let memory_budget_bytes = catalog
            .integer(MEMORY_BUDGET, overrides.memory_budget_bytes)?
            .unwrap_or(DEFAULT_MEMORY_BUDGET_BYTES);
        let limits = DomainLimits {
            memory_budget_bytes,
            file_spill_budget_bytes: catalog
                .integer(FILE_SPILL_BUDGET, overrides.file_spill_budget_bytes)?
                .unwrap_or(DEFAULT_FILE_SPILL_BUDGET_BYTES),
            max_outstanding_calls: catalog
                .integer(CALL_MAX_OUTSTANDING, overrides.max_outstanding_calls)?
                .unwrap_or_else(|| default_max_outstanding_calls(memory_budget_bytes)),
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn resolve_server(
        overrides: &ServerConfigOverrides,
        catalog: &EnvCatalog,
    ) -> Result<ResolvedServerConfig, ConfigError> {
        if let Some(id) = &overrides.server_id {
            validate_server_id(id).map_err(ConfigError::new)?;
        }
        let startup_timeout = overrides
            .startup_timeout
            .unwrap_or(DEFAULT_SERVER_STARTUP_TIMEOUT);
        deadline_after(Instant::now(), startup_timeout)
            .map_err(|error| ConfigError::new(format!("startup_timeout: {error}")))?;
        let domain_limits = Self::resolve_domain_limits(&overrides.domain_limits, catalog)?;
        let defaults = ServerIpcConfig::default();
        let ipc = ServerIpcConfig {
            max_payload_size: catalog
                .integer(MAX_PAYLOAD, overrides.max_payload_size)?
                .unwrap_or(defaults.max_payload_size),
            max_execution_workers: catalog
                .integer(EXECUTION_WORKERS, overrides.max_execution_workers)?
                .unwrap_or(defaults.max_execution_workers),
            outcome_max_entries: catalog
                .integer(OUTCOME_MAX_ENTRIES, overrides.outcome_max_entries)?
                .unwrap_or(defaults.outcome_max_entries),
            outcome_max_retained_bytes: catalog.integer(
                OUTCOME_MAX_RETAINED_BYTES,
                overrides.outcome_max_retained_bytes,
            )?,
        };
        ipc.validate_with_memory_budget(domain_limits.memory_budget_bytes)?;
        Ok(ResolvedServerConfig {
            server_id: overrides.server_id.clone(),
            startup_timeout,
            ipc,
            domain_limits,
        })
    }

    pub fn resolve_relay_server(
        overrides: RelayConfigOverrides,
        catalog: &EnvCatalog,
    ) -> Result<RelayConfig, ConfigError> {
        let text = |explicit: Option<String>, key| match explicit.as_deref().and_then(nonempty) {
            Some(value) => Ok(Some(value.to_owned())),
            None => Ok::<_, ConfigError>(catalog.text(key)?.map(str::to_owned)),
        };
        let defaults = RelayConfig::default();
        let bind = text(overrides.bind, RELAY_BIND)?.unwrap_or(defaults.bind);
        let relay_id = text(overrides.relay_id, RELAY_ID)?.unwrap_or(defaults.relay_id);
        let advertise_url = text(overrides.advertise_url, RELAY_ADVERTISE_URL)?.unwrap_or_default();
        let seeds = match overrides.seeds {
            Some(seeds) => seeds,
            None => catalog
                .text(RELAY_SEEDS)?
                .into_iter()
                .flat_map(|value| value.split(',').filter_map(nonempty).map(str::to_owned))
                .collect(),
        };
        let config = RelayConfig {
            bind,
            edge_bind: text(overrides.edge_bind, RELAY_EDGE_BIND)?,
            tls_cert: catalog_path(overrides.tls_cert, RELAY_TLS_CERT, catalog)?,
            tls_key: catalog_path(overrides.tls_key, RELAY_TLS_KEY, catalog)?,
            tls_auto: catalog_bool(overrides.tls_auto, RELAY_TLS_AUTO, catalog)?,
            tls_san: match overrides.tls_san {
                Some(names) => names,
                None => catalog
                    .text(RELAY_TLS_SAN)?
                    .into_iter()
                    .flat_map(|names| names.split(',').filter_map(nonempty).map(str::to_owned))
                    .collect(),
            },
            state_dir: catalog_path(overrides.state_dir, RELAY_STATE_DIR, catalog)?,
            insecure_plaintext: catalog_bool(
                overrides.insecure_plaintext,
                RELAY_INSECURE_PLAINTEXT,
                catalog,
            )?,
            relay_id,
            advertise_url,
            seeds,
            use_proxy: Self::resolve_relay_use_proxy(overrides.use_proxy, catalog)?,
            domain_limits: Self::resolve_domain_limits(&overrides.domain_limits, catalog)?,
            ..defaults
        };
        config.validate().map_err(ConfigError::new)?;
        Ok(config)
    }

    pub fn resolve_relay_anchor_address(
        catalog: &EnvCatalog,
    ) -> Result<Option<String>, ConfigError> {
        Ok(catalog.text(RELAY_ANCHOR)?.map(str::to_owned))
    }

    pub fn resolve_relay_client(
        use_proxy: Option<bool>,
        catalog: &EnvCatalog,
    ) -> Result<ResolvedRelayClientConfig, ConfigError> {
        let use_proxy = Self::resolve_relay_use_proxy(use_proxy, catalog)?;
        let tls_trust = match (catalog.raw(RELAY_CA_CERT)?, catalog.raw(RELAY_CERT_SHA256)?) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::new(format!(
                    "{RELAY_CA_CERT} and {RELAY_CERT_SHA256} are mutually exclusive"
                )));
            }
            (Some(path), None) => {
                if path.trim().is_empty() {
                    return Err(ConfigError::new(format!(
                        "{RELAY_CA_CERT} must be a nonempty path"
                    )));
                }
                RelayTlsTrust::CustomCaCert(PathBuf::from(path))
            }
            (None, Some(pin)) => {
                let mut digest = [0; 32];
                hex::decode_to_slice(pin.strip_prefix("sha256:").unwrap_or(pin), &mut digest)
                    .map_err(|_| {
                        ConfigError::new(format!(
                            "{RELAY_CERT_SHA256} must be a 64-digit hexadecimal SHA-256 digest"
                        ))
                    })?;
                RelayTlsTrust::CertSha256(digest)
            }
            (None, None) => RelayTlsTrust::SystemDefault,
        };
        Ok(ResolvedRelayClientConfig {
            use_proxy,
            tls_trust,
        })
    }

    fn resolve_relay_use_proxy(
        use_proxy: Option<bool>,
        catalog: &EnvCatalog,
    ) -> Result<bool, ConfigError> {
        catalog_bool(use_proxy, RELAY_USE_PROXY, catalog)
    }

    /// Resolve the immutable local endpoint context from typed options and
    /// configuration sources: explicit code > process env > `.env` > platform
    /// default. The resolved root is validated without touching the
    /// filesystem; a custom final directory must be pre-created by the application.
    /// An explicit empty, relative, `..`-bearing, NUL-bearing, or non-UTF-8
    /// root is rejected instead of silently falling back to the default.
    /// Environment values reach native validation verbatim — a trailing space
    /// is a legal Unix directory name and must resolve identically to the
    /// same code-level path, so nothing is trimmed into or out of validity.
    pub fn resolve_local_endpoint(
        options: LocalEndpointOptions,
        catalog: &EnvCatalog,
    ) -> Result<LocalEndpointContext, ConfigError> {
        let (root, source_name) = match options.unix_root {
            Some(root) => (root, "local endpoint root option".to_owned()),
            None => match catalog.raw(IPC_ROOT)? {
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

#[cfg(test)]
thread_local! {
    static ENV_FILE_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, unix))]
mod local_tests;
#[cfg(test)]
mod tests;
