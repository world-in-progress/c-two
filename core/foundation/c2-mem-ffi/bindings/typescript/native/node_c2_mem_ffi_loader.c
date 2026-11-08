#ifndef _WIN32
#define _POSIX_C_SOURCE 200809L
#endif
#include <node_api.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#include <pthread.h>
#endif
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "c2_mem_ffi.h"

/* Successful loads stay pinned for the lifetime of the Node environment. Native
 * callbacks and external ArrayBuffers may outlive the JS symbols object. */
typedef struct Symbols {
    void *library;
    uint32_t (*abi_version)(void);
    C2MemFfiStatus (*capture)(const char *, C2MemFfiLocalEndpointContext **);
    void (*context_free)(C2MemFfiLocalEndpointContext *);
    C2MemFfiStatus (*name_len)(const C2MemFfiLocalEndpointContext *, const char *, size_t *);
    C2MemFfiStatus (*name_copy)(const C2MemFfiLocalEndpointContext *, const char *, char *, size_t, size_t *);
    C2MemFfiStatus (*namespace_len)(const C2MemFfiLocalEndpointContext *, size_t *);
    C2MemFfiStatus (*namespace_copy)(const C2MemFfiLocalEndpointContext *, char *, size_t, size_t *);
    C2MemFfiStatus (*domain_new)(const C2MemFfiLocalEndpointContext *, const char *, C2MemFfiClientDomain **, C2MemFfiError **);
    void (*domain_free)(C2MemFfiClientDomain *);
    C2MemFfiStatus (*domain_stats)(const C2MemFfiClientDomain *, C2MemFfiResult **);
    C2MemFfiStatus (*endpoint_new)(const C2MemFfiClientDomain *, const char *, const char *, C2MemFfiEndpointContext **, C2MemFfiError **);
    C2MemFfiStatus (*endpoint_prepare)(const C2MemFfiEndpointContext *, const char *, const char *, const char *, C2MemFfiComplete, void *);
    C2MemFfiStatus (*endpoint_deactivate)(const C2MemFfiEndpointContext *);
    C2MemFfiStatus (*endpoint_close)(const C2MemFfiEndpointContext *, C2MemFfiComplete, void *);
    void (*endpoint_free)(C2MemFfiEndpointContext *);
    C2MemFfiStatus (*call_begin)(const C2MemFfiEndpointContext *, const char *, const char *, uint8_t, const uint64_t *, C2MemFfiCall **, C2MemFfiError **);
    C2MemFfiStatus (*call_charge)(const C2MemFfiCall *, uint64_t, C2MemFfiComplete, void *);
    C2MemFfiStatus (*call_dispatch)(const C2MemFfiCall *, const uint8_t *, size_t, C2MemFfiComplete, void *);
    C2MemFfiStatus (*call_next)(const C2MemFfiCall *, C2MemFfiComplete, void *);
    C2MemFfiStatus (*call_cancel)(const C2MemFfiCall *);
    C2MemFfiStatus (*call_observe)(const C2MemFfiCall *, const uint64_t *, C2MemFfiComplete, void *);
    C2MemFfiStatus (*call_check_deadline)(const C2MemFfiCall *, C2MemFfiError **);
    void (*call_free)(C2MemFfiCall *);
    C2MemFfiStatus (*buffer_data)(const C2MemFfiBuffer *, const uint8_t **, size_t *);
    void (*buffer_release)(C2MemFfiBuffer *);
    const char *(*error_parts)(const C2MemFfiError *);
    void (*error_free)(C2MemFfiError *);
    void (*result_free)(C2MemFfiResult *);
} Symbols;

static napi_value fail(napi_env env, const char *message) { napi_throw_error(env, NULL, message); return NULL; }
static napi_value bad(napi_env env, const char *message) { napi_throw_type_error(env, NULL, message); return NULL; }
static bool ok(napi_status status) { return status == napi_ok; }
static bool put(napi_env env, napi_value obj, const char *name, napi_value value) {
    return ok(napi_set_named_property(env, obj, name, value));
}
static bool method(napi_env env, napi_value obj, const char *name, napi_callback fn, void *data) {
    napi_value value;
    return ok(napi_create_function(env, name, NAPI_AUTO_LENGTH, fn, data, &value)) && put(env, obj, name, value);
}
static bool args(napi_env env, napi_callback_info info, size_t *count, napi_value *values, napi_value *receiver, void **data) {
    return ok(napi_get_cb_info(env, info, count, values, receiver, data));
}
static char *string(napi_env env, napi_value value, const char *message) {
    size_t len = 0, written = 0;
    if (!ok(napi_get_value_string_utf8(env, value, NULL, 0, &len))) { bad(env, message); return NULL; }
    char *out = malloc(len + 1);
    if (out == NULL) { fail(env, "Out of memory."); return NULL; }
    if (!ok(napi_get_value_string_utf8(env, value, out, len + 1, &written)) || memchr(out, 0, written) != NULL) {
        free(out); bad(env, "Native string arguments cannot contain NUL characters."); return NULL;
    }
    out[written] = 0;
    return out;
}
static napi_value js_string(napi_env env, const char *value) {
    napi_value out;
    return ok(napi_create_string_utf8(env, value, NAPI_AUTO_LENGTH, &out)) ? out : NULL;
}
static napi_value json_call(napi_env env, const char *name, napi_value input) {
    napi_value global, json, fn, out;
    if (!ok(napi_get_global(env, &global)) || !ok(napi_get_named_property(env, global, "JSON", &json)) ||
        !ok(napi_get_named_property(env, json, name, &fn)) ||
        !ok(napi_call_function(env, json, fn, 1, &input, &out))) return NULL;
    return out;
}
static char *json_string(napi_env env, napi_value object, const char *label) {
    napi_valuetype type;
    if (!ok(napi_typeof(env, object, &type)) || type != napi_object) { bad(env, label); return NULL; }
    napi_value encoded = json_call(env, "stringify", object);
    return encoded == NULL ? NULL : string(env, encoded, label);
}
static napi_value json_parse(napi_env env, const char *json) {
    napi_value source = js_string(env, json);
    return source == NULL ? NULL : json_call(env, "parse", source);
}
static bool nullish(napi_env env, napi_value value) {
    napi_valuetype type;
    return ok(napi_typeof(env, value, &type)) && (type == napi_null || type == napi_undefined);
}
static bool uint64(napi_env env, napi_value value, uint64_t *out, const char *label) {
    napi_valuetype type;
    if (!ok(napi_typeof(env, value, &type))) return false;
    if (type == napi_bigint) {
        bool lossless = false;
        if (ok(napi_get_value_bigint_uint64(env, value, out, &lossless)) && lossless) return true;
    } else if (type == napi_number) {
        double d;
        if (ok(napi_get_value_double(env, value, &d)) && d >= 0 && d <= 9007199254740991.0 && (double)(uint64_t)d == d) {
            *out = (uint64_t)d; return true;
        }
    }
    bad(env, label); return false;
}
static bool timeout(napi_env env, napi_value value, uint64_t *storage, const uint64_t **out) {
    if (nullish(env, value)) { *out = NULL; return true; }
    if (!uint64(env, value, storage, "Timeout must be a nonnegative safe integer or bigint, or null.")) return false;
    *out = storage; return true;
}
static napi_value error_from_details(napi_env env, napi_value details) {
    napi_value msg, error, fallback;
    bool has = false;
    if (!ok(napi_has_named_property(env, details, "message", &has))) return NULL;
    if (!has || !ok(napi_get_named_property(env, details, "message", &msg))) {
        msg = js_string(env, "C-Two native error.");
    }
    if (msg == NULL || !ok(napi_create_error(env, NULL, msg, &error))) return NULL;
    if (!put(env, error, "nativeDetails", details)) return NULL;
    /* Native error JSON is authoritative; keep every structured field. */
    const char *fields[] = { "code", "canonicalName", "details", "transportPhase", "category", "localProtocolVersion", "peerProtocolVersion", "ioOperation", "ioKind", "rawOsError" };
    for (size_t i = 0; i < sizeof(fields) / sizeof(fields[0]); i++) {
        if (!ok(napi_has_named_property(env, details, fields[i], &has))) return NULL;
        if (has) {
            if (!ok(napi_get_named_property(env, details, fields[i], &fallback)) || !put(env, error, fields[i], fallback)) return NULL;
        }
    }
    return error;
}
static napi_value native_error(napi_env env, Symbols *s, C2MemFfiError *err, C2MemFfiStatus status) {
    napi_value result = NULL;
    if (err != NULL) {
        const char *parts = s->error_parts(err);
        if (parts != NULL) {
            napi_value details = json_parse(env, parts);
            if (details != NULL) result = error_from_details(env, details);
        }
        s->error_free(err);
    }
    if (result == NULL) {
        char message[96];
        snprintf(message, sizeof(message), "C-Two native operation failed with status %u.", (unsigned)status);
        napi_value details, msg, code;
        if (ok(napi_create_object(env, &details)) && (msg = js_string(env, message)) != NULL &&
            ok(napi_create_uint32(env, status, &code)) && put(env, details, "message", msg) &&
            put(env, details, "code", code)) result = error_from_details(env, details);
    }
    return result;
}
static napi_value throw_native(napi_env env, Symbols *s, C2MemFfiError *err, C2MemFfiStatus status) {
    napi_value value = native_error(env, s, err, status);
    if (value != NULL) napi_throw(env, value);
    return NULL;
}
static napi_value throw_status(napi_env env, C2MemFfiStatus status) {
    char message[96];
    snprintf(message, sizeof(message), "C-Two native operation failed with status %u.", (unsigned)status);
    napi_value details, msg, code, error;
    if (!ok(napi_create_object(env, &details)) || (msg = js_string(env, message)) == NULL ||
        !ok(napi_create_uint32(env, status, &code)) || !put(env, details, "message", msg) ||
        !put(env, details, "code", code) || (error = error_from_details(env, details)) == NULL) return NULL;
    napi_throw(env, error); return NULL;
}

typedef enum { OWNER_CONTEXT, OWNER_DOMAIN, OWNER_ENDPOINT, OWNER_CALL } OwnerKind;
typedef struct Owner { Symbols *s; void *ptr; OwnerKind kind; } Owner;
static const napi_type_tag tags[] = {
    { UINT64_C(0x150d476eea241c11), UINT64_C(0x8d9ac802c572b2a1) },
    { UINT64_C(0x150d476eea241c12), UINT64_C(0x8d9ac802c572b2a2) },
    { UINT64_C(0x150d476eea241c13), UINT64_C(0x8d9ac802c572b2a3) },
    { UINT64_C(0x150d476eea241c14), UINT64_C(0x8d9ac802c572b2a4) },
};
static void owner_release(Owner *owner) {
    if (owner == NULL || owner->ptr == NULL) return;
    void *ptr = owner->ptr; owner->ptr = NULL;
    switch (owner->kind) {
    case OWNER_CONTEXT: owner->s->context_free(ptr); break;
    case OWNER_DOMAIN: owner->s->domain_free(ptr); break;
    case OWNER_ENDPOINT: owner->s->endpoint_free(ptr); break;
    case OWNER_CALL: owner->s->call_free(ptr); break;
    }
}
static void owner_finalizer(napi_env env, void *data, void *hint) {
    (void)env; (void)hint;
    owner_release(data); free(data);
}
static Owner *owner_get(napi_env env, napi_value object, OwnerKind kind, bool live) {
    bool tagged = false; void *data = NULL;
    if (!ok(napi_check_object_type_tag(env, object, &tags[kind], &tagged)) || !tagged ||
        !ok(napi_unwrap(env, object, &data)) || data == NULL) { bad(env, "Invalid native receiver."); return NULL; }
    Owner *owner = data;
    if (owner->kind != kind || (live && owner->ptr == NULL)) { bad(env, "Native object is closed."); return NULL; }
    return owner;
}
static napi_value owner_wrap(napi_env env, Symbols *s, OwnerKind kind, void *ptr) {
    Owner *owner = calloc(1, sizeof(*owner));
    if (owner == NULL) { Owner temp = { s, ptr, kind }; owner_release(&temp); return fail(env, "Out of memory."); }
    *owner = (Owner){ s, ptr, kind };
    napi_value obj;
    if (!ok(napi_create_object(env, &obj)) || !ok(napi_type_tag_object(env, obj, &tags[kind])) ||
        !ok(napi_wrap(env, obj, owner, owner_finalizer, NULL, NULL))) {
        owner_finalizer(env, owner, NULL); return NULL;
    }
    return obj;
}

static napi_value version_cb(napi_env env, napi_callback_info info) {
    Symbols *s = NULL;
    if (!args(env, info, NULL, NULL, NULL, (void **)&s)) return NULL;
    napi_value out;
    return ok(napi_create_uint32(env, s->abi_version(), &out)) ? out : NULL;
}
static napi_value context_method(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1], self; void *data = NULL;
    if (!args(env, info, &argc, argv, &self, &data)) return NULL;
    Owner *owner = owner_get(env, self, OWNER_CONTEXT, data != (void *)3);
    if (owner == NULL) return NULL;
    if (data == (void *)3) { owner_release(owner); napi_value out; napi_get_undefined(env, &out); return out; }
    char *address = NULL;
    if (data == (void *)1) {
        if (argc != 1 || (address = string(env, argv[0], "Address must be a string.")) == NULL) return NULL;
    }
    size_t len = 0, written = 0;
    C2MemFfiStatus st = address == NULL ? owner->s->namespace_len(owner->ptr, &len) : owner->s->name_len(owner->ptr, address, &len);
    if (st != C2_MEM_FFI_STATUS_OK || len == SIZE_MAX) { free(address); return throw_status(env, st); }
    char *buffer = malloc(len + 1);
    if (buffer == NULL) { free(address); return fail(env, "Out of memory."); }
    st = address == NULL ? owner->s->namespace_copy(owner->ptr, buffer, len + 1, &written) :
        owner->s->name_copy(owner->ptr, address, buffer, len + 1, &written);
    free(address);
    napi_value out = st == C2_MEM_FFI_STATUS_OK ? js_string(env, buffer) : throw_status(env, st);
    free(buffer); return out;
}
static napi_value capture_cb(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1]; Symbols *s = NULL;
    if (!args(env, info, &argc, argv, NULL, (void **)&s)) return NULL;
    char *root = NULL;
    if (argc > 0 && !nullish(env, argv[0]) && (root = string(env, argv[0], "IPC root must be a string or null.")) == NULL) return NULL;
    C2MemFfiLocalEndpointContext *ptr = NULL;
    C2MemFfiStatus st = s->capture(root, &ptr); free(root);
    if (st != C2_MEM_FFI_STATUS_OK || ptr == NULL) return throw_status(env, st);
    napi_value object = owner_wrap(env, s, OWNER_CONTEXT, ptr);
    if (object == NULL || !method(env, object, "endpointName", context_method, (void *)1) ||
        !method(env, object, "namespaceId", context_method, (void *)2) ||
        !method(env, object, "close", context_method, (void *)3)) return NULL;
    return object;
}

/* The external ArrayBuffer and its typed view can finalize independently.
 * Either may outlive the other, including after explicit detachment. */
typedef struct BufferOwner { Symbols *s; C2MemFfiBuffer *ptr; unsigned references; } BufferOwner;
static const napi_type_tag buffer_tag = { UINT64_C(0x150d476eea241c15), UINT64_C(0x8d9ac802c572b2a5) };
static void buffer_owner_drop(BufferOwner *owner) {
    if (--owner->references != 0) return;
    if (owner->ptr != NULL) owner->s->buffer_release(owner->ptr);
    free(owner);
}
static void buffer_finalizer(napi_env env, void *data, void *hint) {
    (void)env; (void)data;
    buffer_owner_drop(hint);
}
static void buffer_view_finalizer(napi_env env, void *data, void *hint) {
    (void)env; (void)hint;
    buffer_owner_drop(data);
}
static napi_value buffer_release_cb(napi_env env, napi_callback_info info) {
    napi_value self, arraybuffer; size_t argc = 0; void *raw = NULL; bool tagged = false;
    if (!args(env, info, &argc, NULL, &self, NULL) ||
        !ok(napi_check_object_type_tag(env, self, &buffer_tag, &tagged)) || !tagged ||
        !ok(napi_unwrap(env, self, &raw)) || raw == NULL)
        return bad(env, "Invalid native buffer receiver.");
    BufferOwner *owner = raw;
    if (owner->ptr != NULL) {
        if (!ok(napi_get_typedarray_info(env, self, NULL, NULL, NULL, &arraybuffer, NULL))) return NULL;
        C2MemFfiBuffer *ptr = owner->ptr;
        owner->ptr = NULL;
        if (!ok(napi_detach_arraybuffer(env, arraybuffer))) { owner->ptr = ptr; return fail(env, "Failed to detach native buffer."); }
        owner->s->buffer_release(ptr);
    }
    napi_value out; napi_get_undefined(env, &out); return out;
}
static napi_value buffer_view(napi_env env, Symbols *s, C2MemFfiBuffer *buffer) {
    const uint8_t *data = NULL; size_t len = 0;
    C2MemFfiStatus st = s->buffer_data(buffer, &data, &len);
    if (st != C2_MEM_FFI_STATUS_OK || (data == NULL && len != 0)) {
        s->buffer_release(buffer); return throw_status(env, st);
    }
    BufferOwner *owner = calloc(1, sizeof(*owner));
    if (owner == NULL) { s->buffer_release(buffer); return fail(env, "Out of memory."); }
    *owner = (BufferOwner){ s, buffer, 1 };
    napi_value ab, view;
    if (!ok(napi_create_external_arraybuffer(env, (void *)data, len, buffer_finalizer, owner, &ab))) {
        buffer_owner_drop(owner); return NULL;
    }
    if (!ok(napi_create_typedarray(env, napi_uint8_array, len, ab, 0, &view)) ||
        !ok(napi_type_tag_object(env, view, &buffer_tag))) return NULL;
    owner->references++;
    if (!ok(napi_wrap(env, view, owner, buffer_view_finalizer, NULL, NULL))) {
        buffer_owner_drop(owner); return NULL;
    }
    if (!method(env, view, "release", buffer_release_cb, NULL)) return NULL;
    return view;
}

typedef enum { ASYNC_JSON, ASYNC_VOID, ASYNC_NEXT, ASYNC_OBSERVE } AsyncKind;
#ifdef _WIN32
typedef CRITICAL_SECTION AsyncMutex;
static bool async_mutex_init(AsyncMutex *mutex) { return InitializeCriticalSectionAndSpinCount(mutex, 0) != 0; }
static void async_mutex_lock(AsyncMutex *mutex) { EnterCriticalSection(mutex); }
static void async_mutex_unlock(AsyncMutex *mutex) { LeaveCriticalSection(mutex); }
static void async_mutex_destroy(AsyncMutex *mutex) { DeleteCriticalSection(mutex); }
#else
typedef pthread_mutex_t AsyncMutex;
static bool async_mutex_init(AsyncMutex *mutex) {
    pthread_mutexattr_t attributes;
    if (pthread_mutexattr_init(&attributes) != 0) return false;
    int status = pthread_mutexattr_settype(&attributes, PTHREAD_MUTEX_RECURSIVE);
    if (status == 0) status = pthread_mutex_init(mutex, &attributes);
    pthread_mutexattr_destroy(&attributes);
    return status == 0;
}
static void async_mutex_lock(AsyncMutex *mutex) { pthread_mutex_lock(mutex); }
static void async_mutex_unlock(AsyncMutex *mutex) { pthread_mutex_unlock(mutex); }
static void async_mutex_destroy(AsyncMutex *mutex) { pthread_mutex_destroy(mutex); }
#endif
typedef struct Async {
    Symbols *s;
    napi_deferred deferred;
    napi_threadsafe_function tsfn;
    AsyncKind kind;
    AsyncMutex mutex;
    unsigned references;
    bool cleanup_registered;
} Async;
static void async_drop(Async *operation) {
    async_mutex_lock(&operation->mutex);
    bool last = --operation->references == 0;
    async_mutex_unlock(&operation->mutex);
    if (last) { async_mutex_destroy(&operation->mutex); free(operation); }
}
static void async_cleanup(void *data) {
    Async *operation = data;
    async_mutex_lock(&operation->mutex);
    /* Env cleanup hooks run before TSFN finalizers. Close the handle path
     * before Worker teardown can invalidate the TSFN. */
    operation->tsfn = NULL;
    operation->cleanup_registered = false;
    async_mutex_unlock(&operation->mutex);
    async_drop(operation);
}
static void async_finalizer(napi_env env, void *data, void *hint) {
    (void)hint;
    Async *operation = data;
    async_mutex_lock(&operation->mutex);
    operation->tsfn = NULL;
    bool remove_cleanup = operation->cleanup_registered;
    operation->cleanup_registered = false;
    async_mutex_unlock(&operation->mutex);
    if (remove_cleanup) {
        napi_remove_env_cleanup_hook(env, async_cleanup, operation);
        async_drop(operation);
    }
    async_drop(operation);
}
static napi_value result_value(napi_env env, Async *operation, C2MemFfiResult *result) {
    if (operation->kind == ASYNC_NEXT) {
        if (result->buffer == NULL) { napi_value none; napi_get_null(env, &none); return none; }
        C2MemFfiBuffer *buffer = result->buffer; result->buffer = NULL;
        return buffer_view(env, operation->s, buffer);
    }
    if (operation->kind == ASYNC_VOID) { napi_value out; napi_get_undefined(env, &out); return out; }
    if (result->json == NULL) { napi_value none; napi_get_null(env, &none); return none; }
    napi_value out = json_parse(env, result->json);
    if (out == NULL || operation->kind != ASYNC_OBSERVE) return out;
    napi_value error; bool has = false;
    if (ok(napi_has_named_property(env, out, "error", &has)) && has && ok(napi_get_named_property(env, out, "error", &error)) && !nullish(env, error)) {
        napi_value typed = error_from_details(env, error);
        if (typed != NULL) put(env, out, "error", typed);
    }
    if (ok(napi_has_named_property(env, out, "completeItems", &has)) && has) {
        napi_value count;
        if (ok(napi_get_named_property(env, out, "completeItems", &count)) && !nullish(env, count)) {
            char *decimal = string(env, count, "Invalid native completeItems count.");
            if (decimal != NULL) {
                char *end = NULL; unsigned long long n = strtoull(decimal, &end, 10);
                if (*decimal != 0 && *end == 0) {
                    napi_value bigint; if (ok(napi_create_bigint_uint64(env, (uint64_t)n, &bigint))) put(env, out, "completeItems", bigint);
                }
                free(decimal);
            }
        }
    }
    return out;
}
static void deliver(napi_env env, napi_value js_cb, void *context, void *data) {
    (void)js_cb;
    Async *operation = context; C2MemFfiResult *result = data;
    if (env != NULL) {
        napi_value value = NULL;
        bool failure = false;
        if (result == NULL) { value = js_string(env, "Native callback returned no result."); failure = true; }
        else if (result->error != NULL) {
            C2MemFfiError *error = result->error; result->error = NULL;
            value = native_error(env, operation->s, error, C2_MEM_FFI_STATUS_IO_ERROR); failure = true;
        } else value = result_value(env, operation, result);
        if (value == NULL) {
            napi_value exception;
            if (ok(napi_get_and_clear_last_exception(env, &exception))) value = exception;
            else value = js_string(env, "Failed to project native result.");
            failure = true;
        }
        if (failure) napi_reject_deferred(env, operation->deferred, value);
        else napi_resolve_deferred(env, operation->deferred, value);
    }
    if (result != NULL) operation->s->result_free(result);
}
static void native_complete(void *user, C2MemFfiResult *result) {
    Async *operation = user;
    /* The native callback and TSFN finalizer share the context. Worker teardown
     * may finalize the TSFN before this callback arrives. Serialize handle use
     * with finalization and never touch it after N-API reports napi_closing. */
    async_mutex_lock(&operation->mutex);
    napi_threadsafe_function tsfn = operation->tsfn;
    napi_status status = tsfn == NULL ? napi_closing :
        napi_call_threadsafe_function(tsfn, result, napi_tsfn_nonblocking);
    if (tsfn != NULL) {
        operation->tsfn = NULL;
        if (status != napi_closing) napi_release_threadsafe_function(tsfn, napi_tsfn_release);
    }
    async_mutex_unlock(&operation->mutex);
    if (status != napi_ok && result != NULL) operation->s->result_free(result);
    async_drop(operation);
}
static Async *async_new(napi_env env, Symbols *s, AsyncKind kind, napi_value *promise) {
    Async *operation = calloc(1, sizeof(*operation));
    if (operation == NULL) { fail(env, "Out of memory."); return NULL; }
    if (!async_mutex_init(&operation->mutex)) { free(operation); fail(env, "Out of memory."); return NULL; }
    operation->s = s; operation->kind = kind;
    /* Caller, Rust completion, TSFN finalizer, and env cleanup hook each own
     * one reference. The latter two may finish before the Rust callback. */
    operation->references = 4;
    napi_value name = js_string(env, "C-Two native completion");
    if (name == NULL || !ok(napi_create_promise(env, &operation->deferred, promise)) ||
        !ok(napi_create_threadsafe_function(env, NULL, NULL, name, 0, 1, operation, async_finalizer, operation, deliver, &operation->tsfn))) {
        async_mutex_destroy(&operation->mutex); free(operation);
        fail(env, "Failed to create native completion."); return NULL;
    }
    if (!ok(napi_add_env_cleanup_hook(env, async_cleanup, operation))) {
        async_mutex_lock(&operation->mutex);
        napi_threadsafe_function tsfn = operation->tsfn;
        operation->tsfn = NULL;
        napi_release_threadsafe_function(tsfn, napi_tsfn_abort);
        async_mutex_unlock(&operation->mutex);
        /* The hook was never installed, and no native callback was launched. */
        async_drop(operation);
        async_drop(operation);
        async_drop(operation);
        fail(env, "Failed to register native completion cleanup."); return NULL;
    }
    operation->cleanup_registered = true;
    return operation;
}
static napi_value async_status(napi_env env, Async *operation, C2MemFfiStatus status, napi_value promise) {
    if (status != C2_MEM_FFI_STATUS_OK) {
        napi_value reason = native_error(env, operation->s, NULL, status);
        if (reason != NULL) napi_reject_deferred(env, operation->deferred, reason);
        /* A failed launch has no native completion; return its reference here. */
        async_mutex_lock(&operation->mutex);
        napi_threadsafe_function tsfn = operation->tsfn;
        operation->tsfn = NULL;
        if (tsfn != NULL) napi_release_threadsafe_function(tsfn, napi_tsfn_abort);
        async_mutex_unlock(&operation->mutex);
        async_drop(operation);
    }
    async_drop(operation);
    return promise;
}

static napi_value endpoint_method(napi_env env, napi_callback_info info);
static napi_value call_method(napi_env env, napi_callback_info info);
static napi_value domain_method(napi_env env, napi_callback_info info) {
    size_t argc = 2; napi_value argv[2], self; void *data = NULL;
    if (!args(env, info, &argc, argv, &self, &data)) return NULL;
    Owner *owner = owner_get(env, self, OWNER_DOMAIN, data != (void *)3);
    if (owner == NULL) return NULL;
    Symbols *s = owner->s;
    if (data == (void *)3) { owner_release(owner); napi_value out; napi_get_undefined(env, &out); return out; }
    if (data == (void *)2) {
        C2MemFfiResult *result = NULL; C2MemFfiStatus st = s->domain_stats(owner->ptr, &result);
        if (st != C2_MEM_FFI_STATUS_OK || result == NULL) {
            napi_value out = result != NULL && result->error != NULL ? throw_native(env, s, result->error, st) : throw_status(env, st);
            if (result != NULL) { result->error = NULL; s->result_free(result); }
            return out;
        }
        napi_value out = result->error != NULL ? throw_native(env, s, result->error, st) :
            result->json != NULL ? json_parse(env, result->json) : NULL;
        result->error = NULL; s->result_free(result); return out;
    }
    if (argc != 2) return bad(env, "createEndpoint expects address and options.");
    char *address = string(env, argv[0], "Address must be a string.");
    if (address == NULL) return NULL;
    char *options = json_string(env, argv[1], "Endpoint options must be an object.");
    if (options == NULL) { free(address); return NULL; }
    C2MemFfiEndpointContext *endpoint = NULL; C2MemFfiError *error = NULL;
    C2MemFfiStatus st = s->endpoint_new(owner->ptr, address, options, &endpoint, &error);
    free(address); free(options);
    if (st != C2_MEM_FFI_STATUS_OK || endpoint == NULL) return throw_native(env, s, error, st);
    napi_value object = owner_wrap(env, s, OWNER_ENDPOINT, endpoint);
    if (object == NULL || !method(env, object, "prepare", endpoint_method, (void *)1) ||
        !method(env, object, "beginCall", endpoint_method, (void *)2) ||
        !method(env, object, "deactivate", endpoint_method, (void *)3) ||
        !method(env, object, "close", endpoint_method, (void *)4)) return NULL;
    return object;
}
static napi_value endpoint_method(napi_env env, napi_callback_info info) {
    size_t argc = 4; napi_value argv[4], self; void *data = NULL;
    if (!args(env, info, &argc, argv, &self, &data)) return NULL;
    Owner *owner = owner_get(env, self, OWNER_ENDPOINT, true);
    if (owner == NULL) return NULL;
    Symbols *s = owner->s; C2MemFfiStatus st;
    if (data == (void *)3) {
        st = s->endpoint_deactivate(owner->ptr);
        if (st != C2_MEM_FFI_STATUS_OK) return throw_status(env, st);
        napi_value out; napi_get_undefined(env, &out); return out;
    }
    if (data == (void *)4) {
        napi_value promise; Async *operation = async_new(env, s, ASYNC_VOID, &promise);
        if (operation == NULL) return NULL;
        st = s->endpoint_close(owner->ptr, native_complete, operation);
        return async_status(env, operation, st, promise);
    }
    if (data == (void *)1) {
        if (argc != 2 && argc != 3) return bad(env, "prepare expects route name, contract and optional route token.");
        char *route = string(env, argv[0], "Route name must be a string.");
        if (route == NULL) return NULL;
        char *contract = json_string(env, argv[1], "Contract must be an object.");
        if (contract == NULL) { free(route); return NULL; }
        char *token = NULL;
        if (argc == 3 && !nullish(env, argv[2])) {
            token = json_string(env, argv[2], "Route token must be an object.");
            if (token == NULL) { free(route); free(contract); return NULL; }
        }
        napi_value promise; Async *operation = async_new(env, s, ASYNC_JSON, &promise);
        if (operation == NULL) { free(route); free(contract); free(token); return NULL; }
        st = s->endpoint_prepare(owner->ptr, route, contract, token, native_complete, operation);
        free(route); free(contract); free(token);
        return async_status(env, operation, st, promise);
    }
    if (argc != 4) return bad(env, "beginCall expects route, method, stream flag and timeout.");
    char *route = string(env, argv[0], "Route name must be a string.");
    if (route == NULL) return NULL;
    char *name = string(env, argv[1], "Method name must be a string.");
    if (name == NULL) { free(route); return NULL; }
    bool stream = false;
    if (!ok(napi_get_value_bool(env, argv[2], &stream))) { free(route); free(name); return bad(env, "Stream flag must be a boolean."); }
    uint64_t millis = 0; const uint64_t *limit = NULL;
    if (!timeout(env, argv[3], &millis, &limit)) { free(route); free(name); return NULL; }
    C2MemFfiCall *call = NULL; C2MemFfiError *error = NULL;
    st = s->call_begin(owner->ptr, route, name, stream ? 1 : 0, limit, &call, &error);
    free(route); free(name);
    if (st != C2_MEM_FFI_STATUS_OK || call == NULL) return throw_native(env, s, error, st);
    napi_value object = owner_wrap(env, s, OWNER_CALL, call);
    if (object == NULL || !method(env, object, "chargeInput", call_method, (void *)1) ||
        !method(env, object, "dispatch", call_method, (void *)2) ||
        !method(env, object, "next", call_method, (void *)3) ||
        !method(env, object, "cancel", call_method, (void *)4) ||
        !method(env, object, "observe", call_method, (void *)5) ||
        !method(env, object, "checkDeadline", call_method, (void *)6) ||
        !method(env, object, "close", call_method, (void *)7)) return NULL;
    return object;
}
static napi_value call_method(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1], self; void *data = NULL;
    if (!args(env, info, &argc, argv, &self, &data)) return NULL;
    bool closing = data == (void *)7;
    Owner *owner = owner_get(env, self, OWNER_CALL, !closing);
    if (owner == NULL) return NULL;
    Symbols *s = owner->s; C2MemFfiStatus st;
    if (closing) { owner_release(owner); napi_value out; napi_get_undefined(env, &out); return out; }
    if (data == (void *)4 || data == (void *)6) {
        if (data == (void *)4) {
            st = s->call_cancel(owner->ptr);
            if (st != C2_MEM_FFI_STATUS_OK) return throw_status(env, st);
        } else {
            C2MemFfiError *error = NULL;
            st = s->call_check_deadline(owner->ptr, &error);
            if (st != C2_MEM_FFI_STATUS_OK || error != NULL) return throw_native(env, s, error, st);
        }
        napi_value out; napi_get_undefined(env, &out); return out;
    }
    AsyncKind kind = data == (void *)3 ? ASYNC_NEXT : data == (void *)5 ? ASYNC_OBSERVE : ASYNC_VOID;
    napi_value promise; Async *operation;
    if (data == (void *)1) {
        uint64_t len = 0;
        if (argc != 1 || !uint64(env, argv[0], &len, "Input byte length must be a nonnegative integer.")) return NULL;
        operation = async_new(env, s, kind, &promise); if (operation == NULL) return NULL;
        st = s->call_charge(owner->ptr, len, native_complete, operation);
    } else if (data == (void *)2) {
        if (argc != 1) return bad(env, "dispatch expects a Uint8Array.");
        bool typed = false; napi_typedarray_type type; size_t len = 0, offset = 0;
        void *bytes = NULL; napi_value arraybuffer;
        if (!ok(napi_is_typedarray(env, argv[0], &typed)) || !typed ||
            !ok(napi_get_typedarray_info(env, argv[0], &type, &len, &bytes, &arraybuffer, &offset)) ||
            type != napi_uint8_array || (bytes == NULL && len != 0)) return bad(env, "dispatch expects a Uint8Array.");
        operation = async_new(env, s, kind, &promise); if (operation == NULL) return NULL;
        /* ABI 8 copies borrowed bytes before this function returns. */
        st = s->call_dispatch(owner->ptr, bytes, len, native_complete, operation);
    } else if (data == (void *)3) {
        operation = async_new(env, s, kind, &promise); if (operation == NULL) return NULL;
        st = s->call_next(owner->ptr, native_complete, operation);
    } else {
        uint64_t millis = 0; const uint64_t *limit = NULL;
        if (argc != 1 || !timeout(env, argv[0], &millis, &limit)) return NULL;
        operation = async_new(env, s, kind, &promise); if (operation == NULL) return NULL;
        st = s->call_observe(owner->ptr, limit, native_complete, operation);
    }
    return async_status(env, operation, st, promise);
}
static napi_value domain_cb(napi_env env, napi_callback_info info) {
    size_t argc = 2; napi_value argv[2]; Symbols *s = NULL;
    if (!args(env, info, &argc, argv, NULL, (void **)&s)) return NULL;
    if (argc != 2) return bad(env, "createClientDomain expects a context and factory options.");
    C2MemFfiLocalEndpointContext *context = NULL;
    if (argc > 0 && !nullish(env, argv[0])) {
        Owner *owner = owner_get(env, argv[0], OWNER_CONTEXT, true);
        if (owner == NULL) return NULL;
        if (owner->s != s) return bad(env, "Context belongs to another native library.");
        context = owner->ptr;
    }
    char *options = json_string(env, argv[1], "Factory options must be an object.");
    if (options == NULL) return NULL;
    C2MemFfiClientDomain *domain = NULL; C2MemFfiError *error = NULL;
    C2MemFfiStatus st = s->domain_new(context, options, &domain, &error);
    free(options);
    if (st != C2_MEM_FFI_STATUS_OK || domain == NULL) return throw_native(env, s, error, st);
    napi_value object = owner_wrap(env, s, OWNER_DOMAIN, domain);
    if (object == NULL || !method(env, object, "createEndpoint", domain_method, (void *)1) ||
        !method(env, object, "stats", domain_method, (void *)2) ||
        !method(env, object, "close", domain_method, (void *)3)) return NULL;
    return object;
}

static void close_library(void *library) {
#ifdef _WIN32
    FreeLibrary((HMODULE)library);
#else
    dlclose(library);
#endif
}
static void *open_library(const char *path, char *message, size_t size) {
#ifdef _WIN32
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (length == 0) { snprintf(message, size, "Invalid UTF-8 library path (Windows error %lu)", GetLastError()); return NULL; }
    wchar_t *wide = calloc((size_t)length, sizeof(wchar_t));
    if (wide == NULL) { snprintf(message, size, "Out of memory."); return NULL; }
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, length) == 0) {
        snprintf(message, size, "Invalid UTF-8 library path (Windows error %lu)", GetLastError()); free(wide); return NULL;
    }
    DWORD capacity = GetFullPathNameW(wide, 0, NULL, NULL);
    wchar_t *absolute = capacity == 0 ? NULL : calloc(capacity, sizeof(wchar_t));
    if (absolute == NULL) { snprintf(message, size, "Cannot resolve library path (Windows error %lu)", GetLastError()); free(wide); return NULL; }
    DWORD written = GetFullPathNameW(wide, capacity, absolute, NULL);
    free(wide);
    if (written == 0 || written >= capacity) { snprintf(message, size, "Cannot resolve library path (Windows error %lu)", GetLastError()); free(absolute); return NULL; }
    HMODULE library = LoadLibraryExW(absolute, NULL, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
    DWORD code = library == NULL ? GetLastError() : 0; free(absolute);
    if (library == NULL) snprintf(message, size, "Windows error %lu", code);
    return library;
#else
    void *library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) snprintf(message, size, "%s", dlerror());
    return library;
#endif
}
static bool load_symbol(napi_env env, void *library, const char *name, void **out) {
#ifdef _WIN32
    *out = (void *)GetProcAddress((HMODULE)library, name);
#else
    dlerror(); *out = dlsym(library, name);
#endif
    if (*out != NULL) return true;
    char message[512]; snprintf(message, sizeof(message), "Failed to load c2-mem-ffi symbol %s.", name);
    fail(env, message); return false;
}
static bool load_all(napi_env env, Symbols *s) {
#define LOAD_REQUIRED(field, name) if (!load_symbol(env, s->library, name, (void **)&s->field)) return false
    LOAD_REQUIRED(abi_version, "c2_mem_ffi_abi_version");
    uint32_t version = s->abi_version();
    if (version != C2_MEM_FFI_ABI_VERSION) {
        char message[128]; snprintf(message, sizeof(message), "c2_mem_ffi ABI version %u is incompatible with expected version %u.", version, C2_MEM_FFI_ABI_VERSION);
        fail(env, message); return false;
    }
    LOAD_REQUIRED(capture, "c2_mem_ffi_local_endpoint_context_capture");
    LOAD_REQUIRED(context_free, "c2_mem_ffi_local_endpoint_context_free");
    LOAD_REQUIRED(name_len, "c2_mem_ffi_local_endpoint_context_name_len");
    LOAD_REQUIRED(name_copy, "c2_mem_ffi_local_endpoint_context_name_copy");
    LOAD_REQUIRED(namespace_len, "c2_mem_ffi_local_endpoint_context_namespace_id_len");
    LOAD_REQUIRED(namespace_copy, "c2_mem_ffi_local_endpoint_context_namespace_id_copy");
    LOAD_REQUIRED(domain_new, "c2_mem_ffi_client_domain_new");
    LOAD_REQUIRED(domain_free, "c2_mem_ffi_client_domain_free");
    LOAD_REQUIRED(domain_stats, "c2_mem_ffi_client_domain_stats");
    LOAD_REQUIRED(endpoint_new, "c2_mem_ffi_endpoint_context_new");
    LOAD_REQUIRED(endpoint_prepare, "c2_mem_ffi_endpoint_context_prepare");
    LOAD_REQUIRED(endpoint_deactivate, "c2_mem_ffi_endpoint_context_deactivate");
    LOAD_REQUIRED(endpoint_close, "c2_mem_ffi_endpoint_context_close");
    LOAD_REQUIRED(endpoint_free, "c2_mem_ffi_endpoint_context_free");
    LOAD_REQUIRED(call_begin, "c2_mem_ffi_call_begin");
    LOAD_REQUIRED(call_charge, "c2_mem_ffi_call_charge_input");
    LOAD_REQUIRED(call_dispatch, "c2_mem_ffi_call_dispatch");
    LOAD_REQUIRED(call_next, "c2_mem_ffi_call_next");
    LOAD_REQUIRED(call_cancel, "c2_mem_ffi_call_cancel");
    LOAD_REQUIRED(call_observe, "c2_mem_ffi_call_observe");
    LOAD_REQUIRED(call_check_deadline, "c2_mem_ffi_call_check_deadline");
    LOAD_REQUIRED(call_free, "c2_mem_ffi_call_free");
    LOAD_REQUIRED(buffer_data, "c2_mem_ffi_buffer_data");
    LOAD_REQUIRED(buffer_release, "c2_mem_ffi_buffer_release");
    LOAD_REQUIRED(error_parts, "c2_mem_ffi_error_parts");
    LOAD_REQUIRED(error_free, "c2_mem_ffi_error_free");
    LOAD_REQUIRED(result_free, "c2_mem_ffi_result_free");
#undef LOAD_REQUIRED
    return true;
}
static napi_value load(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1];
    if (!args(env, info, &argc, argv, NULL, NULL) || argc != 1) return bad(env, "load expects a native library path.");
    char *path = string(env, argv[0], "Library path must be a string.");
    if (path == NULL) return NULL;
    char message[384] = {0}; void *library = open_library(path, message, sizeof(message)); free(path);
    if (library == NULL) { char full[512]; snprintf(full, sizeof(full), "Failed to load c2_mem_ffi shared library: %s", message); return fail(env, full); }
    Symbols *s = calloc(1, sizeof(*s));
    if (s == NULL) { close_library(library); return fail(env, "Out of memory."); }
    s->library = library;
    if (!load_all(env, s)) { close_library(library); free(s); return NULL; }
    napi_value object;
    if (!ok(napi_create_object(env, &object)) ||
        !method(env, object, "c2_mem_ffi_abi_version", version_cb, s) ||
        !method(env, object, "captureEndpointContext", capture_cb, s) ||
        !method(env, object, "createClientDomain", domain_cb, s)) {
        /* Keep the library pinned if an already-created JS method escaped. */
        return NULL;
    }
    return object;
}
static napi_value Init(napi_env env, napi_value exports) {
    if (!method(env, exports, "load", load, NULL)) return NULL;
    return exports;
}
NAPI_MODULE(c2_mem_ffi_node, Init)
