use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyMapping};

use c2_config::{
    ClientIpcConfig, ClientIpcConfigOverrides, ConfigResolver, ConfigSources,
    RuntimeConfigOverrides, ServerIpcConfig, ServerIpcConfigOverrides,
};

#[pyfunction]
fn resolve_relay_anchor_address() -> PyResult<Option<String>> {
    c2_config::ConfigResolver::resolve_relay_anchor_address(ConfigSources::from_process())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
fn resolve_relay_use_proxy() -> PyResult<bool> {
    c2_config::ConfigResolver::resolve_relay_use_proxy(ConfigSources::from_process())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
#[pyo3(signature = (global_overrides=None))]
fn resolve_shm_threshold(global_overrides: Option<&Bound<'_, PyDict>>) -> PyResult<u64> {
    let mut overrides = RuntimeConfigOverrides::default();
    apply_shm_overrides(&mut overrides, global_overrides)?;
    ConfigResolver::resolve_shm_threshold(overrides.shm_threshold, ConfigSources::from_process())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
#[pyo3(signature = (override_value=None))]
fn resolve_remote_payload_chunk_size(override_value: Option<u64>) -> PyResult<u64> {
    ConfigResolver::resolve_remote_payload_chunk_size(override_value, ConfigSources::from_process())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
#[pyo3(signature = (overrides=None, global_overrides=None))]
fn resolve_server_ipc_config(
    py: Python<'_>,
    overrides: Option<&Bound<'_, PyAny>>,
    global_overrides: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let mut runtime = RuntimeConfigOverrides::default();
    let server_overrides = parse_server_ipc_overrides(overrides)?;
    apply_shm_overrides(&mut runtime, global_overrides)?;
    let resolved = ConfigResolver::resolve_server_ipc(
        server_overrides,
        runtime,
        ConfigSources::from_process(),
    )
    .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(server_ipc_to_dict(py, &resolved)?.into_any().unbind())
}

#[pyfunction]
#[pyo3(signature = (overrides=None, global_overrides=None))]
fn resolve_client_ipc_config(
    py: Python<'_>,
    overrides: Option<&Bound<'_, PyAny>>,
    global_overrides: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let mut runtime = RuntimeConfigOverrides::default();
    let client_overrides = parse_client_ipc_overrides(overrides)?;
    apply_shm_overrides(&mut runtime, global_overrides)?;
    let resolved = ConfigResolver::resolve_client_ipc(
        client_overrides,
        runtime,
        ConfigSources::from_process(),
    )
    .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(client_ipc_to_dict(py, &resolved)?.into_any().unbind())
}

#[pyfunction]
fn validate_server_id(server_id: &str) -> PyResult<()> {
    c2_config::validate_server_id(server_id).map_err(PyValueError::new_err)
}

#[pyfunction]
fn validate_ipc_region_id(region_id: &str) -> PyResult<()> {
    c2_config::validate_ipc_region_id(region_id).map_err(PyValueError::new_err)
}

/// Opaque projection of one Core-owned deadline across SDK glue and transport.
#[pyclass(name = "ConnectAttempt", module = "c_two._native", frozen)]
pub(crate) struct PyConnectAttempt {
    pub(crate) inner: c2_core::ConnectAttempt,
}

#[pymethods]
impl PyConnectAttempt {
    #[new]
    #[pyo3(signature = (timeout_seconds=None))]
    fn new(timeout_seconds: Option<f64>) -> PyResult<Self> {
        let options = c2_config::ConnectOptions::from_timeout_secs(timeout_seconds)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        c2_core::ConnectAttempt::start(options)
            .map(|inner| Self { inner })
            .map_err(crate::core_error_ffi::core_error_to_py)
    }

    fn check(&self, stage: &str) -> PyResult<()> {
        self.inner
            .check(stage)
            .map_err(crate::core_error_ffi::core_error_to_py)
    }

    /// Python owns its binding locks; Core supplies all timing and errors.
    /// Python's native lock acquire releases the GIL while it waits.
    fn acquire_lock(&self, py: Python<'_>, lock: &Bound<'_, PyAny>, stage: &str) -> PyResult<()> {
        loop {
            let remaining = self
                .inner
                .remaining(stage)
                .map_err(crate::core_error_ffi::core_error_to_py)?;
            let acquired: bool = match remaining {
                None => lock.call_method0("acquire")?.extract()?,
                Some(_) => {
                    let max_timeout: f64 =
                        py.import("threading")?.getattr("TIMEOUT_MAX")?.extract()?;
                    let kwargs = PyDict::new(py);
                    // Imports/argument setup consume the same budget too.
                    let remaining = self
                        .inner
                        .remaining(stage)
                        .map_err(crate::core_error_ffi::core_error_to_py)?
                        .expect("a finite attempt remains finite");
                    kwargs.set_item("timeout", remaining.as_secs_f64().min(max_timeout))?;
                    lock.call_method("acquire", (), Some(&kwargs))?.extract()?
                }
            };
            if acquired {
                if let Err(error) = self.inner.check(stage) {
                    lock.call_method0("release")?;
                    return Err(crate::core_error_ffi::core_error_to_py(error));
                }
                return Ok(());
            }
            self.check(stage)?;
        }
    }
}

fn apply_shm_overrides(
    overrides: &mut RuntimeConfigOverrides,
    global: Option<&Bound<'_, PyDict>>,
) -> PyResult<()> {
    let Some(global) = global else {
        return Ok(());
    };
    overrides.shm_threshold = get_opt(global, "shm_threshold")?;
    Ok(())
}

pub(crate) fn parse_server_ipc_overrides(
    overrides: Option<&Bound<'_, PyAny>>,
) -> PyResult<ServerIpcConfigOverrides> {
    let Some(dict) = copy_ipc_overrides(overrides)? else {
        return Ok(ServerIpcConfigOverrides::default());
    };
    parse_server_ipc_overrides_dict(&dict)
}

fn parse_server_ipc_overrides_dict(dict: &Bound<'_, PyDict>) -> PyResult<ServerIpcConfigOverrides> {
    validate_ipc_override_key_names(dict)?;
    reject_forbidden_ipc_fields(dict)?;
    reject_unknown_ipc_fields(dict, c2_config::SERVER_IPC_OVERRIDE_KEYS)?;
    Ok(ServerIpcConfigOverrides {
        base: c2_config::BaseIpcConfigOverrides {
            ..Default::default()
        },
        pool_enabled: get_opt(dict, "pool_enabled")?,
        pool_segment_size: get_opt(dict, "pool_segment_size")?,
        max_pool_segments: get_opt(dict, "max_pool_segments")?,
        pool_prewarm_segments: get_opt(dict, "pool_prewarm_segments")?,
        pool_min_retained_segments: get_opt(dict, "pool_min_retained_segments")?,
        reassembly_segment_size: get_opt(dict, "reassembly_segment_size")?,
        reassembly_max_segments: get_opt(dict, "reassembly_max_segments")?,
        max_total_chunks: get_opt(dict, "max_total_chunks")?,
        chunk_gc_interval_secs: get_opt(dict, "chunk_gc_interval")?,
        chunk_threshold_ratio: get_opt(dict, "chunk_threshold_ratio")?,
        chunk_assembler_timeout_secs: get_opt(dict, "chunk_assembler_timeout")?,
        max_reassembly_bytes: get_opt(dict, "max_reassembly_bytes")?,
        chunk_size: get_opt(dict, "chunk_size")?,
        shm_backing_budget_bytes: get_opt(dict, "shm_backing_budget_bytes")?,
        file_backing_budget_bytes: get_opt(dict, "file_backing_budget_bytes")?,
        live_reassembly_budget_bytes: get_opt(dict, "live_reassembly_budget_bytes")?,
        max_frame_size: get_opt(dict, "max_frame_size")?,
        max_payload_size: get_opt(dict, "max_payload_size")?,
        max_pending_requests: get_opt(dict, "max_pending_requests")?,
        max_execution_workers: get_opt(dict, "max_execution_workers")?,
        pool_decay_seconds: get_opt(dict, "pool_decay_seconds")?,
        heartbeat_interval_secs: get_opt(dict, "heartbeat_interval")?,
        heartbeat_timeout_secs: get_opt(dict, "heartbeat_timeout")?,
        ..Default::default()
    })
}

pub(crate) fn parse_client_ipc_overrides(
    overrides: Option<&Bound<'_, PyAny>>,
) -> PyResult<ClientIpcConfigOverrides> {
    client_overrides(overrides)
}

pub(crate) fn server_ipc_overrides_to_dict<'py>(
    py: Python<'py>,
    overrides: &ServerIpcConfigOverrides,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    if let Some(value) = overrides.base.pool_enabled {
        dict.set_item("pool_enabled", value)?;
    }
    if let Some(value) = overrides.base.pool_segment_size {
        dict.set_item("pool_segment_size", value)?;
    }
    if let Some(value) = overrides.base.max_pool_segments {
        dict.set_item("max_pool_segments", value)?;
    }
    if let Some(value) = overrides.base.pool_prewarm_segments {
        dict.set_item("pool_prewarm_segments", value)?;
    }
    if let Some(value) = overrides.base.pool_min_retained_segments {
        dict.set_item("pool_min_retained_segments", value)?;
    }
    if let Some(value) = overrides.base.reassembly_segment_size {
        dict.set_item("reassembly_segment_size", value)?;
    }
    if let Some(value) = overrides.base.reassembly_max_segments {
        dict.set_item("reassembly_max_segments", value)?;
    }
    if let Some(value) = overrides.base.max_total_chunks {
        dict.set_item("max_total_chunks", value)?;
    }
    if let Some(value) = overrides.base.chunk_gc_interval_secs {
        dict.set_item("chunk_gc_interval", value)?;
    }
    if let Some(value) = overrides.base.chunk_threshold_ratio {
        dict.set_item("chunk_threshold_ratio", value)?;
    }
    if let Some(value) = overrides.base.chunk_assembler_timeout_secs {
        dict.set_item("chunk_assembler_timeout", value)?;
    }
    if let Some(value) = overrides.base.max_reassembly_bytes {
        dict.set_item("max_reassembly_bytes", value)?;
    }
    if let Some(value) = overrides.base.chunk_size {
        dict.set_item("chunk_size", value)?;
    }
    if let Some(value) = overrides.base.shm_backing_budget_bytes {
        dict.set_item("shm_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.base.file_backing_budget_bytes {
        dict.set_item("file_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.base.live_reassembly_budget_bytes {
        dict.set_item("live_reassembly_budget_bytes", value)?;
    }
    if let Some(value) = overrides.pool_enabled {
        dict.set_item("pool_enabled", value)?;
    }
    if let Some(value) = overrides.pool_segment_size {
        dict.set_item("pool_segment_size", value)?;
    }
    if let Some(value) = overrides.max_pool_segments {
        dict.set_item("max_pool_segments", value)?;
    }
    if let Some(value) = overrides.pool_prewarm_segments {
        dict.set_item("pool_prewarm_segments", value)?;
    }
    if let Some(value) = overrides.pool_min_retained_segments {
        dict.set_item("pool_min_retained_segments", value)?;
    }
    if let Some(value) = overrides.reassembly_segment_size {
        dict.set_item("reassembly_segment_size", value)?;
    }
    if let Some(value) = overrides.reassembly_max_segments {
        dict.set_item("reassembly_max_segments", value)?;
    }
    if let Some(value) = overrides.max_total_chunks {
        dict.set_item("max_total_chunks", value)?;
    }
    if let Some(value) = overrides.chunk_gc_interval_secs {
        dict.set_item("chunk_gc_interval", value)?;
    }
    if let Some(value) = overrides.chunk_threshold_ratio {
        dict.set_item("chunk_threshold_ratio", value)?;
    }
    if let Some(value) = overrides.chunk_assembler_timeout_secs {
        dict.set_item("chunk_assembler_timeout", value)?;
    }
    if let Some(value) = overrides.max_reassembly_bytes {
        dict.set_item("max_reassembly_bytes", value)?;
    }
    if let Some(value) = overrides.chunk_size {
        dict.set_item("chunk_size", value)?;
    }
    if let Some(value) = overrides.shm_backing_budget_bytes {
        dict.set_item("shm_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.file_backing_budget_bytes {
        dict.set_item("file_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.live_reassembly_budget_bytes {
        dict.set_item("live_reassembly_budget_bytes", value)?;
    }
    if let Some(value) = overrides.max_frame_size {
        dict.set_item("max_frame_size", value)?;
    }
    if let Some(value) = overrides.max_payload_size {
        dict.set_item("max_payload_size", value)?;
    }
    if let Some(value) = overrides.max_pending_requests {
        dict.set_item("max_pending_requests", value)?;
    }
    if let Some(value) = overrides.max_execution_workers {
        dict.set_item("max_execution_workers", value)?;
    }
    if let Some(value) = overrides.pool_decay_seconds {
        dict.set_item("pool_decay_seconds", value)?;
    }
    if let Some(value) = overrides.heartbeat_interval_secs {
        dict.set_item("heartbeat_interval", value)?;
    }
    if let Some(value) = overrides.heartbeat_timeout_secs {
        dict.set_item("heartbeat_timeout", value)?;
    }
    Ok(dict)
}

pub(crate) fn client_ipc_overrides_to_dict<'py>(
    py: Python<'py>,
    overrides: &ClientIpcConfigOverrides,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    if let Some(value) = overrides.base.pool_enabled {
        dict.set_item("pool_enabled", value)?;
    }
    if let Some(value) = overrides.base.pool_segment_size {
        dict.set_item("pool_segment_size", value)?;
    }
    if let Some(value) = overrides.base.max_pool_segments {
        dict.set_item("max_pool_segments", value)?;
    }
    if let Some(value) = overrides.base.pool_prewarm_segments {
        dict.set_item("pool_prewarm_segments", value)?;
    }
    if let Some(value) = overrides.base.pool_min_retained_segments {
        dict.set_item("pool_min_retained_segments", value)?;
    }
    if let Some(value) = overrides.base.reassembly_segment_size {
        dict.set_item("reassembly_segment_size", value)?;
    }
    if let Some(value) = overrides.base.reassembly_max_segments {
        dict.set_item("reassembly_max_segments", value)?;
    }
    if let Some(value) = overrides.base.max_total_chunks {
        dict.set_item("max_total_chunks", value)?;
    }
    if let Some(value) = overrides.base.chunk_gc_interval_secs {
        dict.set_item("chunk_gc_interval", value)?;
    }
    if let Some(value) = overrides.base.chunk_threshold_ratio {
        dict.set_item("chunk_threshold_ratio", value)?;
    }
    if let Some(value) = overrides.base.chunk_assembler_timeout_secs {
        dict.set_item("chunk_assembler_timeout", value)?;
    }
    if let Some(value) = overrides.base.max_reassembly_bytes {
        dict.set_item("max_reassembly_bytes", value)?;
    }
    if let Some(value) = overrides.base.chunk_size {
        dict.set_item("chunk_size", value)?;
    }
    if let Some(value) = overrides.base.shm_backing_budget_bytes {
        dict.set_item("shm_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.base.file_backing_budget_bytes {
        dict.set_item("file_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.base.live_reassembly_budget_bytes {
        dict.set_item("live_reassembly_budget_bytes", value)?;
    }
    if let Some(value) = overrides.pool_enabled {
        dict.set_item("pool_enabled", value)?;
    }
    if let Some(value) = overrides.pool_segment_size {
        dict.set_item("pool_segment_size", value)?;
    }
    if let Some(value) = overrides.max_pool_segments {
        dict.set_item("max_pool_segments", value)?;
    }
    if let Some(value) = overrides.pool_prewarm_segments {
        dict.set_item("pool_prewarm_segments", value)?;
    }
    if let Some(value) = overrides.pool_min_retained_segments {
        dict.set_item("pool_min_retained_segments", value)?;
    }
    if let Some(value) = overrides.reassembly_segment_size {
        dict.set_item("reassembly_segment_size", value)?;
    }
    if let Some(value) = overrides.reassembly_max_segments {
        dict.set_item("reassembly_max_segments", value)?;
    }
    if let Some(value) = overrides.max_total_chunks {
        dict.set_item("max_total_chunks", value)?;
    }
    if let Some(value) = overrides.chunk_gc_interval_secs {
        dict.set_item("chunk_gc_interval", value)?;
    }
    if let Some(value) = overrides.chunk_threshold_ratio {
        dict.set_item("chunk_threshold_ratio", value)?;
    }
    if let Some(value) = overrides.chunk_assembler_timeout_secs {
        dict.set_item("chunk_assembler_timeout", value)?;
    }
    if let Some(value) = overrides.max_reassembly_bytes {
        dict.set_item("max_reassembly_bytes", value)?;
    }
    if let Some(value) = overrides.chunk_size {
        dict.set_item("chunk_size", value)?;
    }
    if let Some(value) = overrides.pool_decay_seconds {
        dict.set_item("pool_decay_seconds", value)?;
    }
    if let Some(value) = overrides.shm_backing_budget_bytes {
        dict.set_item("shm_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.file_backing_budget_bytes {
        dict.set_item("file_backing_budget_bytes", value)?;
    }
    if let Some(value) = overrides.live_reassembly_budget_bytes {
        dict.set_item("live_reassembly_budget_bytes", value)?;
    }
    Ok(dict)
}

fn copy_ipc_overrides<'py>(
    overrides: Option<&Bound<'py, PyAny>>,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    let Some(overrides) = overrides else {
        return Ok(None);
    };
    if overrides.is_none() {
        return Ok(None);
    }
    let mapping = overrides
        .cast::<PyMapping>()
        .map_err(|_| PyTypeError::new_err("ipc_overrides must be a mapping"))?;
    let copied = PyDict::new(overrides.py());
    copied.update(mapping)?;
    Ok(Some(copied))
}

fn client_overrides(overrides: Option<&Bound<'_, PyAny>>) -> PyResult<ClientIpcConfigOverrides> {
    let Some(dict) = copy_ipc_overrides(overrides)? else {
        return Ok(ClientIpcConfigOverrides::default());
    };
    validate_ipc_override_key_names(&dict)?;
    reject_forbidden_ipc_fields(&dict)?;
    reject_unknown_ipc_fields(&dict, c2_config::CLIENT_IPC_OVERRIDE_KEYS)?;
    Ok(ClientIpcConfigOverrides {
        base: c2_config::BaseIpcConfigOverrides {
            ..Default::default()
        },
        pool_enabled: get_opt(&dict, "pool_enabled")?,
        pool_segment_size: get_opt(&dict, "pool_segment_size")?,
        max_pool_segments: get_opt(&dict, "max_pool_segments")?,
        pool_prewarm_segments: get_opt(&dict, "pool_prewarm_segments")?,
        pool_min_retained_segments: get_opt(&dict, "pool_min_retained_segments")?,
        reassembly_segment_size: get_opt(&dict, "reassembly_segment_size")?,
        reassembly_max_segments: get_opt(&dict, "reassembly_max_segments")?,
        max_total_chunks: get_opt(&dict, "max_total_chunks")?,
        chunk_gc_interval_secs: get_opt(&dict, "chunk_gc_interval")?,
        chunk_threshold_ratio: get_opt(&dict, "chunk_threshold_ratio")?,
        chunk_assembler_timeout_secs: get_opt(&dict, "chunk_assembler_timeout")?,
        max_reassembly_bytes: get_opt(&dict, "max_reassembly_bytes")?,
        chunk_size: get_opt(&dict, "chunk_size")?,
        pool_decay_seconds: get_opt(&dict, "pool_decay_seconds")?,
        shm_backing_budget_bytes: get_opt(&dict, "shm_backing_budget_bytes")?,
        file_backing_budget_bytes: get_opt(&dict, "file_backing_budget_bytes")?,
        live_reassembly_budget_bytes: get_opt(&dict, "live_reassembly_budget_bytes")?,
        ..Default::default()
    })
}

fn reject_forbidden_ipc_fields(dict: &Bound<'_, PyDict>) -> PyResult<()> {
    for key in c2_config::FORBIDDEN_IPC_OVERRIDE_KEYS {
        if dict.contains(*key)? {
            return Err(PyValueError::new_err(
                "shm_threshold is a global transport policy; use set_transport_policy(shm_threshold=...)",
            ));
        }
    }
    Ok(())
}

fn reject_unknown_ipc_fields(dict: &Bound<'_, PyDict>, allowed: &[&str]) -> PyResult<()> {
    for item in dict.keys().iter() {
        let key: String = item.extract()?;
        if !allowed.contains(&key.as_str()) {
            return Err(PyValueError::new_err(format!(
                "unknown IPC override option: {key}"
            )));
        }
    }
    Ok(())
}

fn validate_ipc_override_key_names(dict: &Bound<'_, PyDict>) -> PyResult<()> {
    for item in dict.keys().iter() {
        item.extract::<String>()
            .map_err(|_| PyTypeError::new_err("IPC override option names must be strings"))?;
    }
    Ok(())
}

fn get_opt<T>(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<T>>
where
    T: for<'a, 'py> FromPyObject<'a, 'py, Error = PyErr>,
{
    match dict.get_item(key)? {
        Some(value) if !value.is_none() => Ok(Some(value.extract::<T>()?)),
        _ => Ok(None),
    }
}

fn server_ipc_to_dict<'py>(py: Python<'py>, cfg: &ServerIpcConfig) -> PyResult<Bound<'py, PyDict>> {
    let dict = base_ipc_to_dict(py, &cfg.base)?;
    dict.set_item("shm_threshold", cfg.shm_threshold)?;
    dict.set_item("max_frame_size", cfg.max_frame_size)?;
    dict.set_item("max_payload_size", cfg.max_payload_size)?;
    dict.set_item("max_pending_requests", cfg.max_pending_requests)?;
    dict.set_item("max_execution_workers", cfg.max_execution_workers)?;
    dict.set_item("pool_decay_seconds", cfg.pool_decay_seconds)?;
    dict.set_item("heartbeat_interval", cfg.heartbeat_interval_secs)?;
    dict.set_item("heartbeat_timeout", cfg.heartbeat_timeout_secs)?;
    Ok(dict)
}

pub(crate) fn client_ipc_to_dict<'py>(
    py: Python<'py>,
    cfg: &ClientIpcConfig,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = base_ipc_to_dict(py, &cfg.base)?;
    dict.set_item("shm_threshold", cfg.shm_threshold)?;
    dict.set_item("pool_decay_seconds", cfg.pool_decay_seconds)?;
    Ok(dict)
}

fn base_ipc_to_dict<'py>(
    py: Python<'py>,
    cfg: &c2_config::BaseIpcConfig,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("pool_enabled", cfg.pool_enabled)?;
    dict.set_item("pool_segment_size", cfg.pool_segment_size)?;
    dict.set_item("max_pool_segments", cfg.max_pool_segments)?;
    dict.set_item("max_pool_memory", cfg.max_pool_memory)?;
    dict.set_item("pool_prewarm_segments", cfg.pool_prewarm_segments)?;
    dict.set_item("pool_min_retained_segments", cfg.pool_min_retained_segments)?;
    dict.set_item("reassembly_segment_size", cfg.reassembly_segment_size)?;
    dict.set_item("reassembly_max_segments", cfg.reassembly_max_segments)?;
    dict.set_item("max_total_chunks", cfg.max_total_chunks)?;
    dict.set_item("chunk_gc_interval", cfg.chunk_gc_interval_secs)?;
    dict.set_item("chunk_threshold_ratio", cfg.chunk_threshold_ratio)?;
    dict.set_item("chunk_assembler_timeout", cfg.chunk_assembler_timeout_secs)?;
    dict.set_item("max_reassembly_bytes", cfg.max_reassembly_bytes)?;
    dict.set_item("chunk_size", cfg.chunk_size)?;
    dict.set_item("shm_backing_budget_bytes", cfg.shm_backing_budget_bytes)?;
    dict.set_item("file_backing_budget_bytes", cfg.file_backing_budget_bytes)?;
    dict.set_item(
        "live_reassembly_budget_bytes",
        cfg.live_reassembly_budget_bytes,
    )?;
    Ok(dict)
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConnectAttempt>()?;
    m.add_function(wrap_pyfunction!(resolve_relay_anchor_address, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_relay_use_proxy, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_shm_threshold, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_remote_payload_chunk_size, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_server_ipc_config, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_client_ipc_config, m)?)?;
    m.add_function(wrap_pyfunction!(validate_server_id, m)?)?;
    m.add_function(wrap_pyfunction!(validate_ipc_region_id, m)?)?;
    Ok(())
}

#[cfg(test)]
mod connect_attempt_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn python_lock_projection_expires_before_owner_releases_and_recovers() {
        Python::initialize();
        for stage in [
            "registry_init_wait",
            "registry_snapshot_wait",
            "local_slot_wait",
        ] {
            let lock = Python::attach(|py| {
                py.import("threading")
                    .unwrap()
                    .getattr("Lock")
                    .unwrap()
                    .call0()
                    .unwrap()
                    .unbind()
            });
            let owner_lock = Python::attach(|py| lock.clone_ref(py));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let owner = std::thread::spawn(move || {
                Python::attach(|py| owner_lock.bind(py).call_method0("acquire").map(|_| ()))
                    .unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                Python::attach(|py| owner_lock.bind(py).call_method0("release").map(|_| ()))
                    .unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let attempt = PyConnectAttempt::new(Some(0.1)).unwrap();
            let started = Instant::now();
            Python::attach(|py| {
                let error = attempt.acquire_lock(py, lock.bind(py), stage).unwrap_err();
                assert!(started.elapsed() < Duration::from_millis(250));
                assert!(
                    lock.bind(py)
                        .call_method0("locked")
                        .unwrap()
                        .extract::<bool>()
                        .unwrap()
                );
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
                    stage
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
            owner.join().unwrap();
            let recovered = PyConnectAttempt::new(None).unwrap();
            Python::attach(|py| {
                recovered.acquire_lock(py, lock.bind(py), stage).unwrap();
                lock.bind(py).call_method0("release").unwrap();
            });
        }
    }
}
