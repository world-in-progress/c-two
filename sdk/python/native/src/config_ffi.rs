use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyInt};

use c2_config::{ConfigField, ConfigResolver, ConfigSources, EnvCatalog};

fn config_error(error: c2_config::ConfigError) -> PyErr {
    PyValueError::new_err(error.to_string())
}

#[pyfunction]
fn resolve_relay_anchor_address() -> PyResult<Option<String>> {
    let catalog = EnvCatalog::load(ConfigSources::from_process()).map_err(config_error)?;
    ConfigResolver::resolve_relay_anchor_address(&catalog).map_err(config_error)
}

#[pyfunction]
fn resolve_relay_use_proxy() -> PyResult<bool> {
    let catalog = EnvCatalog::load(ConfigSources::from_process()).map_err(config_error)?;
    ConfigResolver::resolve_relay_client(None, &catalog)
        .map(|config| config.use_proxy)
        .map_err(config_error)
}

pub(crate) fn config_field<T>(
    value: &Bound<'_, PyAny>,
    unset: &Bound<'_, PyAny>,
    parse: impl FnOnce(&Bound<'_, PyAny>) -> PyResult<T>,
) -> PyResult<ConfigField<T>> {
    if value.is(unset) {
        Ok(ConfigField::Keep)
    } else if value.is_none() {
        Ok(ConfigField::Inherit)
    } else {
        parse(value).map(ConfigField::Set)
    }
}

pub(crate) fn config_uint<T>(value: &Bound<'_, PyAny>, field: &str) -> PyResult<T>
where
    for<'py> T: FromPyObject<'py, 'py, Error = PyErr>,
{
    if !value.is_instance_of::<PyInt>() || value.is_instance_of::<PyBool>() {
        return Err(PyTypeError::new_err(format!("{field} must be an integer")));
    }
    value.extract()
}

#[pyfunction]
fn validate_server_id(server_id: &str) -> PyResult<()> {
    c2_config::validate_server_id(server_id).map_err(PyValueError::new_err)
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

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConnectAttempt>()?;
    m.add_function(wrap_pyfunction!(resolve_relay_anchor_address, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_relay_use_proxy, m)?)?;
    m.add_function(wrap_pyfunction!(validate_server_id, m)?)?;
    Ok(())
}

#[cfg(test)]
mod connect_attempt_tests {
    use super::*;
    use std::time::Duration;

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
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let owner = std::thread::spawn(move || {
                Python::attach(|py| owner_lock.bind(py).call_method0("acquire").map(|_| ()))
                    .unwrap();
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Python::attach(|py| owner_lock.bind(py).call_method0("release").map(|_| ()))
                    .unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let waiter_lock = Python::attach(|py| lock.clone_ref(py));
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let observation = Python::attach(|py| {
                    let attempt = PyConnectAttempt::new(Some(0.1)).unwrap();
                    let result = attempt.acquire_lock(py, waiter_lock.bind(py), stage);
                    let still_locked = waiter_lock
                        .bind(py)
                        .call_method0("locked")
                        .unwrap()
                        .extract::<bool>()
                        .unwrap();
                    (result, still_locked)
                });
                done_tx.send(observation).unwrap();
            });
            let observed = done_rx.recv_timeout(Duration::from_secs(2));
            release_tx.send(()).unwrap();
            owner.join().unwrap();
            waiter.join().unwrap();
            let (result, still_locked) =
                observed.expect("connect must expire before owner release");
            assert!(still_locked);
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
            let recovered = PyConnectAttempt::new(None).unwrap();
            Python::attach(|py| {
                recovered.acquire_lock(py, lock.bind(py), stage).unwrap();
                lock.bind(py).call_method0("release").unwrap();
            });
        }
    }
}
