//! PyO3 projection of the language-neutral C-Two Core runtime.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};

use c2_config::{ConfigField, DomainLimitsPatch, ServerConfigPatch};
use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract, MethodAccess,
    MethodMode, PORTABLE_CONTRACT_SCHEMA, contract_descriptor_sha256_hex,
};
use c2_core::{
    ClientConfigPatch, Connect, FailedStartShutdownReceipt, Host, HostLifecycleSnapshot,
    HostOptions, MethodDefinition, RegisterOutcome, Registration, Runtime, RuntimeOptions,
    RuntimeShutdownReceipt, RuntimeShutdownTarget, ServiceConcurrencyMode, ServiceDefinition,
    ShutdownOutcome, UnregisterOutcome,
};

use crate::config_ffi::{PyConnectAttempt, config_field, config_uint};
use crate::core_error_ffi::{
    core_error_to_py, lifecycle_error_to_py, relay_cleanup_error_to_dict,
    route_close_outcome_to_dict,
};
use crate::core_ffi::{PyCoreClient, PyCoreService};
use crate::endpoint_ffi::{PyEndpointCredential, PyLocalEndpointContext};
use crate::local_copy_ffi::{PyLocalCopyTracker, local_copy_stats_dict};
use crate::route_concurrency_ffi::PyRouteConcurrency;

fn checked_shutdown_deadline(timeout_seconds: f64) -> PyResult<(Duration, Instant)> {
    let timeout = c2_config::timeout_from_seconds(timeout_seconds)
        .map_err(|error| PyValueError::new_err(format!("timeout_seconds: {error}")))?;
    let deadline = c2_config::deadline_after(Instant::now(), timeout)
        .map_err(|error| PyValueError::new_err(format!("timeout_seconds: {error}")))?;
    Ok((timeout, deadline))
}

#[cfg(test)]
mod connect_attempt_tests {
    use super::*;

    #[test]
    fn retirement_owner_lock_timeout_does_not_seal_or_hold_the_gil() {
        Python::initialize();
        Python::attach(|py| {
            let session = Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
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
        // This probe replaces a public exception initializer. Run it alone in
        // a child process so the normal parallel Native suite shares no mutation.
        if std::env::var_os("C2_TEST_SDK_SHUTDOWN_REENTRY_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime_session_ffi::connect_attempt_tests::sdk_shutdown_projects_owner_timeout_after_releasing_binding_lock", "--nocapture"])
                .env("C2_TEST_SDK_SHUTDOWN_REENTRY_CHILD", "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
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
            let session = Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            runtime
                .bind(py)
                .setattr("_runtime_session", session.clone_ref(py))
                .unwrap();
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

    fn run_in_child_with_timeout(test_name: &str, env_key: &str) -> bool {
        if std::env::var_os(env_key).is_some() {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(env_key, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return true;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "isolated child test {test_name} timed out (possible deadlock)\n{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn shutdown_drops_server_bridge_after_releasing_bridge_lock() {
        if run_in_child_with_timeout(
            "runtime_session_ffi::connect_attempt_tests::shutdown_drops_server_bridge_after_releasing_bridge_lock",
            "C2_TEST_SHUTDOWN_BRIDGE_DROP_CHILD",
        ) {
            return;
        }
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let globals = PyDict::new(py);
            globals.set_item("session", session.clone_ref(py)).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
events = []
class ReenteringBridge:
    def __init__(self, session):
        self._session = session
    def __del__(self):
        events.append("del_entered")
        bridge = self._session.ensure_server_bridge()
        assert bridge is not None
        events.append("del_reentered")
bridge = ReenteringBridge(session)
"#
                ),
                Some(&globals),
                None,
            )
            .unwrap();
            let bridge = globals.get_item("bridge").unwrap().unwrap().unbind();
            globals.del_item("bridge").unwrap();
            {
                let borrowed = session.borrow(py);
                borrowed.server_bridge.lock().bridge = Some(bridge);
            }
            let outcome = session.borrow(py).shutdown(py, 5.0).unwrap();
            let completed: bool = outcome
                .get_item("completed")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            let events: Vec<String> = globals
                .get_item("events")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            session.borrow(py).clear_retired_session(py);
            assert!(completed);
            assert_eq!(events, vec!["del_entered", "del_reentered"]);
        });
    }

    #[test]
    fn publish_server_bridge_preserves_winner_and_drops_loser_outside_lock() {
        if run_in_child_with_timeout(
            "runtime_session_ffi::connect_attempt_tests::publish_server_bridge_preserves_winner_and_drops_loser_outside_lock",
            "C2_TEST_BRIDGE_RACE_CHILD",
        ) {
            return;
        }
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let globals = PyDict::new(py);
            globals.set_item("session", session.clone_ref(py)).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
events = []
winner = object()
class LoserBridge:
    def __init__(self, session):
        self._session = session
    def __del__(self):
        events.append("loser_del_entered")
        seen = self._session.ensure_server_bridge()
        assert seen is winner
        events.append("loser_del_reentered")
loser = LoserBridge(session)
"#
                ),
                Some(&globals),
                None,
            )
            .unwrap();
            let winner = globals.get_item("winner").unwrap().unwrap().unbind();
            let loser = globals.get_item("loser").unwrap().unwrap().unbind();
            globals.del_item("loser").unwrap();
            let chosen = {
                let borrowed = session.borrow(py);
                borrowed.server_bridge.lock().bridge = Some(winner.clone_ref(py));
                publish_server_bridge(py, &borrowed.server_bridge, loser).unwrap()
            };
            let same_winner = chosen.bind(py).is(winner.bind(py));
            let stored_is_winner = session
                .borrow(py)
                .server_bridge
                .lock()
                .bridge
                .as_ref()
                .unwrap()
                .bind(py)
                .is(winner.bind(py));
            let events: Vec<String> = globals
                .get_item("events")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            session.borrow(py).clear_retired_session(py);
            assert!(same_winner);
            assert!(stored_is_winner);
            assert_eq!(events, vec!["loser_del_entered", "loser_del_reentered"]);
        });
    }

    #[test]
    fn ensure_server_bridge_rejects_publication_after_retirement() {
        if run_in_child_with_timeout(
            "runtime_session_ffi::connect_attempt_tests::ensure_server_bridge_rejects_publication_after_retirement",
            "C2_TEST_BRIDGE_RETIRED_CHILD",
        ) {
            return;
        }
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let globals = PyDict::new(py);
            globals.set_item("session", session.clone_ref(py)).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
import threading
import c_two.transport.server.native as native_mod

entered = threading.Event()
release = threading.Event()
original_init = native_mod.NativeServerBridge.__init__
def gated_init(self, *args, **kwargs):
    entered.set()
    assert release.wait(5.0)
    original_init(self, *args, **kwargs)
native_mod.NativeServerBridge.__init__ = gated_init
"#
                ),
                Some(&globals),
                None,
            )
            .unwrap();

            let worker_session = session.clone_ref(py);
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    let outcome = worker_session
                        .bind(py)
                        .call_method0("ensure_server_bridge")
                        .err()
                        .map(|err| {
                            err.value(py)
                                .getattr("lifecycle_kind")
                                .and_then(|value| value.extract::<String>())
                                .unwrap_or_default()
                        });
                    result_tx.send(outcome).unwrap();
                });
            });

            let entered = globals.get_item("entered").unwrap().unwrap();
            let release = globals.get_item("release").unwrap().unwrap();
            assert!(entered.call_method1("wait", (5.0,)).unwrap().extract::<bool>().unwrap());

            let wait = PyRuntimeSession::begin_shutdown(session.clone_ref(py), py, 5.0).unwrap();
            let outcome = wait.observe(py).unwrap();
            assert!(outcome
                .get_item("completed")
                .unwrap()
                .unwrap()
                .extract::<bool>()
                .unwrap());

            release.call_method0("set").unwrap();
            let kind = py.detach(move || result_rx.recv_timeout(Duration::from_secs(5)).unwrap());
            py.detach(|| worker.join()).unwrap();
            py.run(
                pyo3::ffi::c_str!("native_mod.NativeServerBridge.__init__ = original_init"),
                Some(&globals),
                None,
            )
            .unwrap();

            assert_eq!(kind.as_deref(), Some("runtime_retired"));
            assert!(session.borrow(py).server_bridge.lock().bridge.is_none());
            assert!(session.borrow(py).server_bridge.lock().retired);
            let second_err = session
                .bind(py)
                .call_method0("ensure_server_bridge")
                .unwrap_err();
            let second_kind: String = second_err
                .value(py)
                .getattr("lifecycle_kind")
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(second_kind, "runtime_retired");
        });
    }

    #[test]
    fn stale_shutdown_generation_preserves_newer_host_state() {
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let bridge = py.eval(pyo3::ffi::c_str!("object()"), None, None).unwrap().unbind();
            {
                let borrowed = session.borrow(py);
                borrowed.host.lock().generation = 2;
                borrowed.server_bridge.lock().bridge = Some(bridge.clone_ref(py));
            }
            // A late observer from Host generation 1 must not clear Host generation 2's bridge.
            session.borrow(py).clear_completed_host_generation(py, 1);
            assert!(session
                .borrow(py)
                .server_bridge
                .lock()
                .bridge
                .as_ref()
                .is_some_and(|stored| stored.bind(py).is(bridge.bind(py))));
            // Matching generation 2 clears it cleanly.
            session.borrow(py).clear_completed_host_generation(py, 2);
            assert!(session.borrow(py).server_bridge.lock().bridge.is_none());
        });
    }

    #[test]
    fn host_lock_methods_release_gil_before_acquiring_host_lock() {
        if run_in_child_with_timeout(
            "runtime_session_ffi::connect_attempt_tests::host_lock_methods_release_gil_before_acquiring_host_lock",
            "C2_TEST_HOST_LOCK_GIL_ORDER_CHILD",
        ) {
            return;
        }
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let borrowed = session.borrow(py);
            let host_owner = borrowed.host.lock();

            let s1 = session.clone_ref(py);
            let s2 = session.clone_ref(py);
            let s3 = session.clone_ref(py);
            let s4 = session.clone_ref(py);
            let s5 = session.clone_ref(py);
            let s6 = session.clone_ref(py);
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();

            let t1 = entered_tx.clone();
            let w1 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t1.send("host_started").unwrap();
                    s1.borrow(py).host_started(py)
                })
            });
            let t2 = entered_tx.clone();
            let w2 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t2.send("server_credential").unwrap();
                    s2.borrow(py).server_credential(py).is_none()
                })
            });
            let t3 = entered_tx.clone();
            let w3 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t3.send("native_lifecycle_snapshot").unwrap();
                    s3.borrow(py)
                        .native_lifecycle_snapshot(py)
                        .unwrap()
                        .is_none()
                })
            });
            let t4 = entered_tx.clone();
            let w4 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t4.send("native_terminal_outcome").unwrap();
                    s4.borrow(py)
                        .native_terminal_outcome(py)
                        .unwrap()
                        .is_none()
                })
            });
            let t5 = entered_tx.clone();
            let w5 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t5.send("set_relay_anchor_address").unwrap();
                    s5.borrow(py).set_relay_anchor_address(py, None).unwrap();
                    true
                })
            });
            let t6 = entered_tx;
            let w6 = std::thread::spawn(move || {
                Python::attach(|py| {
                    t6.send("clear_server_identity").unwrap();
                    s6.borrow(py).clear_server_identity(py).unwrap();
                    true
                })
            });

            py.detach(move || {
                for _ in 0..6 {
                    entered_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("worker did not attach to Python before locking host");
                }
                // Wait briefly so every attached worker has detached and parked on `self.host.lock()`.
                std::thread::sleep(Duration::from_millis(25));
                // Reacquiring the GIL while still holding `host_owner` succeeds only
                // if none of the 6 waiters kept the GIL while blocking on `self.host.lock()`.
                let check: i32 = Python::attach(|py| {
                    py.eval(pyo3::ffi::c_str!("1 + 1"), None, None)
                        .unwrap()
                        .extract()
                        .unwrap()
                });
                assert_eq!(check, 2);
            });
            drop(host_owner);
            py.detach(|| {
                assert!(!w1.join().unwrap());
                assert!(w2.join().unwrap());
                assert!(w3.join().unwrap());
                assert!(w4.join().unwrap());
                assert!(w5.join().unwrap());
                assert!(w6.join().unwrap());
            });
        });
    }

    #[test]
    fn ensure_host_projects_lifecycle_error_after_releasing_host_lock() {
        if run_in_child_with_timeout(
            "runtime_session_ffi::connect_attempt_tests::ensure_host_projects_lifecycle_error_after_releasing_host_lock",
            "C2_TEST_ENSURE_HOST_LOCK_REENTRY_CHILD",
        ) {
            return;
        }
        Python::initialize();
        Python::attach(|py| {
            let session =
                Py::new(py, PyRuntimeSession::new(None, None, false).unwrap()).unwrap();
            let wait = PyRuntimeSession::begin_shutdown(session.clone_ref(py), py, 5.0).unwrap();
            let outcome = wait.observe(py).unwrap();
            assert!(outcome
                .get_item("completed")
                .unwrap()
                .unwrap()
                .extract::<bool>()
                .unwrap());

            let globals = PyDict::new(py);
            globals.set_item("session", session.clone_ref(py)).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
import c_two as cc
observed = []
original = cc.error.CoreError.__init__
def observe_error(self, category, message):
    observed.append(session.host_started)
    original(self, category, message)
cc.error.CoreError.__init__ = observe_error
"#
                ),
                Some(&globals),
                None,
            )
            .unwrap();
            let err = session.borrow(py).ensure_host_started(py).unwrap_err();
            let kind: String = err
                .value(py)
                .getattr("lifecycle_kind")
                .unwrap()
                .extract()
                .unwrap();
            py.run(
                pyo3::ffi::c_str!("cc.error.CoreError.__init__ = original"),
                Some(&globals),
                None,
            )
            .unwrap();
            let observed: Vec<bool> = globals
                .get_item("observed")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(kind, "runtime_retired");
            assert_eq!(observed, vec![false]);
        });
    }
}

#[pyclass(name = "RuntimeSession", frozen)]
pub struct PyRuntimeSession {
    inner: Arc<Runtime>,
    host: Mutex<HostRecord>,
    registrations: Mutex<HashMap<String, Arc<SessionRegistration>>>,
    server_bridge: Mutex<ServerBridgeSlot>,
}

/// One live route registration shared by the session map and Python handles.
///
/// The session keeps the inner `Registration` alive for the whole route
/// lifetime; a dropped Python `NativeRegistration` never unregisters. Taking
/// the inner value out (explicit unregister, retirement) seals every handle
/// minted from this registration against later same-name registrations.
pub(crate) struct SessionRegistration {
    route_name: String,
    observer: c2_core::RegistrationObserver,
    registration: Mutex<Option<Registration>>,
}

impl SessionRegistration {
    fn new(route_name: String, registration: Registration) -> Arc<Self> {
        let observer = registration.observer();
        Arc::new(Self {
            route_name,
            observer,
            registration: Mutex::new(Some(registration)),
        })
    }

    fn publisher(
        self: &Arc<Self>,
        topic_name: &str,
    ) -> Result<c2_core::TopicPublisher, c2_core::Error> {
        let guard = self.registration.lock();
        match guard.as_ref() {
            Some(registration) => registration.publisher(topic_name),
            None => Err(crate::topic_ffi::registration_closed_error(
                &self.route_name,
            )),
        }
    }

    fn is_closed(&self) -> bool {
        self.registration
            .lock()
            .as_ref()
            .is_none_or(c2_core::Registration::is_closed)
    }

    fn active_subscribers(&self) -> usize {
        self.observer.active_subscribers()
    }

    /// Remove and close the inner registration, reporting its native outcome.
    ///
    /// The inner value is taken out under the lock and drained strictly
    /// outside it: while the native close waits for active subscribers,
    /// `is_closed` reports `true` and `publisher` refuses with 707 instead of
    /// deadlocking on this mutex.
    fn close(&self) -> Result<UnregisterOutcome, c2_core::Error> {
        let Some(mut registration) = self.registration.lock().take() else {
            return Err(crate::topic_ffi::registration_closed_error(
                &self.route_name,
            ));
        };
        registration.close()
    }

    /// Take the inner registration out (session retirement); dropping it runs
    /// the idempotent native close.
    fn take(self: &Arc<Self>) -> Option<Registration> {
        self.registration.lock().take()
    }
}

/// Python handle to one session registration. Keeps the shared slot alive;
/// dropping it keeps the route registered.
#[pyclass(name = "NativeRegistration", frozen, skip_from_py_object)]
pub(crate) struct PyNativeRegistration {
    shared: Arc<SessionRegistration>,
}

#[pymethods]
impl PyNativeRegistration {
    #[getter]
    fn route_name(&self) -> &str {
        &self.shared.route_name
    }

    /// Mint a publisher bound to this registration instance. Fails with
    /// `ResourceClosed` (707) once the registration ended, and with 713 for a
    /// topic the contract does not declare.
    fn topic_publisher(
        &self,
        py: Python<'_>,
        topic_name: &str,
    ) -> PyResult<crate::topic_ffi::PyTopicPublisher> {
        py.detach(|| self.shared.publisher(topic_name))
            .map(crate::topic_ffi::PyTopicPublisher::new)
            .map_err(core_error_to_py)
    }

    #[getter]
    fn is_closed(&self, py: Python<'_>) -> bool {
        py.detach(|| self.shared.is_closed())
    }

    #[getter]
    fn active_subscribers(&self, py: Python<'_>) -> usize {
        py.detach(|| self.shared.active_subscribers())
    }
}

#[derive(Default)]
struct HostRecord {
    host: Option<Host>,
    failed_start_receipt: Option<FailedStartShutdownReceipt>,
    generation: u64,
}

#[derive(Default)]
struct ServerBridgeSlot {
    bridge: Option<Py<PyAny>>,
    retired: bool,
}

enum EnsureHostError {
    Lifecycle(c2_core::LifecycleError),
    Core(c2_core::Error),
    Runtime(&'static str),
}

fn ensure_host_error_to_py(error: EnsureHostError) -> PyErr {
    match error {
        EnsureHostError::Lifecycle(error) => lifecycle_error_to_py(error),
        EnsureHostError::Core(error) => core_error_to_py(error),
        EnsureHostError::Runtime(message) => PyRuntimeError::new_err(message),
    }
}

fn publish_server_bridge(
    py: Python<'_>,
    slot: &Mutex<ServerBridgeSlot>,
    candidate: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let (outcome, discarded) = {
        let mut guard = slot.lock();
        if guard.retired {
            (Err(c2_core::LifecycleError::RuntimeRetired), Some(candidate))
        } else if let Some(existing) = guard.bridge.as_ref() {
            (Ok(existing.clone_ref(py)), Some(candidate))
        } else {
            guard.bridge = Some(candidate.clone_ref(py));
            (Ok(candidate), None)
        }
    };
    drop(discarded);
    outcome.map_err(lifecycle_error_to_py)
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

impl PyRuntimeSession {
    fn clear_retired_session(&self, py: Python<'_>) {
        // Core has permanently retired this session. Seal bridge publication
        // and move native registrations and the Python bridge out of their
        // locks before dropping either value so no destructor runs under a lock.
        let retired_bridge = py.detach(|| {
            let retired_registrations: Vec<Option<Registration>> = self
                .registrations
                .lock()
                .drain()
                .map(|(_, registration)| registration.take())
                .collect();
            let bridge = {
                let mut slot = self.server_bridge.lock();
                slot.retired = true;
                slot.bridge.take()
            };
            drop(retired_registrations);
            bridge
        });
        drop(retired_bridge);
    }

    fn clear_completed_host_generation(&self, py: Python<'_>, observed_generation: u64) {
        // Low-level shutdown is restartable: clear registrations and bridge
        // only when no newer Host generation has been published since this
        // observation started, and drop values strictly outside all locks.
        let retired_bridge = py.detach(|| {
            let record = self.host.lock();
            if record.generation != observed_generation {
                return None;
            }
            let retired_registrations: Vec<Option<Registration>> = self
                .registrations
                .lock()
                .drain()
                .map(|(_, registration)| registration.take())
                .collect();
            let bridge = self.server_bridge.lock().bridge.take();
            drop(record);
            drop(retired_registrations);
            bridge
        });
        drop(retired_bridge);
    }

    fn project_shutdown_observation<'py>(
        &self,
        py: Python<'py>,
        outcome: ShutdownOutcome,
        completed: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        if completed {
            self.clear_retired_session(py);
        }
        let dict = shutdown_outcome_to_dict(py, outcome)?;
        dict.set_item("completed", completed)?;
        Ok(dict)
    }

    /// Start this session's one Core host.
    ///
    /// Reuse the live Host, or publish a fresh Host after native startup succeeds.
    /// Callers must invoke this detached from the Python GIL and project any
    /// error only after the lock-bearing call returns.
    fn ensure_host(&self) -> Result<Host, EnsureHostError> {
        let mut host_guard = self.host.lock();
        self.inner
            .ensure_active()
            .map_err(EnsureHostError::Lifecycle)?;
        if let Some(host) = host_guard.host.as_ref() {
            if host.shutdown_outcome().is_none() {
                if !host.is_running() || self.inner.has_pending_teardown() {
                    return Err(EnsureHostError::Runtime(
                        "Core host is still draining; consume terminal shutdown before restarting",
                    ));
                }
                return Ok(host.clone());
            }
            if !self.registrations.lock().is_empty() {
                return Err(EnsureHostError::Runtime(
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
            host_guard.generation = host_guard.generation.wrapping_add(1);
        } else if let Some(receipt) = failed_receipt {
            host_guard.failed_start_receipt = Some(receipt);
        }
        drop(host_guard);
        started.map_err(EnsureHostError::Core)
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
            .map(|client| PyCoreClient::new(client, self.local_copy_tracker()))
            .map_err(core_error_to_py)
    }
}

#[pymethods]
impl PyRuntimeSession {
    /// Validate the exact native shutdown budget before serve installs signals
    /// or prints its banner, without starting retirement or waiting.
    fn validate_shutdown_timeout(&self, timeout_seconds: f64) -> PyResult<()> {
        checked_shutdown_deadline(timeout_seconds).map(|_| ())
    }

    fn connection_events(&self) -> crate::connection_ffi::PyConnectionEvents {
        crate::connection_ffi::PyConnectionEvents {
            inner: self.inner.connection_events(),
        }
    }

    #[new]
    #[pyo3(signature = (*, server_id=None, startup_timeout=None, use_process_relay_anchor=true))]
    fn new(
        server_id: Option<String>,
        startup_timeout: Option<f64>,
        use_process_relay_anchor: bool,
    ) -> PyResult<Self> {
        let inner = Runtime::new(RuntimeOptions {
            use_process_relay_anchor,
            ..RuntimeOptions::default()
        })
        .map_err(runtime_configuration_error_to_py)?;
        if server_id.is_some() || startup_timeout.is_some() {
            inner
                .update_server_config(ServerConfigPatch {
                    server_id: server_id.map_or(ConfigField::Keep, ConfigField::Set),
                    startup_timeout: startup_timeout
                        .map(parse_startup_timeout)
                        .transpose()?
                        .map_or(ConfigField::Keep, ConfigField::Set),
                    ..ServerConfigPatch::default()
                })
                .map_err(runtime_configuration_error_to_py)?;
        }
        Ok(Self {
            inner: Arc::new(inner),
            host: Mutex::new(HostRecord::default()),
            registrations: Mutex::new(HashMap::new()),
            server_bridge: Mutex::new(ServerBridgeSlot::default()),
        })
    }

    fn local_copy_tracker(&self) -> PyLocalCopyTracker {
        PyLocalCopyTracker::from_arc(self.inner.holds_tracker())
    }

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
            self.inner
                .set_captured_local_endpoint(context.inner.clone())
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

    /// Return "initiated", "absent" or "unconfirmed" for this Runtime's endpoint context.
    /// Only "initiated" confirms initiation; completion uses the original Host receipt.
    /// "absent" means this endpoint has no listener.
    #[pyo3(signature = (address, timeout_seconds=0.5))]
    fn shutdown_direct_ipc(
        &self,
        py: Python<'_>,
        address: &str,
        timeout_seconds: f64,
    ) -> PyResult<&'static str> {
        let timeout = crate::control_ffi::timeout_duration(timeout_seconds)?;
        let outcome = py
            .detach(|| self.inner.shutdown_direct_ipc(address, timeout))
            .map_err(runtime_configuration_error_to_py)?;
        Ok(crate::control_ffi::shutdown_outcome_name(outcome))
    }

    fn memory_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let stats = self.inner.memory_stats();
        let dict = PyDict::new(py);
        dict.set_item("client", role_memory_dict(py, &stats.client)?)?;
        dict.set_item("server", role_memory_dict(py, &stats.server)?)?;
        let retired = PyList::empty(py);
        for report in stats.retired {
            let role = role_memory_dict(py, &report.stats)?;
            role.set_item("role", report.role.as_str())?;
            retired.append(role)?;
        }
        dict.set_item("retired", retired)?;
        dict.set_item("holds", local_copy_stats_dict(py, &stats.holds)?)?;
        Ok(dict)
    }

    fn call_execution_snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let stats = self.inner.call_execution_snapshot();
        let dict = PyDict::new(py);
        dict.set_item("client", call_execution_snapshot_dict(py, &stats.client)?)?;
        dict.set_item("server", call_execution_snapshot_dict(py, &stats.server)?)?;
        let retired = PyList::empty(py);
        for report in stats.retired {
            let role = call_execution_snapshot_dict(py, &report.stats)?;
            role.set_item("role", report.role.as_str())?;
            retired.append(role)?;
        }
        dict.set_item("retired", retired)?;
        Ok(dict)
    }

    fn inherit_retirement(&self, previous: &Self) -> PyResult<()> {
        let handoff = previous
            .inner
            .capture_retirement_handoff()
            .map_err(lifecycle_error_to_py)?;
        self.inner
            .adopt_retirement_handoff(&handoff)
            .map_err(lifecycle_error_to_py)
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
        self.inner.server_overrides().server_id
    }

    #[getter]
    fn server_address(&self) -> Option<String> {
        self.inner.server_address()
    }

    /// Snapshot the existing host's bound listener without creating a host.
    #[getter]
    fn server_credential(&self, py: Python<'_>) -> Option<PyEndpointCredential> {
        py.detach(|| {
            self.host
                .lock()
                .host
                .as_ref()
                .and_then(Host::endpoint_credential)
        })
        .map(PyEndpointCredential::new)
    }

    #[getter]
    fn startup_timeout(&self) -> Option<f64> {
        self.inner
            .server_overrides()
            .startup_timeout
            .map(|timeout| timeout.as_secs_f64())
    }

    #[allow(clippy::too_many_arguments)]
    fn update_server_config(
        &self,
        py: Python<'_>,
        unset: &Bound<'_, PyAny>,
        server_id: &Bound<'_, PyAny>,
        startup_timeout: &Bound<'_, PyAny>,
        max_payload_size: &Bound<'_, PyAny>,
        max_execution_workers: &Bound<'_, PyAny>,
        memory_budget_bytes: &Bound<'_, PyAny>,
        file_spill_budget_bytes: &Bound<'_, PyAny>,
        max_outstanding_calls: &Bound<'_, PyAny>,
        outcome_max_entries: &Bound<'_, PyAny>,
        outcome_max_retained_bytes: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let patch = ServerConfigPatch {
            server_id: config_field(server_id, unset, |value| value.extract())?,
            startup_timeout: config_field(startup_timeout, unset, |value| {
                parse_startup_timeout(value.extract()?)
            })?,
            max_payload_size: config_field(max_payload_size, unset, |value| {
                config_uint(value, "max_payload_size")
            })?,
            max_execution_workers: config_field(max_execution_workers, unset, |value| {
                config_uint(value, "max_execution_workers")
            })?,
            outcome_max_entries: config_field(outcome_max_entries, unset, |value| {
                config_uint(value, "outcome_max_entries")
            })?,
            outcome_max_retained_bytes: config_field(outcome_max_retained_bytes, unset, |value| {
                config_uint(value, "outcome_max_retained_bytes")
            })?,
            domain_limits: domain_limits_patch(
                unset,
                memory_budget_bytes,
                file_spill_budget_bytes,
                max_outstanding_calls,
            )?,
        };
        py.detach(|| self.inner.update_server_config(patch))
            .map_err(runtime_configuration_error_to_py)
    }

    fn update_client_config(
        &self,
        py: Python<'_>,
        unset: &Bound<'_, PyAny>,
        memory_budget_bytes: &Bound<'_, PyAny>,
        file_spill_budget_bytes: &Bound<'_, PyAny>,
        max_outstanding_calls: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let domain_limits = domain_limits_patch(
            unset,
            memory_budget_bytes,
            file_spill_budget_bytes,
            max_outstanding_calls,
        )?;
        py.detach(|| {
            self.inner
                .update_client_config(ClientConfigPatch { domain_limits })
        })
        .map_err(runtime_configuration_error_to_py)
    }

    fn set_relay_anchor_address(
        &self,
        py: Python<'_>,
        relay_anchor_address: Option<String>,
    ) -> PyResult<()> {
        py.detach(|| {
            let host = self.host.lock();
            if host.host.is_some() {
                let requested = relay_anchor_address
                    .as_deref()
                    .map(str::trim)
                    .map(|value| value.trim_end_matches('/'));
                let current = self.inner.relay_anchor_address_override();
                if requested != current.as_deref() {
                    return Err("relay anchor is frozen after the Core host starts");
                }
                return Ok(());
            }
            self.inner.set_relay_anchor_address(relay_anchor_address);
            Ok(())
        })
        .map_err(PyRuntimeError::new_err)
    }

    #[getter]
    fn relay_anchor_address_override(&self) -> Option<String> {
        self.inner.relay_anchor_address_override()
    }

    #[getter]
    fn client_config_frozen(&self) -> bool {
        self.inner.client_config_frozen()
    }

    fn ensure_server_bridge<'py>(slf: PyRef<'py, Self>) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        {
            let guard = slf.server_bridge.lock();
            if guard.retired {
                drop(guard);
                return Err(lifecycle_error_to_py(
                    c2_core::LifecycleError::RuntimeRetired,
                ));
            }
            if let Some(server) = guard.bridge.as_ref() {
                return Ok(server.clone_ref(py));
            }
        }
        slf.inner
            .ensure_server()
            .map_err(runtime_configuration_error_to_py)?;
        let module = py.import("c_two.transport.server.native")?;
        let class = module.getattr("NativeServerBridge")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("local_copy_tracker", slf.local_copy_tracker())?;
        kwargs.set_item("runtime_session", &slf)?;
        let server = class.call((), Some(&kwargs))?.unbind();
        publish_server_bridge(py, &slf.server_bridge, server)
    }

    fn ensure_host_started(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.ensure_host().map(|_| ()))
            .map_err(ensure_host_error_to_py)
    }

    /// True while the Core host is actually running.
    ///
    /// This reports the native `Host.is_running()` observation instead of the
    /// SDK's mere handle presence, so a host that already stopped (explicit
    /// shutdown or a failed start attempt) does not read as
    /// started.
    #[getter]
    fn host_started(&self, py: Python<'_>) -> bool {
        py.detach(|| {
            self.host
                .lock()
                .host
                .as_ref()
                .is_some_and(c2_core::Host::is_running)
        })
    }

    /// Thin read-only lifecycle observation for the bridge.
    ///
    /// The projection exposes the finished flag and the separate
    /// listener and drained-work facts. `None` means no
    /// host exists. It never initiates or consumes shutdown or endpoint cleanup.
    fn native_lifecycle_snapshot<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let snapshot = py.detach(|| {
            let host = self.host.lock().host.clone();
            host.map(|host| host.lifecycle_snapshot())
        });
        snapshot
            .map(|snapshot| lifecycle_snapshot_to_dict(py, &snapshot))
            .transpose()
    }

    /// Terminal native shutdown observation, or `None` while the host has not
    /// produced a completed transaction.
    fn native_terminal_outcome<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let outcome = py.detach(|| {
            let (host, receipt) = {
                let record = self.host.lock();
                (record.host.clone(), record.failed_start_receipt.clone())
            };
            if let Some(receipt) = receipt {
                return receipt.terminal_outcome();
            }
            host.and_then(|host| host.shutdown_outcome())
        });
        match outcome {
            Some(outcome) => {
                let dict = shutdown_outcome_to_dict(py, outcome)?;
                dict.set_item("completed", true)?;
                Ok(Some(dict))
            }
            None => Ok(None),
        }
    }

    #[pyo3(signature = (name, dispatcher, method_names, access_map, concurrency_mode, max_pending, max_workers, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, descriptor_json, topic_names, topic_capacity, initial_topics))]
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
        topic_names: Vec<String>,
        topic_capacity: &Bound<'py, PyDict>,
        initial_topics: &Bound<'py, PyDict>,
    ) -> PyResult<Py<PyAny>> {
        if py.detach(|| self.registrations.lock().contains_key(name)) {
            return Err(PyValueError::new_err(format!(
                "Name already registered: {name:?}",
            )));
        }
        let methods = method_definitions(&method_names, access_map)?;
        let expected =
            expected_route_contract(name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash)?;
        let service = Arc::new(PyCoreService::new(name.to_string(), dispatcher));
        let (definition, portable) =
            service_definition(descriptor_json, expected, methods, service)?;
        let definition = definition
            .with_concurrency(
                parse_concurrency_mode(concurrency_mode)?,
                max_pending,
                max_workers,
            )
            .map_err(core_error_to_py)?;
        let definition = apply_topic_configuration(
            definition,
            portable,
            topic_names,
            topic_capacity,
            initial_topics,
        )?;
        let host = py
            .detach(|| self.ensure_host())
            .map_err(ensure_host_error_to_py)?;
        let registration = py
            .detach(|| host.register(definition))
            .map_err(core_error_to_py)?;
        let outcome = registration.outcome().clone();
        let concurrency = registration.route_concurrency();
        let shared = SessionRegistration::new(name.to_string(), registration);
        // Clone the handle's Arc before publishing the slot so a concurrent
        // unregister/re-register race can never hand out the wrong instance.
        let handle_shared = Arc::clone(&shared);
        py.detach(|| {
            let replaced = self.registrations.lock().insert(name.to_string(), shared);
            drop(replaced);
        });

        let outcome = register_outcome_to_dict(py, outcome)?.unbind();
        let concurrency = Py::new(py, PyRouteConcurrency::new(concurrency))?;
        let handle = Py::new(
            py,
            PyNativeRegistration {
                shared: handle_shared,
            },
        )?;
        Ok((outcome, concurrency, handle)
            .into_pyobject(py)?
            .into_any()
            .unbind())
    }

    fn unregister_route<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyDict>> {
        let registration = py
            .detach(|| self.registrations.lock().remove(name))
            .ok_or_else(|| PyValueError::new_err(format!("Name not registered: {name:?}")))?;
        let outcome = py
            .detach(|| registration.close())
            .map_err(core_error_to_py)?;
        unregister_outcome_to_dict(py, outcome)
    }

    /// Attach a same-process subscription to the route's current registration.
    ///
    /// Validates the exact route name and contract identity like a remote
    /// `OPEN`: 701 for an absent route, 709 on identity mismatch, 713 for a
    /// topic the matched contract does not declare.
    #[allow(clippy::too_many_arguments)] // PyO3 signature is the existing Python call boundary.
    #[pyo3(signature = (route_name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, topic, start, on_lag))]
    fn subscribe_local_route(
        &self,
        py: Python<'_>,
        route_name: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        topic: &str,
        start: &str,
        on_lag: &str,
    ) -> PyResult<crate::topic_ffi::PyLocalTopicSubscription> {
        let expected = expected_route_contract(
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        )?;
        let options = crate::topic_ffi::subscribe_options(start, on_lag)?;
        let host = py.detach(|| self.host.lock().host.clone());
        py.detach(move || {
            let host = host.ok_or_else(|| crate::topic_ffi::route_host_absent_error(route_name))?;
            host.subscribe_local(&expected, topic, options)
        })
        .map(crate::topic_ffi::PyLocalTopicSubscription::new)
        .map_err(core_error_to_py)
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

    /// Read the existing irreversible Core fence without constructing a Python
    /// exception under an SDK binding lock. Refusal is projected after unlock.
    #[getter]
    fn retired(&self, py: Python<'_>) -> bool {
        py.detach(|| self.inner.ensure_active().is_err())
    }

    #[pyo3(signature = (timeout_seconds=5.0))]
    fn begin_shutdown(
        slf: Py<Self>,
        py: Python<'_>,
        timeout_seconds: f64,
    ) -> PyResult<PyRuntimeShutdownWait> {
        let (_, deadline) = checked_shutdown_deadline(timeout_seconds)?;
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

    #[pyo3(signature = (timeout_seconds=5.0))]
    fn shutdown<'py>(&self, py: Python<'py>, timeout_seconds: f64) -> PyResult<Bound<'py, PyDict>> {
        // Validate before taking the Host or changing any session state.
        let (timeout, deadline) = checked_shutdown_deadline(timeout_seconds)?;
        let receipt = py
            .detach(|| self.inner.retirement_receipt(deadline))
            .map_err(|error| shutdown_error_to_py(py, error, timeout_seconds))?;
        if let Some(receipt) = receipt {
            let (outcome, completed) = py.detach(|| receipt.observe_until(deadline));
            return self.project_shutdown_observation(py, outcome, completed);
        }
        let (receipt_result, host, observed_generation) = py.detach(|| {
            let record = self.host.lock();
            if let Some(receipt) = record.failed_start_receipt.as_ref() {
                // Keep this Host ownership lock through observation and local
                // cleanup, but release it before dropping registrations or
                // reacquiring the Python GIL. A later ensure_host cannot publish a
                // Host that we then clear.
                let (outcome, completed) = receipt.observe_with_timeout(timeout);
                let (retired_registrations, retired_bridge) = if completed {
                    let registrations: Vec<Option<Registration>> = self
                        .registrations
                        .lock()
                        .drain()
                        .map(|(_, registration)| registration.take())
                        .collect();
                    let bridge = self.server_bridge.lock().bridge.take();
                    (registrations, bridge)
                } else {
                    (Vec::new(), None)
                };
                drop(record);
                drop(retired_registrations);
                (Some((outcome, completed, retired_bridge)), None, 0)
            } else {
                (None, record.host.clone(), record.generation)
            }
        });
        if let Some((outcome, completed, retired_bridge)) = receipt_result {
            drop(retired_bridge);
            let dict = shutdown_outcome_to_dict(py, outcome)?;
            dict.set_item("completed", completed)?;
            return Ok(dict);
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
                // No Host was selected. Low-level close leaves this Runtime's
                // outgoing clients and call admission available.
                (ShutdownOutcome::default(), true)
            }
        });
        if completed {
            self.clear_completed_host_generation(py, observed_generation);
        }
        // Keep the Host journal for repeated consumption and observation. A
        // pending drain retains every Registration instead of invoking Drop close.
        let dict = shutdown_outcome_to_dict(py, outcome)?;
        dict.set_item("completed", completed)?;
        Ok(dict)
    }

    fn clear_server_identity(&self, py: Python<'_>) -> PyResult<()> {
        let result = py.detach(|| {
            let record = self.host.lock();
            let active = record
                .host
                .as_ref()
                .is_some_and(|host| host.shutdown_outcome().is_none());
            if active {
                return Err(None);
            }
            // Core now reports why identity reset is refused (pending native
            // teardown), so this must never silently report success. Keep the
            // Host lock across the Core clear so a concurrent start cannot race
            // in between the active check and identity reset.
            self.inner.clear_server_identity().map_err(Some)
        });
        match result {
            Ok(()) => Ok(()),
            Err(None) => Err(PyRuntimeError::new_err(
                "cannot clear server identity while the Core host is active",
            )),
            Err(Some(error)) => Err(runtime_configuration_error_to_py(error)),
        }
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

fn parse_startup_timeout(seconds: f64) -> PyResult<Duration> {
    c2_config::timeout_from_seconds(seconds)
        .map_err(|error| PyValueError::new_err(format!("startup_timeout: {error}")))
}

fn domain_limits_patch(
    unset: &Bound<'_, PyAny>,
    memory_budget_bytes: &Bound<'_, PyAny>,
    file_spill_budget_bytes: &Bound<'_, PyAny>,
    max_outstanding_calls: &Bound<'_, PyAny>,
) -> PyResult<DomainLimitsPatch> {
    Ok(DomainLimitsPatch {
        memory_budget_bytes: config_field(memory_budget_bytes, unset, |value| {
            config_uint(value, "memory_budget_bytes")
        })?,
        file_spill_budget_bytes: config_field(file_spill_budget_bytes, unset, |value| {
            config_uint(value, "file_spill_budget_bytes")
        })?,
        max_outstanding_calls: config_field(max_outstanding_calls, unset, |value| {
            config_uint(value, "max_outstanding_calls")
        })?,
    })
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
    service: Arc<PyCoreService>,
) -> PyResult<(ServiceDefinition, bool)> {
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
        if release
            .descriptor()
            .methods()
            .iter()
            .any(|method| method.mode() == MethodMode::ServerStream)
        {
            return ServiceDefinition::new_stream(
                &release,
                release.reference(),
                expected.route_name,
                methods,
                service,
            )
            .map_err(core_error_to_py)
            .map(|definition| (definition, true));
        }
        return ServiceDefinition::new(
            &release,
            release.reference(),
            expected.route_name,
            methods,
            service,
        )
        .map_err(core_error_to_py)
        .map(|definition| (definition, true));
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
    let descriptor_methods = descriptor
        .get("methods")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| PyValueError::new_err("Python descriptor is missing methods"))?;
    if descriptor_methods.len() != methods.len() {
        return Err(PyValueError::new_err(
            "Python descriptor method count differs from route",
        ));
    }
    let modes = descriptor_methods
        .iter()
        .zip(&methods)
        .map(|(value, method)| {
            if value.get("name").and_then(serde_json::Value::as_str) != Some(method.name.as_str()) {
                return Err(PyValueError::new_err(
                    "Python descriptor method differs from route",
                ));
            }
            match value.get("mode") {
                None => Ok(MethodMode::Unary),
                Some(value) if value.as_str() == Some("server_stream") => {
                    Ok(MethodMode::ServerStream)
                }
                _ => Err(PyValueError::new_err("unsupported Python method mode")),
            }
        })
        .collect::<PyResult<Vec<_>>>()?;
    if modes.contains(&MethodMode::ServerStream) {
        ServiceDefinition::new_nonportable_stream(release_ref, expected, methods, modes, service)
            .map_err(core_error_to_py)
            .map(|definition| (definition, false))
    } else {
        ServiceDefinition::new_nonportable(release_ref, expected, methods, service)
            .map_err(core_error_to_py)
            .map(|definition| (definition, false))
    }
}

/// Apply the Python-declared topic configuration to one service definition.
///
/// Portable services take their topic set from the admitted release
/// descriptor (validated as a superset of the declared names); nonportable
/// services declare their language-local topic names explicitly. Capacities
/// and staged initial frames apply to both paths and commit atomically with
/// the registration.
fn apply_topic_configuration(
    definition: ServiceDefinition,
    portable: bool,
    topic_names: Vec<String>,
    topic_capacity: &Bound<'_, PyDict>,
    initial_topics: &Bound<'_, PyDict>,
) -> PyResult<ServiceDefinition> {
    let mut definition = definition;
    if portable {
        let declared: std::collections::BTreeSet<&str> =
            definition.topic_names().into_iter().collect();
        for name in &topic_names {
            if !declared.contains(name.as_str()) {
                return Err(PyValueError::new_err(format!(
                    "topic {name:?} is not declared on the portable contract descriptor",
                )));
            }
        }
    } else if !topic_names.is_empty() {
        definition = definition
            .with_nonportable_topics(topic_names)
            .map_err(core_error_to_py)?;
    }
    for (topic, capacity) in topic_capacity {
        let topic = topic.extract::<String>()?;
        let capacity = capacity.extract::<usize>()?;
        definition = definition
            .with_topic_capacity(&topic, capacity)
            .map_err(core_error_to_py)?;
    }
    for (topic, payload) in initial_topics {
        let topic = topic.extract::<String>()?;
        let payload = payload.extract::<Vec<u8>>()?;
        definition = definition
            .with_initial_topic_bytes(&topic, payload)
            .map_err(core_error_to_py)?;
    }
    Ok(definition)
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
    dict.set_item("server_id", outcome.server_id)?;
    dict.set_item("server_instance_id", outcome.server_instance_id)?;
    dict.set_item("ipc_address", outcome.ipc_address)?;
    dict.set_item("relay_registered", outcome.relay_registered)?;
    Ok(dict)
}

fn call_execution_snapshot_dict<'py>(
    py: Python<'py>,
    snapshot: &c2_core::RoleCallExecutionStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("max_operations", snapshot.max_operations)?;
    dict.set_item("used_operations", snapshot.used_operations)?;
    dict.set_item("peak_operations", snapshot.peak_operations)?;
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

fn role_memory_dict<'py>(
    py: Python<'py>,
    stats: &c2_core::RoleMemoryStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("domain_id", stats.domain_id)?;
    dict.set_item("generation", stats.generation)?;
    dict.set_item("frozen", stats.frozen)?;
    dict.set_item("closed", stats.closed)?;
    dict.set_item(
        "memory",
        stats
            .memory
            .as_ref()
            .map(|cell| memory_cell_dict(py, cell))
            .transpose()?,
    )?;
    dict.set_item(
        "file_spill",
        stats
            .file_spill
            .as_ref()
            .map(|cell| memory_cell_dict(py, cell))
            .transpose()?,
    )?;
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
    dict.set_item("route_close_error", outcome.route_close_error)?;
    dict.set_item("runtime_barrier_error", outcome.runtime_barrier_error)?;
    dict.set_item("ipc_client_close_error", outcome.ipc_client_close_error)?;
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

fn lifecycle_snapshot_to_dict<'py>(
    py: Python<'py>,
    snapshot: &HostLifecycleSnapshot,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("finished", snapshot.finished)?;
    dict.set_item("listener_closed", snapshot.listener_closed)?;
    dict.set_item("work_drained", snapshot.work_drained)?;
    Ok(dict)
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyRuntimeSession>()?;
    module.add_class::<PyNativeRegistration>()?;
    Ok(())
}
