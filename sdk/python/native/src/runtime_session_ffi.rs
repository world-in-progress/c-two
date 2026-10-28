//! PyO3 projection of the language-neutral C-Two Core runtime.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};

use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract, MethodAccess,
    PORTABLE_CONTRACT_SCHEMA, contract_descriptor_sha256_hex,
};
use c2_core::{
    CallExecutionObserver, Connect, FailedStartShutdownReceipt, Host, HostClientHeldLeases,
    HostLifecyclePhase, HostLifecycleSnapshot, HostOptions, MethodDefinition, RegisterOutcome,
    Registration, RetiredMemoryObservation, Runtime, RuntimeOptions, RuntimeShutdownReceipt,
    RuntimeShutdownTarget, ServerLifecyclePolicy, ServiceConcurrencyMode, ServiceDefinition,
    ShutdownOutcome, UnregisterOutcome,
};
use c2_mem::{BufferLeaseStats, BufferLeaseTracker};

use crate::config_ffi::{
    PyConnectAttempt, client_ipc_overrides_to_dict, client_ipc_to_dict, parse_client_ipc_overrides,
    parse_server_ipc_overrides, server_ipc_overrides_to_dict,
};
use crate::core_error_ffi::{
    core_error_to_py, lifecycle_error_to_py, relay_cleanup_error_to_dict,
    route_close_outcome_to_dict,
};
use crate::core_ffi::{PyCoreClient, PyCoreService};
use crate::endpoint_ffi::{PyEndpointCredential, PyLocalEndpointContext};
use crate::lease_ffi::{PyBufferLeaseTracker, lease_stats_dict};
use crate::owner_ffi::{
    PyNativeOwnerBootstrap, PyOwnerRecoveryTicket, PyPreparedOwnerTakeover, owner_error,
};
use crate::route_concurrency_ffi::PyRouteConcurrency;

pub(crate) fn checked_timeout(seconds: f64) -> Result<Duration, &'static str> {
    const INVALID: &str =
        "timeout_seconds must be finite, non-negative, and representable as a Duration";
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(INVALID);
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| INVALID)
}

#[cfg(test)]
mod shutdown_timeout_tests {
    use super::checked_timeout;
    use std::time::Duration;

    #[test]
    fn invalid_timeouts_are_rejected_without_panicking() {
        for seconds in [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -1.0,
            -0.001,
            1e300,
            u64::MAX as f64,
        ] {
            assert!(checked_timeout(seconds).is_err(), "{seconds:?}");
        }
    }

    #[test]
    fn zero_and_normal_timeouts_remain_valid() {
        assert_eq!(checked_timeout(0.0), Ok(Duration::ZERO));
        assert_eq!(checked_timeout(-0.0), Ok(Duration::ZERO));
        assert_eq!(checked_timeout(0.125), Ok(Duration::from_millis(125)));
        assert_eq!(checked_timeout(5.0), Ok(Duration::from_secs(5)));
    }
}

#[cfg(test)]
mod connect_attempt_tests {
    use super::*;

    #[test]
    fn retired_hostless_session_keeps_explicit_name_validation() {
        Python::initialize();
        Python::attach(|py| {
            let session = Py::new(
                py,
                PyRuntimeSession::new(None, None, None, None, None, false, None, None, None)
                    .unwrap(),
            )
            .unwrap();
            PyRuntimeSession::begin_shutdown(session.clone_ref(py), py, 3.0)
                .unwrap()
                .observe(py)
                .unwrap();
            let error = session
                .borrow(py)
                .shutdown(py, Some(vec!["missing".into()]), None, 1.0)
                .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
        });
    }

    #[test]
    fn retirement_owner_lock_timeout_does_not_seal_or_hold_the_gil() {
        Python::initialize();
        Python::attach(|py| {
            let session = Py::new(
                py,
                PyRuntimeSession::new(None, None, None, None, None, false, None, None, None)
                    .unwrap(),
            )
            .unwrap();
            let borrowed = session.borrow(py);
            let owner = borrowed.host.lock();
            let worker_session = session.clone_ref(py);
            let (finished, observed) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    let wait = PyRuntimeSession::begin_shutdown(worker_session, py, 0.025).unwrap();
                    let error = wait.observe(py).unwrap_err();
                    let value = error.value(py);
                    let kind: String = value.getattr("lifecycle_kind").unwrap().extract().unwrap();
                    let stage: String = value
                        .getattr("details")
                        .unwrap()
                        .get_item("stage")
                        .unwrap()
                        .extract()
                        .unwrap();
                    let timeout: f64 = value.getattr("timeout_seconds").unwrap().extract().unwrap();
                    finished.send((kind, stage, timeout)).unwrap();
                });
            });
            let result = py.detach(move || observed.recv_timeout(Duration::from_secs(3)));
            drop(owner);
            worker.join().unwrap();
            assert_eq!(
                result.unwrap(),
                (
                    "shutdown_wait_timeout".into(),
                    "shutdown_owner_wait".into(),
                    0.025,
                )
            );
            assert!(
                borrowed
                    .inner
                    .retirement_receipt(Instant::now())
                    .unwrap()
                    .is_none()
            );
            borrowed.inner.ensure_active().unwrap();
        });
    }

    #[test]
    fn sdk_shutdown_projects_owner_timeout_after_releasing_binding_lock() {
        Python::initialize();
        Python::attach(|py| {
            let globals = PyDict::new(py);
            py.run(
                pyo3::ffi::c_str!(
                    r#"
import c_two as cc
runtime = cc.Runtime()
original = cc.error.CoreError.__init__
def observe_error(self, category, message):
    assert not runtime._lock.locked(), 'error projection kept SDK binding lock'
    assert runtime.names == []
    original(self, category, message)
cc.error.CoreError.__init__ = observe_error
"#
                ),
                Some(&globals),
                None,
            )
            .unwrap();
            let runtime = globals.get_item("runtime").unwrap().unwrap().unbind();
            // The embedded test crate and installed extension have separate
            // PyType identities. Give the real SDK Runtime this test's native instance.
            let session = Py::new(py, PyRuntimeSession::new(None, None, None, None, None, false, None, None, None).unwrap()).unwrap();
            runtime.bind(py).setattr("_runtime_session", session.clone_ref(py)).unwrap();
            let borrowed = session.borrow(py);
            let owner = borrowed.host.lock();
            let worker_runtime = runtime.clone_ref(py);
            let (done, observed) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("timeout", 0.025).unwrap();
                    let error = worker_runtime
                        .bind(py)
                        .call_method("shutdown", (), Some(&kwargs))
                        .unwrap_err();
                    done.send(
                        error
                            .value(py)
                            .getattr("lifecycle_kind")
                            .and_then(|value| value.extract::<String>()),
                    )
                    .unwrap();
                });
            });
            let result = py.detach(move || observed.recv_timeout(Duration::from_secs(3)));
            drop(owner);
            py.detach(|| worker.join()).unwrap();
            py.run(
                pyo3::ffi::c_str!("cc.error.CoreError.__init__ = original"),
                Some(&globals),
                None,
            )
            .unwrap();
            assert_eq!(result.unwrap().unwrap(), "shutdown_wait_timeout");
            assert!(
                borrowed
                    .inner
                    .retirement_receipt(Instant::now())
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn retired_explicit_name_observation_keeps_owner_lock_in_original_deadline() {
        Python::initialize();
        Python::attach(|py| {
            let session = Py::new(
                py,
                PyRuntimeSession::new(None, None, None, None, None, false, None, None, None)
                    .unwrap(),
            )
            .unwrap();
            PyRuntimeSession::begin_shutdown(session.clone_ref(py), py, 3.0)
                .unwrap()
                .observe(py)
                .unwrap();
            let borrowed = session.borrow(py);
            let owner = borrowed.host.lock();
            let worker_session = session.clone_ref(py);
            let (done, observed) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    let error = worker_session
                        .borrow(py)
                        .shutdown(py, Some(Vec::new()), None, 0.0)
                        .unwrap_err();
                    done.send(
                        error
                            .value(py)
                            .getattr("lifecycle_kind")
                            .unwrap()
                            .extract::<String>()
                            .unwrap(),
                    )
                    .unwrap();
                })
            });
            let result = py.detach(move || observed.recv_timeout(Duration::from_secs(3)));
            drop(owner);
            py.detach(|| worker.join()).unwrap();
            assert_eq!(result.unwrap(), "shutdown_wait_timeout");
        });
    }

    #[test]
    fn relay_override_host_lock_expires_before_owner_releases_and_recovers() {
        Python::initialize();
        let session = Arc::new(
            PyRuntimeSession::new(None, None, None, None, None, false, None, None, None).unwrap(),
        );
        let host = session.host.lock();
        let worker_session = session.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = Python::attach(|py| {
                let attempt = PyConnectAttempt {
                    inner: c2_core::ConnectAttempt::start(
                        c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(100)),
                    )
                    .unwrap(),
                };
                worker_session.set_relay_anchor_address(
                    py,
                    Some("http://relay.test".into()),
                    Some(&attempt),
                )
            });
            done_tx.send(result).unwrap();
        });
        let observed = done_rx.recv_timeout(Duration::from_secs(2));
        drop(host);
        worker.join().unwrap();
        let result = observed.expect("native host lock must expire before its owner releases");
        let error = result.unwrap_err();
        Python::attach(|py| {
            let value = error.value(py);
            assert_eq!(
                value.getattr("code").unwrap().extract::<u16>().unwrap(),
                715
            );
            let details = value.getattr("details").unwrap();
            assert_eq!(
                details
                    .get_item("stage")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "relay_override_host_wait"
            );
            assert_eq!(
                details
                    .get_item("operation")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "connect"
            );
        });
        Python::attach(|py| {
            session.set_relay_anchor_address(py, Some("http://relay.test".into()), None)
        })
        .unwrap();
        assert_eq!(
            session.inner.relay_anchor_address_override().as_deref(),
            Some("http://relay.test")
        );
        assert!(!session.inner.client_config_frozen());
        assert_eq!(session.inner.path_counters().direct_ipc(), 0);
    }
}

#[pyclass(name = "RuntimeSession", frozen)]
pub struct PyRuntimeSession {
    inner: Arc<Runtime>,
    lease_tracker: Arc<BufferLeaseTracker>,
    host: Mutex<HostRecord>,
    registrations: Mutex<HashMap<String, Registration>>,
    server_bridge: Mutex<Option<Py<PyAny>>>,
    /// Retired read-only observations adopted from previous sessions.
    ///
    /// Each entry is one retirement event: it holds only weak views of budget
    /// accounting and lease metadata, so a closed session's outstanding
    /// charges stay observable after `cc.shutdown()` for exactly as long as
    /// real owners (old proxies, in-flight responses, outstanding holds and
    /// reservations) keep them alive — never by retaining the retired
    /// session's Runtime, cache, pools, callbacks, or payloads. Records whose
    /// last owner is gone are pruned individually on the next observation.
    retired: Mutex<Vec<Arc<RetiredMemoryObservation>>>,
    /// Opaque Core observers retain only publication and counter metadata,
    /// never retired runtimes, transports, services or payload ownership.
    retired_call_execution: Mutex<Vec<CallExecutionObserver>>,
}

#[derive(Default)]
struct HostRecord {
    host: Option<Host>,
    failed_start_receipt: Option<FailedStartShutdownReceipt>,
}

/// One caller's bounded observation of an already-started Core transaction.
/// The receipt owns no competing cleanup work; Core continues after this wait drops.
#[pyclass(name = "RuntimeShutdownWait", module = "c_two._native", frozen)]
pub struct PyRuntimeShutdownWait {
    session: Py<PyRuntimeSession>,
    receipt: Result<RuntimeShutdownReceipt, c2_core::LifecycleError>,
    deadline: Instant,
    timeout_seconds: f64,
}

#[pymethods]
impl PyRuntimeShutdownWait {
    fn observe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        // Error construction can execute Python. The SDK observes outside its
        // binding lock, after begin has either transferred work or kept its owner.
        let receipt = self
            .receipt
            .as_ref()
            .map_err(|error| shutdown_error_to_py(py, error.clone(), self.timeout_seconds))?;
        let (outcome, completed) = py.detach(|| receipt.observe_until(self.deadline));
        self.session
            .borrow(py)
            .project_shutdown_observation(py, outcome, completed)
    }
}

fn shutdown_error_to_py(py: Python<'_>, error: c2_core::LifecycleError, seconds: f64) -> PyErr {
    let timed_out = matches!(error, c2_core::LifecycleError::ShutdownWaitTimeout { .. });
    let error = lifecycle_error_to_py(error);
    if timed_out {
        let _ = error.value(py).setattr("timeout_seconds", seconds);
    }
    error
}

/// Read-only observation bundle passed from a shutting-down session to its
/// replacement.
///
/// The Python registry moves one of these between sessions; it carries no
/// mutable transport authority and exposes no Python surface at all.
/// Memory observations use weak handles. Call execution observers retain
/// only Core-owned counter metadata, including a publication cell for domains
/// not yet initialized at capture. Neither observer retains runtime authority.
/// The pending bundle is the one created by the latest retirement event; carried
/// bundles belong to earlier retirements. Capture is retry-safe: a failed
/// replacement attempt drops its un-adopted bundle without consuming the old
/// session's records, and a later retry re-captures everything the session
/// owns at that moment.
#[pyclass(name = "RetiredMemoryObservation", frozen)]
pub struct PyRetiredMemoryObservation {
    inner: c2_core::RetirementHandoff,
    call_execution: Vec<CallExecutionObserver>,
}

impl PyRuntimeSession {
    fn check_shutdown_route_names(
        &self,
        route_names: Option<&[String]>,
        deadline: Option<Instant>,
    ) -> PyResult<()> {
        if let Some(names) = route_names {
            let registered = match deadline {
                Some(deadline) => self.registrations.try_lock_until(deadline).ok_or_else(|| {
                    lifecycle_error_to_py(c2_core::LifecycleError::ShutdownWaitTimeout {
                        stage: "shutdown_registrations_wait",
                    })
                })?,
                None => self.registrations.lock(),
            };
            for name in names {
                if !registered.contains_key(name) {
                    return Err(PyValueError::new_err(format!(
                        "Name not registered: {name:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn project_shutdown_observation<'py>(
        &self,
        py: Python<'py>,
        outcome: ShutdownOutcome,
        completed: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        if completed {
            // Core has sealed publication before this path can run. Drop native
            // registrations without the GIL, then release the Python bridge.
            let bridge = py.detach(|| {
                self.registrations.lock().clear();
                self.server_bridge.lock().take()
            });
            drop(bridge);
        }
        let dict = shutdown_outcome_to_dict(py, outcome)?;
        dict.set_item("completed", completed)?;
        Ok(dict)
    }

    /// Start this session's one Core host.
    ///
    /// The default [`HostOptions`] inherits the native lifecycle policy the
    /// Runtime has frozen (or the one `set_lifecycle_policy` pinned), so an
    /// `OwnerBound` policy consumes the attached owner capability here and a
    /// `Persistent` one does not. The capability is consumed at most once: a
    /// second `ensure_host` reuses the live host and never re-consumes.
    fn ensure_host(&self) -> PyResult<Host> {
        let mut host_guard = self.host.lock();
        if let Err(error) = self.inner.ensure_active() {
            drop(host_guard);
            return Err(lifecycle_error_to_py(error));
        }
        if let Some(host) = host_guard.host.as_ref() {
            if host.shutdown_outcome().is_none() {
                if !host.is_running() || self.inner.has_pending_teardown() {
                    return Err(PyRuntimeError::new_err(
                        "Core host is still draining; consume terminal shutdown before restarting",
                    ));
                }
                return Ok(host.clone());
            }
            if !matches!(
                self.inner.lifecycle_policy(),
                ServerLifecyclePolicy::Persistent
            ) {
                return Err(PyRuntimeError::new_err(
                    "owner-bound host cannot restart with an already consumed owner capability",
                ));
            }
            if !self.registrations.lock().is_empty() {
                return Err(PyRuntimeError::new_err(
                    "consume terminal shutdown route outcomes before restarting the Core host",
                ));
            }
            // Persistent restart publishes a fresh Host only after its native
            // start succeeds. Keep the old terminal journal on every error.
        }
        let mut failed_receipt = None;
        let started = self
            .inner
            .host_with_failed_start_receipt(HostOptions::default(), |receipt| {
                failed_receipt = Some(receipt)
            });
        if let Ok(host) = &started {
            host_guard.host = Some(host.clone());
            host_guard.failed_start_receipt = None;
        } else if let Some(receipt) = failed_receipt {
            host_guard.failed_start_receipt = Some(receipt);
        }
        // Error projection attaches to Python; release the Host lock before the GIL.
        drop(host_guard);
        started.map_err(core_error_to_py)
    }

    /// Capture this session's retirement bundle plus every adopted earlier
    /// one, without consuming the session's records.
    ///
    /// Capture is a pure re-read, so it is retry-safe: a replacement that
    /// fails to construct or adopt leaves the old session — and every
    /// observation it already carried — exactly where they were, and a later
    /// retry re-captures the session's own domains and tracker as they exist
    /// at that moment, including scopes first created after the failed
    /// attempt. Composition de-duplicates by domain/tracker identity, so a
    /// session captured twice can never double-count. Capturing never
    /// connects, maps memory, freezes config, or instantiates a host.
    fn retire_bundles(&self) -> PyRetiredMemoryObservation {
        self.prune_retired_call_execution();
        let pending = Arc::new(RetiredMemoryObservation::new());
        if let Some(observer) = self.inner.outgoing_memory_observer() {
            pending.push_scope(c2_core::scope::RUNTIME_OUTGOING, observer);
        }
        if let Some(host) = self.host.lock().host.as_ref() {
            pending.push_scope(c2_core::scope::SERVER, host.server_memory_observer());
        }
        pending.push_tracker(Arc::clone(&self.lease_tracker));
        PyRetiredMemoryObservation {
            inner: c2_core::RetirementHandoff::new(pending, self.retired.lock().clone()),
            call_execution: {
                let mut observers = self.retired_call_execution.lock().clone();
                let observer = self.inner.call_execution_observer();
                if observer.is_live() {
                    observers.push(observer);
                }
                observers
            },
        }
    }

    /// Live retired observations, pruning records whose last owner is gone.
    ///
    /// Delegates to the shared Core list-maintenance rule: a record survives
    /// exactly while a real owner keeps its budget accounting or lease
    /// metadata alive — zero counters alone never prune, because a supported
    /// producer (an old proxy, an in-flight response) may still publish — and
    /// an emptied bundle goes away.
    fn live_retired_observations(&self) -> Vec<Arc<RetiredMemoryObservation>> {
        self.prune_retired_call_execution();
        let mut retired = self.retired.lock();
        RetiredMemoryObservation::retain_live_bundles(&mut retired);
        retired.clone()
    }

    fn prune_retired_call_execution(&self) {
        self.retired_call_execution
            .lock()
            .retain(CallExecutionObserver::is_live);
    }

    /// Retained-lease counters across the live tracker and every retired one.
    fn merged_lease_stats(&self) -> BufferLeaseStats {
        let mut stats = self.lease_tracker.stats();
        let retired = self.live_retired_observations();
        stats.merge(&RetiredMemoryObservation::compose_lease_stats(&retired));
        stats
    }

    fn connect_core(
        &self,
        py: Python<'_>,
        expected: ExpectedRouteContract,
        mode: Connect,
        timeout_seconds: Option<f64>,
        connect_attempt: Option<&PyConnectAttempt>,
    ) -> PyResult<PyCoreClient> {
        if connect_attempt.is_some() && timeout_seconds.is_some() {
            return Err(PyValueError::new_err(
                "provide connect_attempt or timeout_seconds, not both",
            ));
        }
        let attempt = match connect_attempt {
            Some(attempt) => attempt.inner,
            None => {
                let options = c2_config::ConnectOptions::from_timeout_secs(timeout_seconds)
                    .map_err(|error| PyValueError::new_err(error.to_string()))?;
                c2_core::ConnectAttempt::start(options).map_err(core_error_to_py)?
            }
        };
        py.detach(|| self.inner.connect_with_attempt(expected, mode, &attempt))
            .map(PyCoreClient::new)
            .map_err(core_error_to_py)
    }
}

#[pymethods]
impl PyRuntimeSession {
    fn connection_events(&self) -> crate::connection_ffi::PyConnectionEvents {
        crate::connection_ffi::PyConnectionEvents {
            inner: self.inner.connection_events(),
        }
    }

    #[new]
    #[pyo3(signature = (server_id=None, server_ipc_overrides=None, client_ipc_overrides=None, shm_threshold=None, remote_payload_chunk_size=None, use_process_relay_anchor=true, max_outstanding_calls=None, retained_input_budget_bytes=None, startup_timeout=None))]
    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    fn new(
        server_id: Option<String>,
        server_ipc_overrides: Option<&Bound<'_, PyAny>>,
        client_ipc_overrides: Option<&Bound<'_, PyAny>>,
        shm_threshold: Option<u64>,
        remote_payload_chunk_size: Option<u64>,
        use_process_relay_anchor: bool,
        max_outstanding_calls: Option<u64>,
        retained_input_budget_bytes: Option<u64>,
        startup_timeout: Option<f64>,
    ) -> PyResult<Self> {
        let server_ipc_overrides = server_ipc_overrides
            .map(|value| parse_server_ipc_overrides(Some(value)))
            .transpose()?;
        let client_ipc_overrides = client_ipc_overrides
            .map(|value| parse_client_ipc_overrides(Some(value)))
            .transpose()?;
        let server_startup_timeout = startup_timeout
            .map(checked_timeout)
            .transpose()
            .map_err(PyValueError::new_err)?;
        let inner = Runtime::new(RuntimeOptions {
            server_id,
            server_ipc_overrides,
            server_startup_timeout,
            client_ipc_overrides,
            shm_threshold,
            remote_payload_chunk_size,
            relay_anchor_address: None,
            use_process_relay_anchor,
        })
        .map_err(runtime_configuration_error_to_py)?;
        let call_execution_overrides = c2_config::CallExecutionLimitsOverrides {
            max_outstanding_calls,
            retained_input_budget_bytes,
        };
        if max_outstanding_calls.is_some() || retained_input_budget_bytes.is_some() {
            inner
                .set_call_execution_limits(call_execution_overrides.clone())
                .map_err(lifecycle_error_to_py)?;
        }
        Ok(Self {
            inner: Arc::new(inner),
            lease_tracker: Arc::new(BufferLeaseTracker::default()),
            host: Mutex::new(HostRecord::default()),
            registrations: Mutex::new(HashMap::new()),
            server_bridge: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
            retired_call_execution: Mutex::new(Vec::new()),
        })
    }

    fn lease_tracker(&self) -> PyBufferLeaseTracker {
        PyBufferLeaseTracker::from_arc(Arc::clone(&self.lease_tracker))
    }

    /// Configure Core's local domain; no Python/native copy of root authority.
    /// An opaque captured context can pin a replacement without process discovery.
    #[pyo3(signature = (*, root=None, context=None))]
    fn set_local_endpoint(
        &self,
        root: Option<String>,
        context: Option<&PyLocalEndpointContext>,
    ) -> PyResult<()> {
        if let Some(context) = context {
            if root.is_some() {
                return Err(PyValueError::new_err(
                    "root and context are mutually exclusive",
                ));
            }
            let options = c2_config::LocalEndpointOptions {
                unix_root: context.inner.unix_root().map(Into::into),
            };
            let sources = c2_config::ConfigSources::empty();
            // A captured choice must still describe this platform scope.
            // Refuse a user-scope change rather than silently rebasing it.
            let resolved =
                c2_config::ConfigResolver::resolve_local_endpoint(options.clone(), sources.clone())
                    .map_err(|error| PyValueError::new_err(error.to_string()))?;
            if resolved != context.inner {
                return Err(PyValueError::new_err(
                    "captured local endpoint context does not match the current platform scope",
                ));
            }
            self.inner.set_local_endpoint_with_sources(options, sources)
        } else {
            self.inner
                .set_local_endpoint(c2_config::LocalEndpointOptions {
                    unix_root: root.map(Into::into),
                })
        }
        .map_err(runtime_configuration_error_to_py)
    }

    /// Core captures and inherits the policy without resolving local scope.
    fn inherit_local_endpoint_selection(&self, previous: &Self) -> PyResult<()> {
        self.inner
            .inherit_local_endpoint_selection(&previous.inner)
            .map_err(runtime_configuration_error_to_py)
    }

    /// Pure native observation; deriving names never freezes local I/O policy.
    fn local_endpoint_context(&self) -> PyResult<PyLocalEndpointContext> {
        self.inner
            .local_endpoint_context()
            .map(|inner| PyLocalEndpointContext { inner })
            .map_err(runtime_configuration_error_to_py)
    }

    fn local_endpoint(&self, address: &str) -> PyResult<String> {
        self.inner
            .local_endpoint(address)
            .map(|endpoint| endpoint.os_name().to_string_lossy().into_owned())
            .map_err(runtime_configuration_error_to_py)
    }

    #[getter]
    fn local_endpoint_frozen(&self) -> bool {
        self.inner.local_endpoint_frozen()
    }

    #[pyo3(signature = (address, timeout_seconds=0.5))]
    fn ping_direct_ipc(
        &self,
        py: Python<'_>,
        address: &str,
        timeout_seconds: f64,
    ) -> PyResult<bool> {
        let timeout = crate::control_ffi::timeout_duration(timeout_seconds)?;
        py.detach(|| self.inner.ping_direct_ipc(address, timeout))
            .map_err(runtime_configuration_error_to_py)
    }

    #[pyo3(signature = (address, timeout_seconds=0.5))]
    fn shutdown_direct_ipc<'py>(
        &self,
        py: Python<'py>,
        address: &str,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        let timeout = crate::control_ffi::timeout_duration(timeout_seconds)?;
        let outcome = py
            .detach(|| self.inner.shutdown_direct_ipc(address, timeout))
            .map_err(runtime_configuration_error_to_py)?;
        crate::control_ffi::shutdown_ack_dict(py, outcome)
    }

    fn hold_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        lease_stats_dict(py, &self.merged_lease_stats())
    }

    #[pyo3(signature = (*, max_outstanding_calls=None, retained_input_budget_bytes=None))]
    fn set_call_execution_limits(
        &self,
        max_outstanding_calls: Option<u64>,
        retained_input_budget_bytes: Option<u64>,
    ) -> PyResult<()> {
        let overrides = c2_config::CallExecutionLimitsOverrides {
            max_outstanding_calls,
            retained_input_budget_bytes,
        };
        self.inner
            .set_call_execution_limits(overrides)
            .map_err(lifecycle_error_to_py)
    }

    #[getter]
    fn call_execution_limits_overrides<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let overrides = self.inner.call_execution_limits_overrides();
        let dict = PyDict::new(py);
        dict.set_item("max_outstanding_calls", overrides.max_outstanding_calls)?;
        dict.set_item(
            "retained_input_budget_bytes",
            overrides.retained_input_budget_bytes,
        )?;
        Ok(dict)
    }

    /// Actual Core-owned finite input charges, including unfinished native
    /// continuations after a Python caller times out or a session is replaced.
    fn call_execution_snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let snapshot = self
            .inner
            .call_execution_snapshot()
            .map_err(lifecycle_error_to_py)?;
        let dict = call_execution_snapshot_dict(py, &snapshot)?;
        let mut retired = self.retired_call_execution.lock();
        let mut reports = Vec::new();
        // Initialization and close publication come from the same Core
        // authority. An uninitialized open domain stays observable; a closed
        // domain is retired only after actual owners release all charges.
        retired.retain(CallExecutionObserver::is_live);
        for observer in retired.iter() {
            let observation = observer.snapshot();
            let report = match observation.counters {
                Some(snapshot) => call_execution_snapshot_dict(py, &snapshot)?,
                None => {
                    let report = PyDict::new(py);
                    report.set_item("max_operations", py.None())?;
                    report.set_item("max_retained_bytes", py.None())?;
                    report.set_item("used_operations", 0)?;
                    report.set_item("used_retained_bytes", 0)?;
                    report.set_item("peak_operations", 0)?;
                    report.set_item("peak_retained_bytes", 0)?;
                    report.set_item("rejected_reservations", 0)?;
                    report.set_item("closed", observation.closed)?;
                    report
                }
            };
            report.set_item("initialized", observation.initialized)?;
            report.set_item("state", "retired")?;
            reports.push(report);
        }
        dict.set_item("retired", PyList::new(py, reports)?)?;
        Ok(dict)
    }

    /// Read-only, scope-labelled memory-budget snapshot.
    ///
    /// Reports C-Two-owned IPC backing and live reassembly accounting for the
    /// per-Runtime outgoing client domain and the per-server direction, plus
    /// the Rust-owned retained-buffer (`hold_stats`) counters. Retired domains
    /// adopted from earlier shutdowns appear in `retired` with the same
    /// scope-labelled shape while a real owner — an old proxy's native
    /// client, an in-flight response, an outstanding hold or reservation —
    /// keeps their accounting alive; records detach once their last owner is
    /// gone, so repeated empty session swaps never accumulate. Observing this
    /// never connects, maps memory, freezes client configuration, instantiates
    /// a host, or resets accounting. `runtime_outgoing`/`server` are ``None``
    /// until that scope exists (a scope that was never frozen stays
    /// ``None``).
    ///
    /// The three budget cells are independent accounting scopes, not process
    /// RSS; their sum must never be read as physical memory usage.
    fn memory_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let stats = match self.host.lock().host.as_ref() {
            Some(host) => host.memory_stats(),
            None => self.inner.memory_stats(),
        };
        let dict = PyDict::new(py);
        dict.set_item(
            "runtime_outgoing",
            match stats.runtime_outgoing.as_ref() {
                Some(scope) => {
                    memory_scope_dict(py, "runtime_outgoing", "active", scope)?.into_any()
                }
                None => py.None().into_bound(py),
            },
        )?;
        dict.set_item(
            "server",
            match stats.server.as_ref() {
                Some(scope) => memory_scope_dict(py, "server", "active", scope)?.into_any(),
                None => py.None().into_bound(py),
            },
        )?;
        let retired = self.live_retired_observations();
        let retired_list = PyList::new(
            py,
            RetiredMemoryObservation::compose_scope_reports(&retired)
                .iter()
                .map(|report| memory_scope_dict(py, report.role, "retired", &report.stats))
                .collect::<PyResult<Vec<_>>>()?,
        )?;
        dict.set_item("retired", retired_list)?;
        dict.set_item("holds", lease_stats_dict(py, &self.merged_lease_stats())?)?;
        dict.set_item("budget_cells_note", MEMORY_ACCOUNTING_NOTE)?;
        Ok(dict)
    }

    /// Detach a read-only observation of this session's memory domains.
    ///
    /// The process registry calls this before shutting down this session and
    /// hands the result to its replacement, so charges retained by held data
    /// and retired budget domains stay observable after `cc.shutdown()`. The
    /// pending bundle holds only weak views of budget accounting and lease
    /// metadata. Call-execution observers keep only Core publication/counter
    /// metadata, even when captured before first finite-domain initialization.
    /// They retain no Runtime, transport, service or payload registry, and
    /// expose no independent accounting authority. Earlier bundles are carried forward
    /// unchanged, and this session keeps its own records until the handoff
    /// succeeds, so a failed replacement never loses them.
    fn retire_memory_observation(&self) -> PyRetiredMemoryObservation {
        self.retire_bundles()
    }

    /// Adopt a retired observation from a previous session.
    ///
    /// Every carried bundle is installed as-is, including any that currently
    /// report zero usage: a zero record stays observable because a supported
    /// producer of the retired session — an old proxy, an in-flight response
    /// — may still publish into it, and records are pruned only when their
    /// last real owner is gone.
    fn adopt_retired_memory_observation(&self, observation: &PyRetiredMemoryObservation) {
        let mut retired = self.retired.lock();
        observation.inner.adopt_into(&mut retired);
        let mut execution = self.retired_call_execution.lock();
        execution.retain(CallExecutionObserver::is_live);
        for observer in &observation.call_execution {
            if observer.is_live()
                && !execution
                    .iter()
                    .any(|existing| existing.same_domain(observer))
            {
                execution.push(observer.clone());
            }
        }
    }

    fn sweep_hold_leases<'py>(
        &self,
        py: Python<'py>,
        threshold_seconds: f64,
    ) -> PyResult<Bound<'py, PyList>> {
        if !threshold_seconds.is_finite() || threshold_seconds < 0.0 {
            return Err(PyValueError::new_err(
                "threshold_seconds must be a non-negative finite number",
            ));
        }
        // Compose the live tracker with every retired bundle's snapshots so
        // retired lease metadata does not disappear while a real owner keeps
        // it alive. The retired trackers themselves stay inside the
        // observation bundles; only their snapshot values reach Python.
        let threshold = Duration::from_secs_f64(threshold_seconds);
        let retired = self.live_retired_observations();
        let mut snapshots = self.lease_tracker.sweep_retained(threshold);
        snapshots.extend(RetiredMemoryObservation::compose_sweep_snapshots(
            &retired, threshold,
        ));
        snapshots.sort_by_key(|snapshot| snapshot.id);
        let list = PyList::new(
            py,
            snapshots
                .iter()
                .map(|snapshot| crate::lease_ffi::lease_snapshot_dict(py, snapshot))
                .collect::<PyResult<Vec<_>>>()?,
        )?;
        Ok(list)
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

    /// Snapshot the existing host's bound listener without creating a host.
    #[getter]
    fn server_credential(&self) -> Option<PyEndpointCredential> {
        self.host
            .lock()
            .host
            .as_ref()
            .and_then(Host::endpoint_credential)
            .map(PyEndpointCredential::new)
    }

    #[getter]
    fn server_ipc_overrides<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .server_ipc_overrides()
            .map(|overrides| server_ipc_overrides_to_dict(py, &overrides))
            .transpose()
    }

    #[getter]
    fn startup_timeout(&self) -> Option<f64> {
        self.inner
            .server_startup_timeout()
            .map(|timeout| timeout.as_secs_f64())
    }

    #[pyo3(signature = (server_id=None, server_ipc_overrides=None, startup_timeout=None))]
    fn set_server_options(
        &self,
        server_id: Option<String>,
        server_ipc_overrides: Option<&Bound<'_, PyAny>>,
        startup_timeout: Option<f64>,
    ) -> PyResult<()> {
        // Preserve frozen-before-parsing precedence; Core also fences the commit.
        if self.host.lock().host.is_some() {
            return Err(PyRuntimeError::new_err(
                "server options are frozen after the Core host starts",
            ));
        }
        let server_ipc_overrides = server_ipc_overrides
            .map(|value| parse_server_ipc_overrides(Some(value)))
            .transpose()?;
        let startup_timeout = startup_timeout
            .map(checked_timeout)
            .transpose()
            .map_err(PyValueError::new_err)?;
        self.inner
            .set_server_options(server_id, server_ipc_overrides, startup_timeout)
            .map_err(runtime_configuration_error_to_py)?;
        self.server_bridge.lock().take();
        Ok(())
    }

    #[pyo3(signature = (relay_anchor_address, *, connect_attempt=None))]
    fn set_relay_anchor_address(
        &self,
        py: Python<'_>,
        relay_anchor_address: Option<String>,
        connect_attempt: Option<&PyConnectAttempt>,
    ) -> PyResult<()> {
        let attempt = connect_attempt.map(|attempt| attempt.inner);
        py.detach(|| {
            let host = match &attempt {
                Some(attempt) => attempt
                    .lock(&self.host, "relay_override_host_wait")
                    .map_err(core_error_to_py)?,
                None => self.host.lock(),
            };
            if host.host.is_some() {
                let requested = relay_anchor_address
                    .as_deref()
                    .map(str::trim)
                    .map(|value| value.trim_end_matches('/'));
                let current = match &attempt {
                    Some(attempt) => self
                        .inner
                        .relay_anchor_address_override_with_attempt(attempt)
                        .map_err(core_error_to_py)?,
                    None => self.inner.relay_anchor_address_override(),
                };
                if requested != current.as_deref() {
                    return Err(PyRuntimeError::new_err(
                        "relay anchor is frozen after the Core host starts",
                    ));
                }
                return Ok(());
            }
            match &attempt {
                Some(attempt) => self
                    .inner
                    .set_relay_anchor_address_with_attempt(relay_anchor_address, attempt)
                    .map_err(core_error_to_py),
                None => {
                    self.inner.set_relay_anchor_address(relay_anchor_address);
                    Ok(())
                }
            }
        })
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
        slf.inner
            .ensure_server()
            .map_err(runtime_configuration_error_to_py)?;
        let module = py.import("c_two.transport.server.native")?;
        let class = module.getattr("NativeServerBridge")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("lease_tracker", slf.lease_tracker())?;
        kwargs.set_item("runtime_session", &slf)?;
        let server = class.call((), Some(&kwargs))?.unbind();
        *slf.server_bridge.lock() = Some(server.clone_ref(py));
        Ok(server)
    }

    fn _preflight_server_config(&self) -> PyResult<()> {
        self.inner
            .server_ipc_config()
            .map(|_| ())
            .map_err(runtime_configuration_error_to_py)
    }

    fn ensure_host_started(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.ensure_host().map(|_| ()))
    }

    /// True while the Core host is actually running.
    ///
    /// This reports the native `Host.is_running()` observation instead of the
    /// SDK's mere handle presence, so a host that already stopped (explicit
    /// shutdown, owner EOF, or a failed start attempt) does not read as
    /// started.
    #[getter]
    fn host_started(&self) -> bool {
        self.host
            .lock()
            .host
            .as_ref()
            .is_some_and(c2_core::Host::is_running)
    }

    /// Thin read-only lifecycle observation for the bridge.
    ///
    /// The projection exposes only the native policy/phase and the separate
    /// listener, drained-work and client-held-lease facts. `None` means no
    /// host exists, never "stopped". It never initiates or consumes shutdown
    /// and never exposes the owner capability.
    fn native_lifecycle_snapshot<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let host = self.host.lock().host.clone();
        host.map(|host| lifecycle_snapshot_to_dict(py, &host.lifecycle_snapshot()))
            .transpose()
    }

    /// Terminal native shutdown observation, or `None` while the host has not
    /// produced a completed transaction.
    fn native_terminal_outcome<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let (host, receipt) = {
            let record = self.host.lock();
            (record.host.clone(), record.failed_start_receipt.clone())
        };
        if let Some(receipt) = receipt {
            return match receipt.terminal_outcome() {
                Some(outcome) => {
                    let dict = shutdown_outcome_to_dict(py, outcome)?;
                    dict.set_item("completed", true)?;
                    Ok(Some(dict))
                }
                None => Ok(None),
            };
        }
        let Some(host) = host else {
            return Ok(None);
        };
        match host.shutdown_outcome() {
            Some(outcome) => {
                let dict = shutdown_outcome_to_dict(py, outcome)?;
                dict.set_item("completed", true)?;
                Ok(Some(dict))
            }
            None => Ok(None),
        }
    }

    /// Whether the native owner capability is attached and not yet consumed.
    #[getter]
    fn owner_control_attached(&self) -> bool {
        self.inner.owner_control_attached()
    }

    /// Attach one opaque native owner control receiver.
    ///
    /// The receiver is consumed exactly once. A duplicate attach, a late
    /// attach after a host consumed the capability, and a capability that is
    /// already closed are all rejected by Core before any host can publish
    /// readiness, so a policy name alone can never create an owner-bound host.
    fn prepare_owner_takeover(
        &self,
        py: Python<'_>,
        ticket: &PyOwnerRecoveryTicket,
    ) -> PyResult<PyPreparedOwnerTakeover> {
        py.detach(|| self.inner.prepare_owner_takeover(ticket.inner.clone()))
            .map(|inner| PyPreparedOwnerTakeover { inner })
            .map_err(|error| owner_error(py, error))
    }

    fn attach_owner_control(&self, receiver: &PyNativeOwnerBootstrap) -> PyResult<()> {
        let host_guard = self.host.lock();
        if host_guard.host.is_some() || self.inner.owner_control_consumed() {
            return Err(PyRuntimeError::new_err(
                "owner control capability must be attached before the Core host starts",
            ));
        }
        if !matches!(
            self.inner.lifecycle_policy(),
            ServerLifecyclePolicy::OwnerBound { .. }
        ) {
            return Err(PyValueError::new_err(
                "owner_control requires the owner_bound lifecycle policy",
            ));
        }
        if self.inner.owner_control_attached() {
            return Err(PyRuntimeError::new_err(
                "owner control capability already attached",
            ));
        }
        let receiver = receiver.take_for_attach()?;
        self.inner
            .attach_owner_control(receiver)
            .map_err(runtime_configuration_error_to_py)
    }

    /// Set the native lifecycle policy before the first host freezes it.
    #[pyo3(signature = (policy=None, owner_missing_grace_seconds=None))]
    fn set_lifecycle_policy(
        &self,
        policy: Option<&str>,
        owner_missing_grace_seconds: Option<f64>,
    ) -> PyResult<()> {
        let policy = parse_lifecycle_policy(policy, owner_missing_grace_seconds)?;
        self.inner
            .set_lifecycle_policy(policy)
            .map_err(runtime_configuration_error_to_py)
    }

    /// Read-only projection of the frozen native lifecycle policy.
    #[getter]
    fn lifecycle_policy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        lifecycle_policy_to_dict(py, &self.inner.lifecycle_policy())
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
            if self.host.lock().host.is_some() {
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

    /// Extract Python numeric input before the SDK takes its binding lock.
    /// Range/deadline validation occurs once, at begin_shutdown.
    #[staticmethod]
    fn shutdown_timeout_seconds(seconds: f64) -> f64 {
        seconds
    }

    #[pyo3(signature = (connect_attempt=None))]
    fn check_active(
        &self,
        py: Python<'_>,
        connect_attempt: Option<&PyConnectAttempt>,
    ) -> PyResult<()> {
        py.detach(|| match connect_attempt {
            Some(attempt) => self.inner.ensure_active_with_attempt(&attempt.inner),
            None => self.inner.ensure_active().map_err(c2_core::Error::from),
        })
        .map_err(core_error_to_py)
    }

    #[pyo3(signature = (timeout_seconds=5.0))]
    fn begin_shutdown(
        slf: Py<Self>,
        py: Python<'_>,
        timeout_seconds: f64,
    ) -> PyResult<PyRuntimeShutdownWait> {
        let timeout = checked_timeout(timeout_seconds).map_err(PyValueError::new_err)?;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            PyValueError::new_err("timeout_seconds cannot be represented as a shutdown deadline")
        })?;
        let receipt = {
            let borrowed = slf.borrow(py);
            let session: &PyRuntimeSession = &borrowed;
            py.detach(|| {
                let record = session.host.try_lock_until(deadline).ok_or(
                    c2_core::LifecycleError::ShutdownWaitTimeout {
                        stage: "shutdown_owner_wait",
                    },
                )?;
                let target = match (&record.failed_start_receipt, &record.host) {
                    (Some(receipt), _) => RuntimeShutdownTarget::FailedStart(receipt),
                    (None, Some(host)) => RuntimeShutdownTarget::Host(host),
                    (None, None) => RuntimeShutdownTarget::ClientsOnly,
                };
                session.inner.begin_session_retirement(target, deadline)
            })
        };
        Ok(PyRuntimeShutdownWait {
            session: slf,
            receipt,
            deadline,
            timeout_seconds,
        })
    }

    #[pyo3(signature = (route_names=None, relay_anchor_address=None, timeout_seconds=5.0))]
    fn shutdown<'py>(
        &self,
        py: Python<'py>,
        route_names: Option<Vec<String>>,
        relay_anchor_address: Option<String>,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        // Validate before taking the Host or changing any session state.
        let timeout = checked_timeout(timeout_seconds).map_err(PyValueError::new_err)?;
        let _ = relay_anchor_address;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            PyValueError::new_err("timeout_seconds cannot be represented as a shutdown deadline")
        })?;
        let receipt = py
            .detach(|| self.inner.retirement_receipt(deadline))
            .map_err(|error| shutdown_error_to_py(py, error, timeout_seconds))?;
        if let Some(receipt) = receipt {
            if route_names.is_some() {
                let check_names = py
                    .detach(|| {
                        let record = self.host.try_lock_until(deadline).ok_or(
                            c2_core::LifecycleError::ShutdownWaitTimeout {
                                stage: "shutdown_owner_wait",
                            },
                        )?;
                        if record.failed_start_receipt.is_some() {
                            return Ok(false);
                        }
                        let host = record.host.clone();
                        drop(record);
                        Ok::<_, c2_core::LifecycleError>(match host {
                            Some(host) => host.shutdown_outcome_until(deadline)?.is_none(),
                            None => true,
                        })
                    })
                    .map_err(|error| shutdown_error_to_py(py, error, timeout_seconds))?;
                if check_names {
                    py.detach(|| {
                        self.check_shutdown_route_names(route_names.as_deref(), Some(deadline))
                    })?;
                }
            }
            let (outcome, completed) = py.detach(|| receipt.observe_until(deadline));
            return self.project_shutdown_observation(py, outcome, completed);
        }
        let (receipt_result, host) = py.detach(|| {
            let record = self.host.lock();
            if let Some(receipt) = record.failed_start_receipt.as_ref() {
                // Keep this Host ownership lock through observation and local
                // cleanup, but release it before reacquiring the Python GIL.
                // A later ensure_host cannot publish a Host that we then clear.
                let (outcome, completed) = receipt.observe_with_timeout(timeout);
                let retired_bridge = if completed {
                    self.registrations.lock().clear();
                    self.server_bridge.lock().take()
                } else {
                    None
                };
                (Some((outcome, completed, retired_bridge)), None)
            } else {
                (None, record.host.clone())
            }
        });
        if let Some((outcome, completed, retired_bridge)) = receipt_result {
            drop(retired_bridge);
            let dict = shutdown_outcome_to_dict(py, outcome)?;
            dict.set_item("completed", completed)?;
            return Ok(dict);
        }
        if host
            .as_ref()
            .is_none_or(|host| host.shutdown_outcome().is_none())
        {
            self.check_shutdown_route_names(route_names.as_deref(), None)?;
        }
        let (outcome, completed) = py.detach(|| {
            // Completion and the projected result are one immutable native
            // observation. Rust may finish while this thread reacquires the
            // GIL; never combine that later completion with an earlier pending
            // result whose route outcomes have not been consumed by Python.
            if let Some(host) = host.as_ref() {
                let pending = host.shutdown_with_timeout(timeout);
                match host.shutdown_outcome() {
                    Some(terminal) => (terminal, true),
                    None => (pending, false),
                }
            } else {
                let outcome = self.inner.shutdown_without_host(timeout);
                let completed = !shutdown_outcome_is_incomplete(&outcome);
                (outcome, completed)
            }
        });
        if completed {
            self.registrations.lock().clear();
            self.server_bridge.lock().take();
        }
        // Keep the Host journal for repeated consumption and observation. A
        // pending drain retains every Registration instead of invoking Drop close.
        let dict = shutdown_outcome_to_dict(py, outcome)?;
        dict.set_item("completed", completed)?;
        Ok(dict)
    }

    /// Move an unstarted lifecycle into a fully prepared replacement. Rust
    /// owns the capability transfer; Python never observes or clones it.
    fn transfer_unstarted_lifecycle_to(&self, replacement: &PyRuntimeSession) -> PyResult<()> {
        self.inner
            .transfer_unstarted_lifecycle_to(&replacement.inner)
            .map_err(lifecycle_error_to_py)
    }

    fn clear_server_identity(&self) -> PyResult<()> {
        if self
            .host
            .lock()
            .host
            .as_ref()
            .is_some_and(|host| host.shutdown_outcome().is_none())
        {
            return Err(PyRuntimeError::new_err(
                "cannot clear server identity while the Core host is active",
            ));
        }
        // Core now reports why identity reset is refused (pending native
        // teardown), so this must never silently report success.
        self.inner
            .clear_server_identity()
            .map_err(runtime_configuration_error_to_py)
    }

    fn clear_relay_projection_cache(&self) {
        self.inner.clear_relay_projection_cache();
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    #[pyo3(signature = (address, route_name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, *, timeout_seconds=None, connect_attempt=None))]
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
        timeout_seconds: Option<f64>,
        connect_attempt: Option<&PyConnectAttempt>,
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
            timeout_seconds,
            connect_attempt,
        )
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    #[pyo3(signature = (address, route_name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, *, timeout_seconds=None, connect_attempt=None))]
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
        timeout_seconds: Option<f64>,
        connect_attempt: Option<&PyConnectAttempt>,
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
            timeout_seconds,
            connect_attempt,
        )
    }

    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    #[pyo3(signature = (route_name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, *, timeout_seconds=None, connect_attempt=None))]
    fn connect_via_relay(
        &self,
        py: Python<'_>,
        route_name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        timeout_seconds: Option<f64>,
        connect_attempt: Option<&PyConnectAttempt>,
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
            Connect::RelayAware,
            timeout_seconds,
            connect_attempt,
        )
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

// `PyRetiredMemoryObservation` intentionally exposes no Python surface: the
// observation lifecycle is decided by real owners (weak handles detach when
// the last producer/lease owner is gone), so there is no fence to mark and no
// state to read from Python. It exists only as the opaque handoff token the
// registry moves between sessions.
#[pymethods]
impl PyRetiredMemoryObservation {}

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

/// Human-readable reminder attached to the native snapshot: the budget cells
/// are C-Two-owned accounting scopes, not process RSS.
const MEMORY_ACCOUNTING_NOTE: &str =
    "C-Two-owned IPC backing and live reassembly charges; not process RSS";

fn call_execution_snapshot_dict<'py>(
    py: Python<'py>,
    snapshot: &c2_core::CallExecutionSnapshot,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("max_operations", snapshot.max_operations)?;
    dict.set_item("max_retained_bytes", snapshot.max_retained_bytes)?;
    dict.set_item("used_operations", snapshot.used_operations)?;
    dict.set_item("used_retained_bytes", snapshot.used_retained_bytes)?;
    dict.set_item("peak_operations", snapshot.peak_operations)?;
    dict.set_item("peak_retained_bytes", snapshot.peak_retained_bytes)?;
    dict.set_item("rejected_reservations", snapshot.rejected_reservations)?;
    dict.set_item("closed", snapshot.closed)?;
    Ok(dict)
}

fn memory_cell_dict<'py>(
    py: Python<'py>,
    cell: &c2_core::MemoryCellStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("limit_bytes", cell.limit_bytes)?;
    dict.set_item("used_bytes", cell.used_bytes)?;
    dict.set_item("peak_bytes", cell.peak_bytes)?;
    dict.set_item("rejected_allocations", cell.rejected_allocations)?;
    dict.set_item("rejected_bytes", cell.rejected_bytes)?;
    Ok(dict)
}

fn memory_scope_dict<'py>(
    py: Python<'py>,
    role: &str,
    state: &str,
    scope: &c2_core::MemoryScopeStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("role", role)?;
    dict.set_item("state", state)?;
    let limits = PyDict::new(py);
    limits.set_item("shm_backing_bytes", scope.limits.shm_backing_budget_bytes)?;
    limits.set_item("file_backing_bytes", scope.limits.file_backing_budget_bytes)?;
    limits.set_item(
        "live_reassembly_bytes",
        scope.limits.live_reassembly_budget_bytes,
    )?;
    dict.set_item("limits", limits)?;
    let cells = PyDict::new(py);
    cells.set_item("shm", memory_cell_dict(py, &scope.shm)?)?;
    cells.set_item("file", memory_cell_dict(py, &scope.file)?)?;
    cells.set_item("reassembly", memory_cell_dict(py, &scope.reassembly)?)?;
    dict.set_item("cells", cells)?;
    Ok(dict)
}

fn unregister_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: UnregisterOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", outcome.route_name)?;
    dict.set_item("local_removed", outcome.local_removed)?;
    dict.set_item("close", route_close_outcome_to_dict(py, &outcome.close)?)?;
    match outcome.relay_error {
        Some(error) => dict.set_item("relay_error", relay_cleanup_error_to_dict(py, &error)?)?,
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
        .map(|close| route_close_outcome_to_dict(py, &close).map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("route_outcomes", PyList::new(py, route_outcomes)?)?;
    let relay_errors = outcome
        .relay_errors
        .into_iter()
        .map(|error| relay_cleanup_error_to_dict(py, &error).map(Bound::unbind))
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("relay_errors", PyList::new(py, relay_errors)?)?;
    dict.set_item("server_was_started", outcome.server_was_started)?;
    dict.set_item("ipc_clients_drained", outcome.ipc_clients_drained)?;
    dict.set_item("http_clients_drained", outcome.http_clients_drained)?;
    dict.set_item("route_close_error", outcome.route_close_error)?;
    dict.set_item("runtime_barrier_error", outcome.runtime_barrier_error)?;
    dict.set_item("ipc_client_close_error", outcome.ipc_client_close_error)?;
    dict.set_item(
        "ipc_clients_abnormally_closed",
        outcome.ipc_clients_abnormally_closed,
    )?;
    let client_close_details = outcome
        .ipc_client_close_details
        .into_iter()
        .map(|detail| {
            let item = PyDict::new(py);
            item.set_item("address", detail.address)?;
            item.set_item("reason", detail.reason)?;
            Ok(item.unbind())
        })
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item(
        "ipc_client_close_details",
        PyList::new(py, client_close_details)?,
    )?;
    Ok(dict)
}

/// Whether a shutdown outcome still has work that no later close may treat
/// as finished.
///
/// An incomplete transaction is not terminal: the native host keeps the
/// actual server, its route journal and the outgoing client barriers, and a
/// later consume observes the real completion. No SDK-side state may claim
/// the host stopped, release its registration slots, or clear its identity
/// while this is true.
fn shutdown_outcome_is_incomplete(outcome: &ShutdownOutcome) -> bool {
    outcome.runtime_barrier_error.is_some()
        || outcome.route_close_error.is_some()
        || outcome.ipc_client_close_error.is_some()
        || outcome
            .route_outcomes
            .iter()
            .any(|route| !route.active_drained || route.close_error.is_some())
}

fn lifecycle_policy_to_dict<'py>(
    py: Python<'py>,
    policy: &ServerLifecyclePolicy,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    match policy {
        ServerLifecyclePolicy::Persistent => {
            dict.set_item("policy", "persistent")?;
            dict.set_item("owner_bound", false)?;
            dict.set_item("owner_missing_grace_seconds", py.None())?;
        }
        ServerLifecyclePolicy::OwnerBound {
            owner_missing_grace,
        } => {
            dict.set_item("policy", "owner_bound")?;
            dict.set_item("owner_bound", true)?;
            dict.set_item(
                "owner_missing_grace_seconds",
                owner_missing_grace.as_secs_f64(),
            )?;
        }
    }
    Ok(dict)
}

fn parse_lifecycle_policy(
    policy: Option<&str>,
    owner_missing_grace_seconds: Option<f64>,
) -> PyResult<ServerLifecyclePolicy> {
    match policy {
        None | Some("persistent") => {
            if owner_missing_grace_seconds.is_some() {
                return Err(PyValueError::new_err(
                    "owner_missing_grace_seconds is only valid with the owner_bound policy",
                ));
            }
            Ok(ServerLifecyclePolicy::Persistent)
        }
        Some("owner_bound") => {
            let seconds = owner_missing_grace_seconds.ok_or_else(|| {
                PyValueError::new_err(
                    "owner_bound requires an explicit owner_missing_grace_seconds window",
                )
            })?;
            let grace = checked_timeout(seconds).map_err(PyValueError::new_err)?;
            ServerLifecyclePolicy::owner_bound(grace).map_err(PyValueError::new_err)
        }
        Some(other) => Err(PyValueError::new_err(format!(
            "invalid lifecycle policy {other:?}; expected \"persistent\" or \"owner_bound\"",
        ))),
    }
}

fn lifecycle_phase_name(phase: &HostLifecyclePhase) -> PyResult<&'static str> {
    Ok(match phase {
        HostLifecyclePhase::Persistent => "persistent",
        HostLifecyclePhase::Armed => "armed",
        HostLifecyclePhase::OwnerMissing => "owner_missing",
        HostLifecyclePhase::Draining => "draining",
        HostLifecyclePhase::Finished => "finished",
        HostLifecyclePhase::WatcherIoError(_) => "watcher_io_error",
    })
}

fn lifecycle_snapshot_to_dict<'py>(
    py: Python<'py>,
    snapshot: &HostLifecycleSnapshot,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("policy", lifecycle_policy_to_dict(py, &snapshot.policy)?)?;
    dict.set_item("phase", lifecycle_phase_name(&snapshot.phase)?)?;
    match &snapshot.phase {
        HostLifecyclePhase::WatcherIoError(message) => {
            dict.set_item("watcher_error", message)?;
        }
        _ => dict.set_item("watcher_error", py.None())?,
    }
    // A finished transaction is terminal; every earlier phase, including a
    // bounded draining transaction, still has native work outstanding.
    dict.set_item("terminal", snapshot.phase == HostLifecyclePhase::Finished)?;
    dict.set_item("listener_closed", snapshot.listener_closed)?;
    dict.set_item("work_drained", snapshot.work_drained)?;
    let leases: HostClientHeldLeases = snapshot.client_held_leases;
    let held = PyDict::new(py);
    held.set_item("response_shm_bytes", leases.response_shm_bytes)?;
    held.set_item("response_file_bytes", leases.response_file_bytes)?;
    held.set_item("reassembly_bytes", leases.reassembly_bytes)?;
    dict.set_item("client_held_leases", held)?;
    Ok(dict)
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyRuntimeSession>()?;
    module.add_class::<PyRetiredMemoryObservation>()?;
    Ok(())
}
