use std::collections::BTreeMap;
use std::sync::Arc;

use c2_core::{
    AccessLevel, AdmissionContext, AdmissionDecision, AdmissionHook, AdmissionOperation,
    AdmissionTransport, CallContext, CallMetadata,
};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyTuple};

/// Release Python-owned Host values on the thread that retires them, including
/// when Core releases the last Arc on a thread outside the interpreter.
struct PythonOwner(Option<Py<PyAny>>);

impl PythonOwner {
    fn new(value: Py<PyAny>) -> Self {
        Self(Some(value))
    }

    fn value(&self) -> &Py<PyAny> {
        self.0.as_ref().expect("live Python owner")
    }
}

impl Drop for PythonOwner {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            Python::try_attach(|py| value.drop_ref(py));
        }
    }
}

pub(crate) struct PyAdmissionHook {
    callable: PythonOwner,
}

impl PyAdmissionHook {
    pub(crate) fn new(callable: Py<PyAny>) -> Self {
        Self {
            callable: PythonOwner::new(callable),
        }
    }

    pub(crate) fn is_same_callable(&self, other: &Self) -> bool {
        self.callable.value().as_ptr() == other.callable.value().as_ptr()
    }

    fn invoke(
        &self,
        py: Python<'_>,
        context: &AdmissionContext<'_>,
    ) -> PyResult<AdmissionDecision> {
        let module = py.import("c_two._call_context")?;
        let access = context.access().map(|access| match access {
            AccessLevel::Read => "read",
            AccessLevel::Write => "write",
        });
        let transport = match context.transport() {
            AdmissionTransport::ThreadLocal => "thread_local",
            AdmissionTransport::Ipc => "ipc",
        };
        let context = module.getattr("AdmissionContext")?.call1((
            context.route_name(),
            context.crm_ns(),
            context.crm_name(),
            context.crm_ver(),
            context.target(),
            access,
            operation_name(context.operation()),
            transport,
            context.call_id(),
            metadata_dict(py, context.metadata())?,
        ))?;
        let decision = self.callable.value().bind(py).call1((context,))?;
        if decision.is_none() {
            return Ok(AdmissionDecision::allow());
        }
        if !decision
            .get_type()
            .is(&module.getattr("AdmissionDecision")?)
        {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "admission hook must return None or AdmissionDecision",
            ));
        }
        if decision
            .getattr("_allowed")?
            .cast_exact::<PyBool>()?
            .is_true()
        {
            let extension = decision.getattr("_extension")?;
            return Ok(if extension.is_none() {
                AdmissionDecision::allow()
            } else {
                AdmissionDecision::allow_with_extension(Arc::new(PythonOwner::new(
                    extension.unbind(),
                )))
            });
        }
        let message = decision.getattr("_message")?.extract::<String>()?;
        let details = decision.getattr("_details")?;
        let details = if details.is_none() {
            BTreeMap::new()
        } else {
            details
                .call_method0("items")?
                .try_iter()?
                .map(|item| item?.extract::<(String, String)>())
                .collect::<PyResult<_>>()?
        };
        Ok(AdmissionDecision::deny_with_details(message, details))
    }
}

impl AdmissionHook for PyAdmissionHook {
    fn admit(&self, context: &AdmissionContext<'_>) -> AdmissionDecision {
        Python::attach(|py| match self.invoke(py, context) {
            Ok(decision) => decision,
            Err(_) => AdmissionDecision::deny_with_details(
                "admission hook failed",
                BTreeMap::from([("reason".into(), "hook_error".into())]),
            ),
        })
    }
}

fn operation_name(operation: AdmissionOperation) -> &'static str {
    match operation {
        AdmissionOperation::Unary => "unary",
        AdmissionOperation::ServerStream => "server_stream",
        AdmissionOperation::Subscribe => "subscribe",
        AdmissionOperation::Recover => "recover",
        AdmissionOperation::QueryOutcome => "query_outcome",
    }
}

pub(crate) fn metadata_dict<'py>(
    py: Python<'py>,
    metadata: &CallMetadata,
) -> PyResult<Bound<'py, PyDict>> {
    let entries = PyDict::new(py);
    for (key, value) in metadata.iter() {
        entries.set_item(key, value)?;
    }
    Ok(entries)
}

pub(crate) fn project_call_context<'py>(
    py: Python<'py>,
    context: &CallContext,
) -> PyResult<Bound<'py, PyAny>> {
    let extension = context.extension::<PythonOwner>();
    let extension = extension.as_ref().map(|value| value.value().bind(py));
    py.import("c_two._call_context")?
        .getattr("CallContext")?
        .call1((
            context.route_name(),
            context.target(),
            operation_name(context.operation()),
            context.call_id(),
            metadata_dict(py, context.metadata())?,
            extension,
        ))
}

pub(crate) fn dispatch_in_call_context<'py>(
    py: Python<'py>,
    dispatcher: &Py<PyAny>,
    args: Bound<'py, PyTuple>,
) -> PyResult<Bound<'py, PyAny>> {
    let Some(context) = c2_core::current_call_context() else {
        return dispatcher.bind(py).call1(args);
    };
    let context = project_call_context(py, &context)?;
    py.import("c_two._call_context")?
        .getattr("_run_in_call_context")?
        .call1((context, dispatcher, args))
}
