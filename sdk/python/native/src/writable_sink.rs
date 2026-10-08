//! Call-scoped writable buffer exposed to Python payload write plans.

use parking_lot::Mutex;
use pyo3::exceptions::{PyAttributeError, PyBufferError};
use pyo3::ffi;
use pyo3::prelude::*;

#[pyclass(name = "WritablePayloadSink", frozen)]
pub(crate) struct PyWritablePayloadSink {
    inner: Mutex<SinkState>,
}

struct SinkState {
    bytes: Option<Vec<u8>>,
    active: bool,
    exports: usize,
}

impl PyWritablePayloadSink {
    fn new(len: usize) -> Self {
        Self {
            inner: Mutex::new(SinkState {
                bytes: Some(vec![0; len]),
                active: true,
                exports: 0,
            }),
        }
    }

    fn close_inner(&self) {
        self.inner.lock().active = false;
    }

    fn take_bytes(&self) -> PyResult<Vec<u8>> {
        let mut state = self.inner.lock();
        state.active = false;
        if state.exports != 0 {
            return Err(PyBufferError::new_err(
                "prepared payload writer retained an exported destination buffer",
            ));
        }
        state
            .bytes
            .take()
            .ok_or_else(|| PyBufferError::new_err("writable payload sink is closed"))
    }
}

#[pymethods]
impl PyWritablePayloadSink {
    fn close(&self) {
        self.close_inner();
    }

    fn __len__(&self) -> usize {
        self.inner.lock().bytes.as_ref().map_or(0, Vec::len)
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let mut state = this.inner.lock();
        if !state.active {
            return Err(PyBufferError::new_err("writable payload sink is closed"));
        }
        let bytes = state
            .bytes
            .as_mut()
            .ok_or_else(|| PyBufferError::new_err("writable payload sink is closed"))?;
        // The Vec never moves/reallocates while exported. Each export owns a
        // Python reference to the sink, so escaped views pin the Vec even after
        // callback failure or timeout. Transfer requires zero exports under the
        // same lock that fences new acquisition. No unsafe Send wrapper exists.
        unsafe {
            (*view).buf = bytes.as_mut_ptr().cast();
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = bytes.len() as isize;
            (*view).readonly = 0;
            (*view).itemsize = 1;
            (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
                c"B".as_ptr().cast_mut()
            } else {
                std::ptr::null_mut()
            };
            (*view).ndim = 1;
            (*view).shape = if flags & ffi::PyBUF_ND != 0 {
                &mut (*view).len
            } else {
                std::ptr::null_mut()
            };
            (*view).strides = if flags & ffi::PyBUF_STRIDES != 0 {
                &mut (*view).itemsize
            } else {
                std::ptr::null_mut()
            };
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
        }
        state.exports += 1;
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.inner.lock().exports -= 1;
    }
}

pub(crate) fn prepared_plan_nbytes(plan: &Bound<'_, PyAny>) -> PyResult<Option<usize>> {
    match plan.getattr("write_into") {
        Ok(value) if value.is_callable() => {}
        Ok(_) => return Ok(None),
        Err(err) if !err.is_instance_of::<PyAttributeError>(plan.py()) => return Err(err),
        Err(_) => return Ok(None),
    }
    match plan.getattr("nbytes") {
        Ok(value) => return value.extract().map(Some),
        Err(err) if !err.is_instance_of::<PyAttributeError>(plan.py()) => return Err(err),
        Err(_) => {}
    }
    match plan.getattr("byte_length") {
        Ok(value) => value.extract().map(Some),
        Err(err) if !err.is_instance_of::<PyAttributeError>(plan.py()) => Err(err),
        Err(_) => Ok(None),
    }
}

pub(crate) fn materialize_python_payload_plan(
    py: Python<'_>,
    plan: &Bound<'_, PyAny>,
    nbytes: usize,
) -> PyResult<Vec<u8>> {
    let sink = Py::new(py, PyWritablePayloadSink::new(nbytes))?;
    let result = plan.call_method1("write_into", (sink.bind(py),));
    sink.bind(py).borrow().close_inner();
    result?;
    let bytes = sink.bind(py).borrow().take_bytes()?;
    Ok(bytes)
}
