use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract,
    MAX_CONTRACT_METHODS, MethodAccess, PORTABLE_CONTRACT_SCHEMA, validate_contract_text_field,
    validate_expected_route_contract,
};
use c2_error::{C2Error, ErrorCode};
use c2_server::{
    AccessLevel, ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
    RouteBuildSpec, RouteConcurrencyHandle, SchedulerAcquireError, SchedulerGuard, SchedulerLimits,
    SchedulerSnapshot, Server, ServerIdentity, ServerRuntimeBuilder,
};

use crate::outcome::RuntimeRouteSpec;
use crate::{Error, LifecycleError, RegisterOutcome, Runtime, ShutdownOutcome, UnregisterOutcome};

/// One generated method entry validated against an admitted release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDefinition {
    pub index: u16,
    pub name: String,
    pub access: MethodAccess,
}

/// Opaque-byte service boundary implemented by generated host adapters.
pub trait EncodedService: Send + Sync + 'static {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error>;
}

/// Language-neutral execution policy shared by remote dispatch and any
/// same-process SDK projection of a registered route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ServiceConcurrencyMode {
    Parallel,
    Exclusive,
    #[default]
    ReadParallel,
}

impl ServiceConcurrencyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Exclusive => "exclusive",
            Self::ReadParallel => "read_parallel",
        }
    }
}

impl From<ServiceConcurrencyMode> for ConcurrencyMode {
    fn from(mode: ServiceConcurrencyMode) -> Self {
        match mode {
            ServiceConcurrencyMode::Parallel => Self::Parallel,
            ServiceConcurrencyMode::Exclusive => Self::Exclusive,
            ServiceConcurrencyMode::ReadParallel => Self::ReadParallel,
        }
    }
}

impl From<ConcurrencyMode> for ServiceConcurrencyMode {
    fn from(mode: ConcurrencyMode) -> Self {
        match mode {
            ConcurrencyMode::Parallel => Self::Parallel,
            ConcurrencyMode::Exclusive => Self::Exclusive,
            ConcurrencyMode::ReadParallel => Self::ReadParallel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteConcurrencySnapshot {
    pub mode: ServiceConcurrencyMode,
    pub max_pending: Option<usize>,
    pub max_workers: Option<usize>,
    pub pending: usize,
    pub active_workers: usize,
    pub closed: bool,
    pub is_unconstrained: bool,
}

impl From<SchedulerSnapshot> for RouteConcurrencySnapshot {
    fn from(snapshot: SchedulerSnapshot) -> Self {
        Self {
            mode: snapshot.mode.into(),
            max_pending: snapshot.max_pending,
            max_workers: snapshot.max_workers,
            pending: snapshot.pending,
            active_workers: snapshot.active_workers,
            closed: snapshot.closed,
            is_unconstrained: snapshot.is_unconstrained,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteConcurrencyError {
    Closed,
    Capacity { field: &'static str, limit: usize },
}

impl fmt::Display for RouteConcurrencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("route closed"),
            Self::Capacity { field, limit } => {
                write!(
                    formatter,
                    "route concurrency capacity exceeded: {field}={limit}"
                )
            }
        }
    }
}

impl std::error::Error for RouteConcurrencyError {}

impl From<SchedulerAcquireError> for RouteConcurrencyError {
    fn from(error: SchedulerAcquireError) -> Self {
        match error {
            SchedulerAcquireError::Closed => Self::Closed,
            SchedulerAcquireError::Capacity { field, limit } => Self::Capacity { field, limit },
        }
    }
}

/// Cloneable projection of the exact scheduler used by remote route dispatch.
#[derive(Clone)]
pub struct RouteConcurrency {
    inner: RouteConcurrencyHandle,
}

impl fmt::Debug for RouteConcurrency {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteConcurrency")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl RouteConcurrency {
    pub fn snapshot(&self) -> RouteConcurrencySnapshot {
        self.inner.snapshot().into()
    }

    pub fn blocking_acquire(
        &self,
        method_index: u16,
    ) -> Result<RouteConcurrencyGuard, RouteConcurrencyError> {
        self.inner
            .blocking_acquire(method_index)
            .map(|inner| RouteConcurrencyGuard { _inner: inner })
            .map_err(RouteConcurrencyError::from)
    }

    pub fn is_unconstrained(&self) -> bool {
        self.inner.is_unconstrained()
    }
}

pub struct RouteConcurrencyGuard {
    _inner: SchedulerGuard,
}

impl fmt::Debug for RouteConcurrencyGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteConcurrencyGuard")
            .finish_non_exhaustive()
    }
}

/// A release-verified route definition ready for Core registration.
pub struct ServiceDefinition {
    release_ref: ContractReleaseRef,
    expected: ExpectedRouteContract,
    methods: Arc<[MethodDefinition]>,
    service: Arc<dyn EncodedService>,
    concurrency_mode: ServiceConcurrencyMode,
    max_pending: Option<usize>,
    max_workers: Option<usize>,
}

impl fmt::Debug for ServiceDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceDefinition")
            .field("release_ref", &self.release_ref)
            .field("expected", &self.expected)
            .field("methods", &self.methods)
            .field("concurrency_mode", &self.concurrency_mode)
            .field("max_pending", &self.max_pending)
            .field("max_workers", &self.max_workers)
            .finish_non_exhaustive()
    }
}

impl ServiceDefinition {
    pub fn new<I>(
        release: &ContractRelease,
        release_ref: ContractReleaseRef,
        route_name: impl Into<String>,
        methods: I,
        service: Arc<dyn EncodedService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        release_ref.verify_release(release)?;
        let expected = release.expected_route(route_name)?;
        let methods = methods.into_iter().collect::<Vec<_>>();
        let descriptor_methods = release.descriptor().methods();
        if methods.len() != descriptor_methods.len() {
            return Err(method_contract_error(
                "$.methods",
                format!(
                    "service defines {} methods but release defines {}",
                    methods.len(),
                    descriptor_methods.len()
                ),
            ));
        }
        for (position, (method, descriptor)) in
            methods.iter().zip(descriptor_methods.iter()).enumerate()
        {
            let expected_index = u16::try_from(position).map_err(|_| {
                method_contract_error(
                    "$.methods",
                    "method index exceeds the C-Two wire capacity".to_string(),
                )
            })?;
            if method.index != expected_index {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].index"),
                    format!(
                        "expected positional index {expected_index}, got {}",
                        method.index
                    ),
                ));
            }
            if method.name != descriptor.name() {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].name"),
                    format!(
                        "expected release method {:?}, got {:?}",
                        descriptor.name(),
                        method.name
                    ),
                ));
            }
            if method.access != descriptor.access() {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].access"),
                    format!(
                        "expected release access {:?}, got {:?}",
                        descriptor.access(),
                        method.access
                    ),
                ));
            }
        }

        Ok(Self {
            release_ref,
            expected,
            methods: methods.into(),
            service,
            concurrency_mode: ServiceConcurrencyMode::default(),
            max_pending: None,
            max_workers: None,
        })
    }

    /// Construct an explicitly nonportable language service.
    ///
    /// This path exists for language-local behaviors such as Python pickle. It
    /// requires a release reference whose schema is not the portable
    /// `c-two.contract.v2` schema and therefore cannot masquerade as a portable
    /// generated service.
    #[doc(hidden)]
    pub fn new_nonportable<I>(
        release_ref: ContractReleaseRef,
        expected: ExpectedRouteContract,
        methods: I,
        service: Arc<dyn EncodedService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        validate_expected_route_contract(&expected)?;
        if release_ref.contract_schema() == PORTABLE_CONTRACT_SCHEMA {
            return Err(method_contract_error(
                "$.contract_schema",
                "portable services must be constructed from an admitted ContractRelease"
                    .to_string(),
            ));
        }
        for (path, reference, route) in [
            (
                "$.crm.namespace",
                release_ref.crm_namespace(),
                expected.crm_ns.as_str(),
            ),
            (
                "$.crm.name",
                release_ref.crm_name(),
                expected.crm_name.as_str(),
            ),
            (
                "$.crm.version",
                release_ref.crm_version(),
                expected.crm_ver.as_str(),
            ),
        ] {
            if reference != route {
                return Err(method_contract_error(
                    path,
                    format!(
                        "nonportable release reference value {reference:?} does not match route value {route:?}"
                    ),
                ));
            }
        }

        let methods = methods.into_iter().collect::<Vec<_>>();
        validate_nonportable_methods(&methods)?;
        Ok(Self {
            release_ref,
            expected,
            methods: methods.into(),
            service,
            concurrency_mode: ServiceConcurrencyMode::default(),
            max_pending: None,
            max_workers: None,
        })
    }

    pub fn with_concurrency(
        mut self,
        mode: ServiceConcurrencyMode,
        max_pending: Option<usize>,
        max_workers: Option<usize>,
    ) -> Result<Self, Error> {
        SchedulerLimits::try_from_usize(max_pending, max_workers)
            .map_err(|message| method_contract_error("$.concurrency", message))?;
        self.concurrency_mode = mode;
        self.max_pending = max_pending;
        self.max_workers = max_workers;
        Ok(self)
    }

    pub fn release_ref(&self) -> &ContractReleaseRef {
        &self.release_ref
    }

    pub fn expected_route(&self) -> &ExpectedRouteContract {
        &self.expected
    }

    pub fn methods(&self) -> &[MethodDefinition] {
        &self.methods
    }
}

fn validate_nonportable_methods(methods: &[MethodDefinition]) -> Result<(), Error> {
    if methods.len() > MAX_CONTRACT_METHODS {
        return Err(method_contract_error(
            "$.methods",
            format!(
                "service defines {} methods but the contract cap is {MAX_CONTRACT_METHODS}",
                methods.len()
            ),
        ));
    }
    let mut names = HashSet::new();
    for (position, method) in methods.iter().enumerate() {
        let expected_index = u16::try_from(position).map_err(|_| {
            method_contract_error(
                "$.methods",
                "method index exceeds the C-Two wire capacity".to_string(),
            )
        })?;
        if method.index != expected_index {
            return Err(method_contract_error(
                &format!("$.methods[{position}].index"),
                format!(
                    "expected positional index {expected_index}, got {}",
                    method.index
                ),
            ));
        }
        validate_contract_text_field("method name", &method.name)?;
        if !names.insert(method.name.as_str()) {
            return Err(method_contract_error(
                &format!("$.methods[{position}].name"),
                format!("duplicate method name {:?}", method.name),
            ));
        }
    }
    Ok(())
}

fn method_contract_error(path: &str, message: String) -> Error {
    ContractError::InvalidDescriptor {
        path: path.to_string(),
        message,
    }
    .into()
}

#[derive(Debug, Clone)]
enum RelaySelection {
    Inherit,
    Disabled,
    Explicit(String),
}

/// Core host lifecycle options that do not expose transport implementation
/// types.
#[derive(Debug, Clone)]
pub struct HostOptions {
    relay: RelaySelection,
    relay_use_proxy: Option<bool>,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    lifecycle: c2_config::ServerLifecyclePolicy,
    lifecycle_explicit: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            relay: RelaySelection::Inherit,
            relay_use_proxy: None,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
            lifecycle: c2_config::ServerLifecyclePolicy::Persistent,
            lifecycle_explicit: false,
        }
    }
}

impl HostOptions {
    pub fn with_relay_anchor_address(mut self, address: impl Into<String>) -> Self {
        self.relay = RelaySelection::Explicit(address.into());
        self
    }

    pub fn without_relay(mut self) -> Self {
        self.relay = RelaySelection::Disabled;
        self
    }

    pub fn with_relay_use_proxy(mut self, use_proxy: bool) -> Self {
        self.relay_use_proxy = Some(use_proxy);
        self
    }

    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Select the validated server lifecycle policy.
    ///
    /// `Persistent` (the default) keeps the explicit-shutdown semantics. An
    /// `OwnerBound` policy additionally requires the native owner control
    /// capability to be attached to the Runtime before the host starts.
    pub fn with_lifecycle(mut self, policy: c2_config::ServerLifecyclePolicy) -> Self {
        self.lifecycle = policy;
        self.lifecycle_explicit = true;
        self
    }

    /// Shorthand for the `OwnerBound` policy with one grace window.
    pub fn with_owner_bound(self, owner_missing_grace: Duration) -> Self {
        self.with_lifecycle(c2_config::ServerLifecyclePolicy::OwnerBound {
            owner_missing_grace,
        })
    }

    pub fn lifecycle(&self) -> &c2_config::ServerLifecyclePolicy {
        &self.lifecycle
    }
}

/// Core-owned host for one process server and any number of validated routes.
#[derive(Clone)]
pub struct Host {
    inner: Arc<HostInner>,
}

impl fmt::Debug for Host {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Host")
            .field("server_id", &self.inner.server.server_id())
            .field("ipc_address", &self.inner.server.ipc_address())
            .finish_non_exhaustive()
    }
}

struct HostInner {
    runtime: Runtime,
    server: Arc<Server>,
    relay_anchor_address: Option<String>,
    relay_use_proxy: bool,
    shutdown_timeout: Duration,
    route_names: Mutex<HashSet<String>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown_outcome: Mutex<Option<ShutdownOutcome>>,
    shutdown_gate: Mutex<()>,
    lifecycle: crate::owner_bound::HostLifecycleCell,
    lifecycle_policy: c2_config::ServerLifecyclePolicy,
}

impl HostInner {
    fn refresh_shutdown_outcome(&self) {
        if self.shutdown_outcome.lock().is_none() {
            if let Some(outcome) = self.runtime.shutdown_observation(&self.server) {
                self.lifecycle.finish_from_outcome(&outcome);
                *self.shutdown_outcome.lock() = Some(outcome);
                self.route_names.lock().clear();
            }
        }
    }

    fn shutdown(&self) -> ShutdownOutcome {
        self.shutdown_with_timeout(self.shutdown_timeout)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> ShutdownOutcome {
        let deadline = Instant::now() + timeout;
        self.refresh_shutdown_outcome();
        if let Some(outcome) = self.shutdown_outcome.lock().clone() {
            return outcome;
        }
        let Some(_gate) = self
            .shutdown_gate
            .try_lock_for(deadline.saturating_duration_since(Instant::now()))
        else {
            return ShutdownOutcome {
                runtime_barrier_error: Some("native shutdown transaction is still pending".into()),
                ..Default::default()
            };
        };
        self.run_shutdown_transaction(deadline.saturating_duration_since(Instant::now()))
    }

    fn run_shutdown_transaction(&self, timeout: Duration) -> ShutdownOutcome {
        self.refresh_shutdown_outcome();
        if let Some(outcome) = self.shutdown_outcome.lock().clone() {
            return outcome;
        }
        let route_names = self.route_names.lock().iter().cloned().collect();
        let outcome = self.runtime.shutdown(
            Some(&self.server),
            route_names,
            self.relay_anchor_address.as_deref(),
            self.relay_use_proxy,
            None,
            timeout,
        );
        // Never join a running drain or the current host thread. Dropping a JoinHandle
        // detaches it; its Server/runtime continue owning all actual callback work.
        if self
            .thread
            .lock()
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            if let Some(thread) = self.thread.lock().take() {
                let _ = thread.join();
            }
        }
        if outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none() {
            self.route_names.lock().clear();
            self.lifecycle.finish_from_outcome(&outcome);
            *self.shutdown_outcome.lock() = Some(outcome.clone());
        }
        outcome
    }

    fn finish_owner_shutdown_from_host_thread(&self) {
        let Some(_gate) = self.shutdown_gate.try_lock() else {
            return;
        };
        self.run_shutdown_transaction(self.shutdown_timeout);
    }

    fn lifecycle_snapshot(&self) -> crate::owner_bound::HostLifecycleSnapshot {
        self.refresh_shutdown_outcome();
        let server_state = self.server.lifecycle_state();
        let budget = self.server.memory_budget_snapshot().budget;
        let work_drained = self.shutdown_outcome.lock().as_ref().map(|outcome| {
            outcome
                .route_outcomes
                .iter()
                .all(|close| close.active_drained)
        });
        crate::owner_bound::HostLifecycleSnapshot {
            policy: self.lifecycle_policy,
            phase: self.lifecycle.phase(),
            listener_closed: matches!(
                server_state,
                c2_server::ServerLifecycleState::Stopped
                    | c2_server::ServerLifecycleState::Failed(_)
            ),
            work_drained,
            client_held_leases: crate::owner_bound::HostClientHeldLeases {
                response_shm_bytes: budget.shm.used_bytes,
                response_file_bytes: budget.file.used_bytes,
                reassembly_bytes: budget.reassembly.used_bytes,
            },
        }
    }

    fn unregister(&self, route_name: &str) -> Result<UnregisterOutcome, Error> {
        self.refresh_shutdown_outcome();
        if self.runtime.shutdown_pending(&self.server) {
            return Err(LifecycleError::Server(
                "native shutdown is still draining; route hooks are not yet safe".into(),
            )
            .into());
        }
        if let Some(shutdown) = self.shutdown_outcome.lock().as_ref()
            && let Some(close) = shutdown
                .route_outcomes
                .iter()
                .find(|close| close.route_name == route_name)
        {
            return Ok(UnregisterOutcome {
                route_name: route_name.to_string(),
                local_removed: close.local_removed,
                close: close.clone(),
                relay_error: shutdown
                    .relay_errors
                    .iter()
                    .find(|error| error.route_name == route_name)
                    .cloned(),
            });
        }
        let outcome = self.runtime.unregister_route(
            &self.server,
            route_name,
            self.relay_anchor_address.as_deref(),
            self.relay_use_proxy,
        )?;
        if outcome.local_removed {
            self.route_names.lock().remove(route_name);
        }
        Ok(outcome)
    }
}

impl Drop for HostInner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl Host {
    /// Exact native endpoint retained for bind, readiness, and credentials.
    pub fn local_endpoint(&self) -> &c2_config::LocalEndpoint {
        self.inner.server.local_endpoint()
    }

    pub fn local_endpoint_context(&self) -> &c2_config::LocalEndpointContext {
        self.local_endpoint().context()
    }

    /// Read-only memory statistics for this host and its Runtime.
    ///
    /// Composes the Runtime's outgoing client domain with the Core host's
    /// server direction (response pool, reassembly pool, and response
    /// prewarm). Observation never connects, maps memory, freezes
    /// configuration, or resets accounting; charges retained by outstanding
    /// response or held data stay visible until their backing is released.
    pub fn memory_stats(&self) -> crate::RuntimeMemoryStats {
        crate::RuntimeMemoryStats {
            runtime_outgoing: self.inner.runtime.outgoing_memory_stats(),
            server: Some(crate::MemoryScopeStats::from_server(
                self.inner.server.memory_budget_snapshot(),
            )),
        }
    }

    /// Read-only observer of this host's server-direction budget.
    ///
    /// The handle shares only the server accounting counters and resolved
    /// limits, so a stopped host keeps retained response/reassembly charges
    /// observable without retaining the Server, its pools, or its callbacks.
    pub fn server_memory_observer(&self) -> c2_mem::BudgetObserver {
        self.inner.server.memory_budget_observer()
    }

    pub fn register(&self, definition: ServiceDefinition) -> Result<Registration, Error> {
        let route_name = definition.expected.route_name.clone();
        let (route_spec, runtime_spec) = route_specs(&definition)?;
        let callback: Arc<dyn CrmCallback> = Arc::new(CoreCallback {
            service: Arc::clone(&definition.service),
        });
        let route = self
            .inner
            .server
            .build_route(route_spec, callback)
            .map_err(|error| LifecycleError::Server(error.to_string()))?;
        let route_concurrency = RouteConcurrency {
            inner: route.route_handle(),
        };
        let outcome = self.inner.runtime.register_route(
            &self.inner.server,
            route,
            runtime_spec,
            self.inner.relay_anchor_address.as_deref(),
            self.inner.relay_use_proxy,
        )?;
        self.inner.route_names.lock().insert(route_name.clone());
        Ok(Registration {
            host: Arc::clone(&self.inner),
            route_name,
            register_outcome: outcome,
            close_outcome: None,
            route_concurrency,
        })
    }

    pub fn shutdown(&self) -> ShutdownOutcome {
        self.inner.shutdown()
    }

    /// Bound this caller's observation without changing or cancelling the native drain.
    pub fn shutdown_with_timeout(&self, timeout: Duration) -> ShutdownOutcome {
        self.inner.shutdown_with_timeout(timeout)
    }

    pub fn is_running(&self) -> bool {
        self.inner.server.is_running()
    }

    /// Observe the shared terminal native journal without initiating or consuming shutdown.
    pub fn shutdown_outcome(&self) -> Option<ShutdownOutcome> {
        self.inner.refresh_shutdown_outcome();
        self.inner.shutdown_outcome.lock().clone()
    }

    /// Read-only lifecycle observation for this host.
    ///
    /// The snapshot exposes the policy and phase plus the separately
    /// observable listener, drained-work, and client-held lease facts. It
    /// never exposes OS handles or the owner control capability, and it does
    /// not report endpoint (socket/lock) cleanup results.
    pub fn lifecycle_snapshot(&self) -> crate::owner_bound::HostLifecycleSnapshot {
        self.inner.lifecycle_snapshot()
    }
}

/// One successful route registration with idempotent explicit and Drop
/// cleanup.
pub struct Registration {
    host: Arc<HostInner>,
    route_name: String,
    register_outcome: RegisterOutcome,
    close_outcome: Option<UnregisterOutcome>,
    route_concurrency: RouteConcurrency,
}

impl fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("route_name", &self.route_name)
            .field("register_outcome", &self.register_outcome)
            .field("closed", &self.close_outcome.is_some())
            .finish()
    }
}

impl Registration {
    pub fn route_name(&self) -> &str {
        &self.route_name
    }

    pub fn outcome(&self) -> &RegisterOutcome {
        &self.register_outcome
    }

    pub fn is_closed(&self) -> bool {
        self.close_outcome.is_some()
    }

    pub fn route_concurrency(&self) -> RouteConcurrency {
        self.route_concurrency.clone()
    }

    pub fn close(&mut self) -> Result<UnregisterOutcome, Error> {
        if let Some(outcome) = &self.close_outcome {
            return Ok(outcome.clone());
        }
        let outcome = self.host.unregister(&self.route_name)?;
        self.close_outcome = Some(outcome.clone());
        Ok(outcome)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Runtime {
    pub fn host(&self, options: HostOptions) -> Result<Host, Error> {
        options
            .lifecycle
            .validate()
            .map_err(|message| LifecycleError::Configuration(message))?;
        let policy = if options.lifecycle_explicit {
            options.lifecycle
        } else {
            self.lifecycle_policy()
        };
        self.freeze_lifecycle_policy(policy)?;
        let options = options.with_lifecycle(policy);
        let identity = self.ensure_server()?;
        let config = self.server_ipc_config()?;
        let relay_anchor_address = match options.relay {
            RelaySelection::Inherit => self.effective_relay_anchor_address()?,
            RelaySelection::Disabled => None,
            RelaySelection::Explicit(address) => Some(address),
        };
        let relay_use_proxy = match options.relay_use_proxy {
            Some(use_proxy) => use_proxy,
            None if relay_anchor_address.is_some() => self.relay_use_proxy()?,
            None => false,
        };
        let server = Arc::new(
            Server::new_with_identity_and_endpoint(
                self.freeze_local_endpoint_context()?
                    .endpoint(&identity.ipc_address)
                    .map_err(|error| LifecycleError::Configuration(error.to_string()))?,
                config.clone(),
                ServerIdentity {
                    server_id: identity.server_id,
                    server_instance_id: identity.server_instance_id,
                },
            )
            .map_err(|error| LifecycleError::Server(error.to_string()))?,
        );
        let server_runtime = ServerRuntimeBuilder::build(&config)
            .map_err(|error| LifecycleError::Server(error.to_string()))?;
        server
            .begin_start_attempt()
            .map_err(|error| LifecycleError::Server(error.to_string()))?;

        // Consume the native owner control capability before this host can
        // publish readiness. Naming the OwnerBound policy alone fails here,
        // and the capability leaves the Runtime state so it can never be
        // cloned, reattached, or logged with the options.
        let owner_receiver = match options.lifecycle {
            c2_config::ServerLifecyclePolicy::Persistent => None,
            c2_config::ServerLifecyclePolicy::OwnerBound { .. } => {
                Some(self.consume_owner_control()?)
            }
        };
        let lifecycle = crate::owner_bound::HostLifecycleCell::new(&options.lifecycle);
        let inner = Arc::new(HostInner {
            runtime: self.clone(),
            server: Arc::clone(&server),
            relay_anchor_address,
            relay_use_proxy,
            shutdown_timeout: options.shutdown_timeout,
            route_names: Mutex::new(HashSet::new()),
            thread: Mutex::new(None),
            shutdown_outcome: Mutex::new(None),
            shutdown_gate: Mutex::new(()),
            lifecycle: lifecycle.clone(),
            lifecycle_policy: options.lifecycle,
        });
        // The host thread observes the inner state only through a Weak
        // reference: a live watcher must never keep a dropped Host/Runtime
        // alive, and the thread always ends within bounded shutdown waits.
        let weak_inner = Arc::downgrade(&inner);
        let run_server = Arc::clone(&server);
        let watch = lifecycle;
        let owner_grace = options.lifecycle.owner_missing_grace();
        let owner_startup_timeout = options.startup_timeout;
        let thread = std::thread::Builder::new()
            .name(format!("c2-host-{}", server.server_id()))
            .spawn(move || {
                let Some(mut receiver) = owner_receiver else {
                    // Persistent policy: the accept loop alone decides
                    // nothing about lifecycle; explicit shutdown does.
                    let result = server_runtime.block_on(run_server.run());
                    if result.is_err() {
                        run_server.finalize_runtime_stopped();
                    }
                    return;
                };
                enum ThreadOutcome {
                    ServerFinished(Result<(), c2_server::ServerError>),
                    PreReadyRefused,
                }
                let outcome = server_runtime.block_on(async {
                    // Arm the owner watcher before the accept loop runs, so
                    // readiness can only be published behind a live watcher
                    // and an already-gone capability is refused pre-ready.
                    match crate::owner_bound::arm_owner_watch_with_fence(crate::owner_bound::arm_owner_watch(&mut receiver), &run_server, owner_startup_timeout).await {
                        crate::owner_bound::OwnerArmOutcome::Alive => {}
                        crate::owner_bound::OwnerArmOutcome::Closed => {
                            receiver.shutdown();
                            let _ = run_server.reject_start_attempt(
                                "owner control capability was already closed before the host became ready"
                                    .to_string(),
                            );
                            return ThreadOutcome::PreReadyRefused;
                        }
                        crate::owner_bound::OwnerArmOutcome::IoError(message) => {
                            receiver.shutdown();
                            let _ = run_server.reject_start_attempt(format!(
                                "owner control watcher failed before the host became ready: {message}"
                            ));
                            return ThreadOutcome::PreReadyRefused;
                        }
                    }
                    let run = run_server.run();
                    tokio::pin!(run);
                    tokio::select! {
                        result = &mut run => ThreadOutcome::ServerFinished(result),
                        _ = run_server.wait_for_shutdown_requested() => {
                            receiver.shutdown();
                            ThreadOutcome::ServerFinished(run.await)
                        },
                        _ = crate::owner_bound::supervise_owner(
                            &mut receiver, &run_server, &watch,
                            owner_grace.expect("OwnerBound policy carries a grace window"),
                        ) => {
                            // Owner supervision only initiates drain. Never drop the run future
                            // after a caller timeout; it owns the route journal and callbacks.
                            receiver.shutdown();
                            ThreadOutcome::ServerFinished(run.await)
                        }
                    }

                });
                match outcome {
                    ThreadOutcome::ServerFinished(result) => {
                        if result.is_err() { run_server.finalize_runtime_stopped(); }
                        // Reconcile every cause of run termination, including an explicit stop
                        // winning the select. The watcher/grace has already been cancelled.
                        if let Some(inner) = weak_inner.upgrade() {
                            inner.finish_owner_shutdown_from_host_thread();
                        }
                    }
                    ThreadOutcome::PreReadyRefused => {}

                }
            })
            .map_err(|error| LifecycleError::Server(error.to_string()))?;
        *inner.thread.lock() = Some(thread);
        let wait_runtime = Runtime::server_runtime()?;
        if let Err(error) =
            wait_runtime.block_on(server.wait_until_responsive(options.startup_timeout))
        {
            let _ = wait_runtime.block_on(server.shutdown_and_wait(options.shutdown_timeout));
            if let Some(thread) = inner.thread.lock().take() {
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
            return Err(LifecycleError::Server(error.to_string()).into());
        }

        Ok(Host { inner })
    }
}

fn route_specs(
    definition: &ServiceDefinition,
) -> Result<(RouteBuildSpec, RuntimeRouteSpec), Error> {
    let method_names = definition
        .methods
        .iter()
        .map(|method| method.name.to_string())
        .collect::<Vec<_>>();
    let access_map = definition
        .methods
        .iter()
        .map(|method| {
            let access = match method.access {
                MethodAccess::Read => AccessLevel::Read,
                MethodAccess::Write => AccessLevel::Write,
            };
            (method.index, access)
        })
        .collect::<HashMap<_, _>>();
    let route = &definition.expected;
    let limits = SchedulerLimits::try_from_usize(definition.max_pending, definition.max_workers)
        .map_err(|message| method_contract_error("$.concurrency", message))?;
    let route_spec = RouteBuildSpec {
        name: route.route_name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
        method_names: method_names.clone(),
        access_map: access_map.clone(),
        concurrency_mode: definition.concurrency_mode.into(),
        limits,
    };
    let runtime_spec = RuntimeRouteSpec {
        name: route.route_name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
        method_names,
        access_map,
        concurrency_mode: definition.concurrency_mode.into(),
        max_pending: limits.max_pending.map(usize::from),
        max_workers: limits.max_workers.map(usize::from),
    };
    Ok((route_spec, runtime_spec))
}

struct CoreCallback {
    service: Arc<dyn EncodedService>,
}

impl CrmCallback for CoreCallback {
    fn invoke(
        &self,
        _route_name: &str,
        method_idx: u16,
        request: RequestData,
        _response_pool: Arc<parking_lot::RwLock<c2_mem::MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        let mut lease = RequestLease::new(request);
        let bytes = match lease.copy_bytes() {
            Ok(bytes) => bytes,
            Err(copy_error) => {
                let release_error = lease.release().err();
                return Err(CrmError::UserError(
                    protocol_violation(
                        "request transport bytes could not be copied",
                        copy_error,
                        release_error,
                    )
                    .to_wire_bytes(),
                ));
            }
        };
        let result = self.service.invoke(method_idx, &bytes);
        if let Err(release_error) = lease.release() {
            return Err(CrmError::UserError(
                protocol_violation(
                    "request transport lease could not be released",
                    release_error,
                    None,
                )
                .to_wire_bytes(),
            ));
        }
        result
            .map(ResponseMeta::Inline)
            .map_err(|error| CrmError::UserError(error.to_wire_bytes()))
    }
}

fn protocol_violation(message: &str, cause: String, release_error: Option<String>) -> C2Error {
    let mut details = std::collections::BTreeMap::from([
        ("cause".to_string(), cause),
        ("protocol".to_string(), "c-two.error.v1".to_string()),
        ("transport".to_string(), "ipc".to_string()),
    ]);
    if let Some(release_error) = release_error {
        details.insert("release_error".to_string(), release_error);
    }
    C2Error::new(ErrorCode::ProtocolViolation, message).with_details(details)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn live_owner_watcher_does_not_keep_a_dropped_host_alive() {
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let (_owner, receiver) = c2_local::owner_control_pair().unwrap();
        runtime.attach_owner_control(receiver).unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_shutdown_timeout(Duration::from_millis(100))
                    .with_owner_bound(Duration::from_secs(30)),
            )
            .unwrap();
        let weak = Arc::downgrade(&host.inner);
        let started = Instant::now();
        drop(host);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            weak.upgrade().is_none(),
            "pending watcher must not own HostInner"
        );
    }
    #[test]
    fn real_relay_idle_eviction_does_not_stop_persistent_host() {
        use crate::{Connect, EncodedClient};
        use c2_http::relay::{RelayConfig, RelayServer};
        struct Echo;
        impl EncodedService for Echo {
            fn invoke(&self, _: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
                Ok(request.to_vec())
            }
        }
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let url = format!("http://{address}");
        let mut relay = RelayServer::start(RelayConfig {
            bind: address.to_string(),
            advertise_url: url.clone(),
            idle_timeout_secs: 1,
            anti_entropy_interval: Duration::ZERO,
            heartbeat_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .with_relay_anchor_address(&url)
                    .with_relay_use_proxy(false),
            )
            .unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let route = format!("relay-idle-{}", uuid::Uuid::new_v4().simple());
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().to_string(),
                access: method.access(),
            })
            .collect::<Vec<_>>();
        let _registration = host
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods,
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        let client_runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let client = client_runtime
            .connect(
                release.expected_route(&route).unwrap(),
                Connect::ExplicitRelay {
                    relay_url: url.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            client.call_owned("echo", b"before-idle").unwrap(),
            b"before-idle"
        );
        let business_and_control_connections = host.inner.server.active_connection_count();
        assert!(business_and_control_connections > 0);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(8);
        while host.inner.server.active_connection_count() >= business_and_control_connections {
            assert!(
                Instant::now() < deadline,
                "real relay upstream must be idle-evicted"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(host.is_running());
        assert_eq!(
            host.lifecycle_snapshot().phase,
            crate::HostLifecyclePhase::Persistent
        );
        let client = client_runtime
            .connect(
                release.expected_route(&route).unwrap(),
                Connect::ExplicitRelay { relay_url: url },
            )
            .unwrap();
        assert_eq!(
            client.call_owned("echo", b"after-idle").unwrap(),
            b"after-idle"
        );
        host.shutdown();
        relay.stop().unwrap();
    }
    #[test]
    fn stopped_listener_with_pending_relay_withdraw_blocks_restart_and_identity_reset() {
        use std::io::{Read, Write};
        struct Echo;
        impl EncodedService for Echo {
            fn invoke(&self, _: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
                Ok(request.to_vec())
            }
        }
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_shutdown_timeout(Duration::from_millis(100)),
            )
            .unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let route = format!("withdraw-fence-{}", uuid::Uuid::new_v4().simple());
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().to_string(),
                access: method.access(),
            })
            .collect::<Vec<_>>();
        let registration = host
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods.clone(),
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("/_unregister"));
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
        });
        let incomplete = runtime.shutdown(
            Some(&host.inner.server),
            vec![route.clone()],
            Some(&relay_url),
            false,
            None,
            Duration::from_millis(100),
        );
        assert!(incomplete.runtime_barrier_error.is_some());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !host.is_running(),
            "listener must already be terminal while relay teardown waits"
        );
        let restart = runtime
            .host(HostOptions::default().without_relay())
            .unwrap_err();
        assert!(restart.to_string().contains("pending native teardown"));
        assert!(runtime.clear_server_identity().is_err());
        assert!(
            runtime
                .set_server_options(Some("forbidden-new-identity".into()), None)
                .is_err()
        );
        release_tx.send(()).unwrap();
        peer.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while host.shutdown_outcome().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.shutdown().relay_errors.is_empty());
        drop(registration);
        drop(host);
        let next = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let _next_registration = next
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods,
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(next.is_running());
        assert!(next.shutdown_outcome().is_none());
        assert_eq!(next.shutdown().removed_routes, vec![route]);
    }
}
