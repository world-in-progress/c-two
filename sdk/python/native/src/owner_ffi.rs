//! Thin Python projection of Core owner capabilities and native child handles.

use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;

use c2_core::{
    OwnerBootstrap, OwnerControl, OwnerControlErrorKind, OwnerPhase, OwnerRecoveryTicket,
    OwnerStatus, PreparedOwnerTakeover, owner_control_pair as native_owner_control_pair,
};
use pyo3::types::PyBytes;

pyo3::create_exception!(_native, OwnerControlError, PyRuntimeError);

fn error_kind(kind: OwnerControlErrorKind) -> &'static str {
    match kind {
        OwnerControlErrorKind::Malformed => "malformed",
        OwnerControlErrorKind::UnsupportedVersion => "unsupported_version",
        OwnerControlErrorKind::IdentityMismatch => "identity_mismatch",
        OwnerControlErrorKind::OwnerPresent => "owner_present",
        OwnerControlErrorKind::InvalidGrant => "invalid_grant",
        OwnerControlErrorKind::GraceExpired => "grace_expired",
        OwnerControlErrorKind::ShuttingDown => "shutting_down",
        OwnerControlErrorKind::Unavailable => "unavailable",
        OwnerControlErrorKind::NotSent => "not_sent",
        OwnerControlErrorKind::Indeterminate => "indeterminate",
    }
}

pub(crate) fn owner_error(py: Python<'_>, error: c2_core::OwnerControlError) -> PyErr {
    let exception = OwnerControlError::new_err(error.to_string());
    if let Err(error) = exception
        .value(py)
        .setattr("kind", error_kind(error.kind()))
    {
        return error;
    }
    if let Err(error) = exception
        .value(py)
        .setattr("cause_kind", error.cause_kind().map(error_kind))
    {
        return error;
    }
    exception
}

fn control_timeout(timeout: Option<f64>) -> PyResult<Option<Duration>> {
    timeout
        .map(|seconds| {
            Duration::try_from_secs_f64(seconds).map_err(|_| {
                PyValueError::new_err(
                    "timeout must be finite, non-negative, and representable as a Duration",
                )
            })
        })
        .transpose()
}

#[pyclass(name = "OwnerControl", module = "c_two._native", frozen)]
pub(crate) struct PyOwnerControl {
    inner: OwnerControl,
}

#[pymethods]
impl PyOwnerControl {
    #[pyo3(signature = (timeout=None))]
    fn recovery_ticket(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<PyOwnerRecoveryTicket> {
        let timeout = control_timeout(timeout)?;
        py.detach(|| self.inner.recovery_ticket(timeout))
            .map(|inner| PyOwnerRecoveryTicket { inner })
            .map_err(|error| owner_error(py, error))
    }

    #[pyo3(signature = (timeout=None))]
    fn revoke_recovery(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<()> {
        let timeout = control_timeout(timeout)?;
        py.detach(|| self.inner.revoke_recovery(timeout))
            .map_err(|error| owner_error(py, error))
    }

    #[pyo3(signature = (timeout=None))]
    fn request_shutdown(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<()> {
        let timeout = control_timeout(timeout)?;
        py.detach(|| self.inner.request_shutdown(timeout))
            .map_err(|error| owner_error(py, error))
    }

    fn close(&self, py: Python<'_>) {
        py.detach(|| self.inner.close());
    }

    fn __repr__(&self) -> &'static str {
        "<c_two.OwnerControl (opaque owner capability)>"
    }
}

#[pyclass(
    name = "OwnerRecoveryTicket",
    module = "c_two._native",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct PyOwnerRecoveryTicket {
    pub(crate) inner: OwnerRecoveryTicket,
}

#[pymethods]
impl PyOwnerRecoveryTicket {
    /// Explicit export for storage by the controlling application.
    fn export<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.export())
    }

    #[staticmethod]
    fn from_bytes(py: Python<'_>, data: &[u8]) -> PyResult<Self> {
        OwnerRecoveryTicket::import(data)
            .map(|inner| Self { inner })
            .map_err(|error| owner_error(py, error))
    }

    fn __repr__(&self) -> &'static str {
        "<c_two.OwnerRecoveryTicket (redacted)>"
    }
}

#[pyclass(name = "OwnerStatus", module = "c_two._native", frozen)]
pub(crate) struct PyOwnerStatus {
    inner: OwnerStatus,
}

#[pymethods]
impl PyOwnerStatus {
    #[getter]
    fn address(&self) -> &str {
        &self.inner.address
    }
    #[getter]
    fn server_id(&self) -> &str {
        &self.inner.server_id
    }
    #[getter]
    fn server_instance_id(&self) -> &str {
        &self.inner.server_instance_id
    }
    #[getter]
    fn epoch(&self) -> u64 {
        self.inner.epoch
    }
    #[getter]
    fn context_binding<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.context_binding)
    }
    #[getter]
    fn phase(&self) -> &'static str {
        match self.inner.phase {
            OwnerPhase::Armed => "armed",
            OwnerPhase::OwnerMissing => "owner_missing",
            OwnerPhase::Draining => "draining",
            OwnerPhase::Finished => "finished",
            OwnerPhase::WatcherIoError => "watcher_io_error",
        }
    }
}

#[pyclass(name = "PreparedOwnerTakeover", module = "c_two._native", frozen)]
pub(crate) struct PyPreparedOwnerTakeover {
    pub(crate) inner: PreparedOwnerTakeover,
}

#[pymethods]
impl PyPreparedOwnerTakeover {
    #[getter]
    fn successor_ticket(&self) -> PyOwnerRecoveryTicket {
        PyOwnerRecoveryTicket {
            inner: self.inner.successor_ticket().clone(),
        }
    }
    #[getter]
    fn recovery_ticket(&self) -> PyOwnerRecoveryTicket {
        PyOwnerRecoveryTicket {
            inner: self.inner.recovery_ticket().clone(),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn inspect(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<PyOwnerStatus> {
        let timeout = control_timeout(timeout)?;
        py.detach(|| self.inner.inspect(timeout))
            .map(|inner| PyOwnerStatus { inner })
            .map_err(|error| owner_error(py, error))
    }

    #[pyo3(signature = (timeout=None))]
    fn commit(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<PyOwnerControl> {
        let timeout = control_timeout(timeout)?;
        py.detach(|| self.inner.commit(timeout))
            .map(|inner| PyOwnerControl { inner })
            .map_err(|error| owner_error(py, error))
    }

    fn __repr__(&self) -> &'static str {
        "<c_two.PreparedOwnerTakeover (redacted)>"
    }
}

/// Receiver-side endpoint of the private owner control pair.
///
/// The controller transfers the *capability*, never a path: it starts the one
/// intended child with this endpoint as that child's stdin, and the child
/// adopts it with [`adopt_owner_stdin`](Self::adopt_owner_stdin). Ordinary
/// business stdin is deliberately not adopted.
#[pyclass(name = "NativeOwnerBootstrap", module = "c_two._native", frozen)]
pub struct PyNativeOwnerBootstrap {
    inner: parking_lot::Mutex<Option<OwnerBootstrap>>,
}

impl PyNativeOwnerBootstrap {
    fn new(receiver: OwnerBootstrap) -> Self {
        Self {
            inner: parking_lot::Mutex::new(Some(receiver)),
        }
    }

    /// Take the native receiver for exactly one `RuntimeSession` attach.
    ///
    /// A second take on the same object fails instead of silently producing a
    /// second capability, so one receiver can never arm two owner-bound hosts.
    pub(crate) fn take_for_attach(&self) -> PyResult<OwnerBootstrap> {
        self.inner
            .lock()
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("owner bootstrap was already consumed"))
    }
}

#[pymethods]
impl PyNativeOwnerBootstrap {
    /// Explicitly adopt an owner control receiver from the inherited stdin of
    /// a trusted launcher's child process.
    ///
    /// This is the only adoption seam and it is never implicit: it must be
    /// called deliberately by a child that the controller started with the
    /// receiver endpoint as its stdio. The inherited source descriptor or
    /// handle is borrowed and duplicated; it keeps its previous owner and is
    /// never closed here. On Unix the inherited descriptor is fd `0`; on
    /// Windows it is the process standard-input handle.
    ///
    /// The native layer validates only the OS object type, endpoint, mode and
    /// nonblocking/overlapped properties. It does not authenticate who created
    /// the handle; that provenance is the trusted launcher's responsibility.
    /// Ordinary business connections must never be adopted this way.
    #[staticmethod]
    fn adopt_owner_stdin(py: Python<'_>) -> PyResult<PyNativeOwnerBootstrap> {
        let receiver = py.detach(adopt_process_stdin)?;
        Ok(PyNativeOwnerBootstrap::new(receiver))
    }

    /// Whether this value still owns an unconsumed native receiver.
    #[getter]
    fn is_available(&self) -> bool {
        self.inner.lock().is_some()
    }

    fn __repr__(&self) -> &'static str {
        // Never render a descriptor, handle, endpoint path, or token.
        "<c_two._native.NativeOwnerBootstrap (opaque owner capability)>"
    }
}

fn adopt_process_stdin() -> PyResult<OwnerBootstrap> {
    #[cfg(unix)]
    {
        // SAFETY: fd 0 is the descriptor this process was explicitly started
        // with. The call duplicates it, so the inherited descriptor keeps its
        // original owner and is never closed here.
        unsafe { OwnerBootstrap::from_inherited_fd(0) }
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        let stdin = std::io::stdin();
        // SAFETY: the standard-input handle is the inherited handle this
        // process was explicitly started with. The call duplicates it, so the
        // inherited handle keeps its original owner.
        unsafe { OwnerBootstrap::from_inherited_handle(stdin.as_handle().as_raw_handle()) }
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(PyValueError::new_err(
            "owner control adoption is unsupported on this platform",
        ))
    }
}

/// Create the private owner control pair: `(keepalive, receiver)`.
///
/// Both OS endpoints are created non-inheritable and neither is registered
/// with an async runtime. The controller keeps the keepalive private and
/// hands the receiver to exactly one target child. The returned values expose
/// no descriptor, handle, name, or secret.
#[pyfunction]
fn owner_control_pair() -> PyResult<(PyOwnerControl, PyNativeOwnerBootstrap)> {
    let (keepalive, receiver) = native_owner_control_pair()
        .map_err(|error| PyValueError::new_err(format!("owner control pair failed: {error}")))?;
    Ok((
        PyOwnerControl { inner: keepalive },
        PyNativeOwnerBootstrap::new(receiver),
    ))
}

struct ChildState {
    child: Option<Child>,
    status: Option<ExitStatus>,
    error: Option<String>,
}

struct ChildObservation {
    state: Mutex<ChildState>,
    changed: Condvar,
}

struct ChildReaper {
    children: Mutex<Vec<Arc<ChildObservation>>>,
    changed: Condvar,
}

// One process-wide reaper, initialized before launching a child. It polls all
// children without waiting on any one live process, retains Windows handles,
// and reaps Unix exits even when the Python handle is closed or dropped.
// Drop never starts a thread, waits, or kills a legitimate running service.
static CHILD_REAPER: LazyLock<Result<Arc<ChildReaper>, String>> = LazyLock::new(|| {
    let reaper = Arc::new(ChildReaper {
        children: Mutex::new(Vec::new()),
        changed: Condvar::new(),
    });
    let worker = Arc::clone(&reaper);
    std::thread::Builder::new()
        .name("c2-owned-child-reaper".into())
        .spawn(move || {
            let mut children = worker.children.lock();
            loop {
                while children.is_empty() {
                    worker.changed.wait(&mut children);
                }
                children.retain(|observation| {
                    let mut state = observation.state.lock();
                    let result = state.child.as_mut().expect("unreaped child").try_wait();
                    match result {
                        Ok(Some(status)) => {
                            state.status = Some(status);
                            state.error = None;
                            state.child.take();
                            observation.changed.notify_all();
                            false
                        }
                        Ok(None) => {
                            state.error = None;
                            true
                        }
                        Err(error) => {
                            state.error = Some(error.to_string());
                            observation.changed.notify_all();
                            // Keep the OS owner and retry; an observation error
                            // never discards an unreaped process.
                            true
                        }
                    }
                });
                if !children.is_empty() {
                    worker
                        .changed
                        .wait_for(&mut children, Duration::from_millis(10));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(reaper)
});

/// Opaque process owner returned by `spawn_owned_child`.
#[pyclass(name = "OwnedChild", module = "c_two._native", frozen)]
pub struct PyOwnedChild {
    id: u32,
    observation: Mutex<Option<Arc<ChildObservation>>>,
}

impl PyOwnedChild {
    fn observation(&self) -> PyResult<Arc<ChildObservation>> {
        self.observation
            .lock()
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("owned child handle is closed"))
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

#[pymethods]
impl PyOwnedChild {
    #[getter]
    fn id(&self) -> u32 {
        self.id
    }

    /// Return the cached exit code, or None while the process is running.
    fn poll(&self, py: Python<'_>) -> PyResult<Option<i32>> {
        let observation = self.observation()?;
        py.detach(|| {
            let state = observation.state.lock();
            if let Some(error) = &state.error {
                return Err(PyRuntimeError::new_err(error.clone()));
            }
            Ok(state.status.map(exit_code))
        })
    }

    /// Wait with a finite deadline. Timeout leaves the process and handle live.
    #[pyo3(signature = (timeout=30.0))]
    fn wait(&self, py: Python<'_>, timeout: f64) -> PyResult<i32> {
        let timeout =
            crate::runtime_session_ffi::checked_timeout(timeout).map_err(PyValueError::new_err)?;
        if Instant::now().checked_add(timeout).is_none() {
            return Err(PyValueError::new_err(
                "timeout exceeds the native wait deadline range",
            ));
        }
        let observation = self.observation()?;
        py.detach(|| {
            let started = Instant::now();
            let mut state = observation.state.lock();
            loop {
                if let Some(status) = state.status {
                    return Ok(exit_code(status));
                }
                if let Some(error) = &state.error {
                    return Err(PyRuntimeError::new_err(error.clone()));
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(PyTimeoutError::new_err("owned child wait timed out"));
                }
                observation.changed.wait_for(&mut state, remaining);
            }
        })
    }

    /// Explicitly request process termination; call wait to observe its exit.
    fn kill(&self, py: Python<'_>) -> PyResult<()> {
        let observation = self.observation()?;
        py.detach(|| {
            let mut state = observation.state.lock();
            if let Some(child) = state.child.as_mut() {
                child
                    .kill()
                    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            }
            Ok(())
        })
    }

    /// Release this observer. The shared reaper keeps the process until exit;
    /// closing the handle never kills a service or closes the owner keepalive.
    fn close(&self) {
        self.observation.lock().take();
    }

    fn __repr__(&self) -> &'static str {
        "<c_two._native.OwnedChild (opaque process owner)>"
    }
}

/// Spawn the one intended child whose stdin is the owner control receiver.
///
/// The controller must do this before the child can observe any owner EOF.
/// The receiver's native endpoint is the only thing wired to the child's
/// stdin, so unrelated children never inherit the capability, and the
/// capability never travels through argv, the environment, or a file path.
///
/// Returns an opaque process owner. The receiver is consumed by this call; the
/// child owns its own duplicate of the endpoint from then on.
#[pyfunction]
#[pyo3(signature = (receiver, program, args=None, cwd=None))]
fn spawn_owned_child(
    py: Python<'_>,
    receiver: &PyNativeOwnerBootstrap,
    program: &str,
    args: Option<Vec<String>>,
    cwd: Option<String>,
) -> PyResult<PyOwnedChild> {
    let reaper = CHILD_REAPER
        .as_ref()
        .map_err(|error| PyRuntimeError::new_err(error.clone()))?;
    let mut inner = receiver.inner.lock();
    let receiver = inner
        .as_mut()
        .ok_or_else(|| PyRuntimeError::new_err("owner control receiver was already consumed"))?;
    let stdin = receiver
        .take_stdio()
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    inner.take();
    // Detach around process creation and error cleanup. No Python lock or GIL
    // is needed while the OS creates or waits for a child.
    drop(inner);
    let program = program.to_owned();
    py.detach(|| {
        let mut command = Command::new(program);
        command.args(args.unwrap_or_default()).stdin(stdin);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().map_err(|error| {
            PyValueError::new_err(format!("owner bound child spawn failed: {error}"))
        })?;
        let id = child.id();
        let observation = Arc::new(ChildObservation {
            state: Mutex::new(ChildState {
                child: Some(child),
                status: None,
                error: None,
            }),
            changed: Condvar::new(),
        });
        // The worker was created before capability consumption or process
        // creation. Publish into its owned queue without a fallible send or
        // an unbounded rollback wait after the child exists.
        reaper.children.lock().push(Arc::clone(&observation));
        reaper.changed.notify_one();
        Ok(PyOwnedChild {
            id,
            observation: Mutex::new(Some(observation)),
        })
    })
}

/// Native availability probe used by the Python facade.
#[pyfunction]
fn owner_control_supported() -> bool {
    cfg!(any(unix, windows))
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyOwnerControl>()?;
    module.add_class::<PyOwnerRecoveryTicket>()?;
    module.add_class::<PyPreparedOwnerTakeover>()?;
    module.add_class::<PyOwnerStatus>()?;
    module.add(
        "OwnerControlError",
        module.py().get_type::<OwnerControlError>(),
    )?;
    module.add_class::<PyNativeOwnerBootstrap>()?;
    module.add_class::<PyOwnedChild>()?;
    module.add_function(wrap_pyfunction!(owner_control_pair, module)?)?;
    module.add_function(wrap_pyfunction!(spawn_owned_child, module)?)?;
    module.add_function(wrap_pyfunction!(owner_control_supported, module)?)?;
    Ok(())
}
