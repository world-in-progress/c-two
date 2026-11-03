//! Synchronous IPC client — embeds a tokio runtime handle.
//!
//! Wraps [`IpcClient`] for blocking calls from SDK bindings.
//! Multiple `SyncClient` instances share a single tokio runtime.

use parking_lot::{Mutex, RwLock};
use std::sync::{Arc, OnceLock};

use c2_mem::MemPool;

use crate::client::{
    ClientIpcConfig, IpcCallControl, IpcClient, IpcError, MethodTable, RouteBinding,
};
use crate::response::ResponseData;

/// Whether an IPC call failure is proven to precede CRM dispatch.
///
/// This is intentionally a transport-owned fact. Core retry policy consumes
/// it directly and never infers dispatch safety from display text or an OS
/// error kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportPhase {
    PreDispatch,
    DispatchUncertain,
}

/// An IPC call failure paired with its authoritative dispatch phase.
#[derive(Debug)]
pub struct IpcCallError {
    phase: TransportPhase,
    source: IpcError,
}

impl IpcCallError {
    pub(crate) const fn new(phase: TransportPhase, source: IpcError) -> Self {
        Self { phase, source }
    }

    pub const fn phase(&self) -> TransportPhase {
        self.phase
    }

    pub const fn is_retry_safe(&self) -> bool {
        matches!(self.phase, TransportPhase::PreDispatch)
    }

    pub const fn source_error(&self) -> &IpcError {
        &self.source
    }

    pub fn into_source(self) -> IpcError {
        self.source
    }
}

impl std::fmt::Display for IpcCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "IPC call failed during {:?}: {}",
            self.phase, self.source
        )
    }
}

impl std::error::Error for IpcCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn call_error_phase(error: &IpcError) -> TransportPhase {
    match error {
        IpcError::LocalCallRejected(_)
        | IpcError::Config(_)
        | IpcError::Protocol(_)
        | IpcError::ProtocolVersionMismatch { .. }
        | IpcError::IdentityMismatch { .. }
        | IpcError::ContractMismatch(_)
        | IpcError::RouteNotFound(_)
        | IpcError::RouteRemoved { .. }
        | IpcError::RouteClosed { .. }
        | IpcError::RouteStale { .. }
        | IpcError::MethodNotFound { .. }
        | IpcError::Shm(_)
        | IpcError::Pool(_)
        | IpcError::PoolRetired => TransportPhase::PreDispatch,
        IpcError::ResponseFailure(_)
        | IpcError::Io(_)
        | IpcError::Connect(_)
        | IpcError::Decode(_)
        | IpcError::Chunk(_)
        | IpcError::CrmError(_)
        | IpcError::Closed => TransportPhase::DispatchUncertain,
    }
}

// ── Global shared runtime ────────────────────────────────────────────────

static GLOBAL_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static RUNTIME_INIT: Mutex<()> = Mutex::new(());

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("c2-client-io")
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
}

fn initialize_runtime<'a>(
    slot: &'a OnceLock<tokio::runtime::Runtime>,
    coordinator: &Mutex<()>,
    deadline: c2_config::ConnectDeadline,
) -> Result<&'a tokio::runtime::Runtime, IpcError> {
    crate::client::connect_check(deadline, "ipc_runtime_wait")?;
    if let Some(runtime) = slot.get() {
        return Ok(runtime);
    }
    // OnceLock::get_or_init has no timed wait. All initializers first claim
    // this gate, so a caller can leave without waiting for another builder.
    let _initializing = match deadline.instant() {
        Some(instant) => coordinator
            .try_lock_until(instant)
            .ok_or_else(|| crate::client::connect_expired("ipc_runtime_wait"))?,
        None => coordinator.lock(),
    };
    crate::client::connect_check(deadline, "ipc_runtime_wait")?;
    if let Some(runtime) = slot.get() {
        return Ok(runtime);
    }
    let runtime = build_runtime();
    if let Err(error) = crate::client::connect_check(deadline, "ipc_runtime_start") {
        runtime.shutdown_background();
        return Err(error);
    }
    slot.set(runtime)
        .expect("executor coordinator owns initialization");
    Ok(slot
        .get()
        .expect("executor was published by its coordinator"))
}

/// Share the client I/O executor with Core's native connection operations.
#[doc(hidden)]
pub fn shared_io_handle(
    deadline: c2_config::ConnectDeadline,
) -> Result<tokio::runtime::Handle, IpcError> {
    initialize_runtime(&GLOBAL_RUNTIME, &RUNTIME_INIT, deadline)
        .map(|runtime| runtime.handle().clone())
}

/// Return the shared tokio runtime, creating it on first call.
///
/// The runtime uses 2 worker threads — sufficient for client I/O.
fn get_or_create_runtime() -> &'static tokio::runtime::Runtime {
    initialize_runtime(
        &GLOBAL_RUNTIME,
        &RUNTIME_INIT,
        c2_config::ConnectDeadline::default(),
    )
    .expect("unbounded executor initialization cannot expire")
}

// ── SyncClient ───────────────────────────────────────────────────────────

/// Synchronous IPC client — embeds a tokio runtime handle.
///
/// Wraps `IpcClient` for blocking calls from SDK bindings.
/// Multiple SyncClients share a single tokio runtime.
pub struct SyncClient {
    inner: IpcClient,
    rt: tokio::runtime::Handle,
}

// Compile-time assertion: SyncClient must be Send+Sync for binding wrappers
// that may be shared across threads.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<SyncClient>();
        _assert_sync::<SyncClient>();
    }
};

impl SyncClient {
    /// Connect with a transport-internal pool (pooled-client acquire path).
    ///
    /// The pool was built from `config` by the caller inside this crate and is
    /// owned solely by the resulting client. `budget` is the owning cache's shared domain
    /// context: the pool already charges it and the client's reassembly pool
    /// charges the same context.
    pub(crate) fn connect_transport_pool_attempt_with_deadline(
        endpoint: c2_config::LocalEndpoint,
        pool: Arc<RwLock<MemPool>>,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
        deadline: c2_config::ConnectDeadline,
        event_source: Option<&c2_local::ConnectionEventSource>,
    ) -> Result<(Self, Result<(), IpcError>), IpcError> {
        let rt = get_or_create_runtime();
        let mut client = IpcClient::with_transport_pool(endpoint, pool, config, budget)?;
        if let Some(source) = event_source {
            client = client.with_connection_event_source(source.clone());
        }
        let result = rt.block_on(client.connect_with_deadline(deadline));
        Ok((
            Self {
                inner: client,
                rt: rt.handle().clone(),
            },
            result,
        ))
    }

    /// The native endpoint retained by this connection.
    pub fn local_endpoint(&self) -> &c2_config::LocalEndpoint {
        self.inner
            .local_endpoint()
            .expect("connected client has a valid endpoint")
    }

    /// Synchronous CRM call through an immutable route binding.
    pub fn call_bound(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        self.rt
            .block_on(self.call_bound_async(binding, method_name, data))
    }

    /// Synchronous CRM call with an explicit dispatch-safety phase on failure.
    pub fn call_bound_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcCallError> {
        self.rt
            .block_on(self.call_bound_async_phased(binding, method_name, data))
    }

    /// Await the actual IPC future on the caller's runtime. The receive task
    /// remains on the connection's shared runtime; no blocking bridge or
    /// nested `block_on` is used here.
    pub async fn call_bound_async(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        self.inner.call_bound(binding, method_name, data).await
    }

    pub async fn call_bound_async_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcCallError> {
        self.inner
            .call_bound_phased(binding, method_name, data)
            .await
    }

    /// Await a route-bound call with owner-supplied first-byte authority.
    /// The owner must retain this future after granting dispatch, even if its
    /// caller stops waiting, until send/reply ownership is settled.
    pub async fn call_bound_controlled_async(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        control: &IpcCallControl,
    ) -> Result<ResponseData, IpcError> {
        self.inner
            .call_bound_controlled(binding, method_name, data, control)
            .await
    }

    pub async fn call_bound_controlled_async_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        control: &IpcCallControl,
    ) -> Result<ResponseData, IpcCallError> {
        self.inner
            .call_bound_controlled_phased(binding, method_name, data, control)
            .await
    }

    /// The complete resolved client configuration this connection was created
    /// from. Used by the owning cache to reject a same-address hit whose
    /// requested policy differs from the cached connection's policy.
    pub(crate) fn config(&self) -> &ClientIpcConfig {
        self.inner.config()
    }

    /// Synchronous close.
    pub fn close(&mut self) {
        self.rt.block_on(self.inner.close_shared());
    }

    /// Synchronous shared-ownership close with a bounded close barrier.
    ///
    /// Usable through `Arc<SyncClient>` when unique ownership cannot be
    /// proven. The connection's driver owns outstanding frames and loans;
    /// copied responses retain no physical connection.
    /// False leaves the original driver settling its loans. A later observer
    /// sees that same physical close; no cleanup worker is started.
    pub fn close_shared(&self, timeout: std::time::Duration) -> bool {
        self.rt.block_on(self.inner.close_shared_bounded(timeout))
    }

    pub(crate) fn deactivate(&self) {
        self.inner.deactivate();
    }

    /// Whether the client is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Observe the existing physical connection without connecting/reconnecting.
    pub fn connection_observation(&self) -> Option<c2_local::ConnectionObservation> {
        self.inner.connection_observation()
    }

    /// Get the route table for a named route.
    pub fn route_table(&self, name: &str) -> Option<MethodTable> {
        self.inner.route_table(name)
    }

    /// Get all route names.
    pub fn route_names(&self) -> Vec<String> {
        self.inner.route_names()
    }

    /// Validate the connected route against an expected CRM contract.
    pub fn validate_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        self.inner.validate_route_contract(expected)
    }

    /// Authoritatively acquire and bind a route against an expected CRM contract.
    pub fn acquire_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteBinding, IpcError> {
        self.rt.block_on(self.inner.acquire_route(expected))
    }

    /// Authoritatively acquire one exact route token.
    pub fn acquire_route_token(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        route_uid: &str,
    ) -> Result<RouteBinding, IpcError> {
        self.rt
            .block_on(self.inner.acquire_route_token(expected, route_uid))
    }

    /// Acquire under the caller's unchanged absolute connection deadline.
    pub fn acquire_route_with_deadline(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<RouteBinding, IpcError> {
        self.rt
            .block_on(self.inner.acquire_route_with_deadline(expected, deadline))
    }

    pub fn acquire_route_token_with_deadline(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        route_uid: &str,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<RouteBinding, IpcError> {
        self.rt.block_on(
            self.inner
                .acquire_route_token_with_deadline(expected, route_uid, deadline),
        )
    }

    /// CRM tag advertised by a route, if present.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_len_for_test(&self) -> usize {
        self.inner.pending_len_for_test()
    }

    /// CRM tag advertised by a route, if present.
    pub fn route_contract(&self, route_name: &str) -> Option<c2_contract::ExpectedRouteContract> {
        self.inner.route_contract(route_name)
    }

    /// Identity announced by the connected IPC server handshake.
    pub fn server_identity(&self) -> Option<&c2_wire::handshake::ServerIdentity> {
        self.inner.server_identity()
    }

    /// Stable logical server ID announced by the connected IPC server.
    pub fn server_id(&self) -> Option<&str> {
        self.inner.server_id()
    }

    /// Per-server-incarnation ID announced by the connected IPC server.
    pub fn server_instance_id(&self) -> Option<&str> {
        self.inner.server_instance_id()
    }
}

// ── Test-only helpers ────────────────────────────────────────────────────

#[cfg(test)]
impl SyncClient {
    /// Create an unconnected `SyncClient` for pool bookkeeping tests.
    ///
    /// The resulting client is **not** connected to any server —
    /// `is_connected()` returns `false` and route-bound calls will fail.
    pub(crate) fn new_unconnected(address: &str) -> Self {
        let rt = get_or_create_runtime();
        let inner = IpcClient::with_config(address, ClientIpcConfig::default()).unwrap();
        Self {
            inner,
            rt: rt.handle().clone(),
        }
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn connect_deadline_executor_initialization_wait_is_bounded() {
        let slot = Arc::new(OnceLock::new());
        let coordinator = Arc::new(Mutex::new(()));
        let owner_slot = slot.clone();
        let owner_coordinator = coordinator.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _initializing = owner_coordinator.lock();
            owner_slot.get_or_init(|| {
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv();
                build_runtime()
            });
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let waiter_slot = slot.clone();
        let waiter_coordinator = coordinator.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let deadline = c2_config::ConnectDeadline::start(
                c2_config::ConnectOptions::new()
                    .with_timeout(std::time::Duration::from_millis(100)),
            )
            .unwrap();
            let result =
                initialize_runtime(&waiter_slot, &waiter_coordinator, deadline).map(|_| ());
            result_tx.send(result).unwrap();
        });
        // Observe expiry before allowing initialization to finish. The watchdog
        // tolerates scheduling delays without changing the 100 ms budget.
        let observed = result_rx.recv_timeout(std::time::Duration::from_secs(2));
        let unpublished = slot.get().is_none();
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        waiter.join().unwrap();
        assert!(
            observed.is_ok(),
            "executor initializer wait did not expire while the owner was held"
        );
        let result = observed.unwrap();
        assert!(matches!(result, Err(IpcError::LocalCallRejected(error))
            if error.details["stage"] == "ipc_runtime_wait"));
        assert!(
            unpublished,
            "deadline returned before the owner published its runtime"
        );
        let recovered =
            initialize_runtime(&slot, &coordinator, c2_config::ConnectDeadline::default()).unwrap();
        assert_eq!(recovered.handle().block_on(async { 7 }), 7);
    }

    /// Expose runtime pointer for cross-module test assertions.
    pub fn runtime_ptr() -> *const tokio::runtime::Runtime {
        get_or_create_runtime() as *const _
    }

    #[test]
    fn test_global_runtime_returns_same_instance() {
        let rt1 = get_or_create_runtime();
        let rt2 = get_or_create_runtime();
        // OnceLock guarantees the same pointer — verify via handle equality.
        let h1 = rt1.handle();
        let h2 = rt2.handle();
        // Both handles should be able to spawn; identity check via pointer.
        assert!(std::ptr::eq(rt1, rt2));
        // Extra: verify the handles are functional.
        let result = h1.block_on(async { 42 });
        assert_eq!(result, 42);
        let result2 = h2.block_on(async { 43 });
        assert_eq!(result2, 43);
    }

    #[test]
    fn call_phase_is_transport_owned_and_dispatch_uncertainty_is_not_retry_safe() {
        assert_eq!(
            call_error_phase(&IpcError::PoolRetired),
            TransportPhase::PreDispatch
        );
        let before_dispatch = IpcCallError::new(
            TransportPhase::PreDispatch,
            IpcError::MethodNotFound {
                route_name: "route".to_string(),
                method_name: "missing".to_string(),
            },
        );
        let uncertain = IpcCallError::new(TransportPhase::DispatchUncertain, IpcError::Closed);

        assert!(before_dispatch.is_retry_safe());
        assert!(!uncertain.is_retry_safe());
    }

    #[test]
    fn response_admission_is_dispatch_uncertain_and_not_retry_safe() {
        let budget = c2_mem::MemoryBudget::new(0, 0, 0);
        let pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix_and_budget(
                c2_mem::PoolConfig::default(),
                "phase-pure".to_string(),
                budget,
            )
            .unwrap(),
        ));
        let registry =
            c2_wire::chunk::ChunkRegistry::new(pool, c2_wire::chunk::ChunkConfig::default());
        let failure = registry.insert_reply(1, 1, 128, 2, 64).unwrap_err();
        let error = IpcError::Chunk(failure.into());
        let phase = call_error_phase(&error);
        assert_eq!(phase, TransportPhase::DispatchUncertain);
        assert!(!IpcCallError::new(phase, error).is_retry_safe());
    }

    #[test]
    fn sync_client_projects_server_identity() {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "identity-server".to_string(),
            server_instance_id: "identity-instance".to_string(),
        };
        let mut client = SyncClient::new_unconnected("ipc://identity_projection_sync");
        client.inner.server_identity = Some(identity.clone());

        assert_eq!(client.server_identity(), Some(&identity));
        assert_eq!(client.server_id(), Some("identity-server"));
        assert_eq!(client.server_instance_id(), Some("identity-instance"));
    }
}
