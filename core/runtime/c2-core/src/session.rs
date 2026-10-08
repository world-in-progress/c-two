//! Rust-owned process runtime session.
//!
//! The session owns process identity, direct IPC client configuration state,
//! and route registration transactions. Python SDKs provide language-specific
//! callbacks and local direct-call bindings, but must not duplicate the runtime
//! authority implemented here.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use c2_contract::ExpectedRouteContract;
use c2_http::client::{
    HttpError, RelayAwareClientConfig, RelayAwareHttpClient, RelayControlClient,
    RelayLocalIpcCandidate, RelayRegistration, RelayRegistrationScope, RelayResolvedTarget,
};
use c2_server::{BuiltRoute, ServerLifecycleState, ServerRouteCloseOutcome};

use crate::call_execution::{CallExecutionContext, CallExecutionObserver};
use crate::outcome::RuntimeRouteSpec;
use crate::{
    LifecycleError, RegisterFailureOutcome, RegisterOutcome, RelayCleanupError, RouteCloseOutcome,
    ShutdownOutcome, UnregisterOutcome,
};
use crate::{
    ObservedPath, PathCounters, auto_server_id, auto_server_instance_id, ipc_address_for_server_id,
    validate_server_id,
};

#[cfg(test)]
thread_local! {
    static FORCE_SERVER_RUNTIME_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

pub type ServerIpcConfigOverrides = c2_config::ServerIpcConfigOverrides;
pub type ClientIpcConfigOverrides = c2_config::ClientIpcConfigOverrides;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub server_id: String,
    pub server_instance_id: String,
    pub ipc_address: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    pub server_id: Option<String>,
    pub server_ipc_overrides: Option<ServerIpcConfigOverrides>,
    pub client_ipc_overrides: Option<ClientIpcConfigOverrides>,
    pub shm_threshold: Option<u64>,
    pub remote_payload_chunk_size: Option<u64>,
    pub relay_anchor_address: Option<String>,
    pub use_process_relay_anchor: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            server_id: None,
            server_ipc_overrides: None,
            client_ipc_overrides: None,
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        }
    }
}

fn registration_rollback_outcome(route_name: &str, local_removed: bool) -> RouteCloseOutcome {
    RouteCloseOutcome {
        route_name: route_name.to_string(),
        local_removed,
        active_drained: true,
        closed_reason: "registration_rollback".to_string(),
        close_error: None,
    }
}

fn http_error_parts(err: HttpError) -> (Option<u16>, String) {
    match err {
        HttpError::ServerError(status_code, body) => (Some(status_code), body),
        other => (None, other.to_string()),
    }
}

fn route_close_success(route_name: &str, closed_reason: &str) -> RouteCloseOutcome {
    RouteCloseOutcome {
        route_name: route_name.to_string(),
        local_removed: true,
        active_drained: true,
        closed_reason: closed_reason.to_string(),
        close_error: None,
    }
}

fn route_close_failure(route_name: &str, closed_reason: &str, error: String) -> RouteCloseOutcome {
    RouteCloseOutcome {
        route_name: route_name.to_string(),
        local_removed: false,
        active_drained: false,
        closed_reason: closed_reason.to_string(),
        close_error: Some(error),
    }
}

fn route_close_from_server_outcome(outcome: ServerRouteCloseOutcome) -> RouteCloseOutcome {
    RouteCloseOutcome {
        route_name: outcome.route_name,
        local_removed: true,
        active_drained: outcome.active_drained,
        closed_reason: outcome.closed_reason,
        close_error: None,
    }
}

#[derive(Clone)]
pub struct Runtime {
    state: Arc<Mutex<RuntimeState>>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

struct ShutdownTransaction {
    server: std::sync::Weak<c2_server::Server>,
    outcome: Mutex<Option<ShutdownOutcome>>,
    completed: Condvar,
    delivered: AtomicBool,
}

struct RuntimeState {
    server_id_override: Option<String>,
    server_ipc_overrides: Option<ServerIpcConfigOverrides>,
    client_ipc_overrides: Option<ClientIpcConfigOverrides>,
    shm_threshold: Option<u64>,
    remote_payload_chunk_size: Option<u64>,
    call_execution_overrides: c2_config::CallExecutionLimitsOverrides,
    call_execution_sources: Option<c2_config::ConfigSources>,
    call_execution_revision: u64,
    call_execution: Option<CallExecutionContext>,
    call_execution_observer: CallExecutionObserver,
    client_config_frozen: bool,
    client_config_revision: u64,
    local_endpoint_options: c2_config::LocalEndpointOptions,
    local_endpoint_sources: Option<c2_config::ConfigSources>,
    local_endpoint_revision: u64,
    frozen_local_endpoint: Option<c2_config::LocalEndpointContext>,
    /// Resolved client IPC config fixed by the first valid connection
    /// attempt. Later acquires reuse it verbatim so one Runtime can never
    /// run two competing client configurations.
    frozen_client_config: Option<c2_config::ClientIpcConfig>,
    /// Outgoing IPC cache in one immutable endpoint domain. Created on first
    /// acquire so HTTP-only Runtimes need no local platform scope. Clones share
    /// it; distinct Runtimes are isolated, and drains never replace the pool.
    client_pool: Option<Arc<c2_ipc::ClientPool>>,
    identity: Option<RuntimeIdentity>,
    relay_anchor_address_override: Option<String>,
    use_process_relay_anchor: bool,
    relay_projection: Option<RelayProjection>,
    path_counters: PathCounters,
    /// Native owner control capability attached by an explicit call. It is
    /// deliberately not part of [`RuntimeOptions`]: the capability never
    /// travels through cloneable option data, argv, environment variables,
    /// or logs.
    owner_control: Option<c2_local::OwnerControlReceiver>,
    /// Set once an owner-bound host consumed the capability; later attach or
    /// consume attempts are rejected.
    owner_control_consumed: bool,
    lifecycle_policy: c2_config::ServerLifecyclePolicy,
    lifecycle_frozen: bool,
    shutdown_transactions: HashMap<String, Arc<ShutdownTransaction>>,
    registration_scopes: HashMap<String, HashMap<String, RelayRegistrationScope>>,
    #[cfg(test)]
    forced_relay_config_error: Option<String>,
}

#[derive(Clone)]
struct RelayProjection {
    relay_anchor_address: String,
    relay_use_proxy: bool,
    control: Arc<RelayControlClient>,
    local_endpoint_context: Option<c2_config::LocalEndpointContext>,
}

pub(crate) struct RelayClientSettings {
    pub(crate) use_proxy: bool,
    pub(crate) max_attempts: usize,
    pub(crate) call_timeout_secs: f64,
    pub(crate) remote_payload_chunk_size: u64,
}

pub(crate) enum RelayResolvedConnection {
    Ipc {
        client: RelayAwareHttpClient,
        candidate: RelayLocalIpcCandidate,
        context: c2_config::LocalEndpointContext,
    },
    Http {
        client: RelayAwareHttpClient,
        route_uid: String,
        route_revision: u64,
    },
}

impl RuntimeState {
    fn ensure_no_pending_teardown(&self) -> Result<(), LifecycleError> {
        if self
            .shutdown_transactions
            .values()
            .any(|transaction| transaction.outcome.lock().is_none())
        {
            return Err(LifecycleError::Configuration("pending native teardown must finish before starting a new Host or resetting server identity".into()));
        }
        Ok(())
    }
}

impl Drop for RuntimeState {
    fn drop(&mut self) {
        // Observers retain accounting metadata alone. Closing on last session
        // owner drop does not refund surviving real input/preparation owners.
        self.call_execution_observer.close();
    }
}

impl Runtime {
    pub fn new(options: RuntimeOptions) -> Result<Self, LifecycleError> {
        if let Some(server_id) = options.server_id.as_deref() {
            validate_server_id(server_id)?;
        }
        Ok(Self {
            state: Arc::new(Mutex::new(RuntimeState {
                server_id_override: options.server_id,
                server_ipc_overrides: options.server_ipc_overrides,
                client_ipc_overrides: options.client_ipc_overrides,
                shm_threshold: options.shm_threshold,
                remote_payload_chunk_size: options.remote_payload_chunk_size,
                call_execution_overrides: Default::default(),
                call_execution_sources: None,
                call_execution_revision: 0,
                call_execution: None,
                call_execution_observer: CallExecutionObserver::new(),
                client_config_frozen: false,
                client_config_revision: 0,
                local_endpoint_options: Default::default(),
                local_endpoint_sources: None,
                local_endpoint_revision: 0,
                frozen_local_endpoint: None,
                frozen_client_config: None,
                client_pool: None,
                identity: None,
                relay_anchor_address_override: options
                    .relay_anchor_address
                    .map(|addr| canonical_relay_anchor_address(&addr)),
                use_process_relay_anchor: options.use_process_relay_anchor,
                relay_projection: None,
                path_counters: PathCounters::default(),
                owner_control: None,
                owner_control_consumed: false,
                lifecycle_policy: Default::default(),
                lifecycle_frozen: false,
                shutdown_transactions: HashMap::new(),
                registration_scopes: HashMap::new(),
                #[cfg(test)]
                forced_relay_config_error: None,
            })),
        })
    }

    /// Configure the independent finite-call domain before its first business
    /// call. Connect does not freeze this policy. Clones and every Client share
    /// the one domain; teardown never replaces it or launders live charges.
    pub fn set_call_execution_limits(
        &self,
        overrides: c2_config::CallExecutionLimitsOverrides,
    ) -> Result<(), LifecycleError> {
        self.configure_call_execution_limits(overrides, None)
    }

    /// Deterministic resolver inputs replace process discovery for this domain.
    pub fn set_call_execution_limits_with_sources(
        &self,
        overrides: c2_config::CallExecutionLimitsOverrides,
        sources: c2_config::ConfigSources,
    ) -> Result<(), LifecycleError> {
        self.configure_call_execution_limits(overrides, Some(sources))
    }

    fn configure_call_execution_limits(
        &self,
        overrides: c2_config::CallExecutionLimitsOverrides,
        sources: Option<c2_config::ConfigSources>,
    ) -> Result<(), LifecycleError> {
        let limits = resolve_call_execution_limits(overrides.clone(), sources.clone())?;
        let mut state = self.state.lock();
        if let Some(context) = &state.call_execution {
            let snapshot = context.snapshot();
            return if snapshot.max_operations == limits.max_outstanding_calls
                && snapshot.max_retained_bytes == limits.retained_input_budget_bytes
            {
                // Record accepted optional code inputs, even for an idempotent
                // setter. The frozen budget and its live charges stay intact.
                state.call_execution_overrides = overrides;
                state.call_execution_sources = sources;
                Ok(()) // Idempotent configuration, never replace this domain.
            } else {
                Err(LifecycleError::ConfigFrozen)
            };
        }
        if state.call_execution_observer.is_closed() {
            return Err(LifecycleError::ConfigFrozen);
        }
        state.call_execution_overrides = overrides;
        state.call_execution_sources = sources;
        state.call_execution_revision += 1;
        Ok(())
    }

    /// Consistent read-only input retention counters. Before first call this
    /// resolves policy without freezing or constructing an executor. It counts
    /// actual finite owners, including continuations after caller timeout;
    /// response-storage observations stay in outgoing_memory_observer().
    pub fn call_execution_snapshot(&self) -> Result<crate::CallExecutionSnapshot, LifecycleError> {
        Ok(self.resolve_call_execution_context(false)?.snapshot())
    }

    /// Capture pure metadata for this Runtime's independent domain, including
    /// future first-call initialization. This neither resolves nor freezes policy.
    pub fn call_execution_observer(&self) -> CallExecutionObserver {
        self.state.lock().call_execution_observer.clone()
    }

    /// Last accepted optional code inputs, without resolving defaults or
    /// environment. This is not a second copy of the effective domain limits.
    pub fn call_execution_limits_overrides(&self) -> c2_config::CallExecutionLimitsOverrides {
        self.state.lock().call_execution_overrides.clone()
    }

    pub(crate) fn call_execution_context(&self) -> Result<CallExecutionContext, LifecycleError> {
        self.resolve_call_execution_context(true)
    }

    fn resolve_call_execution_context(
        &self,
        freeze: bool,
    ) -> Result<CallExecutionContext, LifecycleError> {
        loop {
            let (overrides, sources, revision) = {
                let state = self.state.lock();
                if let Some(context) = &state.call_execution {
                    return Ok(context.clone());
                }
                (
                    state.call_execution_overrides.clone(),
                    state.call_execution_sources.clone(),
                    state.call_execution_revision,
                )
            };
            // Environment/.env I/O outside runtime metadata, just as for the
            // existing configuration domains. A racing setter wins by revision.
            let resolved = resolve_call_execution_limits(overrides, sources);
            let mut state = self.state.lock();
            if let Some(context) = &state.call_execution {
                return Ok(context.clone());
            }
            if state.call_execution_revision != revision {
                continue;
            }
            let context = CallExecutionContext::new(&resolved?);
            if state.call_execution_observer.is_closed() {
                context.close();
            }
            if freeze {
                state.call_execution_observer.publish(&context);
                state.call_execution = Some(context.clone());
            }
            return Ok(context);
        }
    }

    /// Configure local endpoints without binding, connecting, or freezing memory policy.
    /// Code options override process environment, then .env, then platform defaults.
    pub fn set_local_endpoint(
        &self,
        options: c2_config::LocalEndpointOptions,
    ) -> Result<(), LifecycleError> {
        self.set_local_endpoint_sources(options, None)
    }

    /// Configure local endpoints with explicit resolver inputs (for deterministic
    /// embedding and isolated fixtures). These inputs replace process discovery.
    pub fn set_local_endpoint_with_sources(
        &self,
        options: c2_config::LocalEndpointOptions,
        sources: c2_config::ConfigSources,
    ) -> Result<(), LifecycleError> {
        self.set_local_endpoint_sources(options, Some(sources))
    }

    fn set_local_endpoint_sources(
        &self,
        options: c2_config::LocalEndpointOptions,
        sources: Option<c2_config::ConfigSources>,
    ) -> Result<(), LifecycleError> {
        // Resolver file reads and platform scope lookup run outside Runtime metadata.
        let context = resolve_local_endpoint(options.clone(), sources.clone())?;
        let mut state = self.state.lock();
        if let Some(frozen) = &state.frozen_local_endpoint {
            return if frozen == &context {
                Ok(())
            } else {
                Err(LifecycleError::ConfigFrozen)
            };
        }
        state.local_endpoint_options = options;
        state.local_endpoint_sources = sources;
        state.local_endpoint_revision += 1;
        Ok(())
    }

    /// Pure context observation. Before first local I/O it reflects current
    /// resolver inputs; afterwards it returns the immutable Runtime snapshot.
    /// Never creates directories, a ClientPool, or a client memory domain.
    pub fn local_endpoint_context(
        &self,
    ) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
        self.resolve_local_endpoint_context(false)
    }

    pub fn local_endpoint(
        &self,
        address: &str,
    ) -> Result<c2_config::LocalEndpoint, LifecycleError> {
        self.local_endpoint_context()?
            .endpoint(address)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))
    }

    pub fn local_endpoint_frozen(&self) -> bool {
        self.state.lock().frozen_local_endpoint.is_some()
    }

    pub(crate) fn freeze_local_endpoint_context(
        &self,
    ) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
        self.resolve_local_endpoint_context(true)
    }

    fn resolve_local_endpoint_context(
        &self,
        freeze: bool,
    ) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
        self.resolve_local_endpoint_context_guarded(freeze, None)
            .map(|context| {
                context.expect("unconditional context resolution cannot reject a domain")
            })
    }

    /// Resolve and, for an imminent local acquire, publish under the same
    /// metadata fence. A stale relay candidate cannot freeze a different domain.
    fn resolve_local_endpoint_context_guarded(
        &self,
        freeze: bool,
        captured: Option<&c2_config::LocalEndpointContext>,
    ) -> Result<Option<c2_config::LocalEndpointContext>, LifecycleError> {
        let accepts = |context: &c2_config::LocalEndpointContext| {
            captured.is_none_or(|captured| captured == context)
        };
        loop {
            let (options, sources, revision) = {
                let state = self.state.lock();
                if let Some(context) = &state.frozen_local_endpoint {
                    return Ok(accepts(context).then(|| context.clone()));
                }
                (
                    state.local_endpoint_options.clone(),
                    state.local_endpoint_sources.clone(),
                    state.local_endpoint_revision,
                )
            };
            let resolved = resolve_local_endpoint(options, sources);
            let mut state = self.state.lock();
            if let Some(context) = &state.frozen_local_endpoint {
                return Ok(accepts(context).then(|| context.clone()));
            }
            // A concurrent setter wins only if it committed before the freeze.
            // Discard even stale resolver errors and retry its newer inputs.
            if state.local_endpoint_revision != revision {
                continue;
            }
            let context = resolved?;
            // Compare before publishing, while still holding the revision fence.
            // HTTP fallback leaves an unused Runtime's configuration mutable.
            if !accepts(&context) {
                return Ok(None);
            }
            if freeze {
                state.frozen_local_endpoint = Some(context.clone());
            }
            return Ok(Some(context));
        }
    }

    pub fn ensure_server(&self) -> Result<RuntimeIdentity, LifecycleError> {
        let mut state = self.state.lock();
        if let Some(identity) = &state.identity {
            return Ok(identity.clone());
        }

        let server_id = match state.server_id_override.clone() {
            Some(server_id) => server_id,
            None => auto_server_id(),
        };
        validate_server_id(&server_id)?;
        let identity = RuntimeIdentity {
            ipc_address: ipc_address_for_server_id(&server_id),
            server_id,
            server_instance_id: auto_server_instance_id(),
        };
        state.identity = Some(identity.clone());
        Ok(identity)
    }

    pub fn server_id(&self) -> Option<String> {
        self.state
            .lock()
            .identity
            .as_ref()
            .map(|identity| identity.server_id.clone())
    }

    pub fn server_id_override(&self) -> Option<String> {
        self.state.lock().server_id_override.clone()
    }

    pub fn server_address(&self) -> Option<String> {
        self.state
            .lock()
            .identity
            .as_ref()
            .map(|identity| identity.ipc_address.clone())
    }

    pub fn server_ipc_overrides(&self) -> Option<ServerIpcConfigOverrides> {
        self.state.lock().server_ipc_overrides.clone()
    }

    pub fn set_server_options(
        &self,
        server_id: Option<String>,
        server_ipc_overrides: Option<ServerIpcConfigOverrides>,
    ) -> Result<(), LifecycleError> {
        if let Some(server_id) = server_id.as_deref() {
            validate_server_id(server_id)?;
        }
        let mut state = self.state.lock();
        state.ensure_no_pending_teardown()?;
        state.server_id_override = server_id;
        state.server_ipc_overrides = server_ipc_overrides;
        state.identity = None;
        Ok(())
    }

    pub fn client_ipc_overrides(&self) -> Option<ClientIpcConfigOverrides> {
        self.state.lock().client_ipc_overrides.clone()
    }

    pub fn set_client_ipc_overrides(
        &self,
        overrides: Option<ClientIpcConfigOverrides>,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        if state.client_config_frozen {
            return Err(LifecycleError::ClientConfigFrozen);
        }
        state.client_ipc_overrides = overrides;
        state.client_config_revision += 1;
        Ok(())
    }

    pub fn shm_threshold_override(&self) -> Option<u64> {
        self.state.lock().shm_threshold
    }

    pub fn remote_payload_chunk_size_override(&self) -> Option<u64> {
        self.state.lock().remote_payload_chunk_size
    }

    pub fn client_config_frozen(&self) -> bool {
        self.state.lock().client_config_frozen
    }

    pub fn path_counters(&self) -> PathCounters {
        self.state.lock().path_counters
    }

    pub(crate) fn record_path(&self, path: ObservedPath) {
        self.state.lock().path_counters.record(path);
    }

    /// Read-only snapshot of this Runtime's outgoing client memory domain.
    ///
    /// `None` until the first valid connection attempt freezes the domain;
    /// observing it never connects, maps memory, or freezes configuration.
    /// Charges retained by completed or held data stay observable across a
    /// shutdown for as long as the Runtime (or any budget guard) exists.
    pub fn outgoing_memory_stats(&self) -> Option<crate::MemoryScopeStats> {
        let pool = self.state.lock().client_pool.clone()?;
        pool.memory_budget_snapshot()
            .map(|snapshot| crate::MemoryScopeStats::from_budget(snapshot.limits, snapshot.budget))
    }

    /// Read-only observer of this Runtime's frozen outgoing client domain.
    ///
    /// `None` until the first connection attempt freezes the domain. The
    /// returned handle shares only the domain counters and resolved limits, so
    /// a retired Runtime keeps charges held by outstanding owners observable
    /// without retaining the cache, connections, or pools. Observing never
    /// connects, maps memory, or freezes configuration.
    pub fn outgoing_memory_observer(&self) -> Option<c2_mem::BudgetObserver> {
        let pool = self.state.lock().client_pool.clone()?;
        pool.memory_budget_observer()
    }

    /// Read-only memory statistics for this Runtime without a host.
    ///
    /// The server scope is reported by [`Host::memory_stats`](crate::Host::memory_stats),
    /// which composes this outgoing scope with its server direction.
    pub fn memory_stats(&self) -> crate::RuntimeMemoryStats {
        crate::RuntimeMemoryStats {
            runtime_outgoing: self.outgoing_memory_stats(),
            server: None,
        }
    }

    /// Acquire an outgoing IPC client from this Runtime's cache.
    ///
    /// The resolved client config freezes atomically under the RuntimeState
    /// lock before any connection I/O: the first valid connection attempt —
    /// including one that later fails — fixes the config for this Runtime's
    /// lifetime, and a setter racing a stalled first connect observes
    /// `LifecycleError::ClientConfigFrozen` instead of producing a second
    /// configuration. The connect itself runs outside the state lock through
    /// the cache's epoch fence.
    pub(crate) fn acquire_ipc_client(
        &self,
        address: &str,
    ) -> Result<Arc<c2_ipc::SyncClient>, c2_ipc::IpcError> {
        let context = self
            .freeze_local_endpoint_context()
            .map_err(|error| c2_ipc::IpcError::Config(error.to_string()))?;
        let (config, pool) = loop {
            let (overrides, revision) = {
                let mut state = self.state.lock();
                if let Some(config) = state.frozen_client_config.clone() {
                    let pool = state
                        .client_pool
                        .get_or_insert_with(|| {
                            Arc::new(c2_ipc::ClientPool::with_endpoint_context(
                                Duration::from_secs(60),
                                context.clone(),
                            ))
                        })
                        .clone();
                    break (config, pool);
                }
                (
                    c2_config::RuntimeConfigOverrides {
                        client_ipc: state.client_ipc_overrides.clone().unwrap_or_default(),
                        shm_threshold: state.shm_threshold,
                        ..Default::default()
                    },
                    state.client_config_revision,
                )
            };
            let resolved = resolve_client_config(overrides);
            let mut state = self.state.lock();
            if state.client_config_revision != revision || state.frozen_client_config.is_some() {
                continue;
            }
            let config = resolved.map_err(|error| c2_ipc::IpcError::Config(error.to_string()))?;
            state.frozen_client_config = Some(config.clone());
            state.client_config_frozen = true;
            let pool = state
                .client_pool
                .get_or_insert_with(|| {
                    Arc::new(c2_ipc::ClientPool::with_endpoint_context(
                        Duration::from_secs(60),
                        context.clone(),
                    ))
                })
                .clone();
            break (config, pool);
        };
        // Connect outside the RuntimeState lock.
        pool.acquire(address, Some(&config))
    }

    pub(crate) fn release_ipc_client(&self, address: &str, client: &Arc<c2_ipc::SyncClient>) {
        let pool = self.state.lock().client_pool.clone();
        if let Some(pool) = pool {
            pool.release_if_same(address, client);
        }
    }

    pub(crate) fn discard_ipc_client(&self, address: &str, client: &Arc<c2_ipc::SyncClient>) {
        let pool = self.state.lock().client_pool.clone();
        if let Some(pool) = pool {
            pool.discard_if_same(address, client);
        }
    }

    pub(crate) fn server_ipc_config(&self) -> Result<c2_config::ServerIpcConfig, LifecycleError> {
        let runtime_overrides = c2_config::RuntimeConfigOverrides {
            server_ipc: self.server_ipc_overrides().unwrap_or_default(),
            shm_threshold: self.shm_threshold_override(),
            ..Default::default()
        };
        c2_config::ConfigResolver::resolve_server_ipc(
            runtime_overrides.server_ipc.clone(),
            runtime_overrides,
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
    }

    pub(crate) fn relay_client_settings(&self) -> Result<RelayClientSettings, LifecycleError> {
        let sources = c2_config::ConfigSources::from_process();
        let use_proxy = self.relay_use_proxy()?;
        let configured_attempts =
            c2_config::ConfigResolver::resolve_relay_route_max_attempts(sources.clone())
                .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let call_timeout_secs =
            c2_config::ConfigResolver::resolve_relay_call_timeout_secs(sources.clone())
                .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let remote_payload_chunk_size =
            c2_config::ConfigResolver::resolve_remote_payload_chunk_size(
                self.remote_payload_chunk_size_override(),
                sources,
            )
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        Ok(RelayClientSettings {
            use_proxy,
            // One initial observation plus at most one pre-dispatch refresh.
            max_attempts: configured_attempts.clamp(1, 2),
            call_timeout_secs,
            remote_payload_chunk_size,
        })
    }

    pub(crate) fn relay_use_proxy(&self) -> Result<bool, LifecycleError> {
        #[cfg(test)]
        if let Some(message) = self.state.lock().forced_relay_config_error.clone() {
            return Err(LifecycleError::Relay(message));
        }
        c2_config::ConfigResolver::resolve_relay_use_proxy(c2_config::ConfigSources::from_process())
            .map_err(|error| LifecycleError::Configuration(error.to_string()))
    }

    pub fn clear_server_identity(&self) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_no_pending_teardown()?;
        state.identity = None;
        Ok(())
    }

    pub fn set_relay_anchor_address(&self, relay_anchor_address: Option<String>) {
        let mut state = self.state.lock();
        let relay_anchor_address =
            relay_anchor_address.map(|addr| canonical_relay_anchor_address(&addr));
        if state.relay_anchor_address_override != relay_anchor_address {
            state.relay_projection = None;
        }
        state.relay_anchor_address_override = relay_anchor_address;
    }

    pub fn relay_anchor_address_override(&self) -> Option<String> {
        self.state.lock().relay_anchor_address_override.clone()
    }

    /// Set the native policy before the first Host freezes it.
    pub fn set_lifecycle_policy(
        &self,
        policy: c2_config::ServerLifecyclePolicy,
    ) -> Result<(), LifecycleError> {
        policy.validate().map_err(LifecycleError::Configuration)?;
        let mut state = self.state.lock();
        if state.lifecycle_frozen {
            return Err(LifecycleError::Configuration(
                "server lifecycle policy is frozen".into(),
            ));
        }
        state.lifecycle_policy = policy;
        Ok(())
    }

    /// Atomically move the unstarted server lifecycle to a prepared Runtime.
    ///
    /// Session replacement is a pre-use configuration transaction. Both sides
    /// are checked before moving the opaque receiver, so a refused handoff
    /// leaves the source usable. Lock order is stable even for opposing calls.
    /// A host-frozen or consumed capability can never be transferred or reused.
    pub fn transfer_unstarted_lifecycle_to(
        &self,
        replacement: &Runtime,
    ) -> Result<(), LifecycleError> {
        if Arc::ptr_eq(&self.state, &replacement.state) {
            return Err(LifecycleError::Configuration(
                "cannot transfer lifecycle to the same Runtime".into(),
            ));
        }
        let transfer = |source: &mut RuntimeState, target: &mut RuntimeState| {
            source.ensure_no_pending_teardown()?;
            target.ensure_no_pending_teardown()?;
            if source.lifecycle_frozen
                || source.owner_control_consumed
                || source.client_config_frozen
            {
                return Err(LifecycleError::Configuration(
                    "cannot transfer lifecycle after Runtime use".into(),
                ));
            }
            if target.lifecycle_frozen
                || target.owner_control_consumed
                || target.owner_control.is_some()
                || target.client_config_frozen
                || target.lifecycle_policy != Default::default()
            {
                return Err(LifecycleError::Configuration(
                    "replacement Runtime already owns a lifecycle".into(),
                ));
            }
            target.lifecycle_policy = source.lifecycle_policy;
            target.owner_control = source.owner_control.take();
            // The retired source cannot adopt another capability after moving
            // its original receiver. Closing it remains a hostless operation.
            if target.owner_control.is_some() {
                source.owner_control_consumed = true;
            }
            Ok(())
        };
        if Arc::as_ptr(&self.state) < Arc::as_ptr(&replacement.state) {
            transfer(&mut self.state.lock(), &mut replacement.state.lock())
        } else {
            let mut target = replacement.state.lock();
            transfer(&mut self.state.lock(), &mut target)
        }
    }

    /// Observe native teardown authority without consuming a Host journal.
    /// SDK start projections use this while the listener can still report
    /// running as active callbacks drain.
    pub fn has_pending_teardown(&self) -> bool {
        self.state.lock().ensure_no_pending_teardown().is_err()
    }

    pub fn lifecycle_policy(&self) -> c2_config::ServerLifecyclePolicy {
        self.state.lock().lifecycle_policy
    }

    pub(crate) fn freeze_lifecycle_policy(
        &self,
        policy: c2_config::ServerLifecyclePolicy,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_no_pending_teardown()?;
        if state.lifecycle_frozen && state.lifecycle_policy != policy {
            return Err(LifecycleError::Configuration(
                "server lifecycle policy is frozen".into(),
            ));
        }
        state.lifecycle_policy = policy;
        state.lifecycle_frozen = true;
        Ok(())
    }

    /// Attach one private native capability, outside cloneable configuration data.
    /// Once a Host consumes it, reattachment and takeover are rejected.
    pub fn attach_owner_control(
        &self,
        receiver: c2_local::OwnerControlReceiver,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        if state.owner_control_consumed {
            return Err(LifecycleError::Configuration(
                "owner control capability was already consumed by an owner-bound host; late capability changes are rejected"
                    .to_string(),
            ));
        }
        if state.owner_control.is_some() {
            return Err(LifecycleError::Configuration(
                "owner control capability already attached; a Runtime accepts exactly one owner control receiver"
                    .to_string(),
            ));
        }
        state.owner_control = Some(receiver);
        Ok(())
    }

    /// Whether an unconsumed native owner control capability is attached.
    pub fn owner_control_attached(&self) -> bool {
        let state = self.state.lock();
        state.owner_control.is_some() && !state.owner_control_consumed
    }

    /// Read-only one-way consumption state for SDK capability preflight.
    pub fn owner_control_consumed(&self) -> bool {
        self.state.lock().owner_control_consumed
    }

    /// Consume the attached capability for one owner-bound host start.
    ///
    /// This is the freeze point: naming the `OwnerBound` policy without an
    /// attached capability fails here, before the host can publish readiness.
    /// A second consume attempt also fails, so one Runtime can never run two
    /// owner-bound hosts from one capability.
    pub(crate) fn consume_owner_control(
        &self,
    ) -> Result<c2_local::OwnerControlReceiver, LifecycleError> {
        let mut state = self.state.lock();
        match state.owner_control.take() {
            Some(receiver) => {
                state.owner_control_consumed = true;
                Ok(receiver)
            }
            None if state.owner_control_consumed => Err(LifecycleError::Configuration(
                "owner control capability was already consumed by an owner-bound host".to_string(),
            )),
            None => Err(LifecycleError::Configuration(
                "OwnerBound host policy requires an attached native owner control capability before the host can publish readiness"
                    .to_string(),
            )),
        }
    }

    pub fn effective_relay_anchor_address(&self) -> Result<Option<String>, LifecycleError> {
        #[cfg(test)]
        if let Some(message) = self.state.lock().forced_relay_config_error.clone() {
            return Err(LifecycleError::Relay(message));
        }
        let (override_address, use_process_relay_anchor) = {
            let state = self.state.lock();
            (
                state.relay_anchor_address_override.clone(),
                state.use_process_relay_anchor,
            )
        };
        if let Some(address) = override_address {
            return Ok(Some(address));
        }
        if !use_process_relay_anchor {
            return Ok(None);
        }
        c2_config::ConfigResolver::resolve_relay_anchor_address(
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| LifecycleError::Relay(e.to_string()))
    }

    #[cfg(test)]
    fn force_relay_config_error_for_test(&self, message: impl Into<String>) {
        self.state.lock().forced_relay_config_error = Some(message.into());
    }

    pub(crate) fn register_route(
        &self,
        server: &Arc<c2_server::Server>,
        route: BuiltRoute,
        spec: RuntimeRouteSpec,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
    ) -> Result<RegisterOutcome, LifecycleError> {
        let identity = self.ensure_server()?;
        let route_name = spec.name.clone();
        let route_uid = route.route_uid().to_string();
        let route_revision = route.route_revision();
        let registration_scope = RelayRegistrationScope {
            name: route_name.clone(),
            server_id: identity.server_id.clone(),
            server_instance_id: identity.server_instance_id.clone(),
            route_uid: route_uid.clone(),
            route_revision,
        };
        let effective_relay_anchor_address =
            self.effective_relay_anchor_address_arg(relay_anchor_address)?;
        if route.name() != spec.name {
            return Err(LifecycleError::Server(format!(
                "route/spec name mismatch: route={:?}, spec={:?}",
                route.name(),
                spec.name
            )));
        }
        if route.method_names() != spec.method_names.as_slice() {
            return Err(LifecycleError::Server(format!(
                "route/spec method_names mismatch for {route_name}"
            )));
        }
        if route.access_map_snapshot() != spec.access_map {
            return Err(LifecycleError::Server(format!(
                "route/spec access map mismatch for {route_name}"
            )));
        }
        if route.crm_ns() != spec.crm_ns
            || route.crm_name() != spec.crm_name
            || route.crm_ver() != spec.crm_ver
            || route.abi_hash() != spec.abi_hash
            || route.signature_hash() != spec.signature_hash
        {
            return Err(LifecycleError::Server(format!(
                "route/spec crm contract mismatch for {route_name}: route={}/{}/{} hashes={}/{} spec={}/{}/{} hashes={}/{}",
                route.crm_ns(),
                route.crm_name(),
                route.crm_ver(),
                route.abi_hash(),
                route.signature_hash(),
                spec.crm_ns,
                spec.crm_name,
                spec.crm_ver,
                spec.abi_hash,
                spec.signature_hash,
            )));
        }
        let scheduler_snapshot = route.scheduler_snapshot();
        if scheduler_snapshot.mode != spec.concurrency_mode
            || scheduler_snapshot.max_pending != spec.max_pending
            || scheduler_snapshot.max_workers != spec.max_workers
        {
            return Err(LifecycleError::Server(format!(
                "route/spec scheduler mismatch for {route_name}"
            )));
        }

        let rt = Self::server_runtime()?;
        let reservation = rt.block_on(server.reserve_route(route)).map_err(|e| {
            if e.to_string().contains("already registered") {
                LifecycleError::DuplicateRoute(route_name.clone())
            } else {
                LifecycleError::Server(e.to_string())
            }
        })?;
        let mut reservation = Some(reservation);

        let mut relay_registered = false;
        let mut relay_projection = None;
        if let Some(relay_anchor_address) = effective_relay_anchor_address.as_deref() {
            let projection = match self.relay_projection_for_address(
                relay_anchor_address,
                relay_use_proxy,
                Some(server.local_endpoint().context()),
            ) {
                Ok(projection) => projection,
                Err(err) => {
                    rt.block_on(server.abort_reserved_route(
                        reservation.take().expect("reservation should exist"),
                    ));
                    return Err(LifecycleError::RegisterFailure(Box::new(
                        RegisterFailureOutcome {
                            route_name: route_name.clone(),
                            failure_source: "relay_projection".to_string(),
                            error_message: err.to_string(),
                            status_code: None,
                            rollback: Some(registration_rollback_outcome(&route_name, false)),
                            relay_cleanup_error: None,
                        },
                    )));
                }
            };
            let registration_token = reservation
                .as_ref()
                .expect("reservation should exist")
                .registration_token()
                .to_string();
            let expected = ExpectedRouteContract {
                route_name: spec.name.clone(),
                crm_ns: spec.crm_ns.clone(),
                crm_name: spec.crm_name.clone(),
                crm_ver: spec.crm_ver.clone(),
                abi_hash: spec.abi_hash.clone(),
                signature_hash: spec.signature_hash.clone(),
            };
            if let Err(err) = projection.control.prepare_register(
                RelayRegistration {
                    expected: &expected,
                    server_id: &identity.server_id,
                    server_instance_id: &identity.server_instance_id,
                    address: &identity.ipc_address,
                    max_payload_size: server.config().max_payload_size,
                },
                &registration_token,
            ) {
                rt.block_on(
                    server.abort_reserved_route(
                        reservation.take().expect("reservation should exist"),
                    ),
                );
                let (status_code, error_message) = http_error_parts(err);
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "relay_prepare".to_string(),
                        error_message,
                        status_code,
                        rollback: Some(registration_rollback_outcome(&route_name, false)),
                        relay_cleanup_error: None,
                    },
                )));
            }
            relay_projection = Some(projection);
        }

        // Save the concrete token before native commit can make the route observable.
        // Owner EOF may retire it while final relay publish is still awaiting a response.
        self.state
            .lock()
            .registration_scopes
            .entry(format!(
                "{}:{:p}",
                server.server_instance_id(),
                server.as_ref()
            ))
            .or_default()
            .insert(route_name.clone(), registration_scope.clone());
        let relay_needs_final_publish = relay_projection.is_some();
        let route_admission_token = if relay_needs_final_publish {
            match rt.block_on(server.commit_reserved_route_closed(
                reservation.take().expect("reservation should exist"),
            )) {
                Ok(token) => Some(token),
                Err(err) => {
                    return Err(LifecycleError::RegisterFailure(Box::new(
                        RegisterFailureOutcome {
                            route_name: route_name.clone(),
                            failure_source: "commit".to_string(),
                            error_message: err.to_string(),
                            status_code: None,
                            rollback: Some(registration_rollback_outcome(&route_name, false)),
                            relay_cleanup_error: None,
                        },
                    )));
                }
            }
        } else {
            if let Err(err) = rt.block_on(
                server.commit_reserved_route(reservation.take().expect("reservation should exist")),
            ) {
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "commit".to_string(),
                        error_message: err.to_string(),
                        status_code: None,
                        rollback: Some(registration_rollback_outcome(&route_name, false)),
                        relay_cleanup_error: None,
                    },
                )));
            }
            None
        };

        if let Some(projection) = relay_projection.as_ref() {
            let expected = ExpectedRouteContract {
                route_name: spec.name.clone(),
                crm_ns: spec.crm_ns.clone(),
                crm_name: spec.crm_name.clone(),
                crm_ver: spec.crm_ver.clone(),
                abi_hash: spec.abi_hash.clone(),
                signature_hash: spec.signature_hash.clone(),
            };
            if let Err(err) = projection.control.register(RelayRegistration {
                expected: &expected,
                server_id: &identity.server_id,
                server_instance_id: &identity.server_instance_id,
                address: &identity.ipc_address,
                max_payload_size: server.config().max_payload_size,
            }) {
                let local_removed = rt.block_on(server.unregister_route(&spec.name));
                let relay_cleanup_error = self.relay_cleanup(
                    effective_relay_anchor_address.as_deref(),
                    relay_use_proxy,
                    &registration_scope,
                );
                let (status_code, error_message) = http_error_parts(err);
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "relay_register".to_string(),
                        error_message,
                        status_code,
                        rollback: Some(registration_rollback_outcome(&route_name, local_removed)),
                        relay_cleanup_error,
                    },
                )));
            }
            let route_admission_token = route_admission_token
                .expect("relay-backed registration should have a route admission token");
            if let Err(err) = rt.block_on(server.open_route_admission(route_admission_token)) {
                let local_removed = rt.block_on(server.unregister_route(&spec.name));
                let relay_cleanup_error = self.relay_cleanup(
                    effective_relay_anchor_address.as_deref(),
                    relay_use_proxy,
                    &registration_scope,
                );
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "route_open".to_string(),
                        error_message: err.to_string(),
                        status_code: None,
                        rollback: Some(registration_rollback_outcome(&route_name, local_removed)),
                        relay_cleanup_error,
                    },
                )));
            }
            relay_registered = true;
        }

        Ok(RegisterOutcome {
            route_name,
            route_uid,
            route_revision,
            server_id: identity.server_id,
            server_instance_id: identity.server_instance_id,
            ipc_address: identity.ipc_address,
            relay_registered,
        })
    }

    pub(crate) fn unregister_route(
        &self,
        server: &Arc<c2_server::Server>,
        name: &str,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
    ) -> Result<UnregisterOutcome, LifecycleError> {
        let route_name = name.to_string();
        let effective_relay_anchor_address =
            self.effective_relay_anchor_address_arg(relay_anchor_address)?;
        let scopes = self.capture_registration_scopes(server, std::slice::from_ref(&route_name));
        let rt = Self::server_runtime()?;
        let local_removed = rt.block_on(server.unregister_route(&route_name));
        if !local_removed {
            return Err(LifecycleError::MissingRoute(route_name));
        }

        let relay_error = if let Some(relay_anchor_address) =
            effective_relay_anchor_address.as_deref()
        {
            match scopes.get(&route_name) {
                Some(scope) => {
                    self.relay_cleanup(Some(relay_anchor_address), relay_use_proxy, scope)
                }
                None => Some(RelayCleanupError {
                    route_name: route_name.clone(),
                    status_code: None,
                    message: "no captured registration scope; refusing unsafe name-only withdrawal"
                        .into(),
                }),
            }
        } else {
            None
        };
        self.retire_registration_scopes(server, &scopes);
        Ok(UnregisterOutcome {
            route_name: route_name.clone(),
            local_removed,
            close: route_close_success(&route_name, "unregister"),
            relay_error,
        })
    }

    fn capture_registration_scopes(
        &self,
        server: &c2_server::Server,
        names: &[String],
    ) -> HashMap<String, RelayRegistrationScope> {
        let key = format!("{}:{:p}", server.server_instance_id(), server);
        let mut scopes = self
            .state
            .lock()
            .registration_scopes
            .get(&key)
            .cloned()
            .unwrap_or_default();
        for name in names {
            if !scopes.contains_key(name) {
                if let Some((route_uid, route_revision)) = server.registered_route_identity(name) {
                    scopes.insert(
                        name.clone(),
                        RelayRegistrationScope {
                            name: name.clone(),
                            server_id: server.server_id().to_owned(),
                            server_instance_id: server.server_instance_id().to_owned(),
                            route_uid,
                            route_revision,
                        },
                    );
                }
            }
        }
        scopes
    }

    fn retire_registration_scopes(
        &self,
        server: &c2_server::Server,
        scopes: &HashMap<String, RelayRegistrationScope>,
    ) {
        let key = format!("{}:{:p}", server.server_instance_id(), server);
        let mut state = self.state.lock();
        if let Some(current) = state.registration_scopes.get_mut(&key) {
            current.retain(|name, scope| scopes.get(name) != Some(scope));
            if current.is_empty() {
                state.registration_scopes.remove(&key);
            }
        }
    }

    pub(crate) fn shutdown_pending(&self, server: &c2_server::Server) -> bool {
        let transaction = self
            .state
            .lock()
            .shutdown_transactions
            .get(&format!("{}:{:p}", server.server_instance_id(), server))
            .cloned();
        transaction.is_some_and(|transaction| transaction.outcome.lock().is_none())
    }

    /// Observe a pending native transaction without consuming the route journal.
    pub(crate) fn shutdown_observation(
        &self,
        server: &c2_server::Server,
    ) -> Option<ShutdownOutcome> {
        let transaction = self
            .state
            .lock()
            .shutdown_transactions
            .get(&format!("{}:{:p}", server.server_instance_id(), server))
            .cloned()?;
        let outcome = transaction.outcome.lock().clone();
        outcome
    }

    pub(crate) fn shutdown(
        &self,
        server: Option<&Arc<c2_server::Server>>,
        route_names: Vec<String>,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
        relay_cleanup_config_error: Option<String>,
        shutdown_timeout: Duration,
    ) -> ShutdownOutcome {
        // Close only finite admission; retain this domain and all actual
        // owners. Unlimited never consults it. No cancellation or early refund.
        {
            let state = self.state.lock();
            state.call_execution_observer.close();
        }
        let deadline = Instant::now() + shutdown_timeout;
        let scopes = server
            .map(|server| self.capture_registration_scopes(server, &route_names))
            .unwrap_or_default();
        let Some(server) = server else {
            return self.shutdown_transaction(
                None,
                route_names,
                relay_anchor_address,
                relay_use_proxy,
                relay_cleanup_config_error,
                shutdown_timeout,
                scopes,
            );
        };
        // Construct before transferring work so construction failures cannot signal a server.
        #[cfg(test)]
        if FORCE_SERVER_RUNTIME_FAILURE.with(std::cell::Cell::get) {
            return self.shutdown_transaction(
                Some(server),
                route_names,
                relay_anchor_address,
                relay_use_proxy,
                relay_cleanup_config_error,
                shutdown_timeout,
                scopes,
            );
        }
        let key = format!("{}:{:p}", server.server_instance_id(), server.as_ref());
        let transaction = {
            let mut state = self.state.lock();
            // A completed transaction retains only a Weak concrete Server identity.
            // Repeated Persistent hosts do not inherit old outcomes or retain stale pools.
            state.shutdown_transactions.retain(|_, transaction| {
                transaction.server.strong_count() > 0 || transaction.outcome.lock().is_none()
            });
            if let Some(transaction) = state.shutdown_transactions.get(&key) {
                Arc::clone(transaction)
            } else {
                let transaction = Arc::new(ShutdownTransaction {
                    server: Arc::downgrade(server),
                    outcome: Mutex::new(None),
                    completed: Condvar::new(),
                    delivered: AtomicBool::new(false),
                });
                state
                    .shutdown_transactions
                    .insert(key, Arc::clone(&transaction));
                let worker_runtime = self.clone();
                let worker_server = Arc::clone(server);
                let worker_transaction = Arc::clone(&transaction);
                let relay = relay_anchor_address.map(str::to_owned);
                // Only actual pending work retains Runtime/Server. No watcher retains Host.
                let spawn = std::thread::Builder::new()
                    .name("c2-shutdown".into())
                    .spawn(move || {
                        let outcome = worker_runtime.shutdown_transaction(
                            Some(&worker_server),
                            route_names,
                            relay.as_deref(),
                            relay_use_proxy,
                            relay_cleanup_config_error,
                            shutdown_timeout,
                            scopes.clone(),
                        );
                        worker_runtime.retire_registration_scopes(&worker_server, &scopes);
                        *worker_transaction.outcome.lock() = Some(outcome);
                        worker_transaction.completed.notify_all();
                    });
                if let Err(error) = spawn {
                    *transaction.outcome.lock() = Some(ShutdownOutcome {
                        runtime_barrier_error: Some(error.to_string()),
                        ..Default::default()
                    });
                    transaction.completed.notify_all();
                }
                transaction
            }
        };
        let mut result = transaction.outcome.lock();
        while result.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return ShutdownOutcome {
                    server_was_started: server.is_running(),
                    runtime_barrier_error: Some(
                        "native shutdown transaction is still draining after the caller deadline"
                            .into(),
                    ),
                    ..Default::default()
                };
            }
            transaction.completed.wait_for(&mut result, remaining);
        }
        if transaction.delivered.swap(true, Ordering::AcqRel) {
            return ShutdownOutcome {
                ipc_clients_drained: true,
                ..Default::default()
            };
        }
        result.as_ref().unwrap().clone()
    }

    fn shutdown_transaction(
        &self,
        server: Option<&Arc<c2_server::Server>>,
        route_names: Vec<String>,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
        relay_cleanup_config_error: Option<String>,
        shutdown_timeout: Duration,
        scopes: HashMap<String, RelayRegistrationScope>,
    ) -> ShutdownOutcome {
        let (mut effective_relay_anchor_address, mut relay_config_errors) =
            match self.effective_relay_anchor_address_arg(relay_anchor_address) {
                Ok(address) => (address, Vec::new()),
                Err(err) => (None, vec![err.to_string()]),
            };
        if let Some(message) = relay_cleanup_config_error {
            effective_relay_anchor_address = None;
            relay_config_errors.push(message);
        }
        let server_was_started = server.map(|server| server.is_running()).unwrap_or(false);
        let mut outcome = ShutdownOutcome {
            server_was_started,
            ..Default::default()
        };
        for message in relay_config_errors {
            for route_name in &route_names {
                outcome.relay_errors.push(RelayCleanupError {
                    route_name: route_name.clone(),
                    status_code: None,
                    message: message.clone(),
                });
            }
        }

        if let Some(server) = server {
            // A running Server owns the only drain and route journal. Request its close
            // immediately so explicit shutdown cancels owner observation/grace while callbacks
            // continue in the run loop. Never race a separate unregister future against it.
            if matches!(
                server.lifecycle_state(),
                ServerLifecycleState::Starting | ServerLifecycleState::Ready
            ) {
                match Self::server_runtime() {
                    Ok(rt) => {
                        rt.block_on(server.close_business_admission("shutdown"));
                        server.request_shutdown_signal_with_reason("shutdown");
                    }
                    Err(err) => {
                        outcome.route_close_error = Some(err.to_string());
                        for route_name in &route_names {
                            outcome.route_outcomes.push(route_close_failure(
                                route_name,
                                "shutdown",
                                err.to_string(),
                            ));
                        }
                        return outcome;
                    }
                }
            }
        }

        // Close this Runtime's outgoing IPC client cache before touching the
        // server, in both hosted and hostless lifecycles. The cache handle is
        // cloned under the state lock; every connect and close barrier runs
        // outside it. Other Runtimes keep their own caches untouched. The
        // drain report is only a completed claim when every detached client
        // confirmed its bounded close AND the drain was not blocked behind a
        // concurrent drain transaction or in-flight detached-close barriers.
        let client_pool = self.state.lock().client_pool.clone();
        if let Some(client_pool) = client_pool {
            let mut client_close = client_pool.close_all(shutdown_timeout);
            if server.is_some() {
                // This native transaction owns outstanding client barriers as well as routes.
                // Its callers time out independently; keep retrying actual unconfirmed work.
                while !client_close.unconfirmed.is_empty() || client_close.error.is_some() {
                    client_close = client_pool.close_all(Duration::from_secs(1));
                }
            }
            outcome.ipc_clients_drained =
                client_close.unconfirmed.is_empty() && client_close.error.is_none();
            if client_close.error.is_some() || !client_close.unconfirmed.is_empty() {
                let mut detail = client_close.error.unwrap_or_default();
                if !client_close.unconfirmed.is_empty() {
                    if !detail.is_empty() {
                        detail.push_str("; ");
                    }
                    detail.push_str(&format!(
                        "unconfirmed IPC client cache closes for {:?}",
                        client_close.unconfirmed
                    ));
                }
                outcome.ipc_client_close_error = Some(detail);
            }
        } else {
            outcome.ipc_clients_drained = true;
        }

        if let Some(server) = server {
            let recorded_direct_shutdown_outcomes = if matches!(
                server.lifecycle_state(),
                ServerLifecycleState::Stopping
                    | ServerLifecycleState::Stopped
                    | ServerLifecycleState::Failed(_)
            ) {
                match Self::server_runtime() {
                    Ok(rt) => match rt.block_on(server.observe_shutdown_completion_unbounded()) {
                        Ok(completion) => {
                            outcome.runtime_barrier_error = completion.runtime_error;
                            completion.route_outcomes
                        }
                        Err(err) => {
                            outcome.runtime_barrier_error = Some(err.to_string());
                            Vec::new()
                        }
                    },
                    Err(err) => {
                        outcome.runtime_barrier_error = Some(err.to_string());
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let mut recorded_route_names = std::collections::HashSet::new();
            for close in recorded_direct_shutdown_outcomes {
                recorded_route_names.insert(close.route_name.clone());
                outcome.removed_routes.push(close.route_name.clone());
                outcome
                    .route_outcomes
                    .push(route_close_from_server_outcome(close));
            }
            let mut can_signal_shutdown = true;
            let pending_runtime_unregisters: Vec<String> = route_names
                .iter()
                .filter(|route_name| !recorded_route_names.contains(route_name.as_str()))
                .cloned()
                .collect();
            if !pending_runtime_unregisters.is_empty() {
                match Self::server_runtime() {
                    Ok(rt) => {
                        let close_outcomes = rt.block_on(server.unregister_routes_for_shutdown(
                            &pending_runtime_unregisters,
                            "shutdown",
                        ));
                        for close in close_outcomes {
                            outcome.removed_routes.push(close.route_name.clone());
                            outcome
                                .route_outcomes
                                .push(route_close_from_server_outcome(close));
                        }
                    }
                    Err(err) => {
                        outcome.route_close_error = Some(err.to_string());
                        for route_name in &pending_runtime_unregisters {
                            outcome.route_outcomes.push(route_close_failure(
                                route_name,
                                "shutdown",
                                err.to_string(),
                            ));
                        }
                        can_signal_shutdown = false;
                    }
                }
            }
            if can_signal_shutdown && server_was_started {
                match Self::server_runtime() {
                    Ok(rt) => match rt.block_on(async {
                        server.request_shutdown_signal_with_reason("shutdown");
                        server.observe_shutdown_completion_unbounded().await
                    }) {
                        Ok(completion) => {
                            if let Some(error) = completion.runtime_error {
                                outcome.runtime_barrier_error = Some(error);
                            }
                            for close in completion.route_outcomes {
                                if !outcome.removed_routes.contains(&close.route_name) {
                                    outcome.removed_routes.push(close.route_name.clone());
                                    outcome
                                        .route_outcomes
                                        .push(route_close_from_server_outcome(close));
                                }
                            }
                        }
                        Err(err) => {
                            outcome.runtime_barrier_error = Some(err.to_string());
                        }
                    },
                    Err(err) => {
                        outcome.runtime_barrier_error = Some(err.to_string());
                    }
                }
            }
            let mut cleaned = std::collections::HashSet::new();
            for close in &outcome.route_outcomes {
                if close.local_removed && cleaned.insert(close.route_name.as_str()) {
                    if let Some(scope) = scopes.get(&close.route_name) {
                        if let Some(error) = self.relay_cleanup(
                            effective_relay_anchor_address.as_deref(),
                            relay_use_proxy,
                            scope,
                        ) {
                            outcome.relay_errors.push(error);
                        }
                    } else if effective_relay_anchor_address.is_some() {
                        outcome.relay_errors.push(RelayCleanupError { route_name: close.route_name.clone(), status_code: None,
                            message: "no captured registration scope; refusing unsafe name-only withdrawal".into() });
                    }
                }
            }
        } else if effective_relay_anchor_address.is_some() {
            for route_name in &route_names {
                outcome.relay_errors.push(RelayCleanupError {
                    route_name: route_name.clone(),
                    status_code: None,
                    message: "hostless shutdown has no captured registration scope".into(),
                });
            }
        }

        outcome
    }

    /// Shut down a client-only (hostless) Runtime.
    ///
    /// Rust and native SDK lifecycles without a Host/server use this path so
    /// outgoing IPC clients owned by this Runtime are still detached and
    /// closed through the bounded shared-ownership barrier. Relay routes and
    /// server state are not involved; nothing process-global is reset.
    pub fn shutdown_without_host(&self, shutdown_timeout: Duration) -> ShutdownOutcome {
        self.shutdown(None, Vec::new(), None, false, None, shutdown_timeout)
    }

    pub(crate) fn resolve_relay_connection(
        &self,
        relay_anchor_address: &str,
        expected: ExpectedRouteContract,
        relay_use_proxy: bool,
        max_attempts: usize,
        call_timeout_secs: f64,
        remote_payload_chunk_size: u64,
    ) -> Result<RelayResolvedConnection, LifecycleError> {
        let frozen_context = self.state.lock().frozen_local_endpoint.clone();
        let projection = self.relay_projection_for_address(
            relay_anchor_address,
            relay_use_proxy,
            frozen_context.as_ref(),
        )?;
        let client = RelayAwareHttpClient::new_with_control(
            Arc::clone(&projection.control),
            expected,
            projection.relay_use_proxy,
            RelayAwareClientConfig {
                max_attempts,
                call_timeout_secs,
                remote_payload_chunk_size,
            },
        )
        .map_err(runtime_http_error)?;
        let client = self.project_relay_client_context(client, true);
        match client.resolve_target().map_err(runtime_http_error)? {
            RelayResolvedTarget::Ipc { candidate } => {
                let context = client
                    .local_ipc_context()
                    .expect("selected IPC candidate has a compared context")
                    .clone();
                Ok(RelayResolvedConnection::Ipc {
                    client,
                    candidate,
                    context,
                })
            }
            RelayResolvedTarget::Http {
                route_uid,
                route_revision,
                ..
            } => Ok(RelayResolvedConnection::Http {
                client,
                route_uid,
                route_revision,
            }),
        }
    }

    pub(crate) fn resolve_relay_connection_after_local_ipc_failures(
        client: RelayAwareHttpClient,
        failed_candidates: &[RelayLocalIpcCandidate],
    ) -> Result<RelayResolvedConnection, LifecycleError> {
        match client
            .resolve_target_after_local_ipc_failures(failed_candidates)
            .map_err(runtime_http_error)?
        {
            RelayResolvedTarget::Ipc { candidate } => {
                let context = client
                    .local_ipc_context()
                    .expect("selected IPC candidate has a compared context")
                    .clone();
                Ok(RelayResolvedConnection::Ipc {
                    client,
                    candidate,
                    context,
                })
            }
            RelayResolvedTarget::Http {
                route_uid,
                route_revision,
                ..
            } => Ok(RelayResolvedConnection::Http {
                client,
                route_uid,
                route_revision,
            }),
        }
    }

    pub(crate) fn connect_explicit_relay_http_client(
        &self,
        relay_url: &str,
        expected: ExpectedRouteContract,
        relay_use_proxy: bool,
        max_attempts: usize,
        call_timeout_secs: f64,
        remote_payload_chunk_size: u64,
    ) -> Result<(RelayAwareHttpClient, String, u64), LifecycleError> {
        let client = RelayAwareHttpClient::new(
            relay_url,
            expected,
            relay_use_proxy,
            RelayAwareClientConfig {
                max_attempts,
                call_timeout_secs,
                remote_payload_chunk_size,
            },
        )
        .map_err(runtime_http_error)?;
        let client = self
            .project_relay_client_context(client, false)
            .with_http_only();
        match client.resolve_http_target().map_err(runtime_http_error)? {
            RelayResolvedTarget::Http {
                route_uid,
                route_revision,
                ..
            } => Ok((client, route_uid, route_revision)),
            RelayResolvedTarget::Ipc { .. } => unreachable!("HTTP relay connect returned IPC"),
        }
    }

    fn project_relay_client_context(
        &self,
        client: RelayAwareHttpClient,
        prefer_local_ipc: bool,
    ) -> RelayAwareHttpClient {
        let frozen = self.state.lock().frozen_local_endpoint.clone();
        if let Some(context) = frozen {
            return client.with_local_endpoint_context(&context);
        }
        if prefer_local_ipc && client.allows_local_ipc() {
            match self.local_endpoint_context() {
                Ok(context) => client.with_local_endpoint_context(&context),
                // An unusable local root/scope must not break a usable HTTP route.
                Err(_) => client.with_http_only(),
            }
        } else {
            client.with_http_only()
        }
    }

    /// Fence the captured comparison immediately before local acquire. A setter
    /// may have changed the Runtime root while relay resolution was in flight.
    /// Never reinterpret that candidate's logical address in a different domain.
    pub(crate) fn reconcile_relay_connection(
        &self,
        resolved: RelayResolvedConnection,
    ) -> Result<RelayResolvedConnection, LifecycleError> {
        let RelayResolvedConnection::Ipc {
            client,
            candidate,
            context,
        } = resolved
        else {
            return Ok(resolved);
        };
        if self.freeze_relay_candidate_context(&context) {
            return Ok(RelayResolvedConnection::Ipc {
                client,
                candidate,
                context,
            });
        }
        let client = client.with_http_only();
        match client.resolve_http_target().map_err(runtime_http_error)? {
            RelayResolvedTarget::Http {
                route_uid,
                route_revision,
                ..
            } => Ok(RelayResolvedConnection::Http {
                client,
                route_uid,
                route_revision,
            }),
            RelayResolvedTarget::Ipc { .. } => {
                unreachable!("HTTP-only reconciliation returned IPC")
            }
        }
    }

    fn freeze_relay_candidate_context(&self, captured: &c2_config::LocalEndpointContext) -> bool {
        self.resolve_local_endpoint_context_guarded(true, Some(captured))
            .is_ok_and(|context| context.is_some())
    }

    pub fn clear_relay_projection_cache(&self) {
        if let Some(projection) = self.state.lock().relay_projection.as_ref() {
            projection.control.clear_cache();
        }
    }

    fn relay_projection_for_address(
        &self,
        relay_anchor_address: &str,
        relay_use_proxy: bool,
        local_endpoint_context: Option<&c2_config::LocalEndpointContext>,
    ) -> Result<RelayProjection, LifecycleError> {
        let relay_anchor_address = canonical_relay_anchor_address(relay_anchor_address);
        {
            let state = self.state.lock();
            if let Some(projection) = state.relay_projection.as_ref()
                && projection.relay_anchor_address == relay_anchor_address
                && projection.relay_use_proxy == relay_use_proxy
                && projection.local_endpoint_context.as_ref() == local_endpoint_context
            {
                return Ok(projection.clone());
            }
        }

        let control = RelayControlClient::new(&relay_anchor_address, relay_use_proxy)
            .map_err(|e| LifecycleError::Relay(e.to_string()))?;
        let control = Arc::new(match local_endpoint_context {
            Some(context) => control.with_local_endpoint_context(context),
            None => control,
        });
        let projection = RelayProjection {
            relay_anchor_address,
            relay_use_proxy,
            control,
            local_endpoint_context: local_endpoint_context.cloned(),
        };
        self.state.lock().relay_projection = Some(projection.clone());
        Ok(projection)
    }

    pub(crate) fn server_runtime() -> Result<tokio::runtime::Runtime, LifecycleError> {
        #[cfg(test)]
        if FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.get()) {
            return Err(LifecycleError::Server(
                "failed to create runtime: injected test failure".to_string(),
            ));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LifecycleError::Server(format!("failed to create runtime: {e}")))?;
        Ok(rt)
    }

    fn relay_cleanup(
        &self,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
        scope: &RelayRegistrationScope,
    ) -> Option<RelayCleanupError> {
        let route_name = scope.name.as_str();
        let relay_anchor_address = relay_anchor_address?;
        let frozen_context = self.state.lock().frozen_local_endpoint.clone();
        let projection = match self.relay_projection_for_address(
            relay_anchor_address,
            relay_use_proxy,
            frozen_context.as_ref(),
        ) {
            Ok(projection) => projection,
            Err(err) => {
                return Some(RelayCleanupError {
                    route_name: route_name.to_string(),
                    status_code: None,
                    message: err.to_string(),
                });
            }
        };
        match projection.control.unregister_registration(scope) {
            Ok(()) => None,
            Err(HttpError::ServerError(status_code, body)) => Some(RelayCleanupError {
                route_name: route_name.to_string(),
                status_code: Some(status_code),
                message: if body.is_empty() {
                    format!("HTTP {status_code}")
                } else {
                    body
                },
            }),
            Err(err) => Some(RelayCleanupError {
                route_name: route_name.to_string(),
                status_code: None,
                message: err.to_string(),
            }),
        }
    }

    fn effective_relay_anchor_address_arg(
        &self,
        relay_anchor_address: Option<&str>,
    ) -> Result<Option<String>, LifecycleError> {
        match relay_anchor_address {
            Some(address) => Ok(Some(address.to_string())),
            None => self.effective_relay_anchor_address(),
        }
    }
}

fn canonical_relay_anchor_address(address: &str) -> String {
    address.trim().trim_end_matches('/').to_string()
}

fn resolve_call_execution_limits(
    overrides: c2_config::CallExecutionLimitsOverrides,
    sources: Option<c2_config::ConfigSources>,
) -> Result<c2_config::CallExecutionLimits, LifecycleError> {
    c2_config::ConfigResolver::resolve_call_execution_limits(
        overrides,
        sources.unwrap_or_else(c2_config::ConfigSources::from_process),
    )
    .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

fn resolve_local_endpoint(
    options: c2_config::LocalEndpointOptions,
    sources: Option<c2_config::ConfigSources>,
) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
    c2_config::ConfigResolver::resolve_local_endpoint(
        options,
        sources.unwrap_or_else(c2_config::ConfigSources::from_process),
    )
    .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Resolve from a metadata snapshot, outside the Runtime lock.
fn resolve_client_config(
    runtime_overrides: c2_config::RuntimeConfigOverrides,
) -> Result<c2_config::ClientIpcConfig, LifecycleError> {
    c2_config::ConfigResolver::resolve_client_ipc(
        runtime_overrides.client_ipc.clone(),
        runtime_overrides,
        c2_config::ConfigSources::from_process(),
    )
    .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

fn runtime_http_error(err: HttpError) -> LifecycleError {
    match err {
        HttpError::ServerError(status_code, message) => LifecycleError::RelayHttp {
            status_code,
            message,
        },
        other => LifecycleError::Relay(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use c2_server::config::ServerIpcConfig;
    use c2_server::{
        AccessLevel, BuiltRoute, ConcurrencyMode, CrmCallback, CrmError, RequestData, ResponseMeta,
        RouteBuildSpec, SchedulerLimits,
    };

    fn observer_runtime() -> Runtime {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        runtime
            .set_call_execution_limits_with_sources(
                c2_config::CallExecutionLimitsOverrides {
                    max_outstanding_calls: Some(8),
                    retained_input_budget_bytes: Some(256),
                },
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        runtime
    }

    #[test]
    fn call_execution_observer_captured_before_concurrent_initialization_sees_actual_domain() {
        use c2_config::{CallOptions, CallTimeout};
        let runtime = observer_runtime();
        let observer = runtime.call_execution_observer();
        assert!(!observer.snapshot().initialized);
        assert!(observer.is_live());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let context = runtime.call_execution_context().unwrap();
                    let mut call = context
                        .prepare(
                            CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                            None,
                        )
                        .unwrap();
                    call.charge_input(32).unwrap();
                    call
                })
            })
            .collect::<Vec<_>>();
        let calls = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let observed = observer.snapshot();
        assert!(observed.initialized);
        assert!(!observed.closed);
        let counters = observed.counters.unwrap();
        assert_eq!(
            (counters.used_operations, counters.used_retained_bytes),
            (8, 256)
        );
        assert_eq!(counters, runtime.call_execution_snapshot().unwrap());
        assert!(observer.same_domain(&runtime.clone().call_execution_observer()));
        drop(calls);
        assert_eq!(observer.snapshot().counters.unwrap().used_operations, 0);
        assert!(observer.is_live());
    }

    #[test]
    fn call_execution_observer_preserves_actual_input_after_runtime_drop_without_retaining_session()
    {
        use c2_config::{CallOptions, CallTimeout};
        let runtime = observer_runtime();
        let weak_session = Arc::downgrade(&runtime.state);
        let observer = runtime.call_execution_observer();
        let context = runtime.call_execution_context().unwrap();
        let input = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                None,
            )
            .unwrap()
            .execute(
                32,
                || Ok(vec![7; 32]),
                |input, scope| async move {
                    scope
                        .try_begin_dispatch()
                        .map_err(crate::call_execution::scope_error)?;
                    Ok(input)
                },
            )
            .unwrap()
            .wait()
            .unwrap();
        drop(context);
        drop(runtime);
        // Last RuntimeState is gone: no service/transport/cache can be kept
        // alive by the observer. Its budget metadata still sees the real Vec.
        assert!(weak_session.upgrade().is_none());
        assert_eq!(input.as_ref().as_ref(), &[7; 32]);
        let observation = observer.snapshot();
        assert!(observation.closed);
        let counters = observation.counters.unwrap();
        assert!(counters.closed);
        assert_eq!(
            (counters.used_operations, counters.used_retained_bytes),
            (1, 32)
        );
        assert!(observer.is_live());
        drop(input);
        let until = Instant::now() + Duration::from_secs(10);
        while observer.is_live() {
            assert!(
                Instant::now() < until,
                "real input owner did not release its charge"
            );
            std::thread::yield_now();
        }
        let counters = observer.snapshot().counters.unwrap();
        assert_eq!(
            (counters.used_operations, counters.used_retained_bytes),
            (0, 0)
        );
        assert!(counters.closed);
    }

    #[test]
    fn call_execution_observer_freeze_shutdown_race_always_publishes_closed_budget() {
        for _ in 0..32 {
            let runtime = observer_runtime();
            let observer = runtime.call_execution_observer();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let initializing = {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    runtime.call_execution_context().unwrap()
                })
            };
            barrier.wait();
            runtime.shutdown(None, Vec::new(), None, false, None, Duration::from_secs(1));
            let context = initializing.join().unwrap();
            assert!(context.snapshot().closed);
            let observed = observer.snapshot();
            assert!(observed.initialized && observed.closed);
            assert!(observed.counters.unwrap().closed);
            assert!(!observer.is_live());
        }
    }

    #[test]
    fn call_execution_observer_never_resolves_or_freezes_configuration() {
        let runtime = observer_runtime();
        // Invalid resolver data proves observer reads do not invoke resolution.
        runtime.state.lock().call_execution_sources = Some(c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([(
                "C2_CALL_MAX_OUTSTANDING".into(),
                "invalid".into(),
            )]),
        });
        runtime.state.lock().call_execution_overrides = Default::default();
        let observer = runtime.call_execution_observer();
        assert!(observer.is_live());
        assert_eq!(observer.snapshot().counters, None);
        assert!(runtime.call_execution_snapshot().is_err());
        assert!(runtime.state.lock().call_execution.is_none());
        runtime
            .set_call_execution_limits_with_sources(
                c2_config::CallExecutionLimitsOverrides {
                    max_outstanding_calls: Some(3),
                    retained_input_budget_bytes: Some(64),
                },
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        assert!(!observer.snapshot().initialized);
    }

    #[test]
    fn call_execution_accepted_overrides_update_idempotently_without_replacing_budget() {
        let runtime = observer_runtime();
        let mut call = runtime
            .call_execution_context()
            .unwrap()
            .prepare(
                c2_config::CallOptions::with_timeout(c2_config::CallTimeout::After(
                    Duration::from_secs(10),
                )),
                None,
            )
            .unwrap();
        call.charge_input(32).unwrap();
        let observer = runtime.call_execution_observer();
        let sources = c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([
                ("C2_CALL_MAX_OUTSTANDING".into(), "8".into()),
                ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES".into(), "256".into()),
            ]),
        };
        runtime
            .set_call_execution_limits_with_sources(Default::default(), sources)
            .unwrap();
        let accepted = runtime.call_execution_limits_overrides();
        assert_eq!(accepted.max_outstanding_calls, None);
        assert_eq!(accepted.retained_input_budget_bytes, None);
        assert_eq!(observer.snapshot().counters.unwrap().max_operations, 8);
        assert_eq!(
            observer.snapshot().counters.unwrap().used_retained_bytes,
            32
        );
        assert!(
            runtime
                .set_call_execution_limits_with_sources(
                    c2_config::CallExecutionLimitsOverrides {
                        max_outstanding_calls: Some(9),
                        retained_input_budget_bytes: None,
                    },
                    c2_config::ConfigSources::empty(),
                )
                .is_err()
        );
        assert_eq!(
            runtime
                .call_execution_limits_overrides()
                .max_outstanding_calls,
            None
        );
    }

    #[test]
    fn http_projection_does_not_resolve_invalid_local_configuration() {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        // Model lazy process configuration with a real isolated resolver map;
        // no process-wide environment mutation or local platform lookup.
        runtime.state.lock().local_endpoint_sources = Some(c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([("C2_IPC_ROOT".into(), "relative".into())]),
        });
        let expected = c2_contract::ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap()
        .expected_route("route")
        .unwrap();
        for (anchor, prefer_local) in [
            ("http://127.0.0.1:9", false),
            ("https://relay.example", true),
            ("http://127.0.0.1:9", true),
        ] {
            let client = RelayAwareHttpClient::new(
                anchor,
                expected.clone(),
                false,
                RelayAwareClientConfig::default(),
            )
            .unwrap();
            let client = runtime.project_relay_client_context(client, prefer_local);
            assert!(!client.allows_local_ipc());
            assert!(client.local_ipc_context().is_none());
            assert!(!runtime.local_endpoint_frozen());
            assert!(runtime.outgoing_memory_stats().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_relay_namespace_is_checked_against_the_final_freeze() {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        let at = |root: &str| c2_config::LocalEndpointOptions {
            unix_root: Some(root.into()),
        };
        runtime
            .set_local_endpoint_with_sources(
                at("/tmp/c2-captured-a"),
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        let captured = runtime.local_endpoint_context().unwrap();
        assert!(!runtime.local_endpoint_frozen());
        runtime
            .set_local_endpoint_with_sources(
                at("/tmp/c2-captured-b"),
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        assert!(!runtime.freeze_relay_candidate_context(&captured));
        assert!(!runtime.local_endpoint_frozen());
        assert_ne!(captured, runtime.local_endpoint_context().unwrap());
        runtime
            .set_local_endpoint_with_sources(
                at("/tmp/c2-captured-c"),
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        let current = runtime.local_endpoint_context().unwrap();
        assert!(!runtime.local_endpoint_frozen());
        // This is the private imminent-acquire fence, rather than a pure query.
        assert!(runtime.freeze_relay_candidate_context(&current));
        assert!(runtime.local_endpoint_frozen());
        assert!(!runtime.freeze_relay_candidate_context(&captured));
        assert_eq!(runtime.local_endpoint_context().unwrap(), current);
        assert!(runtime.outgoing_memory_stats().is_none());
        assert!(
            runtime
                .set_local_endpoint_with_sources(
                    at("/tmp/c2-captured-a"),
                    c2_config::ConfigSources::empty()
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_relay_candidate_context_does_not_publish_a_freeze() {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        runtime
            .set_local_endpoint_with_sources(
                c2_config::LocalEndpointOptions::default(),
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        let captured = runtime.local_endpoint_context().unwrap();
        runtime.state.lock().local_endpoint_sources = Some(c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([("C2_IPC_ROOT".into(), "relative".into())]),
        });
        assert!(!runtime.freeze_relay_candidate_context(&captured));
        assert!(!runtime.local_endpoint_frozen());
        assert!(runtime.outgoing_memory_stats().is_none());
        runtime
            .set_local_endpoint_with_sources(
                c2_config::LocalEndpointOptions::default(),
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        assert_eq!(runtime.local_endpoint_context().unwrap(), captured);
    }

    #[cfg(unix)]
    #[test]
    fn relay_candidate_publication_and_setter_share_one_revision_fence() {
        for _ in 0..32 {
            let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions {
                        unix_root: Some("/tmp/c2-fence-a".into()),
                    },
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            let captured = runtime.local_endpoint_context().unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let setter = runtime.clone();
            let setter_barrier = barrier.clone();
            let thread = std::thread::spawn(move || {
                setter_barrier.wait();
                setter.set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions {
                        unix_root: Some("/tmp/c2-fence-b".into()),
                    },
                    c2_config::ConfigSources::empty(),
                )
            });
            barrier.wait();
            let accepted = runtime.freeze_relay_candidate_context(&captured);
            let set = thread.join().unwrap();
            if accepted {
                assert_eq!(set, Err(LifecycleError::ConfigFrozen));
                assert!(runtime.local_endpoint_frozen());
                assert_eq!(runtime.local_endpoint_context().unwrap(), captured);
            } else {
                assert!(set.is_ok());
                assert!(!runtime.local_endpoint_frozen());
                assert_ne!(runtime.local_endpoint_context().unwrap(), captured);
            }
            assert!(runtime.outgoing_memory_stats().is_none());
        }
    }

    #[test]
    fn call_execution_snapshot_resolves_sources_without_freezing() {
        use c2_config::{CallExecutionLimitsOverrides, ConfigSources, EnvMap};
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        runtime
            .set_call_execution_limits_with_sources(
                CallExecutionLimitsOverrides::default(),
                ConfigSources {
                    env_file: c2_config::EnvFilePolicy::Disabled,
                    process_env: EnvMap::from([
                        ("C2_CALL_MAX_OUTSTANDING".into(), "3".into()),
                        ("C2_CALL_RETAINED_INPUT_BUDGET_BYTES".into(), "17".into()),
                    ]),
                },
            )
            .unwrap();
        let snapshot = runtime.call_execution_snapshot().unwrap();
        assert_eq!(
            (snapshot.max_operations, snapshot.max_retained_bytes),
            (3, 17)
        );
        assert_eq!(
            (snapshot.used_operations, snapshot.used_retained_bytes),
            (0, 0)
        );
        // Observation did not freeze: a later typed override wins.
        runtime
            .set_call_execution_limits_with_sources(
                CallExecutionLimitsOverrides {
                    max_outstanding_calls: Some(1),
                    retained_input_budget_bytes: Some(32),
                },
                ConfigSources::empty(),
            )
            .unwrap();
        assert_eq!(runtime.call_execution_snapshot().unwrap().max_operations, 1);
    }

    #[test]
    fn call_execution_clones_freeze_one_domain_without_laundering_live_permits() {
        use c2_config::{CallExecutionLimitsOverrides, CallOptions, CallTimeout, ConfigSources};
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        let overrides = CallExecutionLimitsOverrides {
            max_outstanding_calls: Some(1),
            retained_input_budget_bytes: Some(32),
        };
        runtime
            .set_call_execution_limits_with_sources(overrides.clone(), ConfigSources::empty())
            .unwrap();
        let clone = runtime.clone();
        let context = runtime.call_execution_context().unwrap();
        let mut call = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                None,
            )
            .unwrap();
        call.charge_input(32).unwrap();
        assert_eq!(
            clone.call_execution_snapshot().unwrap().used_retained_bytes,
            32
        );
        clone
            .set_call_execution_limits_with_sources(overrides, ConfigSources::empty())
            .unwrap();
        assert_eq!(clone.call_execution_snapshot().unwrap().used_operations, 1);
        assert_eq!(
            clone.set_call_execution_limits_with_sources(
                CallExecutionLimitsOverrides {
                    max_outstanding_calls: Some(2),
                    retained_input_budget_bytes: Some(64)
                },
                ConfigSources::empty(),
            ),
            Err(LifecycleError::ConfigFrozen)
        );
        assert!(
            clone
                .call_execution_context()
                .unwrap()
                .prepare(
                    CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                    None
                )
                .is_err()
        );
        drop(call);
        assert_eq!(clone.call_execution_snapshot().unwrap().used_operations, 0);
    }

    #[test]
    fn call_execution_shutdown_closes_admission_and_keeps_actual_charge() {
        use c2_config::{CallExecutionLimitsOverrides, CallOptions, CallTimeout, ConfigSources};
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        runtime
            .set_call_execution_limits_with_sources(
                CallExecutionLimitsOverrides {
                    max_outstanding_calls: Some(1),
                    retained_input_budget_bytes: Some(32),
                },
                ConfigSources::empty(),
            )
            .unwrap();
        let context = runtime.call_execution_context().unwrap();
        let observer = runtime.call_execution_observer();
        let mut call = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                None,
            )
            .unwrap();
        call.charge_input(32).unwrap();
        runtime.shutdown(None, Vec::new(), None, false, None, Duration::from_secs(1));
        let snapshot = runtime.call_execution_snapshot().unwrap();
        assert!(snapshot.closed);
        assert_eq!(
            (snapshot.used_operations, snapshot.used_retained_bytes),
            (1, 32)
        );
        assert_eq!(observer.snapshot().counters, Some(snapshot));
        assert!(observer.is_live());
        assert!(
            context
                .prepare(
                    CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(10))),
                    None
                )
                .is_err()
        );
        // Explicit Unlimited stays uncharged even in the closed finite domain.
        let unlimited = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::Unlimited),
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        drop(unlimited);
        assert_eq!(
            runtime
                .call_execution_snapshot()
                .unwrap()
                .used_retained_bytes,
            32
        );
        drop(call);
        assert_eq!(
            runtime
                .call_execution_snapshot()
                .unwrap()
                .used_retained_bytes,
            0
        );
        assert!(!observer.is_live());
    }

    static TEST_ID: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NoopCallback;

    impl CrmCallback for NoopCallback {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<c2_mem::MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Empty)
        }
    }

    fn unique_route_name(prefix: &str) -> String {
        let n = TEST_ID.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{n}", std::process::id())
    }

    fn test_server(prefix: &str) -> Arc<c2_server::Server> {
        let name = unique_route_name(prefix);
        Arc::new(
            c2_server::Server::new(&format!("ipc://{name}"), ServerIpcConfig::default())
                .expect("test server should construct"),
        )
    }

    fn dummy_route_spec(name: &str) -> RuntimeRouteSpec {
        RuntimeRouteSpec {
            name: name.to_string(),
            crm_ns: "test.runtime".to_string(),
            crm_name: "Runtime".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            method_names: vec!["ping".to_string()],
            access_map: HashMap::new(),
            concurrency_mode: ConcurrencyMode::ReadParallel,
            max_pending: None,
            max_workers: None,
        }
    }

    fn dummy_route(server: &Arc<c2_server::Server>, name: &str) -> BuiltRoute {
        let spec = dummy_route_spec(name);
        server
            .build_route(
                RouteBuildSpec {
                    name: spec.name,
                    crm_ns: spec.crm_ns,
                    crm_name: spec.crm_name,
                    crm_ver: spec.crm_ver,
                    abi_hash: spec.abi_hash,
                    signature_hash: spec.signature_hash,
                    method_names: spec.method_names,
                    access_map: spec.access_map,
                    concurrency_mode: spec.concurrency_mode,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(NoopCallback),
            )
            .expect("dummy route should build")
    }

    fn register_dummy(server: &Arc<c2_server::Server>, name: &str) {
        let rt = Runtime::server_runtime().expect("runtime");
        let route = dummy_route(server, name);
        let reservation = rt
            .block_on(server.reserve_route(route))
            .expect("dummy route should reserve");
        rt.block_on(server.commit_reserved_route(reservation))
            .expect("dummy route should register");
    }

    fn with_process_relay_anchor(value: &str, test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let previous_anchor = std::env::var_os("C2_RELAY_ANCHOR_ADDRESS");
        let previous_env_file = std::env::var_os("C2_ENV_FILE");
        // SAFETY: this test holds a process-local mutex while mutating the
        // environment and restores the original values before releasing it.
        unsafe {
            std::env::set_var("C2_RELAY_ANCHOR_ADDRESS", value);
            std::env::set_var("C2_ENV_FILE", "");
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        // SAFETY: see the setup block above; restoration is performed while
        // holding the same process-local mutex.
        unsafe {
            match previous_anchor {
                Some(value) => std::env::set_var("C2_RELAY_ANCHOR_ADDRESS", value),
                None => std::env::remove_var("C2_RELAY_ANCHOR_ADDRESS"),
            }
            match previous_env_file {
                Some(value) => std::env::set_var("C2_ENV_FILE", value),
                None => std::env::remove_var("C2_ENV_FILE"),
            }
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn explicit_server_id_derives_ipc_address() {
        let session = Runtime::new(RuntimeOptions {
            server_id: Some("unit-server".to_string()),
            server_ipc_overrides: None,
            client_ipc_overrides: None,
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        })
        .expect("session should accept valid server id");

        assert_eq!(session.server_id(), None);
        assert_eq!(session.server_address(), None);

        let identity = session.ensure_server().expect("ensure server");
        assert_eq!(identity.server_id, "unit-server");
        assert_eq!(identity.ipc_address, "ipc://unit-server");
        assert_eq!(session.server_id().as_deref(), Some("unit-server"));
        assert_eq!(
            session.server_address().as_deref(),
            Some("ipc://unit-server")
        );
    }

    #[test]
    fn invalid_server_id_is_rejected() {
        for bad in ["", " ", "bad/name", "bad\\name", ".", "..", "bad\nid"] {
            let err = Runtime::new(RuntimeOptions {
                server_id: Some(bad.to_string()),
                server_ipc_overrides: None,
                client_ipc_overrides: None,
                shm_threshold: None,
                remote_payload_chunk_size: None,
                relay_anchor_address: None,
                use_process_relay_anchor: true,
            })
            .expect_err("invalid server id should be rejected");
            assert!(
                err.to_string().contains("server_id"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn auto_server_id_is_valid_and_address_matches() {
        let session = Runtime::new(RuntimeOptions::default()).expect("session");
        let identity = session.ensure_server().expect("ensure server");

        c2_config::validate_server_id(&identity.server_id).expect("generated id should validate");
        assert!(identity.server_id.starts_with("cc"));
        assert_eq!(
            identity.ipc_address,
            format!("ipc://{}", identity.server_id)
        );
        assert_eq!(session.ensure_server().expect("idempotent"), identity);
    }

    #[test]
    fn clear_server_identity_preserves_explicit_override() {
        let session = Runtime::new(RuntimeOptions {
            server_id: Some("unit-retry".to_string()),
            server_ipc_overrides: None,
            client_ipc_overrides: None,
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        })
        .expect("session should accept valid server id");

        let first = session.ensure_server().expect("ensure server");
        assert_eq!(first.server_id, "unit-retry");
        session.clear_server_identity().unwrap();
        assert_eq!(session.server_id(), None);
        assert_eq!(session.server_address(), None);

        let second = session.ensure_server().expect("ensure server again");
        assert_eq!(second.server_id, "unit-retry");
        assert_eq!(second.ipc_address, "ipc://unit-retry");
    }

    #[test]
    fn server_ipc_overrides_are_session_owned() {
        let overrides = ServerIpcConfigOverrides {
            pool_segment_size: Some(2 * 1024 * 1024),
            max_pool_segments: Some(2),
            ..Default::default()
        };
        let session = Runtime::new(RuntimeOptions {
            server_id: Some("unit-server-overrides".to_string()),
            server_ipc_overrides: Some(overrides),
            client_ipc_overrides: None,
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        })
        .expect("session should accept valid options");

        let projected = session
            .server_ipc_overrides()
            .expect("overrides should be stored");
        assert_eq!(projected.pool_segment_size, Some(2 * 1024 * 1024));
        assert_eq!(projected.max_pool_segments, Some(2));
    }

    #[test]
    fn client_ipc_overrides_are_session_owned() {
        let overrides = ClientIpcConfigOverrides {
            reassembly_segment_size: Some(16 * 1024 * 1024),
            ..Default::default()
        };
        let session = Runtime::new(RuntimeOptions {
            server_id: None,
            server_ipc_overrides: None,
            client_ipc_overrides: Some(overrides),
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        })
        .expect("session should accept valid options");

        let projected = session
            .client_ipc_overrides()
            .expect("overrides should be stored");
        assert_eq!(projected.reassembly_segment_size, Some(16 * 1024 * 1024));
    }

    #[test]
    fn relay_anchor_address_override_is_canonicalized_and_clear_cache_is_safe() {
        let session = Runtime::new(RuntimeOptions {
            relay_anchor_address: Some(" http://relay.test/ ".to_string()),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");

        assert_eq!(
            session.relay_anchor_address_override().as_deref(),
            Some("http://relay.test"),
        );

        session.clear_relay_projection_cache();
        session.set_relay_anchor_address(Some("http://relay-b.test/".to_string()));
        assert_eq!(
            session.relay_anchor_address_override().as_deref(),
            Some("http://relay-b.test"),
        );
    }

    #[test]
    fn process_relay_anchor_can_be_disabled_for_standalone_direct_ipc() {
        with_process_relay_anchor("http://127.0.0.1:9", || {
            let session = Runtime::new(RuntimeOptions {
                use_process_relay_anchor: false,
                ..Default::default()
            })
            .expect("session");
            assert_eq!(
                session
                    .effective_relay_anchor_address()
                    .expect("relay lookup should not fail"),
                None,
            );

            let default_session = Runtime::new(RuntimeOptions::default()).expect("session");
            assert_eq!(
                default_session
                    .effective_relay_anchor_address()
                    .expect("default session should read process relay anchor")
                    .as_deref(),
                Some("http://127.0.0.1:9"),
            );
        });
    }

    #[test]
    fn unregister_missing_route_reports_missing_route() {
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("missing-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("missing-server");
        let err = session
            .unregister_route(&server, "absent", None, false)
            .expect_err("missing route should error");
        assert_eq!(err, LifecycleError::MissingRoute("absent".to_string()));
    }

    #[test]
    fn register_route_rejects_crm_contract_mismatch() {
        let route_name = unique_route_name("crm-contract-mismatch");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("crm-contract-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("crm-contract-server");
        let route = dummy_route(&server, &route_name);
        let mut spec = dummy_route_spec(&route_name);
        spec.crm_ns = "different.ns".to_string();

        let err = session
            .register_route(&server, route, spec, None, false)
            .expect_err("CRM contract mismatch should be rejected before registration");
        assert!(
            err.to_string().contains("crm contract mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn register_route_rejects_access_map_mismatch() {
        let route_name = unique_route_name("access-map-mismatch");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("access-map-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("access-map-server");
        let route = dummy_route(&server, &route_name);
        let mut spec = dummy_route_spec(&route_name);
        spec.access_map.insert(0, AccessLevel::Read);

        let err = session
            .register_route(&server, route, spec, None, false)
            .expect_err("access-map mismatch should be rejected before registration");
        assert!(
            err.to_string().contains("access map mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unregister_without_relay_does_not_publish_lazy_identity() {
        let route_name = unique_route_name("no-relay-unregister");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        let server = test_server("no-relay-unregister-server");
        register_dummy(&server, &route_name);

        let outcome = session
            .unregister_route(&server, &route_name, None, false)
            .expect("local unregister should succeed");

        assert!(outcome.local_removed);
        assert!(outcome.close.active_drained);
        assert_eq!(outcome.close.closed_reason, "unregister");
        assert!(outcome.relay_error.is_none());
        assert_eq!(session.server_id(), None);
        assert_eq!(session.server_address(), None);
    }

    #[test]
    fn unregister_relay_failure_returns_structured_outcome_after_local_remove() {
        let route_name = unique_route_name("relay-fail-route");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("relay-fail-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-fail-server");
        register_dummy(&server, &route_name);

        let outcome = session
            .unregister_route(&server, &route_name, Some("http://127.0.0.1:9"), false)
            .expect("local unregister should succeed despite relay failure");
        assert_eq!(outcome.route_name, route_name);
        assert!(outcome.local_removed);
        let relay_error = outcome.relay_error.expect("relay error should be captured");
        assert_eq!(relay_error.route_name, outcome.route_name);
        assert_eq!(relay_error.status_code, None);
        assert!(!relay_error.message.is_empty());

        let err = session
            .unregister_route(&server, &outcome.route_name, None, false)
            .expect_err("route should have already been removed locally");
        assert!(matches!(err, LifecycleError::MissingRoute(_)));
    }

    #[test]
    fn relay_backed_registration_keeps_route_invisible_until_publish_commits() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        let route_name = unique_route_name("relay-commit-gate");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("relay-commit-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-commit-server");
        let route = dummy_route(&server, &route_name);
        let spec = dummy_route_spec(&route_name);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake relay");
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let relay_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept relay register");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read relay register");
            request_seen_tx
                .send(String::from_utf8_lossy(&buf[..n]).to_string())
                .unwrap();
            release_rx.recv().unwrap();
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .expect("write relay response");
        });

        let server_for_thread = Arc::clone(&server);
        let register_thread = thread::spawn(move || {
            session.register_route(&server_for_thread, route, spec, Some(&relay_url), false)
        });

        let request = request_seen_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("relay register request should arrive");
        assert!(
            request.contains("\"prepare_only\":true"),
            "relay-backed registration must perform a non-publishing prepare before local commit"
        );

        let rt = Runtime::server_runtime().expect("runtime");
        let visible_while_relay_pending = rt.block_on(server.contains_route(&route_name));

        release_tx.send(()).unwrap();
        relay_thread.join().unwrap();
        let result = register_thread.join().unwrap();
        let visible_after_failure = rt.block_on(server.contains_route(&route_name));

        let failure = match result {
            Err(LifecycleError::RegisterFailure(failure)) => failure,
            other => panic!("unexpected register result: {other:?}"),
        };
        assert!(
            !visible_while_relay_pending,
            "route became visible before relay-backed registration committed",
        );
        assert_eq!(failure.route_name, route_name);
        assert_eq!(failure.failure_source, "relay_prepare");
        let rollback = failure
            .rollback
            .expect("rollback outcome should be preserved");
        assert_eq!(rollback.route_name, route_name);
        assert!(!rollback.local_removed);
        assert!(rollback.active_drained);
        assert_eq!(rollback.closed_reason, "registration_rollback");
        assert_eq!(rollback.close_error, None);
        assert!(
            !visible_after_failure,
            "failed registration must not leave route visible"
        );
    }

    #[test]
    fn relay_register_failure_after_prepare_rolls_back_and_preserves_cleanup_error() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        let route_name = unique_route_name("relay-final-fail");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("relay-final-fail-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-final-fail-server");
        let route = dummy_route(&server, &route_name);
        let spec = dummy_route_spec(&route_name);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake relay");
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, request_rx) = mpsc::channel();
        let relay_thread = thread::spawn(move || {
            for response in [
                "HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 12\r\n\r\nfinal failed",
                "HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 14\r\n\r\ncleanup failed",
            ] {
                let (mut stream, _) = listener.accept().expect("accept relay request");
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).expect("read relay request");
                request_tx
                    .send(String::from_utf8_lossy(&buf[..n]).to_string())
                    .unwrap();
                stream
                    .write_all(response.as_bytes())
                    .expect("write relay response");
            }
        });

        let result = session.register_route(&server, route, spec, Some(&relay_url), false);
        relay_thread.join().unwrap();
        let requests = (0..3)
            .map(|_| {
                request_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
            })
            .collect::<Vec<_>>();

        assert!(requests[0].contains("\"prepare_only\":true"));
        assert!(!requests[1].contains("\"prepare_only\":true"));
        assert!(requests[2].contains("/_unregister"));

        let failure = match result {
            Err(LifecycleError::RegisterFailure(failure)) => failure,
            other => panic!("unexpected register result: {other:?}"),
        };
        assert_eq!(failure.failure_source, "relay_register");
        assert_eq!(failure.status_code, Some(409));
        let rollback = failure
            .rollback
            .expect("rollback outcome should be preserved");
        assert_eq!(rollback.route_name, route_name);
        assert!(rollback.local_removed);
        assert!(rollback.active_drained);
        assert_eq!(rollback.closed_reason, "registration_rollback");
        assert_eq!(rollback.close_error, None);
        let relay_cleanup_error = failure
            .relay_cleanup_error
            .expect("relay cleanup failure should be preserved");
        assert_eq!(relay_cleanup_error.status_code, Some(409));
        assert_eq!(relay_cleanup_error.message, "cleanup failed");

        let rt = Runtime::server_runtime().expect("runtime");
        assert!(
            !rt.block_on(server.contains_route(&route_name)),
            "relay final publish failure must roll back local route"
        );
    }

    #[test]
    fn relay_backed_registration_keeps_committed_route_closed_until_publish_finalizes() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        let route_name = unique_route_name("relay-publish-gate");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("relay-publish-gate-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-publish-gate-server");
        let route = dummy_route(&server, &route_name);
        let spec = dummy_route_spec(&route_name);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake relay");
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let (final_request_tx, final_request_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let relay_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept prepare request");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).expect("read prepare request");
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .expect("write prepare response");

            let (mut stream, _) = listener.accept().expect("accept final publish request");
            let n = stream.read(&mut buf).expect("read final publish request");
            final_request_tx
                .send(String::from_utf8_lossy(&buf[..n]).to_string())
                .unwrap();
            release_rx.recv().unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 12\r\n\r\nfinal failed",
                )
                .expect("write final publish response");
        });

        let registration_observer = session.clone();
        let server_for_thread = Arc::clone(&server);
        let register_thread = thread::spawn(move || {
            session.register_route(&server_for_thread, route, spec, Some(&relay_url), false)
        });

        let final_request = final_request_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("final relay register request should arrive");
        assert!(
            !final_request.contains("\"prepare_only\":true"),
            "final relay publish must use ordinary register after local commit"
        );

        let rt = Runtime::server_runtime().expect("runtime");
        assert!(
            rt.block_on(server.contains_route(&route_name)),
            "route should be committed for final IPC re-attestation"
        );
        let snapshot = rt
            .block_on(server.route_scheduler_snapshot(&route_name))
            .expect("committed route should have scheduler state");
        let was_closed_during_final_publish = snapshot.closed;
        let native_identity = server.registered_route_identity(&route_name).unwrap();
        let key = format!("{}:{:p}", server.server_instance_id(), server.as_ref());
        let observed_scope = registration_observer
            .state
            .lock()
            .registration_scopes
            .get(&key)
            .unwrap()
            .get(&route_name)
            .unwrap()
            .clone();
        assert_eq!(
            (observed_scope.route_uid, observed_scope.route_revision),
            native_identity,
            "registration scope must be saved before final publish can return or owner drain can remove the catalog record"
        );

        release_tx.send(()).unwrap();
        relay_thread.join().unwrap();
        let result = register_thread.join().unwrap();
        let failure = match result {
            Err(LifecycleError::RegisterFailure(failure)) => failure,
            other => panic!("unexpected register result: {other:?}"),
        };
        assert!(
            was_closed_during_final_publish,
            "relay-backed route must not admit direct IPC calls before final relay publish completes"
        );
        assert_eq!(failure.failure_source, "relay_register");
        assert!(
            !rt.block_on(server.contains_route(&route_name)),
            "failed final publish must still roll back the locally committed route"
        );
    }

    #[test]
    fn shutdown_is_idempotent_and_reports_removed_routes_once() {
        let first = unique_route_name("shutdown-a");
        let second = unique_route_name("shutdown-b");
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("shutdown-session")),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("shutdown-server");
        register_dummy(&server, &first);
        register_dummy(&server, &second);

        let first_outcome = session.shutdown(
            Some(&server),
            vec![first.clone(), second.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        assert_eq!(first_outcome.removed_routes, vec![first, second]);
        assert!(first_outcome.relay_errors.is_empty());

        let second_outcome = session.shutdown(
            Some(&server),
            first_outcome.removed_routes.clone(),
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        assert!(second_outcome.removed_routes.is_empty());
        assert!(second_outcome.relay_errors.is_empty());
    }

    #[test]
    fn shutdown_closes_all_route_admissions_before_waiting_for_drain() {
        let first = unique_route_name("shutdown-close-a");
        let second = unique_route_name("shutdown-close-b");
        let session = Arc::new(
            Runtime::new(RuntimeOptions {
                server_id: Some(unique_route_name("shutdown-close-session")),
                use_process_relay_anchor: false,
                ..Default::default()
            })
            .expect("session"),
        );
        let server = test_server("shutdown-close-server");

        let first_route = dummy_route(&server, &first);
        let first_handle = first_route.route_handle();
        session
            .register_route(&server, first_route, dummy_route_spec(&first), None, false)
            .expect("first route should register");

        let second_route = dummy_route(&server, &second);
        let second_handle = second_route.route_handle();
        session
            .register_route(
                &server,
                second_route,
                dummy_route_spec(&second),
                None,
                false,
            )
            .expect("second route should register");

        let first_guard = first_handle
            .blocking_acquire(0)
            .expect("first route guard should enter");
        let shutdown_session = Arc::clone(&session);
        let shutdown_server = Arc::clone(&server);
        let route_names = vec![first.clone(), second.clone()];
        let shutdown_thread = std::thread::spawn(move || {
            shutdown_session.shutdown(
                Some(&shutdown_server),
                route_names,
                None,
                false,
                None,
                Duration::from_secs(5),
            )
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !first_handle.snapshot().closed {
            assert!(
                std::time::Instant::now() < deadline,
                "shutdown should close the first route while waiting for its active guard"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            second_handle.snapshot().closed,
            "shutdown must close every target route admission before waiting for any route to drain"
        );

        drop(first_guard);
        let outcome = shutdown_thread.join().expect("shutdown thread should join");
        assert_eq!(outcome.removed_routes, vec![first, second]);
        assert!(outcome.route_close_error.is_none());
    }

    #[test]
    fn shutdown_without_relay_does_not_publish_lazy_identity() {
        let route_name = unique_route_name("shutdown-no-relay");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        let server = test_server("shutdown-no-relay-server");
        register_dummy(&server, &route_name);

        let outcome = session.shutdown(
            Some(&server),
            vec![route_name],
            None,
            false,
            None,
            Duration::from_secs(5),
        );

        assert_eq!(outcome.removed_routes.len(), 1);
        assert_eq!(outcome.route_outcomes.len(), 1);
        assert!(outcome.route_outcomes[0].active_drained);
        assert_eq!(outcome.route_outcomes[0].closed_reason, "shutdown");
        assert!(outcome.relay_errors.is_empty());
        assert!(outcome.runtime_barrier_error.is_none());
        assert_eq!(session.server_id(), None);
        assert_eq!(session.server_address(), None);
    }

    #[test]
    fn host_without_relay_does_not_resolve_unrelated_relay_configuration() {
        let session = Runtime::new(RuntimeOptions {
            server_id: Some(unique_route_name("host-no-relay-config")),
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        session.force_relay_config_error_for_test("relay configuration must remain untouched");

        let host = session
            .host(crate::HostOptions::default().without_relay())
            .expect("local host must not resolve relay configuration");
        drop(host);
    }

    #[test]
    fn shutdown_records_relay_config_resolution_errors() {
        let route_name = unique_route_name("shutdown-relay-config");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        session.force_relay_config_error_for_test("forced relay config failure");
        let server = test_server("shutdown-relay-config-server");
        register_dummy(&server, &route_name);
        let outcome = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        assert_eq!(outcome.removed_routes, vec![route_name.clone()]);
        assert_eq!(outcome.relay_errors.len(), 1);
        assert_eq!(outcome.relay_errors[0].route_name, route_name);
        assert!(
            outcome.relay_errors[0]
                .message
                .contains("forced relay config failure"),
            "unexpected relay error: {:?}",
            outcome.relay_errors[0]
        );
    }

    #[test]
    fn shutdown_records_external_relay_cleanup_config_errors() {
        let route_name = unique_route_name("shutdown-relay-cleanup-config");
        let session = Runtime::new(RuntimeOptions {
            relay_anchor_address: Some("http://127.0.0.1:9".to_string()),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("shutdown-relay-cleanup-config-server");
        register_dummy(&server, &route_name);

        let outcome = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            Some("forced relay proxy config failure".to_string()),
            Duration::from_secs(5),
        );

        assert_eq!(outcome.removed_routes, vec![route_name.clone()]);
        assert_eq!(outcome.relay_errors.len(), 1);
        assert_eq!(outcome.relay_errors[0].route_name, route_name);
        assert!(
            outcome.relay_errors[0]
                .message
                .contains("forced relay proxy config failure"),
            "unexpected relay error: {:?}",
            outcome.relay_errors[0]
        );
    }

    #[test]
    fn shutdown_runtime_construction_failure_reports_error_without_signalling_server() {
        let route_name = unique_route_name("shutdown-runtime-fail");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        let server = test_server("shutdown-runtime-fail-server");
        register_dummy(&server, &route_name);

        let runner = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let rt = Runtime::server_runtime().expect("runtime");
                rt.block_on(server.run())
            })
        };

        let rt = Runtime::server_runtime().expect("runtime");
        rt.block_on(server.wait_until_ready(Duration::from_secs(2)))
            .expect("server ready");

        FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.set(true));
        let outcome = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.set(false));

        assert!(outcome.removed_routes.is_empty());
        assert!(outcome.relay_errors.is_empty());
        assert_eq!(outcome.route_outcomes.len(), 1);
        assert!(!outcome.route_outcomes[0].active_drained);
        assert!(
            outcome
                .route_close_error
                .as_deref()
                .is_some_and(|err| err.contains("failed to create runtime: injected test failure"))
        );
        rt.block_on(server.wait_until_stopped(Duration::from_millis(50)))
            .expect_err("shutdown must not signal server when route close runtime is unavailable");
        assert!(
            rt.block_on(server.contains_route(&route_name)),
            "route must remain registered when shutdown route close cannot run",
        );

        rt.block_on(server.shutdown_and_wait(Duration::from_secs(2)))
            .expect("cleanup shutdown should stop server");
        runner.join().unwrap().unwrap();
    }

    #[test]
    fn shutdown_consumes_recorded_direct_shutdown_route_outcomes() {
        let route_name = unique_route_name("direct-shutdown-record");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        let server = test_server("direct-shutdown-record-server");
        register_dummy(&server, &route_name);

        let runner = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let rt = Runtime::server_runtime().expect("runtime");
                rt.block_on(server.run())
            })
        };

        let rt = Runtime::server_runtime().expect("runtime");
        rt.block_on(server.wait_until_ready(Duration::from_secs(2)))
            .expect("server ready");
        let ack = c2_ipc::shutdown(server.ipc_address(), Duration::from_secs(2))
            .expect("direct IPC shutdown should be acknowledged");
        assert!(ack.acknowledged);
        assert!(ack.shutdown_started);
        assert!(ack.route_outcomes.is_empty());
        rt.block_on(server.wait_until_stopped(Duration::from_secs(2)))
            .expect("server stopped");

        let outcome = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );

        assert_eq!(outcome.removed_routes, vec![route_name.clone()]);
        assert_eq!(outcome.route_outcomes.len(), 1);
        assert_eq!(outcome.route_outcomes[0].route_name, route_name);
        assert_eq!(
            outcome.route_outcomes[0].closed_reason,
            "direct_ipc_shutdown"
        );
        assert!(outcome.route_outcomes[0].active_drained);
        assert!(
            !rt.block_on(server.contains_route(&outcome.route_outcomes[0].route_name)),
            "direct shutdown record should come from native close transaction, not leave route registered",
        );

        runner.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_preserves_failed_external_cleanup_error_and_route_journal() {
        use std::os::unix::fs::FileExt;

        let route_name = unique_route_name("direct-shutdown-cleanup-fail");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("session");
        let server = test_server("direct-shutdown-cleanup-fail-server");
        let route = dummy_route(&server, &route_name);
        let handle = route.route_handle();
        session
            .register_route(&server, route, dummy_route_spec(&route_name), None, false)
            .expect("route should register");
        let runner = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let rt = Runtime::server_runtime().expect("runtime");
                rt.block_on(server.run())
            })
        };
        let rt = Runtime::server_runtime().expect("runtime");
        rt.block_on(server.wait_until_ready(Duration::from_secs(2)))
            .expect("server ready");
        let credential = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match c2_local::inspect_endpoint(server.local_endpoint()) {
                        c2_local::EndpointInspection::Present(credential) => return credential,
                        c2_local::EndpointInspection::IoError(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        other => panic!("cannot capture test endpoint identity: {other:?}"),
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("test endpoint identity acquisition must finish")
        });
        let lease_path =
            std::path::Path::new(server.local_endpoint().os_name()).with_extension("lease");
        let record = std::fs::read(&lease_path).unwrap();
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .unwrap();
        // Keep the real shutdown draining until after its IPC acknowledgement,
        // so endpoint connection validation still sees the intact lease record.
        let active = handle.blocking_acquire(0).expect("active route guard");
        let ack = c2_ipc::shutdown(server.ipc_address(), Duration::from_secs(2))
            .expect("direct shutdown must be acknowledged");
        assert!(ack.acknowledged && ack.shutdown_started);
        assert!(ack.route_outcomes.is_empty(), "ack is initiation only");
        // Damage only this test's pinned lease record, then let actual active
        // work drain before listener retirement detects the failure.
        lease.write_all_at(b"invalid", 0).unwrap();
        drop(active);
        let stopped = rt.block_on(server.wait_until_stopped(Duration::from_secs(2)));
        // Restore before assertions, then reclaim only the captured endpoint
        // identity after the native run loop has released its listener lease.
        lease.write_all_at(&record, 0).unwrap();
        stopped.expect("external shutdown must reach a terminal fence");
        let run_result = runner.join().expect("server thread must join");
        drop(lease);
        let reaped = c2_local::reap_endpoint(server.local_endpoint(), &credential);
        assert!(
            matches!(reaped, c2_local::EndpointReapResult::Reaped),
            "{reaped:?}"
        );
        assert!(
            run_result
                .unwrap_err()
                .to_string()
                .contains("InvalidRecord")
        );
        assert!(matches!(
            server.lifecycle_state(),
            ServerLifecycleState::Failed(_)
        ));
        assert!(!server.is_running());
        assert!(handle.snapshot().closed);
        assert!(!rt.block_on(server.contains_route(&route_name)));

        let outcome = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        assert!(
            outcome
                .runtime_barrier_error
                .as_deref()
                .is_some_and(|error| error.contains("InvalidRecord")),
            "failed listener cleanup must remain observable: {outcome:?}",
        );
        assert!(
            !outcome.server_was_started,
            "Failed is not a running server"
        );
        assert!(outcome.ipc_clients_drained);
        assert!(outcome.route_close_error.is_none());
        assert_eq!(outcome.removed_routes, vec![route_name.clone()]);
        assert_eq!(outcome.route_outcomes.len(), 1);
        let close = &outcome.route_outcomes[0];
        assert_eq!(close.route_name, route_name);
        assert_eq!(close.closed_reason, "direct_ipc_shutdown");
        assert!(close.local_removed && close.active_drained);
        assert!(close.close_error.is_none());
        assert!(matches!(
            server.lifecycle_state(),
            ServerLifecycleState::Failed(_)
        ));
        rt.block_on(server.wait_until_terminal()).unwrap();

        let repeated = session.shutdown(
            Some(&server),
            vec![route_name],
            None,
            false,
            None,
            Duration::from_secs(5),
        );
        assert!(repeated.removed_routes.is_empty());
        assert!(repeated.route_outcomes.is_empty());
        assert_eq!(session.shutdown_observation(&server), Some(outcome));
    }

    // ── Read-only memory-budget snapshots ────────────────────────────────

    #[test]
    fn runtime_memory_stats_observe_without_freezing_and_track_outgoing_domain() {
        let session = Runtime::new(RuntimeOptions {
            server_id: None,
            server_ipc_overrides: None,
            client_ipc_overrides: Some(ClientIpcConfigOverrides {
                shm_backing_budget_bytes: Some(4096),
                file_backing_budget_bytes: Some(0),
                live_reassembly_budget_bytes: Some(2048),
                ..Default::default()
            }),
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: false,
        })
        .expect("runtime");

        // Observing an unused Runtime must not create or freeze a context.
        let stats = session.memory_stats();
        assert!(stats.runtime_outgoing.is_none());
        assert!(stats.server.is_none());
        assert!(!session.client_config_frozen());
        assert!(session.outgoing_memory_observer().is_none());

        // A failed first attempt still freezes the domain limits atomically.
        assert!(
            session
                .acquire_ipc_client("ipc://nonexistent_runtime_memory_domain")
                .is_err(),
            "no server is listening on this address"
        );
        assert!(session.client_config_frozen());

        let stats = session.memory_stats();
        let outgoing = stats
            .runtime_outgoing
            .expect("a frozen domain is observable");
        assert_eq!(outgoing.limits.shm_backing_budget_bytes, 4096);
        assert_eq!(outgoing.limits.file_backing_budget_bytes, 0);
        assert_eq!(outgoing.limits.live_reassembly_budget_bytes, 2048);
        assert_eq!(outgoing.shm.used_bytes, 0);
        assert_eq!(outgoing.file.used_bytes, 0);
        assert_eq!(outgoing.reassembly.used_bytes, 0);
        assert_eq!(outgoing.shm.peak_bytes, 0);
        assert_eq!(outgoing.shm.rejected_allocations, 0);

        // The retired-observation handle carries the same limits and counters
        // without touching the live cache again.
        let observer = session
            .outgoing_memory_observer()
            .expect("the frozen domain is observable");
        assert_eq!(*observer.limits(), outgoing.limits);
        assert_eq!(observer.used_bytes(), 0);
    }

    #[test]
    fn outgoing_domain_observer_outlives_runtime_shutdown_without_touching_others() {
        let runtime_a = Runtime::new(RuntimeOptions {
            server_id: None,
            server_ipc_overrides: None,
            client_ipc_overrides: Some(ClientIpcConfigOverrides {
                shm_backing_budget_bytes: Some(1111),
                ..Default::default()
            }),
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: false,
        })
        .expect("runtime a");
        let runtime_b = Runtime::new(RuntimeOptions {
            server_id: None,
            server_ipc_overrides: None,
            client_ipc_overrides: Some(ClientIpcConfigOverrides {
                shm_backing_budget_bytes: Some(2222),
                ..Default::default()
            }),
            shm_threshold: None,
            remote_payload_chunk_size: None,
            relay_anchor_address: None,
            use_process_relay_anchor: false,
        })
        .expect("runtime b");

        let _ = runtime_a.acquire_ipc_client("ipc://nonexistent_runtime_a");
        let _ = runtime_b.acquire_ipc_client("ipc://nonexistent_runtime_b");
        let a = runtime_a
            .memory_stats()
            .runtime_outgoing
            .expect("runtime a domain");
        let b = runtime_b
            .memory_stats()
            .runtime_outgoing
            .expect("runtime b domain");
        assert_eq!(a.limits.shm_backing_budget_bytes, 1111);
        assert_eq!(b.limits.shm_backing_budget_bytes, 2222);

        // A retired observation taken before shutdown keeps reporting exactly
        // its own domain after the Runtime is gone.
        let retired_a = runtime_a
            .outgoing_memory_observer()
            .expect("runtime a observer");

        // Shutting one Runtime must not reset or change the other's domain.
        let outcome = runtime_a.shutdown_without_host(Duration::from_secs(2));
        assert!(outcome.ipc_clients_drained);
        assert_eq!(*retired_a.limits(), a.limits);
        assert_eq!(retired_a.used_bytes(), 0);
        let b_after = runtime_b
            .memory_stats()
            .runtime_outgoing
            .expect("runtime b domain stays observable");
        assert_eq!(b_after.limits.shm_backing_budget_bytes, 2222);
        assert_eq!(b_after.shm.limit_bytes, 2222);
    }
}
