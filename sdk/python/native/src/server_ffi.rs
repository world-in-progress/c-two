//! PyO3 bindings for the C-Two IPC server (`c2-server`).
//!
//! Exposes `RustServer` — a UDS server that hosts CRM routes and dispatches
//! method calls through Python callables.  The server runs on a background
//! OS thread with its own tokio runtime.
//!
//! **GIL handling**: `PyCrmCallback::invoke()` acquires the GIL internally
//! (called from `spawn_blocking` inside tokio). All blocking `PyServer`
//! methods release the GIL via `py.detach()`.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyDict};

use c2_config::{BaseIpcConfig, ServerIpcConfig};
use c2_error::{C2Error, ErrorCode};
use c2_mem::MemPool;
use c2_server::response::try_prepare_shm_response;
use c2_server::{
    AccessLevel, BuiltRoute as ServerBuiltRoute, ConcurrencyMode, CrmCallback, CrmError,
    RequestData, ResponseMeta, RouteBuildSpec, SchedulerLimits, Server, ServerRuntimeBuilder,
};

use crate::response_backing::{PyResponseFinalBackingAllocator, try_response_final_backing_meta};
use crate::route_concurrency_ffi::PyRouteConcurrency;
use crate::shm_buffer::PyShmBuffer;
use crate::writable_sink::{prepared_plan_nbytes, write_python_payload_plan};

pub(crate) fn parse_concurrency_mode(mode: &str) -> PyResult<ConcurrencyMode> {
    match mode {
        "parallel" => Ok(ConcurrencyMode::Parallel),
        "exclusive" => Ok(ConcurrencyMode::Exclusive),
        "read_parallel" => Ok(ConcurrencyMode::ReadParallel),
        other => Err(PyValueError::new_err(format!(
            "invalid concurrency mode '{other}', expected 'parallel', 'exclusive', or 'read_parallel'"
        ))),
    }
}

// ---------------------------------------------------------------------------
// PyCrmCallback — bridges a Python callable to the Rust CrmCallback trait
// ---------------------------------------------------------------------------

pub(crate) struct PyCrmCallback {
    py_callable: Py<PyAny>,
    shm_threshold: u64,
    max_payload_size: u64,
}

// SAFETY: Py<PyAny> is Send when accessed only under the GIL.
// `invoke()` always acquires the GIL via `Python::attach` before
// touching `py_callable`.
unsafe impl Send for PyCrmCallback {}
unsafe impl Sync for PyCrmCallback {}

impl CrmCallback for PyCrmCallback {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        Python::attach(|py| {
            // Convert RequestData → PyShmBuffer
            let shm_buf = match request {
                RequestData::Inline(data) => PyShmBuffer::from_inline(data),
                RequestData::Shm {
                    pool,
                    seg_idx,
                    offset,
                    data_size,
                    is_dedicated,
                } => PyShmBuffer::from_peer_shm(pool, seg_idx, offset, data_size, is_dedicated),
                RequestData::Handle { handle, pool } => PyShmBuffer::from_handle(handle, pool),
            };
            let buf_obj = Py::new(py, shm_buf)
                .map_err(|e| CrmError::InternalError(format!("failed to create ShmBuffer: {e}")))?;

            let response_allocator = Py::new(
                py,
                PyResponseFinalBackingAllocator::new(
                    Arc::clone(&response_pool),
                    self.shm_threshold,
                    self.max_payload_size,
                ),
            )
            .map_err(|e| {
                CrmError::InternalError(format!(
                    "failed to create response final backing allocator: {e}"
                ))
            })?;

            // Call Python: dispatcher(route_name, method_idx, shm_buffer, response_allocator)
            let args = (route_name, method_idx, buf_obj, response_allocator);
            match self.py_callable.call1(py, args) {
                Ok(result) => parse_response_meta(
                    py,
                    result,
                    &response_pool,
                    self.shm_threshold,
                    self.max_payload_size,
                ),
                Err(e) => {
                    // Check for .error_bytes attribute (CrmCallError from Python).
                    let val = e.value(py);
                    if let Ok(attr) = val.getattr("error_bytes") {
                        if let Ok(b) = attr.cast::<PyBytes>() {
                            return Err(CrmError::UserError(b.as_bytes().to_vec()));
                        }
                    }
                    Err(CrmError::InternalError(format!("{e}")))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// PyServer — main pyclass
// ---------------------------------------------------------------------------

/// A Rust-native IPC server for hosting CRM routes.
///
/// Route registration is driven by `RuntimeSession`, which owns registration
/// transactions, relay publication, rollback, and route admission state.
#[pyclass(name = "RustServer", frozen)]
pub struct PyServer {
    pub(crate) inner: Arc<Server>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
}

pub(crate) struct BuiltRoute {
    pub route: ServerBuiltRoute,
    pub route_handle: PyRouteConcurrency,
}

impl PyServer {
    pub(crate) fn build_route(
        &self,
        _py: Python<'_>,
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
    ) -> PyResult<BuiltRoute> {
        let mode = parse_concurrency_mode(concurrency_mode)?;
        let limits = SchedulerLimits::try_from_usize(max_pending, max_workers)
            .map_err(PyValueError::new_err)?;

        // Parse access_map: {int: "read"|"write"} → HashMap<u16, AccessLevel>
        let mut map = HashMap::new();
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
            map.insert(idx, level);
        }

        let callback = Arc::new(PyCrmCallback {
            py_callable: dispatcher,
            shm_threshold: self.inner.response_shm_threshold(),
            max_payload_size: self.inner.response_max_payload_size(),
        });
        let spec = RouteBuildSpec {
            name: name.to_string(),
            crm_ns: crm_ns.to_string(),
            crm_name: crm_name.to_string(),
            crm_ver: crm_ver.to_string(),
            abi_hash: abi_hash.to_string(),
            signature_hash: signature_hash.to_string(),
            method_names,
            access_map: map,
            concurrency_mode: mode,
            limits,
        };
        let route = self
            .inner
            .build_route(spec, callback)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let route_handle = PyRouteConcurrency::new(route.route_handle());

        Ok(BuiltRoute {
            route,
            route_handle,
        })
    }

    pub(crate) fn runtime_is_running(&self) -> bool {
        self.inner.is_running()
    }

    pub(crate) fn shutdown_runtime_barrier_blocking(
        &self,
        py: Python<'_>,
        timeout: Duration,
    ) -> PyResult<()> {
        py.detach(|| self.stop_runtime_and_wait(timeout))
    }

    fn stop_runtime_and_wait(&self, timeout: Duration) -> PyResult<()> {
        let rt = {
            let mut rt_guard = self.rt.lock();
            rt_guard.take()
        };

        let wait_result = if let Some(rt) = rt {
            let result = rt.block_on(self.inner.shutdown_and_wait(timeout));
            rt.shutdown_background();
            result.map(|_| ())
        } else {
            Ok(())
        };
        self.inner.finalize_runtime_stopped();
        wait_result.map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    fn start_runtime_and_wait(&self, timeout: Duration) -> PyResult<()> {
        let server_for_run = Arc::clone(&self.inner);
        let server_for_wait = Arc::clone(&self.inner);

        let rt = ServerRuntimeBuilder::build(self.inner.config())
            .map_err(|e| PyRuntimeError::new_err(format!("failed to create runtime: {e}")))?;

        {
            let mut rt_guard = self.rt.lock();
            if rt_guard.is_some() {
                return Err(PyRuntimeError::new_err("server is already running"));
            }

            self.inner
                .begin_start_attempt()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            rt.spawn(async move {
                if let Err(e) = server_for_run.run().await {
                    eprintln!("c2-server error: {e}");
                }
            });

            *rt_guard = Some(rt);
        }

        let readiness = {
            let rt_guard = self.rt.lock();
            let rt = rt_guard
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("server runtime missing after start"))?;
            rt.block_on(server_for_wait.wait_until_responsive(timeout))
        };

        match readiness {
            Ok(()) => Ok(()),
            Err(err) => {
                let message = err.to_string();
                let cleanup = self.stop_runtime_and_wait(Duration::from_secs(5));
                let message = if let Err(cleanup_err) = cleanup {
                    format!("{message}; cleanup failed: {cleanup_err}")
                } else {
                    message
                };
                if message.contains("did not become ready") {
                    Err(PyTimeoutError::new_err(message))
                } else {
                    Err(PyRuntimeError::new_err(message))
                }
            }
        }
    }
}

#[pymethods]
impl PyServer {
    /// Create a new server targeting the given IPC address.
    #[new]
    #[pyo3(signature = (
        address, shm_threshold, pool_enabled, pool_segment_size,
        max_pool_segments, reassembly_segment_size,
        reassembly_max_segments, max_frame_size, max_payload_size,
        max_pending_requests, max_execution_workers, pool_decay_seconds, heartbeat_interval,
        heartbeat_timeout, max_total_chunks, chunk_gc_interval,
        chunk_threshold_ratio, chunk_assembler_timeout,
        max_reassembly_bytes, chunk_size,
        server_id=None, server_instance_id=None,
    ))]
    fn new(
        address: &str,
        shm_threshold: u64,
        pool_enabled: bool,
        pool_segment_size: u64,
        max_pool_segments: u32,
        reassembly_segment_size: u64,
        reassembly_max_segments: u32,
        max_frame_size: u64,
        max_payload_size: u64,
        max_pending_requests: u32,
        max_execution_workers: u32,
        pool_decay_seconds: f64,
        heartbeat_interval: f64,
        heartbeat_timeout: f64,
        max_total_chunks: u32,
        chunk_gc_interval: f64,
        chunk_threshold_ratio: f64,
        chunk_assembler_timeout: f64,
        max_reassembly_bytes: u64,
        chunk_size: u64,
        server_id: Option<String>,
        server_instance_id: Option<String>,
    ) -> PyResult<Self> {
        let config = ServerIpcConfig {
            base: BaseIpcConfig {
                pool_enabled,
                pool_segment_size,
                max_pool_segments,
                max_pool_memory: pool_segment_size
                    .checked_mul(u64::from(max_pool_segments))
                    .ok_or_else(|| {
                        PyRuntimeError::new_err("max_pool_memory derived value overflowed")
                    })?,
                reassembly_segment_size,
                reassembly_max_segments,
                max_total_chunks,
                chunk_gc_interval_secs: chunk_gc_interval,
                chunk_threshold_ratio,
                chunk_assembler_timeout_secs: chunk_assembler_timeout,
                max_reassembly_bytes,
                chunk_size,
            },
            shm_threshold,
            max_frame_size,
            max_payload_size,
            max_pending_requests,
            max_execution_workers,
            pool_decay_seconds,
            heartbeat_interval_secs: heartbeat_interval,
            heartbeat_timeout_secs: heartbeat_timeout,
        };
        config
            .validate()
            .map_err(|e| PyValueError::new_err(format!("invalid IPC server config: {e}")))?;
        let server = match (server_id, server_instance_id) {
            (Some(server_id), Some(server_instance_id)) => Server::new_with_identity(
                address,
                config,
                c2_server::ServerIdentity {
                    server_id,
                    server_instance_id,
                },
            ),
            (None, None) => Server::new(address, config),
            _ => Err(c2_server::ServerError::Config(
                "server_id and server_instance_id must be provided together".to_string(),
            )),
        }
        .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        Ok(Self {
            inner: Arc::new(server),
            rt: Mutex::new(None),
        })
    }

    /// Start the server on a background thread with a tokio runtime.
    ///
    /// The GIL is released while the runtime spins up.
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        self.start_and_wait(py, 5.0)
    }

    /// Start the server and wait until the native IPC listener is ready.
    #[pyo3(signature = (timeout_seconds=5.0))]
    fn start_and_wait(&self, py: Python<'_>, timeout_seconds: f64) -> PyResult<()> {
        if !timeout_seconds.is_finite() || timeout_seconds < 0.0 {
            return Err(PyValueError::new_err(
                "timeout_seconds must be a non-negative finite number",
            ));
        }
        let timeout = Duration::from_secs_f64(timeout_seconds);
        py.detach(|| self.start_runtime_and_wait(timeout))
    }

    /// Whether the server is currently running.
    #[getter]
    fn is_running(&self) -> bool {
        self.runtime_is_running()
    }

    /// Whether the server has reached native IPC readiness.
    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// The filesystem path of the UDS socket.
    #[getter]
    fn socket_path(&self) -> String {
        self.inner.socket_path().to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// parse_response_meta — convert Python return value to ResponseMeta
// ---------------------------------------------------------------------------

/// Convert Python dispatcher return value into ResponseMeta.
///
/// Expected return types:
/// - `None` → `ResponseMeta::Empty`
/// - `bytes` / buffer-protocol object → native response selection
fn parse_response_meta(
    py: Python<'_>,
    result: Py<PyAny>,
    response_pool: &parking_lot::RwLock<MemPool>,
    shm_threshold: u64,
    max_payload_size: u64,
) -> Result<ResponseMeta, CrmError> {
    let result = result.bind(py);

    // None → Empty
    if result.is_none() {
        return Ok(ResponseMeta::Empty);
    }

    if let Some(meta) = try_response_final_backing_meta(result)
        .map_err(|e| resource_output_error(format!("prepared response backing failed: {e}")))?
    {
        return Ok(meta);
    }

    // Prepared write plan → native response selection with direct destination fill.
    if let Some(len) = prepared_plan_nbytes(result)
        .map_err(|e| resource_output_error(format!("prepared response size failed: {e}")))?
    {
        if len == 0 {
            return Ok(ResponseMeta::Empty);
        }
        ensure_response_len_within_limit(len, max_payload_size)?;
        if let Some(meta) = try_prepare_shm_response(response_pool, shm_threshold, len, |dst| {
            write_python_payload_plan(py, result, dst).map_err(|e| e.to_string())
        })
        .map_err(resource_output_error)?
        {
            return Ok(meta);
        }
        let mut data = allocate_response_vec(len)?;
        write_python_payload_plan(py, result, &mut data).map_err(|e| {
            resource_output_error(format!("failed to write prepared response: {e}"))
        })?;
        return Ok(ResponseMeta::Inline(data));
    }

    // bytes → direct SHM preparation for large payloads, otherwise owned data.
    if let Ok(bytes) = result.cast::<PyBytes>() {
        let data = bytes.as_bytes();
        if data.is_empty() {
            return Ok(ResponseMeta::Empty);
        }
        ensure_response_len_within_limit(data.len(), max_payload_size)?;
        if let Some(meta) =
            try_prepare_shm_response(response_pool, shm_threshold, data.len(), |dst| {
                dst.copy_from_slice(data);
                Ok(())
            })
            .map_err(resource_output_error)?
        {
            return Ok(meta);
        }
        return Ok(ResponseMeta::Inline(copy_slice_to_response_vec(data)?));
    }

    // Generic buffer exporter → direct SHM preparation for large payloads.
    if let Ok(buffer) = PyBuffer::<u8>::get(result) {
        let len = buffer.len_bytes();
        if len == 0 {
            return Ok(ResponseMeta::Empty);
        }
        ensure_response_len_within_limit(len, max_payload_size)?;
        if let Some(meta) = try_prepare_shm_response(response_pool, shm_threshold, len, |dst| {
            buffer.copy_to_slice(py, dst).map_err(|e| e.to_string())
        })
        .map_err(resource_output_error)?
        {
            return Ok(meta);
        }
        let mut data = allocate_response_vec(len)?;
        buffer
            .copy_to_slice(py, &mut data)
            .map_err(|e| resource_output_error(format!("failed to copy response buffer: {e}")))?;
        return Ok(ResponseMeta::Inline(data));
    }

    Err(resource_output_error(
        "dispatcher must return None or a bytes-like buffer",
    ))
}

fn resource_output_error(message: impl Into<String>) -> CrmError {
    CrmError::UserError(
        C2Error::new(ErrorCode::ResourceOutputSerializing, message.into()).to_wire_bytes(),
    )
}

fn ensure_response_len_within_limit(len: usize, max_payload_size: u64) -> Result<(), CrmError> {
    let len_u64 = u64::try_from(len).unwrap_or(u64::MAX);
    if len_u64 > max_payload_size {
        return Err(resource_output_error(format!(
            "response payload size {len} exceeds max_payload_size {max_payload_size}"
        )));
    }
    Ok(())
}

fn allocate_response_vec(len: usize) -> Result<Vec<u8>, CrmError> {
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|e| resource_output_error(format!("failed to allocate response buffer: {e}")))?;
    data.resize(len, 0);
    Ok(data)
}

fn copy_slice_to_response_vec(data: &[u8]) -> Result<Vec<u8>, CrmError> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(data.len()).map_err(|e| {
        resource_output_error(format!("failed to allocate response fallback buffer: {e}"))
    })?;
    owned.extend_from_slice(data);
    Ok(owned)
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register server classes on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyServer>()?;
    Ok(())
}
