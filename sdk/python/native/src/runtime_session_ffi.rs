//! PyO3 projection of the language-neutral C-Two Core runtime.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};

use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract, MethodAccess,
    PORTABLE_CONTRACT_SCHEMA, contract_descriptor_sha256_hex,
};
use c2_core::{
    Connect, Host, HostOptions, MethodDefinition, RegisterOutcome, Registration, RelayCleanupError,
    RouteCloseOutcome, Runtime, RuntimeOptions, ServiceConcurrencyMode, ServiceDefinition,
    ShutdownOutcome, UnregisterOutcome,
};
use c2_mem::BufferLeaseTracker;

use crate::config_ffi::{
    client_ipc_overrides_to_dict, client_ipc_to_dict, parse_client_ipc_overrides,
    parse_server_ipc_overrides, server_ipc_overrides_to_dict,
};
use crate::core_error_ffi::{core_error_to_py, lifecycle_error_to_py};
use crate::core_ffi::{PyCoreClient, PyCoreService};
use crate::lease_ffi::PyBufferLeaseTracker;
use crate::route_concurrency_ffi::PyRouteConcurrency;

#[pyclass(name = "RuntimeSession", frozen)]
pub struct PyRuntimeSession {
    inner: Arc<Runtime>,
    lease_tracker: Arc<BufferLeaseTracker>,
    host: Mutex<Option<Host>>,
    registrations: Mutex<HashMap<String, Registration>>,
    server_bridge: Mutex<Option<Py<PyAny>>>,
}

impl PyRuntimeSession {
    fn ensure_host(&self) -> PyResult<Host> {
        if let Some(host) = self.host.lock().as_ref() {
            return Ok(host.clone());
        }
        let host = self
            .inner
            .host(HostOptions::default())
            .map_err(core_error_to_py)?;
        *self.host.lock() = Some(host.clone());
        Ok(host)
    }

    fn connect_core(
        &self,
        py: Python<'_>,
        expected: ExpectedRouteContract,
        mode: Connect,
    ) -> PyResult<PyCoreClient> {
        py.detach(|| self.inner.connect(expected, mode))
            .map(PyCoreClient::new)
            .map_err(core_error_to_py)
    }
}

#[pymethods]
impl PyRuntimeSession {
    #[new]
    #[pyo3(signature = (server_id=None, server_ipc_overrides=None, client_ipc_overrides=None, shm_threshold=None, remote_payload_chunk_size=None, use_process_relay_anchor=true))]
    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn new(
        server_id: Option<String>,
        server_ipc_overrides: Option<&Bound<'_, PyAny>>,
        client_ipc_overrides: Option<&Bound<'_, PyAny>>,
        shm_threshold: Option<u64>,
        remote_payload_chunk_size: Option<u64>,
        use_process_relay_anchor: bool,
    ) -> PyResult<Self> {
        let server_ipc_overrides = server_ipc_overrides
            .map(|value| parse_server_ipc_overrides(Some(value)))
            .transpose()?;
        let client_ipc_overrides = client_ipc_overrides
            .map(|value| parse_client_ipc_overrides(Some(value)))
            .transpose()?;
        let inner = Runtime::new(RuntimeOptions {
            server_id,
            server_ipc_overrides,
            client_ipc_overrides,
            shm_threshold,
            remote_payload_chunk_size,
            relay_anchor_address: None,
            use_process_relay_anchor,
        })
        .map_err(runtime_configuration_error_to_py)?;
        Ok(Self {
            inner: Arc::new(inner),
            lease_tracker: Arc::new(BufferLeaseTracker::default()),
            host: Mutex::new(None),
            registrations: Mutex::new(HashMap::new()),
            server_bridge: Mutex::new(None),
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
            .map_err(runtime_configuration_error_to_py)?;
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
        if self.host.lock().is_some() {
            return Err(PyRuntimeError::new_err(
                "server options are frozen after the Core host starts",
            ));
        }
        let server_ipc_overrides = server_ipc_overrides
            .map(|value| parse_server_ipc_overrides(Some(value)))
            .transpose()?;
        self.inner
            .set_server_options(server_id, server_ipc_overrides)
            .map_err(runtime_configuration_error_to_py)?;
        self.server_bridge.lock().take();
        Ok(())
    }

    fn set_relay_anchor_address(&self, relay_anchor_address: Option<String>) -> PyResult<()> {
        if self.host.lock().is_some() {
            let requested = relay_anchor_address
                .as_deref()
                .map(str::trim)
                .map(|value| value.trim_end_matches('/'));
            let current = self.inner.relay_anchor_address_override();
            if requested != current.as_deref() {
                return Err(PyRuntimeError::new_err(
                    "relay anchor is frozen after the Core host starts",
                ));
            }
            return Ok(());
        }
        self.inner.set_relay_anchor_address(relay_anchor_address);
        Ok(())
    }

    #[getter]
    fn relay_anchor_address_override(&self) -> Option<String> {
        self.inner.relay_anchor_address_override()
    }

    #[getter]
    fn effective_relay_anchor_address(&self) -> PyResult<Option<String>> {
        self.inner
            .effective_relay_anchor_address()
            .map_err(lifecycle_error_to_py)
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
        let runtime_overrides = c2_config::RuntimeConfigOverrides {
            client_ipc: self.inner.client_ipc_overrides().unwrap_or_default(),
            shm_threshold: self.inner.shm_threshold_override(),
            ..Default::default()
        };
        let resolved = c2_config::ConfigResolver::resolve_client_ipc(
            runtime_overrides.client_ipc.clone(),
            runtime_overrides,
            c2_config::ConfigSources::from_process(),
        )
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
        client_ipc_to_dict(py, &resolved)
    }

    #[getter]
    fn client_config_frozen(&self) -> bool {
        self.inner.client_config_frozen()
    }

    fn set_client_ipc_overrides(&self, overrides: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let overrides = overrides
            .map(|value| parse_client_ipc_overrides(Some(value)))
            .transpose()?;
        match self.inner.set_client_ipc_overrides(overrides) {
            Ok(()) => Ok(true),
            Err(c2_core::LifecycleError::ClientConfigFrozen) => Ok(false),
            Err(error) => Err(lifecycle_error_to_py(error)),
        }
    }

    fn ensure_server_bridge<'py>(slf: PyRef<'py, Self>) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        if let Some(server) = slf.server_bridge.lock().as_ref() {
            return Ok(server.clone_ref(py));
        }
        let identity = slf
            .inner
            .ensure_server()
            .map_err(runtime_configuration_error_to_py)?;
        let module = py.import("c_two.transport.server.native")?;
        let class = module.getattr("NativeServerBridge")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("bind_address", &identity.ipc_address)?;
        kwargs.set_item("server_id", &identity.server_id)?;
        kwargs.set_item("server_instance_id", &identity.server_instance_id)?;
        if let Some(overrides) = slf.inner.server_ipc_overrides() {
            kwargs.set_item(
                "ipc_overrides",
                server_ipc_overrides_to_dict(py, &overrides)?,
            )?;
        }
        kwargs.set_item("lease_tracker", slf.lease_tracker())?;
        kwargs.set_item("runtime_session", &slf)?;
        let server = class.call((), Some(&kwargs))?.unbind();
        *slf.server_bridge.lock() = Some(server.clone_ref(py));
        Ok(server)
    }

    fn ensure_host_started(&self) -> PyResult<()> {
        self.ensure_host().map(|_| ())
    }

    #[getter]
    fn host_started(&self) -> bool {
        self.host.lock().is_some()
    }

    #[pyo3(signature = (name, dispatcher, method_names, access_map, concurrency_mode, max_pending, max_workers, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, descriptor_json, relay_anchor_address=None))]
    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn register_route<'py>(
        &self,
        py: Python<'py>,
        name: &str,
        dispatcher: Py<PyAny>,
        method_names: Vec<String>,
        access_map: &Bound<'py, PyDict>,
        concurrency_mode: &str,
        max_pending: Option<usize>,
        max_workers: Option<usize>,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        descriptor_json: &[u8],
        relay_anchor_address: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        if self.registrations.lock().contains_key(name) {
            return Err(PyValueError::new_err(format!(
                "Name already registered: {name:?}",
            )));
        }
        if let Some(relay_anchor_address) = relay_anchor_address {
            if self.host.lock().is_some() {
                return Err(PyRuntimeError::new_err(
                    "explicit registration relay anchor must be configured before the Core host starts",
                ));
            }
            self.inner
                .set_relay_anchor_address(Some(relay_anchor_address));
        }

        let methods = method_definitions(&method_names, access_map)?;
        let expected =
            expected_route_contract(name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash)?;
        let service: Arc<dyn c2_core::EncodedService> =
            Arc::new(PyCoreService::new(name.to_string(), dispatcher));
        let definition = service_definition(descriptor_json, expected, methods, service)?
            .with_concurrency(
                parse_concurrency_mode(concurrency_mode)?,
                max_pending,
                max_workers,
            )
            .map_err(core_error_to_py)?;
        let host = self.ensure_host()?;
        let registration = py
            .detach(|| host.register(definition))
            .map_err(core_error_to_py)?;
        let outcome = registration.outcome().clone();
        let concurrency = registration.route_concurrency();
        self.registrations
            .lock()
            .insert(name.to_string(), registration);

        let outcome = register_outcome_to_dict(py, outcome)?.unbind();
        let concurrency = Py::new(py, PyRouteConcurrency::new(concurrency))?;
        Ok((outcome, concurrency)
            .into_pyobject(py)?
            .into_any()
            .unbind())
    }

    fn unregister_route<'py>(
        &self,
        py: Python<'py>,
        name: &str,
        _relay_anchor_address: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let mut registration = self
            .registrations
            .lock()
            .remove(name)
            .ok_or_else(|| PyValueError::new_err(format!("Name not registered: {name:?}")))?;
        let outcome = py
            .detach(|| registration.close())
            .map_err(core_error_to_py)?;
        unregister_outcome_to_dict(py, outcome)
    }

    #[pyo3(signature = (route_names=None, relay_anchor_address=None, timeout_seconds=5.0))]
    fn shutdown<'py>(
        &self,
        py: Python<'py>,
        route_names: Option<Vec<String>>,
        relay_anchor_address: Option<String>,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        let _ = (relay_anchor_address, timeout_seconds);
        if let Some(route_names) = route_names {
            let registered = self.registrations.lock();
            for route_name in route_names {
                if !registered.contains_key(&route_name) {
                    return Err(PyValueError::new_err(format!(
                        "Name not registered: {route_name:?}",
                    )));
                }
            }
        }
        let host = self.host.lock().take();
        let outcome = py.detach(move || {
            host.as_ref()
                .map_or_else(ShutdownOutcome::default, Host::shutdown)
        });
        self.registrations.lock().clear();
        self.server_bridge.lock().take();
        shutdown_outcome_to_dict(py, outcome)
    }

    fn clear_server_identity(&self) -> PyResult<()> {
        if self.host.lock().is_some() {
            return Err(PyRuntimeError::new_err(
                "cannot clear server identity while the Core host is active",
            ));
        }
        self.inner.clear_server_identity();
        Ok(())
    }

    fn clear_relay_projection_cache(&self) {
        self.inner.clear_relay_projection_cache();
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn acquire_ipc_client(
        &self,
        py: Python<'_>,
        address: &str,
        route_name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
    ) -> PyResult<PyCoreClient> {
        let expected = expected_route_contract(
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        )?;
        self.connect_core(
            py,
            expected,
            Connect::DirectIpc {
                address: address.to_string(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn connect_explicit_relay_http(
        &self,
        py: Python<'_>,
        address: &str,
        route_name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
    ) -> PyResult<PyCoreClient> {
        let expected = expected_route_contract(
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        )?;
        self.connect_core(
            py,
            expected,
            Connect::ExplicitRelay {
                relay_url: address.to_string(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn connect_via_relay(
        &self,
        py: Python<'_>,
        route_name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
    ) -> PyResult<PyCoreClient> {
        let expected = expected_route_contract(
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        )?;
        self.connect_core(py, expected, Connect::RelayAware)
    }

    fn path_counters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let counters = self.inner.path_counters();
        let dict = PyDict::new(py);
        dict.set_item("direct_ipc", counters.direct_ipc())?;
        dict.set_item("explicit_relay", counters.explicit_relay())?;
        dict.set_item("relay_aware_local_ipc", counters.relay_aware_local_ipc())?;
        dict.set_item("relay_aware_relay", counters.relay_aware_relay())?;
        Ok(dict)
    }
}

fn runtime_configuration_error_to_py(error: c2_core::LifecycleError) -> PyErr {
    match error {
        c2_core::LifecycleError::InvalidServerId(message)
        | c2_core::LifecycleError::Configuration(message) => PyValueError::new_err(message),
        other => lifecycle_error_to_py(other),
    }
}

fn method_definitions(
    method_names: &[String],
    access_map: &Bound<'_, PyDict>,
) -> PyResult<Vec<MethodDefinition>> {
    method_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let index = u16::try_from(index).map_err(|_| {
                PyValueError::new_err("method index exceeds the C-Two wire capacity")
            })?;
            let access = access_map
                .get_item(index)?
                .ok_or_else(|| {
                    PyValueError::new_err(format!(
                        "missing access metadata for method index {index}",
                    ))
                })?
                .extract::<String>()?;
            let access = match access.as_str() {
                "read" => MethodAccess::Read,
                "write" => MethodAccess::Write,
                other => {
                    return Err(PyValueError::new_err(format!(
                        "invalid method access {other:?}",
                    )));
                }
            };
            Ok(MethodDefinition {
                index,
                name: name.clone(),
                access,
            })
        })
        .collect()
}

fn parse_concurrency_mode(value: &str) -> PyResult<ServiceConcurrencyMode> {
    match value {
        "parallel" => Ok(ServiceConcurrencyMode::Parallel),
        "exclusive" => Ok(ServiceConcurrencyMode::Exclusive),
        "read_parallel" => Ok(ServiceConcurrencyMode::ReadParallel),
        other => Err(PyValueError::new_err(format!(
            "invalid concurrency mode {other:?}",
        ))),
    }
}

fn service_definition(
    descriptor_json: &[u8],
    expected: ExpectedRouteContract,
    methods: Vec<MethodDefinition>,
    service: Arc<dyn c2_core::EncodedService>,
) -> PyResult<ServiceDefinition> {
    let descriptor: serde_json::Value = serde_json::from_slice(descriptor_json)
        .map_err(|error| PyValueError::new_err(format!("invalid contract descriptor: {error}")))?;
    let schema = descriptor
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PyValueError::new_err("contract descriptor is missing schema"))?;
    if schema == PORTABLE_CONTRACT_SCHEMA {
        let release = ContractRelease::from_descriptor_json(descriptor_json)
            .map_err(|error| core_error_to_py(c2_core::Error::from(error)))?;
        let release_expected = release
            .expected_route(expected.route_name.clone())
            .map_err(|error| core_error_to_py(c2_core::Error::from(error)))?;
        if release_expected != expected {
            return Err(core_error_to_py(
                ContractError::InvalidDescriptor {
                    path: "$.fingerprints".to_string(),
                    message: "Python route projection does not match the admitted portable release"
                        .to_string(),
                }
                .into(),
            ));
        }
        return ServiceDefinition::new(
            &release,
            release.reference(),
            expected.route_name,
            methods,
            service,
        )
        .map_err(core_error_to_py);
    }

    let digest = contract_descriptor_sha256_hex(descriptor_json)
        .map_err(|error| core_error_to_py(c2_core::Error::from(error)))?;
    let release_ref_json = serde_json::to_vec(&serde_json::json!({
        "schema": "c-two.contract-release-ref.v1",
        "contract_schema": schema,
        "crm": {
            "namespace": expected.crm_ns,
            "name": expected.crm_name,
            "version": expected.crm_ver,
        },
        "descriptor_sha256": digest,
    }))
    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let release_ref = ContractReleaseRef::from_json(&release_ref_json)
        .map_err(|error| core_error_to_py(c2_core::Error::from(error)))?;
    ServiceDefinition::new_nonportable(release_ref, expected, methods, service)
        .map_err(core_error_to_py)
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
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(expected)
}

fn register_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: RegisterOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("route_uid", outcome.route_uid)?;
    dict.set_item("route_revision", outcome.route_revision)?;
    dict.set_item("server_id", outcome.server_id)?;
    dict.set_item("server_instance_id", outcome.server_instance_id)?;
    dict.set_item("ipc_address", outcome.ipc_address)?;
    dict.set_item("relay_registered", outcome.relay_registered)?;
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
        .map(|close| route_close_outcome_to_dict(py, close).map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("route_outcomes", PyList::new(py, route_outcomes)?)?;
    let relay_errors = outcome
        .relay_errors
        .into_iter()
        .map(|error| relay_cleanup_error_to_dict(py, error).map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("relay_errors", PyList::new(py, relay_errors)?)?;
    dict.set_item("server_was_started", outcome.server_was_started)?;
    dict.set_item("ipc_clients_drained", outcome.ipc_clients_drained)?;
    dict.set_item("http_clients_drained", outcome.http_clients_drained)?;
    dict.set_item("route_close_error", outcome.route_close_error)?;
    dict.set_item("runtime_barrier_error", outcome.runtime_barrier_error)?;
    Ok(dict)
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyRuntimeSession>()?;
    Ok(())
}
