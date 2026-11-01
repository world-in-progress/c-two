#include <node_api.h>

#ifdef _WIN32
/* Types only: every uv_* function this addon uses is resolved at runtime from
 * the process module (node.exe's bundled libuv) so descriptor tables always
 * belong to the Node CRT. No uv symbol is linked directly. */
#include <uv.h>
#include <windows.h>
#else
#include <dlfcn.h>
#include <unistd.h>
#endif
#include <stdbool.h>
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "c2_mem_ffi.h"

static void close_library(void *library) {
#ifdef _WIN32
    FreeLibrary((HMODULE)library);
#else
    dlclose(library);
#endif
}

static void *open_library(const char *path, char *error, size_t error_size) {
#ifdef _WIN32
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (length == 0) {
        snprintf(error, error_size, "Invalid UTF-8 library path (Windows error %lu)", GetLastError());
        return NULL;
    }
    wchar_t *wide = (wchar_t *)calloc((size_t)length, sizeof(wchar_t));
    if (wide == NULL) {
        snprintf(error, error_size, "Out of memory.");
        return NULL;
    }
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, length) == 0) {
        snprintf(error, error_size, "Invalid UTF-8 library path (Windows error %lu)", GetLastError());
        free(wide);
        return NULL;
    }
    DWORD capacity = GetFullPathNameW(wide, 0, NULL, NULL);
    wchar_t *absolute = capacity == 0 ? NULL : (wchar_t *)calloc(capacity, sizeof(wchar_t));
    if (absolute == NULL) {
        snprintf(error, error_size, "Cannot resolve library path (Windows error %lu)", GetLastError());
        free(wide);
        return NULL;
    }
    DWORD written = GetFullPathNameW(wide, capacity, absolute, NULL);
    free(wide);
    if (written == 0 || written >= capacity) {
        snprintf(error, error_size, "Cannot resolve library path (Windows error %lu)", GetLastError());
        free(absolute);
        return NULL;
    }
    // Restrict dependency lookup to this library's directory and system defaults.
    HMODULE library = LoadLibraryExW(absolute, NULL,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
    DWORD code = library == NULL ? GetLastError() : 0;
    free(absolute);
    if (library == NULL) {
        snprintf(error, error_size, "Windows error %lu", code);
    }
    return (void *)library;
#else
    void *library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) {
        snprintf(error, error_size, "%s", dlerror());
    }
    return library;
#endif
}

typedef C2MemFfiStatus (*memory_context_new_fn)(uintptr_t connection, uintptr_t process, C2MemFfiMemoryContext **out);
typedef void (*memory_context_free_fn)(C2MemFfiMemoryContext *context);
typedef C2MemFfiStatus (*memory_context_prefix_len_fn)(const C2MemFfiMemoryContext *context, size_t *out);
typedef C2MemFfiStatus (*memory_context_prefix_copy_fn)(const C2MemFfiMemoryContext *context, char *dst, size_t len, size_t *out);
typedef C2MemFfiStatus (*memory_context_finish_handshake_fn)(const C2MemFfiMemoryContext *context, const char *prefix);
typedef C2MemFfiStatus (*request_prepare_fn)(const C2MemFfiMemoryContext *context, const uint8_t *data, size_t len, C2MemFfiRequest **out, C2MemFfiBlock *block);
typedef void (*request_discard_fn)(C2MemFfiRequest *request);
typedef C2MemFfiStatus (*response_admit_fn)(const C2MemFfiMemoryContext *context, C2MemFfiBlock block, C2MemFfiResponse **out);
typedef C2MemFfiStatus (*response_consume_fn)(C2MemFfiResponse *response, uint8_t *dst, size_t len, size_t *out);
typedef void (*response_discard_fn)(C2MemFfiResponse *response);
typedef C2MemFfiStatus (*memory_context_receive_release_fn)(const C2MemFfiMemoryContext *context, const uint8_t *bytes, size_t len);
typedef C2MemFfiStatus (*write_begin_fn)(const C2MemFfiMemoryContext *context, C2MemFfiRequest *request, uint8_t kind, C2MemFfiWrite **out);
typedef C2MemFfiStatus (*write_prefix_len_fn)(const C2MemFfiWrite *write, size_t *out);
typedef C2MemFfiStatus (*write_prefix_copy_fn)(const C2MemFfiWrite *write, uint8_t *dst, size_t len);
typedef C2MemFfiStatus (*write_start_fn)(C2MemFfiWrite *write);
typedef C2MemFfiStatus (*write_complete_fn)(C2MemFfiWrite *write, uint8_t success);
typedef void (*write_discard_fn)(C2MemFfiWrite *write);
typedef C2MemFfiStatus (*memory_context_deactivate_fn)(const C2MemFfiMemoryContext *context);
typedef C2MemFfiStatus (*memory_context_peer_end_fn)(const C2MemFfiMemoryContext *context);
typedef C2MemFfiStatus (*memory_context_try_close_fn)(const C2MemFfiMemoryContext *context, uint8_t *out);
typedef uint32_t (*abi_version_fn)(void);
typedef C2MemFfiStatus (*endpoint_context_capture_fn)(const char *, C2MemFfiLocalEndpointContext **);
typedef void (*endpoint_context_free_fn)(C2MemFfiLocalEndpointContext *);
typedef C2MemFfiStatus (*endpoint_context_name_len_fn)(const C2MemFfiLocalEndpointContext *, const char *, size_t *);
typedef C2MemFfiStatus (*endpoint_context_name_copy_fn)(const C2MemFfiLocalEndpointContext *, const char *, char *, size_t, size_t *);
typedef C2MemFfiStatus (*endpoint_context_namespace_id_len_fn)(const C2MemFfiLocalEndpointContext *, size_t *);
typedef C2MemFfiStatus (*endpoint_context_namespace_id_copy_fn)(const C2MemFfiLocalEndpointContext *, char *, size_t, size_t *);
typedef C2MemFfiStatus (*endpoint_context_connect_unix_fn)(const C2MemFfiLocalEndpointContext *, const char *, int32_t *, int32_t *);
typedef C2MemFfiStatus (*endpoint_context_connect_windows_fn)(const C2MemFfiLocalEndpointContext *, const char *, uintptr_t *, uintptr_t *, int32_t *);

typedef struct C2MemFfiNodeSymbols {
    void *library;
    abi_version_fn abi_version;
    endpoint_context_capture_fn endpoint_context_capture;
    endpoint_context_free_fn endpoint_context_free;
    endpoint_context_name_len_fn endpoint_context_name_len;
    endpoint_context_name_copy_fn endpoint_context_name_copy;
    endpoint_context_namespace_id_len_fn endpoint_context_namespace_id_len;
    endpoint_context_namespace_id_copy_fn endpoint_context_namespace_id_copy;
    endpoint_context_connect_unix_fn endpoint_context_connect_unix;
    endpoint_context_connect_windows_fn endpoint_context_connect_windows;
    memory_context_new_fn memory_context_new;
    memory_context_free_fn memory_context_free;
    memory_context_prefix_len_fn memory_context_prefix_len;
    memory_context_prefix_copy_fn memory_context_prefix_copy;
    memory_context_finish_handshake_fn memory_context_finish_handshake;
    request_prepare_fn request_prepare;
    request_discard_fn request_discard;
    response_admit_fn response_admit;
    response_consume_fn response_consume;
    response_discard_fn response_discard;
    memory_context_receive_release_fn memory_context_receive_release;
    write_begin_fn write_begin;
    write_prefix_len_fn write_prefix_len;
    write_prefix_copy_fn write_prefix_copy;
    write_start_fn write_start;
    write_complete_fn write_complete;
    write_discard_fn write_discard;
    memory_context_deactivate_fn memory_context_deactivate;
    memory_context_peer_end_fn memory_context_peer_end;
    memory_context_try_close_fn memory_context_try_close;
} C2MemFfiNodeSymbols;

typedef struct C2MemFfiNodeEndpointContext {
    C2MemFfiNodeSymbols *symbols;
    C2MemFfiLocalEndpointContext *ptr;
    size_t active_connects;
    bool closed;
    bool finalized;
} C2MemFfiNodeEndpointContext;

/* Only wrapped objects created by this addon can carry a context owner.
 * ABI 7 rotates the tag: a stale addon's
 * endpoint contexts must fail the tag check instead of being unwrapped. */
static const napi_type_tag endpoint_context_tag = {
    UINT64_C(0x5a71e0c3f28b94d7), UINT64_C(0xd6340f81b9e57c2a)
};

static napi_value throw_error(napi_env env, const char *message) {
    napi_throw_error(env, NULL, message);
    return NULL;
}

static napi_value throw_type_error(napi_env env, const char *message) {
    napi_throw_type_error(env, NULL, message);
    return NULL;
}

static bool check_napi(napi_status status) {
    return status == napi_ok;
}

static bool set_named_value(napi_env env, napi_value object, const char *name, napi_value value) {
    return check_napi(napi_set_named_property(env, object, name, value));
}

static bool set_named_uint32(napi_env env, napi_value object, const char *name, uint32_t value) {
    napi_value js_value;
    if (!check_napi(napi_create_uint32(env, value, &js_value))) {
        return false;
    }
    return set_named_value(env, object, name, js_value);
}

static bool set_named_uint64_bigint(napi_env env, napi_value object, const char *name, uint64_t value) {
    napi_value js_value;
    return check_napi(napi_create_bigint_uint64(env, value, &js_value)) &&
        set_named_value(env, object, name, js_value);
}

static bool set_named_bool(napi_env env, napi_value object, const char *name, bool value) {
    napi_value js_value;
    if (!check_napi(napi_get_boolean(env, value, &js_value))) {
        return false;
    }
    return set_named_value(env, object, name, js_value);
}

static napi_value make_status_result(napi_env env, C2MemFfiStatus status, napi_value value) {
    napi_value result;
    if (!check_napi(napi_create_object(env, &result))) {
        return NULL;
    }
    if (!set_named_uint32(env, result, "status", (uint32_t)status)) {
        return NULL;
    }
    if (value != NULL && status == C2_MEM_FFI_STATUS_OK) {
        if (!set_named_value(env, result, "value", value)) {
            return NULL;
        }
    }
    return result;
}

static napi_value make_status_number_result(napi_env env, C2MemFfiStatus status, double value) {
    napi_value js_value = NULL;
    if (status == C2_MEM_FFI_STATUS_OK && !check_napi(napi_create_double(env, value, &js_value))) {
        return NULL;
    }
    return make_status_result(env, status, js_value);
}

static char *read_string_arg(napi_env env, napi_value value, const char *label) {
    size_t length = 0;
    if (!check_napi(napi_get_value_string_utf8(env, value, NULL, 0, &length))) {
        throw_type_error(env, label);
        return NULL;
    }
    char *buffer = (char *)malloc(length + 1);
    if (buffer == NULL) {
        throw_error(env, "Out of memory.");
        return NULL;
    }
    size_t written = 0;
    if (!check_napi(napi_get_value_string_utf8(env, value, buffer, length + 1, &written))) {
        free(buffer);
        throw_type_error(env, label);
        return NULL;
    }
    buffer[written] = '\0';
    if (memchr(buffer, '\0', written) != NULL) {
        free(buffer);
        throw_type_error(env, "Native string arguments cannot contain NUL characters.");
        return NULL;
    }
    return buffer;
}

static bool read_uint32_arg(napi_env env, napi_value value, const char *label, uint32_t *out) {
    double numeric = 0.0;
    if (!check_napi(napi_get_value_double(env, value, &numeric))) {
        throw_type_error(env, label);
        return false;
    }
    if (numeric != numeric ||
        numeric < 0.0 ||
        numeric > (double)UINT32_MAX) {
        throw_type_error(env, label);
        return false;
    }
    uint32_t narrowed = (uint32_t)numeric;
    if ((double)narrowed != numeric) {
        throw_type_error(env, label);
        return false;
    }
    *out = narrowed;
    return true;
}

static bool get_named_property(napi_env env, napi_value object, const char *name, napi_value *out) {
    if (!check_napi(napi_get_named_property(env, object, name, out))) {
        char message[160];
        snprintf(message, sizeof(message), "Expected c2-mem-ffi block property %s.", name);
        throw_type_error(env, message);
        return false;
    }
    return true;
}

static bool read_block_common(
    napi_env env,
    napi_value value,
    uint16_t *segment_index,
    uint8_t *is_dedicated,
    uint32_t *generation,
    uint32_t *offset,
    uint32_t *byte_length,
    uint64_t *loan_id) {
    napi_value property;
    uint32_t temp_u32 = 0;
    bool temp_bool = false;
    if (!get_named_property(env, value, "segmentIndex", &property) ||
        !read_uint32_arg(env, property, "block.segmentIndex must be a u32 integer.", &temp_u32)) {
        return false;
    }
    if (temp_u32 > UINT16_MAX) {
        throw_type_error(env, "block.segmentIndex must fit in u16.");
        return false;
    }
    *segment_index = (uint16_t)temp_u32;
    if (!get_named_property(env, value, "dedicated", &property) ||
        !check_napi(napi_get_value_bool(env, property, &temp_bool))) {
        throw_type_error(env, "block.dedicated must be a boolean.");
        return false;
    }
    *is_dedicated = temp_bool ? 1 : 0;
    if (!get_named_property(env, value, "generation", &property) ||
        !read_uint32_arg(env, property, "block.generation must be a u32 integer.", generation)) {
        return false;
    }
    if (!get_named_property(env, value, "offset", &property) ||
        !read_uint32_arg(env, property, "block.offset must be a u32 integer.", offset)) {
        return false;
    }
    if (!get_named_property(env, value, "byteLength", &property) ||
        !read_uint32_arg(env, property, "block.byteLength must be a u32 integer.", byte_length)) {
        return false;
    }
    bool lossless = false;
    if (!get_named_property(env, value, "loanId", &property) ||
        !check_napi(napi_get_value_bigint_uint64(env, property, loan_id, &lossless)) ||
        !lossless) {
        throw_type_error(env, "block.loanId must be a lossless u64 bigint.");
        return false;
    }
    return true;
}

static bool read_block(napi_env env, napi_value value, C2MemFfiBlock *block) {
    memset(block, 0, sizeof(*block));
    const char *fields[] = { "segmentIndex", "generation", "offset", "byteLength", "loanId", "dedicated" };
    napi_value keys;
    uint32_t length = 0;
    if (!check_napi(napi_get_all_property_names(env, value, napi_key_own_only,
            napi_key_all_properties, napi_key_numbers_to_strings, &keys)) ||
        !check_napi(napi_get_array_length(env, keys, &length))) return false;
    for (uint32_t i = 0; i < length; i++) {
        napi_value key;
        napi_valuetype type;
        char name[32];
        size_t written = 0;
        bool known = false;
        if (!check_napi(napi_get_element(env, keys, i, &key)) ||
            !check_napi(napi_typeof(env, key, &type))) return false;
        if (type == napi_string && check_napi(napi_get_value_string_utf8(env, key, name, sizeof(name), &written))) {
            for (size_t j = 0; j < sizeof(fields) / sizeof(fields[0]); j++) {
                if (written == strlen(fields[j]) && memcmp(name, fields[j], written) == 0) { known = true; break; }
            }
        }
        if (!known) { throw_type_error(env, "Unknown block fields."); return false; }
    }
    return read_block_common(env, value, &block->segment_index, &block->is_dedicated,
        &block->generation, &block->offset, &block->byte_length, &block->loan_id);
}
static napi_value make_block(napi_env env, C2MemFfiBlock block) {
    napi_value object;
    if (!check_napi(napi_create_object(env, &object)) ||
        !set_named_uint32(env, object, "segmentIndex", block.segment_index) ||
        !set_named_uint32(env, object, "generation", block.generation) ||
        !set_named_uint32(env, object, "offset", block.offset) ||
        !set_named_uint32(env, object, "byteLength", block.byte_length) ||
        !set_named_uint64_bigint(env, object, "loanId", block.loan_id) ||
        !set_named_bool(env, object, "dedicated", block.is_dedicated != 0)) return NULL;
    return object;
}

static bool get_uint8_array(
    napi_env env,
    napi_value value,
    const char *label,
    uint8_t **out_data,
    size_t *out_length) {
    bool is_typed_array = false;
    if (!check_napi(napi_is_typedarray(env, value, &is_typed_array)) || !is_typed_array) {
        throw_type_error(env, label);
        return false;
    }
    napi_typedarray_type type;
    size_t length = 0;
    void *data = NULL;
    napi_value arraybuffer;
    size_t byte_offset = 0;
    if (!check_napi(napi_get_typedarray_info(env, value, &type, &length, &data, &arraybuffer, &byte_offset)) ||
        type != napi_uint8_array ||
        (data == NULL && length != 0)) {
        throw_type_error(env, label);
        return false;
    }
    *out_data = (uint8_t *)data;
    *out_length = length;
    return true;
}

static napi_value abi_version_callback(napi_env env, napi_callback_info info) {
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, NULL, NULL, NULL, (void **)&symbols))) return NULL;
    napi_value value;
    if (!check_napi(napi_create_uint32(env, symbols->abi_version(), &value))) return NULL;
    return value;
}

enum MemoryKind { MEMORY_CONTEXT, MEMORY_REQUEST, MEMORY_RESPONSE, MEMORY_WRITE };
typedef struct MemoryOwner {
    C2MemFfiNodeSymbols *symbols;
    enum MemoryKind kind;
    void *ptr;
} MemoryOwner;
static const napi_type_tag memory_tag = { UINT64_C(0xb178382913ac77af), UINT64_C(0x17d088f27bcb0700) };
static void memory_release(MemoryOwner *owner) {
    if (owner->ptr == NULL) return;
    switch (owner->kind) {
    case MEMORY_CONTEXT: owner->symbols->memory_context_free(owner->ptr); break;
    case MEMORY_REQUEST: owner->symbols->request_discard(owner->ptr); break;
    case MEMORY_RESPONSE: owner->symbols->response_discard(owner->ptr); break;
    case MEMORY_WRITE: owner->symbols->write_discard(owner->ptr); break;
    }
    owner->ptr = NULL;
}
static void memory_finalize(napi_env env, void *data, void *hint) {
    (void)env; (void)hint;
    MemoryOwner *owner = data;
    memory_release(owner);
    free(owner);
}
static MemoryOwner *memory_owner(napi_env env, napi_value value, enum MemoryKind kind, bool live) {
    bool tagged = false;
    void *raw = NULL;
    if (!check_napi(napi_check_object_type_tag(env, value, &memory_tag, &tagged)) || !tagged ||
        !check_napi(napi_unwrap(env, value, &raw)) || raw == NULL) {
        throw_type_error(env, "Expected an opaque ABI 7 memory owner."); return NULL;
    }
    MemoryOwner *owner = raw;
    if (owner->kind != kind || (live && owner->ptr == NULL)) {
        throw_type_error(env, "ABI 7 memory owner is consumed or has a different kind."); return NULL;
    }
    return owner;
}
static napi_value memory_wrap(napi_env env, C2MemFfiNodeSymbols *symbols, enum MemoryKind kind, void *ptr) {
    MemoryOwner *owner = malloc(sizeof(*owner));
    if (owner == NULL) {
        MemoryOwner temporary = { symbols, kind, ptr }; memory_release(&temporary);
        return throw_error(env, "Out of memory.");
    }
    *owner = (MemoryOwner){ symbols, kind, ptr };
    napi_value object;
    if (!check_napi(napi_create_object(env, &object)) || !check_napi(napi_type_tag_object(env, object, &memory_tag)) ||
        !check_napi(napi_wrap(env, object, owner, memory_finalize, NULL, NULL))) {
        memory_finalize(env, owner, NULL); return NULL;
    }
    return object;
}
static napi_value memory_bytes(napi_env env, size_t len, uint8_t **data) {
    napi_value buffer, view;
    if (!check_napi(napi_create_arraybuffer(env, len, (void **)data, &buffer)) ||
        !check_napi(napi_create_typedarray(env, napi_uint8_array, len, buffer, 0, &view))) return NULL;
    return view;
}
enum MemoryOp { M_PREFIX, M_HANDSHAKE, M_PREPARE, M_ADMIT, M_CONSUME, M_DISCARD_RESPONSE,
    M_BEGIN, M_START, M_COMPLETE, M_RECEIVE, M_DEACTIVATE, M_PEER_END, M_TRY_CLOSE, M_CLOSE, M_DISCARD_REQUEST, M_DISCARD_WRITE };
static napi_value memory_call(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2], receiver;
    void *operation = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, &receiver, &operation))) return NULL;
    enum MemoryOp op = (enum MemoryOp)(uintptr_t)operation;
    bool needs_context = op != M_CLOSE && op != M_CONSUME && op != M_DISCARD_RESPONSE &&
        op != M_DISCARD_REQUEST && op != M_COMPLETE && op != M_DISCARD_WRITE;
    MemoryOwner *owner = memory_owner(env, receiver, MEMORY_CONTEXT, needs_context);
    if (owner == NULL) return NULL;
    C2MemFfiNodeSymbols *s = owner->symbols;
    C2MemFfiMemoryContext *ctx = owner->ptr;
    C2MemFfiStatus status = C2_MEM_FFI_STATUS_OK;
    napi_value value = NULL;
    MemoryOwner *created = NULL;
    switch (op) {
    case M_PREFIX: {
        size_t len = 0, written = 0;
        status = s->memory_context_prefix_len(ctx, &len);
        if (status != C2_MEM_FFI_STATUS_OK) break;
        char *buffer = malloc(len + 1);
        if (buffer == NULL) return throw_error(env, "Out of memory.");
        status = s->memory_context_prefix_copy(ctx, buffer, len + 1, &written);
        if (status == C2_MEM_FFI_STATUS_OK && !check_napi(napi_create_string_utf8(env, buffer, written, &value))) value = NULL;
        free(buffer); break;
    }
    case M_HANDSHAKE: {
        if (argc != 1) return throw_type_error(env, "finishHandshake expects a prefix or null.");
        napi_valuetype type;
        if (!check_napi(napi_typeof(env, args[0], &type))) return NULL;
        char *prefix = type == napi_null ? NULL : read_string_arg(env, args[0], "Expected response prefix.");
        if (type != napi_null && prefix == NULL) return NULL;
        status = s->memory_context_finish_handshake(ctx, prefix);
        free(prefix); break;
    }
    case M_PREPARE: {
        uint8_t *data = NULL; size_t len = 0; C2MemFfiRequest *request = NULL; C2MemFfiBlock block;
        if (argc != 1 || !get_uint8_array(env, args[0], "Expected request bytes.", &data, &len)) return NULL;
        status = s->request_prepare(ctx, data, len, &request, &block);
        if (status == C2_MEM_FFI_STATUS_OK) {
            value = memory_wrap(env, s, MEMORY_REQUEST, request);
            if (value == NULL) return NULL;
            created = memory_owner(env, value, MEMORY_REQUEST, true);
            napi_value descriptor = make_block(env, block);
            if (descriptor == NULL || !set_named_value(env, value, "block", descriptor)) { memory_release(created); return NULL; }
        }
        break;
    }
    case M_ADMIT: {
        C2MemFfiBlock block; C2MemFfiResponse *response = NULL;
        if (argc != 1 || !read_block(env, args[0], &block)) {
            s->memory_context_deactivate(ctx);
            return NULL;
        }
        status = s->response_admit(ctx, block, &response);
        if (status == C2_MEM_FFI_STATUS_OK) {
            value = memory_wrap(env, s, MEMORY_RESPONSE, response);
            if (value == NULL) return NULL;
            created = memory_owner(env, value, MEMORY_RESPONSE, true);
        }
        break;
    }
    case M_CONSUME: {
        if (argc != 2) return throw_type_error(env, "Expected owner and destination.");
        MemoryOwner *item = memory_owner(env, args[0], MEMORY_RESPONSE, true);
        if (item == NULL || item->symbols != s) return NULL;
        uint8_t *dst = NULL; size_t len = 0, written = 0;
        if (!get_uint8_array(env, args[1], "Expected destination bytes.", &dst, &len)) {
            memory_release(item);
            return NULL;
        }
        C2MemFfiResponse *response = item->ptr; item->ptr = NULL;
        status = s->response_consume(response, dst, len, &written);
        return make_status_number_result(env, status, (double)written);
    }
    case M_DISCARD_RESPONSE: case M_DISCARD_REQUEST: case M_DISCARD_WRITE: {
        if (argc != 1) return throw_type_error(env, "Expected memory owner.");
        MemoryOwner *item = memory_owner(env, args[0], op == M_DISCARD_RESPONSE ? MEMORY_RESPONSE : op == M_DISCARD_WRITE ? MEMORY_WRITE : MEMORY_REQUEST, false);
        if (item == NULL || item->symbols != s) return NULL;
        memory_release(item); break;
    }
    case M_BEGIN: {
        if (argc < 1) return throw_type_error(env, "beginWrite expects kind and optional request.");
        uint32_t kind = 0;
        if (!read_uint32_arg(env, args[0], "Expected write kind.", &kind) || kind > 2) return NULL;
        C2MemFfiRequest *request = NULL;
        if (argc == 2) {
            MemoryOwner *item = memory_owner(env, args[1], MEMORY_REQUEST, true);
            if (item == NULL || item->symbols != s) return NULL;
            request = item->ptr; item->ptr = NULL;
        }
        C2MemFfiWrite *write = NULL;
        status = s->write_begin(ctx, request, (uint8_t)kind, &write);
        if (status == C2_MEM_FFI_STATUS_OK && write != NULL) {
            value = memory_wrap(env, s, MEMORY_WRITE, write);
            if (value == NULL) return NULL;
            created = memory_owner(env, value, MEMORY_WRITE, true);
            size_t len = 0; uint8_t *data = NULL;
            status = s->write_prefix_len(write, &len);
            napi_value prefix = status == C2_MEM_FFI_STATUS_OK ? memory_bytes(env, len, &data) : NULL;
            if (prefix == NULL) { memory_release(created); return NULL; }
            status = s->write_prefix_copy(write, data, len);
            if (!set_named_value(env, value, "prefix", prefix)) { memory_release(created); return NULL; }
        }
        break;
    }
    case M_START: case M_COMPLETE: {
        if (argc < 1) return throw_type_error(env, "Expected write owner.");
        MemoryOwner *item = memory_owner(env, args[0], MEMORY_WRITE, true);
        if (item == NULL || item->symbols != s) return NULL;
        if (op == M_START) status = s->write_start(item->ptr);
        else {
            bool success = false;
            if (argc != 2 || !check_napi(napi_get_value_bool(env, args[1], &success))) return throw_type_error(env, "Expected write outcome.");
            C2MemFfiWrite *write = item->ptr; item->ptr = NULL;
            status = s->write_complete(write, success ? 1 : 0);
        }
        break;
    }
    case M_RECEIVE: {
        uint8_t *data = NULL; size_t len = 0;
        if (argc != 1 || !get_uint8_array(env, args[0], "Expected release frame.", &data, &len)) return NULL;
        status = s->memory_context_receive_release(ctx, data, len); break;
    }
    case M_DEACTIVATE: status = s->memory_context_deactivate(ctx); break;
    case M_PEER_END: status = s->memory_context_peer_end(ctx); break;
    case M_TRY_CLOSE: {
        uint8_t closed = 0;
        status = s->memory_context_try_close(ctx, &closed);
        if (!check_napi(napi_get_boolean(env, closed != 0, &value))) return NULL;
        break;
    }
    case M_CLOSE: memory_release(owner); break;
    }
    napi_value result = make_status_result(env, status, value);
    if (created != NULL && (result == NULL || status != C2_MEM_FFI_STATUS_OK)) memory_release(created);
    return result;
}
static napi_value make_memory(napi_env env, C2MemFfiNodeSymbols *symbols, uintptr_t connection, uintptr_t process) {
    C2MemFfiMemoryContext *ctx = NULL;
    C2MemFfiStatus status = symbols->memory_context_new(connection, process, &ctx);
    if (status != C2_MEM_FFI_STATUS_OK) return throw_error(env, "Failed to bind memory to the verified connection.");
    napi_value object = memory_wrap(env, symbols, MEMORY_CONTEXT, ctx);
    if (object == NULL) return NULL;
#define METHOD(name, op) { name, NULL, memory_call, NULL, NULL, NULL, napi_default, (void *)(uintptr_t)op }
    napi_property_descriptor methods[] = {
        METHOD("prefix", M_PREFIX), METHOD("finishHandshake", M_HANDSHAKE), METHOD("prepareRequest", M_PREPARE),
        METHOD("admitResponse", M_ADMIT), METHOD("consumeResponse", M_CONSUME), METHOD("discardResponse", M_DISCARD_RESPONSE),
        METHOD("beginWrite", M_BEGIN), METHOD("startWrite", M_START), METHOD("completeWrite", M_COMPLETE),
        METHOD("receiveRelease", M_RECEIVE), METHOD("deactivate", M_DEACTIVATE), METHOD("peerEnd", M_PEER_END),
        METHOD("tryClose", M_TRY_CLOSE), METHOD("close", M_CLOSE), METHOD("discardRequest", M_DISCARD_REQUEST), METHOD("discardWrite", M_DISCARD_WRITE),
    };
#undef METHOD
    if (!check_napi(napi_define_properties(env, object, sizeof(methods) / sizeof(methods[0]), methods))) {
        memory_release(memory_owner(env, object, MEMORY_CONTEXT, true)); return NULL;
    }
    return object;
}

/* A null address selects the namespace id query on the same captured context. */
static napi_value endpoint_string_result(
    napi_env env,
    C2MemFfiNodeSymbols *symbols,
    const C2MemFfiLocalEndpointContext *context,
    const char *address) {
    size_t length = 0;
    C2MemFfiStatus status;
    if (address != NULL) {
        status = symbols->endpoint_context_name_len(context, address, &length);
    } else {
        status = symbols->endpoint_context_namespace_id_len(context, &length);
    }
    if (status != C2_MEM_FFI_STATUS_OK) {
        return make_status_result(env, status, NULL);
    }
    if (length == SIZE_MAX) {
        return throw_error(env, "IPC endpoint name is too large.");
    }
    char *buffer = (char *)malloc(length + 1);
    if (buffer == NULL) {
        return throw_error(env, "Out of memory.");
    }
    size_t written = 0;
    if (address != NULL) {
        status = symbols->endpoint_context_name_copy(context, address, buffer, length + 1, &written);
    } else {
        status = symbols->endpoint_context_namespace_id_copy(context, buffer, length + 1, &written);
    }
    napi_value value = NULL;
    if (status == C2_MEM_FFI_STATUS_OK && (written > length || buffer[written] != '\0')) {
        free(buffer);
        return throw_error(env, "Invalid native IPC endpoint string result.");
    }
    if (status == C2_MEM_FFI_STATUS_OK && !check_napi(napi_create_string_utf8(env, buffer, written, &value))) {
        free(buffer);
        return NULL;
    }
    free(buffer);
    return make_status_result(env, status, value);
}

static napi_value local_endpoint(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, (void **)&symbols)) || argc < 1) {
        return throw_type_error(env, "c2_mem_ffi_local_endpoint expects an ipc:// address.");
    }
    char *address = read_string_arg(env, args[0], "IPC address must be a string.");
    if (address == NULL) {
        return NULL;
    }
    C2MemFfiLocalEndpointContext *context = NULL;
    C2MemFfiStatus status = symbols->endpoint_context_capture(NULL, &context);
    if (status != C2_MEM_FFI_STATUS_OK || context == NULL) {
        free(address);
        if (context != NULL) symbols->endpoint_context_free(context);
        if (status == C2_MEM_FFI_STATUS_OK) {
            return throw_error(env, "Native IPC endpoint capture returned no context.");
        }
        return make_status_result(env, status, NULL);
    }
    napi_value result = endpoint_string_result(env, symbols, context, address);
    symbols->endpoint_context_free(context);
    free(address);
    return result;
}

static void endpoint_context_release_closed(C2MemFfiNodeEndpointContext *context) {
    if (!context->closed || context->active_connects != 0) return;
    if (context->ptr != NULL) {
        context->symbols->endpoint_context_free(context->ptr);
        context->ptr = NULL;
    }
    if (context->finalized) free(context);
}

static void endpoint_context_finalize(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    C2MemFfiNodeEndpointContext *context = (C2MemFfiNodeEndpointContext *)data;
    context->closed = true;
    context->finalized = true;
    endpoint_context_release_closed(context);
}

static C2MemFfiNodeEndpointContext *endpoint_context_receiver(
    napi_env env, napi_value receiver, bool require_open) {
    bool tagged = false;
    void *raw = NULL;
    if (!check_napi(napi_check_object_type_tag(env, receiver, &endpoint_context_tag, &tagged)) ||
        !tagged || !check_napi(napi_unwrap(env, receiver, &raw)) || raw == NULL) {
        throw_type_error(env, "Expected a c2-mem-ffi native endpoint context receiver.");
        return NULL;
    }
    C2MemFfiNodeEndpointContext *context = (C2MemFfiNodeEndpointContext *)raw;
    if (require_open && (context->closed || context->ptr == NULL)) {
        throw_type_error(env, "c2-mem-ffi native endpoint context is closed.");
        return NULL;
    }
    return context;
}

static napi_value endpoint_context_name(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1], receiver;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, &receiver, NULL)) || argc < 1) {
        return throw_type_error(env, "endpointName expects an ipc:// address.");
    }
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, receiver, true);
    if (context == NULL) return NULL;
    char *address = read_string_arg(env, args[0], "IPC address must be a string.");
    if (address == NULL) return NULL;
    napi_value result = endpoint_string_result(env, context->symbols, context->ptr, address);
    free(address);
    return result;
}

static napi_value endpoint_context_namespace_id(napi_env env, napi_callback_info info) {
    napi_value receiver;
    if (!check_napi(napi_get_cb_info(env, info, NULL, NULL, &receiver, NULL))) return NULL;
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, receiver, true);
    if (context == NULL) return NULL;
    return endpoint_string_result(env, context->symbols, context->ptr, NULL);
}

static napi_value endpoint_context_close(napi_env env, napi_callback_info info) {
    napi_value receiver;
    if (!check_napi(napi_get_cb_info(env, info, NULL, NULL, &receiver, NULL))) return NULL;
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, receiver, false);
    if (context == NULL) return NULL;
    context->closed = true;
    endpoint_context_release_closed(context);
    napi_value result;
    if (!check_napi(napi_get_undefined(env, &result))) return NULL;
    return result;
}

/* A descriptor remains owned until explicitly taken by Node's Socket. An
 * ignored async result is closed by GC; every failed handoff closes it here. */
typedef struct NativeSocketFd { int32_t fd; C2MemFfiNodeSymbols *symbols; bool memory_taken; } NativeSocketFd;
static const napi_type_tag socket_fd_tag = {
    UINT64_C(0xb82cd6be1d234b9a), UINT64_C(0x9b4e465211132206)
};

#ifdef _WIN32
/* Descriptors handed to Node must live in the Node CRT's descriptor table.
 * node.exe exports its bundled libuv's wrappers (uv_open_osfhandle and
 * friends), so this addon resolves them from the process module instead of
 * calling _open_osfhandle from another DLL's CRT or treating a HANDLE as an
 * fd. Verified against official Node v22.23.4 win-x64 exports and the vendored
 * libuv sources (win/handle.c, win/fs.c, win/pipe.c). */
typedef int (*c2_uv_open_osfhandle_fn)(uv_os_fd_t);
typedef int (*c2_uv_fs_close_fn)(uv_loop_t *, uv_fs_t *, uv_file, uv_fs_cb);
typedef void (*c2_uv_fs_req_cleanup_fn)(uv_fs_t *);

static c2_uv_open_osfhandle_fn c2_uv_open_osfhandle;
static c2_uv_fs_close_fn c2_uv_fs_close;
static c2_uv_fs_req_cleanup_fn c2_uv_fs_req_cleanup;
static INIT_ONCE process_libuv_once = INIT_ONCE_STATIC_INIT;

static BOOL CALLBACK initialize_process_libuv(PINIT_ONCE once, PVOID parameter, PVOID *context) {
    (void)once; (void)parameter; (void)context;
    HMODULE process_module = GetModuleHandleW(NULL);
    if (process_module == NULL) {
        return TRUE;
    }
    c2_uv_open_osfhandle = (c2_uv_open_osfhandle_fn)(void *)GetProcAddress(process_module, "uv_open_osfhandle");
    c2_uv_fs_close = (c2_uv_fs_close_fn)(void *)GetProcAddress(process_module, "uv_fs_close");
    c2_uv_fs_req_cleanup = (c2_uv_fs_req_cleanup_fn)(void *)GetProcAddress(process_module, "uv_fs_req_cleanup");
    if (c2_uv_open_osfhandle == NULL || c2_uv_fs_close == NULL ||
        c2_uv_fs_req_cleanup == NULL) {
        c2_uv_open_osfhandle = NULL;
        c2_uv_fs_close = NULL;
        c2_uv_fs_req_cleanup = NULL;
    }
    return TRUE;
}

static bool resolve_process_libuv(void) {
    /* Worker environments share this addon. Publish the complete immutable
     * function group once before any worker can mint a Node-owned descriptor. */
    InitOnceExecuteOnce(&process_libuv_once, initialize_process_libuv, NULL, NULL);
    return c2_uv_open_osfhandle != NULL;
}

/* Release through the Node CRT that minted the descriptor. The supported
 * Node process initializes stdio slots 0-2 and libuv preserves them. */
static void close_windows_pipe_fd(int32_t fd) {
    if (fd < 0) {
        return;
    }
    assert(fd >= 3);
    assert(c2_uv_fs_close != NULL && c2_uv_fs_req_cleanup != NULL);
    uv_fs_t request;
    memset(&request, 0, sizeof(request));
    c2_uv_fs_close(NULL, &request, fd, NULL);
    c2_uv_fs_req_cleanup(&request);
}
#endif

static void close_socket_fd(int32_t fd) {
#ifndef _WIN32
    if (fd >= 0) close(fd);
#else
    close_windows_pipe_fd(fd);
#endif
}

static void socket_fd_finalize(napi_env env, void *data, void *hint) {
    (void)env; (void)hint;
    NativeSocketFd *owner = (NativeSocketFd *)data;
    close_socket_fd(owner->fd);
    free(owner);
}

static NativeSocketFd *socket_fd_receiver(napi_env env, napi_callback_info info) {
    napi_value receiver;
    bool tagged = false;
    void *raw = NULL;
    if (!check_napi(napi_get_cb_info(env, info, NULL, NULL, &receiver, NULL)) ||
        !check_napi(napi_check_object_type_tag(env, receiver, &socket_fd_tag, &tagged)) ||
        !tagged || !check_napi(napi_unwrap(env, receiver, &raw)) || raw == NULL) {
        throw_type_error(env, "Expected a native connected Unix socket owner.");
        return NULL;
    }
    return (NativeSocketFd *)raw;
}

static napi_value socket_fd_take_memory(napi_env env, napi_callback_info info) {
    NativeSocketFd *owner = socket_fd_receiver(env, info);
    if (owner == NULL) return NULL;
    if (owner->fd < 0 || owner->memory_taken) return throw_type_error(env, "Native memory context was already transferred or socket closed.");
    napi_value memory = make_memory(env, owner->symbols, (uintptr_t)owner->fd, 0);
    if (memory != NULL) owner->memory_taken = true;
    return memory;
}

static napi_value socket_fd_take(napi_env env, napi_callback_info info) {
    NativeSocketFd *owner = socket_fd_receiver(env, info);
    if (owner == NULL) return NULL;
    if (owner->fd < 0) return throw_type_error(env, "Native socket was already closed or transferred.");
    napi_value value;
    if (!check_napi(napi_create_int32(env, owner->fd, &value))) return NULL;
    owner->fd = -1;
    return value;
}

static napi_value socket_fd_close(napi_env env, napi_callback_info info) {
    NativeSocketFd *owner = socket_fd_receiver(env, info);
    if (owner == NULL) return NULL;
    close_socket_fd(owner->fd);
    owner->fd = -1;
    napi_value value;
    if (!check_napi(napi_get_undefined(env, &value))) return NULL;
    return value;
}

static napi_value socket_fd_object(napi_env env, C2MemFfiNodeSymbols *symbols, int32_t fd, NativeSocketFd **out_owner) {
    NativeSocketFd *owner = (NativeSocketFd *)malloc(sizeof(*owner));
    if (owner == NULL) { close_socket_fd(fd); return throw_error(env, "Out of memory."); }
    owner->fd = fd;
    owner->symbols = symbols;
    owner->memory_taken = false;
    napi_value object;
    napi_property_descriptor methods[] = {
        { "takeMemory", NULL, socket_fd_take_memory, NULL, NULL, NULL, napi_default, NULL },
        { "takeFd", NULL, socket_fd_take, NULL, NULL, NULL, napi_default, NULL },
        { "close", NULL, socket_fd_close, NULL, NULL, NULL, napi_default, NULL },
    };
    if (!check_napi(napi_create_object(env, &object)) ||
        !check_napi(napi_define_properties(env, object, 3, methods)) ||
        !check_napi(napi_type_tag_object(env, object, &socket_fd_tag)) ||
        !check_napi(napi_wrap(env, object, owner, socket_fd_finalize, NULL, NULL))) {
        socket_fd_finalize(env, owner, NULL);
        return NULL;
    }
    *out_owner = owner;
    return object;
}

typedef struct EndpointConnectWork {
    C2MemFfiNodeEndpointContext *context;
    char *address;
    napi_ref receiver;
    napi_async_work work;
    napi_deferred deferred;
    C2MemFfiStatus status;
    int32_t fd;
    int32_t os_error;
} EndpointConnectWork;

static void endpoint_connect_execute(napi_env env, void *data) {
    (void)env;
    EndpointConnectWork *work = (EndpointConnectWork *)data;
    work->status = work->context->symbols->endpoint_context_connect_unix(
        work->context->ptr, work->address, &work->fd, &work->os_error);
}

static void endpoint_connect_dispose(napi_env env, EndpointConnectWork *work) {
    close_socket_fd(work->fd);
    work->context->active_connects -= 1;
    endpoint_context_release_closed(work->context);
    if (work->receiver != NULL) napi_delete_reference(env, work->receiver);
    if (work->work != NULL) napi_delete_async_work(env, work->work);
    free(work->address);
    free(work);
}

static void endpoint_connect_complete(napi_env env, napi_status status, void *data) {
    EndpointConnectWork *work = (EndpointConnectWork *)data;
    if (status != napi_ok || work->context->closed) work->status = C2_MEM_FFI_STATUS_IO_ERROR;
    NativeSocketFd *owner = NULL;
    napi_value value = NULL;
    if (work->status == C2_MEM_FFI_STATUS_OK) {
        int32_t fd = work->fd;
        work->fd = -1;
        value = socket_fd_object(env, work->context->symbols, fd, &owner);
    }
    napi_value result = make_status_result(env, work->status, value);
    if (result == NULL || !check_napi(napi_resolve_deferred(env, work->deferred, result))) {
        if (owner != NULL) { close_socket_fd(owner->fd); owner->fd = -1; }
    }
    endpoint_connect_dispose(env, work);
}

static napi_value endpoint_context_connect(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1], receiver;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, &receiver, NULL)) || argc != 1) {
        return throw_type_error(env, "connectUnix expects an ipc:// address.");
    }
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, receiver, true);
    if (context == NULL) return NULL;
    char *address = read_string_arg(env, args[0], "IPC address must be a string.");
    if (address == NULL) return NULL;
    EndpointConnectWork *work = (EndpointConnectWork *)calloc(1, sizeof(*work));
    if (work == NULL) { free(address); return throw_error(env, "Out of memory."); }
    work->context = context;
    work->address = address;
    work->fd = -1;
    context->active_connects += 1;
    napi_value promise, name;
    if (!check_napi(napi_create_promise(env, &work->deferred, &promise)) ||
        !check_napi(napi_create_string_utf8(env, "c2-local-connect", NAPI_AUTO_LENGTH, &name)) ||
        !check_napi(napi_create_reference(env, receiver, 1, &work->receiver)) ||
        !check_napi(napi_create_async_work(env, NULL, name, endpoint_connect_execute,
            endpoint_connect_complete, work, &work->work)) ||
        !check_napi(napi_queue_async_work(env, work->work))) {
        endpoint_connect_dispose(env, work);
        return throw_error(env, "Failed to queue a native Unix connection.");
    }
    return promise;
}

/* A Windows pipe owner keeps the pinned server process handle until the
 * descriptor is taken by Node's Socket and the resulting connection closes,
 * including GC of an ignored result. */
typedef struct NativeWindowsSocket {
    int32_t fd;        /* -1 once transferred to Node or closed */
    uintptr_t process; /* server process pin; 0 once released */
    uintptr_t pipe; /* borrowed from fd, only before transfer */
    C2MemFfiNodeSymbols *symbols;
    bool memory_taken;
} NativeWindowsSocket;
static const napi_type_tag windows_socket_tag = {
    UINT64_C(0x2f6b97d84c0a51e3), UINT64_C(0x6a1c40fe93b7d209)
};

static void windows_socket_release(NativeWindowsSocket *owner) {
    close_socket_fd(owner->fd);
    owner->fd = -1;
#ifdef _WIN32
    if (owner->process != 0) {
        CloseHandle((HANDLE)owner->process);
    }
#endif
    owner->process = 0;
}

static void windows_socket_finalize(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    NativeWindowsSocket *owner = (NativeWindowsSocket *)data;
    if (owner == NULL) {
        return;
    }
    windows_socket_release(owner);
    free(owner);
}

static NativeWindowsSocket *windows_socket_receiver(napi_env env, napi_callback_info info) {
    napi_value receiver;
    bool tagged = false;
    void *raw = NULL;
    if (!check_napi(napi_get_cb_info(env, info, NULL, NULL, &receiver, NULL)) ||
        !check_napi(napi_check_object_type_tag(env, receiver, &windows_socket_tag, &tagged)) ||
        !tagged || !check_napi(napi_unwrap(env, receiver, &raw)) || raw == NULL) {
        throw_type_error(env, "Expected a native connected Windows pipe owner.");
        return NULL;
    }
    return (NativeWindowsSocket *)raw;
}

static napi_value windows_socket_take_memory(napi_env env, napi_callback_info info) {
    NativeWindowsSocket *owner = windows_socket_receiver(env, info);
    if (owner == NULL) return NULL;
    if (owner->fd < 0 || owner->memory_taken) return throw_type_error(env, "Native memory context was already transferred or pipe closed.");
    napi_value memory = make_memory(env, owner->symbols, owner->pipe, owner->process);
    if (memory != NULL) owner->memory_taken = true;
    return memory;
}

static napi_value windows_socket_take(napi_env env, napi_callback_info info) {
    NativeWindowsSocket *owner = windows_socket_receiver(env, info);
    if (owner == NULL) return NULL;
    if (owner->fd < 0) return throw_type_error(env, "Native Windows pipe was already closed or transferred.");
    napi_value value;
    if (!check_napi(napi_create_int32(env, owner->fd, &value))) return NULL;
    owner->fd = -1;
    return value;
}

static napi_value windows_socket_close(napi_env env, napi_callback_info info) {
    NativeWindowsSocket *owner = windows_socket_receiver(env, info);
    if (owner == NULL) return NULL;
    windows_socket_release(owner);
    napi_value value;
    if (!check_napi(napi_get_undefined(env, &value))) return NULL;
    return value;
}

static napi_value windows_socket_object(napi_env env, C2MemFfiNodeSymbols *symbols, uintptr_t pipe, int32_t fd, uintptr_t process, NativeWindowsSocket **out_owner) {
    NativeWindowsSocket *owner = (NativeWindowsSocket *)malloc(sizeof(*owner));
    if (owner == NULL) {
        close_socket_fd(fd);
#ifdef _WIN32
        if (process != 0) CloseHandle((HANDLE)process);
#endif
        return throw_error(env, "Out of memory.");
    }
    owner->fd = fd;
    owner->process = process;
    owner->pipe = pipe;
    owner->symbols = symbols;
    owner->memory_taken = false;
    napi_value object;
    napi_property_descriptor methods[] = {
        { "takeMemory", NULL, windows_socket_take_memory, NULL, NULL, NULL, napi_default, NULL },
        { "takeFd", NULL, windows_socket_take, NULL, NULL, NULL, napi_default, NULL },
        { "close", NULL, windows_socket_close, NULL, NULL, NULL, napi_default, NULL },
    };
    if (!check_napi(napi_create_object(env, &object)) ||
        !check_napi(napi_define_properties(env, object, 3, methods)) ||
        !check_napi(napi_type_tag_object(env, object, &windows_socket_tag)) ||
        !check_napi(napi_wrap(env, object, owner, windows_socket_finalize, NULL, NULL))) {
        windows_socket_finalize(env, owner, NULL);
        return NULL;
    }
    *out_owner = owner;
    return object;
}

typedef struct WindowsConnectWork {
    C2MemFfiNodeEndpointContext *context;
    char *address;
    napi_ref receiver;
    napi_async_work work;
    napi_deferred deferred;
    C2MemFfiStatus status;
    int32_t os_error;
    uintptr_t pipe;
    uintptr_t process;
} WindowsConnectWork;

static void windows_connect_execute(napi_env env, void *data) {
    (void)env;
    WindowsConnectWork *work = (WindowsConnectWork *)data;
    work->status = work->context->symbols->endpoint_context_connect_windows(
        work->context->ptr, work->address, &work->pipe, &work->process, &work->os_error);
}

static void windows_connect_discard_handles(WindowsConnectWork *work) {
#ifdef _WIN32
    if (work->pipe != 0) {
        CloseHandle((HANDLE)work->pipe);
    }
    if (work->process != 0) {
        CloseHandle((HANDLE)work->process);
    }
#endif
    work->pipe = 0;
    work->process = 0;
}

static void windows_connect_dispose(napi_env env, WindowsConnectWork *work) {
    windows_connect_discard_handles(work);
    work->context->active_connects -= 1;
    endpoint_context_release_closed(work->context);
    if (work->receiver != NULL) napi_delete_reference(env, work->receiver);
    if (work->work != NULL) napi_delete_async_work(env, work->work);
    free(work->address);
    free(work);
}

#ifdef _WIN32
static void windows_connect_reject_with_message(napi_env env, WindowsConnectWork *work, const char *message) {
    napi_value text, error;
    if (check_napi(napi_create_string_utf8(env, message, NAPI_AUTO_LENGTH, &text)) &&
        check_napi(napi_create_error(env, NULL, text, &error))) {
        napi_reject_deferred(env, work->deferred, error);
    } else {
        throw_error(env, message);
    }
}
#endif

static void windows_connect_complete(napi_env env, napi_status status, void *data) {
    WindowsConnectWork *work = (WindowsConnectWork *)data;
    if (status != napi_ok || work->context->closed) {
        work->status = C2_MEM_FFI_STATUS_IO_ERROR;
        windows_connect_discard_handles(work);
    }
    NativeWindowsSocket *owner = NULL;
    uintptr_t verified_pipe = 0;
    napi_value value = NULL;
    if (work->status == C2_MEM_FFI_STATUS_OK) {
        int32_t fd = -1;
#ifdef _WIN32
        /* Convert on the JS thread before anything can register the handle
         * with an event loop: the descriptor is minted in the Node CRT and
         * handed straight to net.Socket's fd adoption path. */
        if (!resolve_process_libuv()) {
            windows_connect_discard_handles(work);
            windows_connect_reject_with_message(env, work,
                "This Node process does not export the libuv descriptor transfer API (uv_open_osfhandle).");
            windows_connect_dispose(env, work);
            return;
        }
        fd = (int32_t)c2_uv_open_osfhandle((HANDLE)work->pipe);
        if (fd < 0) {
            /* The Node CRT did not adopt the handle, so the work entry is
             * still the single owner of both raw handles and releases them. */
            char message[160];
            snprintf(message, sizeof(message),
                "Node libuv descriptor transfer failed for the native Windows pipe (returned %d).", fd);
            windows_connect_discard_handles(work);
            windows_connect_reject_with_message(env, work, message);
            windows_connect_dispose(env, work);
            return;
        }
        /* Ownership handoff: only after a successful conversion does the CRT
         * descriptor own the pipe handle; the work entry keeps the process
         * pin until the socket owner below takes it over. */
        verified_pipe = work->pipe;
        work->pipe = 0;
        /* Node PlatformInit occupies stdio slots; Windows libuv never closes
         * them. A native embedder that bypasses that initialization is outside
         * this Node connector's ownership contract (Node v22.23.4 node.cc). */
        assert(fd >= 3);
#else
        /* The native entry never succeeds outside Windows. */
        windows_connect_discard_handles(work);
        work->status = C2_MEM_FFI_STATUS_IO_ERROR;
#endif
        if (work->status == C2_MEM_FFI_STATUS_OK) {
            uintptr_t process = work->process;
            work->process = 0; /* The socket owner now pins the server process. */
            value = windows_socket_object(env, work->context->symbols, verified_pipe, fd, process, &owner);
            if (value == NULL) {
                /* windows_socket_object released the descriptor and process pin. */
                windows_connect_dispose(env, work);
                return;
            }
        }
    }
    napi_value result = make_status_result(env, work->status, value);
    if (result == NULL || !check_napi(napi_resolve_deferred(env, work->deferred, result))) {
        if (owner != NULL) windows_socket_release(owner);
    }
    windows_connect_dispose(env, work);
}

static napi_value queue_windows_connect(napi_env env, C2MemFfiNodeEndpointContext *context, napi_value receiver, char *address) {
    WindowsConnectWork *work = (WindowsConnectWork *)calloc(1, sizeof(*work));
    if (work == NULL) {
        free(address);
        return throw_error(env, "Out of memory.");
    }
    work->context = context;
    work->address = address;
    context->active_connects += 1;
    napi_value promise, name;
    if (!check_napi(napi_create_promise(env, &work->deferred, &promise)) ||
        !check_napi(napi_create_string_utf8(env, "c2-local-connect-windows", NAPI_AUTO_LENGTH, &name)) ||
        !check_napi(napi_create_reference(env, receiver, 1, &work->receiver)) ||
        !check_napi(napi_create_async_work(env, NULL, name, windows_connect_execute,
            windows_connect_complete, work, &work->work)) ||
        !check_napi(napi_queue_async_work(env, work->work))) {
        windows_connect_dispose(env, work);
        return throw_error(env, "Failed to queue a native Windows connection.");
    }
    return promise;
}

static napi_value endpoint_context_connect_windows_method(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1], receiver;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, &receiver, NULL)) || argc != 1) {
        return throw_type_error(env, "connectWindows expects an ipc:// address.");
    }
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, receiver, true);
    if (context == NULL) return NULL;
    char *address = read_string_arg(env, args[0], "IPC address must be a string.");
    if (address == NULL) return NULL;
    return queue_windows_connect(env, context, receiver, address);
}

/* Module-level verified Windows connect: the receiver argument must be a real
 * addon-tagged endpoint context. Forged endpoint objects carrying arbitrary
 * connectUnix/connectWindows functions cannot pass the native type-tag check
 * performed here, so unverified sockets cannot enter official Windows flows. */
static napi_value endpoint_context_connect_windows_verified(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2];
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, (void **)&symbols)) || argc != 2) {
        return throw_type_error(env, "verified Windows connect expects a context object and an ipc:// address.");
    }
    C2MemFfiNodeEndpointContext *context = endpoint_context_receiver(env, args[0], true);
    if (context == NULL) return NULL;
    char *address = read_string_arg(env, args[1], "IPC address must be a string.");
    if (address == NULL) return NULL;
    return queue_windows_connect(env, context, args[0], address);
}

static napi_value endpoint_context_capture(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, (void **)&symbols))) return NULL;
    char *root = NULL;
    if (argc > 0) {
        napi_valuetype type;
        if (!check_napi(napi_typeof(env, args[0], &type))) return NULL;
        if (type != napi_null && type != napi_undefined) {
            root = read_string_arg(env, args[0], "IPC root must be a string or null.");
            if (root == NULL) return NULL;
        }
    }
    C2MemFfiLocalEndpointContext *ptr = NULL;
    C2MemFfiStatus status = symbols->endpoint_context_capture(root, &ptr);
    free(root);
    if (status != C2_MEM_FFI_STATUS_OK || ptr == NULL) {
        if (ptr != NULL) symbols->endpoint_context_free(ptr);
        if (status == C2_MEM_FFI_STATUS_OK) {
            return throw_error(env, "Native IPC endpoint capture returned no context.");
        }
        return make_status_result(env, status, NULL);
    }
    C2MemFfiNodeEndpointContext *context = (C2MemFfiNodeEndpointContext *)calloc(1, sizeof(*context));
    if (context == NULL) {
        symbols->endpoint_context_free(ptr);
        return throw_error(env, "Out of memory.");
    }
    context->symbols = symbols;
    context->ptr = ptr;
    napi_value object;
    napi_property_descriptor methods[] = {
        { "endpointName", NULL, endpoint_context_name, NULL, NULL, NULL, napi_default, NULL },
        { "namespaceId", NULL, endpoint_context_namespace_id, NULL, NULL, NULL, napi_default, NULL },
        { "connectUnix", NULL, endpoint_context_connect, NULL, NULL, NULL, napi_default, NULL },
        { "connectWindows", NULL, endpoint_context_connect_windows_method, NULL, NULL, NULL, napi_default, NULL },
        { "close", NULL, endpoint_context_close, NULL, NULL, NULL, napi_default, NULL },
    };
    if (!check_napi(napi_create_object(env, &object)) ||
        !check_napi(napi_define_properties(env, object, sizeof(methods) / sizeof(methods[0]), methods)) ||
        !check_napi(napi_type_tag_object(env, object, &endpoint_context_tag)) ||
        !check_napi(napi_wrap(env, object, context, endpoint_context_finalize, NULL, NULL))) {
        endpoint_context_finalize(env, context, NULL);
        return NULL;
    }
    napi_value result = make_status_result(env, status, object);
    if (result == NULL) {
        /* The wrapped object still owns its small shell; close native ownership now. */
        symbols->endpoint_context_free(context->ptr);
        context->ptr = NULL;
    }
    return result;
}

static bool load_symbol(napi_env env, void *library, const char *name, void **out) {
#ifdef _WIN32
    void *symbol = (void *)GetProcAddress((HMODULE)library, name);
    if (symbol == NULL) {
        char message[512];
        snprintf(message, sizeof(message), "Failed to load c2-mem-ffi symbol %s (Windows error %lu)", name, GetLastError());
        throw_error(env, message);
        return false;
    }
#else
    dlerror();
    void *symbol = dlsym(library, name);
    const char *error = dlerror();
    if (error != NULL || symbol == NULL) {
        char message[512];
        snprintf(message, sizeof(message), "Failed to load c2-mem-ffi symbol %s: %s", name, error == NULL ? "not found" : error);
        throw_error(env, message);
        return false;
    }
#endif
    *out = symbol;
    return true;
}

static bool load_all_symbols(napi_env env, C2MemFfiNodeSymbols *symbols) {
#define LOAD_REQUIRED(field, exported) \
    do { \
        if (!load_symbol(env, symbols->library, exported, (void **)&symbols->field)) { \
            return false; \
        } \
    } while (0)
    LOAD_REQUIRED(abi_version, "c2_mem_ffi_abi_version");
    uint32_t loaded_abi_version = symbols->abi_version();
    if (loaded_abi_version != C2_MEM_FFI_ABI_VERSION) {
        char message[256];
        snprintf(message, sizeof(message), "c2_mem_ffi ABI version %u is incompatible with expected version %u.", loaded_abi_version, C2_MEM_FFI_ABI_VERSION);
        throw_error(env, message);
        return false;
    }
    LOAD_REQUIRED(endpoint_context_capture, "c2_mem_ffi_local_endpoint_context_capture");
    LOAD_REQUIRED(endpoint_context_free, "c2_mem_ffi_local_endpoint_context_free");
    LOAD_REQUIRED(endpoint_context_name_len, "c2_mem_ffi_local_endpoint_context_name_len");
    LOAD_REQUIRED(endpoint_context_name_copy, "c2_mem_ffi_local_endpoint_context_name_copy");
    LOAD_REQUIRED(endpoint_context_namespace_id_len, "c2_mem_ffi_local_endpoint_context_namespace_id_len");
    LOAD_REQUIRED(endpoint_context_namespace_id_copy, "c2_mem_ffi_local_endpoint_context_namespace_id_copy");
    LOAD_REQUIRED(endpoint_context_connect_unix, "c2_mem_ffi_local_endpoint_context_connect_unix");
    LOAD_REQUIRED(endpoint_context_connect_windows, "c2_mem_ffi_local_endpoint_context_connect_windows");
    LOAD_REQUIRED(memory_context_new, "c2_mem_ffi_memory_context_new");
    LOAD_REQUIRED(memory_context_free, "c2_mem_ffi_memory_context_free");
    LOAD_REQUIRED(memory_context_prefix_len, "c2_mem_ffi_memory_context_prefix_len");
    LOAD_REQUIRED(memory_context_prefix_copy, "c2_mem_ffi_memory_context_prefix_copy");
    LOAD_REQUIRED(memory_context_finish_handshake, "c2_mem_ffi_memory_context_finish_handshake");
    LOAD_REQUIRED(request_prepare, "c2_mem_ffi_request_prepare");
    LOAD_REQUIRED(request_discard, "c2_mem_ffi_request_discard");
    LOAD_REQUIRED(response_admit, "c2_mem_ffi_response_admit");
    LOAD_REQUIRED(response_consume, "c2_mem_ffi_response_consume");
    LOAD_REQUIRED(response_discard, "c2_mem_ffi_response_discard");
    LOAD_REQUIRED(memory_context_receive_release, "c2_mem_ffi_memory_context_receive_release");
    LOAD_REQUIRED(write_begin, "c2_mem_ffi_write_begin");
    LOAD_REQUIRED(write_prefix_len, "c2_mem_ffi_write_prefix_len");
    LOAD_REQUIRED(write_prefix_copy, "c2_mem_ffi_write_prefix_copy");
    LOAD_REQUIRED(write_start, "c2_mem_ffi_write_start");
    LOAD_REQUIRED(write_complete, "c2_mem_ffi_write_complete");
    LOAD_REQUIRED(write_discard, "c2_mem_ffi_write_discard");
    LOAD_REQUIRED(memory_context_deactivate, "c2_mem_ffi_memory_context_deactivate");
    LOAD_REQUIRED(memory_context_peer_end, "c2_mem_ffi_memory_context_peer_end");
    LOAD_REQUIRED(memory_context_try_close, "c2_mem_ffi_memory_context_try_close");
#undef LOAD_REQUIRED
    return true;
}

static bool set_function(napi_env env, napi_value object, const char *name, napi_callback callback, C2MemFfiNodeSymbols *symbols) {
    napi_value function;
    if (!check_napi(napi_create_function(env, name, NAPI_AUTO_LENGTH, callback, symbols, &function))) {
        return false;
    }
    return set_named_value(env, object, name, function);
}

static napi_value load(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 1) {
        return throw_type_error(env, "load expects a c2_mem_ffi shared library path.");
    }
    char *library_path = read_string_arg(env, args[0], "libraryPath must be a string.");
    if (library_path == NULL) {
        return NULL;
    }
    char loader_error[384] = {0};
    void *library = open_library(library_path, loader_error, sizeof(loader_error));
    if (library == NULL) {
        char message[512];
        snprintf(message, sizeof(message), "Failed to load c2_mem_ffi shared library: %s", loader_error);
        free(library_path);
        return throw_error(env, message);
    }
    free(library_path);
    C2MemFfiNodeSymbols *symbols = (C2MemFfiNodeSymbols *)calloc(1, sizeof(C2MemFfiNodeSymbols));
    if (symbols == NULL) {
        close_library(library);
        return throw_error(env, "Out of memory.");
    }
    symbols->library = library;
    if (!load_all_symbols(env, symbols)) {
        close_library(library);
        free(symbols);
        return NULL;
    }
    napi_value object;
    if (!check_napi(napi_create_object(env, &object))) {
        close_library(library);
        free(symbols);
        return NULL;
    }
    if (!set_function(env, object, "c2_mem_ffi_abi_version", abi_version_callback, symbols) ||
        !set_function(env, object, "c2_mem_ffi_local_endpoint", local_endpoint, symbols) ||
        !set_function(env, object, "c2_mem_ffi_local_endpoint_context_capture", endpoint_context_capture, symbols) ||
        !set_function(env, object, "c2_mem_ffi_endpoint_context_connect_windows", endpoint_context_connect_windows_verified, symbols)) {
        close_library(library);
        free(symbols);
        return NULL;
    }
    return object;
}

static napi_value Init(napi_env env, napi_value exports) {
    napi_value load_function;
    if (!check_napi(napi_create_function(env, "load", NAPI_AUTO_LENGTH, load, NULL, &load_function))) {
        return NULL;
    }
    napi_set_named_property(env, exports, "load", load_function);
    return exports;
}

NAPI_MODULE(c2_mem_ffi_node, Init)
