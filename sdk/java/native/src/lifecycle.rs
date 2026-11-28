use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_contract::{
    ContractError, ContractLimitMetric, ContractRelease, ContractReleaseRef, ExpectedRouteContract,
    MAX_CONTRACT_SOURCE_BYTES, MethodAccess, MethodMode,
};
use c2_core::{
    AdmissionHook, Connect, Host, HostOptions, LifecycleError, MethodDefinition, Registration,
    RegistrationObserver, RelayCleanupError, RoleCallExecutionStats, RoleMemoryStats,
    RouteCloseOutcome, Runtime, RuntimeCallExecutionStats, RuntimeIdentity, RuntimeMemoryStats,
    RuntimeOptions, RuntimeShutdownTarget, ServiceConcurrencyMode, ServiceDefinition,
    ShutdownOutcome,
};
use c2_error::{C2Error, ErrorCode};
use jni::{
    JNIEnv,
    objects::{GlobalRef, JByteArray, JClass, JIntArray, JObject, JObjectArray, JString, JValue},
    sys::{jboolean, jint, jlong, jobject, jobjectArray, jstring},
};
use parking_lot::Mutex;

use crate::bridge::{Failure, call_options, get, java_string, optional_string, put, run, string};
use crate::callbacks;
use crate::client::JavaClient;

#[derive(Default)]
struct StartupState {
    target: Option<ShutdownTarget>,
    hook: Option<(GlobalRef, Arc<dyn AdmissionHook>)>,
}

enum ShutdownTarget {
    Host(JavaHost),
    FailedStart(c2_core::FailedStartShutdownReceipt),
}

struct JavaRuntime {
    core: Runtime,
    connect_timeout: Option<Duration>,
    // Serializes selecting a shutdown target with recording the originating startup.
    // The transaction is always observed after releasing this lock.
    startup: Mutex<StartupState>,
}

#[derive(Clone)]
struct JavaHost {
    core: Host,
    identity: RuntimeIdentity,
    registrations: Arc<Mutex<HashMap<String, Arc<JavaRegistration>>>>,
}

struct JavaRegistration {
    core: Mutex<Option<Registration>>,
    observer: RegistrationObserver,
}

fn object<'local>(
    env: &mut JNIEnv<'local>,
    receiver: &JObject<'_>,
    name: &str,
    signature: &str,
) -> Result<JObject<'local>, Failure> {
    Ok(env.call_method(receiver, name, signature, &[])?.l()?)
}

fn optional_text(
    env: &mut JNIEnv<'_>,
    receiver: &JObject<'_>,
    name: &str,
) -> Result<Option<String>, Failure> {
    let value = JString::from(object(env, receiver, name, "()Ljava/lang/String;")?);
    optional_string(env, &value)
}

fn text(env: &mut JNIEnv<'_>, receiver: &JObject<'_>, name: &str) -> Result<String, Failure> {
    let value = JString::from(object(env, receiver, name, "()Ljava/lang/String;")?);
    string(env, &value)
}

fn optional_number(
    env: &mut JNIEnv<'_>,
    receiver: &JObject<'_>,
    field: &str,
    signature: &str,
) -> Result<Option<u64>, Failure> {
    let value = object(env, receiver, field, signature)?;
    if value.is_null() {
        return Ok(None);
    }
    let number = env.call_method(&value, "longValue", "()J", &[])?.j()?;
    let number = u64::try_from(number)
        .map_err(|_| Failure::argument(format!("{field} must be non-negative")))?;
    Ok(Some(number))
}

fn optional_long(
    env: &mut JNIEnv<'_>,
    receiver: &JObject<'_>,
    field: &str,
) -> Result<Option<u64>, Failure> {
    optional_number(env, receiver, field, "()Ljava/lang/Long;")
}

fn optional_int(
    env: &mut JNIEnv<'_>,
    receiver: &JObject<'_>,
    field: &str,
) -> Result<Option<u64>, Failure> {
    optional_number(env, receiver, field, "()Ljava/lang/Integer;")
}

fn flag(env: &mut JNIEnv<'_>, receiver: &JObject<'_>, field: &str) -> Result<bool, Failure> {
    Ok(env.call_method(receiver, field, "()Z", &[])?.z()?)
}

fn duration(env: &mut JNIEnv<'_>, receiver: &JObject<'_>) -> Result<Option<Duration>, Failure> {
    if receiver.is_null() {
        return Ok(None);
    }
    let seconds = env.call_method(receiver, "getSeconds", "()J", &[])?.j()?;
    let nanos = env.call_method(receiver, "getNano", "()I", &[])?.i()?;
    let seconds =
        u64::try_from(seconds).map_err(|_| Failure::argument("timeout must be non-negative"))?;
    Ok(Some(Duration::new(seconds, nanos as u32)))
}

fn named_duration(
    env: &mut JNIEnv<'_>,
    receiver: &JObject<'_>,
    field: &str,
) -> Result<Option<Duration>, Failure> {
    let value = object(env, receiver, field, "()Ljava/time/Duration;")?;
    duration(env, &value)
}

fn host_options(env: &mut JNIEnv<'_>, value: &JObject<'_>) -> Result<HostOptions, Failure> {
    let mut options = HostOptions::default();
    if let Some(value) = optional_text(env, value, "serverId")? {
        options = options.with_server_id(value);
    }
    if let Some(value) = named_duration(env, value, "startupTimeout")? {
        options = options.with_startup_timeout(value);
    }
    if let Some(value) = named_duration(env, value, "shutdownTimeout")? {
        options = options.with_shutdown_timeout(value);
    }
    if let Some(value) = optional_long(env, value, "maxPayloadSize")? {
        options = options.with_max_payload_size(value);
    }
    if let Some(value) = optional_int(env, value, "maxExecutionWorkers")? {
        options = options.with_max_execution_workers(value as u32);
    }
    if let Some(value) = optional_long(env, value, "outcomeMaxEntries")? {
        options = options.with_outcome_max_entries(native_size(value)?);
    }
    if let Some(value) = optional_long(env, value, "outcomeMaxRetainedBytes")? {
        options = options.with_outcome_max_retained_bytes(value);
    }
    if let Some(value) = optional_long(env, value, "memoryBudgetBytes")? {
        options = options.with_memory_budget_bytes(value);
    }
    if let Some(value) = optional_long(env, value, "fileSpillBudgetBytes")? {
        options = options.with_file_spill_budget_bytes(value);
    }
    if let Some(value) = optional_long(env, value, "maxOutstandingCalls")? {
        options = options.with_max_outstanding_calls(value);
    }
    if flag(env, value, "relayDisabled")? {
        options = options.without_relay();
    } else if let Some(value) = optional_text(env, value, "relayAnchorAddress")? {
        options = options.with_relay_anchor_address(value);
    }
    let proxy = object(env, value, "relayUseProxy", "()Ljava/lang/Boolean;")?;
    if !proxy.is_null() {
        options = options.with_relay_use_proxy(flag(env, &proxy, "booleanValue")?);
    }
    if flag(env, value, "clearAdmissionHook")? {
        options = options.without_admission_hook();
    }
    Ok(options)
}

fn native_size(value: u64) -> Result<usize, Failure> {
    usize::try_from(value).map_err(|_| Failure::argument("size exceeds this platform's range"))
}

fn expected_route(
    env: &mut JNIEnv<'_>,
    value: &JObject<'_>,
) -> Result<ExpectedRouteContract, Failure> {
    Ok(ExpectedRouteContract {
        route_name: text(env, value, "routeName")?,
        crm_ns: text(env, value, "crmNamespace")?,
        crm_name: text(env, value, "crmName")?,
        crm_ver: text(env, value, "crmVersion")?,
        abi_hash: text(env, value, "abiHash")?,
        signature_hash: text(env, value, "signatureHash")?,
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeCreate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    value: JObject<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let options = RuntimeOptions {
            client_limits: c2_core::DomainLimitsOverrides {
                memory_budget_bytes: optional_long(env, &value, "memoryBudgetBytes")?,
                file_spill_budget_bytes: optional_long(env, &value, "fileSpillBudgetBytes")?,
                max_outstanding_calls: optional_long(env, &value, "maxOutstandingCalls")?,
            },
            local_endpoint: c2_core::LocalEndpointOptions {
                unix_root: optional_text(env, &value, "nativeUnixEndpointRoot")?.map(PathBuf::from),
            },
            relay_anchor_address: optional_text(env, &value, "relayAnchorAddress")?,
            use_process_relay_anchor: flag(env, &value, "useProcessRelayAnchor")?,
        };
        Ok(put(JavaRuntime {
            core: Runtime::new(options)?,
            connect_timeout: named_duration(env, &value, "connectTimeout")?,
            startup: Mutex::default(),
        }))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeStartHost(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    value: JObject<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let runtime = get::<JavaRuntime>(id)?;
        let mut options = host_options(env, &value)?;
        let hook = object(env, &value, "admissionHook", "()Lio/ctwo/AdmissionHook;")?;
        let mut startup = runtime.startup.lock();
        let mut new_hook = None;
        if !hook.is_null() {
            // Core compares configured hooks by Arc identity. Preserve that identity
            // when the same Java hook is used to start the Host again.
            let adapter = match &startup.hook {
                Some((previous, adapter)) if env.is_same_object(&hook, previous.as_obj())? => {
                    adapter.clone()
                }
                _ => {
                    let adapter = callbacks::admission(env, &hook)?;
                    new_hook = Some((env.new_global_ref(&hook)?, adapter.clone()));
                    adapter
                }
            };
            options = options.with_admission_hook(adapter);
        }

        let was_frozen = runtime.core.server_config_frozen();
        let mut failed_start = None;
        let result = runtime
            .core
            .host_with_failed_start_receipt(options, |receipt| {
                failed_start = Some(receipt);
            });
        if !was_frozen && runtime.core.server_config_frozen() {
            startup.hook = new_hook;
        }
        let host = match result {
            Ok(host) => host,
            Err(error) => {
                let displaced = failed_start.and_then(|receipt| {
                    startup.target.replace(ShutdownTarget::FailedStart(receipt))
                });
                drop(startup);
                drop(displaced);
                return Err(error.into());
            }
        };
        let identity = runtime.core.ensure_server()?;
        let registrations = match startup.target.as_ref() {
            Some(ShutdownTarget::Host(current))
                if current.identity.server_instance_id == identity.server_instance_id =>
            {
                current.registrations.clone()
            }
            _ => Arc::default(),
        };
        let host = JavaHost {
            core: host,
            identity,
            registrations,
        };
        let displaced = startup.target.replace(ShutdownTarget::Host(host.clone()));
        drop(startup);
        drop(displaced);
        Ok(put(host))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeConnect(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    expected: JObject<'_>,
    kind: jint,
    address: JString<'_>,
    timeout: JObject<'_>,
    defaults: JObject<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let runtime = get::<JavaRuntime>(id)?;
        let expected = expected_route(env, &expected)?;
        let mode = match kind {
            0 => Connect::DirectIpc {
                address: string(env, &address)?,
            },
            1 => Connect::ExplicitRelay {
                relay_url: string(env, &address)?,
            },
            2 => match optional_string(env, &address)? {
                Some(anchor_url) => Connect::RelayAwareAt { anchor_url },
                None => Connect::RelayAware,
            },
            _ => return Err(Failure::argument("unknown connection mode")),
        };
        let mut options = c2_config::ConnectOptions::new();
        if let Some(timeout) = duration(env, &timeout)?.or(runtime.connect_timeout) {
            options = options.with_timeout(timeout);
        }
        let defaults = call_options(env, &defaults)?;
        let core = runtime
            .core
            .connect_with_options(expected, mode, options)?
            .with_call_options(defaults);
        Ok(put(JavaClient {
            core,
            holds: runtime.core.holds_tracker(),
        }))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeShutdown(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    timeout: JObject<'_>,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let runtime = get::<JavaRuntime>(id)?;
        let timeout =
            duration(env, &timeout)?.ok_or_else(|| Failure::argument("timeout is null"))?;
        let deadline = c2_config::deadline_after(Instant::now(), timeout)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let receipt = {
            let state = runtime.startup.try_lock_until(deadline).ok_or(
                LifecycleError::ShutdownWaitTimeout {
                    stage: "java_startup_wait",
                },
            )?;
            let target = match state.target.as_ref() {
                Some(ShutdownTarget::Host(host)) => RuntimeShutdownTarget::Host(&host.core),
                Some(ShutdownTarget::FailedStart(receipt)) => {
                    RuntimeShutdownTarget::FailedStart(receipt)
                }
                None => RuntimeShutdownTarget::ClientsOnly,
            };
            runtime.core.begin_session_retirement(target, deadline)?
        };
        let (outcome, completed) = receipt.observe_until(deadline);
        Ok(shutdown_snapshot(env, outcome, completed)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeIdentity(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    field: jint,
) -> jstring {
    run(&mut env, std::ptr::null_mut(), |env| {
        let host = get::<JavaHost>(id)?;
        let value = match field {
            0 => &host.identity.server_id,
            1 => &host.identity.server_instance_id,
            2 => host.core.local_endpoint().address(),
            _ => return Err(Failure::argument("unknown Host identity field")),
        };
        Ok(env.new_string(value)?.into_raw())
    })
}

fn service_definition(
    env: &mut JNIEnv<'_>,
    release: &ContractRelease,
    parameters: &JObject<'_>,
) -> Result<ServiceDefinition, Failure> {
    let route_name = text(env, parameters, "routeName")?;
    let service = object(
        env,
        parameters,
        "callbacks",
        "()Lio/ctwo/internal/ServiceCallbacks;",
    )?;
    let service = callbacks::service(env, &service)?;
    let methods = release
        .descriptor()
        .methods()
        .iter()
        .enumerate()
        .map(|(index, method)| MethodDefinition {
            index: index as u16,
            name: method.name().to_owned(),
            access: method.access(),
        });
    let mut definition =
        ServiceDefinition::new_stream(release, release.reference(), route_name, methods, service)?;
    let mode = match env
        .call_method(parameters, "nativeConcurrencyMode", "()I", &[])?
        .i()?
    {
        0 => ServiceConcurrencyMode::Parallel,
        1 => ServiceConcurrencyMode::Exclusive,
        2 => ServiceConcurrencyMode::ReadParallel,
        _ => return Err(Failure::argument("unknown service concurrency mode")),
    };
    let max_pending = optional_int(env, parameters, "maxPending")?
        .map(native_size)
        .transpose()?;
    let max_workers = optional_int(env, parameters, "maxWorkers")?
        .map(native_size)
        .transpose()?;
    definition = definition.with_concurrency(mode, max_pending, max_workers)?;
    let topics = JObjectArray::from(object(
        env,
        parameters,
        "capacityTopics",
        "()[Ljava/lang/String;",
    )?);
    let capacities = JIntArray::from(object(env, parameters, "topicCapacities", "()[I")?);
    let mut values = vec![0; env.get_array_length(&capacities)? as usize];
    env.get_int_array_region(&capacities, 0, &mut values)?;
    for (index, capacity) in values.into_iter().enumerate() {
        let topic = JString::from(env.get_object_array_element(&topics, index as i32)?);
        let name = string(env, &topic)?;
        env.delete_local_ref(topic)?;
        definition = definition.with_topic_capacity(
            &name,
            usize::try_from(capacity)
                .map_err(|_| Failure::argument("topic capacity must be positive"))?,
        )?;
    }
    let topics = JObjectArray::from(object(
        env,
        parameters,
        "initialTopics",
        "()[Ljava/lang/String;",
    )?);
    let payloads = JObjectArray::from(object(env, parameters, "initialPayloads", "()[[B")?);
    for index in 0..env.get_array_length(&payloads)? {
        let (name, payload) = env.with_local_frame(4, |env| {
            let topic = JString::from(env.get_object_array_element(&topics, index)?);
            let payload = JByteArray::from(env.get_object_array_element(&payloads, index)?);
            let payload = env.convert_byte_array(&payload)?;
            Ok::<_, Failure>((string(env, &topic)?, payload))
        })?;
        definition = definition.with_initial_topic_bytes(&name, payload)?;
    }
    Ok(definition)
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeRegister(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    release_id: jlong,
    parameters: JObject<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let host = get::<JavaHost>(id)?;
        let release = get::<ContractRelease>(release_id)?;
        let definition = service_definition(env, &release, &parameters)?;
        let registration = host.core.register(definition)?;
        let name = registration.route_name().to_owned();
        let observer = registration.observer();
        let registration = Arc::new(JavaRegistration {
            core: Mutex::new(Some(registration)),
            observer,
        });
        let replaced = host.registrations.lock().insert(name, registration.clone());
        drop(replaced);
        Ok(put(registration))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeIsRunning(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jboolean {
    run(&mut env, 0, |_| {
        Ok(u8::from(get::<JavaHost>(id)?.core.is_running()))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeShutdown(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    timeout: JObject<'_>,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let host = get::<JavaHost>(id)?;
        let observed = match duration(env, &timeout)? {
            Some(timeout) => {
                c2_config::deadline_after(Instant::now(), timeout)
                    .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
                host.core.shutdown_with_timeout(timeout)
            }
            None => host.core.shutdown(),
        };
        let (outcome, completed) = match host.core.shutdown_outcome() {
            Some(terminal) => (terminal, true),
            None => (observed, false),
        };
        Ok(shutdown_snapshot(env, outcome, completed)?.into_raw())
    })
}

fn closing_registration() -> Failure {
    C2Error::new(ErrorCode::ResourceClosed, "registration is closing").into()
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Registration_nativeIsClosed(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jboolean {
    run(&mut env, 0, |_| {
        Ok(u8::from(
            get::<Arc<JavaRegistration>>(id)?.observer.is_closed(),
        ))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Registration_nativeActiveSubscribers(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jlong {
    run(&mut env, 0, |_| {
        Ok(get::<Arc<JavaRegistration>>(id)?
            .observer
            .active_subscribers() as jlong)
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Registration_nativePublisher(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    name: JString<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let registration = get::<Arc<JavaRegistration>>(id)?;
        let topic = string(env, &name)?;
        let guard = registration.core.lock();
        let core = guard.as_ref().ok_or_else(closing_registration)?;
        Ok(put(core.publisher(&topic)?))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeUnregister(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    name: JString<'_>,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let host = get::<JavaHost>(id)?;
        let name = string(env, &name)?;
        let registration = host
            .registrations
            .lock()
            .get(&name)
            .cloned()
            .ok_or_else(|| LifecycleError::MissingRoute(name.clone()))?;
        // A callback can inspect the registration or attempt a publish while
        // Core drains it. Keep those calls off the native unregister wait.
        let mut core = registration
            .core
            .lock()
            .take()
            .ok_or_else(closing_registration)?;
        let outcome = core.close();
        *registration.core.lock() = Some(core);
        let outcome = outcome?;
        let removed = {
            let mut registrations = host.registrations.lock();
            if registrations
                .get(&name)
                .is_some_and(|entry| Arc::ptr_eq(entry, &registration))
            {
                registrations.remove(&name)
            } else {
                None
            }
        };
        drop(removed);
        let name = env.new_string(outcome.route_name)?;
        let close = route_close(env, outcome.close)?;
        let relay = match outcome.relay_error {
            Some(error) => relay_cleanup(env, error)?,
            None => JObject::null(),
        };
        Ok(env.new_object(
            "io/ctwo/UnregisterOutcome",
            "(Ljava/lang/String;ZLio/ctwo/ShutdownOutcome$RouteClose;Lio/ctwo/ShutdownOutcome$RelayCleanup;)V",
            &[
                JValue::Object(name.as_ref()),
                JValue::Bool(outcome.local_removed.into()),
                JValue::Object(&close),
                JValue::Object(&relay),
            ],
        )?.into_raw())
    })
}

fn new_array<'local, T>(
    env: &mut JNIEnv<'local>,
    class: &str,
    items: impl ExactSizeIterator<Item = T>,
    mut project: impl for<'frame> FnMut(&mut JNIEnv<'frame>, T) -> Result<JObject<'frame>, Failure>,
) -> Result<JObjectArray<'local>, Failure> {
    let size =
        i32::try_from(items.len()).map_err(|_| Failure::argument("too many snapshot entries"))?;
    let array = env.new_object_array(size, class, JObject::null())?;
    for (index, item) in items.enumerate() {
        env.with_local_frame(16, |env| {
            let entry = project(env, item)?;
            env.set_object_array_element(&array, index as i32, entry)?;
            Ok::<(), Failure>(())
        })?;
    }
    Ok(array)
}

fn java_long<'local>(
    env: &mut JNIEnv<'local>,
    value: Option<u64>,
) -> Result<JObject<'local>, Failure> {
    match value {
        Some(value) => {
            Ok(env.new_object("java/lang/Long", "(J)V", &[JValue::Long(value as jlong)])?)
        }
        None => Ok(JObject::null()),
    }
}

fn java_boolean<'local>(
    env: &mut JNIEnv<'local>,
    value: Option<bool>,
) -> Result<JObject<'local>, Failure> {
    match value {
        Some(value) => {
            Ok(env.new_object("java/lang/Boolean", "(Z)V", &[JValue::Bool(value.into())])?)
        }
        None => Ok(JObject::null()),
    }
}

fn memory_cell<'local>(
    env: &mut JNIEnv<'local>,
    stats: Option<c2_core::MemoryCellStats>,
) -> Result<JObject<'local>, Failure> {
    let Some(stats) = stats else {
        return Ok(JObject::null());
    };
    Ok(env.new_object(
        "io/ctwo/MemoryStatsSnapshot$Cell",
        "(JJJJJ)V",
        &[
            JValue::Long(stats.limit_bytes as jlong),
            JValue::Long(stats.used_bytes as jlong),
            JValue::Long(stats.peak_bytes as jlong),
            JValue::Long(stats.rejected_allocations as jlong),
            JValue::Long(stats.rejected_bytes as jlong),
        ],
    )?)
}

fn memory_role<'local>(
    env: &mut JNIEnv<'local>,
    stats: RoleMemoryStats,
) -> Result<JObject<'local>, Failure> {
    env.with_local_frame_returning_local(8, |env| {
        let id = java_long(env, stats.domain_id)?;
        let memory = memory_cell(env, stats.memory)?;
        let file_spill = memory_cell(env, stats.file_spill)?;
        Ok(env.new_object(
            "io/ctwo/MemoryStatsSnapshot$Role",
            "(Ljava/lang/Long;JZZLio/ctwo/MemoryStatsSnapshot$Cell;Lio/ctwo/MemoryStatsSnapshot$Cell;)V",
            &[
                JValue::Object(&id),
                JValue::Long(stats.generation as jlong),
                JValue::Bool(stats.frozen.into()),
                JValue::Bool(stats.closed.into()),
                JValue::Object(&memory),
                JValue::Object(&file_spill),
            ],
        )?)
    })
}

fn memory_snapshot<'local>(
    env: &mut JNIEnv<'local>,
    stats: RuntimeMemoryStats,
) -> Result<JObject<'local>, Failure> {
    let client = memory_role(env, stats.client)?;
    let server = memory_role(env, stats.server)?;
    let retired = new_array(
        env,
        "io/ctwo/MemoryStatsSnapshot$RetiredRole",
        stats.retired.into_iter(),
        |env, entry| {
            let role = env.new_string(entry.role.as_str())?;
            let stats = memory_role(env, entry.stats)?;
            Ok(env.new_object(
                "io/ctwo/MemoryStatsSnapshot$RetiredRole",
                "(Ljava/lang/String;Lio/ctwo/MemoryStatsSnapshot$Role;)V",
                &[JValue::Object(role.as_ref()), JValue::Object(&stats)],
            )?)
        },
    )?;
    let directions = new_array(
        env,
        "io/ctwo/MemoryStatsSnapshot$CopyBucket",
        stats.holds.by_direction.into_iter(),
        |env, (kind, bucket)| {
            let direction = env.new_string(kind.as_str())?;
            Ok(env.new_object(
                "io/ctwo/MemoryStatsSnapshot$CopyBucket",
                "(Ljava/lang/String;JJ)V",
                &[
                    JValue::Object(direction.as_ref()),
                    JValue::Long(bucket.active_holds as jlong),
                    JValue::Long(bucket.total_held_bytes as jlong),
                ],
            )?)
        },
    )?;
    let holds = env.new_object(
        "io/ctwo/MemoryStatsSnapshot$LocalCopies",
        "(JJD[Lio/ctwo/MemoryStatsSnapshot$CopyBucket;)V",
        &[
            JValue::Long(stats.holds.active_holds as jlong),
            JValue::Long(stats.holds.total_held_bytes as jlong),
            JValue::Double(stats.holds.oldest_hold_seconds),
            JValue::Object(directions.as_ref()),
        ],
    )?;
    Ok(env.new_object(
        "io/ctwo/MemoryStatsSnapshot",
        "(Lio/ctwo/MemoryStatsSnapshot$Role;Lio/ctwo/MemoryStatsSnapshot$Role;[Lio/ctwo/MemoryStatsSnapshot$RetiredRole;Lio/ctwo/MemoryStatsSnapshot$LocalCopies;)V",
        &[
            JValue::Object(&client),
            JValue::Object(&server),
            JValue::Object(retired.as_ref()),
            JValue::Object(&holds),
        ],
    )?)
}

fn execution_role<'local>(
    env: &mut JNIEnv<'local>,
    stats: RoleCallExecutionStats,
) -> Result<JObject<'local>, Failure> {
    env.with_local_frame_returning_local(8, |env| {
        let max = java_long(env, stats.max_operations)?;
        let used = java_long(env, stats.used_operations)?;
        let peak = java_long(env, stats.peak_operations)?;
        let rejected = java_long(env, stats.rejected_reservations)?;
        Ok(env.new_object(
            "io/ctwo/CallExecutionSnapshot$Role",
            "(Ljava/lang/Long;Ljava/lang/Long;Ljava/lang/Long;Ljava/lang/Long;Z)V",
            &[
                JValue::Object(&max),
                JValue::Object(&used),
                JValue::Object(&peak),
                JValue::Object(&rejected),
                JValue::Bool(stats.closed.into()),
            ],
        )?)
    })
}

fn execution_snapshot<'local>(
    env: &mut JNIEnv<'local>,
    stats: RuntimeCallExecutionStats,
) -> Result<JObject<'local>, Failure> {
    let client = execution_role(env, stats.client)?;
    let server = execution_role(env, stats.server)?;
    let retired = new_array(
        env,
        "io/ctwo/CallExecutionSnapshot$RetiredRole",
        stats.retired.into_iter(),
        |env, entry| {
            let role = env.new_string(entry.role.as_str())?;
            let stats = execution_role(env, entry.stats)?;
            Ok(env.new_object(
                "io/ctwo/CallExecutionSnapshot$RetiredRole",
                "(Ljava/lang/String;Lio/ctwo/CallExecutionSnapshot$Role;)V",
                &[JValue::Object(role.as_ref()), JValue::Object(&stats)],
            )?)
        },
    )?;
    Ok(env.new_object(
        "io/ctwo/CallExecutionSnapshot",
        "(Lio/ctwo/CallExecutionSnapshot$Role;Lio/ctwo/CallExecutionSnapshot$Role;[Lio/ctwo/CallExecutionSnapshot$RetiredRole;)V",
        &[JValue::Object(&client), JValue::Object(&server), JValue::Object(retired.as_ref())],
    )?)
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeMemoryStats(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let stats = get::<JavaRuntime>(id)?.core.memory_stats();
        Ok(memory_snapshot(env, stats)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Runtime_nativeCallExecutionSnapshot(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let stats = get::<JavaRuntime>(id)?.core.call_execution_snapshot();
        Ok(execution_snapshot(env, stats)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeMemoryStats(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let stats = get::<JavaHost>(id)?.core.memory_stats();
        Ok(memory_snapshot(env, stats)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeCallExecutionSnapshot(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let stats = get::<JavaHost>(id)?.core.call_execution_snapshot();
        Ok(execution_snapshot(env, stats)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_Host_nativeLifecycleSnapshot(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let value = get::<JavaHost>(id)?.core.lifecycle_snapshot();
        let work_drained = java_boolean(env, value.work_drained)?;
        Ok(env
            .new_object(
                "io/ctwo/HostLifecycleSnapshot",
                "(ZZLjava/lang/Boolean;)V",
                &[
                    JValue::Bool(value.finished.into()),
                    JValue::Bool(value.listener_closed.into()),
                    JValue::Object(&work_drained),
                ],
            )?
            .into_raw())
    })
}

fn route_close<'local>(
    env: &mut JNIEnv<'local>,
    item: RouteCloseOutcome,
) -> Result<JObject<'local>, Failure> {
    let name = env.new_string(item.route_name)?;
    let reason = env.new_string(item.closed_reason)?;
    let error = java_string(env, item.close_error.as_deref())?;
    Ok(env.new_object(
        "io/ctwo/ShutdownOutcome$RouteClose",
        "(Ljava/lang/String;ZZLjava/lang/String;Ljava/lang/String;)V",
        &[
            JValue::Object(name.as_ref()),
            JValue::Bool(item.local_removed.into()),
            JValue::Bool(item.active_drained.into()),
            JValue::Object(reason.as_ref()),
            JValue::Object(error.as_ref()),
        ],
    )?)
}

fn relay_cleanup<'local>(
    env: &mut JNIEnv<'local>,
    item: RelayCleanupError,
) -> Result<JObject<'local>, Failure> {
    let name = env.new_string(item.route_name)?;
    let message = env.new_string(item.message)?;
    let status = match item.status_code {
        Some(code) => env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(code.into())])?,
        None => JObject::null(),
    };
    Ok(env.new_object(
        "io/ctwo/ShutdownOutcome$RelayCleanup",
        "(Ljava/lang/String;Ljava/lang/Integer;Ljava/lang/String;)V",
        &[
            JValue::Object(name.as_ref()),
            JValue::Object(&status),
            JValue::Object(message.as_ref()),
        ],
    )?)
}

fn shutdown_snapshot<'local>(
    env: &mut JNIEnv<'local>,
    outcome: ShutdownOutcome,
    completed: bool,
) -> Result<JObject<'local>, Failure> {
    let removed = new_array(
        env,
        "java/lang/String",
        outcome.removed_routes.into_iter(),
        |env, name| Ok(env.new_string(name)?.into()),
    )?;
    let routes = new_array(
        env,
        "io/ctwo/ShutdownOutcome$RouteClose",
        outcome.route_outcomes.into_iter(),
        route_close,
    )?;
    let relay = new_array(
        env,
        "io/ctwo/ShutdownOutcome$RelayCleanup",
        outcome.relay_errors.into_iter(),
        relay_cleanup,
    )?;
    let route_error = java_string(env, outcome.route_close_error.as_deref())?;
    let runtime_error = java_string(env, outcome.runtime_barrier_error.as_deref())?;
    let client_error = java_string(env, outcome.ipc_client_close_error.as_deref())?;
    let close_details = new_array(
        env,
        "io/ctwo/ShutdownOutcome$IpcClientClose",
        outcome.ipc_client_close_details.into_iter(),
        |env, item| {
            let address = env.new_string(item.address)?;
            let reason = env.new_string(item.reason)?;
            Ok(env.new_object(
                "io/ctwo/ShutdownOutcome$IpcClientClose",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(address.as_ref()),
                    JValue::Object(reason.as_ref()),
                ],
            )?)
        },
    )?;
    Ok(env.new_object(
        "io/ctwo/ShutdownOutcome",
        "(Z[Ljava/lang/String;[Lio/ctwo/ShutdownOutcome$RouteClose;[Lio/ctwo/ShutdownOutcome$RelayCleanup;ZZLjava/lang/String;Ljava/lang/String;Ljava/lang/String;[Lio/ctwo/ShutdownOutcome$IpcClientClose;)V",
        &[
            JValue::Bool(completed.into()),
            JValue::Object(removed.as_ref()),
            JValue::Object(routes.as_ref()),
            JValue::Object(relay.as_ref()),
            JValue::Bool(outcome.server_was_started.into()),
            JValue::Bool(outcome.ipc_clients_drained.into()),
            JValue::Object(route_error.as_ref()),
            JValue::Object(runtime_error.as_ref()),
            JValue::Object(client_error.as_ref()),
            JValue::Object(close_details.as_ref()),
        ],
    )?)
}

fn contract_bytes(env: &mut JNIEnv<'_>, source: &JByteArray<'_>) -> Result<Vec<u8>, Failure> {
    let observed = env.get_array_length(source)? as u64;
    if observed > MAX_CONTRACT_SOURCE_BYTES {
        return Err(ContractError::LimitExceeded {
            metric: ContractLimitMetric::SourceBytes,
            limit: MAX_CONTRACT_SOURCE_BYTES,
            observed,
            path: "$".into(),
        }
        .into());
    }
    Ok(env.convert_byte_array(source)?)
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeFromDescriptor(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    source: JByteArray<'_>,
) -> jlong {
    run(&mut env, 0, |env| {
        let source = contract_bytes(env, &source)?;
        Ok(put(ContractRelease::from_descriptor_json(&source)?))
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeDescriptor(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jstring {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        Ok(env
            .new_string(release.canonical_descriptor_json())?
            .into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeDigest(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jstring {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        Ok(env
            .new_string(release.descriptor_sha256().as_str())?
            .into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeFingerprint(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    signature: jboolean,
) -> jstring {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        let descriptor = release.descriptor();
        let hash = if signature == 0 {
            descriptor.abi_hash()
        } else {
            descriptor.signature_hash()
        };
        Ok(env.new_string(hash)?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeReference(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jstring {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        Ok(env
            .new_string(release.reference().to_canonical_json()?)?
            .into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeVerifyReference(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    source: JByteArray<'_>,
) {
    run(&mut env, (), |env| {
        let release = get::<ContractRelease>(id)?;
        let source = contract_bytes(env, &source)?;
        ContractReleaseRef::from_json(&source)?.verify_release(&release)?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeExpectedRoute(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
    route: JString<'_>,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        let expected = release.expected_route(string(env, &route)?)?;
        let route_name = env.new_string(expected.route_name)?;
        let ns = env.new_string(expected.crm_ns)?;
        let name = env.new_string(expected.crm_name)?;
        let version = env.new_string(expected.crm_ver)?;
        let abi = env.new_string(expected.abi_hash)?;
        let signature = env.new_string(expected.signature_hash)?;
        Ok(env.new_object(
            "io/ctwo/ExpectedRouteContract",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                JValue::Object(route_name.as_ref()), JValue::Object(ns.as_ref()),
                JValue::Object(name.as_ref()), JValue::Object(version.as_ref()),
                JValue::Object(abi.as_ref()), JValue::Object(signature.as_ref()),
            ],
        )?.into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeMethods(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobjectArray {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        let methods = release.descriptor().methods().iter().enumerate();
        Ok(new_array(
            env,
            "io/ctwo/MethodDefinition",
            methods,
            |env, (index, method)| {
                let name = env.new_string(method.name())?;
                Ok(env.new_object(
                    "io/ctwo/MethodDefinition",
                    "(ILjava/lang/String;ZZ)V",
                    &[
                        JValue::Int(index as jint),
                        JValue::Object(name.as_ref()),
                        JValue::Bool(matches!(method.access(), MethodAccess::Write).into()),
                        JValue::Bool(matches!(method.mode(), MethodMode::ServerStream).into()),
                    ],
                )?)
            },
        )?
        .into_raw())
    })
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_ctwo_ContractRelease_nativeTopicNames(
    mut env: JNIEnv<'_>,
    _receiver: JObject<'_>,
    id: jlong,
) -> jobjectArray {
    run(&mut env, std::ptr::null_mut(), |env| {
        let release = get::<ContractRelease>(id)?;
        Ok(new_array(
            env,
            "java/lang/String",
            release.descriptor().topics().iter(),
            |env, topic| Ok(env.new_string(topic.name())?.into()),
        )?
        .into_raw())
    })
}
