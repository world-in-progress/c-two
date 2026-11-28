use std::{sync::Arc, time::Duration};

use c2_core::{
    AdapterFailurePhase, CallOutcomeStatus, Client, CoreResponseStream, CoreTopicSubscription,
    Error, HeldResponse, LagPolicy, LocalCopyTracker, PreparedCall, ResultUnavailableReason,
    SubscribeOptions, TopicPublisher, TopicStart,
};
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jlongArray, jobject};

use crate::bridge::{
    Failure, adapter_error, call_options, class, error_object, get, put, run, string,
};
use crate::payload::{
    SlabPath, can_direct, direct, plan_size, put_response, record_slab, with_payload_bytes,
};

pub(crate) struct JavaClient {
    pub(crate) core: Client,
    pub(crate) holds: Arc<LocalCopyTracker>,
}

impl JavaClient {
    fn route_name(&self) -> &str {
        &self.core.expected_route().route_name
    }

    fn receive<T>(&self, core: T, method: String) -> Receiver<T> {
        Receiver {
            core,
            holds: Arc::clone(&self.holds),
            route: self.route_name().to_owned(),
            method,
        }
    }

    fn adopt(&self, response: HeldResponse, method: &str, held: bool) -> jlong {
        put_response(response, held, &self.holds, self.route_name(), method)
    }
}

struct Receiver<T> {
    core: T,
    holds: Arc<LocalCopyTracker>,
    route: String,
    method: String,
}

impl<T> Receiver<T> {
    fn adopt(&self, response: HeldResponse, held: bool) -> jlong {
        put_response(response, held, &self.holds, &self.route, &self.method)
    }
}

fn serialization_error(env: &mut JNIEnv, failure: Failure) -> Failure {
    let mut error = adapter_error(env, AdapterFailurePhase::ClientInputSerializing, failure);
    error
        .details
        .insert("transport_phase".into(), "pre_dispatch".into());
    error.into()
}

fn encode_request(
    env: &mut JNIEnv,
    mut prepared: PreparedCall,
    input: &JObject,
) -> Result<c2_core::EncodedCall, Failure> {
    if input.is_null() {
        return Ok(prepared.encode_vec(Vec::new())?);
    }
    let planned = plan_size(env, input).map_err(|failure| serialization_error(env, failure))?;
    if let Some(len) = planned {
        if can_direct(env, input).map_err(|failure| serialization_error(env, failure))?
            && let Some(slab) = prepared.try_alloc_request_slab(len)?
            && let Some(slab) =
                direct(env, input, slab).map_err(|failure| serialization_error(env, failure))?
        {
            let encoded = prepared.encode_slab(slab)?;
            record_slab(SlabPath::Request);
            return Ok(encoded);
        }
        prepared.charge_input(len as u64)?;
    }
    with_payload_bytes(env, input, |bytes| Ok(prepared.encode_slice(bytes)))
        .map_err(|failure| serialization_error(env, failure))?
        .map_err(Failure::from)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_withCallOptions(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    options: JObject,
) -> jlong {
    run(&mut env, 0, |env| {
        let client = get::<JavaClient>(handle)?;
        Ok(put(JavaClient {
            core: client.core.with_call_options(call_options(env, &options)?),
            holds: Arc::clone(&client.holds),
        }))
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_call(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    method: JString,
    call_id: JString,
    input: JObject,
    stream: jboolean,
    held: jboolean,
) -> jlong {
    run(&mut env, 0, |env| {
        let client = get::<JavaClient>(handle)?;
        let method = string(env, &method)?;
        let prepared = if stream != 0 {
            if !call_id.is_null() {
                return Err(Failure::argument("Only unary calls can carry a call ID"));
            }
            client.core.begin_stream(&method)?
        } else if call_id.is_null() {
            client.core.begin_call(&method)?
        } else {
            client
                .core
                .begin_keyed_call(&method, &string(env, &call_id)?)?
        };
        let encoded = encode_request(env, prepared, &input)?;
        if stream != 0 {
            Ok(put(client.receive(encoded.stream()?, method)))
        } else {
            Ok(client.adopt(encoded.call_held()?, &method, held != 0))
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_recover(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    method: JString,
    call_id: JString,
    held: jboolean,
) -> jlong {
    run(&mut env, 0, |env| {
        let client = get::<JavaClient>(handle)?;
        let method = string(env, &method)?;
        let call_id = string(env, &call_id)?;
        let response = client.core.recover_unary_call_held(&method, &call_id)?;
        Ok(client.adopt(response, &method, held != 0))
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_query(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    call_id: JString,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let client = get::<JavaClient>(handle)?;
        let outcome = client.core.query_call(&string(env, &call_id)?)?;
        let status = match outcome.status {
            CallOutcomeStatus::InFlight => 0,
            CallOutcomeStatus::Succeeded => 1,
            CallOutcomeStatus::Failed => 2,
            CallOutcomeStatus::Unknown => 3,
        };
        let method = match outcome.method_name {
            Some(name) => env.new_string(name)?.into(),
            None => JObject::null(),
        };
        let reason = match outcome.result_unavailable_reason {
            Some(ResultUnavailableReason::NotRetained) => env.new_string("not_retained")?.into(),
            Some(ResultUnavailableReason::Evicted) => env.new_string("evicted")?.into(),
            None => JObject::null(),
        };
        let code = outcome.error_code.map_or(-1, |code| code as jint);
        let outcome_class = class(env, "io/ctwo/CallOutcome")?;
        Ok(env
            .call_static_method(
                outcome_class,
                "fromNative",
                "(ILjava/lang/String;ZLjava/lang/String;IZ)Lio/ctwo/CallOutcome;",
                &[
                    JValue::Int(status),
                    JValue::Object(&method),
                    JValue::Bool(outcome.result_available.into()),
                    JValue::Object(&reason),
                    JValue::Int(code),
                    JValue::Bool(outcome.error_details_available.into()),
                ],
            )?
            .l()?
            .into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_streamNext(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    held: jboolean,
) -> jlong {
    run(&mut env, 0, |_| {
        let stream = get::<Receiver<CoreResponseStream>>(handle)?;
        Ok(stream
            .core
            .next_item()?
            .map_or(0, |item| stream.adopt(item, held != 0)))
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_streamCancel(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    run(&mut env, (), |_| {
        get::<Receiver<CoreResponseStream>>(handle)?.core.cancel()?;
        Ok(())
    });
}

fn close_timeout(seconds: jlong, nanos: jint) -> Result<Option<Duration>, Failure> {
    if seconds == -1 && nanos == 0 {
        return Ok(None);
    }
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(Failure::argument(
            "Expected a nonnegative close observation timeout",
        ));
    }
    Ok(Some(Duration::new(seconds as u64, nanos as u32)))
}

fn close_receipt<'local>(
    env: &mut JNIEnv<'local>,
    class_name: &str,
    signature: &str,
    completed: bool,
    complete_items: Option<u64>,
    terminal: Option<Result<(), Error>>,
) -> Result<JObject<'local>, Failure> {
    let error = match terminal {
        Some(Err(error)) => error_object(env, &error)?,
        _ => JObject::null(),
    };
    let receipt_class = class(env, class_name)?;
    Ok(env
        .call_static_method(
            receipt_class,
            "fromNative",
            signature,
            &[
                JValue::Bool(completed.into()),
                JValue::Long(complete_items.map_or(-1, |count| count as jlong)),
                JValue::Object(&error),
            ],
        )?
        .l()?)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_streamClose(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    seconds: jlong,
    nanos: jint,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let stream = get::<Receiver<CoreResponseStream>>(handle)?;
        let receipt = stream
            .core
            .close_with_timeout(close_timeout(seconds, nanos)?);
        Ok(close_receipt(
            env,
            "io/ctwo/StreamCloseReceipt",
            "(ZJLio/ctwo/error/C2Exception;)Lio/ctwo/StreamCloseReceipt;",
            receipt.completed,
            receipt.complete_items,
            receipt.terminal,
        )?
        .into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_subscribe(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    topic: JString,
    start: jint,
    lag: jint,
) -> jlong {
    run(&mut env, 0, |env| {
        let start = match start {
            0 => TopicStart::Latest,
            1 => TopicStart::Next,
            _ => return Err(Failure::argument("Unknown subscription starting cursor")),
        };
        let on_lag = match lag {
            0 => LagPolicy::Skip,
            1 => LagPolicy::Error,
            _ => return Err(Failure::argument("Unknown subscription lag policy")),
        };
        let client = get::<JavaClient>(handle)?;
        let topic = string(env, &topic)?;
        let subscription = client
            .core
            .subscribe(&topic, SubscribeOptions { start, on_lag })?;
        Ok(put(client.receive(subscription, topic)))
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_subscriptionStartSeq(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    run(&mut env, 0, |_| {
        Ok(get::<Receiver<CoreTopicSubscription>>(handle)?
            .core
            .start_seq() as jlong)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_topicNext(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    held: jboolean,
) -> jlongArray {
    run(&mut env, std::ptr::null_mut(), |env| {
        let subscription = get::<Receiver<CoreTopicSubscription>>(handle)?;
        let result = env.new_long_array(3)?;
        let delivery = subscription.core.next_delivery()?;
        let sequence = delivery.seq() as jlong;
        let dropped = delivery.dropped() as jlong;
        let owner = subscription.adopt(delivery.into_held(), held != 0);
        if let Err(error) = env.set_long_array_region(&result, 0, &[sequence, dropped, owner]) {
            crate::bridge::release(owner);
            return Err(error.into());
        }
        Ok(result.into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_topicCancel(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    run(&mut env, (), |_| {
        get::<Receiver<CoreTopicSubscription>>(handle)?
            .core
            .cancel()?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_topicClose(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    seconds: jlong,
    nanos: jint,
) -> jobject {
    run(&mut env, std::ptr::null_mut(), |env| {
        let subscription = get::<Receiver<CoreTopicSubscription>>(handle)?;
        let receipt = subscription
            .core
            .close_with_timeout(close_timeout(seconds, nanos)?);
        Ok(close_receipt(
            env,
            "io/ctwo/TopicCloseReceipt",
            "(ZJLio/ctwo/error/C2Exception;)Lio/ctwo/TopicCloseReceipt;",
            receipt.completed,
            receipt.complete_items,
            receipt.terminal,
        )?
        .into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_publish(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    input: JObject,
) -> jlong {
    run(&mut env, 0, |env| {
        let publisher = get::<TopicPublisher>(handle)?;
        if let Some(len) = plan_size(env, &input)?
            && can_direct(env, &input)?
            && let Some(slab) = publisher.try_alloc_slab(len)?
            && let Some(slab) = direct(env, &input, slab)?
        {
            let sequence = publisher.publish_slab(slab)?;
            record_slab(SlabPath::Publisher);
            return Ok(sequence as jlong);
        }
        let sequence =
            with_payload_bytes(env, &input, |bytes| Ok(publisher.publish_bytes(bytes)))??;
        Ok(sequence as jlong)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_publisherHeadSeq(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    run(&mut env, 0, |_| {
        Ok(get::<TopicPublisher>(handle)?.head_seq() as jlong)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_publisherIsClosed(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    run(&mut env, 0, |_| {
        Ok(get::<TopicPublisher>(handle)?.is_closed().into())
    })
}
