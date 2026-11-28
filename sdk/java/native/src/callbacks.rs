use std::sync::Arc;

use c2_core::{
    AccessLevel, AdapterFailurePhase as Phase, AdmissionContext, AdmissionDecision, AdmissionHook,
    AdmissionOperation, AdmissionTransport, EncodedResponse, EncodedService, EncodedStream,
    EncodedStreamItem, EncodedStreamService, ResponseSink, current_call_context,
};
use c2_error::{C2Error, ErrorCode};
use c2_mem::RecycledBytes;
use jni::{
    JNIEnv, JavaVM,
    objects::{GlobalRef, JObject, JObjectArray, JString, JValue},
    sys::jlong,
};

use crate::bridge::{
    Failure, adapter_error, class, java_string, optional_string, pairs, read_pairs, release,
};
use crate::payload::{copy_payload, encode_response, put_recycled, put_vec};

struct JavaCallback {
    vm: Arc<JavaVM>,
    target: GlobalRef,
}

impl JavaCallback {
    fn new(env: &JNIEnv<'_>, target: &JObject<'_>) -> Result<Self, Failure> {
        Ok(Self {
            vm: Arc::new(env.get_java_vm()?),
            target: env.new_global_ref(target)?,
        })
    }

    fn frame<T>(
        &self,
        callback: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Result<T, C2Error>,
    ) -> Result<T, C2Error> {
        let mut env = self
            .vm
            .attach_current_thread_as_daemon()
            .map_err(jni_failure)?;
        let result = env.with_local_frame(64, |env| {
            Ok::<_, jni::errors::Error>(callback(env, self.target.as_obj()))
        });
        result.unwrap_or_else(|failure| {
            Err(adapter_error(
                &mut env,
                Phase::ResourceFunctionExecuting,
                failure.into(),
            ))
        })
    }
}

fn jni_failure(error: jni::errors::Error) -> C2Error {
    C2Error::new(ErrorCode::ResourceFunctionExecuting, error.to_string())
}

fn project<'local, T>(
    env: &mut JNIEnv<'local>,
    phase: Phase,
    callback: impl FnOnce(&mut JNIEnv<'local>) -> Result<T, Failure>,
) -> Result<T, C2Error> {
    callback(env).map_err(|failure| adapter_error(env, phase, failure))
}

pub(crate) fn admission(
    env: &JNIEnv<'_>,
    target: &JObject<'_>,
) -> Result<Arc<dyn AdmissionHook>, Failure> {
    let callback = JavaCallback::new(env, target)?;
    Ok(Arc::new(move |context: &AdmissionContext<'_>| {
        callback
            .frame(|env, hook| {
                project(env, Phase::ResourceFunctionExecuting, |env| {
                    admit(env, hook, context)
                })
            })
            .unwrap_or_else(|_| {
                AdmissionDecision::deny_with_details(
                    "admission hook failed",
                    [("reason".into(), "hook_error".into())].into(),
                )
            })
    }))
}

fn admit(
    env: &mut JNIEnv<'_>,
    hook: &JObject<'_>,
    context: &AdmissionContext<'_>,
) -> Result<AdmissionDecision, Failure> {
    let route = env.new_string(context.route_name())?;
    let namespace = env.new_string(context.crm_ns())?;
    let crm = env.new_string(context.crm_name())?;
    let version = env.new_string(context.crm_ver())?;
    let target = java_string(env, context.target())?;
    let access = java_string(
        env,
        context.access().map(|access| match access {
            AccessLevel::Read => "READ",
            AccessLevel::Write => "WRITE",
        }),
    )?;
    let operation = env.new_string(match context.operation() {
        AdmissionOperation::Unary => "UNARY",
        AdmissionOperation::ServerStream => "SERVER_STREAM",
        AdmissionOperation::Subscribe => "SUBSCRIBE",
        AdmissionOperation::Recover => "RECOVER",
        AdmissionOperation::QueryOutcome => "QUERY_OUTCOME",
    })?;
    let transport = env.new_string(match context.transport() {
        AdmissionTransport::Ipc => "IPC",
        AdmissionTransport::ThreadLocal => "THREAD_LOCAL",
    })?;
    let call_id = java_string(env, context.call_id())?;
    let metadata = pairs(env, context.metadata().iter())?;
    let context_class = class(env, "io/ctwo/AdmissionContext")?;
    let context = env
        .call_static_method(
            context_class,
            "fromNative",
            concat!(
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;",
                "Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;",
                "Ljava/lang/String;[Ljava/lang/String;)Lio/ctwo/AdmissionContext;"
            ),
            &[
                JValue::Object(&route),
                JValue::Object(&namespace),
                JValue::Object(&crm),
                JValue::Object(&version),
                JValue::Object(&target),
                JValue::Object(&access),
                JValue::Object(&operation),
                JValue::Object(&transport),
                JValue::Object(&call_id),
                JValue::Object(&metadata),
            ],
        )?
        .l()?;
    let decision = env
        .call_method(
            hook,
            "admit",
            "(Lio/ctwo/AdmissionContext;)Lio/ctwo/AdmissionDecision;",
            &[JValue::Object(&context)],
        )?
        .l()?;
    let reason = env
        .call_method(&decision, "reason", "()Ljava/lang/String;", &[])?
        .l()?;
    match optional_string(env, &JString::from(reason))? {
        None => Ok(AdmissionDecision::allow()),
        Some(reason) => {
            let fields = env
                .call_method(&decision, "nativeEntries", "()[Ljava/lang/String;", &[])?
                .l()?;
            Ok(AdmissionDecision::deny_with_details(
                reason,
                read_pairs(env, JObjectArray::from(fields))?,
            ))
        }
    }
}

pub(crate) fn service(
    env: &JNIEnv<'_>,
    target: &JObject<'_>,
) -> Result<Arc<dyn EncodedStreamService>, Failure> {
    Ok(Arc::new(JavaService(JavaCallback::new(env, target)?)))
}

fn call_context<'local>(env: &mut JNIEnv<'local>) -> Result<JObject<'local>, Failure> {
    let context =
        current_call_context().ok_or_else(|| Failure::argument("missing Core call context"))?;
    let route = env.new_string(context.route_name())?;
    let method = env.new_string(context.target())?;
    let call_id = java_string(env, context.call_id())?;
    let metadata = pairs(env, context.metadata().iter())?;
    let context_class = class(env, "io/ctwo/CallContext")?;
    Ok(env.call_static_method(
        context_class, "fromNative",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;)Lio/ctwo/CallContext;",
        &[JValue::Object(&route), JValue::Object(&method), JValue::Object(&call_id),
          JValue::Object(&metadata)],
    )?.l()?)
}

struct InputOwner(jlong);

impl Drop for InputOwner {
    fn drop(&mut self) {
        release(self.0);
    }
}

fn open_input<'local>(
    env: &mut JNIEnv<'local>,
    callbacks: &JObject<'_>,
    context: &JObject<'_>,
    owner: &InputOwner,
) -> Result<JObject<'local>, C2Error> {
    project(env, Phase::ResourceInputFromBuffer, |env| {
        Ok(env
            .call_method(
                callbacks,
                "openInput",
                "(Lio/ctwo/CallContext;J)Lio/ctwo/Held;",
                &[JValue::Object(context), JValue::Long(owner.0)],
            )?
            .l()?)
    })
}

struct JavaService(JavaCallback);

impl JavaService {
    fn unary<T>(
        &self,
        owner: InputOwner,
        encode: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Result<T, Failure>,
    ) -> Result<T, C2Error> {
        self.0.frame(|env, callbacks| {
            let context = project(env, Phase::ResourceFunctionExecuting, call_context)?;
            let input = open_input(env, callbacks, &context, &owner)?;
            let output = project(env, Phase::ResourceFunctionExecuting, |env| {
                Ok(env
                    .call_method(
                        callbacks,
                        "invoke",
                        "(Lio/ctwo/CallContext;Lio/ctwo/Held;)Ljava/lang/Object;",
                        &[JValue::Object(&context), JValue::Object(&input)],
                    )?
                    .l()?)
            });
            let response = output.and_then(|output| {
                let encoded = project(env, Phase::ResourceOutputSerializing, |env| {
                    let source = env
                        .call_method(
                            callbacks,
                            "encodeOutput",
                            "(Lio/ctwo/CallContext;Ljava/lang/Object;)Ljava/lang/Object;",
                            &[JValue::Object(&context), JValue::Object(&output)],
                        )?
                        .l()?;
                    encode(env, &source)
                });
                let closed = project(env, Phase::ResourceOutputSerializing, |env| {
                    let receiver = env.get_object_class(callbacks)?;
                    env.call_static_method(
                        receiver,
                        "closeOutput",
                        "(Lio/ctwo/Held;Ljava/lang/Object;)V",
                        &[JValue::Object(&input), JValue::Object(&output)],
                    )?;
                    Ok(())
                });
                encoded.and_then(|value| closed.map(|()| value))
            });
            let clean = project(env, Phase::ResourceInputFromBuffer, |env| {
                env.call_method(&input, "close", "()V", &[])?;
                Ok(())
            });
            response.and_then(|value| clean.map(|()| value))
        })
    }

    fn stream(&self, owner: InputOwner) -> Result<Box<dyn EncodedStream>, C2Error> {
        self.0.frame(|env, callbacks| {
            let context = project(env, Phase::ResourceFunctionExecuting, call_context)?;
            let input = open_input(env, callbacks, &context, &owner)?;
            let stream = project(env, Phase::ResourceFunctionExecuting, |env| {
                Ok(env.call_method(callbacks, "openStream",
                    "(Lio/ctwo/CallContext;Lio/ctwo/Held;)Lio/ctwo/internal/ServiceCallbacks$ActiveStream;",
                    &[JValue::Object(&context), JValue::Object(&input)])?.l()?)
            });
            match stream {
                Ok(stream) => match project(env, Phase::ResourceFunctionExecuting, |env| {
                    Ok(env.new_global_ref(&stream)?)
                }) {
                    Ok(target) => Ok(Box::new(JavaStream(JavaCallback {
                        vm: Arc::clone(&self.0.vm), target,
                    })) as Box<dyn EncodedStream>),
                    Err(failure) => {
                        let _ = project(env, Phase::ResourceFunctionExecuting, |env| {
                            env.call_method(stream, "finish", "()V", &[])?;
                            Ok(())
                        });
                        Err(failure)
                    }
                },
                Err(failure) => {
                    let _ = project(env, Phase::ResourceInputFromBuffer, |env| {
                        env.call_method(input, "close", "()V", &[])?;
                        Ok(())
                    });
                    Err(failure)
                }
            }
        })
    }
}

impl EncodedService for JavaService {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        self.unary(InputOwner(put_vec(request)), copy_payload)
    }

    fn invoke_into(
        &self,
        _: u16,
        request: RecycledBytes,
        sink: &ResponseSink,
    ) -> Result<EncodedResponse, C2Error> {
        self.unary(InputOwner(put_recycled(request)), |env, payload| {
            encode_response(env, payload, sink)
        })
    }
}

impl EncodedStreamService for JavaService {
    fn open_stream(&self, _: u16, request: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
        self.stream(InputOwner(put_vec(request)))
    }

    fn open_stream_recycled(
        &self,
        _: u16,
        request: RecycledBytes,
    ) -> Result<Box<dyn EncodedStream>, C2Error> {
        self.stream(InputOwner(put_recycled(request)))
    }
}

struct JavaStream(JavaCallback);

impl EncodedStream for JavaStream {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        self.0.frame(|env, stream| {
            let payload = project(env, Phase::ResourceFunctionExecuting, |env| {
                Ok(env
                    .call_method(stream, "next", "()Lio/fastdb/payload/Payload;", &[])?
                    .l()?)
            })?;
            if payload.is_null() {
                return Ok(None);
            }
            let bytes = project(env, Phase::ResourceOutputSerializing, |env| {
                let source = env
                    .call_method(
                        stream,
                        "encodeItem",
                        "(Lio/fastdb/payload/Payload;)Ljava/lang/Object;",
                        &[JValue::Object(&payload)],
                    )?
                    .l()?;
                copy_payload(env, &source)
            });
            let closed = project(env, Phase::ResourceOutputSerializing, |env| {
                env.call_method(
                    stream,
                    "closeItem",
                    "(Lio/fastdb/payload/Payload;)V",
                    &[JValue::Object(&payload)],
                )?;
                Ok(())
            });
            bytes.and_then(|bytes| {
                closed.map(|()| Some(Box::new(StreamBytes(bytes)) as Box<dyn EncodedStreamItem>))
            })
        })
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        self.0.frame(|env, stream| {
            project(env, Phase::ResourceFunctionExecuting, |env| {
                env.call_method(stream, "finish", "()V", &[])?;
                Ok(())
            })
        })
    }
}

struct StreamBytes(Vec<u8>);

impl EncodedStreamItem for StreamBytes {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        let bytes = self
            .0
            .get(offset..)
            .and_then(|bytes| bytes.get(..destination.len()))
            .ok_or_else(|| {
                C2Error::new(
                    ErrorCode::ResourceOutputSerializing,
                    "invalid stream item range",
                )
            })?;
        destination.copy_from_slice(bytes);
        Ok(())
    }
}
