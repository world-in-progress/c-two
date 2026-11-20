use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use c2_core::{
    Error, LifecycleError, LocalIoCause, RegisterFailureOutcome, RelayCleanupError,
    RouteCloseOutcome, TransportKind, TransportPhase,
};

pyo3::import_exception!(c_two.error, CoreError);

pub(crate) fn core_error_to_py(error: Error) -> PyErr {
    // The Core authority classifies the dispatch phase once; Python only
    // reads the projected `_transport_phase` slot.
    let phase = phase_str(error.transport_phase());
    match error {
        mismatch @ Error::ProtocolVersionMismatch { local, peer } => local_error(
            "protocol_version_mismatch",
            mismatch.to_string(),
            None,
            |value| {
                set_attr(value, "local_protocol_version", local);
                set_attr(value, "peer_protocol_version", peer);
                set_attr(value, "transport_kind", "ipc");
                set_attr(value, "_transport_phase", "pre_dispatch");
                set_attr(value, "fallback_eligible", false);
            },
        ),
        Error::Semantic(error) => {
            let exception = CoreError::new_err(("semantic", error.message.clone()));
            Python::attach(|py| {
                let value = exception.value(py);
                set_io_cause(value, None);
                set_semantic_error(value, &error, phase, true);
                set_attr(value, "_transport_phase", phase);
            });
            exception
        }
        Error::Contract(error) => local_error("contract", error.to_string(), None, |value| {
            set_attr(value, "_transport_phase", phase);
        }),
        Error::Admission(error) => local_error("admission", error.to_string(), None, |value| {
            set_attr(value, "_transport_phase", phase);
        }),
        Error::Transport(error) => {
            let kind = match error.kind() {
                TransportKind::Ipc => "ipc",
                TransportKind::Http => "http",
            };
            let fallback_eligible = error.is_fallback_eligible();
            local_error(
                "transport",
                error.to_string(),
                error.local_io_cause(),
                |value| {
                    set_attr(value, "transport_kind", kind);
                    set_attr(value, "_transport_phase", phase);
                    set_attr(value, "fallback_eligible", fallback_eligible);
                    if let Some(semantic) = error.semantic_error() {
                        set_semantic_error(value, &semantic, phase, false);
                    }
                },
            )
        }
        Error::Lifecycle(error) => lifecycle_error_to_py(error),
    }
}

fn phase_str(phase: Option<TransportPhase>) -> Option<&'static str> {
    phase.map(|phase| match phase {
        TransportPhase::PreDispatch => "pre_dispatch",
        TransportPhase::DispatchUncertain => "dispatch_uncertain",
    })
}

/// Project one canonical semantic error as a public `c_two.error` exception.
///
/// Used by native admission surfaces that refuse a call before any CoreError
/// family exists; the projection carries the caller-supplied dispatch phase.
pub(crate) fn semantic_error_to_py(py: Python<'_>, error: c2_error::C2Error, phase: &str) -> PyErr {
    let factory = py
        .import("c_two.error")
        .and_then(|module| module.getattr("CCError"))
        .and_then(|class| class.getattr("_from_parts"));
    let factory = match factory {
        Ok(factory) => factory,
        Err(err) => return err,
    };
    let details = PyDict::new(py);
    for (key, item) in &error.details {
        let _ = details.set_item(key, item);
    }
    let kwargs = PyDict::new(py);
    if let Err(err) = kwargs.set_item("transport_phase", phase) {
        return err;
    }
    match factory.call(
        (
            u16::from(error.code),
            error.message.as_str(),
            details.as_any(),
        ),
        Some(&kwargs),
    ) {
        Ok(value) => PyErr::from_value(value),
        Err(err) => err,
    }
}

/// Project the immutable semantic snapshot of one CoreError value: the code,
/// its registry name, the inner semantic message and the diagnostic details.
fn set_semantic_error(
    value: &Bound<'_, PyAny>,
    error: &c2_error::C2Error,
    phase: Option<&str>,
    promote_control_details: bool,
) {
    if let Ok(entries) = PyTuple::new(
        value.py(),
        error.details.iter().map(|(key, item)| (key.as_str(), item.as_str())),
    ) {
        set_attr(
            value,
            "_semantic_snapshot",
            (u16::from(error.code), error.message.as_str(), entries, phase),
        );
    }
    set_attr(value, "code", u16::from(error.code));
    set_attr(value, "name", error.code.name());
    set_attr(value, "semantic_message", error.message.as_str());
    let details = PyDict::new(value.py());
    for (key, item) in &error.details {
        if promote_control_details && phase.is_some() {
            if key == "fallback_eligible" {
                set_attr(value, "fallback_eligible", item == "true");
            }
            if key == "route_withdrawal" {
                set_attr(value, "route_withdrawal", item == "true");
            }
        }
        let _ = details.set_item(key, item);
    }
    set_attr(value, "details", details);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_failure_projection_is_dispatch_uncertain() {
        Python::initialize();
        Python::attach(|py| {
            for source in [
                c2_ipc::IpcError::Protocol("malformed response".into()),
                c2_ipc::IpcError::Shm("response admission failed".into()),
            ] {
                let error = core_error_to_py(c2_core::normalize_ipc_error(
                    c2_ipc::IpcError::ResponseFailure(Box::new(source)),
                    TransportPhase::PreDispatch,
                ));
                let value = error.value(py);
                assert_eq!(
                    value
                        .getattr("category")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "transport"
                );
                assert_eq!(
                    value
                        .getattr("transport_phase")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "dispatch_uncertain"
                );
                assert!(
                    !value
                        .getattr("fallback_eligible")
                        .unwrap()
                        .extract::<bool>()
                        .unwrap()
                );
            }
        });
    }

    #[test]
    fn transport_error_projects_semantic_snapshot_without_wire_bytes() {
        Python::initialize();
        Python::attach(|py| {
            let semantic = c2_error::C2Error::new(
                c2_error::ErrorCode::ResourceRemoved,
                "grid withdrawn by owner",
            )
            .with_details(std::collections::BTreeMap::from([(
                "route".into(),
                "grid".into(),
            )]));
            let rejected = c2_ipc::IpcError::LocalCallRejected(semantic.clone());
            let error = core_error_to_py(c2_core::normalize_ipc_error(
                c2_ipc::IpcError::ResponseFailure(Box::new(rejected)),
                TransportPhase::PreDispatch,
            ));
            let value = error.value(py);
            assert_eq!(
                value.getattr("code").unwrap().extract::<u16>().unwrap(),
                u16::from(c2_error::ErrorCode::ResourceRemoved)
            );
            assert_eq!(
                value.getattr("name").unwrap().extract::<String>().unwrap(),
                "ResourceRemoved"
            );
            assert_eq!(
                value
                    .getattr("semantic_message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "grid withdrawn by owner"
            );
            assert_eq!(
                value
                    .getattr("transport_phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "dispatch_uncertain"
            );
            let module = py.import("c_two.error").unwrap();
            let projected = value.getattr("semantic").unwrap();
            assert!(projected
                .is_instance(&module.getattr("ResourceRemoved").unwrap())
                .unwrap());
            assert_eq!(
                projected
                    .getattr("message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "grid withdrawn by owner"
            );
            assert_eq!(
                projected
                    .getattr("transport_phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "dispatch_uncertain"
            );
        });
    }

    #[test]
    fn protocol_version_error_uses_public_class_and_typed_fields() {
        Python::initialize();
        Python::attach(|py| {
            let error = core_error_to_py(Error::ProtocolVersionMismatch {
                local: 15,
                peer: 14,
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
                ("local_protocol_version", 15_u32),
                ("peer_protocol_version", 14),
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
                "semantic_message",
                "io_operation",
                "io_kind",
                "raw_os_error",
            ] {
                assert!(value.getattr(name).unwrap().is_none(), "{name}");
            }
        });
    }

    #[test]
    fn semantic_error_projection_reads_core_phase_from_semantic_details() {
        Python::initialize();
        Python::attach(|py| {
            let mut semantic = c2_error::C2Error::new(
                c2_error::ErrorCode::CallDeadlineExceeded,
                "deadline elapsed before a definitive outcome",
            );
            semantic
                .details
                .insert("transport_phase".into(), "dispatch_uncertain".into());
            let error = core_error_to_py(Error::Semantic(semantic));
            let value = error.value(py);
            assert_eq!(
                value
                    .getattr("transport_phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "dispatch_uncertain"
            );
            let projected = value.getattr("semantic").unwrap();
            let deadline_class = py
                .import("c_two.error")
                .unwrap()
                .getattr("CallDeadlineExceeded")
                .unwrap();
            assert!(projected.is_instance(&deadline_class).unwrap());
            assert_eq!(
                projected
                    .getattr("transport_phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "dispatch_uncertain"
            );
        });
    }

    #[test]
    fn registration_failure_preserves_semantic_error_and_original_receipt() {
        Python::initialize();
        Python::attach(|py| {
            for code in [
                c2_error::ErrorCode::ResourceAlreadyRegistered,
                c2_error::ErrorCode::ContractMismatch,
                c2_error::ErrorCode::ResourceUnavailable,
            ] {
                let semantic = c2_error::C2Error::new(code, "canonical relay refusal")
                    .with_details(std::collections::BTreeMap::from([
                        ("name".into(), "grid".into()),
                        ("registrations".into(), "[]".into()),
                    ]));
                let status = if code == c2_error::ErrorCode::ResourceUnavailable {
                    503
                } else {
                    409
                };
                let original = serde_json::to_string(&semantic.envelope()).unwrap();
                let error = core_error_to_py(Error::Lifecycle(LifecycleError::RegisterFailure(
                    Box::new(RegisterFailureOutcome {
                        route_name: "grid".into(),
                        failure_source: "relay_prepare".into(),
                        error_message: original.clone(),
                        status_code: Some(status),
                        semantic_error: Some(semantic.clone()),
                        rollback: Some(RouteCloseOutcome {
                            route_name: "grid".into(),
                            local_removed: false,
                            active_drained: true,
                            closed_reason: "registration_rollback".into(),
                            close_error: None,
                        }),
                        relay_cleanup_error: None,
                    }),
                )));
                let value = error.value(py);
                assert_eq!(
                    value
                        .getattr("category")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "lifecycle"
                );
                assert_eq!(
                    value
                        .getattr("status_code")
                        .unwrap()
                        .extract::<u16>()
                        .unwrap(),
                    status
                );
                assert_eq!(
                    value.getattr("code").unwrap().extract::<u16>().unwrap(),
                    u16::from(code)
                );
                assert_eq!(
                    value.getattr("name").unwrap().extract::<String>().unwrap(),
                    code.name()
                );
                assert_eq!(
                    value
                        .getattr("details")
                        .unwrap()
                        .extract::<std::collections::BTreeMap<String, String>>()
                        .unwrap(),
                    semantic.details
                );
                assert_eq!(
                    value
                        .getattr("semantic_message")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "canonical relay refusal"
                );
                assert_eq!(
                    value
                        .getattr("message")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    format!("registration failed at relay_prepare: {original}")
                );
                let failure = value.getattr("registration_failure").unwrap();
                assert_eq!(
                    failure
                        .get_item("error_message")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    original
                );
                let rollback = failure.get_item("rollback").unwrap();
                assert!(
                    !rollback
                        .get_item("local_removed")
                        .unwrap()
                        .extract::<bool>()
                        .unwrap()
                );
                assert!(
                    rollback
                        .get_item("active_drained")
                        .unwrap()
                        .extract::<bool>()
                        .unwrap()
                );
                assert_eq!(
                    value
                        .getattr("relay_duplicate")
                        .unwrap()
                        .is_truthy()
                        .unwrap(),
                    code == c2_error::ErrorCode::ResourceAlreadyRegistered
                );
            }
        });
    }

    #[test]
    fn semantic_error_to_py_builds_typed_exception_with_phase() {
        Python::initialize();
        Python::attach(|py| {
            let error = semantic_error_to_py(
                py,
                c2_error::C2Error::new(c2_error::ErrorCode::ResourceClosed, "route closed")
                    .with_details(std::collections::BTreeMap::from([(
                        "route".into(),
                        "grid".into(),
                    )])),
                "pre_dispatch",
            );
            let value = error.value(py);
            let module = py.import("c_two.error").unwrap();
            assert!(value
                .is_instance(&module.getattr("ResourceClosed").unwrap())
                .unwrap());
            assert_eq!(
                value
                    .getattr("message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "route closed"
            );
            assert_eq!(
                value
                    .getattr("transport_phase")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "pre_dispatch"
            );
            let details = value.getattr("details").unwrap();
            assert_eq!(
                details
                    .get_item("route")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "grid"
            );
        });
    }
}

pub(crate) fn lifecycle_error_to_py(error: LifecycleError) -> PyErr {
    let wrapped = Error::Lifecycle(error);
    let phase = phase_str(wrapped.transport_phase());
    let Error::Lifecycle(error) = wrapped else {
        unreachable!("wrapped value stays a lifecycle error");
    };
    let message = error.to_string();
    local_error(
        "lifecycle",
        message,
        error.local_io_cause(),
        |value| {
            set_attr(value, "_transport_phase", phase);
            match &error {
            LifecycleError::RuntimeRetired => {
                set_attr(value, "lifecycle_kind", "runtime_retired");
            }
            LifecycleError::ShutdownWaitTimeout { stage } => {
                set_attr(value, "lifecycle_kind", "shutdown_wait_timeout");
                let details = PyDict::new(value.py());
                let _ = details.set_item("stage", stage);
                set_attr(value, "details", details);
            }
            LifecycleError::InvalidServerId(_) => {
                set_attr(value, "lifecycle_kind", "invalid_server_id");
            }
            LifecycleError::ConfigFrozen => {
                set_attr(value, "lifecycle_kind", "config_frozen");
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
            LifecycleError::MissingRelayAddress => {
                set_attr(value, "lifecycle_kind", "missing_relay_address");
            }
            LifecycleError::RegisterFailure(failure) => {
                set_attr(value, "lifecycle_kind", "register_failure");
                set_attr(value, "failure_source", &failure.failure_source);
                set_attr(value, "route_name", &failure.route_name);
                if let Some(semantic_error) = &failure.semantic_error {
                    set_semantic_error(value, semantic_error, phase, false);
                }
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
                        set_attr(
                            value,
                            "relay_duplicate",
                            failure.semantic_error.as_ref().is_some_and(|error| {
                                error.code == c2_error::ErrorCode::ResourceAlreadyRegistered
                            }),
                        );
                    }
                }
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
        "endpoint_unverified_reason",
        cause.and_then(|cause| match cause {
            LocalIoCause::Bind(bind) => bind.unverified_reason.map(|reason| reason.as_str()),
            LocalIoCause::Connect(_) => None,
        }),
    );
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
