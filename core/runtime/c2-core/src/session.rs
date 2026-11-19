//! Rust-owned process runtime session.
//!
//! The session owns process identity, direct IPC client configuration state,
//! and route registration transactions. Python SDKs provide language-specific
//! callbacks and local direct-call bindings, but must not duplicate the runtime
//! authority implemented here.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use c2_config::ConnectDeadline;
use c2_contract::ExpectedRouteContract;
use c2_http::client::{
    HttpError, RelayAwareClientConfig, RelayAwareHttpClient, RelayControlClient,
    RelayLocalIpcCandidate, RelayRegistration, RelayRegistrationScope, RelayResolvedTarget,
};
use c2_server::{BuiltRoute, ServerLifecycleState, ServerRouteCloseOutcome};

use crate::call_execution::CallExecutionContext;
use crate::memory::role_call_stats;
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
    static FORCE_SHUTDOWN_SPAWN_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn unlimited_lifecycle_error(error: crate::Error) -> LifecycleError {
    match error {
        crate::Error::Lifecycle(error) => error,
        _ => unreachable!("unlimited metadata projection cannot expire or dispatch"),
    }
}

fn connect_stage_check(deadline: ConnectDeadline, stage: &'static str) -> Result<(), crate::Error> {
    deadline
        .check(stage)
        .map_err(|_| crate::connect_deadline_error(stage))
}

fn connect_state_lock<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: ConnectDeadline,
    stage: &'static str,
) -> Result<parking_lot::MutexGuard<'a, T>, crate::Error> {
    crate::connect_deadline::lock(mutex, deadline, stage)
}

fn connect_error_to_ipc(error: crate::Error) -> c2_ipc::IpcError {
    match error {
        crate::Error::Semantic(error) => c2_ipc::IpcError::LocalCallRejected(error),
        crate::Error::Lifecycle(LifecycleError::RuntimeRetired) => c2_ipc::IpcError::PoolRetired,
        other => c2_ipc::IpcError::Config(other.to_string()),
    }
}

fn shutdown_lock<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: Instant,
    stage: &'static str,
) -> Result<parking_lot::MutexGuard<'a, T>, LifecycleError> {
    mutex
        .try_lock_until(deadline)
        .ok_or(LifecycleError::ShutdownWaitTimeout { stage })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub server_id: String,
    pub server_instance_id: String,
    pub ipc_address: String,
}

/// Client-role configuration only: every Host identity, carrying-capacity
/// and server-domain setting belongs to [`crate::HostOptions`].
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// Client-domain capacity code overrides, applied before the domain's
    /// first freeze.
    pub client_limits: c2_config::DomainLimitsOverrides,
    pub local_endpoint: c2_config::LocalEndpointOptions,
    pub relay_anchor_address: Option<String>,
    pub use_process_relay_anchor: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            client_limits: Default::default(),
            local_endpoint: Default::default(),
            relay_anchor_address: None,
            use_process_relay_anchor: true,
        }
    }
}

/// The current Host and its startup or shutdown transaction are owned by the
/// Runtime. Weak running handles let the last Host owner initiate normal drain.
#[derive(Default)]
enum HostSlot {
    #[default]
    Vacant,
    Starting(Arc<HostStartup>),
    Running {
        inner: Weak<crate::host::HostInner>,
        server: Weak<c2_server::Server>,
    },
}

type HostStartOutcome = (
    Result<crate::Host, LifecycleError>,
    Option<FailedStartShutdownReceipt>,
);

#[derive(Default)]
pub(crate) struct HostStartup {
    result: Mutex<Option<HostStartOutcome>>,
    completed: Condvar,
}

impl HostStartup {
    pub(crate) fn wait(&self) -> HostStartOutcome {
        let mut result = self.result.lock();
        while result.is_none() {
            self.completed.wait(&mut result);
        }
        result
            .as_ref()
            .expect("completed start owns an outcome")
            .clone()
    }
}

pub(crate) enum HostAdmission {
    Shared(crate::Host),
    Starting(Arc<HostStartup>),
    Granted(HostStart, Arc<HostStartup>),
}

pub(crate) struct HostStart {
    pub identity: RuntimeIdentity,
    pub ipc: c2_config::ServerIpcConfig,
    pub budget: c2_mem::DomainBudget,
    pub startup_timeout: Duration,
    pub settings: crate::roles::HostSettings,
    pub endpoint_catalog: Option<(u64, c2_config::EnvCatalog)>,
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

fn http_error_parts(err: HttpError) -> (Option<u16>, String, Option<c2_error::C2Error>) {
    let message = err.to_string();
    match err {
        HttpError::ServerError(status_code, body) => {
            let semantic_error = parse_control_semantic_error(&body);
            (Some(status_code), body, semantic_error)
        }
        HttpError::LocalCallRejected(error) => (None, message, Some(error)),
        _ => (None, message, None),
    }
}

fn parse_control_semantic_error(body: &str) -> Option<c2_error::C2Error> {
    if let Ok(error) = c2_error::C2Error::from_json_bytes(body.as_bytes()) {
        return Some(error);
    }
    let trimmed = body.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        // Plain-text control responses keep semantic_error = None.
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(map))
            if !map.contains_key("version") && !map.contains_key("code") =>
        {
            // Non-C2E1 control payloads such as LocalEndpointNamespaceMismatch
            // preserve their control message and HTTP status without fabricating 703 or 713.
            None
        }
        _ => Some(crate::error::semantic_error_from_http_body(body)),
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
    runtime: Weak<Mutex<RuntimeState>>,
    server: Weak<c2_server::Server>,
    shutdown_timeout: Duration,
    outcome: Mutex<ShutdownProgress>,
    completed: Condvar,
}

#[derive(Default)]
struct ShutdownProgress {
    outcome: Option<ShutdownOutcome>,
    retirement: Option<RetirementExecution>,
}

#[cfg(test)]
struct ShutdownPublicationGate {
    entered: std::sync::mpsc::SyncSender<()>,
    proceed: Mutex<std::sync::mpsc::Receiver<()>>,
}

struct RetirementExecution {
    executor: tokio::runtime::Runtime,
    seed: Option<c2_ipc::pool::ClientCloseSeed>,
    shutdown_timeout: Duration,
    // Kept until after result publication so Host Drop cannot wait on this driver.
    _host: Option<crate::Host>,
}

pub(crate) struct ShutdownWork {
    pub(crate) server: Option<Arc<c2_server::Server>>,
    pub(crate) route_names: Vec<String>,
    pub(crate) relay_anchor_address: Option<String>,
    pub(crate) relay_use_proxy: bool,
    pub(crate) shutdown_timeout: Duration,
    relay_cleanup_config_error: Option<String>,
    scopes: HashMap<String, RelayRegistrationScope>,
}

impl ShutdownWork {
    pub(crate) fn new(server: Option<Arc<c2_server::Server>>, shutdown_timeout: Duration) -> Self {
        Self {
            server,
            route_names: Vec::new(),
            relay_anchor_address: None,
            relay_use_proxy: false,
            shutdown_timeout,
            relay_cleanup_config_error: None,
            scopes: HashMap::new(),
        }
    }
}

impl ShutdownTransaction {
    fn new(
        runtime: &Runtime,
        server: Weak<c2_server::Server>,
        shutdown_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: Arc::downgrade(&runtime.state),
            server,
            shutdown_timeout,
            outcome: Mutex::new(ShutdownProgress::default()),
            completed: Condvar::new(),
        })
    }

    fn observe_until(&self, deadline: Instant) -> Option<ShutdownOutcome> {
        let mut outcome = self.outcome.try_lock_until(deadline)?;
        while outcome.outcome.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            self.completed.wait_for(&mut outcome, remaining);
        }
        outcome.outcome.clone()
    }

    fn observation(&self, deadline: Instant) -> (ShutdownOutcome, bool) {
        match self.observe_until(deadline) {
            Some(outcome) => (outcome, true),
            None => (
                ShutdownOutcome {
                    server_was_started: self.server.upgrade().is_some_and(|s| s.is_running()),
                    runtime_barrier_error: Some(
                        "native shutdown transaction is still draining after the caller deadline"
                            .into(),
                    ),
                    ..Default::default()
                },
                false,
            ),
        }
    }
}

/// One explicitly selected target belonging to this Runtime. ClientsOnly covers
/// the outgoing client scope; it does not discover independently owned Hosts.
#[derive(Debug)]
pub enum RuntimeShutdownTarget<'a> {
    Host(&'a crate::Host),
    FailedStart(&'a FailedStartShutdownReceipt),
    ClientsOnly,
}

/// Non-consuming observation of this Runtime's permanent retirement.
#[derive(Clone)]
pub struct RuntimeShutdownReceipt {
    transaction: Arc<ShutdownTransaction>,
}

impl fmt::Debug for RuntimeShutdownReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeShutdownReceipt")
            .finish_non_exhaustive()
    }
}

impl RuntimeShutdownReceipt {
    pub fn observe_until(&self, deadline: Instant) -> (ShutdownOutcome, bool) {
        self.transaction.observation(deadline)
    }

    pub fn terminal_outcome(&self) -> Option<ShutdownOutcome> {
        self.transaction.outcome.lock().outcome.clone()
    }
}

/// Read-only observation of the native cleanup transaction for one failed
/// Host start. It retains the transaction, whose Server reference is weak.
#[derive(Clone)]
pub struct FailedStartShutdownReceipt {
    transaction: Arc<ShutdownTransaction>,
}

impl fmt::Debug for FailedStartShutdownReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedStartShutdownReceipt")
            .finish_non_exhaustive()
    }
}

impl FailedStartShutdownReceipt {
    /// Wait within this caller's budget. Completion is separate from cleanup success.
    pub fn observe_with_timeout(&self, timeout: Duration) -> (ShutdownOutcome, bool) {
        match c2_config::deadline_after(Instant::now(), timeout) {
            Ok(deadline) => self.transaction.observation(deadline),
            Err(error) => (ShutdownOutcome::invalid_timeout(error), false),
        }
    }

    /// Return the original terminal outcome, including cleanup errors.
    pub fn terminal_outcome(&self) -> Option<ShutdownOutcome> {
        self.transaction.outcome.lock().outcome.clone()
    }

    #[cfg(test)]
    pub(crate) fn same_transaction_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.transaction, &other.transaction)
    }

    #[cfg(test)]
    pub(crate) fn set_terminal_error_for_test(&self, message: &str) {
        self.transaction
            .outcome
            .lock()
            .outcome
            .as_mut()
            .expect("test transaction is terminal")
            .runtime_barrier_error = Some(message.to_string());
    }
}

struct RuntimeState {
    connection_event_source: c2_local::ConnectionEventSource,
    /// The lazily frozen client role domain: frozen by the first valid remote
    /// business connection or call, never by Host startup or relay control.
    client_domain: crate::roles::RoleDomain,
    /// The lazily frozen server role domain: frozen by the first Host start;
    /// a Host restart reuses its budget with real in-flight accounting intact.
    server_role: crate::roles::ServerRole,
    host_slot: HostSlot,
    local_endpoint_options: c2_config::LocalEndpointOptions,
    local_endpoint_sources: Option<c2_config::ConfigSources>,
    local_endpoint_revision: u64,
    frozen_local_endpoint: Option<c2_config::LocalEndpointContext>,
    /// Accepted domain inherited from a used session, still configurable until I/O.
    inherited_local_endpoint: Option<c2_config::LocalEndpointContext>,
    /// Outgoing IPC cache in one immutable endpoint domain, charging the
    /// client role budget. Created on first acquire so HTTP-only Runtimes need
    /// no local platform scope. Clones share it; distinct Runtimes are
    /// isolated, and drains never replace the pool.
    client_pool: Option<Arc<c2_ipc::ClientPool>>,
    identity: Option<RuntimeIdentity>,
    relay_anchor_address_override: Option<String>,
    use_process_relay_anchor: bool,
    relay_projection: Option<RelayProjection>,
    path_counters: PathCounters,
    /// Retired role accounting adopted from earlier sessions of a rotation.
    retired: crate::memory::RetiredRoles,
    /// Local hold metadata of this session.
    holds: Arc<crate::memory::LocalCopyTracker>,
    shutdown_transactions: HashMap<String, Arc<ShutdownTransaction>>,
    retirement: Option<Arc<ShutdownTransaction>>,
    registration_scopes: HashMap<String, HashMap<String, RelayRegistrationScope>>,
    #[cfg(test)]
    forced_relay_config_error: Option<String>,
    #[cfg(test)]
    shutdown_publication_gate: Option<Arc<ShutdownPublicationGate>>,
}

#[derive(Clone)]
struct RelayProjection {
    relay_anchor_address: String,
    relay_use_proxy: bool,
    control: Arc<RelayControlClient>,
    local_endpoint_context: Option<c2_config::LocalEndpointContext>,
}

pub(crate) enum RelayResolvedConnection {
    Ipc {
        client: RelayAwareHttpClient,
        candidate: RelayLocalIpcCandidate,
        context: c2_config::LocalEndpointContext,
    },
    Http {
        client: RelayAwareHttpClient,
    },
}

fn pending_host_teardown() -> LifecycleError {
    LifecycleError::Configuration("pending native teardown must finish before starting a new Host or resetting server identity".into())
}

impl RuntimeState {
    fn ensure_active(&self) -> Result<(), LifecycleError> {
        if self.retirement.is_some() {
            return Err(LifecycleError::RuntimeRetired);
        }
        Ok(())
    }

    fn has_pending_shutdown(&self) -> bool {
        self.shutdown_transactions
            .values()
            .any(|transaction| transaction.outcome.lock().outcome.is_none())
    }

    fn ensure_no_pending_teardown(&self) -> Result<(), LifecycleError> {
        self.ensure_active()?;
        if self.has_pending_shutdown() {
            return Err(pending_host_teardown());
        }
        Ok(())
    }
}

impl Drop for RuntimeState {
    fn drop(&mut self) {
        // Observers retain accounting metadata alone. Closing on last session
        // owner drop does not refund surviving real input/preparation owners.
        // The last session owner permanently retires both role domains.
        self.client_domain.close_operations();
        self.server_role.close_operations();
    }
}

impl Runtime {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn ensure_active(&self) -> Result<(), LifecycleError> {
        self.state.lock().ensure_active()
    }

    pub fn ensure_active_with_attempt(
        &self,
        attempt: &crate::ConnectAttempt,
    ) -> Result<(), crate::Error> {
        self.ensure_active_with_deadline(attempt.deadline())
    }

    pub(crate) fn ensure_active_with_deadline(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<(), crate::Error> {
        connect_state_lock(&self.state, deadline, "runtime_admission_wait")?.ensure_active()?;
        Ok(())
    }

    pub fn retirement_receipt(
        &self,
        deadline: Instant,
    ) -> Result<Option<RuntimeShutdownReceipt>, LifecycleError> {
        Ok(
            shutdown_lock(&self.state, deadline, "core_retirement_wait")?
                .retirement
                .clone()
                .map(|transaction| RuntimeShutdownReceipt { transaction }),
        )
    }

    /// Permanently close this Runtime's admission and hand its selected scope
    /// to one native driver. The deadline bounds lock acquisition and receipt
    /// observation, never the driver's cleanup budget.
    pub fn begin_session_retirement(
        &self,
        target: RuntimeShutdownTarget<'_>,
        deadline: Instant,
    ) -> Result<RuntimeShutdownReceipt, LifecycleError> {
        let owned = match &target {
            RuntimeShutdownTarget::Host(host) => host.belongs_to(self),
            RuntimeShutdownTarget::FailedStart(receipt) => {
                Weak::ptr_eq(&receipt.transaction.runtime, &Arc::downgrade(&self.state))
            }
            RuntimeShutdownTarget::ClientsOnly => true,
        };
        if !owned {
            return Err(LifecycleError::Configuration(
                "shutdown target belongs to another Runtime".into(),
            )
            .into());
        }
        let required_server = match &target {
            RuntimeShutdownTarget::Host(host) => Some(host.retirement_server()),
            RuntimeShutdownTarget::FailedStart(receipt) => Some(receipt.transaction.server.clone()),
            RuntimeShutdownTarget::ClientsOnly => None,
        };
        let observe_existing = |transaction: &Arc<ShutdownTransaction>| {
            if required_server
                .as_ref()
                .is_some_and(|server| !Weak::ptr_eq(server, &transaction.server))
            {
                return Err(LifecycleError::Configuration(
                    "shutdown target is outside the existing retirement scope".into(),
                )
                .into());
            }
            Ok(RuntimeShutdownReceipt {
                transaction: transaction.clone(),
            })
        };
        {
            let state = shutdown_lock(&self.state, deadline, "core_retirement_wait")?;
            if let Some(transaction) = &state.retirement {
                return observe_existing(transaction);
            }
        }
        let (mut work, selected, host) = match target {
            RuntimeShutdownTarget::Host(host) => {
                (host.retirement_work(deadline)?, None, Some(host.clone()))
            }
            RuntimeShutdownTarget::FailedStart(receipt) => (
                ShutdownWork::new(
                    receipt.transaction.server.upgrade(),
                    receipt.transaction.shutdown_timeout,
                ),
                Some(receipt.transaction.clone()),
                None,
            ),
            RuntimeShutdownTarget::ClientsOnly => {
                (ShutdownWork::new(None, Duration::from_secs(5)), None, None)
            }
        };
        // All fallible executor preparation precedes seal and ownership handoff.
        // A pending-to-terminal race needs this same executor, not a spare worker.
        let mut execution = RetirementExecution {
            executor: Self::server_runtime()?,
            seed: None,
            shutdown_timeout: work.shutdown_timeout,
            _host: host,
        };
        let mut state = shutdown_lock(&self.state, deadline, "core_retirement_wait")?;
        if let Some(transaction) = &state.retirement {
            return observe_existing(transaction);
        }
        match &state.host_slot {
            HostSlot::Starting(_) => return Err(pending_host_teardown()),
            HostSlot::Running { server, .. }
                if required_server
                    .as_ref()
                    .is_some_and(|target| !Weak::ptr_eq(target, server))
                    && server.upgrade().is_some_and(|current| current.is_running()) =>
            {
                return Err(LifecycleError::Configuration(
                    "shutdown target is outside the current Host scope".into(),
                ));
            }
            _ => {}
        }
        let key = work
            .server
            .as_ref()
            .map(|server| format!("{}:{:p}", server.server_instance_id(), server.as_ref()));
        let existing = selected.or_else(|| {
            key.as_ref()
                .and_then(|key| state.shutdown_transactions.get(key).cloned())
        });
        // Acquire the original pool locks within this caller's deadline before
        // spawning or publishing. An uncommitted guard leaves admission intact.
        let pool = state.client_pool.clone();
        let seal = pool
            .as_ref()
            .map(|pool| {
                pool.prepare_seal(deadline)
                    .ok_or(LifecycleError::ShutdownWaitTimeout {
                        stage: "core_pool_seal_wait",
                    })
            })
            .transpose()?;
        let historical = if let Some(transaction) = &existing {
            let mut progress =
                shutdown_lock(&transaction.outcome, deadline, "core_shutdown_outcome_wait")?;
            if progress.outcome.is_none() {
                execution.seed = seal.map(|seal| seal.commit());
                progress.retirement = Some(execution);
                state.client_domain.close_operations();
                state.server_role.close_operations();
                state.retirement = Some(transaction.clone());
                return Ok(RuntimeShutdownReceipt {
                    transaction: transaction.clone(),
                });
            }
            progress.outcome.clone()
        } else {
            None
        };
        if let Some(key) = &key {
            work.scopes = state
                .registration_scopes
                .get(key)
                .cloned()
                .unwrap_or_default();
        }
        let transaction = ShutdownTransaction::new(
            self,
            required_server.unwrap_or_default(),
            work.shutdown_timeout,
        );
        // The new worker first takes RuntimeState. It cannot execute until this
        // section has published both the scope and its permanent admission fence.
        self.spawn_shutdown_driver(transaction.clone(), work, historical)?;
        execution.seed = seal.map(|seal| seal.commit());
        transaction.outcome.lock().retirement = Some(execution);
        if existing.is_none() {
            if let Some(key) = key {
                state.shutdown_transactions.insert(key, transaction.clone());
            }
        }
        // Permanent retirement closes both role domains' call admission.
        state.client_domain.close_operations();
        state.server_role.close_operations();
        state.retirement = Some(transaction.clone());
        Ok(RuntimeShutdownReceipt { transaction })
    }

    /// Capture this session's retirement handoff for a replacement session:
    /// both roles' explicit code-override intent (an auto-generated server id
    /// never travels as an override), the weak observers of this session's
    /// own two domains, and every earlier adopted retired record. Capture is
    /// retry-safe and does not consume the records.
    pub fn capture_retirement_handoff(
        &self,
    ) -> Result<crate::memory::RetirementHandoff, LifecycleError> {
        let state = self.state.lock();
        if !state
            .retirement
            .as_ref()
            .is_some_and(|transaction| transaction.outcome.lock().outcome.is_some())
        {
            return Err(LifecycleError::Configuration(
                "retirement must complete before capturing a Runtime handoff".into(),
            ));
        }
        let host_unsettled = match &state.host_slot {
            HostSlot::Starting(_) => true,
            HostSlot::Running { server, .. } => server.upgrade().is_some_and(|server| {
                let key = format!("{}:{:p}", server.server_instance_id(), server.as_ref());
                server.is_running() || !state.shutdown_transactions.contains_key(&key)
            }),
            HostSlot::Vacant => false,
        };
        if host_unsettled || state.has_pending_shutdown() {
            return Err(pending_host_teardown());
        }
        let mut roles: Vec<crate::roles::RetiredRoleObserver> = Vec::new();
        roles.extend(state.client_domain.retire_observer(crate::RoleKind::Client));
        roles.extend(state.server_role.retire_observer());
        let (adopted_roles, adopted_trackers) = state.retired.capture();
        roles.extend(adopted_roles);
        let trackers = std::iter::once(state.holds.observer())
            .chain(adopted_trackers)
            .collect();
        Ok(crate::memory::RetirementHandoff::new(
            roles,
            trackers,
            *state.client_domain.overrides(),
            state.server_role.overrides().clone(),
        ))
    }

    /// Adopt a captured retirement handoff: install both roles' explicit
    /// code-override intent and merge the retired records into this fresh
    /// session's observation list.
    pub fn adopt_retirement_handoff(
        &self,
        handoff: &crate::memory::RetirementHandoff,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_active()?;
        if state.client_domain.frozen() || state.server_role.frozen() {
            return Err(LifecycleError::ConfigFrozen);
        }
        let (client_overrides, server_overrides) = handoff.override_intent();
        state.client_domain.adopt_overrides(*client_overrides);
        state.server_role.adopt_overrides(server_overrides.clone());
        state.identity = None;
        handoff.adopt_into(&mut state.retired);
        Ok(())
    }

    /// Subscribe to future outgoing and accepted IPC connections without starting a host or
    /// resolving configuration. The receiver retains metadata alone.
    pub fn connection_events(&self) -> c2_local::ConnectionEvents {
        self.connection_event_source().subscribe()
    }

    pub(crate) fn connection_event_source(&self) -> c2_local::ConnectionEventSource {
        self.state.lock().connection_event_source.clone()
    }

    pub fn new(options: RuntimeOptions) -> Result<Self, LifecycleError> {
        let mut client_domain = crate::roles::RoleDomain::new(options.client_limits);
        if options.client_limits != c2_config::DomainLimitsOverrides::default() {
            client_domain.update(c2_config::DomainLimitsPatch::default())?;
        }
        Ok(Self {
            state: Arc::new(Mutex::new(RuntimeState {
                connection_event_source: c2_local::ConnectionEventSource::new(),
                client_domain,
                server_role: crate::roles::ServerRole::new(Default::default()),
                host_slot: HostSlot::default(),
                local_endpoint_options: options.local_endpoint,
                local_endpoint_sources: None,
                local_endpoint_revision: 0,
                frozen_local_endpoint: None,
                inherited_local_endpoint: None,
                client_pool: None,
                identity: None,
                relay_anchor_address_override: options
                    .relay_anchor_address
                    .map(|addr| canonical_relay_anchor_address(&addr)),
                use_process_relay_anchor: options.use_process_relay_anchor,
                relay_projection: None,
                path_counters: PathCounters::default(),
                retired: Default::default(),
                holds: Arc::new(crate::memory::LocalCopyTracker::new()),
                shutdown_transactions: HashMap::new(),
                retirement: None,
                registration_scopes: HashMap::new(),
                #[cfg(test)]
                forced_relay_config_error: None,
                #[cfg(test)]
                shutdown_publication_gate: None,
            })),
        })
    }

    /// Tri-state incremental update of the client role configuration.
    ///
    /// `Keep` retains the previously accepted override of a field (a later
    /// memory-only change never washes away an explicitly set call limit),
    /// `Inherit` clears the field's override back to environment/defaults, and
    /// `Set` installs a new value. The complete candidate resolves and
    /// validates atomically: one invalid field changes nothing. Once the
    /// client domain has frozen, only a fully identical effective
    /// configuration remains an idempotent success; any difference is
    /// `LifecycleError::ConfigFrozen` — there is no silent-ignore path.
    pub fn update_client_config(
        &self,
        patch: crate::roles::ClientConfigPatch,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_active()?;
        state.client_domain.update(patch.domain_limits)
    }

    /// Tri-state incremental update of the server role configuration: Host
    /// identity, startup timeout, carrying capacity and server-domain limits.
    /// The same freeze contract as [`Self::update_client_config`] applies
    /// through the first Host start.
    pub fn update_server_config(
        &self,
        patch: c2_config::ServerConfigPatch,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_active()?;
        state.ensure_no_pending_teardown()?;
        let old_id = state.server_role.overrides().server_id.clone();
        state.server_role.update(patch)?;
        if !state.server_role.frozen() && state.server_role.overrides().server_id != old_id {
            state.identity = None;
        }
        Ok(())
    }

    /// Whether the client role domain has frozen (first remote business
    /// connection or call).
    pub fn client_config_frozen(&self) -> bool {
        self.state.lock().client_domain.frozen()
    }

    /// Whether the server role domain has frozen (first Host start).
    pub fn server_config_frozen(&self) -> bool {
        self.state.lock().server_role.frozen()
    }

    /// Symmetric read-only call-admission snapshot of both role domains plus
    /// retired roles. Never reads the environment, creates a domain, or
    /// freezes configuration.
    pub fn call_execution_snapshot(&self) -> crate::memory::RuntimeCallExecutionStats {
        self.role_stats_snapshot().call_execution
    }

    /// Observe the frozen client domain without creating one or retaining
    /// producer authority. A retired session may still have live owners.
    pub fn client_domain_observer(&self) -> Option<c2_mem::DomainObserver> {
        self.state
            .lock()
            .client_domain
            .budget()
            .map(c2_mem::DomainBudget::observe)
    }

    /// This session's local-hold metadata tracker. SDKs record their holds
    /// here so `memory_stats().holds` stays observable.
    #[doc(hidden)]
    pub fn holds_tracker(&self) -> Arc<crate::memory::LocalCopyTracker> {
        self.state.lock().holds.clone()
    }

    pub(crate) fn call_execution_context(&self) -> Result<CallExecutionContext, LifecycleError> {
        let mut state = self.state.lock();
        if let Some(budget) = state.client_domain.budget() {
            return Ok(CallExecutionContext::new(budget.clone()));
        }
        state.ensure_active()?;
        Ok(CallExecutionContext::new(state.client_domain.freeze()?))
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
        state.inherited_local_endpoint = None;
        state.local_endpoint_revision += 1;
        Ok(())
    }

    /// Install an already captured native endpoint configuration without resolving again.
    #[doc(hidden)]
    pub fn set_captured_local_endpoint(
        &self,
        context: c2_config::LocalEndpointContext,
    ) -> Result<(), LifecycleError> {
        let mut state = self.state.lock();
        if let Some(frozen) = &state.frozen_local_endpoint {
            return if frozen == &context {
                Ok(())
            } else {
                Err(LifecycleError::ConfigFrozen)
            };
        }
        state.inherited_local_endpoint = Some(context);
        state.local_endpoint_revision += 1;
        Ok(())
    }

    /// Inherit endpoint selection without resolving sources or platform scope.
    /// Unused policies remain lazy; a used domain survives environment changes.
    /// The replacement remains configurable until its own first local I/O.
    pub fn inherit_local_endpoint_selection(&self, previous: &Self) -> Result<(), LifecycleError> {
        if Arc::ptr_eq(&self.state, &previous.state) {
            return Ok(());
        }
        let (options, sources, selected) = {
            let state = previous.state.lock();
            (
                state.local_endpoint_options.clone(),
                state.local_endpoint_sources.clone(),
                state
                    .frozen_local_endpoint
                    .clone()
                    .or_else(|| state.inherited_local_endpoint.clone()),
            )
        };
        // Never hold both Runtime locks, including for opposite-direction handoffs.
        let mut state = self.state.lock();
        if state.frozen_local_endpoint.is_some() {
            return Err(LifecycleError::ConfigFrozen);
        }
        state.local_endpoint_options = options;
        state.local_endpoint_sources = sources;
        state.inherited_local_endpoint = selected;
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
        self.resolve_local_endpoint_context_with_deadline(
            freeze,
            captured,
            ConnectDeadline::default(),
        )
        .map_err(unlimited_lifecycle_error)
    }

    fn resolve_local_endpoint_context_with_deadline(
        &self,
        freeze: bool,
        captured: Option<&c2_config::LocalEndpointContext>,
        deadline: ConnectDeadline,
    ) -> Result<Option<c2_config::LocalEndpointContext>, crate::Error> {
        self.resolve_local_endpoint_for_acquire(freeze, captured, deadline, None)
    }

    fn resolve_local_endpoint_for_acquire(
        &self,
        freeze: bool,
        captured: Option<&c2_config::LocalEndpointContext>,
        deadline: ConnectDeadline,
        activation_catalog: Option<(u64, &c2_config::EnvCatalog)>,
    ) -> Result<Option<c2_config::LocalEndpointContext>, crate::Error> {
        let accepts = |context: &c2_config::LocalEndpointContext| {
            captured.is_none_or(|captured| captured == context)
        };
        loop {
            let (options, sources, revision) = {
                let mut state = connect_state_lock(&self.state, deadline, "runtime_context_wait")?;
                if let Some(context) = &state.frozen_local_endpoint {
                    return Ok(accepts(context).then(|| context.clone()));
                }
                if let Some(context) = state.inherited_local_endpoint.clone() {
                    if !accepts(&context) {
                        return Ok(None);
                    }
                    if freeze {
                        state.frozen_local_endpoint = Some(context.clone());
                    }
                    return Ok(Some(context));
                }
                (
                    state.local_endpoint_options.clone(),
                    state.local_endpoint_sources.clone(),
                    state.local_endpoint_revision,
                )
            };
            let resolved = match (sources, activation_catalog) {
                (None, Some((activation_revision, catalog))) if activation_revision == revision => {
                    c2_config::ConfigResolver::resolve_local_endpoint(options, catalog)
                        .map_err(|error| LifecycleError::Configuration(error.to_string()))
                }
                (sources, _) => resolve_local_endpoint(options, sources),
            };
            let mut state = connect_state_lock(&self.state, deadline, "runtime_context_wait")?;
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
        state.ensure_active()?;
        if let Some(identity) = &state.identity {
            return Ok(identity.clone());
        }
        // The frozen server role pins the identity; before the first Host
        // start the Runtime mints the auto identity the freeze will adopt.
        let server_id = state
            .server_role
            .resolved()
            .and_then(|frozen| frozen.server_id.clone())
            .or_else(|| state.server_role.overrides().server_id.clone())
            .unwrap_or_else(auto_server_id);
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

    pub fn server_address(&self) -> Option<String> {
        self.state
            .lock()
            .identity
            .as_ref()
            .map(|identity| identity.ipc_address.clone())
    }

    /// The frozen server-role configuration, once a Host has started.
    pub fn server_config(&self) -> Option<c2_config::ResolvedServerConfig> {
        let state = self.state.lock();
        state
            .server_role
            .resolved()
            .map(|frozen| c2_config::ResolvedServerConfig {
                server_id: frozen.server_id.clone(),
                startup_timeout: frozen.startup_timeout,
                ipc: frozen.ipc,
                domain_limits: frozen.domain_limits,
            })
    }

    /// Accepted server-role code overrides without resolving defaults or the
    /// environment.
    pub fn server_overrides(&self) -> c2_config::ServerConfigOverrides {
        self.state.lock().server_role.overrides().clone()
    }

    /// Accepted client-domain code overrides without resolving defaults or
    /// the environment.
    pub fn client_overrides(&self) -> c2_config::DomainLimitsOverrides {
        self.state.lock().client_domain.overrides().clone()
    }

    pub fn path_counters(&self) -> PathCounters {
        self.state.lock().path_counters
    }

    pub(crate) fn publish_client_with_deadline<T>(
        &self,
        path: ObservedPath,
        deadline: ConnectDeadline,
        publish: impl FnOnce() -> T,
    ) -> Result<T, crate::Error> {
        let mut state = connect_state_lock(&self.state, deadline, "connect_finish")?;
        state.ensure_active()?;
        let client = publish();
        connect_stage_check(deadline, "connect_finish")?;
        state.path_counters.record(path);
        Ok(client)
    }

    /// Symmetric read-only memory statistics for this Runtime: the `client`
    /// and `server` role domains, retired roles still owned by real in-flight
    /// work, and local holds. Observation never reads the environment,
    /// creates a domain, or freezes configuration.
    pub fn memory_stats(&self) -> crate::RuntimeMemoryStats {
        self.role_stats_snapshot().memory
    }

    /// Read-only observer of the frozen server role domain.
    pub fn server_domain_observer(&self) -> Option<c2_mem::DomainObserver> {
        self.state
            .lock()
            .server_role
            .budget()
            .map(c2_mem::DomainBudget::observe)
    }

    /// Admit one Host start through the single Host slot.
    ///
    /// Concurrent same-configuration starts merge onto the running Host; a
    /// different effective configuration is refused as `ConfigFrozen` by the
    /// frozen server role; a still-draining previous Host refuses new starts
    /// until its original close transaction reaches a real terminal outcome.
    pub(crate) fn begin_host_start(
        &self,
        options: &crate::HostOptions,
    ) -> Result<HostAdmission, LifecycleError> {
        let mut state = self.state.lock();
        state.ensure_no_pending_teardown()?;
        let mut running_host = None;
        let mut starting = None;
        match &state.host_slot {
            HostSlot::Vacant => {}
            HostSlot::Starting(attempt) => starting = Some(attempt.clone()),
            HostSlot::Running { inner, server } => {
                if let Some(server) = server.upgrade() {
                    let key = format!("{}:{:p}", server.server_instance_id(), server.as_ref());
                    if !state.shutdown_transactions.contains_key(&key) {
                        if inner.strong_count() == 0 || !server.is_ready() {
                            return Err(pending_host_teardown());
                        }
                        running_host = Some(inner.clone());
                    } else if server.is_running() {
                        return Err(pending_host_teardown());
                    }
                }
            }
        }

        let previous_identity = state.identity.clone();
        let relay_override = state.relay_anchor_address_override.clone();
        let use_process_relay_anchor = state.use_process_relay_anchor;
        #[cfg(test)]
        let forced_relay_error = state.forced_relay_config_error.clone();
        let freeze = state.server_role.start(
            options.server.clone(),
            || {
                previous_identity
                    .as_ref()
                    .map(|identity| identity.server_id.clone())
                    .unwrap_or_else(auto_server_id)
            },
            |catalog, frozen| {
                options.resolve_settings(catalog, frozen, || {
                    #[cfg(test)]
                    if let Some(message) = forced_relay_error {
                        return Err(LifecycleError::Relay(message));
                    }
                    if relay_override.is_some() || !use_process_relay_anchor {
                        return Ok(relay_override);
                    }
                    c2_config::ConfigResolver::resolve_relay_anchor_address(catalog)
                        .map(|anchor| {
                            anchor.map(|address| canonical_relay_anchor_address(&address))
                        })
                        .map_err(|error| LifecycleError::Relay(error.to_string()))
                })
            },
        )?;
        if let Some(attempt) = starting {
            return Ok(HostAdmission::Starting(attempt));
        }
        if let Some(inner) = running_host {
            return inner
                .upgrade()
                .map(|inner| HostAdmission::Shared(crate::Host::from_inner(inner)))
                .ok_or_else(pending_host_teardown);
        }

        let server_id = freeze
            .config
            .server_id
            .expect("Host start materializes a server id");
        let prior_instance_id = previous_identity
            .filter(|identity| !freeze.restart && identity.server_id == server_id)
            .map(|identity| identity.server_instance_id);
        let identity = RuntimeIdentity {
            ipc_address: ipc_address_for_server_id(&server_id),
            server_id,
            server_instance_id: prior_instance_id.unwrap_or_else(auto_server_instance_id),
        };
        state.identity = Some(identity.clone());
        let endpoint_catalog = if freeze.restart {
            state.server_role.advance_generation();
            None
        } else {
            Some((
                state.local_endpoint_revision,
                state
                    .server_role
                    .catalog_ref()
                    .expect("newly frozen server role")
                    .clone(),
            ))
        };
        let attempt = Arc::new(HostStartup::default());
        state.host_slot = HostSlot::Starting(attempt.clone());
        Ok(HostAdmission::Granted(
            HostStart {
                identity,
                ipc: freeze.config.ipc,
                budget: freeze.budget,
                startup_timeout: freeze.config.startup_timeout,
                settings: freeze.settings,
                endpoint_catalog,
            },
            attempt,
        ))
    }

    pub(crate) fn complete_host_start(
        &self,
        attempt: &HostStartup,
        result: Result<crate::Host, LifecycleError>,
        receipt: Option<FailedStartShutdownReceipt>,
    ) {
        let mut state = self.state.lock();
        state.host_slot = if let Ok(host) = &result {
            HostSlot::Running {
                inner: Arc::downgrade(host.inner()),
                server: host.retirement_server(),
            }
        } else {
            HostSlot::Vacant
        };
        *attempt.result.lock() = Some((result, receipt));
        attempt.completed.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn host_start_waiter_count_for_test(&self) -> usize {
        match &self.state.lock().host_slot {
            HostSlot::Starting(attempt) => Arc::strong_count(attempt).saturating_sub(2),
            _ => 0,
        }
    }

    pub(crate) fn freeze_host_endpoint_context(
        &self,
        activation_catalog: Option<(u64, &c2_config::EnvCatalog)>,
    ) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
        self.resolve_local_endpoint_for_acquire(
            true,
            None,
            ConnectDeadline::default(),
            activation_catalog,
        )
        .map(|context| context.expect("unconditional context resolution"))
        .map_err(unlimited_lifecycle_error)
    }

    pub(crate) fn role_stats_snapshot(&self) -> crate::memory::RoleStatsBundle {
        let state = self.state.lock();
        let client = state.client_domain.stats();
        let server = state.server_role.stats();
        let (retired_memory, retired_calls) = state.retired.reports();
        let mut holds = state.holds.stats();
        holds.merge(&state.retired.local_copy_stats());
        crate::memory::RoleStatsBundle {
            memory: crate::RuntimeMemoryStats {
                client: crate::memory::RoleMemoryStats::from_role(&client),
                server: crate::memory::RoleMemoryStats::from_role(&server),
                retired: retired_memory,
                holds,
            },
            call_execution: crate::memory::RuntimeCallExecutionStats {
                client: role_call_stats(&client),
                server: role_call_stats(&server),
                retired: retired_calls,
            },
        }
    }

    /// Acquire an outgoing IPC client from this Runtime's cache.
    ///
    /// The resolved client config freezes atomically under the RuntimeState
    /// lock before any connection I/O: the first valid connection attempt —
    /// including one that later fails — fixes the config for this Runtime's
    /// lifetime, and a setter racing a stalled first connect observes
    /// `LifecycleError::ConfigFrozen` instead of producing a second
    /// configuration. The connect itself runs outside the state lock through
    /// the cache's epoch fence.
    pub(crate) fn acquire_ipc_client_with_deadline(
        &self,
        address: &str,
        deadline: ConnectDeadline,
    ) -> Result<c2_ipc::pool::ClientLease, c2_ipc::IpcError> {
        c2_config::LocalEndpoint::validate_address(address)
            .map_err(|error| c2_ipc::IpcError::Config(error.to_string()))?;
        self.ipc_client_domain(deadline)?
            .acquire_lease_with_deadline(address, deadline)
    }

    /// Freeze the client role domain and return its budget together with the
    /// outgoing IPC pool charging it. The pool is created on first acquire so
    /// HTTP-only Runtimes need no local platform scope; clones share it, and
    /// drains never replace it.
    pub(crate) fn ipc_client_domain(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<Arc<c2_ipc::ClientPool>, c2_ipc::IpcError> {
        let (budget, activation_catalog) = {
            let mut state = connect_state_lock(&self.state, deadline, "runtime_config_wait")
                .map_err(connect_error_to_ipc)?;
            state
                .ensure_active()
                .map_err(|_| c2_ipc::IpcError::PoolRetired)?;
            if let Some(budget) = state.client_domain.budget() {
                (budget.clone(), None)
            } else {
                let budget = state
                    .client_domain
                    .freeze()
                    .map_err(|error| c2_ipc::IpcError::Config(error.to_string()))?;
                let catalog = state
                    .client_domain
                    .catalog_ref()
                    .expect("just captured")
                    .clone();
                (budget, Some((state.local_endpoint_revision, catalog)))
            }
        };
        // Reuse a catalog only within the same activation attempt. An earlier
        // HTTP acquisition may have frozen capacity before the IPC endpoint
        // was selected, so later local I/O resolves current endpoint inputs.
        let context = self
            .resolve_local_endpoint_for_acquire(
                true,
                None,
                deadline,
                activation_catalog
                    .as_ref()
                    .map(|(revision, catalog)| (*revision, catalog)),
            )
            .map_err(connect_error_to_ipc)?
            .expect("unconditional context resolution");
        let mut state = connect_state_lock(&self.state, deadline, "runtime_config_wait")
            .map_err(connect_error_to_ipc)?;
        state
            .ensure_active()
            .map_err(|_| c2_ipc::IpcError::PoolRetired)?;
        let events = state.connection_event_source.clone();
        Ok(state
            .client_pool
            .get_or_insert_with(|| {
                Arc::new(
                    c2_ipc::ClientPool::with_endpoint_context_and_budget(context, budget)
                        .with_connection_event_source(events),
                )
            })
            .clone())
    }

    /// The client role domain's budget for HTTP data clients; freezes the
    /// client domain on first use exactly like the IPC pool path.
    pub(crate) fn client_domain_budget_with_deadline(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<c2_mem::DomainBudget, crate::Error> {
        let mut state = connect_state_lock(&self.state, deadline, "runtime_config_wait")?;
        state.ensure_active()?;
        let budget = state.client_domain.freeze()?;
        Ok(budget)
    }

    pub(crate) fn discard_ipc_client_with_deadline(
        &self,
        address: &str,
        client: &Arc<c2_ipc::SyncClient>,
        deadline: ConnectDeadline,
    ) -> Result<(), crate::Error> {
        let pool = connect_state_lock(&self.state, deadline, "runtime_pool_wait")?
            .client_pool
            .clone();
        if let Some(pool) = pool {
            pool.discard_if_same_with_deadline(address, client, deadline)
                .map_err(|error| {
                    crate::normalize_ipc_error(error, crate::TransportPhase::PreDispatch)
                })?;
        }
        connect_stage_check(deadline, "pool_discard")
    }

    /// Resolve this Runtime's server IPC configuration without starting a
    /// Host. Never freezes the server role and never reads the environment
    /// once frozen; before the first Host start it resolves the accepted
    /// overrides truthfully.
    pub fn server_ipc_config(&self) -> Result<c2_config::ServerIpcConfig, LifecycleError> {
        let state = self.state.lock();
        if let Some(frozen) = state.server_role.resolved() {
            return Ok(frozen.ipc);
        }
        let catalog = c2_config::EnvCatalog::load(c2_config::ConfigSources::from_process())
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        c2_config::ConfigResolver::resolve_server(state.server_role.overrides(), &catalog)
            .map(|resolved| resolved.ipc)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))
    }

    pub(crate) fn relay_business_config_with_deadline(
        &self,
        explicit_anchor: Option<String>,
        deadline: ConnectDeadline,
    ) -> Result<(String, c2_config::ResolvedRelayClientConfig), crate::Error> {
        let mut state = connect_state_lock(&self.state, deadline, "relay_config")?;
        state.ensure_active()?;
        #[cfg(test)]
        if let Some(message) = &state.forced_relay_config_error {
            return Err(LifecycleError::Relay(message.clone()).into());
        }
        let configured_anchor =
            explicit_anchor.or_else(|| state.relay_anchor_address_override.clone());
        if configured_anchor.is_none() && !state.use_process_relay_anchor {
            return Err(LifecycleError::MissingRelayAddress.into());
        }
        let catalog = match state.client_domain.catalog_ref() {
            Some(catalog) => catalog.clone(),
            None => c2_config::EnvCatalog::load(c2_config::ConfigSources::from_process())
                .map_err(|error| LifecycleError::Configuration(error.to_string()))?,
        };
        let anchor = match configured_anchor {
            Some(anchor) => anchor,
            None => c2_config::ConfigResolver::resolve_relay_anchor_address(&catalog)
                .map_err(|error| LifecycleError::Relay(error.to_string()))?
                .ok_or(LifecycleError::MissingRelayAddress)?,
        };
        let anchor = canonical_relay_anchor_address(&anchor);
        if !c2_http::valid_relay_url(&anchor) {
            return Err(normalize_http_acquire_error(HttpError::InvalidInput(
                "invalid HTTP relay base URL".into(),
            )));
        }
        let settings = c2_config::ConfigResolver::resolve_relay_client(None, &catalog)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        connect_stage_check(deadline, "relay_config")?;
        state.client_domain.freeze_with_catalog(catalog)?;
        Ok((anchor, settings))
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

    /// Atomically move the unstarted server lifecycle to a prepared Runtime.
    ///
    /// Session replacement is a pre-use configuration transaction. Both sides
    /// are checked before moving subscriptions, so a refused handoff leaves
    /// the source usable. Lock order is stable even for opposing calls.
    /// Subscriptions to the future host follow this same pre-use handoff.
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
            if source.server_role.frozen() || source.client_domain.frozen() {
                return Err(LifecycleError::Configuration(
                    "cannot transfer lifecycle after Runtime use".into(),
                ));
            }
            if target.server_role.frozen() || target.client_domain.frozen() {
                return Err(LifecycleError::Configuration(
                    "replacement Runtime already owns a lifecycle".into(),
                ));
            }
            // Pre-use SDK configuration replaces the Runtime, but preserves
            // subscriptions to that same future host. A full shutdown does
            // not use this handoff and ends the old event source normally.
            target.connection_event_source = std::mem::take(&mut source.connection_event_source);
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

    pub fn effective_relay_anchor_address(&self) -> Result<Option<String>, LifecycleError> {
        self.effective_relay_anchor_address_with_deadline(ConnectDeadline::default())
            .map_err(unlimited_lifecycle_error)
    }

    pub(crate) fn effective_relay_anchor_address_with_deadline(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<Option<String>, crate::Error> {
        #[cfg(test)]
        if let Some(message) = connect_state_lock(&self.state, deadline, "relay_config")?
            .forced_relay_config_error
            .clone()
        {
            return Err(LifecycleError::Relay(message).into());
        }
        let (override_address, use_process_relay_anchor) = {
            let state = connect_state_lock(&self.state, deadline, "relay_config")?;
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
        let catalog = c2_config::EnvCatalog::load(c2_config::ConfigSources::from_process())
            .map_err(|e| LifecycleError::Relay(e.to_string()))?;
        c2_config::ConfigResolver::resolve_relay_anchor_address(&catalog)
            .map_err(|e| LifecycleError::Relay(e.to_string()).into())
    }

    #[cfg(test)]
    fn force_relay_config_error_for_test(&self, message: impl Into<String>) {
        self.state.lock().forced_relay_config_error = Some(message.into());
    }

    pub(crate) fn register_route(
        &self,
        server: &Arc<c2_server::Server>,
        route: BuiltRoute,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
    ) -> Result<RegisterOutcome, LifecycleError> {
        self.ensure_active()?;
        let identity = RuntimeIdentity {
            server_id: server.server_id().to_owned(),
            server_instance_id: server.server_instance_id().to_owned(),
            ipc_address: server.local_endpoint().address().to_owned(),
        };
        let route_name = route.name().to_string();
        let route_uid = route.route_uid().to_string();
        let registration_scope = RelayRegistrationScope {
            name: route_name.clone(),
            server_id: identity.server_id.clone(),
            server_instance_id: identity.server_instance_id.clone(),
            route_uid: route_uid.clone(),
        };
        let effective_relay_anchor_address =
            self.effective_relay_anchor_address_arg(relay_anchor_address)?;
        let expected = ExpectedRouteContract {
            route_name: route_name.clone(),
            crm_ns: route.crm_ns().to_string(),
            crm_name: route.crm_name().to_string(),
            crm_ver: route.crm_ver().to_string(),
            abi_hash: route.abi_hash().to_string(),
            signature_hash: route.signature_hash().to_string(),
        };

        let rt = Self::server_runtime()?;
        let reservation = rt.block_on(server.reserve_route(route)).map_err(|e| {
            if e.to_string().contains("already registered") {
                LifecycleError::DuplicateRoute(route_name.clone())
            } else {
                LifecycleError::from(e)
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
                            semantic_error: None,
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
                let (status_code, error_message, semantic_error) = http_error_parts(err);
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "relay_prepare".to_string(),
                        error_message,
                        status_code,
                        semantic_error,
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
                            semantic_error: None,
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
                        semantic_error: None,
                        rollback: Some(registration_rollback_outcome(&route_name, false)),
                        relay_cleanup_error: None,
                    },
                )));
            }
            None
        };

        if let Some(projection) = relay_projection.as_ref() {
            if let Err(err) = projection.control.register(RelayRegistration {
                expected: &expected,
                server_id: &identity.server_id,
                server_instance_id: &identity.server_instance_id,
                address: &identity.ipc_address,
                max_payload_size: server.config().max_payload_size,
            }) {
                let local_removed =
                    self.rollback_closed_registration(&rt, server, &registration_scope);
                let relay_cleanup_error = self.relay_cleanup(
                    effective_relay_anchor_address.as_deref(),
                    relay_use_proxy,
                    &registration_scope,
                );
                let (status_code, error_message, semantic_error) = http_error_parts(err);
                return Err(LifecycleError::RegisterFailure(Box::new(
                    RegisterFailureOutcome {
                        route_name: route_name.clone(),
                        failure_source: "relay_register".to_string(),
                        error_message,
                        status_code,
                        semantic_error,
                        rollback: Some(registration_rollback_outcome(&route_name, local_removed)),
                        relay_cleanup_error,
                    },
                )));
            }
            let route_admission_token = route_admission_token
                .expect("relay-backed registration should have a route admission token");
            if let Err(err) = rt.block_on(server.open_route_admission(route_admission_token)) {
                let local_removed =
                    self.rollback_closed_registration(&rt, server, &registration_scope);
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
                        semantic_error: None,
                        rollback: Some(registration_rollback_outcome(&route_name, local_removed)),
                        relay_cleanup_error,
                    },
                )));
            }
            relay_registered = true;
        }

        Ok(RegisterOutcome {
            route_name,
            server_id: identity.server_id,
            server_instance_id: identity.server_instance_id,
            ipc_address: identity.ipc_address,
            relay_registered,
        })
    }

    fn rollback_closed_registration(
        &self,
        rt: &tokio::runtime::Runtime,
        server: &c2_server::Server,
        scope: &RelayRegistrationScope,
    ) -> bool {
        let key = format!("{}:{:p}", server.server_instance_id(), server);
        let mut state = self.state.lock();
        let Some(current) = state.registration_scopes.get_mut(&key) else {
            return false;
        };
        if current.get(&scope.name) != Some(scope) {
            return false;
        }
        // Every Core registration publishes its scope before native commit.
        // Holding this existing lock excludes a replacement between identity
        // validation and removal. This path only owns a never-opened route:
        // no callback can be draining, and Host::register still retains the
        // ServiceDefinition, so unregister cannot run a service destructor.
        let local_removed = rt.block_on(server.registered_route_identity(&scope.name))
            == Some(scope.route_uid.clone())
            && rt.block_on(server.unregister_route(&scope.name));
        current.remove(&scope.name);
        if current.is_empty() {
            state.registration_scopes.remove(&key);
        }
        local_removed
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
        let mut scopes = self.capture_registration_scopes(server);
        scopes.retain(|name, _| name == &route_name);
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
    ) -> HashMap<String, RelayRegistrationScope> {
        let key = format!("{}:{:p}", server.server_instance_id(), server);
        self.state
            .lock()
            .registration_scopes
            .get(&key)
            .cloned()
            .unwrap_or_default()
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
        transaction.is_some_and(|transaction| transaction.outcome.lock().outcome.is_none())
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
        let outcome = transaction.outcome.lock().outcome.clone();
        outcome
    }

    pub(crate) fn shutdown_observation_until(
        &self,
        server: &c2_server::Server,
        deadline: Instant,
    ) -> Result<Option<ShutdownOutcome>, LifecycleError> {
        let transaction = shutdown_lock(&self.state, deadline, "core_shutdown_outcome_wait")?
            .shutdown_transactions
            .get(&format!("{}:{:p}", server.server_instance_id(), server))
            .cloned();
        transaction
            .map(|transaction| {
                Ok(
                    shutdown_lock(&transaction.outcome, deadline, "core_shutdown_outcome_wait")?
                        .outcome
                        .clone(),
                )
            })
            .transpose()
            .map(Option::flatten)
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
        let deadline = match c2_config::deadline_after(Instant::now(), shutdown_timeout) {
            Ok(deadline) => deadline,
            Err(error) => return ShutdownOutcome::invalid_timeout(error),
        };
        let scopes = server
            .map(|server| self.capture_registration_scopes(server))
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
        let receipt = self.begin_server_shutdown_transaction(
            server,
            route_names,
            relay_anchor_address,
            relay_use_proxy,
            relay_cleanup_config_error,
            shutdown_timeout,
            scopes,
        );
        let Some(result) = receipt.transaction.observe_until(deadline) else {
            return ShutdownOutcome {
                server_was_started: server.is_running(),
                runtime_barrier_error: Some(
                    "native shutdown transaction is still draining after the caller deadline"
                        .into(),
                ),
                ..Default::default()
            };
        };
        result
    }

    /// Start the existing transaction for this concrete Server without using
    /// another caller wait budget. The failed Host's normal Drop remains the
    /// one bounded Core shutdown waiter.
    pub(crate) fn begin_failed_start_shutdown(
        &self,
        server: &Arc<c2_server::Server>,
        route_names: Vec<String>,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
        shutdown_timeout: Duration,
    ) -> FailedStartShutdownReceipt {
        let scopes = self.capture_registration_scopes(server);
        self.begin_server_shutdown_transaction(
            server,
            route_names,
            relay_anchor_address,
            relay_use_proxy,
            None,
            shutdown_timeout,
            scopes,
        )
    }

    fn begin_server_shutdown_transaction(
        &self,
        server: &Arc<c2_server::Server>,
        route_names: Vec<String>,
        relay_anchor_address: Option<&str>,
        relay_use_proxy: bool,
        relay_cleanup_config_error: Option<String>,
        shutdown_timeout: Duration,
        scopes: HashMap<String, RelayRegistrationScope>,
    ) -> FailedStartShutdownReceipt {
        let key = format!("{}:{:p}", server.server_instance_id(), server.as_ref());
        let mut state = self.state.lock();
        state.shutdown_transactions.retain(|_, transaction| {
            transaction.server.strong_count() > 0 || transaction.outcome.lock().outcome.is_none()
        });
        let transaction = if let Some(transaction) = state.shutdown_transactions.get(&key) {
            transaction.clone()
        } else {
            let work = ShutdownWork {
                server: Some(server.clone()),
                route_names,
                relay_anchor_address: relay_anchor_address.map(str::to_owned),
                relay_use_proxy,
                relay_cleanup_config_error,
                shutdown_timeout,
                scopes,
            };
            let transaction =
                ShutdownTransaction::new(self, Arc::downgrade(server), work.shutdown_timeout);
            // Preserve the non-fallible legacy failed-start/Drop fallback.
            if let Err(error) = self.spawn_shutdown_driver(transaction.clone(), work, None) {
                transaction.outcome.lock().outcome = Some(ShutdownOutcome {
                    runtime_barrier_error: Some(error.to_string()),
                    ..Default::default()
                });
                transaction.completed.notify_all();
            }
            state.shutdown_transactions.insert(key, transaction.clone());
            transaction
        };
        FailedStartShutdownReceipt { transaction }
    }

    fn spawn_shutdown_driver(
        &self,
        transaction: Arc<ShutdownTransaction>,
        work: ShutdownWork,
        historical: Option<ShutdownOutcome>,
    ) -> Result<(), LifecycleError> {
        #[cfg(test)]
        if FORCE_SHUTDOWN_SPAWN_FAILURE.with(std::cell::Cell::get) {
            return Err(LifecycleError::Server(crate::ServerFailure::new(
                "failed to spawn shutdown worker: injected test failure".into(),
            )));
        }
        let runtime = self.clone();
        std::thread::Builder::new()
            .name("c2-shutdown".into())
            .spawn(move || {
                // Publication and startup use the same state fence.
                drop(runtime.state.lock());
                runtime.drive_shutdown_transaction(transaction, work, historical);
            })
            .map(|_| ())
            .map_err(|error| {
                LifecycleError::Server(crate::ServerFailure::new(format!(
                    "failed to spawn shutdown worker: {error}"
                )))
            })
    }

    fn drive_shutdown_transaction(
        &self,
        transaction: Arc<ShutdownTransaction>,
        work: ShutdownWork,
        historical: Option<ShutdownOutcome>,
    ) {
        let (pool, mut execution) = {
            let state = self.state.lock();
            let mut progress = transaction.outcome.lock();
            let execution = progress.retirement.take();
            let pool = execution.as_ref().and_then(|_| state.client_pool.clone());
            (pool, execution)
        };
        let mut observation = pool.as_ref().map(|pool| pool.observe_closes());
        if let Some(execution) = execution.as_mut() {
            if let (Some(observer), Some(seed)) = (observation.as_mut(), execution.seed.take()) {
                observer.merge_seed(seed);
            }
        }
        // Start client deactivation without waiting for its physical close.
        // A slow peer must not hold up the selected Host's own drain.
        let early_client_close = if execution.is_some() {
            observation
                .as_mut()
                .map(|observer| observer.close_all(Duration::ZERO))
        } else {
            None
        };
        let needs_final_drain = execution.is_none() || historical.is_some();
        let mut outcome = if let Some(historical) = historical {
            // The original journal is immutable and may already have been delivered.
            // Only the new sealed client scope and any still-live Server remain.
            historical
        } else {
            let outcome = self.execute_shutdown(&work, execution.as_ref().map(|e| &e.executor));
            if let Some(server) = &work.server {
                self.retire_registration_scopes(server, &work.scopes);
            }
            outcome
        };
        if let Some(mut report) = early_client_close {
            outcome
                .ipc_client_close_details
                .append(&mut report.abnormal);
        }
        #[cfg(test)]
        {
            let gate = self.state.lock().shutdown_publication_gate.take();
            if let Some(gate) = gate {
                gate.entered.send(()).unwrap();
                gate.proceed.lock().recv().unwrap();
            }
        }
        {
            let state = self.state.lock();
            let mut progress = transaction.outcome.lock();
            if execution.is_none() {
                execution = progress.retirement.take();
            }
            if execution.is_none() {
                progress.outcome = Some(outcome);
                drop(progress);
                drop(state);
                transaction.completed.notify_all();
                return;
            }
            drop(progress);
            drop(state);
        }
        let mut execution = execution.expect("retirement owns its executor");
        // An ordinary transaction may have started before the first pool was
        // created. Once sealed, that pool can never change or admit new entries.
        let late_pool = if pool.is_none() {
            self.state.lock().client_pool.clone()
        } else {
            None
        };
        let mut late_observation = late_pool.as_ref().map(|pool| pool.observe_closes());
        if let Some(server) = &work.server {
            // A prior error is not a stopped Server. Finish its existing native
            // lifecycle without replaying route unregister or relay withdrawal.
            let mut needs_completion = server.lifecycle_state().is_running()
                || outcome.route_close_error.is_some()
                || outcome.runtime_barrier_error.is_some()
                || outcome
                    .route_outcomes
                    .iter()
                    .any(|close| !close.active_drained || !close.local_removed);
            while needs_completion {
                let completion = execution.executor.block_on(async {
                    if server.lifecycle_state().is_running() {
                        server.request_shutdown_signal_with_reason("shutdown");
                    }
                    server.observe_shutdown_completion_unbounded().await
                });
                match completion {
                    Ok(completion) => {
                        if outcome.runtime_barrier_error.is_none() {
                            outcome.runtime_barrier_error = completion.runtime_error;
                        }
                        for close in completion.route_outcomes {
                            if !outcome.removed_routes.contains(&close.route_name) {
                                outcome.removed_routes.push(close.route_name.clone());
                            }
                            let close = route_close_from_server_outcome(close);
                            if let Some(previous) = outcome
                                .route_outcomes
                                .iter_mut()
                                .find(|old| old.route_name == close.route_name)
                            {
                                if !previous.local_removed || !previous.active_drained {
                                    *previous = close;
                                }
                            } else {
                                outcome.route_outcomes.push(close);
                            }
                        }
                    }
                    Err(error) => {
                        if outcome.runtime_barrier_error.is_none() {
                            outcome.runtime_barrier_error = Some(error.to_string());
                        }
                    }
                }
                needs_completion = server.lifecycle_state().is_running();
            }
        }
        let observer = observation.as_mut().or(late_observation.as_mut());
        if let Some(observer) = observer {
            if let Some(seed) = execution.seed.take() {
                observer.merge_seed(seed);
            }
            // Host completion and outgoing-client closure have separate owners.
            // Retain this transaction while the sealed pool's connections settle.
            if needs_final_drain || !outcome.ipc_clients_drained {
                Self::drain_ipc_clients(
                    &mut outcome,
                    Some(observer),
                    execution.shutdown_timeout,
                    true,
                );
            }
        } else {
            outcome.ipc_clients_drained = true;
        }
        {
            let _state = self.state.lock();
            transaction.outcome.lock().outcome = Some(outcome);
        }
        // Host Drop must observe the published result before waiters are notified.
        drop(execution);
        transaction.completed.notify_all();
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
        let work = ShutdownWork {
            server: server.cloned(),
            route_names,
            relay_anchor_address: relay_anchor_address.map(str::to_owned),
            relay_use_proxy,
            relay_cleanup_config_error,
            shutdown_timeout,
            scopes,
        };
        self.execute_shutdown(&work, None)
    }

    fn with_shutdown_executor<T>(
        prepared: Option<&tokio::runtime::Runtime>,
        execute: impl FnOnce(&tokio::runtime::Runtime) -> T,
    ) -> Result<T, LifecycleError> {
        if let Some(prepared) = prepared {
            return Ok(execute(prepared));
        }
        Ok(execute(&Self::server_runtime()?))
    }

    fn execute_shutdown(
        &self,
        work: &ShutdownWork,
        executor: Option<&tokio::runtime::Runtime>,
    ) -> ShutdownOutcome {
        let server = work.server.as_ref();
        let route_names = &work.route_names;
        let relay_anchor_address = work.relay_anchor_address.as_deref();
        let relay_use_proxy = work.relay_use_proxy;
        let scopes = &work.scopes;
        let (mut effective_relay_anchor_address, mut relay_config_errors) =
            match self.effective_relay_anchor_address_arg(relay_anchor_address) {
                Ok(address) => (address, Vec::new()),
                Err(err) => (None, vec![err.to_string()]),
            };
        if let Some(message) = &work.relay_cleanup_config_error {
            effective_relay_anchor_address = None;
            relay_config_errors.push(message.clone());
        }
        let server_was_started = server.map(|server| server.is_running()).unwrap_or(false);
        let mut outcome = ShutdownOutcome {
            server_was_started,
            ..Default::default()
        };
        for message in relay_config_errors {
            for route_name in route_names {
                outcome.relay_errors.push(RelayCleanupError {
                    route_name: route_name.clone(),
                    status_code: None,
                    message: message.clone(),
                });
            }
        }

        if let Some(server) = server {
            // A running Server owns the only drain and route journal. Request its close
            // immediately; its admission fence preserves accepted queue waits and
            // callbacks in the run loop. Never race a separate unregister against it.
            if matches!(
                server.lifecycle_state(),
                ServerLifecycleState::Starting | ServerLifecycleState::Ready
            ) {
                // Establish the observer executor before starting an otherwise
                // irreversible close; creation failure leaves the live Host alone.
                match Self::with_shutdown_executor(executor, |_rt| {
                    server.request_shutdown_signal_with_reason("shutdown");
                }) {
                    Ok(()) => {}
                    Err(err) => {
                        outcome.route_close_error = Some(err.to_string());
                        for route_name in route_names {
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

        if let Some(server) = server {
            let recorded_direct_shutdown_outcomes = if matches!(
                server.lifecycle_state(),
                ServerLifecycleState::Stopping
                    | ServerLifecycleState::Stopped
                    | ServerLifecycleState::Failed(_)
            ) {
                match Self::with_shutdown_executor(executor, |rt| {
                    rt.block_on(server.observe_shutdown_completion_unbounded())
                }) {
                    Ok(completion) => match completion {
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
                match Self::with_shutdown_executor(executor, |rt| {
                    rt.block_on(
                        server.unregister_routes_for_shutdown(
                            &pending_runtime_unregisters,
                            "shutdown",
                        ),
                    )
                }) {
                    Ok(close_outcomes) => {
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
                match Self::with_shutdown_executor(executor, |rt| {
                    rt.block_on(async {
                        server.request_shutdown_signal_with_reason("shutdown");
                        server.observe_shutdown_completion_unbounded().await
                    })
                }) {
                    Ok(completion) => match completion {
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
            for route_name in route_names {
                outcome.relay_errors.push(RelayCleanupError {
                    route_name: route_name.clone(),
                    status_code: None,
                    message: "hostless shutdown has no captured registration scope".into(),
                });
            }
        }

        outcome
    }

    fn drain_ipc_clients(
        outcome: &mut ShutdownOutcome,
        observation: Option<&mut c2_ipc::pool::ClientCloseObservation<'_>>,
        timeout: Duration,
        keep_draining: bool,
    ) {
        let Some(observation) = observation else {
            outcome.ipc_clients_drained = true;
            return;
        };
        let mut report = observation.close_all(timeout);
        outcome
            .ipc_client_close_details
            .append(&mut report.abnormal);
        while keep_draining && (!report.unconfirmed.is_empty() || report.error.is_some()) {
            report = observation.close_all(Duration::from_secs(1));
            outcome
                .ipc_client_close_details
                .append(&mut report.abnormal);
        }
        outcome.ipc_clients_drained = report.unconfirmed.is_empty() && report.error.is_none();
        if !outcome.ipc_clients_drained {
            let mut detail = report.error.unwrap_or_default();
            if !report.unconfirmed.is_empty() {
                if !detail.is_empty() {
                    detail.push_str("; ");
                }
                detail.push_str(&format!(
                    "unconfirmed IPC client cache closes for {:?}",
                    report.unconfirmed
                ));
            }
            outcome.ipc_client_close_error = Some(detail);
        }
    }

    pub(crate) fn resolve_relay_connection_with_deadline(
        &self,
        relay_anchor_address: &str,
        expected: ExpectedRouteContract,
        relay_use_proxy: bool,
        deadline: ConnectDeadline,
    ) -> Result<RelayResolvedConnection, crate::Error> {
        connect_stage_check(deadline, "relay_acquire")?;
        let frozen_context = connect_state_lock(&self.state, deadline, "runtime_relay_wait")?
            .frozen_local_endpoint
            .clone();
        let projection = self.relay_projection_for_address_with_deadline(
            relay_anchor_address,
            relay_use_proxy,
            frozen_context.as_ref(),
            deadline,
        )?;
        let client = RelayAwareHttpClient::new_with_control_and_budget(
            Arc::clone(&projection.control),
            expected,
            projection.relay_use_proxy,
            RelayAwareClientConfig {
                // One observation followed by at most one pre-dispatch refresh.
                max_attempts: 2,
            },
            self.client_domain_budget_with_deadline(deadline)?,
        )
        .map_err(normalize_http_acquire_error)?;
        let client = self.project_relay_client_context_with_deadline(client, true, deadline)?;
        match client
            .resolve_target_with_deadline(deadline)
            .map_err(normalize_http_acquire_error)?
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
            RelayResolvedTarget::Http { .. } => Ok(RelayResolvedConnection::Http { client }),
        }
    }

    pub(crate) fn resolve_relay_connection_after_local_ipc_failures_with_deadline(
        client: RelayAwareHttpClient,
        failed_candidates: &[RelayLocalIpcCandidate],
        deadline: ConnectDeadline,
    ) -> Result<RelayResolvedConnection, crate::Error> {
        connect_stage_check(deadline, "relay_acquire")?;
        match client
            .resolve_target_after_local_ipc_failures_with_deadline(failed_candidates, deadline)
            .map_err(normalize_http_acquire_error)?
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
            RelayResolvedTarget::Http { .. } => Ok(RelayResolvedConnection::Http { client }),
        }
    }

    pub(crate) fn connect_explicit_relay_http_client_with_deadline(
        &self,
        relay_url: &str,
        expected: ExpectedRouteContract,
        relay_use_proxy: bool,
        deadline: ConnectDeadline,
    ) -> Result<RelayAwareHttpClient, crate::Error> {
        connect_stage_check(deadline, "relay_acquire")?;
        let client = RelayAwareHttpClient::new_with_budget(
            relay_url,
            expected,
            relay_use_proxy,
            RelayAwareClientConfig {
                // One observation followed by at most one pre-dispatch refresh.
                max_attempts: 2,
            },
            self.client_domain_budget_with_deadline(deadline)?,
        )
        .map_err(normalize_http_acquire_error)?;
        let client = self
            .project_relay_client_context_with_deadline(client, false, deadline)?
            .with_http_only();
        match client
            .resolve_http_target_with_deadline(deadline)
            .map_err(normalize_http_acquire_error)?
        {
            RelayResolvedTarget::Http { .. } => Ok(client),
            RelayResolvedTarget::Ipc { .. } => unreachable!("HTTP relay connect returned IPC"),
        }
    }

    fn project_relay_client_context_with_deadline(
        &self,
        client: RelayAwareHttpClient,
        prefer_local_ipc: bool,
        deadline: ConnectDeadline,
    ) -> Result<RelayAwareHttpClient, crate::Error> {
        let frozen = connect_state_lock(&self.state, deadline, "runtime_relay_wait")?
            .frozen_local_endpoint
            .clone();
        if let Some(context) = frozen {
            return Ok(client.with_local_endpoint_context(&context));
        }
        Ok(if prefer_local_ipc && client.allows_local_ipc() {
            match self.resolve_local_endpoint_context_with_deadline(false, None, deadline) {
                Ok(Some(context)) => client.with_local_endpoint_context(&context),
                // An unusable local root/scope must not break a usable HTTP route.
                Err(error @ crate::Error::Semantic(_)) => return Err(error),
                _ => client.with_http_only(),
            }
        } else {
            client.with_http_only()
        })
    }

    /// Fence the captured comparison immediately before local acquire. A setter
    /// may have changed the Runtime root while relay resolution was in flight.
    /// Never reinterpret that candidate's logical address in a different domain.

    pub(crate) fn reconcile_relay_connection_with_deadline(
        &self,
        resolved: RelayResolvedConnection,
        deadline: ConnectDeadline,
    ) -> Result<RelayResolvedConnection, crate::Error> {
        connect_stage_check(deadline, "relay_acquire")?;
        let RelayResolvedConnection::Ipc {
            client,
            candidate,
            context,
        } = resolved
        else {
            return Ok(resolved);
        };
        if self
            .resolve_local_endpoint_context_with_deadline(true, Some(&context), deadline)?
            .is_some()
        {
            return Ok(RelayResolvedConnection::Ipc {
                client,
                candidate,
                context,
            });
        }
        let client = client.with_http_only();
        match client
            .resolve_http_target_with_deadline(deadline)
            .map_err(normalize_http_acquire_error)?
        {
            RelayResolvedTarget::Http { .. } => Ok(RelayResolvedConnection::Http { client }),
            RelayResolvedTarget::Ipc { .. } => {
                unreachable!("HTTP-only reconciliation returned IPC")
            }
        }
    }

    fn relay_projection_for_address(
        &self,
        relay_anchor_address: &str,
        relay_use_proxy: bool,
        local_endpoint_context: Option<&c2_config::LocalEndpointContext>,
    ) -> Result<RelayProjection, LifecycleError> {
        self.relay_projection_for_address_with_deadline(
            relay_anchor_address,
            relay_use_proxy,
            local_endpoint_context,
            ConnectDeadline::default(),
        )
        .map_err(unlimited_lifecycle_error)
    }

    fn relay_projection_for_address_with_deadline(
        &self,
        relay_anchor_address: &str,
        relay_use_proxy: bool,
        local_endpoint_context: Option<&c2_config::LocalEndpointContext>,
        deadline: ConnectDeadline,
    ) -> Result<RelayProjection, crate::Error> {
        let relay_anchor_address = canonical_relay_anchor_address(relay_anchor_address);
        {
            let state = connect_state_lock(&self.state, deadline, "runtime_relay_wait")?;
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
        connect_state_lock(&self.state, deadline, "runtime_relay_wait")?.relay_projection =
            Some(projection.clone());
        Ok(projection)
    }

    pub(crate) fn server_runtime() -> Result<tokio::runtime::Runtime, LifecycleError> {
        #[cfg(test)]
        if FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.get()) {
            return Err(LifecycleError::Server(crate::ServerFailure::new(
                "failed to create runtime: injected test failure".to_string(),
            )));
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                LifecycleError::Server(crate::ServerFailure::new(format!(
                    "failed to create runtime: {e}"
                )))
            })?;
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
            None => {
                let state = self.state.lock();
                if let Some(settings) = state.server_role.host_settings() {
                    return Ok(settings.relay_anchor_address.clone());
                }
                drop(state);
                self.effective_relay_anchor_address()
            }
        }
    }
}

pub(crate) fn canonical_relay_anchor_address(address: &str) -> String {
    address.trim().trim_end_matches('/').to_string()
}

fn resolve_local_endpoint(
    options: c2_config::LocalEndpointOptions,
    sources: Option<c2_config::ConfigSources>,
) -> Result<c2_config::LocalEndpointContext, LifecycleError> {
    let catalog =
        c2_config::EnvCatalog::load(sources.unwrap_or_else(c2_config::ConfigSources::from_process))
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
    c2_config::ConfigResolver::resolve_local_endpoint(options, &catalog)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

fn normalize_http_acquire_error(error: HttpError) -> crate::Error {
    let mut normalized = crate::normalize_http_error(error, crate::TransportPhase::PreDispatch);
    if let crate::Error::Semantic(semantic) = &mut normalized {
        if semantic
            .details
            .get("dispatch_phase")
            .is_some_and(|phase| phase != "pre_dispatch")
        {
            semantic.details.remove("dispatch_phase");
        }
        semantic
            .details
            .insert("transport_phase".into(), "pre_dispatch".into());
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::{
        CallOptions, CallTimeout, ConfigField, DomainLimitsOverrides, DomainLimitsPatch,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use c2_server::config::ServerIpcConfig;
    use c2_server::{
        BuiltRoute, ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec,
        SchedulerLimits,
    };

    #[test]
    fn connect_deadline_runtime_waits_are_bounded_in_all_modes() {
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let expected = ExpectedRouteContract {
            route_name: "connect-timeout".into(),
            crm_ns: "test.connect".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "a".repeat(64),
            signature_hash: "b".repeat(64),
        };
        for mode in [
            crate::Connect::DirectIpc {
                address: "malformed-no-io".into(),
            },
            crate::Connect::ExplicitRelay {
                relay_url: "malformed-no-io".into(),
            },
            crate::Connect::RelayAware,
        ] {
            let state = runtime.state.lock();
            let worker_runtime = runtime.clone();
            let expected = expected.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = worker_runtime.connect_with_options(
                    expected,
                    mode,
                    c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(25)),
                );
                done_tx.send(result).unwrap();
            });
            // Keep the real 25ms budget, but allow the worker to be scheduled.
            // Completion must still precede release of the contended state.
            let result = done_rx.recv_timeout(Duration::from_secs(2));
            drop(state);
            worker.join().unwrap();
            let error = result
                .expect("connect cannot wait forever for RuntimeState")
                .unwrap_err();
            let crate::Error::Semantic(error) = error else {
                panic!("canonical native deadline required")
            };
            assert_eq!(error.code, c2_error::ErrorCode::CallDeadlineExceeded);
            assert_eq!(error.details["operation"], "connect");
            assert_eq!(error.details["transport_phase"], "pre_dispatch");
        }
        assert!(!runtime.client_config_frozen());
        assert_eq!(runtime.path_counters(), PathCounters::default());
    }

    fn client_runtime(memory: u64, calls: u64) -> Runtime {
        Runtime::new(RuntimeOptions {
            client_limits: DomainLimitsOverrides {
                memory_budget_bytes: Some(memory),
                file_spill_budget_bytes: Some(0),
                max_outstanding_calls: Some(calls),
            },
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn concurrent_client_activation_shares_one_domain_and_its_charges() {
        let runtime = client_runtime(256, 8);
        assert!(runtime.client_domain_observer().is_none());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let context = runtime.call_execution_context().unwrap();
                    let mut call = context.prepare(CallOptions::new(), None).unwrap();
                    call.charge_input(32).unwrap();
                    call
                })
            })
            .collect::<Vec<_>>();
        let calls = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let observer = runtime.client_domain_observer().unwrap();
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.operations.used_operations, 8);
        assert_eq!(snapshot.memory.used_bytes, 256);
        assert_eq!(
            runtime.call_execution_snapshot().client.used_operations,
            Some(8),
        );
        assert_eq!(
            runtime.memory_stats().client.domain_id,
            Some(snapshot.domain_id)
        );
        drop(calls);
        assert_eq!(observer.snapshot().operations.used_operations, 0);
    }

    #[test]
    fn weak_domain_observer_sees_actual_input_after_runtime_drop() {
        let runtime = client_runtime(256, 8);
        let weak_session = Arc::downgrade(&runtime.state);
        let context = runtime.call_execution_context().unwrap();
        let observer = runtime.client_domain_observer().unwrap().downgrade();
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
        assert!(weak_session.upgrade().is_none());
        assert_eq!(input.as_ref().as_ref(), &[7; 32]);
        let observation = observer.snapshot().expect("input keeps the charge live");
        assert!(observation.closed);
        assert_eq!(observation.operations.used_operations, 0);
        assert_eq!(observation.memory.used_bytes, 32);
        drop(input);
        let until = Instant::now() + Duration::from_secs(10);
        while observer.is_alive() {
            assert!(
                Instant::now() < until,
                "real input owner did not release its charge"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn client_activation_racing_retirement_cannot_leave_open_admission() {
        for _ in 0..32 {
            let runtime = client_runtime(256, 8);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let initializing = {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    runtime.call_execution_context()
                })
            };
            barrier.wait();
            let deadline = Instant::now() + Duration::from_secs(1);
            let (outcome, completed) = runtime
                .begin_session_retirement(crate::RuntimeShutdownTarget::ClientsOnly, deadline)
                .unwrap()
                .observe_until(deadline);
            assert!(completed, "{outcome:?}");
            match initializing.join().unwrap() {
                Ok(context) => {
                    assert!(context.snapshot().closed);
                    assert!(runtime.call_execution_snapshot().client.closed);
                }
                Err(error) => {
                    assert_eq!(error, LifecycleError::RuntimeRetired);
                    assert!(runtime.client_domain_observer().is_none());
                }
            }
        }
    }

    #[test]
    fn endpoint_selection_inheritance_keeps_invalid_sources_lazy() {
        let previous = Runtime::new(RuntimeOptions::default()).unwrap();
        previous.state.lock().local_endpoint_sources = Some(c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([("C2_IPC_ROOT".into(), "relative".into())]),
        });
        let replacement = Runtime::new(RuntimeOptions::default()).unwrap();
        replacement
            .inherit_local_endpoint_selection(&previous)
            .unwrap();
        replacement
            .inherit_local_endpoint_selection(&replacement.clone())
            .unwrap();
        assert!(!previous.local_endpoint_frozen());
        assert!(!replacement.local_endpoint_frozen());
        assert!(replacement.local_endpoint_context().is_err());
        assert!(!replacement.local_endpoint_frozen());
        assert!(!replacement.memory_stats().client.frozen);
        let expected = c2_contract::ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap()
        .expected_route("route")
        .unwrap();
        let client = RelayAwareHttpClient::new(
            "https://relay.example",
            expected,
            false,
            RelayAwareClientConfig::default(),
        )
        .unwrap();
        let client = replacement
            .project_relay_client_context_with_deadline(client, false, ConnectDeadline::default())
            .unwrap();
        assert!(!client.allows_local_ipc());
        assert!(client.local_ipc_context().is_none());
        assert!(!replacement.local_endpoint_frozen());
        // Explicit sources stay native inputs, rather than disappearing at handoff.
        assert_eq!(
            replacement
                .state
                .lock()
                .local_endpoint_sources
                .as_ref()
                .unwrap()
                .process_env
                .get("C2_IPC_ROOT")
                .unwrap(),
            "relative"
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_selection_inheritance_pins_used_domain_but_allows_reconfiguration() {
        let previous = Runtime::new(RuntimeOptions::default()).unwrap();
        let at = |root: &str| c2_config::LocalEndpointOptions {
            unix_root: Some(root.into()),
        };
        let env_at = |root: &str| c2_config::ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: c2_config::EnvMap::from([("C2_IPC_ROOT".into(), root.into())]),
        };
        previous
            .set_local_endpoint_with_sources(Default::default(), env_at("/tmp/c2-old"))
            .unwrap();
        let captured = previous.freeze_local_endpoint_context().unwrap();
        previous.state.lock().local_endpoint_sources = Some(env_at("/tmp/c2-new"));
        let replacement = Runtime::new(RuntimeOptions::default()).unwrap();
        replacement
            .inherit_local_endpoint_selection(&previous)
            .unwrap();
        assert!(!replacement.local_endpoint_frozen());
        assert_eq!(replacement.local_endpoint_context().unwrap(), captured);
        // Repeated replacements must preserve the inherited selection as well.
        let next = Runtime::new(RuntimeOptions::default()).unwrap();
        next.inherit_local_endpoint_selection(&replacement).unwrap();
        assert_eq!(next.freeze_local_endpoint_context().unwrap(), captured);
        assert!(next.inherit_local_endpoint_selection(&previous).is_err());
        assert_eq!(next.local_endpoint_context().unwrap(), captured);
        assert!(
            next.set_local_endpoint_with_sources(
                at("/tmp/c2-new"),
                c2_config::ConfigSources::empty()
            )
            .is_err()
        );
        replacement
            .set_local_endpoint_with_sources(at("/tmp/c2-new"), c2_config::ConfigSources::empty())
            .unwrap();
        assert_ne!(replacement.local_endpoint_context().unwrap(), captured);
        assert_eq!(previous.local_endpoint_context().unwrap(), captured);
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
            let client = runtime
                .project_relay_client_context_with_deadline(
                    client,
                    prefer_local,
                    ConnectDeadline::default(),
                )
                .unwrap();
            assert!(!client.allows_local_ipc());
            assert!(client.local_ipc_context().is_none());
            assert!(!runtime.local_endpoint_frozen());
            let stats = runtime.memory_stats();
            assert!(!stats.client.frozen && !stats.server.frozen);
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
        assert!(
            !runtime
                .resolve_local_endpoint_context_with_deadline(
                    true,
                    Some(&captured),
                    ConnectDeadline::default()
                )
                .is_ok_and(|context| context.is_some())
        );
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
        assert!(
            runtime
                .resolve_local_endpoint_context_with_deadline(
                    true,
                    Some(&current),
                    ConnectDeadline::default()
                )
                .is_ok_and(|context| context.is_some())
        );
        assert!(runtime.local_endpoint_frozen());
        assert!(
            !runtime
                .resolve_local_endpoint_context_with_deadline(
                    true,
                    Some(&captured),
                    ConnectDeadline::default()
                )
                .is_ok_and(|context| context.is_some())
        );
        assert_eq!(runtime.local_endpoint_context().unwrap(), current);
        assert!(!runtime.memory_stats().client.frozen);
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
        assert!(
            !runtime
                .resolve_local_endpoint_context_with_deadline(
                    true,
                    Some(&captured),
                    ConnectDeadline::default()
                )
                .is_ok_and(|context| context.is_some())
        );
        assert!(!runtime.local_endpoint_frozen());
        assert!(!runtime.memory_stats().client.frozen);
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
            let accepted = runtime
                .resolve_local_endpoint_context_with_deadline(
                    true,
                    Some(&captured),
                    ConnectDeadline::default(),
                )
                .is_ok_and(|context| context.is_some());
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
            assert!(!runtime.memory_stats().client.frozen);
        }
    }

    #[test]
    fn role_snapshots_leave_configuration_mutable_until_activation() {
        let runtime = client_runtime(256, 8);
        let memory = runtime.memory_stats();
        assert!(!memory.client.frozen && !memory.server.frozen);
        assert!(memory.client.memory.is_none() && memory.server.memory.is_none());
        let calls = runtime.call_execution_snapshot();
        assert_eq!(calls.client.max_operations, None);
        assert_eq!(calls.server.max_operations, None);

        runtime
            .update_client_config(crate::ClientConfigPatch {
                domain_limits: DomainLimitsPatch {
                    memory_budget_bytes: ConfigField::Set(32),
                    ..Default::default()
                },
            })
            .unwrap();
        assert!(!runtime.memory_stats().client.frozen);
        runtime.call_execution_context().unwrap();
        assert_eq!(
            runtime.memory_stats().client.memory.unwrap().limit_bytes,
            32
        );
        assert_eq!(
            runtime.call_execution_snapshot().client.max_operations,
            Some(8)
        );
        assert!(!runtime.memory_stats().server.frozen);
    }

    #[test]
    fn idempotent_client_updates_keep_live_permits_and_reject_changes() {
        let runtime = client_runtime(32, 1);
        let clone = runtime.clone();
        let context = runtime.call_execution_context().unwrap();
        let mut call = context.prepare(CallOptions::new(), None).unwrap();
        call.charge_input(32).unwrap();
        let before = clone.memory_stats().client;
        clone
            .update_client_config(crate::ClientConfigPatch {
                domain_limits: DomainLimitsPatch {
                    max_outstanding_calls: ConfigField::Set(1),
                    ..Default::default()
                },
            })
            .unwrap();
        assert_eq!(clone.memory_stats().client, before);
        assert_eq!(
            clone.call_execution_snapshot().client.used_operations,
            Some(1)
        );
        assert_eq!(
            clone.update_client_config(crate::ClientConfigPatch {
                domain_limits: DomainLimitsPatch {
                    memory_budget_bytes: ConfigField::Set(64),
                    max_outstanding_calls: ConfigField::Set(2),
                    ..Default::default()
                },
            }),
            Err(LifecycleError::ConfigFrozen),
        );
        assert_eq!(clone.client_overrides().max_outstanding_calls, Some(1));
        assert!(
            clone
                .call_execution_context()
                .unwrap()
                .prepare(CallOptions::new(), None)
                .is_err()
        );
        drop(call);
        assert_eq!(
            clone.call_execution_snapshot().client.used_operations,
            Some(0)
        );
        assert_eq!(clone.memory_stats().client.memory.unwrap().used_bytes, 0);
    }

    #[test]
    fn client_retirement_closes_admission_and_keeps_actual_charge() {
        let runtime = client_runtime(32, 1);
        let context = runtime.call_execution_context().unwrap();
        let mut call = context.prepare(CallOptions::new(), None).unwrap();
        call.charge_input(32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let (outcome, completed) = runtime
            .begin_session_retirement(crate::RuntimeShutdownTarget::ClientsOnly, deadline)
            .unwrap()
            .observe_until(deadline);
        assert!(completed, "{outcome:?}");
        let snapshot = runtime.call_execution_snapshot().client;
        assert!(snapshot.closed);
        assert_eq!(snapshot.used_operations, Some(1));
        assert_eq!(runtime.memory_stats().client.memory.unwrap().used_bytes, 32);
        assert!(context.prepare(CallOptions::new(), None).is_err());
        assert!(
            context
                .prepare(CallOptions::with_timeout(CallTimeout::Unlimited), None)
                .is_err()
        );
        drop(call);
        assert_eq!(runtime.memory_stats().client.memory.unwrap().used_bytes, 0);
        assert_eq!(
            runtime.call_execution_snapshot().client.used_operations,
            Some(0)
        );
    }

    static TEST_ID: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NoopCallback;

    impl CrmCallback for NoopCallback {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: Vec<u8>,
            _response_pool: Arc<c2_server::ResponsePool>,
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

    fn dummy_route(server: &Arc<c2_server::Server>, name: &str) -> BuiltRoute {
        server
            .build_route(
                RouteBuildSpec {
                    name: name.to_string(),
                    crm_ns: "test.runtime".to_string(),
                    crm_name: "Runtime".to_string(),
                    crm_ver: "0.1.0".to_string(),
                    abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_string(),
                    signature_hash:
                        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                            .to_string(),
                    method_names: vec!["ping".to_string()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
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
    fn invalid_host_server_id_is_rejected_before_freeze() {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        for bad in ["", " ", "bad/name", "bad\\name", ".", "..", "bad\nid"] {
            let error = runtime
                .host(
                    crate::HostOptions::default()
                        .with_server_id(bad)
                        .without_relay(),
                )
                .expect_err("invalid server id should be rejected");
            assert!(error.to_string().contains("server_id"), "{error}");
            assert!(!runtime.server_config_frozen());
            assert!(runtime.server_id().is_none());
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
    fn relay_anchor_address_override_is_canonicalized() {
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
    fn concurrent_unregister_of_the_same_route_has_one_winner() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let route_name = unique_route_name("concurrent-unregister");
        let server = test_server("concurrent-unregister-server");
        let route = dummy_route(&server, &route_name);
        let handle = route.route_handle();
        let rt = Runtime::server_runtime().expect("runtime");
        let reservation = rt
            .block_on(server.reserve_route(route))
            .expect("reserve route");
        rt.block_on(server.commit_reserved_route(reservation))
            .expect("register route");

        // Keep both removals suspended after they have each reached the same
        // registered route. This fixes the interleaving without a sleep or a
        // scheduler-dependent race between two OS threads.
        let active = handle.blocking_acquire(0).expect("active route guard");
        let mut first = std::pin::pin!(server.unregister_route(&route_name));
        let mut second = std::pin::pin!(server.unregister_route(&route_name));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
        let second_initial = second.as_mut().poll(&mut context);
        assert!(!matches!(second_initial, Poll::Ready(true)));
        assert!(handle.snapshot().closed);

        drop(active);
        let (first_removed, second_removed) = match second_initial {
            Poll::Ready(removed) => (rt.block_on(first), removed),
            Poll::Pending => rt.block_on(async { tokio::join!(first, second) }),
        };
        assert_eq!(
            (first_removed, second_removed),
            (true, false),
            "the first removal must succeed and the second must report MissingRoute"
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
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-fail-server");
        session
            .register_route(&server, dummy_route(&server, &route_name), None, false)
            .expect("Core registration captures the concrete relay cleanup scope");

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
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-commit-server");
        let route = dummy_route(&server, &route_name);

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
            session.register_route(&server_for_thread, route, Some(&relay_url), false)
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
        check_relay_final_failure(FinalInterference::None, None);
    }

    #[test]
    fn old_relay_final_failure_preserves_core_registered_same_name_replacement() {
        check_relay_final_failure(FinalInterference::Replace, None);
    }

    #[test]
    fn unrelated_unregister_preserves_pending_registration_rollback() {
        check_relay_final_failure(FinalInterference::UnregisterOther, None);
    }

    #[test]
    fn canonical_relay_failure_keeps_error_and_rollback_receipt() {
        // Error contracts: "including code, name, message, and structured details."
        let error = c2_error::C2Error::new(c2_error::ErrorCode::ResourceAlreadyRegistered, "route already registered: grid")
            .with_details(std::collections::BTreeMap::from([
                ("name".into(), "grid".into()),
                ("registrations".into(), r#"[{"relay_id":"relay-a","relay_url":"http://relay-a:8080","registered_at":1000,"relay_status":"Alive"}]"#.into()),
            ]));
        check_relay_final_failure(FinalInterference::None, Some(error));
    }

    enum FinalInterference {
        None,
        Replace,
        UnregisterOther,
    }

    fn check_relay_final_failure(
        interference: FinalInterference,
        semantic_error: Option<c2_error::C2Error>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        fn read_request(stream: &mut std::net::TcpStream) -> String {
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(
                    reader.read_line(&mut line).unwrap(),
                    0,
                    "incomplete HTTP headers"
                );
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(std::str::from_utf8(&body).unwrap());
            request
        }

        let route_name = unique_route_name("relay-final-fail");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-final-fail-server");
        let route = dummy_route(&server, &route_name);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake relay");
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (final_seen_tx, final_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let expected_message = match &semantic_error {
            Some(error) => serde_json::to_string(&error.envelope()).unwrap(),
            None => "final failed".to_string(),
        };
        let final_response = format!(
            "HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            expected_message.len(),
            expected_message
        );
        let relay_thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, response) in [
                "HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string(),
                final_response,
                "HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 14\r\n\r\ncleanup failed".to_string(),
            ].into_iter().enumerate() {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline => {
                            thread::park_timeout(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept relay request {index}: {error}"),
                    }
                };
                requests.push(read_request(&mut stream));
                if index == 1 {
                    final_seen_tx.send(()).unwrap();
                    // The final response depends on an explicit release,
                    // never on the timing of the replacement registration.
                    release_rx.recv().expect("release final response");
                }
                stream
                    .write_all(response.as_bytes())
                    .expect("write relay response");
            }
            requests
        });

        let registering_session = session.clone();
        let registering_server = Arc::clone(&server);
        let register_thread = thread::spawn(move || {
            registering_session.register_route(&registering_server, route, Some(&relay_url), false)
        });
        let final_seen = final_seen_rx.recv_timeout(Duration::from_secs(5));
        let rt = Runtime::server_runtime().expect("runtime");
        let pending = rt.block_on(server.route_scheduler_snapshot(&route_name));
        let old_scope = session
            .capture_registration_scopes(&server)
            .remove(&route_name);
        let replacement = if matches!(interference, FinalInterference::Replace) {
            // Both removal and replacement go through Core's real transactions,
            // including the saved registration scope; no raw server insertion.
            let removal = session.unregister_route(&server, &route_name, None, false);
            let registration =
                session.register_route(&server, dummy_route(&server, &route_name), None, false);
            Some((removal, registration))
        } else {
            None
        };
        let sibling = if matches!(interference, FinalInterference::UnregisterOther) {
            let name = unique_route_name("unrelated");
            let registration =
                session.register_route(&server, dummy_route(&server, &name), None, false);
            let removal = session.unregister_route(&server, &name, None, false);
            Some((registration, removal))
        } else {
            None
        };
        let released = release_tx.send(());
        let result = register_thread.join();
        let requests = relay_thread.join();
        let remaining_identity = rt.block_on(server.registered_route_identity(&route_name));
        let remaining_scheduler = rt.block_on(server.route_scheduler_snapshot(&route_name));
        let remaining_scope = session
            .capture_registration_scopes(&server)
            .remove(&route_name);
        let shutdown = session.shutdown(
            Some(&server),
            vec![route_name.clone()],
            None,
            false,
            None,
            Duration::from_secs(5),
        );

        // Finish native cleanup and join both workers before the expected red.
        assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
        assert!(shutdown.route_close_error.is_none(), "{shutdown:?}");
        assert!(shutdown.relay_errors.is_empty(), "{shutdown:?}");
        final_seen.unwrap();
        released.unwrap();
        if let Some((registration, removal)) = sibling {
            assert!(registration.is_ok(), "{registration:?}");
            assert!(removal.unwrap().local_removed);
        }
        assert!(
            pending.unwrap().closed,
            "final publish must start with admission closed"
        );
        let requests = requests.unwrap();
        assert!(requests[0].contains("\"prepare_only\":true"));
        assert!(!requests[1].contains("\"prepare_only\":true"));
        assert!(requests[2].contains("/_unregister"));
        let cleanup_scope: RelayRegistrationScope =
            serde_json::from_str(requests[2].split_once("\r\n\r\n").unwrap().1).unwrap();
        let old_scope = old_scope.unwrap();
        assert_eq!(
            cleanup_scope, old_scope,
            "cleanup must target the failed registration"
        );

        let failure = match result.unwrap() {
            Err(LifecycleError::RegisterFailure(failure)) => failure,
            other => panic!("unexpected register result: {other:?}"),
        };
        assert_eq!(failure.route_name, route_name);
        assert_eq!(failure.failure_source, "relay_register");
        assert_eq!(failure.status_code, Some(409));
        assert_eq!(failure.error_message, expected_message);
        assert_eq!(failure.semantic_error, semantic_error);
        let rollback = failure
            .rollback
            .expect("rollback outcome should be preserved");
        assert_eq!(rollback.route_name, route_name);
        assert!(rollback.active_drained);
        assert_eq!(rollback.closed_reason, "registration_rollback");
        assert_eq!(rollback.close_error, None);
        let relay_cleanup_error = failure
            .relay_cleanup_error
            .expect("relay cleanup failure should be preserved");
        assert_eq!(relay_cleanup_error.route_name, route_name);
        assert_eq!(relay_cleanup_error.status_code, Some(409));
        assert_eq!(relay_cleanup_error.message, "cleanup failed");

        if let Some((removal, replacement)) = replacement {
            assert!(removal.unwrap().local_removed);
            let replacement = replacement.unwrap();
            let replacement_uid = remaining_scope
                .as_ref()
                .expect("replacement scope")
                .route_uid
                .clone();
            assert_ne!(replacement_uid, old_scope.route_uid);
            assert!(!replacement.relay_registered);
            assert_eq!(
                remaining_identity,
                Some(replacement_uid.clone()),
                "old final failure must preserve the Core-registered replacement"
            );
            assert!(
                !remaining_scheduler.unwrap().closed,
                "replacement must still admit work"
            );
            assert_eq!(
                remaining_scope,
                Some(RelayRegistrationScope {
                    name: replacement.route_name,
                    server_id: replacement.server_id,
                    server_instance_id: replacement.server_instance_id,
                    route_uid: replacement_uid,
                }),
                "old final failure must preserve the replacement's Core scope"
            );
            assert!(
                !rollback.local_removed,
                "rollback cannot report removal of the replacement"
            );
        } else {
            assert!(rollback.local_removed);
            assert!(
                remaining_identity.is_none(),
                "relay final failure must roll back its own route"
            );
            assert!(remaining_scheduler.is_none());
        }
    }

    #[test]
    fn relay_backed_registration_keeps_committed_route_closed_until_publish_finalizes() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;

        let route_name = unique_route_name("relay-publish-gate");
        let session = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .expect("session");
        let server = test_server("relay-publish-gate-server");
        let route = dummy_route(&server, &route_name);

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
            session.register_route(&server_for_thread, route, Some(&relay_url), false)
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
        let native_identity = rt
            .block_on(server.registered_route_identity(&route_name))
            .unwrap();
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
            observed_scope.route_uid, native_identity,
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
    fn shutdown_is_idempotent_and_retains_its_terminal_outcome() {
        let first = unique_route_name("shutdown-a");
        let second = unique_route_name("shutdown-b");
        let session = Runtime::new(RuntimeOptions {
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
        assert_eq!(second_outcome, first_outcome);
    }

    #[test]
    fn shutdown_closes_all_route_admissions_before_waiting_for_drain() {
        let first = unique_route_name("shutdown-close-a");
        let second = unique_route_name("shutdown-close-b");
        let session = Arc::new(
            Runtime::new(RuntimeOptions {
                use_process_relay_anchor: false,
                ..Default::default()
            })
            .expect("session"),
        );
        let server = test_server("shutdown-close-server");

        let first_route = dummy_route(&server, &first);
        let first_handle = first_route.route_handle();
        session
            .register_route(&server, first_route, None, false)
            .expect("first route should register");

        let second_route = dummy_route(&server, &second);
        let second_handle = second_route.route_handle();
        session
            .register_route(&server, second_route, None, false)
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
        let ack = c2_ipc::shutdown(server.local_endpoint().address(), Duration::from_secs(2))
            .expect("direct IPC shutdown should be acknowledged");
        assert_eq!(ack, c2_ipc::DirectIpcShutdownOutcome::Initiated);
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
        let directory = tempfile::Builder::new()
            .prefix("c2-journal-")
            .tempdir_in("/tmp")
            .unwrap();
        let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
            .unwrap()
            .endpoint(&format!(
                "ipc://{}",
                unique_route_name("direct-shutdown-cleanup-fail-server")
            ))
            .unwrap();
        let server = Arc::new(
            c2_server::Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap(),
        );
        let route = dummy_route(&server, &route_name);
        let handle = route.route_handle();
        session
            .register_route(&server, route, None, false)
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
        let ack = c2_ipc::shutdown_with_endpoint(server.local_endpoint(), Duration::from_secs(2))
            .expect("direct shutdown must be acknowledged");
        assert_eq!(ack, c2_ipc::DirectIpcShutdownOutcome::Initiated);
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
        assert!(!std::path::Path::new(server.local_endpoint().os_name()).exists());
        assert!(!lease_path.exists());
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
        assert!(!outcome.ipc_clients_drained);
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
        assert_eq!(repeated, outcome);
        assert_eq!(session.shutdown_observation(&server), Some(outcome));
    }

    #[test]
    fn failed_remote_acquire_freezes_only_its_client_domain() {
        let runtime = client_runtime(4096, 4);
        assert!(!runtime.memory_stats().client.frozen);
        assert!(runtime.client_domain_observer().is_none());

        let address = format!("ipc://{}", unique_route_name("absent-memory-domain"));
        assert!(
            runtime
                .acquire_ipc_client_with_deadline(&address, ConnectDeadline::default())
                .is_err(),
            "no server is listening on this address"
        );
        let stats = runtime.memory_stats();
        assert!(stats.client.frozen);
        assert!(!stats.server.frozen);
        let memory = stats.client.memory.unwrap();
        assert_eq!(memory.limit_bytes, 4096);
        assert_eq!(memory.used_bytes, 0);
        assert_eq!(memory.peak_bytes, 0);
        assert_eq!(memory.rejected_allocations, 0);
        assert_eq!(stats.client.file_spill.unwrap().limit_bytes, 0);
        let observed = runtime.client_domain_observer().unwrap().snapshot();
        assert_eq!(stats.client.domain_id, Some(observed.domain_id));
        assert_eq!(observed.operations.max_operations, 4);
    }

    #[test]
    fn retiring_one_runtime_domain_does_not_affect_another() {
        let runtime_a = client_runtime(1111, 1);
        let runtime_b = client_runtime(2222, 2);
        runtime_a.call_execution_context().unwrap();
        runtime_b.call_execution_context().unwrap();
        let a = runtime_a.client_domain_observer().unwrap();
        let b = runtime_b.memory_stats().client;
        assert_ne!(Some(a.snapshot().domain_id), b.domain_id);

        let deadline = Instant::now() + Duration::from_secs(2);
        let (outcome, completed) = runtime_a
            .begin_session_retirement(crate::RuntimeShutdownTarget::ClientsOnly, deadline)
            .unwrap()
            .observe_until(deadline);
        assert!(completed && outcome.ipc_clients_drained, "{outcome:?}");
        drop(runtime_a);
        let retired = a.snapshot();
        assert!(retired.closed);
        assert_eq!(retired.memory.limit_bytes, 1111);
        assert_eq!(retired.memory.used_bytes, 0);
        assert_eq!(runtime_b.memory_stats().client, b);
        assert!(!b.closed);
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::{Host, HostOptions};
    use std::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(5);

    fn hosted_runtime() -> (Runtime, Host, Option<tempfile::TempDir>) {
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        #[cfg(unix)]
        let root = {
            let root = tempfile::Builder::new()
                .prefix("c2-retire-")
                .tempdir_in("/tmp")
                .unwrap();
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions {
                        unix_root: Some(root.path().to_path_buf()),
                    },
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            Some(root)
        };
        #[cfg(not(unix))]
        let root = {
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions::default(),
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            None
        };
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        (runtime, host, root)
    }

    #[test]
    fn retirement_executor_and_spawn_failures_keep_owner_and_admission() {
        let (runtime, host, _root) = hosted_runtime();
        for flag in [&FORCE_SERVER_RUNTIME_FAILURE, &FORCE_SHUTDOWN_SPAWN_FAILURE] {
            flag.with(|flag| flag.set(true));
            let result = runtime.begin_session_retirement(
                RuntimeShutdownTarget::Host(&host),
                Instant::now() + WAIT,
            );
            flag.with(|flag| flag.set(false));
            assert!(matches!(result, Err(LifecycleError::Server(_))));
            assert!(
                runtime
                    .retirement_receipt(Instant::now())
                    .unwrap()
                    .is_none()
            );
            assert!(runtime.state.lock().shutdown_transactions.is_empty());
            runtime.ensure_active().unwrap();
            assert!(host.is_running());
            assert!(
                runtime
                    .ping_direct_ipc(host.local_endpoint().address(), WAIT)
                    .unwrap()
            );
        }
        assert!(host.shutdown().runtime_barrier_error.is_none());
    }

    #[test]
    fn zero_budget_contended_core_lock_does_not_publish_retirement() {
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        let state = runtime.state.lock();
        let result =
            runtime.begin_session_retirement(RuntimeShutdownTarget::ClientsOnly, Instant::now());
        assert!(matches!(
            result,
            Err(LifecycleError::ShutdownWaitTimeout {
                stage: "core_retirement_wait"
            })
        ));
        assert!(state.retirement.is_none());
        drop(state);
        runtime.ensure_active().unwrap();
    }

    #[test]
    fn contended_pool_seal_keeps_original_owner_and_does_not_spawn() {
        let (runtime, host, _root) = hosted_runtime();
        let _client = runtime
            .acquire_ipc_client_with_deadline(
                host.local_endpoint().address(),
                ConnectDeadline::default(),
            )
            .unwrap();
        let pool = runtime.state.lock().client_pool.clone().unwrap();
        let guard = pool.prepare_seal(Instant::now()).unwrap();
        let result =
            runtime.begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now());
        assert!(matches!(
            result,
            Err(LifecycleError::ShutdownWaitTimeout {
                stage: "core_pool_seal_wait"
            })
        ));
        assert!(runtime.state.lock().shutdown_transactions.is_empty());
        assert!(
            runtime
                .retirement_receipt(Instant::now())
                .unwrap()
                .is_none()
        );
        assert!(host.is_running());
        drop(guard);
        let receipt = runtime
            .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now() + WAIT)
            .unwrap();
        assert!(receipt.observe_until(Instant::now() + WAIT).1);
        assert!(!host.is_running());
    }

    #[test]
    fn pending_upgrade_and_terminal_publication_are_serialized_on_same_transaction() {
        for upgrade_wins in [true, false] {
            let (runtime, host, _root) = hosted_runtime();
            let (entered, observed) = mpsc::sync_channel(1);
            let (proceed, delayed) = mpsc::channel();
            runtime.state.lock().shutdown_publication_gate =
                Some(Arc::new(ShutdownPublicationGate {
                    entered,
                    proceed: Mutex::new(delayed),
                }));
            host.shutdown_with_timeout(Duration::ZERO);
            observed
                .recv_timeout(WAIT)
                .expect("real driver reached publication");
            let original = runtime
                .state
                .lock()
                .shutdown_transactions
                .values()
                .next()
                .unwrap()
                .clone();
            assert!(original.outcome.lock().outcome.is_none());
            let receipt = if upgrade_wins {
                let progress = original.outcome.lock();
                let contended = runtime
                    .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now());
                assert!(matches!(
                    contended,
                    Err(LifecycleError::ShutdownWaitTimeout {
                        stage: "core_shutdown_outcome_wait"
                    })
                ));
                assert!(progress.retirement.is_none());
                drop(progress);
                FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.set(true));
                let rejected = runtime.begin_session_retirement(
                    RuntimeShutdownTarget::Host(&host),
                    Instant::now() + WAIT,
                );
                FORCE_SERVER_RUNTIME_FAILURE.with(|flag| flag.set(false));
                assert!(rejected.is_err());
                assert!(
                    runtime
                        .retirement_receipt(Instant::now())
                        .unwrap()
                        .is_none()
                );
                assert!(original.outcome.lock().retirement.is_none());
                // Any attempt to spawn a second driver would fail this upgrade.
                FORCE_SHUTDOWN_SPAWN_FAILURE.with(|flag| flag.set(true));
                let receipt = runtime.begin_session_retirement(
                    RuntimeShutdownTarget::Host(&host),
                    Instant::now() + WAIT,
                );
                FORCE_SHUTDOWN_SPAWN_FAILURE.with(|flag| flag.set(false));
                let receipt = receipt.unwrap();
                assert!(Arc::ptr_eq(&receipt.transaction, &original));
                assert!(!receipt.observe_until(Instant::now()).1);
                proceed.send(()).unwrap();
                receipt
            } else {
                proceed.send(()).unwrap();
                let original_outcome = original.observe_until(Instant::now() + WAIT).unwrap();
                let server = host
                    .retirement_work(Instant::now() + WAIT)
                    .unwrap()
                    .server
                    .unwrap();
                let repeated = runtime.shutdown(Some(&server), Vec::new(), None, false, None, WAIT);
                assert_eq!(repeated, original_outcome);
                let receipt = runtime
                    .begin_session_retirement(
                        RuntimeShutdownTarget::Host(&host),
                        Instant::now() + WAIT,
                    )
                    .unwrap();
                assert!(!Arc::ptr_eq(&receipt.transaction, &original));
                receipt
            };
            let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
            assert!(completed && outcome.ipc_clients_drained, "{outcome:?}");
            assert!(!host.is_running());
            assert_eq!(receipt.terminal_outcome(), Some(outcome.clone()));
            let original_outcome = original.outcome.lock().outcome.clone().unwrap();
            if upgrade_wins {
                assert_eq!(original_outcome, outcome);
            } else {
                // A completed Host journal remains immutable. The later
                // retirement additionally drains the outgoing scope.
                assert!(!original_outcome.ipc_clients_drained);
                assert_eq!(host.shutdown_outcome(), Some(original_outcome.clone()));
                assert_eq!(
                    outcome,
                    ShutdownOutcome {
                        ipc_clients_drained: true,
                        ..original_outcome
                    }
                );
            }
        }
    }

    #[test]
    fn terminal_shutdown_error_with_live_server_requires_actual_native_completion() {
        let (runtime, host, _root) = hosted_runtime();
        let server = host
            .retirement_work(Instant::now() + WAIT)
            .unwrap()
            .server
            .unwrap();
        FORCE_SHUTDOWN_SPAWN_FAILURE.with(|flag| flag.set(true));
        let failed = runtime.begin_failed_start_shutdown(&server, Vec::new(), None, false, WAIT);
        FORCE_SHUTDOWN_SPAWN_FAILURE.with(|flag| flag.set(false));
        let historical = failed.terminal_outcome().unwrap();
        assert!(historical.runtime_barrier_error.is_some());
        assert!(server.lifecycle_state().is_running());
        let receipt = runtime
            .begin_session_retirement(RuntimeShutdownTarget::FailedStart(&failed), Instant::now())
            .unwrap();
        let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
        assert!(completed && outcome.ipc_clients_drained, "{outcome:?}");
        assert!(!server.lifecycle_state().is_running());
        assert_eq!(
            outcome.runtime_barrier_error,
            historical.runtime_barrier_error
        );
        assert_eq!(failed.terminal_outcome(), Some(historical));
    }

    #[test]
    fn failed_start_receipt_keeps_terminal_error_after_map_pruning() {
        let (runtime, host, _root) = hosted_runtime();
        let server = host
            .retirement_work(Instant::now() + WAIT)
            .unwrap()
            .server
            .unwrap();
        let failed = runtime.begin_failed_start_shutdown(&server, Vec::new(), None, false, WAIT);
        assert!(failed.observe_with_timeout(WAIT).1);
        failed.set_terminal_error_for_test("retained failed-start cleanup error");
        let historical = failed.terminal_outcome().unwrap();
        drop(host);
        drop(server);
        let other_host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let other_server = other_host
            .retirement_work(Instant::now() + WAIT)
            .unwrap()
            .server
            .unwrap();
        let other_failed =
            runtime.begin_failed_start_shutdown(&other_server, Vec::new(), None, false, WAIT);
        assert!(other_failed.observe_with_timeout(WAIT).1);
        drop(other_host);
        drop(other_server);
        // Remove only the optional index, after the real transaction completed.
        runtime.state.lock().shutdown_transactions.clear();
        assert!(failed.transaction.server.upgrade().is_none());
        let (entered, observed) = mpsc::sync_channel(1);
        let (proceed, delayed) = mpsc::channel();
        runtime.state.lock().shutdown_publication_gate = Some(Arc::new(ShutdownPublicationGate {
            entered,
            proceed: Mutex::new(delayed),
        }));
        let receipt = runtime
            .begin_session_retirement(
                RuntimeShutdownTarget::FailedStart(&failed),
                Instant::now() + WAIT,
            )
            .unwrap();
        observed.recv_timeout(WAIT).unwrap();
        assert!(!receipt.observe_until(Instant::now()).1);
        let again = runtime
            .begin_session_retirement(RuntimeShutdownTarget::FailedStart(&failed), Instant::now());
        // Always release the driver before asserting, including regression failure.
        proceed.send(()).unwrap();
        let again = again.unwrap();
        assert!(Arc::ptr_eq(&receipt.transaction, &again.transaction));
        assert!(matches!(
            runtime.begin_session_retirement(
                RuntimeShutdownTarget::FailedStart(&other_failed),
                Instant::now() + WAIT,
            ),
            Err(LifecycleError::Configuration(_))
        ));
        let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
        assert!(completed && outcome.ipc_clients_drained, "{outcome:?}");
        assert!(!historical.ipc_clients_drained);
        assert_eq!(
            outcome,
            ShutdownOutcome {
                ipc_clients_drained: true,
                ..historical.clone()
            }
        );
        assert_eq!(failed.terminal_outcome(), Some(historical));
    }
}
