//! Python-owned iterators and immutable item snapshots over Core stream records.

use std::ffi::CStr;
use std::time::Duration;

use parking_lot::Mutex;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::PyBufferError;
use pyo3::ffi;

use c2_core::{CoreResponseStream, CoreStreamCloseReceipt, EncodedStream, EncodedStreamItem};
use c2_error::C2Error;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::types::{PyBytes, PyDict, PyMemoryView};

use crate::core_error_ffi::core_error_to_py;
use crate::core_ffi::{
    PyCoreResponse, deferred_plan_len, materialize_deferred_plan, python_error_to_c2,
    write_deferred_range,
};

#[pyclass(name = "CoreResponseStream", frozen, skip_from_py_object)]
pub(crate) struct PyCoreResponseStream {
    inner: CoreResponseStream,
}

impl PyCoreResponseStream {
    pub(crate) fn new(inner: CoreResponseStream) -> Self {
        Self { inner }
    }
}

pub(crate) fn close_timeout(timeout: Option<f64>) -> PyResult<Option<Duration>> {
    timeout
        .map(|seconds| {
            let timeout = c2_core::CallTimeout::try_after_seconds(seconds)
                .map_err(|error| PyValueError::new_err(error.to_string()))?;
            let c2_core::CallTimeout::After(duration) = timeout else {
                unreachable!("seconds produce a duration")
            };
            c2_config::deadline_after(std::time::Instant::now(), duration)
                .map_err(|error| PyValueError::new_err(error.to_string()))?;
            Ok(duration)
        })
        .transpose()
}

pub(crate) fn close_receipt<'py>(
    py: Python<'py>,
    receipt: CoreStreamCloseReceipt,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("completed", receipt.completed)?;
    result.set_item("complete_items", receipt.complete_items)?;
    match receipt.terminal {
        Some(Err(error)) => result.set_item("error", core_error_to_py(error).value(py))?,
        _ => result.set_item("error", py.None())?,
    }
    Ok(result)
}

#[pymethods]
impl PyCoreResponseStream {
    fn next_item(&self, py: Python<'_>) -> PyResult<Option<PyCoreResponse>> {
        py.detach(|| self.inner.next_item())
            .map(|item| item.map(PyCoreResponse::new))
            .map_err(core_error_to_py)
    }

    fn check_deadline(&self) -> PyResult<()> {
        self.inner.check_deadline().map_err(core_error_to_py)
    }

    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.cancel()).map_err(core_error_to_py)
    }

    #[pyo3(signature = (timeout=Some(30.0)))]
    fn close<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyDict>> {
        let timeout = close_timeout(timeout)?;
        let receipt = py.detach(|| self.inner.close_with_timeout(timeout));
        close_receipt(py, receipt)
    }
}

pub(crate) struct PyEncodedStream {
    pub(crate) iterator: Py<PyAny>,
}

struct PyObjectStream {
    iterator: Py<PyAny>,
}

impl c2_core::LocalObjectStream<Py<PyAny>> for PyObjectStream {
    fn step(&mut self) -> Result<Option<Py<PyAny>>, C2Error> {
        Python::attach(|py| {
            let item = self
                .iterator
                .call_method0(py, "step")
                .map_err(|error| python_error_to_c2(py, error))?;
            let (present, value): (bool, Py<PyAny>) = item
                .extract(py)
                .map_err(|error| python_error_to_c2(py, error))?;
            Ok(present.then_some(value))
        })
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Python::attach(|py| {
            self.iterator
                .call_method0(py, "close")
                .map(|_| ())
                .map_err(|error| python_error_to_c2(py, error))
        })
    }
}

#[pyclass(name = "CoreLocalResponseStream", frozen, skip_from_py_object)]
pub(crate) struct PyCoreLocalResponseStream {
    inner: c2_core::CoreLocalResponseStream<Py<PyAny>>,
}

impl PyCoreLocalResponseStream {
    pub(crate) fn open(
        py: Python<'_>,
        route: c2_core::RouteConcurrency,
        method_index: u16,
        factory: Py<PyAny>,
    ) -> PyResult<Self> {
        let inner = py
            .detach(move || {
                route.open_local_stream(method_index, move || {
                    Python::attach(|py| {
                        let iterator = factory
                            .call0(py)
                            .map_err(|error| python_error_to_c2(py, error))?;
                        Ok(Box::new(PyObjectStream { iterator })
                            as Box<dyn c2_core::LocalObjectStream<Py<PyAny>>>)
                    })
                })
            })
            .map_err(core_error_to_py)?;
        Ok(Self { inner })
    }
}

#[pymethods]
impl PyCoreLocalResponseStream {
    fn next_item(&self, py: Python<'_>) -> PyResult<(bool, Py<PyAny>)> {
        match py
            .detach(|| self.inner.next_item())
            .map_err(core_error_to_py)?
        {
            Some(item) => Ok((true, item)),
            None => Ok((false, py.None())),
        }
    }

    fn cancel(&self, py: Python<'_>) {
        py.detach(|| self.inner.cancel());
    }

    #[pyo3(signature = (timeout=Some(30.0)))]
    fn close<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyDict>> {
        let timeout = close_timeout(timeout)?;
        let receipt = py.detach(|| self.inner.close_with_timeout(timeout));
        close_receipt(
            py,
            CoreStreamCloseReceipt {
                completed: receipt.completed,
                terminal: receipt.terminal,
                complete_items: receipt.complete_items,
            },
        )
    }
}

impl EncodedStream for PyEncodedStream {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        Python::attach(|py| {
            let item = self
                .iterator
                .call_method0(py, "step")
                .map_err(|error| python_error_to_c2(py, error))?;
            let (present, value): (bool, Py<PyAny>) = item
                .extract(py)
                .map_err(|error| python_error_to_c2(py, error))?;
            if !present {
                return Ok(None);
            }
            PyStreamItem::new(value.bind(py))
                .map(|item| Some(Box::new(item) as Box<dyn EncodedStreamItem>))
                .map_err(|error| stream_output_error(py, error))
        })
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Python::attach(|py| {
            self.iterator
                .call_method0(py, "close")
                .map(|_| ())
                .map_err(|error| python_error_to_c2(py, error))
        })
    }
}

struct FastDbView {
    buffer: Option<PyBuffer<u8>>,
    view: Py<PyMemoryView>,
    data: *const u8,
    len: usize,
}

// SAFETY: the FastDB exporter holds an immutable access pin until the buffer
// is released. Only the pointer and length are read while detached.
unsafe impl Send for FastDbView {}

impl FastDbView {
    fn new(view: &Bound<'_, PyMemoryView>) -> PyResult<Self> {
        let acquire = || -> PyResult<PyBuffer<u8>> {
            let exporter = view.getattr("obj")?;
            // Only FastDB's non-instantiable native exporter vouches for
            // immutable bytes. A read-only alias of bytearray does not.
            let fastdb = unsafe {
                let ty = ffi::Py_TYPE(exporter.as_ptr());
                ffi::PyType_GetFlags(ty) & ffi::Py_TPFLAGS_DISALLOW_INSTANTIATION != 0
                    && CStr::from_ptr((*ty).tp_name)
                        == c"fastdb4py.core._payload_ext._ReadOnlyBuffer"
            };
            let buffer = PyBuffer::<u8>::get(view.as_any())?;
            if !fastdb || !buffer.readonly() || !buffer.is_c_contiguous() {
                return Err(PyBufferError::new_err(
                    "expected a read-only contiguous FastDB view",
                ));
            }
            Ok(buffer)
        };
        let buffer = match acquire() {
            Ok(buffer) => buffer,
            Err(error) => {
                let _ = view.call_method0("release");
                return Err(error);
            }
        };
        Ok(Self {
            data: buffer.buf_ptr().cast(),
            len: buffer.len_bytes(),
            buffer: Some(buffer),
            view: view.clone().unbind(),
        })
    }
}

impl Drop for FastDbView {
    fn drop(&mut self) {
        Python::attach(|py| {
            drop(self.buffer.take());
            if let Err(error) = self.view.call_method0(py, "release") {
                error.write_unraisable(py, Some(self.view.bind(py).as_any()));
            }
        });
    }
}

enum PyStreamItem {
    Bytes(PyBackedBytes),
    FastDb(FastDbView),
    Plan {
        plan: Py<PyAny>,
        len: usize,
        materialized: Mutex<Option<Vec<u8>>>,
    },
}

impl PyStreamItem {
    fn new(value: &Bound<'_, PyAny>) -> PyResult<Self> {
        if let Ok(bytes) = value.cast_exact::<PyBytes>() {
            return Ok(Self::Bytes(bytes.clone().into()));
        }
        if let Ok(view) = value.cast_exact::<PyMemoryView>() {
            return FastDbView::new(view).map(Self::FastDb);
        }
        let len = deferred_plan_len(value)?
            .ok_or_else(|| PyValueError::new_err("invalid serialized stream item"))?;
        Ok(Self::Plan {
            plan: value.clone().unbind(),
            len,
            materialized: Mutex::new(None),
        })
    }
}

impl EncodedStreamItem for PyStreamItem {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::FastDb(view) => view.len,
            Self::Plan { len, .. } => *len,
        }
    }

    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        let end = offset + destination.len();
        match self {
            Self::Bytes(bytes) => destination.copy_from_slice(&bytes[offset..end]),
            Self::FastDb(view) => {
                // SAFETY: the live Py_buffer holds the immutable FastDB pin.
                let bytes = unsafe { std::slice::from_raw_parts(view.data, view.len) };
                destination.copy_from_slice(&bytes[offset..end]);
            }
            Self::Plan {
                plan,
                len,
                materialized,
            } => {
                Python::attach(|py| {
                    let plan = plan.bind(py);
                    let result = if offset == 0 && end == *len {
                        write_deferred_range(plan, destination)
                    } else {
                        let mut cached = materialized.lock();
                        if let Some(bytes) = cached.as_ref() {
                            destination.copy_from_slice(&bytes[offset..end]);
                            return Ok(());
                        }
                        materialize_deferred_plan(plan, *len).map(|bytes| {
                            destination.copy_from_slice(&bytes[offset..end]);
                            *cached = Some(bytes);
                        })
                    };
                    result.map_err(|error| stream_output_error(py, error))
                })?;
            }
        }
        Ok(())
    }
}

fn stream_output_error(py: Python<'_>, error: PyErr) -> C2Error {
    let projected = py
        .import("c_two.crm.stream")
        .and_then(|module| module.getattr("_resource_output_encode_error"))
        .and_then(|classify| classify.call1((error.value(py),)))
        .map(PyErr::from_value);
    python_error_to_c2(py, projected.unwrap_or(error))
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyCoreResponseStream>()?;
    module.add_class::<PyCoreLocalResponseStream>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_core::EncodedStreamService;
    use std::sync::{Arc, mpsc};

    #[test]
    fn immutable_stream_item_writes_ranges_without_attaching_to_python() {
        Python::initialize();
        let (observed, worker) = Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"\nclass Iterator:\n    def step(self):\n        return True, b'abcdefgh'\n",
                c"item_bytes_test.py",
                c"item_bytes_test",
            )
            .unwrap();
            let mut stream = PyEncodedStream {
                iterator: module
                    .getattr("Iterator")
                    .unwrap()
                    .call0()
                    .unwrap()
                    .unbind(),
            };
            let item = stream.step().unwrap().unwrap();
            assert_eq!(item.len(), 8);
            let (done, received) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut first = [0; 3];
                let mut last = [0; 2];
                item.write_range(0, &mut first).unwrap();
                item.write_range(6, &mut last).unwrap();
                item.write_range(8, &mut []).unwrap();
                let _ = done.send((first, last));
                item
            });
            // The caller keeps Python attached while a separate worker copies
            // the immutable snapshot. A GIL acquisition would block that copy.
            (received.recv_timeout(Duration::from_secs(5)), worker)
        });
        drop(worker.join().unwrap());
        assert_eq!(observed.unwrap(), (*b"abc", *b"gh"));
    }

    #[test]
    fn fastdb_stream_item_releases_its_access_pin() {
        Python::initialize();
        let (item, payload, view) = Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"
import json
from fastdb4py.payload import Builder, BuildPolicy, CompiledSpec
spec = {'schema': 'fastdb.payload.v1', 'profile': 'record.v1',
        'entries': [{'id': 'value', 'cardinality': 'one',
                     'type': {'kind': 'bytes', 'nullable': False}}], 'components': []}
with CompiledSpec.compile(json.dumps(spec).encode()) as compiled:
    with Builder.create(compiled) as builder:
        builder.entry_begin(0, 1).value_bytes(b'abc')
        with builder.freeze() as plan:
            payload = plan.execute(BuildPolicy.ALLOW_STAGING).payload
view = payload.memoryview()
",
                c"fastdb_stream_item.py",
                c"fastdb_stream_item",
            )
            .unwrap();
            let view = module.getattr("view").unwrap();
            (
                PyStreamItem::new(&view).unwrap(),
                module.getattr("payload").unwrap().unbind(),
                view.unbind(),
            )
        });
        let mut copied = vec![0; item.len()];
        item.write_range(0, &mut copied).unwrap();
        Python::attach(|py| {
            let bytes = view.call_method0(py, "tobytes").unwrap();
            assert_eq!(bytes.extract::<Vec<u8>>(py).unwrap(), copied);
        });
        drop(item);
        Python::attach(|py| {
            // Keep a Python reference to the memoryview so destruction alone
            // cannot release the pin.
            assert!(view.call_method0(py, "tobytes").is_err());
            payload.call_method0(py, "invalidate").unwrap();
        });
    }

    enum Job {
        Open(mpsc::Sender<Box<dyn EncodedStream>>),
        Step(Box<dyn EncodedStream>, mpsc::Sender<Box<dyn EncodedStream>>),
        Finish(Box<dyn EncodedStream>, mpsc::Sender<()>),
    }

    #[test]
    fn python_context_follows_one_stream_across_two_actual_workers() {
        Python::initialize();
        let (module, service) = Python::attach(|py| {
            // Share the public exception importer's initialization gate with
            // the other embedded tests before importing SDK submodules.
            let setup = core_error_to_py(
                C2Error::new(
                    c2_error::ErrorCode::ResourceFunctionExecuting,
                    "test initialization",
                )
                .into(),
            );
            let _ = setup.value(py);
            let module = PyModule::from_code(
                py,
                c"
import contextvars, decimal, pickle, threading
from types import SimpleNamespace
from c_two.crm.stream import _ResourceStream
value = contextvars.ContextVar('stream-worker-value', default='outside')
visits = []
def setup(label, precision):
    value.set(label)
    decimal.setcontext(decimal.Context(prec=precision))
def check(label, precision):
    assert value.get() == label
    assert decimal.getcontext().prec == precision
def factory():
    assert value.get() == 'a'
    factory_token = value.set('factory')
    visits.append(('factory', threading.get_ident()))
    def generate():
        token = value.set('inside')
        with decimal.localcontext() as context:
            context.prec = 37
            try:
                visits.append(('next-a', threading.get_ident()))
                yield 1
                assert value.get() == 'inside'
                assert decimal.getcontext().prec == 37
                visits.append(('next-b', threading.get_ident()))
                yield 2
            finally:
                assert value.get() == 'inside'
                assert decimal.getcontext().prec == 37
                value.reset(token)
                assert value.get() == 'factory'
                value.reset(factory_token)
                assert value.get() == 'a'
                visits.append(('finally', threading.get_ident()))
    return generate()
def dispatch(route, method, request):
    return _ResourceStream(factory, output=SimpleNamespace(serialize=pickle.dumps))
",
                c"stream_context_test.py",
                c"stream_context_test",
            )
            .unwrap();
            let service = Arc::new(crate::core_ffi::PyCoreService::new(
                "context".into(),
                module.getattr("dispatch").unwrap().unbind(),
            ));
            (module.unbind(), service)
        });
        let mut senders = Vec::new();
        let mut workers = Vec::new();
        for (label, precision) in [("a", 7), ("b", 13)] {
            let module = Python::attach(|py| module.clone_ref(py));
            let service = service.clone();
            let (tx, rx) = mpsc::channel();
            let rx = std::sync::Mutex::new(rx);
            senders.push(tx);
            workers.push(std::thread::spawn(move || {
                Python::attach(|py| {
                    module
                        .call_method1(py, "setup", (label, precision))
                        .unwrap();
                    while let Ok(job) = py.detach(|| rx.lock().unwrap().recv()) {
                        match job {
                            Job::Open(reply) => {
                                let stream = service.open_stream(0, Vec::new()).unwrap();
                                assert!(reply.send(stream).is_ok());
                            }
                            Job::Step(mut stream, reply) => {
                                let item = stream.step().unwrap().unwrap();
                                let mut bytes = vec![0; item.len()];
                                let split = bytes.len() / 2;
                                item.write_range(0, &mut bytes[..split]).unwrap();
                                item.write_range(split, &mut bytes[split..]).unwrap();
                                assert!(!bytes.is_empty());
                                assert!(reply.send(stream).is_ok());
                            }
                            Job::Finish(stream, reply) => {
                                stream.finish().unwrap();
                                assert!(reply.send(()).is_ok());
                            }
                        }
                        module
                            .call_method1(py, "check", (label, precision))
                            .unwrap();
                    }
                })
            }));
        }
        let (tx, rx) = mpsc::channel();
        assert!(senders[0].send(Job::Open(tx)).is_ok());
        let mut stream = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for worker in [0, 1] {
            let (tx, rx) = mpsc::channel();
            assert!(senders[worker].send(Job::Step(stream, tx)).is_ok());
            stream = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let (tx, rx) = mpsc::channel();
        assert!(senders[0].send(Job::Finish(stream, tx)).is_ok());
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(senders);
        for worker in workers {
            worker.join().unwrap();
        }
        Python::attach(|py| {
            let visits: Vec<(String, u64)> =
                module.getattr(py, "visits").unwrap().extract(py).unwrap();
            assert_eq!(
                visits.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
                ["factory", "next-a", "next-b", "finally"]
            );
            assert_eq!(visits[0].1, visits[1].1);
            assert_ne!(visits[1].1, visits[2].1);
            assert_eq!(visits[0].1, visits[3].1);
        });
    }
}
