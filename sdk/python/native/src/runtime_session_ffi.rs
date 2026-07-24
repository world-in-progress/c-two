//! PyO3 bindings for the Rust-owned process runtime session.
//!
//! Phase 1 exposes server identity/address authority. Later phases will move
//! server lifecycle, route transactions, pools, and relay projection here.

use parking_lot::Mutex;
use pyo3::exceptions::{PyKeyError, PyLookupError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyDict, PyList};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use c2_contract::ExpectedRouteContract;
use c2_core::{
    RegisterFailureOutcome, RegisterOutcome, RelayCleanupError, RelayResolvedConnection,
    RouteCloseOutcome, Runtime, RuntimeOptions, RuntimeRouteSpec, ShutdownOutcome,
    UnregisterOutcome,
};
use c2_http::client::{RelayAwareHttpClient, RelayLocalIpcCandidate};
use c2_ipc::{ClientPool, IpcError, RouteBinding, SyncClient};
use c2_mem::BufferLeaseTracker;
use c2_server::AccessLevel;

use crate::client_ffi::{PyRustClient, PyRustClientPool, call_sync_client};
use crate::config_ffi::{
    client_ipc_overrides_to_dict, client_ipc_to_dict, parse_client_ipc_overrides,
    parse_server_ipc_overrides, server_ipc_overrides_to_dict,
};
use crate::http_ffi::{
    PyRustHttpClient, acquire_http_client_from_global_pool, c2_error_wire_bytes_from_http_body,
    call_relay_aware_http_client, http_client_refcount_from_global_pool,
    release_http_client_from_global_pool, shutdown_http_clients_from_global_pool,
};
use crate::lease_ffi::PyBufferLeaseTracker;
use crate::server_ffi::{PyServer, parse_concurrency_mode};

enum RelayConnectedInner {
    Ipc {
        client: Arc<SyncClient>,
        binding: RouteBinding,
        pool: &'static ClientPool,
    },
    Http {
        client: Arc<RelayAwareHttpClient>,
    },
}

enum RelayIpcConnectError {
    Config(PyErr),
    ContractMismatch(String),
    Unavailable(RelayIpcUnavailable),
}

#[derive(Debug)]
struct RelayIpcUnavailable {
    address: String,
    route_name: String,
    reason: RelayIpcUnavailableReason,
}

#[derive(Debug)]
enum RelayIpcUnavailableReason {
    PoolAcquire {
        error: String,
    },
    IdentityMismatch {
        expected_server_id: String,
        expected_server_instance_id: String,
        actual_server_id: Option<String>,
        actual_server_instance_id: Option<String>,
    },
    RouteMissing {
        route_name: String,
    },
    RouteRemoved {
        route_name: String,
        route_uid: Option<String>,
    },
    RouteClosed {
        route_name: String,
        route_uid: String,
        reason: String,
    },
    RouteStale {
        route_name: String,
        current_route_uid: String,
        current_route_revision: u64,
    },
    CatalogCompacted {
        compacted_revision: u64,
        current_revision: u64,
    },
    WatchUnavailable {
        error: String,
    },
}

impl RelayIpcUnavailable {
    fn pool_acquire(address: &str, route_name: &str, error: IpcError) -> Self {
        Self {
            address: address.to_string(),
            route_name: route_name.to_string(),
            reason: RelayIpcUnavailableReason::PoolAcquire {
                error: error.to_string(),
            },
        }
    }

    fn identity_mismatch(
        address: &str,
        route_name: &str,
        expected_server_id: &str,
        expected_server_instance_id: &str,
        actual_server_id: Option<String>,
        actual_server_instance_id: Option<String>,
    ) -> Self {
        Self {
            address: address.to_string(),
            route_name: route_name.to_string(),
            reason: RelayIpcUnavailableReason::IdentityMismatch {
                expected_server_id: expected_server_id.to_string(),
                expected_server_instance_id: expected_server_instance_id.to_string(),
                actual_server_id,
                actual_server_instance_id,
            },
        }
    }

    fn route_missing(address: &str, route_name: &str) -> Self {
        Self {
            address: address.to_string(),
            route_name: route_name.to_string(),
            reason: RelayIpcUnavailableReason::RouteMissing {
                route_name: route_name.to_string(),
            },
        }
    }

    fn from_route_state_error(address: &str, route_name: &str, error: IpcError) -> Self {
        let reason = match error {
            IpcError::RouteRemoved {
                route_name,
                route_uid,
            } => RelayIpcUnavailableReason::RouteRemoved {
                route_name,
                route_uid,
            },
            IpcError::RouteClosed {
                route_name,
                route_uid,
                reason,
            } => RelayIpcUnavailableReason::RouteClosed {
                route_name,
                route_uid,
                reason,
            },
            IpcError::RouteStale {
                route_name,
                current_route_uid,
                current_route_revision,
            } => RelayIpcUnavailableReason::RouteStale {
                route_name,
                current_route_uid,
                current_route_revision,
            },
            IpcError::CatalogCompacted {
                compacted_revision,
                current_revision,
            } => RelayIpcUnavailableReason::CatalogCompacted {
                compacted_revision,
                current_revision,
            },
            IpcError::WatchUnavailable(error) => {
                RelayIpcUnavailableReason::WatchUnavailable { error }
            }
            other => RelayIpcUnavailableReason::PoolAcquire {
                error: other.to_string(),
            },
        };
        Self {
            address: address.to_string(),
            route_name: route_name.to_string(),
            reason,
        }
    }
}

impl fmt::Display for RelayIpcUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "route '{}' at {} unavailable: {}",
            self.route_name, self.address, self.reason
        )
    }
}

impl fmt::Display for RelayIpcUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoolAcquire { error } => write!(f, "pool acquire failed: {error}"),
            Self::IdentityMismatch {
                expected_server_id,
                expected_server_instance_id,
                actual_server_id,
                actual_server_instance_id,
            } => write!(
                f,
                "identity mismatch: expected {expected_server_id}/{expected_server_instance_id}, got {}/{}",
                actual_server_id.as_deref().unwrap_or("<missing>"),
                actual_server_instance_id.as_deref().unwrap_or("<missing>")
            ),
            Self::RouteMissing { route_name } => write!(f, "route missing: {route_name}"),
            Self::RouteRemoved {
                route_name,
                route_uid,
            } => {
                if let Some(route_uid) = route_uid {
                    write!(f, "route removed: {route_name} uid={route_uid}")
                } else {
                    write!(f, "route removed: {route_name}")
                }
            }
            Self::RouteClosed {
                route_name,
                route_uid,
                reason,
            } => write!(
                f,
                "route closed: {route_name} uid={route_uid} reason={reason}"
            ),
            Self::RouteStale {
                route_name,
                current_route_uid,
                current_route_revision,
            } => write!(
                f,
                "route stale: {route_name} current_uid={current_route_uid} current_revision={current_route_revision}"
            ),
            Self::CatalogCompacted {
                compacted_revision,
                current_revision,
            } => write!(
                f,
                "route catalog compacted: compacted_revision={compacted_revision} current_revision={current_revision}"
            ),
            Self::WatchUnavailable { error } => write!(f, "route watch unavailable: {error}"),
        }
    }
}

impl RelayIpcUnavailableReason {
    fn kind(&self) -> &'static str {
        match self {
            Self::PoolAcquire { .. } => "pool_acquire",
            Self::IdentityMismatch { .. } => "identity_mismatch",
            Self::RouteMissing { .. } => "route_missing",
            Self::RouteRemoved { .. } => "route_removed",
            Self::RouteClosed { .. } => "route_closed",
            Self::RouteStale { .. } => "route_stale",
            Self::CatalogCompacted { .. } => "catalog_compacted",
            Self::WatchUnavailable { .. } => "watch_unavailable",
        }
    }
}

#[pyclass(name = "RelayConnectedClient", frozen)]
pub struct PyRelayConnectedClient {
    mode: String,
    target: String,
    route_name: String,
    inner: RelayConnectedInner,
    closed: Mutex<bool>,
}

impl PyRelayConnectedClient {
    fn close_inner(&self) {
        let mut closed = self.closed.lock();
        if *closed {
            return;
        }
        *closed = true;
        match &self.inner {
            RelayConnectedInner::Ipc { pool, .. } => pool.release(&self.target),
            RelayConnectedInner::Http { .. } => {}
        }
    }
}

impl Drop for PyRelayConnectedClient {
    fn drop(&mut self) {
        self.close_inner();
    }
}

#[pymethods]
impl PyRelayConnectedClient {
    #[getter]
    fn mode(&self) -> &str {
        &self.mode
    }

    #[getter]
    fn target(&self) -> &str {
        &self.target
    }

    #[getter]
    fn route_name(&self) -> &str {
        &self.route_name
    }

    fn call<'py>(&self, py: Python<'py>, method_name: &str, data: &[u8]) -> PyResult<Py<PyAny>> {
        if *self.closed.lock() {
            return Err(PyRuntimeError::new_err("relay connected client is closed"));
        }
        match &self.inner {
            RelayConnectedInner::Ipc {
                client, binding, ..
            } => {
                let response = call_sync_client(py, client, binding, method_name, data)?;
                Ok(Py::new(py, response)?.into_any())
            }
            RelayConnectedInner::Http { client } => {
                Ok(call_relay_aware_http_client(py, client, method_name, data)?
                    .into_any()
                    .unbind())
            }
        }
    }

    fn close(&self) {
        self.close_inner();
    }
}

#[pyclass(name = "RuntimeSession", frozen)]
pub struct PyRuntimeSession {
    inner: Runtime,
    lease_tracker: Arc<BufferLeaseTracker>,
    server_bridge: Mutex<Option<Py<PyAny>>>,
    pool: PyRustClientPool,
}

#[pymethods]
impl PyRuntimeSession {
    #[new]
    #[pyo3(signature = (server_id=None, server_ipc_overrides=None, client_ipc_overrides=None, shm_threshold=None, remote_payload_chunk_size=None, use_process_relay_anchor=true))]
    fn new(
        server_id: Option<String>,
        server_ipc_overrides: Option<&Bound<'_, PyAny>>,
        client_ipc_overrides: Option<&Bound<'_, PyAny>>,
        shm_threshold: Option<u64>,
        remote_payload_chunk_size: Option<u64>,
        use_process_relay_anchor: bool,
    ) -> PyResult<Self> {
        let server_ipc_overrides = match server_ipc_overrides {
            Some(overrides) => Some(parse_server_ipc_overrides(Some(overrides))?),
            None => None,
        };
        let client_ipc_overrides = match client_ipc_overrides {
            Some(overrides) => Some(parse_client_ipc_overrides(Some(overrides))?),
            None => None,
        };
        let inner = Runtime::new(RuntimeOptions {
            server_id,
            server_ipc_overrides,
            client_ipc_overrides,
            shm_threshold,
            remote_payload_chunk_size,
            relay_anchor_address: None,
            use_process_relay_anchor,
        })
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self {
            inner,
            lease_tracker: Arc::new(BufferLeaseTracker::default()),
            server_bridge: Mutex::new(None),
            pool: PyRustClientPool::global(),
        })
    }

    fn lease_tracker(&self) -> PyBufferLeaseTracker {
        PyBufferLeaseTracker::from_arc(Arc::clone(&self.lease_tracker))
    }

    fn hold_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        PyBufferLeaseTracker::from_arc(Arc::clone(&self.lease_tracker)).stats_dict(py)
    }

    fn sweep_hold_leases<'py>(
        &self,
        py: Python<'py>,
        threshold_seconds: f64,
    ) -> PyResult<Bound<'py, PyList>> {
        PyBufferLeaseTracker::from_arc(Arc::clone(&self.lease_tracker))
            .sweep_retained_list(py, threshold_seconds)
    }

    fn ensure_server<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let identity = self
            .inner
            .ensure_server()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let dict = PyDict::new(py);
        dict.set_item("server_id", identity.server_id)?;
        dict.set_item("server_instance_id", identity.server_instance_id)?;
        dict.set_item("ipc_address", identity.ipc_address)?;
        Ok(dict)
    }

    #[getter]
    fn server_id(&self) -> Option<String> {
        self.inner.server_id()
    }

    #[getter]
    fn server_id_override(&self) -> Option<String> {
        self.inner.server_id_override()
    }

    #[getter]
    fn server_address(&self) -> Option<String> {
        self.inner.server_address()
    }

    #[getter]
    fn server_ipc_overrides<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .server_ipc_overrides()
            .map(|overrides| server_ipc_overrides_to_dict(py, &overrides))
            .transpose()
    }

    fn set_server_options(
        &self,
        server_id: Option<String>,
        server_ipc_overrides: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let server_ipc_overrides = match server_ipc_overrides {
            Some(overrides) => Some(parse_server_ipc_overrides(Some(overrides))?),
            None => None,
        };
        self.inner
            .set_server_options(server_id, server_ipc_overrides)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.server_bridge.lock().take();
        Ok(())
    }

    fn set_relay_anchor_address(&self, relay_anchor_address: Option<String>) {
        self.inner.set_relay_anchor_address(relay_anchor_address);
    }

    #[getter]
    fn relay_anchor_address_override(&self) -> Option<String> {
        self.inner.relay_anchor_address_override()
    }

    #[getter]
    fn effective_relay_anchor_address(&self) -> PyResult<Option<String>> {
        self.inner
            .effective_relay_anchor_address()
            .map_err(runtime_error_to_py)
    }

    #[getter]
    fn remote_payload_chunk_size_override(&self) -> Option<u64> {
        self.inner.remote_payload_chunk_size_override()
    }

    #[getter]
    fn client_ipc_overrides<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .client_ipc_overrides()
            .map(|overrides| client_ipc_overrides_to_dict(py, &overrides))
            .transpose()
    }

    #[getter]
    fn client_ipc_config<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let mut runtime_overrides = c2_config::RuntimeConfigOverrides::default();
        runtime_overrides.client_ipc = self.inner.client_ipc_overrides().unwrap_or_default();
        runtime_overrides.shm_threshold = self.inner.shm_threshold_override();
        let resolved = c2_config::ConfigResolver::resolve_client_ipc(
            runtime_overrides.client_ipc.clone(),
            runtime_overrides,
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        client_ipc_to_dict(py, &resolved)
    }

    #[getter]
    fn client_config_frozen(&self) -> bool {
        self.inner.client_config_frozen()
    }

    fn set_client_ipc_overrides(&self, overrides: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let overrides = match overrides {
            Some(overrides) => Some(parse_client_ipc_overrides(Some(overrides))?),
            None => None,
        };
        match self.inner.set_client_ipc_overrides(overrides) {
            Ok(()) => Ok(true),
            Err(c2_core::LifecycleError::ClientConfigFrozen) => Ok(false),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    fn ensure_server_bridge<'py>(&self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        if let Some(server) = self.server_bridge.lock().as_ref() {
            return Ok(server.clone_ref(py));
        }

        let identity = self
            .inner
            .ensure_server()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let module = py.import("c_two.transport.server.native")?;
        let class = module.getattr("NativeServerBridge")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("bind_address", &identity.ipc_address)?;
        kwargs.set_item("server_id", &identity.server_id)?;
        kwargs.set_item("server_instance_id", &identity.server_instance_id)?;
        if let Some(overrides) = self.inner.server_ipc_overrides() {
            kwargs.set_item(
                "ipc_overrides",
                server_ipc_overrides_to_dict(py, &overrides)?,
            )?;
        }
        kwargs.set_item("lease_tracker", self.lease_tracker())?;
        let server = class.call((), Some(&kwargs))?;
        let server_obj = server.unbind();
        *self.server_bridge.lock() = Some(server_obj.clone_ref(py));
        Ok(server_obj)
    }

    #[pyo3(signature = (server_bridge, name, dispatcher, method_names, access_map, concurrency_mode, max_pending, max_workers, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, relay_anchor_address=None))]
    fn register_route<'py>(
        &self,
        py: Python<'py>,
        server_bridge: Py<PyAny>,
        name: &str,
        dispatcher: Py<PyAny>,
        method_names: Vec<String>,
        access_map: &Bound<'_, PyDict>,
        concurrency_mode: &str,
        max_pending: Option<usize>,
        max_workers: Option<usize>,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        relay_anchor_address: Option<&str>,
    ) -> PyResult<(
        Bound<'py, PyDict>,
        crate::route_concurrency_ffi::PyRouteConcurrency,
    )> {
        let server_ref = server_bridge.bind(py).extract::<PyRef<PyServer>>()?;
        let built = server_ref.build_route(
            py,
            name,
            dispatcher,
            method_names.clone(),
            access_map,
            concurrency_mode,
            max_pending,
            max_workers,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        )?;
        let mut native_access_map = std::collections::HashMap::new();
        for (key, value) in access_map.iter() {
            let idx: u16 = key.extract()?;
            let level_str: String = value.extract()?;
            let level = match level_str.as_str() {
                "read" => AccessLevel::Read,
                "write" => AccessLevel::Write,
                other => {
                    return Err(PyRuntimeError::new_err(format!(
                        "invalid access level '{other}', expected 'read' or 'write'"
                    )));
                }
            };
            native_access_map.insert(idx, level);
        }
        let spec = RuntimeRouteSpec {
            name: name.to_string(),
            crm_ns: crm_ns.to_string(),
            crm_name: crm_name.to_string(),
            crm_ver: crm_ver.to_string(),
            abi_hash: abi_hash.to_string(),
            signature_hash: signature_hash.to_string(),
            method_names,
            access_map: native_access_map,
            concurrency_mode: parse_concurrency_mode(concurrency_mode)?,
            max_pending,
            max_workers,
        };
        let relay_use_proxy = resolve_relay_use_proxy_if_needed(&self.inner, relay_anchor_address)?;
        let outcome = self
            .inner
            .register_route(
                &server_ref.inner,
                built.route,
                spec,
                relay_anchor_address,
                relay_use_proxy,
            )
            .map_err(runtime_error_to_py)?;
        Ok((register_outcome_to_dict(py, outcome)?, built.route_handle))
    }

    #[pyo3(signature = (server_bridge, name, relay_anchor_address=None))]
    fn unregister_route<'py>(
        &self,
        py: Python<'py>,
        server_bridge: Py<PyAny>,
        name: &str,
        relay_anchor_address: Option<&str>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let server_inner = {
            let server_ref = server_bridge
                .bind(py)
                .extract::<PyRef<PyServer>>()
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Arc::clone(&server_ref.inner)
        };
        let route_name = name.to_string();
        let relay_anchor_address = relay_anchor_address.map(str::to_owned);
        let relay_use_proxy =
            resolve_relay_use_proxy_if_needed(&self.inner, relay_anchor_address.as_deref())?;
        let outcome = py
            .detach(|| {
                self.inner.unregister_route(
                    &server_inner,
                    &route_name,
                    relay_anchor_address.as_deref(),
                    relay_use_proxy,
                )
            })
            .map_err(runtime_error_to_py)?;
        unregister_outcome_to_dict(py, outcome)
    }

    #[pyo3(signature = (server_bridge=None, route_names=None, relay_anchor_address=None, timeout_seconds=5.0))]
    fn shutdown<'py>(
        &self,
        py: Python<'py>,
        server_bridge: Option<Py<PyAny>>,
        route_names: Option<Vec<String>>,
        relay_anchor_address: Option<&str>,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        if !timeout_seconds.is_finite() || timeout_seconds < 0.0 {
            return Err(PyValueError::new_err(
                "timeout_seconds must be a non-negative finite number",
            ));
        }
        let timeout = Duration::from_secs_f64(timeout_seconds);
        let relay_anchor_address = relay_anchor_address.map(str::to_owned);
        let mut relay_cleanup_config_error = None;
        let relay_use_proxy = match resolve_relay_use_proxy_for_shutdown(
            &self.inner,
            relay_anchor_address.as_deref(),
        ) {
            Ok(value) => value,
            Err(message) => {
                relay_cleanup_config_error = Some(message);
                false
            }
        };
        let route_names = route_names.unwrap_or_default();
        let server_bridge = match server_bridge {
            Some(server_bridge) => Some(server_bridge),
            None => self
                .server_bridge
                .lock()
                .as_ref()
                .map(|server| server.clone_ref(py)),
        };
        let mut outcome = if let Some(server_bridge) = server_bridge {
            let server_inner = {
                let server_ref = server_bridge
                    .bind(py)
                    .extract::<PyRef<PyServer>>()
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                Arc::clone(&server_ref.inner)
            };
            let mut outcome = py.detach(|| {
                self.inner.shutdown(
                    Some(&server_inner),
                    route_names,
                    relay_anchor_address.as_deref(),
                    relay_use_proxy,
                    relay_cleanup_config_error,
                    timeout,
                )
            });
            let server_ref = server_bridge
                .bind(py)
                .extract::<PyRef<PyServer>>()
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            outcome.server_was_started = server_ref.runtime_is_running();
            if let Err(err) = server_ref.shutdown_runtime_barrier_blocking(py, timeout) {
                outcome.runtime_barrier_error = Some(err.to_string());
            }
            outcome
        } else {
            py.detach(|| {
                self.inner.shutdown(
                    None,
                    route_names,
                    relay_anchor_address.as_deref(),
                    relay_use_proxy,
                    relay_cleanup_config_error,
                    timeout,
                )
            })
        };
        py.detach(|| self.pool.inner.shutdown_all());
        outcome.ipc_clients_drained = true;
        py.detach(|| shutdown_http_clients_from_global_pool());
        outcome.http_clients_drained = true;
        self.server_bridge.lock().take();
        shutdown_outcome_to_dict(py, outcome)
    }

    fn clear_server_identity(&self) {
        self.inner.clear_server_identity();
        self.server_bridge.lock().take();
    }

    fn clear_relay_projection_cache(&self) {
        self.inner.clear_relay_projection_cache();
    }

    #[pyo3(signature = (address, route_name, expected_crm_ns, expected_crm_name, expected_crm_ver, expected_abi_hash, expected_signature_hash))]
    fn acquire_ipc_client(
        &self,
        py: Python<'_>,
        address: &str,
        route_name: &str,
        expected_crm_ns: &str,
        expected_crm_name: &str,
        expected_crm_ver: &str,
        expected_abi_hash: &str,
        expected_signature_hash: &str,
    ) -> PyResult<PyRustClient> {
        let addr = address.to_string();
        let pool = self.pool.inner;
        let mut runtime_overrides = c2_config::RuntimeConfigOverrides::default();
        runtime_overrides.client_ipc = self.inner.client_ipc_overrides().unwrap_or_default();
        runtime_overrides.shm_threshold = self.inner.shm_threshold_override();
        let cfg = c2_config::ConfigResolver::resolve_client_ipc(
            runtime_overrides.client_ipc.clone(),
            runtime_overrides,
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let client = py
            .detach(move || pool.acquire(&addr, Some(&cfg)))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?;
        let expected = expected_route_contract(
            route_name,
            expected_crm_ns,
            expected_crm_name,
            expected_crm_ver,
            expected_abi_hash,
            expected_signature_hash,
        )?;
        let binding_result = py.detach({
            let client = Arc::clone(&client);
            let expected = expected.clone();
            move || client.acquire_route(&expected)
        });
        let binding = match binding_result {
            Ok(binding) => binding,
            Err(err) => {
                self.pool.inner.release(address);
                return Err(pyo3::exceptions::PyRuntimeError::new_err(format!("{err}")));
            }
        };
        self.inner.mark_client_config_frozen();
        Ok(PyRustClient::from_arc_bound(client, binding))
    }

    fn release_ipc_client(&self, address: &str) {
        self.pool.inner.release(address);
    }

    fn ipc_client_refcount(&self, address: &str) -> usize {
        self.pool.inner.refcount(address)
    }

    fn acquire_http_client(&self, py: Python<'_>, address: &str) -> PyResult<PyRustHttpClient> {
        let addr = address.to_string();
        let remote_payload_chunk_size_override = self.inner.remote_payload_chunk_size_override();
        let client = py
            .detach(move || {
                let use_proxy = c2_config::ConfigResolver::resolve_relay_use_proxy(
                    c2_config::ConfigSources::from_process(),
                )
                .map_err(|e| c2_http::client::HttpError::Transport(e.to_string()))?;
                let call_timeout_secs = c2_config::ConfigResolver::resolve_relay_call_timeout_secs(
                    c2_config::ConfigSources::from_process(),
                )
                .map_err(|e| c2_http::client::HttpError::Transport(e.to_string()))?;
                let remote_payload_chunk_size =
                    c2_config::ConfigResolver::resolve_remote_payload_chunk_size(
                        remote_payload_chunk_size_override,
                        c2_config::ConfigSources::from_process(),
                    )
                    .map_err(|e| c2_http::client::HttpError::Transport(e.to_string()))?;
                acquire_http_client_from_global_pool(
                    &addr,
                    use_proxy,
                    call_timeout_secs,
                    remote_payload_chunk_size,
                )
            })
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?;
        Ok(PyRustHttpClient::from_arc(client))
    }

    #[pyo3(signature = (address, route_name, expected_crm_ns, expected_crm_name, expected_crm_ver, expected_abi_hash, expected_signature_hash))]
    fn connect_explicit_relay_http(
        &self,
        py: Python<'_>,
        address: &str,
        route_name: &str,
        expected_crm_ns: &str,
        expected_crm_name: &str,
        expected_crm_ver: &str,
        expected_abi_hash: &str,
        expected_signature_hash: &str,
    ) -> PyResult<PyRelayConnectedClient> {
        let expected = expected_route_contract(
            route_name,
            expected_crm_ns,
            expected_crm_name,
            expected_crm_ver,
            expected_abi_hash,
            expected_signature_hash,
        )?;
        let bound_route_name = expected.route_name.clone();
        let use_proxy = resolve_relay_use_proxy_if_needed(&self.inner, Some(address))?;
        let max_attempts = c2_config::ConfigResolver::resolve_relay_route_max_attempts(
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let call_timeout_secs = c2_config::ConfigResolver::resolve_relay_call_timeout_secs(
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let remote_payload_chunk_size = resolve_remote_payload_chunk_size_for_session(&self.inner)?;
        let (client, relay_url) = py
            .detach(move || {
                self.inner.connect_explicit_relay_http_client(
                    address,
                    expected,
                    use_proxy,
                    max_attempts,
                    call_timeout_secs,
                    remote_payload_chunk_size,
                )
            })
            .map_err(runtime_error_to_py)?;
        Ok(PyRelayConnectedClient {
            mode: "http".to_string(),
            target: relay_url,
            route_name: bound_route_name,
            inner: RelayConnectedInner::Http {
                client: Arc::new(client),
            },
            closed: Mutex::new(false),
        })
    }

    fn release_http_client(&self, address: &str) {
        release_http_client_from_global_pool(address);
    }

    fn http_client_refcount(&self, address: &str) -> usize {
        http_client_refcount_from_global_pool(address)
    }

    fn shutdown_http_clients(&self, py: Python<'_>) {
        py.detach(|| shutdown_http_clients_from_global_pool());
    }

    #[pyo3(signature = (route_name, expected_crm_ns, expected_crm_name, expected_crm_ver, expected_abi_hash, expected_signature_hash))]
    fn connect_via_relay(
        &self,
        py: Python<'_>,
        route_name: &str,
        expected_crm_ns: &str,
        expected_crm_name: &str,
        expected_crm_ver: &str,
        expected_abi_hash: &str,
        expected_signature_hash: &str,
    ) -> PyResult<PyRelayConnectedClient> {
        let expected = expected_route_contract(
            route_name,
            expected_crm_ns,
            expected_crm_name,
            expected_crm_ver,
            expected_abi_hash,
            expected_signature_hash,
        )?;
        let relay_anchor_address = self
            .inner
            .effective_relay_anchor_address()
            .map_err(runtime_error_to_py)?
            .ok_or_else(|| PyLookupError::new_err("no relay address configured"))?;
        let use_proxy =
            resolve_relay_use_proxy_if_needed(&self.inner, Some(relay_anchor_address.as_str()))?;
        let max_attempts = c2_config::ConfigResolver::resolve_relay_route_max_attempts(
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let call_timeout_secs = c2_config::ConfigResolver::resolve_relay_call_timeout_secs(
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let remote_payload_chunk_size = resolve_remote_payload_chunk_size_for_session(&self.inner)?;

        let expected_for_resolve = expected.clone();
        let target = py
            .detach(move || {
                self.inner.resolve_relay_connection(
                    expected_for_resolve,
                    use_proxy,
                    max_attempts,
                    call_timeout_secs,
                    remote_payload_chunk_size,
                )
            })
            .map_err(runtime_error_to_py)?;
        let mut target = target;
        let mut failed_local_ipc_candidates = Vec::new();
        loop {
            match target {
                RelayResolvedConnection::Ipc {
                    client: relay_client,
                    candidate,
                } => match self.acquire_relay_ipc_client(py, &candidate, &expected) {
                    Ok((client, binding)) => {
                        return Ok(PyRelayConnectedClient {
                            mode: "ipc".to_string(),
                            target: candidate.address,
                            route_name: expected.route_name.clone(),
                            inner: RelayConnectedInner::Ipc {
                                client,
                                binding,
                                pool: self.pool.inner,
                            },
                            closed: Mutex::new(false),
                        });
                    }
                    Err(RelayIpcConnectError::Config(err)) => return Err(err),
                    Err(RelayIpcConnectError::ContractMismatch(message)) => {
                        return Err(relay_ipc_contract_mismatch_to_py(&expected, &message));
                    }
                    Err(RelayIpcConnectError::Unavailable(reason)) => {
                        eprintln!(
                            "[c-two] Relay-resolved local IPC acquire failed; fallback denied unless a distinct route is available: {reason}"
                        );
                        let failed_candidate = candidate.clone();
                        failed_local_ipc_candidates.push(candidate);
                        let failed = failed_local_ipc_candidates.clone();
                        let next_target = match py.detach(move || {
                            Runtime::resolve_relay_connection_after_local_ipc_failures(
                                relay_client,
                                &failed,
                            )
                        }) {
                            Ok(target) => target,
                            Err(err) if runtime_error_is_fallback_denied(&err) => {
                                return Err(relay_ipc_fallback_denied_to_py(
                                    &reason,
                                    &failed_candidate,
                                ));
                            }
                            Err(err) => return Err(runtime_error_to_py(err)),
                        };
                        target = next_target;
                        continue;
                    }
                },
                RelayResolvedConnection::Http { client, relay_url } => {
                    return Ok(PyRelayConnectedClient {
                        mode: "http".to_string(),
                        target: relay_url,
                        route_name: expected.route_name.clone(),
                        inner: RelayConnectedInner::Http {
                            client: Arc::new(client),
                        },
                        closed: Mutex::new(false),
                    });
                }
            }
        }
    }

    fn shutdown_ipc_clients(&self, py: Python<'_>) {
        py.detach(|| self.pool.inner.shutdown_all());
    }
}

impl PyRuntimeSession {
    fn acquire_relay_ipc_client(
        &self,
        py: Python<'_>,
        candidate: &RelayLocalIpcCandidate,
        expected: &ExpectedRouteContract,
    ) -> Result<(Arc<SyncClient>, RouteBinding), RelayIpcConnectError> {
        let addr = candidate.address.clone();
        let pool = self.pool.inner;
        let mut runtime_overrides = c2_config::RuntimeConfigOverrides::default();
        runtime_overrides.client_ipc = self.inner.client_ipc_overrides().unwrap_or_default();
        runtime_overrides.shm_threshold = self.inner.shm_threshold_override();
        let cfg = c2_config::ConfigResolver::resolve_client_ipc(
            runtime_overrides.client_ipc.clone(),
            runtime_overrides,
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|e| RelayIpcConnectError::Config(PyValueError::new_err(e.to_string())))?;
        let client_result = py.detach({
            let addr = addr.clone();
            move || pool.acquire(&addr, Some(&cfg))
        });
        let client = match client_result {
            Ok(client) => client,
            Err(IpcError::Config(message)) => {
                return Err(RelayIpcConnectError::Config(PyValueError::new_err(message)));
            }
            Err(err) => {
                return Err(RelayIpcConnectError::Unavailable(
                    RelayIpcUnavailable::pool_acquire(&addr, &expected.route_name, err),
                ));
            }
        };

        let actual_identity = client.server_identity();
        let identity_matches = actual_identity.as_ref().is_some_and(|identity| {
            identity.server_id == candidate.server_id
                && identity.server_instance_id == candidate.server_instance_id
        });
        if !identity_matches {
            pool.release(&addr);
            return Err(RelayIpcConnectError::Unavailable(
                RelayIpcUnavailable::identity_mismatch(
                    &addr,
                    &expected.route_name,
                    &candidate.server_id,
                    &candidate.server_instance_id,
                    actual_identity
                        .as_ref()
                        .map(|identity| identity.server_id.clone()),
                    actual_identity
                        .as_ref()
                        .map(|identity| identity.server_instance_id.clone()),
                ),
            ));
        }

        let binding_result = py.detach({
            let client = Arc::clone(&client);
            let expected = expected.clone();
            let route_uid = candidate.route_uid.clone();
            let route_revision = candidate.route_revision;
            move || client.acquire_route_token(&expected, &route_uid, route_revision)
        });
        let binding = match binding_result {
            Ok(binding) => binding,
            Err(err) => {
                pool.release(&addr);
                return match err {
                    IpcError::RouteNotFound(_) => Err(RelayIpcConnectError::Unavailable(
                        RelayIpcUnavailable::route_missing(&addr, &expected.route_name),
                    )),
                    IpcError::ContractMismatch(message) => {
                        Err(RelayIpcConnectError::ContractMismatch(message))
                    }
                    IpcError::RouteRemoved { .. }
                    | IpcError::RouteClosed { .. }
                    | IpcError::RouteStale { .. }
                    | IpcError::CatalogCompacted { .. }
                    | IpcError::WatchUnavailable(_) => Err(RelayIpcConnectError::Unavailable(
                        RelayIpcUnavailable::from_route_state_error(
                            &addr,
                            &expected.route_name,
                            err,
                        ),
                    )),
                    other => Err(RelayIpcConnectError::Unavailable(
                        RelayIpcUnavailable::pool_acquire(&addr, &expected.route_name, other),
                    )),
                };
            }
        };
        self.inner.mark_client_config_frozen();
        Ok((client, binding))
    }
}

fn expected_route_contract(
    route_name: &str,
    crm_ns: &str,
    crm_name: &str,
    crm_ver: &str,
    abi_hash: &str,
    signature_hash: &str,
) -> PyResult<ExpectedRouteContract> {
    let expected = ExpectedRouteContract {
        route_name: route_name.to_string(),
        crm_ns: crm_ns.to_string(),
        crm_name: crm_name.to_string(),
        crm_ver: crm_ver.to_string(),
        abi_hash: abi_hash.to_string(),
        signature_hash: signature_hash.to_string(),
    };
    c2_contract::validate_expected_route_contract(&expected)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    Ok(expected)
}

fn runtime_error_to_py(err: c2_core::LifecycleError) -> PyErr {
    match err {
        c2_core::LifecycleError::InvalidServerId(message) => PyValueError::new_err(message),
        c2_core::LifecycleError::ClientConfigFrozen => PyValueError::new_err(err.to_string()),
        c2_core::LifecycleError::DuplicateRoute(message) => {
            let exc = PyRuntimeError::new_err(message);
            Python::attach(|py| {
                exc.value(py).setattr("status_code", 409).ok();
            });
            exc
        }
        c2_core::LifecycleError::MissingRoute(message) => PyKeyError::new_err(message),
        c2_core::LifecycleError::MissingRelayAddress => PyLookupError::new_err(err.to_string()),
        c2_core::LifecycleError::RelayDuplicateRoute(message) => {
            let exc = PyRuntimeError::new_err(message);
            Python::attach(|py| {
                let value = exc.value(py);
                value.setattr("status_code", 409).ok();
                value.setattr("relay_duplicate", true).ok();
            });
            exc
        }
        c2_core::LifecycleError::RelayHttp {
            status_code,
            message,
        } => {
            let exc = PyRuntimeError::new_err(format!("HTTP {status_code}: {message}"));
            Python::attach(|py| {
                let value = exc.value(py);
                value.setattr("status_code", status_code).ok();
                value.setattr("body", message.clone()).ok();
                let error_bytes = c2_error_wire_bytes_from_http_body(&message);
                value
                    .setattr("error_bytes", PyBytes::new(py, &error_bytes))
                    .ok();
            });
            exc
        }
        c2_core::LifecycleError::RegisterFailure(failure) => {
            let exc = PyRuntimeError::new_err(format!(
                "registration failed at {}: {}",
                failure.failure_source, failure.error_message
            ));
            Python::attach(|py| {
                let value = exc.value(py);
                if let Ok(dict) = register_failure_to_dict(py, (*failure).clone()) {
                    value.setattr("registration_failure", dict.clone()).ok();
                    match dict.get_item("rollback") {
                        Ok(Some(rollback)) => value.setattr("rollback", rollback).ok(),
                        _ => value.setattr("rollback", py.None()).ok(),
                    };
                    match dict.get_item("relay_cleanup_error") {
                        Ok(Some(error)) => value.setattr("relay_cleanup_error", error).ok(),
                        _ => value.setattr("relay_cleanup_error", py.None()).ok(),
                    };
                }
                if let Some(status_code) = failure.status_code {
                    value.setattr("status_code", status_code).ok();
                    if status_code == 409 {
                        value.setattr("relay_duplicate", true).ok();
                    }
                }
            });
            exc
        }
        c2_core::LifecycleError::Server(message) => PyRuntimeError::new_err(message),
        c2_core::LifecycleError::Relay(message) => {
            PyRuntimeError::new_err(format!("relay error: {message}"))
        }
    }
}

fn runtime_error_is_fallback_denied(err: &c2_core::LifecycleError) -> bool {
    let c2_core::LifecycleError::RelayHttp { message, .. } = err else {
        return false;
    };
    let error_bytes = c2_error_wire_bytes_from_http_body(message);
    c2_error::C2Error::from_wire_bytes(&error_bytes)
        .ok()
        .flatten()
        .is_some_and(|error| error.code == c2_error::ErrorCode::FallbackDenied)
}

fn relay_ipc_contract_mismatch_to_py(expected: &ExpectedRouteContract, reason: &str) -> PyErr {
    let message = format!(
        "CRM contract mismatch for relay-resolved IPC route '{}': {reason}",
        expected.route_name
    );
    let mut details = BTreeMap::new();
    details.insert("route".to_string(), expected.route_name.clone());
    details.insert("crm_ns".to_string(), expected.crm_ns.clone());
    details.insert("crm_name".to_string(), expected.crm_name.clone());
    details.insert("crm_ver".to_string(), expected.crm_ver.clone());
    details.insert("abi_hash".to_string(), expected.abi_hash.clone());
    details.insert(
        "signature_hash".to_string(),
        expected.signature_hash.clone(),
    );
    details.insert("direct_ipc_failure".to_string(), reason.to_string());
    let error = c2_error::C2Error::new(c2_error::ErrorCode::ContractMismatch, message.clone())
        .with_details(details);
    let error_bytes = error.to_wire_bytes();
    let exc = PyRuntimeError::new_err(message);
    Python::attach(|py| {
        exc.value(py)
            .setattr("error_bytes", PyBytes::new(py, &error_bytes))
            .ok();
    });
    exc
}

fn relay_ipc_fallback_denied_to_py(
    reason: &RelayIpcUnavailable,
    failed_candidate: &RelayLocalIpcCandidate,
) -> PyErr {
    let message = format!(
        "same local relay HTTP fallback denied for route '{}' after direct IPC acquire failed",
        reason.route_name
    );
    let mut details = BTreeMap::new();
    details.insert("route".to_string(), reason.route_name.clone());
    details.insert("ipc_address".to_string(), reason.address.clone());
    details.insert("server_id".to_string(), failed_candidate.server_id.clone());
    details.insert(
        "server_instance_id".to_string(),
        failed_candidate.server_instance_id.clone(),
    );
    details.insert("route_uid".to_string(), failed_candidate.route_uid.clone());
    details.insert(
        "route_revision".to_string(),
        failed_candidate.route_revision.to_string(),
    );
    details.insert(
        "direct_ipc_failure_kind".to_string(),
        reason.reason.kind().to_string(),
    );
    details.insert("direct_ipc_failure".to_string(), reason.to_string());
    let error = c2_error::C2Error::new(c2_error::ErrorCode::FallbackDenied, message.clone())
        .with_details(details);
    let error_bytes = error.to_wire_bytes();
    let exc = PyRuntimeError::new_err(message);
    Python::attach(|py| {
        let value = exc.value(py);
        value.setattr("status_code", 409).ok();
        value
            .setattr("error_bytes", PyBytes::new(py, &error_bytes))
            .ok();
    });
    exc
}

fn resolve_relay_use_proxy_if_needed(
    session: &Runtime,
    relay_anchor_address: Option<&str>,
) -> PyResult<bool> {
    let has_relay = match relay_anchor_address {
        Some(_) => true,
        None => session
            .effective_relay_anchor_address()
            .map_err(runtime_error_to_py)?
            .is_some(),
    };
    if !has_relay {
        return Ok(false);
    }
    c2_config::ConfigResolver::resolve_relay_use_proxy(c2_config::ConfigSources::from_process())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

fn resolve_remote_payload_chunk_size_for_session(session: &Runtime) -> PyResult<u64> {
    c2_config::ConfigResolver::resolve_remote_payload_chunk_size(
        session.remote_payload_chunk_size_override(),
        c2_config::ConfigSources::from_process(),
    )
    .map_err(|e| PyValueError::new_err(e.to_string()))
}

fn resolve_relay_use_proxy_for_shutdown(
    session: &Runtime,
    relay_anchor_address: Option<&str>,
) -> Result<bool, String> {
    let has_relay = match relay_anchor_address {
        Some(_) => true,
        None => match session.effective_relay_anchor_address() {
            Ok(Some(_)) => true,
            Ok(None) => return Ok(false),
            Err(_) => return Ok(false),
        },
    };
    if !has_relay {
        return Ok(false);
    }
    c2_config::ConfigResolver::resolve_relay_use_proxy(c2_config::ConfigSources::from_process())
        .map_err(|e| e.to_string())
}

fn register_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: RegisterOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("server_id", outcome.server_id)?;
    dict.set_item("server_instance_id", outcome.server_instance_id)?;
    dict.set_item("ipc_address", outcome.ipc_address)?;
    dict.set_item("relay_registered", outcome.relay_registered)?;
    Ok(dict)
}

fn register_failure_to_dict<'py>(
    py: Python<'py>,
    outcome: RegisterFailureOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("failure_source", outcome.failure_source)?;
    dict.set_item("error_message", outcome.error_message)?;
    dict.set_item("status_code", outcome.status_code)?;
    match outcome.rollback {
        Some(rollback) => dict.set_item("rollback", route_close_outcome_to_dict(py, rollback)?)?,
        None => dict.set_item("rollback", py.None())?,
    }
    match outcome.relay_cleanup_error {
        Some(error) => dict.set_item(
            "relay_cleanup_error",
            relay_cleanup_error_to_dict(py, error)?,
        )?,
        None => dict.set_item("relay_cleanup_error", py.None())?,
    }
    Ok(dict)
}

fn relay_cleanup_error_to_dict<'py>(
    py: Python<'py>,
    error: RelayCleanupError,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", error.route_name)?;
    dict.set_item("status_code", error.status_code)?;
    dict.set_item("message", error.message)?;
    Ok(dict)
}

fn route_close_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: RouteCloseOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("local_removed", outcome.local_removed)?;
    dict.set_item("active_drained", outcome.active_drained)?;
    dict.set_item("closed_reason", outcome.closed_reason)?;
    dict.set_item("close_error", outcome.close_error)?;
    Ok(dict)
}

fn unregister_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: UnregisterOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("local_removed", outcome.local_removed)?;
    dict.set_item("close", route_close_outcome_to_dict(py, outcome.close)?)?;
    match outcome.relay_error {
        Some(error) => dict.set_item("relay_error", relay_cleanup_error_to_dict(py, error)?)?,
        None => dict.set_item("relay_error", py.None())?,
    }
    Ok(dict)
}

fn shutdown_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: ShutdownOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("removed_routes", outcome.removed_routes)?;
    let route_outcomes = outcome
        .route_outcomes
        .into_iter()
        .map(|close| route_close_outcome_to_dict(py, close).map(|dict| dict.unbind()))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("route_outcomes", PyList::new(py, route_outcomes)?)?;
    let relay_errors = outcome
        .relay_errors
        .into_iter()
        .map(|error| relay_cleanup_error_to_dict(py, error).map(|dict| dict.unbind()))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("relay_errors", PyList::new(py, relay_errors)?)?;
    dict.set_item("server_was_started", outcome.server_was_started)?;
    dict.set_item("ipc_clients_drained", outcome.ipc_clients_drained)?;
    dict.set_item("http_clients_drained", outcome.http_clients_drained)?;
    dict.set_item("route_close_error", outcome.route_close_error)?;
    dict.set_item("runtime_barrier_error", outcome.runtime_barrier_error)?;
    Ok(dict)
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRelayConnectedClient>()?;
    m.add_class::<PyRuntimeSession>()?;
    Ok(())
}
