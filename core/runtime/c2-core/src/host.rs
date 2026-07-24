use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract, MethodAccess,
};
use c2_error::{C2Error, ErrorCode};
use c2_server::{
    AccessLevel, ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
    RouteBuildSpec, SchedulerLimits, Server, ServerIdentity, ServerRuntimeBuilder,
};

use crate::outcome::RuntimeRouteSpec;
use crate::{Error, LifecycleError, RegisterOutcome, Runtime, ShutdownOutcome, UnregisterOutcome};

/// One generated method entry validated against an admitted release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDefinition {
    pub index: u16,
    pub name: &'static str,
    pub access: MethodAccess,
}

/// Opaque-byte service boundary implemented by generated host adapters.
pub trait EncodedService: Send + Sync + 'static {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error>;
}

/// A release-verified route definition ready for Core registration.
pub struct ServiceDefinition {
    release_ref: ContractReleaseRef,
    expected: ExpectedRouteContract,
    methods: Arc<[MethodDefinition]>,
    service: Arc<dyn EncodedService>,
}

impl fmt::Debug for ServiceDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceDefinition")
            .field("release_ref", &self.release_ref)
            .field("expected", &self.expected)
            .field("methods", &self.methods)
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
        })
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
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            relay: RelaySelection::Inherit,
            relay_use_proxy: None,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
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
}

impl HostInner {
    fn shutdown(&self) -> ShutdownOutcome {
        let _gate = self.shutdown_gate.lock();
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
            self.shutdown_timeout,
        );
        self.route_names.lock().clear();
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
        *self.shutdown_outcome.lock() = Some(outcome.clone());
        outcome
    }

    fn unregister(&self, route_name: &str) -> Result<UnregisterOutcome, Error> {
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
        })
    }

    pub fn shutdown(&self) -> ShutdownOutcome {
        self.inner.shutdown()
    }
}

/// One successful route registration with idempotent explicit and Drop
/// cleanup.
pub struct Registration {
    host: Arc<HostInner>,
    route_name: String,
    register_outcome: RegisterOutcome,
    close_outcome: Option<UnregisterOutcome>,
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
            Server::new_with_identity(
                &identity.ipc_address,
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
        let run_server = Arc::clone(&server);
        let thread = std::thread::Builder::new()
            .name(format!("c2-host-{}", server.server_id()))
            .spawn(move || {
                let result = server_runtime.block_on(run_server.run());
                if result.is_err() {
                    run_server.finalize_runtime_stopped();
                }
            })
            .map_err(|error| LifecycleError::Server(error.to_string()))?;
        let wait_runtime = Runtime::server_runtime()?;
        if let Err(error) =
            wait_runtime.block_on(server.wait_until_responsive(options.startup_timeout))
        {
            let _ = wait_runtime.block_on(server.shutdown_and_wait(options.shutdown_timeout));
            let _ = thread.join();
            return Err(LifecycleError::Server(error.to_string()).into());
        }

        Ok(Host {
            inner: Arc::new(HostInner {
                runtime: self.clone(),
                server,
                relay_anchor_address,
                relay_use_proxy,
                shutdown_timeout: options.shutdown_timeout,
                route_names: Mutex::new(HashSet::new()),
                thread: Mutex::new(Some(thread)),
                shutdown_outcome: Mutex::new(None),
                shutdown_gate: Mutex::new(()),
            }),
        })
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
    let limits = SchedulerLimits::default();
    let route_spec = RouteBuildSpec {
        name: route.route_name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
        method_names: method_names.clone(),
        access_map: access_map.clone(),
        concurrency_mode: ConcurrencyMode::ReadParallel,
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
        concurrency_mode: ConcurrencyMode::ReadParallel,
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
