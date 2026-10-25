use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use c2_core::{
    Error, LifecycleError, LocalIoCause, RegisterFailureOutcome, RelayCleanupError,
    RouteCloseOutcome, TransportKind, TransportPhase,
};

pyo3::import_exception!(c_two.error, CoreError);

pub(crate) fn core_error_to_py(error: Error) -> PyErr {
    match error {
        mismatch @ Error::ProtocolVersionMismatch { local, peer } => local_error(
            "protocol_version_mismatch",
            mismatch.to_string(),
            None,
            |value| {
                set_attr(value, "local_protocol_version", local);
                set_attr(value, "peer_protocol_version", peer);
                set_attr(value, "transport_kind", "ipc");
                set_attr(value, "transport_phase", "pre_dispatch");
                set_attr(value, "fallback_eligible", false);
            },
        ),
        Error::Semantic(error) => {
            let message = error.message.clone();
            let code = u16::from(error.code);
            let name = error.code.name();
            let details = error.details.clone();
            let error_bytes = error.to_wire_bytes();
            let exception = CoreError::new_err(("semantic", message));
            Python::attach(|py| {
                let value = exception.value(py);
                set_attr(value, "code", code);
                set_attr(value, "name", name);
                set_io_cause(value, None);
                let detail_dict = PyDict::new(py);
                for (key, item) in details {
                    if key == "transport_phase" {
                        set_attr(value, "transport_phase", &item);
                    }
                    if key == "fallback_eligible" {
                        set_attr(value, "fallback_eligible", item == "true");
                    }
                    if key == "route_withdrawal" {
                        set_attr(value, "route_withdrawal", item == "true");
                    }
                    let _ = detail_dict.set_item(key, item);
                }
                set_attr(value, "details", detail_dict);
                set_attr(value, "error_bytes", PyBytes::new(py, &error_bytes));
            });
            exception
        }
        Error::Contract(error) => local_error("contract", error.to_string(), None, |_| {}),
        Error::Admission(error) => local_error("admission", error.to_string(), None, |_| {}),
        Error::Transport(error) => {
            let kind = match error.kind() {
                TransportKind::Ipc => "ipc",
                TransportKind::Http => "http",
            };
            let phase = match error.phase() {
                TransportPhase::PreDispatch => "pre_dispatch",
                TransportPhase::DispatchUncertain => "dispatch_uncertain",
            };
            let fallback_eligible = error.is_fallback_eligible();
            local_error(
                "transport",
                error.to_string(),
                error.local_io_cause(),
                |value| {
                    set_attr(value, "transport_kind", kind);
                    set_attr(value, "transport_phase", phase);
                    set_attr(value, "fallback_eligible", fallback_eligible);
                    if let Some(semantic) = error.semantic_error() {
                        set_attr(value, "code", u16::from(semantic.code));
                        set_attr(value, "name", semantic.code.name());
                        let details = PyDict::new(value.py());
                        for (key, item) in &semantic.details {
                            let _ = details.set_item(key, item);
                        }
                        set_attr(value, "details", details);
                        set_attr(
                            value,
                            "error_bytes",
                            PyBytes::new(value.py(), &semantic.to_wire_bytes()),
                        );
                    }
                },
            )
        }
        Error::Lifecycle(error) => lifecycle_error_to_py(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_error_uses_public_class_and_typed_fields() {
        Python::initialize();
        Python::attach(|py| {
            let error = core_error_to_py(Error::ProtocolVersionMismatch {
                local: 14,
                peer: 15,
            });
            let value = error.value(py);
            let class = py
                .import("c_two.error")
                .unwrap()
                .getattr("CoreError")
                .unwrap();
            assert!(value.is_instance(&class).unwrap());
            for (name, expected) in [
                ("category", "protocol_version_mismatch"),
                ("transport_kind", "ipc"),
                ("transport_phase", "pre_dispatch"),
            ] {
                assert_eq!(
                    value.getattr(name).unwrap().extract::<String>().unwrap(),
                    expected
                );
            }
            for (name, expected) in [
                ("local_protocol_version", 14_u32),
                ("peer_protocol_version", 15),
            ] {
                assert_eq!(
                    value.getattr(name).unwrap().extract::<u32>().unwrap(),
                    expected
                );
                assert!(
                    class
                        .getattr("__annotations__")
                        .unwrap()
                        .contains(name)
                        .unwrap()
                );
            }
            assert!(
                !value
                    .getattr("fallback_eligible")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            for name in [
                "code",
                "name",
                "error_bytes",
                "io_operation",
                "io_kind",
                "raw_os_error",
            ] {
                assert!(value.getattr(name).unwrap().is_none(), "{name}");
            }
        });
    }
}

pub(crate) fn lifecycle_error_to_py(error: LifecycleError) -> PyErr {
    let message = error.to_string();
    local_error(
        "lifecycle",
        message,
        error.local_io_cause(),
        |value| match &error {
            LifecycleError::InvalidServerId(_) => {
                set_attr(value, "lifecycle_kind", "invalid_server_id");
            }
            LifecycleError::ConfigFrozen => {
                set_attr(value, "lifecycle_kind", "config_frozen");
            }
            LifecycleError::ClientConfigFrozen => {
                set_attr(value, "lifecycle_kind", "client_config_frozen");
            }
            LifecycleError::DuplicateRoute(route) => {
                set_attr(value, "lifecycle_kind", "duplicate_route");
                set_attr(value, "route_name", route);
                set_attr(value, "status_code", 409_u16);
            }
            LifecycleError::MissingRoute(route) => {
                set_attr(value, "lifecycle_kind", "missing_route");
                set_attr(value, "route_name", route);
            }
            LifecycleError::RelayDuplicateRoute(route) => {
                set_attr(value, "lifecycle_kind", "relay_duplicate_route");
                set_attr(value, "route_name", route);
                set_attr(value, "status_code", 409_u16);
                set_attr(value, "relay_duplicate", true);
            }
            LifecycleError::MissingRelayAddress => {
                set_attr(value, "lifecycle_kind", "missing_relay_address");
            }
            LifecycleError::RelayHttp {
                status_code,
                message,
            } => {
                set_attr(value, "lifecycle_kind", "relay_http");
                set_attr(value, "status_code", *status_code);
                set_attr(value, "body", message);
            }
            LifecycleError::RegisterFailure(failure) => {
                set_attr(value, "lifecycle_kind", "register_failure");
                set_attr(value, "failure_source", &failure.failure_source);
                set_attr(value, "route_name", &failure.route_name);
                if let Ok(details) = register_failure_to_dict(value.py(), failure) {
                    set_attr(value, "registration_failure", details.clone());
                    match details.get_item("rollback") {
                        Ok(Some(rollback)) => set_attr(value, "rollback", rollback),
                        _ => set_attr(value, "rollback", value.py().None()),
                    }
                    match details.get_item("relay_cleanup_error") {
                        Ok(Some(error)) => set_attr(value, "relay_cleanup_error", error),
                        _ => set_attr(value, "relay_cleanup_error", value.py().None()),
                    }
                }
                if let Some(status_code) = failure.status_code {
                    set_attr(value, "status_code", status_code);
                    if status_code == 409 {
                        set_attr(value, "relay_duplicate", true);
                    }
                }
            }
            LifecycleError::HeldResponseCopy { .. } => {
                set_attr(value, "lifecycle_kind", "held_response_copy");
            }
            LifecycleError::HeldResponseRelease { .. } => {
                set_attr(value, "lifecycle_kind", "held_response_release");
            }
            LifecycleError::ResponseCopy(_) => {
                set_attr(value, "lifecycle_kind", "response_copy");
            }
            LifecycleError::Configuration(_) => {
                set_attr(value, "lifecycle_kind", "configuration");
            }
            LifecycleError::Server(_) => {
                set_attr(value, "lifecycle_kind", "server");
            }
            LifecycleError::StartupTimeout(timeout) => {
                set_attr(value, "lifecycle_kind", "startup_timeout");
                set_attr(value, "startup_stage", timeout.stage.as_str());
                set_attr(value, "timeout_seconds", timeout.timeout.as_secs_f64());
            }
            LifecycleError::Relay(_) => {
                set_attr(value, "lifecycle_kind", "relay");
            }
        },
    )
}

fn local_error<F>(
    category: &'static str,
    message: String,
    cause: Option<LocalIoCause>,
    configure: F,
) -> PyErr
where
    F: FnOnce(&Bound<'_, PyAny>),
{
    let exception = CoreError::new_err((category, message));
    Python::attach(|py| {
        let value = exception.value(py);
        set_io_cause(value, cause);
        configure(value);
    });
    exception
}

fn set_io_cause(value: &Bound<'_, PyAny>, cause: Option<LocalIoCause>) {
    set_attr(value, "io_operation", cause.map(LocalIoCause::operation));
    set_attr(
        value,
        "io_kind",
        cause.map(|cause| cause.error().category()),
    );
    set_attr(
        value,
        "raw_os_error",
        cause.and_then(|cause| cause.error().raw_os_error),
    );
}

fn register_failure_to_dict<'py>(
    py: Python<'py>,
    outcome: &RegisterFailureOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", &outcome.route_name)?;
    dict.set_item("failure_source", &outcome.failure_source)?;
    dict.set_item("error_message", &outcome.error_message)?;
    dict.set_item("status_code", outcome.status_code)?;
    match &outcome.rollback {
        Some(rollback) => dict.set_item("rollback", route_close_outcome_to_dict(py, rollback)?)?,
        None => dict.set_item("rollback", py.None())?,
    }
    match &outcome.relay_cleanup_error {
        Some(error) => dict.set_item(
            "relay_cleanup_error",
            relay_cleanup_error_to_dict(py, error)?,
        )?,
        None => dict.set_item("relay_cleanup_error", py.None())?,
    }
    Ok(dict)
}

pub(crate) fn relay_cleanup_error_to_dict<'py>(
    py: Python<'py>,
    error: &RelayCleanupError,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", &error.route_name)?;
    dict.set_item("status_code", error.status_code)?;
    dict.set_item("message", &error.message)?;
    Ok(dict)
}

pub(crate) fn route_close_outcome_to_dict<'py>(
    py: Python<'py>,
    outcome: &RouteCloseOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("route_name", &outcome.route_name)?;
    dict.set_item("local_removed", outcome.local_removed)?;
    dict.set_item("active_drained", outcome.active_drained)?;
    dict.set_item("closed_reason", &outcome.closed_reason)?;
    dict.set_item("close_error", &outcome.close_error)?;
    Ok(dict)
}

fn set_attr<'py, V>(value: &Bound<'py, PyAny>, name: &str, item: V)
where
    V: IntoPyObject<'py>,
{
    let _ = value.setattr(name, item);
}
