use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use c2_core::{
    AdapterFailure, AdapterFailurePhase, CallMetadata, CallOptions, CallTimeout, Error,
    ExternalCause, LifecycleError, TransportPhase, external_cause_details,
    normalize_adapter_failure,
};
use c2_error::{C2Error, ErrorCode};
use jni::{
    JNIEnv,
    errors::Error as JniError,
    objects::{GlobalRef, JClass, JObject, JObjectArray, JString, JThrowable, JValue},
    sys::jlong,
};
use parking_lot::Mutex;

type Handle = Arc<dyn Any + Send + Sync>;
static HANDLES: Mutex<Option<HashMap<jlong, Handle>>> = Mutex::new(None);
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static LOADER: OnceLock<GlobalRef> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum Failure {
    Core(Error),
    Jni(JniError),
    Argument(String),
}

impl Failure {
    pub(crate) fn argument(message: impl Into<String>) -> Self {
        Self::Argument(message.into())
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Core(error)
    }
}

impl From<LifecycleError> for Failure {
    fn from(error: LifecycleError) -> Self {
        Error::from(error).into()
    }
}

impl From<c2_contract::ContractError> for Failure {
    fn from(error: c2_contract::ContractError) -> Self {
        Error::from(error).into()
    }
}

impl From<C2Error> for Failure {
    fn from(error: C2Error) -> Self {
        Error::from(error).into()
    }
}

impl From<JniError> for Failure {
    fn from(error: JniError) -> Self {
        Self::Jni(error)
    }
}

pub(crate) fn put<T: Any + Send + Sync>(value: T) -> jlong {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    HANDLES
        .lock()
        .get_or_insert_default()
        .insert(id, Arc::new(value));
    id
}

pub(crate) fn get<T: Any + Send + Sync>(id: jlong) -> Result<Arc<T>, Failure> {
    let value = HANDLES
        .lock()
        .as_ref()
        .and_then(|handles| handles.get(&id).cloned());
    value
        .and_then(|value| value.downcast().ok())
        .ok_or_else(|| {
            C2Error::new(ErrorCode::ResourceClosed, "native handle has been released").into()
        })
}

pub(crate) fn release(id: jlong) {
    let value = HANDLES
        .lock()
        .as_mut()
        .and_then(|handles| handles.remove(&id));
    drop(value);
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_internal_NativeHandle_drop(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    id: jlong,
) {
    release(id);
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_internal_NativeLibraryLoader_initialize(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    loader: JObject<'_>,
) {
    run(&mut env, (), |env| {
        let loader = env.new_global_ref(loader)?;
        let _ = LOADER.set(loader);
        Ok(())
    });
}

/// Resolve SDK classes using the loader that owns this native library. Daemon
/// threads have no Java caller frame to supply the application loader.
pub(crate) fn class<'local>(
    env: &mut JNIEnv<'local>,
    path: &str,
) -> Result<JClass<'local>, Failure> {
    let Some(loader) = LOADER.get() else {
        return Ok(env.find_class(path)?);
    };
    let name = env.new_string(path.replace('/', "."))?;
    let class = env
        .call_method(
            loader.as_obj(),
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(name.as_ref())],
        )?
        .l()?;
    env.delete_local_ref(name)?;
    Ok(JClass::from(class))
}

pub(crate) fn string(env: &mut JNIEnv<'_>, value: &JString<'_>) -> Result<String, Failure> {
    if value.is_null() {
        return Err(Failure::argument("required string is null"));
    }
    Ok(env.get_string(value)?.into())
}

pub(crate) fn optional_string(
    env: &mut JNIEnv<'_>,
    value: &JString<'_>,
) -> Result<Option<String>, Failure> {
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(string(env, value)?))
    }
}

pub(crate) fn java_string<'local>(
    env: &mut JNIEnv<'local>,
    value: Option<&str>,
) -> Result<JString<'local>, Failure> {
    match value {
        Some(value) => Ok(env.new_string(value)?),
        None => Ok(JString::default()),
    }
}

pub(crate) fn pairs<'local, 'a>(
    env: &mut JNIEnv<'local>,
    entries: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<JObjectArray<'local>, Failure> {
    let entries: Vec<_> = entries.collect();
    let len = entries
        .len()
        .checked_mul(2)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| Failure::argument("too many fields"))?;
    let array = env.new_object_array(len, "java/lang/String", JObject::null())?;
    for (index, (key, value)) in entries.into_iter().enumerate() {
        let key = env.new_string(key)?;
        let value = env.new_string(value)?;
        env.set_object_array_element(&array, (2 * index) as i32, &key)?;
        env.set_object_array_element(&array, (2 * index + 1) as i32, &value)?;
        env.delete_local_ref(key)?;
        env.delete_local_ref(value)?;
    }
    Ok(array)
}

fn strings(env: &mut JNIEnv<'_>, array: JObjectArray<'_>) -> Result<Vec<String>, Failure> {
    let mut result = Vec::new();
    for index in 0..env.get_array_length(&array)? {
        let value = JString::from(env.get_object_array_element(&array, index)?);
        result.push(optional_string(env, &value)?.unwrap_or_default());
        env.delete_local_ref(value)?;
    }
    Ok(result)
}

pub(crate) fn read_pairs(
    env: &mut JNIEnv<'_>,
    array: JObjectArray<'_>,
) -> Result<BTreeMap<String, String>, Failure> {
    let values = strings(env, array)?;
    if values.len() % 2 != 0 {
        return Err(Failure::argument(
            "expected alternating field names and values",
        ));
    }
    let mut entries = BTreeMap::new();
    let mut values = values.into_iter();
    while let Some(key) = values.next() {
        let value = values.next().expect("checked pair count");
        if entries.insert(key, value).is_some() {
            return Err(Failure::argument("duplicate field"));
        }
    }
    Ok(entries)
}

pub(crate) fn call_options(
    env: &mut JNIEnv<'_>,
    options: &JObject<'_>,
) -> Result<CallOptions, Failure> {
    let kind = env.call_method(options, "timeoutKind", "()I", &[])?.i()?;
    let timeout = match kind {
        0 => CallTimeout::Inherit,
        1 => CallTimeout::Unlimited,
        2 => {
            let duration = env
                .call_method(options, "timeout", "()Ljava/time/Duration;", &[])?
                .l()?;
            let seconds = env.call_method(&duration, "getSeconds", "()J", &[])?.j()?;
            let nanos = env.call_method(&duration, "getNano", "()I", &[])?.i()?;
            let seconds =
                u64::try_from(seconds).map_err(|_| Failure::argument("negative timeout"))?;
            CallTimeout::After(Duration::new(seconds, nanos as u32))
        }
        _ => return Err(Failure::argument("unknown timeout policy")),
    };
    let metadata = env
        .call_method(options, "metadata", "()Lio/ctwo/CallMetadata;", &[])?
        .l()?;
    let array = env
        .call_method(metadata, "nativeEntries", "()[Ljava/lang/String;", &[])?
        .l()?;
    let metadata = CallMetadata::empty()
        .extend(read_pairs(env, JObjectArray::from(array))?)
        .map_err(|error| Failure::argument(error.to_string()))?;
    Ok(CallOptions::with_timeout(timeout).with_metadata(metadata))
}

pub(crate) fn error_object<'local>(
    env: &mut JNIEnv<'local>,
    error: &Error,
) -> Result<JObject<'local>, Failure> {
    let mut details = BTreeMap::new();
    let (category, semantic, fallback, kind) = match error {
        Error::Semantic(error) => ("SEMANTIC", Some(error), error.code, None),
        Error::Contract(_) | Error::Admission(_) => {
            ("CONTRACT", None, ErrorCode::ContractMismatch, None)
        }
        Error::Transport(error) => {
            details.insert(
                "transport".into(),
                format!("{:?}", error.kind()).to_ascii_lowercase(),
            );
            (
                "TRANSPORT",
                error.semantic_error(),
                ErrorCode::ClientCallingResource,
                None,
            )
        }
        Error::ProtocolVersionMismatch { local, peer } => {
            details.insert("local_version".into(), local.to_string());
            details.insert("peer_version".into(), peer.to_string());
            ("TRANSPORT", None, ErrorCode::ProtocolViolation, None)
        }
        Error::Lifecycle(error) => {
            let (kind, semantic, code) = match error {
                LifecycleError::RuntimeRetired => {
                    ("runtime_retired", None, ErrorCode::ResourceClosed)
                }
                LifecycleError::ShutdownWaitTimeout { stage } => {
                    details.insert("stage".into(), (*stage).into());
                    ("shutdown_wait_timeout", None, ErrorCode::Unknown)
                }
                LifecycleError::InvalidServerId(_) => {
                    ("invalid_server_id", None, ErrorCode::Unknown)
                }
                LifecycleError::ConfigFrozen => ("config_frozen", None, ErrorCode::Unknown),
                LifecycleError::DuplicateRoute(_) => (
                    "duplicate_route",
                    None,
                    ErrorCode::ResourceAlreadyRegistered,
                ),
                LifecycleError::MissingRoute(_) => {
                    ("missing_route", None, ErrorCode::ResourceNotFound)
                }
                LifecycleError::MissingRelayAddress => {
                    ("missing_relay_address", None, ErrorCode::Unknown)
                }
                LifecycleError::RegisterFailure(outcome) => {
                    details.insert("route_name".into(), outcome.route_name.clone());
                    details.insert("failure_source".into(), outcome.failure_source.clone());
                    (
                        "register_failure",
                        outcome.semantic_error.as_ref(),
                        ErrorCode::Unknown,
                    )
                }
                LifecycleError::HeldResponseRelease { .. } => {
                    ("held_response_release", None, ErrorCode::Unknown)
                }
                LifecycleError::ResponseCopy(_) => {
                    ("response_copy", None, ErrorCode::ClientOutputFromBuffer)
                }
                LifecycleError::Configuration(_) => ("configuration", None, ErrorCode::Unknown),
                LifecycleError::Server(_) => ("server", None, ErrorCode::Unknown),
                LifecycleError::StartupTimeout(_) => ("startup_timeout", None, ErrorCode::Unknown),
                LifecycleError::Relay(_) => ("relay", None, ErrorCode::Unknown),
            };
            ("LIFECYCLE", semantic, code, Some(kind))
        }
    };
    let code = semantic.map_or(fallback, |error| error.code);
    let message = semantic.map_or_else(|| error.to_string(), |error| error.message.clone());
    if let Some(semantic) = semantic {
        details.extend(semantic.details.clone());
    }
    if let Some(cause) = error.local_io_cause() {
        details.insert("io_operation".into(), cause.operation().into());
        details.insert("io_error".into(), format!("{:?}", cause.error()));
    }
    let phase = error.transport_phase().map(|phase| match phase {
        TransportPhase::PreDispatch => "PRE_DISPATCH",
        TransportPhase::DispatchUncertain => "DISPATCH_UNCERTAIN",
    });
    let phase = java_string(env, phase)?;
    let call_id = java_string(env, error.call_id())?;
    let kind = java_string(env, kind)?;
    let category = env.new_string(category)?;
    let message = env.new_string(message)?;
    let entries = pairs(
        env,
        details
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )?;
    let factory = class(env, "io/ctwo/error/ErrorFactory")?;
    Ok(env.call_static_method(
        factory, "create",
        "(Ljava/lang/String;ILjava/lang/String;[Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Lio/ctwo/error/C2Exception;",
        &[JValue::Object(&category), JValue::Int(code as i32), JValue::Object(&message),
          JValue::Object(&entries), JValue::Object(&phase), JValue::Object(&call_id), JValue::Object(&kind)],
    )?.l()?)
}

pub(crate) fn run<'local, T>(
    env: &mut JNIEnv<'local>,
    default: T,
    operation: impl FnOnce(&mut JNIEnv<'local>) -> Result<T, Failure>,
) -> T {
    match operation(env) {
        Ok(value) => value,
        Err(failure) => {
            // A failed Java call may have left the original throwable pending.
            // No JNI operation that constructs an exception may run in that case.
            if !env.exception_check().unwrap_or(true) {
                match failure {
                    Failure::Core(error) => match error_object(env, &error) {
                        Ok(value) => {
                            let _ = env.throw(JThrowable::from(value));
                        }
                        Err(_) if !env.exception_check().unwrap_or(true) => {
                            let _ =
                                env.throw_new("java/lang/IllegalStateException", error.to_string());
                        }
                        Err(_) => {}
                    },
                    Failure::Argument(message) => {
                        let _ = env.throw_new("java/lang/IllegalArgumentException", message);
                    }
                    Failure::Jni(error) => {
                        let _ = env.throw_new("java/lang/IllegalStateException", error.to_string());
                    }
                }
            }
            default
        }
    }
}

pub(crate) fn adapter_error(
    env: &mut JNIEnv<'_>,
    phase: AdapterFailurePhase,
    failure: Failure,
) -> C2Error {
    let exception = take_exception(env);
    let local = |message: String, details| AdapterFailure::Local { message, details };
    let failure = match exception {
        Some(values) => match values.as_slice() {
            [owner, code, symbol, path, message, details] if owner == "fastdb" => local(
                message.clone(),
                external_cause_details(ExternalCause {
                    owner,
                    code,
                    symbol,
                    path,
                    message,
                    details_json: details,
                }),
            ),
            [kind, code, message, rest @ ..] if kind == "semantic" => {
                let code = code
                    .parse::<u16>()
                    .ok()
                    .and_then(|n| ErrorCode::try_from(n).ok())
                    .unwrap_or(ErrorCode::Unknown);
                let details = rest
                    .chunks_exact(2)
                    .map(|pair| (pair[0].clone(), pair[1].clone()))
                    .collect();
                AdapterFailure::Semantic(C2Error::new(code, message).with_details(details))
            }
            [_, name, message] => {
                let description = if message.is_empty() {
                    name.clone()
                } else {
                    format!("{name}: {message}")
                };
                local(description, BTreeMap::new())
            }
            _ => local("Java callback failed".into(), BTreeMap::new()),
        },
        None => match failure {
            Failure::Core(Error::Semantic(error)) => AdapterFailure::Semantic(error),
            Failure::Core(error) => local(error.to_string(), BTreeMap::new()),
            Failure::Argument(message) => local(message, BTreeMap::new()),
            Failure::Jni(error) => local(error.to_string(), BTreeMap::new()),
        },
    };
    normalize_adapter_failure(phase, failure)
}

fn take_exception(env: &mut JNIEnv<'_>) -> Option<Vec<String>> {
    if !env.exception_check().ok()? {
        return None;
    }
    let throwable = env.exception_occurred().ok()?;
    env.exception_clear().ok()?;
    let values: Result<Vec<String>, Failure> = (|| {
        let factory = class(env, "io/ctwo/error/ErrorFactory")?;
        let array = env
            .call_static_method(
                factory,
                "describe",
                "(Ljava/lang/Throwable;)[Ljava/lang/String;",
                &[JValue::Object(&throwable)],
            )?
            .l()?;
        strings(env, JObjectArray::from(array))
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    Some(values.unwrap_or_default())
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_internal_NativeCalls_decodeFailure(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    failure: JThrowable<'_>,
) -> jni::sys::jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        env.throw(failure)?;
        let cause = adapter_error(
            env,
            AdapterFailurePhase::ClientOutputFromBuffer,
            JniError::JavaException.into(),
        );
        Ok(error_object(env, &cause.into())?.into_raw())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn releasing_handle_preserves_operation_owners_and_drops_outside_the_lock() {
        let id = put(String::from("call"));
        let in_progress = get::<String>(id).unwrap();
        release(id);
        assert!(get::<String>(id).is_err());
        assert_eq!(in_progress.as_str(), "call");
        assert_eq!(Arc::strong_count(&in_progress), 1);

        struct ObserveDrop(Arc<AtomicBool>);

        impl Drop for ObserveDrop {
            fn drop(&mut self) {
                // The registry must not serialize destructor work with unrelated calls.
                let unlocked = HANDLES.try_lock().is_some();
                self.0.store(unlocked, Ordering::Relaxed);
            }
        }

        let observed = Arc::new(AtomicBool::new(false));
        let id = put(ObserveDrop(observed.clone()));
        release(id);
        assert!(observed.load(Ordering::Relaxed));
    }
}
