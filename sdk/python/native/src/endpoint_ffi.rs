//! PyO3 projection of the native local-endpoint lifecycle surface.
//!
//! This module is a thin projection, not a second implementation. Every
//! decision that matters is made in Rust:
//!
//! * the OS endpoint is derived from the logical address and current platform by
//!   `c2-config::LocalEndpoint`, never by probing a path in Python;
//! * credentials are parsed and encoded by the one Rust codec
//!   (`EndpointCredential::from_json` / `to_json`), so Python never owns a
//!   field table, never assembles a `LocalEndpoint`, and never patches a
//!   credential;
//! * `reap` validates the credential against that native backend and the
//!   identity check decides the outcome; the logical address is compared
//!   so a credential for another endpoint is a `stale-target` instead of a
//!   silent cross-endpoint probe;
//! * at most one maintenance sweep exists per process, and the lease is taken
//!   and released by Rust;
//! * the sweep stores its own `SweepBudget` (the `c2-local` default unless the
//!   caller overrides a dimension) and validates every value before a lease or
//!   iterator exists, so Python owns no budget constant or range table.
//!
//! `KernelManaged` (Windows named pipes) and `NotApplicable` are honest
//! observations about which layer owns endpoint lifetime. Neither is evidence
//! that a live instance exists, so neither is reported as `alive`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyInt};

use c2_config::LocalEndpoint;
use c2_core::{
    EndpointCredential, EndpointInspection, EndpointReapResult, EndpointSweep,
    EndpointUnverifiedReason, SweepBatch, SweepBudget, inspect_endpoint, reap_endpoint,
};

/// Hard ceiling for one sweep batch's entry budget. A batch is a scheduling
/// slice, never a whole-namespace collection. The default budget itself comes
/// from `c2-local`'s `SweepBudget::default()`, not from a second table here.
const MAX_SWEEP_ENTRIES: u32 = 4096;
/// Hard ceiling for one sweep batch's wall-clock budget, in milliseconds.
const MAX_SWEEP_MS: u32 = 1000;

/// Process-wide maintenance lease. The Rust side owns this flag: Python can
/// observe the outcome but cannot create, transfer, or fake a lease.
static SWEEP_LEASE_HELD: AtomicBool = AtomicBool::new(false);

const SWEEP_LEASE_TAKEN: &str = "a local endpoint sweep is already active in this process";

fn take_sweep_lease() -> PyResult<()> {
    // A single compare-exchange is the whole mutual exclusion: there is no
    // window between checking and taking the lease.
    if SWEEP_LEASE_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(PyRuntimeError::new_err(SWEEP_LEASE_TAKEN));
    }
    Ok(())
}

fn release_sweep_lease() {
    SWEEP_LEASE_HELD.store(false, Ordering::Release);
}

/// Owns the process lease for the sweep's whole life, including the abrupt
/// path where the iterator is dropped without `close()`.
struct SweepLease;

impl SweepLease {
    fn acquire() -> PyResult<Self> {
        take_sweep_lease()?;
        Ok(Self)
    }
}

impl Drop for SweepLease {
    fn drop(&mut self) {
        release_sweep_lease();
    }
}

fn unverified_reason_name(reason: EndpointUnverifiedReason) -> &'static str {
    match reason {
        EndpointUnverifiedReason::UnsafeDirectory => "unsafe-directory",
        EndpointUnverifiedReason::Symlink => "symlink",
        EndpointUnverifiedReason::UnexpectedObject => "unexpected-object",
        EndpointUnverifiedReason::ForeignOwner => "foreign-owner",
        EndpointUnverifiedReason::MissingOwnership => "missing-ownership",
        EndpointUnverifiedReason::InvalidOwnership => "invalid-ownership",
        EndpointUnverifiedReason::InvalidRecord => "invalid-record",
        EndpointUnverifiedReason::RecordMismatch => "record-mismatch",
        EndpointUnverifiedReason::CoordinatorMissing => "coordinator-missing",
        EndpointUnverifiedReason::CoordinatorReplaced => "coordinator-replaced",
        EndpointUnverifiedReason::InitializationIncomplete => "initialization-incomplete",
    }
}

fn io_error_kind_name(kind: std::io::ErrorKind) -> String {
    // `Debug` is the stable spelling of an `ErrorKind`; `kind().to_string()`
    // omits the raw OS code, which is reported separately.
    format!("{kind:?}")
}

/// A result row. Every field the caller may branch on is always present, so a
/// consumer never has to treat a missing field as an implied failure.
fn set_result_fields<'py>(
    dict: &Bound<'py, PyDict>,
    status: &str,
    credential: Option<String>,
    reason: Option<String>,
) -> PyResult<()> {
    dict.set_item("status", status)?;
    dict.set_item("credential", credential)?;
    dict.set_item("reason", reason)?;
    dict.set_item("io_kind", None::<String>)?;
    dict.set_item("raw_os_error", None::<i32>)?;
    dict.set_item("retryable", false)?;
    Ok(())
}

fn result_dict<'py>(py: Python<'py>, status: &str) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    set_result_fields(&dict, status, None, None)?;
    Ok(dict)
}

/// Projects a reap result. Only the native outcome is reported; a partial or
/// unverifiable cleanup never becomes a fabricated success.
fn reap_dict<'py>(py: Python<'py>, result: &EndpointReapResult) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    match result {
        EndpointReapResult::Reaped => set_result_fields(&dict, "reaped", None, None)?,
        EndpointReapResult::AlreadyAbsent => {
            set_result_fields(&dict, "already-absent", None, None)?
        }
        EndpointReapResult::Busy => {
            set_result_fields(&dict, "busy", None, Some("coordinator-held".to_string()))?
        }
        // A decoded credential that does not describe this endpoint, or that
        // no longer matches the live endpoint object, is a stale target. It is
        // never retried as if it were a transient failure.
        EndpointReapResult::StaleTarget => set_result_fields(
            &dict,
            "stale-target",
            None,
            Some("identity-mismatch".to_string()),
        )?,
        EndpointReapResult::Unverified(reason) => set_result_fields(
            &dict,
            "unverified",
            None,
            Some(unverified_reason_name(*reason).to_string()),
        )?,
        // There is no filesystem entry to collect on this platform. This is
        // not an error and not proof that a matching live endpoint exists.
        EndpointReapResult::NotApplicable => set_result_fields(
            &dict,
            "not-applicable",
            None,
            Some("no-filesystem-entry".to_string()),
        )?,
        EndpointReapResult::IoError(error) => {
            set_result_fields(&dict, "io-error", None, nonempty_io_kind(error))?;
            dict.set_item("io_kind", Some(io_error_kind_name(error.kind())))?;
            dict.set_item("raw_os_error", error.raw_os_error())?;
            dict.set_item("retryable", true)?;
        }
    }
    Ok(dict)
}

fn nonempty_io_kind(error: &std::io::Error) -> Option<String> {
    Some(io_error_kind_name(error.kind()))
}

/// Entry point, written and read only through the one Rust credential codec.
/// `parse` and `to_json` are the only value constructors, so every other
/// operation on `PyEndpointCredential` is provably operating on a credential
/// the Rust gate accepted.
#[pyclass(module = "c_two._native", frozen)]
pub(crate) struct PyEndpointCredential {
    inner: EndpointCredential,
}

impl PyEndpointCredential {
    pub(crate) fn new(inner: EndpointCredential) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyEndpointCredential {
    /// Parses a strict JSON credential document.
    ///
    /// Decoding is a pure parse: it re-derives the OS endpoint from the
    /// recorded address and protocol, rejects unknown fields and unsupported
    /// schema versions, and enforces the documented size limit. A successful
    /// decode proves nothing about liveness; only `reap_endpoint` decides.
    #[staticmethod]
    fn from_json(json: &str) -> PyResult<Self> {
        EndpointCredential::from_json(json)
            .map(Self::new)
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Encodes this credential as strict JSON.
    ///
    /// The value round-trips through the same parser that reads it. The
    /// document is a description, not a secret and not an authorization token.
    fn to_json(&self) -> PyResult<String> {
        self.inner
            .to_json()
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// The logical IPC address this credential describes. Metadata only.
    #[getter]
    fn address(&self) -> &str {
        self.inner.endpoint().address()
    }

    /// Native backend credential metadata.
    #[getter]
    fn protocol(&self) -> &'static str {
        self.inner.endpoint().protocol()
    }

    #[getter]
    fn platform(&self) -> &'static str {
        if cfg!(windows) { "windows" } else { "unix" }
    }

    fn __repr__(&self) -> String {
        // Never echo the encoded document: it is not a secret, but a repr that
        // copies identity fields invites callers to treat it as a capability.
        format!(
            "PyEndpointCredential(address={:?}, protocol={:?})",
            self.inner.endpoint().address(),
            self.inner.endpoint().protocol(),
        )
    }
}

/// Inspects one logical local endpoint without creating ownership metadata.
///
#[pyfunction]
#[pyo3(signature = (address))]
fn inspect_endpoint_endpoint<'py>(py: Python<'py>, address: &str) -> PyResult<Bound<'py, PyDict>> {
    let endpoint = endpoint_for(address)?;
    // The inspection may touch the filesystem; it runs without the GIL.
    let inspection = py.detach(|| inspect_endpoint(&endpoint));
    match inspection {
        EndpointInspection::Absent => result_dict(py, "absent"),
        EndpointInspection::Present(credential) => {
            let json = credential
                .to_json()
                .map_err(|error| PyValueError::new_err(error.to_string()))?;
            let dict = result_dict(py, "present")?;
            dict.set_item("credential", Some(json))?;
            Ok(dict)
        }
        // The platform owns endpoint lifetime inside a kernel namespace. This
        // observes the platform, not existence, so it is not "alive".
        EndpointInspection::KernelManaged => {
            let dict = result_dict(py, "not-applicable")?;
            dict.set_item("reason", Some("kernel-managed".to_string()))?;
            Ok(dict)
        }
        EndpointInspection::Unverified(reason) => {
            let dict = result_dict(py, "unverified")?;
            dict.set_item("reason", Some(unverified_reason_name(reason).to_string()))?;
            Ok(dict)
        }
        EndpointInspection::IoError(error) => {
            let dict = result_dict(py, "io-error")?;
            dict.set_item("reason", nonempty_io_kind(&error))?;
            dict.set_item("io_kind", Some(io_error_kind_name(error.kind())))?;
            dict.set_item("raw_os_error", error.raw_os_error())?;
            dict.set_item("retryable", true)?;
            Ok(dict)
        }
    }
}

/// Reaps the exact endpoint object named by `credential`.
///
/// Reap the native endpoint only after validating the credential address.
#[pyfunction]
fn reap_endpoint_credential<'py>(
    py: Python<'py>,
    address: &str,
    credential: &PyEndpointCredential,
) -> PyResult<Bound<'py, PyDict>> {
    let recorded = credential.inner.endpoint();
    let dict = result_dict(py, "stale-target")?;
    if recorded.address() != address {
        dict.set_item("reason", Some("credential-address-mismatch".to_string()))?;
        return Ok(dict);
    }
    // Re-derive through the same authority: a decoded credential never supplies
    // a path, only its logical address and native backend metadata.
    let endpoint = LocalEndpoint::from_address(recorded.address())
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    let result = py.detach(|| reap_endpoint(&endpoint, &credential.inner));
    reap_dict(py, &result)
}

/// A bounded, explicitly driven sweep over one endpoint protocol namespace.
///
/// One process may hold one active sweep. The lease is owned by Rust and is
/// released on `close()` or when this object is dropped. Dropping the object
/// interrupts the round; a new sweep always starts at the beginning.
///
/// The default budget is stored here, once, as a native `SweepBudget`; the
/// Python facade never mirrors a budget number or validates one.
#[pyclass(module = "c_two._native")]
pub(crate) struct PyEndpointSweep {
    inner: Option<EndpointSweep>,
    lease: Option<SweepLease>,
    default_budget: SweepBudget,
    batches: u64,
    closed: bool,
}

impl PyEndpointSweep {
    fn open<'py>(
        py: Python<'py>,
        addresses: Option<Vec<String>>,
        max_entries: Option<Bound<'py, PyAny>>,
        max_ms: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Self> {
        // Both explicit dimensions are validated here, before the process
        // lease is taken or the native iterator is opened: a rejected budget
        // can never leave a held lease or a half-open sweep behind. `None`
        // keeps `SweepBudget::default()` from `c2-local`.
        let default_budget = budget_with_overrides(
            SweepBudget::default(),
            optional_budget_dimension(max_entries.as_ref(), "max_entries", MAX_SWEEP_ENTRIES)?,
            optional_budget_dimension(max_ms.as_ref(), "max_ms", MAX_SWEEP_MS)?,
        );
        // The namespace is derived from `LocalEndpoint` authority using a
        // reserved probe address; it is never a hardcoded directory.
        let probe = LocalEndpoint::from_address("ipc://c2-endpoint-sweep")
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let scope = addresses
            .as_ref()
            .map(|addresses| EndpointSweep::scope_for_addresses(&probe, addresses))
            .transpose()
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let lease = SweepLease::acquire()?;
        // Opening walks and validates the namespace directory, so the GIL is
        // released for the real filesystem work.
        let opened = py.detach(|| match scope.as_ref() {
            Some(scope) => EndpointSweep::for_scope(scope),
            None => EndpointSweep::for_endpoint(&probe),
        });
        let inner = match opened {
            Ok(sweep) => sweep,
            Err(error) => {
                drop(lease);
                return Err(PyValueError::new_err(format!(
                    "cannot open the {} endpoint namespace: {error}",
                    probe.protocol()
                )));
            }
        };
        Ok(Self {
            inner: Some(inner),
            lease: Some(lease),
            default_budget,
            batches: 0,
            closed: false,
        })
    }
}

fn budget_range_error(name: &str, ceiling: u32) -> PyErr {
    PyValueError::new_err(format!("{name} must be between 1 and {ceiling}"))
}

/// Validates one optional budget dimension against the native ceiling.
///
/// `None` means "keep the sweep's stored default" and is not itself a value.
/// A rejected value is never truncated or clamped:
///
/// * `bool` is rejected even though it is an `int` subclass at the C level,
///   because a batch budget is a count, not a flag;
/// * any other non-integer is a `TypeError`;
/// * a non-positive, above-ceiling, or too-wide integer is a clean
///   `ValueError`, including integers wider than the parse width.
///
/// No `Duration` arithmetic runs until this gate returns a bounded value.
fn optional_budget_dimension(
    value: Option<&Bound<'_, PyAny>>,
    name: &str,
    ceiling: u32,
) -> PyResult<Option<u32>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_instance_of::<PyBool>() || !value.is_instance_of::<PyInt>() {
        return Err(PyTypeError::new_err(format!("{name} must be an int")));
    }
    // `i128` is the widest exact parse this gate reasons about. A Python
    // integer wider than that is still a clean range error, never a wrapped
    // or truncated budget.
    let parsed: i128 = value
        .extract()
        .map_err(|_| budget_range_error(name, ceiling))?;
    if parsed <= 0 || parsed > i128::from(ceiling) {
        return Err(budget_range_error(name, ceiling));
    }
    Ok(Some(parsed as u32))
}

/// Resolves the effective budget.
///
/// `None` dimensions keep the already validated native default; an explicit
/// override of one dimension never resets the other. The validator bounded
/// every entry value by the ceiling, and `Duration::from_millis` cannot
/// overflow for any `u64`, so this function is total.
fn budget_with_overrides(
    default: SweepBudget,
    max_entries: Option<u32>,
    max_ms: Option<u32>,
) -> SweepBudget {
    SweepBudget {
        max_entries: max_entries.map_or(default.max_entries, |value| value as usize),
        max_duration: max_ms.map_or(default.max_duration, |value| {
            Duration::from_millis(u64::from(value))
        }),
    }
}

fn batch_dict<'py>(
    py: Python<'py>,
    batch: &SweepBatch,
    batch_index: u64,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("batch", batch_index)?;
    dict.set_item("entries_visited", batch.entries_visited)?;
    dict.set_item("endpoints_examined", batch.endpoints_examined)?;
    dict.set_item("reaped", batch.reaped)?;
    dict.set_item("already_absent", batch.already_absent)?;
    dict.set_item("busy", batch.busy)?;
    dict.set_item("stale_target", batch.stale_target)?;
    dict.set_item("unverified", batch.unverified)?;
    dict.set_item("io_errors", batch.io_errors)?;
    dict.set_item(
        "last_io_error",
        match batch.last_io_error {
            Some(error) => Some((io_error_kind_name(error.kind), error.raw_os_error)),
            None => None,
        },
    )?;
    dict.set_item("not_applicable", batch.not_applicable)?;
    dict.set_item("leases_retired", batch.leases_retired)?;
    dict.set_item("round_complete", batch.round_complete)?;
    dict.set_item("round_interrupted", batch.round_interrupted)?;
    dict.set_item("namespace_changed", batch.namespace_changed)?;
    Ok(dict)
}

#[pymethods]
impl PyEndpointSweep {
    /// Opens the native namespace with optional address scope and bounded budgets.
    #[new]
    #[pyo3(signature = (*, addresses=None, max_entries=None, max_ms=None))]
    fn new<'py>(
        py: Python<'py>,
        addresses: Option<Vec<String>>,
        max_entries: Option<Bound<'py, PyAny>>,
        max_ms: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Self> {
        Self::open(py, addresses, max_entries, max_ms)
    }

    /// Advances the native iterator by one bounded batch.
    ///
    /// The returned dictionary carries the real per-batch counters. Nothing is
    /// collected into a directory-wide list, and a budget is never silently
    /// dropped: each call consumes its own `max_entries` / `max_ms`.
    ///
    /// ``max_entries=None`` and ``max_ms=None`` keep the budget this sweep
    /// validated at open time; an explicit value is validated by the same
    /// native gate before it reaches the iterator. A rejected override leaves
    /// the iterator and the process lease untouched.
    #[pyo3(signature = (*, max_entries=None, max_ms=None))]
    fn next_batch<'py>(
        &mut self,
        py: Python<'py>,
        max_entries: Option<Bound<'py, PyAny>>,
        max_ms: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        self.ensure_open()?;
        let budget = budget_with_overrides(
            self.default_budget,
            optional_budget_dimension(max_entries.as_ref(), "max_entries", MAX_SWEEP_ENTRIES)?,
            optional_budget_dimension(max_ms.as_ref(), "max_ms", MAX_SWEEP_MS)?,
        );
        let inner = self
            .inner
            .as_mut()
            .expect("an open sweep owns a native iterator");
        // Directory walking and per-entry native inspection run without the GIL.
        let batch = py.detach(|| inner.next_batch(budget));
        self.batches += 1;
        batch_dict(py, &batch, self.batches)
    }

    #[getter]
    fn protocol(&self) -> &str {
        if cfg!(windows) {
            "named-pipe"
        } else {
            "managed-v2"
        }
    }

    #[getter]
    fn closed(&self) -> bool {
        self.closed
    }

    /// Releases the native iterator and the process maintenance lease.
    ///
    /// Idempotent: a second call is a no-op rather than a second release. The
    /// iterator is dropped and the lease released one statement apart, so the
    /// lease is never observable as free while the iterator is still live.
    fn close(&mut self) {
        if self.closed {
            return;
        }
        drop(self.inner.take());
        drop(self.lease.take());
        self.closed = true;
    }

    fn __enter__(&mut self) -> PyResult<()> {
        self.ensure_open()
    }

    fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, PyAny>,
        _exc: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        format!("PyEndpointSweep(closed={})", self.closed)
    }
}

impl PyEndpointSweep {
    fn ensure_open(&self) -> PyResult<()> {
        if self.closed || self.inner.is_none() {
            return Err(PyValueError::new_err("sweep is closed"));
        }
        Ok(())
    }
}

impl Drop for PyEndpointSweep {
    fn drop(&mut self) {
        // An abandoned sweep must not leak the process lease. The native
        // iterator drops here too, which interrupts the round.
        self.close();
    }
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyEndpointCredential>()?;
    module.add_class::<PyEndpointSweep>()?;
    module.add_function(wrap_pyfunction!(inspect_endpoint_endpoint, module)?)?;
    module.add_function(wrap_pyfunction!(reap_endpoint_credential, module)?)?;
    Ok(())
}

fn endpoint_for(address: &str) -> PyResult<LocalEndpoint> {
    LocalEndpoint::from_address(address).map_err(|e| PyValueError::new_err(e.to_string()))
}
