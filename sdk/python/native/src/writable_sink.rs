//! Call-scoped writable buffer exposed to Python payload write plans.

use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::{PyAttributeError, PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;

#[pyclass(name = "WritablePayloadSink")]
pub(crate) struct PyWritablePayloadSink {
    ptr: *mut u8,
    len: usize,
    active: AtomicBool,
}

// The sink is a call-scoped buffer-protocol facade over memory owned by the
// native request/response writer. Dropping or inspecting the Python object on
// another thread is safe because Rust never dereferences `ptr` outside
// `__getbuffer__`; active buffer acquisition is fenced by `active`, and
// existing exported buffers remain the caller's unsafe responsibility until the
// write callback returns.
unsafe impl Send for PyWritablePayloadSink {}
unsafe impl Sync for PyWritablePayloadSink {}

impl PyWritablePayloadSink {
    fn new(ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr,
            len,
            active: AtomicBool::new(true),
        }
    }

    fn close_inner(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

#[pymethods]
impl PyWritablePayloadSink {
    fn close(&self) {
        self.close_inner();
    }

    fn __len__(&self) -> usize {
        self.len
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        if !this.active.load(Ordering::SeqCst) {
            return Err(PyBufferError::new_err("writable payload sink is closed"));
        }
        if this.ptr.is_null() {
            return Err(PyBufferError::new_err(
                "writable payload sink has no buffer",
            ));
        }
        unsafe {
            (*view).buf = this.ptr as *mut std::os::raw::c_void;
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = this.len as isize;
            (*view).readonly = 0;
            (*view).itemsize = 1;
            (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
                b"B\0".as_ptr() as *mut std::os::raw::c_char
            } else {
                std::ptr::null_mut()
            };
            (*view).ndim = 1;
            (*view).shape = if flags & ffi::PyBUF_ND != 0 {
                &mut (*view).len as *mut isize
            } else {
                std::ptr::null_mut()
            };
            (*view).strides = if flags & ffi::PyBUF_STRIDES != 0 {
                &mut (*view).itemsize as *mut isize
            } else {
                std::ptr::null_mut()
            };
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
        }
        Ok(())
    }

    unsafe fn __releasebuffer__(
        _slf: &Bound<'_, Self>,
        _view: *mut ffi::Py_buffer,
    ) -> PyResult<()> {
        Ok(())
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

pub(crate) fn write_python_payload_plan(
    py: Python<'_>,
    plan: &Bound<'_, PyAny>,
    destination: &mut [u8],
) -> PyResult<()> {
    let sink = Py::new(
        py,
        PyWritablePayloadSink::new(destination.as_mut_ptr(), destination.len()),
    )?;
    let sink_bound = sink.bind(py);
    let result = plan.call_method1("write_into", (sink_bound,));
    sink_bound.borrow().close_inner();
    result.map(|_| ()).map_err(|err| {
        PyValueError::new_err(format!("prepared payload write failed: {}", err.value(py)))
    })
}
