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

typedef C2MemFfiStatus (*request_pool_new_fn)(
    const char *,
    uint32_t,
    uint16_t,
    uint32_t,
    C2MemFfiRequestPool **);
typedef void (*request_pool_destroy_fn)(C2MemFfiRequestPool *);
typedef C2MemFfiStatus (*request_pool_prefix_len_fn)(const C2MemFfiRequestPool *, size_t *);
typedef C2MemFfiStatus (*request_pool_prefix_copy_fn)(const C2MemFfiRequestPool *, char *, size_t, size_t *);
typedef C2MemFfiStatus (*request_pool_segment_count_fn)(const C2MemFfiRequestPool *, size_t *);
typedef C2MemFfiStatus (*request_pool_segment_name_len_fn)(const C2MemFfiRequestPool *, size_t, size_t *);
typedef C2MemFfiStatus (*request_pool_segment_name_copy_fn)(const C2MemFfiRequestPool *, size_t, char *, size_t, size_t *);
typedef C2MemFfiStatus (*request_pool_segment_data_size_fn)(const C2MemFfiRequestPool *, size_t, uint32_t *);
typedef C2MemFfiStatus (*request_pool_write_fn)(C2MemFfiRequestPool *, const uint8_t *, size_t, C2MemFfiRequestBlock *);
typedef C2MemFfiStatus (*request_pool_read_local_fn)(const C2MemFfiRequestPool *, C2MemFfiRequestBlock, uint8_t *, size_t, size_t *);
typedef C2MemFfiStatus (*request_pool_release_fn)(C2MemFfiRequestPool *, C2MemFfiRequestBlock);
typedef C2MemFfiStatus (*request_pool_forget_consumed_fn)(C2MemFfiRequestPool *, C2MemFfiRequestBlock);
typedef C2MemFfiStatus (*response_pool_new_fn)(
    const char *,
    uint32_t,
    uint16_t,
    uint32_t,
    C2MemFfiResponsePool **);
typedef void (*response_pool_destroy_fn)(C2MemFfiResponsePool *);
typedef C2MemFfiStatus (*response_pool_read_fn)(C2MemFfiResponsePool *, C2MemFfiResponseBlock, uint8_t *, size_t, size_t *);
typedef C2MemFfiStatus (*response_pool_release_fn)(C2MemFfiResponsePool *, C2MemFfiResponseBlock);
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
    request_pool_new_fn request_pool_new;
    request_pool_destroy_fn request_pool_destroy;
    request_pool_prefix_len_fn request_pool_prefix_len;
    request_pool_prefix_copy_fn request_pool_prefix_copy;
    request_pool_segment_count_fn request_pool_segment_count;
    request_pool_segment_name_len_fn request_pool_segment_name_len;
    request_pool_segment_name_copy_fn request_pool_segment_name_copy;
    request_pool_segment_data_size_fn request_pool_segment_data_size;
    request_pool_write_fn request_pool_write;
    request_pool_read_local_fn request_pool_read_local;
    request_pool_release_fn request_pool_release;
    request_pool_forget_consumed_fn request_pool_forget_consumed;
    response_pool_new_fn response_pool_new;
    response_pool_destroy_fn response_pool_destroy;
    response_pool_read_fn response_pool_read;
    response_pool_release_fn response_pool_release;
} C2MemFfiNodeSymbols;

typedef enum C2MemFfiNodePoolKind {
    C2_MEM_FFI_NODE_REQUEST_POOL = 1,
    C2_MEM_FFI_NODE_RESPONSE_POOL = 2,
} C2MemFfiNodePoolKind;

typedef struct C2MemFfiNodePoolHandle {
    C2MemFfiNodeSymbols *symbols;
    C2MemFfiNodePoolKind kind;
    void *ptr;
} C2MemFfiNodePoolHandle;

typedef struct C2MemFfiNodeEndpointContext {
    C2MemFfiNodeSymbols *symbols;
    C2MemFfiLocalEndpointContext *ptr;
    size_t active_connects;
    bool closed;
    bool finalized;
} C2MemFfiNodeEndpointContext;

/* Only wrapped objects created by this addon can carry a context owner.
 * ABI 6 rotated this tag from the ABI 5 constant: a stale ABI 5 addon's
 * endpoint contexts must fail the tag check instead of being unwrapped. */
static const napi_type_tag endpoint_context_tag = {
    UINT64_C(0x5a71e0c3f28b94d6), UINT64_C(0xd6340f81b9e57c2a)
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

static bool get_external_handle(
    napi_env env,
    napi_value value,
    C2MemFfiNodePoolKind expected_kind,
    C2MemFfiNodePoolHandle **out_handle) {
    void *raw = NULL;
    if (!check_napi(napi_get_value_external(env, value, &raw)) || raw == NULL) {
        throw_type_error(env, "Expected a c2-mem-ffi native pool handle.");
        return false;
    }
    C2MemFfiNodePoolHandle *handle = (C2MemFfiNodePoolHandle *)raw;
    if (handle->kind != expected_kind) {
        throw_type_error(env, "c2-mem-ffi native pool handle kind mismatch.");
        return false;
    }
    if (handle->ptr == NULL) {
        throw_type_error(env, "c2-mem-ffi native pool handle is closed.");
        return false;
    }
    *out_handle = handle;
    return true;
}

static napi_value make_pool_external(
    napi_env env,
    C2MemFfiNodeSymbols *symbols,
    C2MemFfiNodePoolKind kind,
    void *ptr);

static void pool_external_finalize(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    C2MemFfiNodePoolHandle *handle = (C2MemFfiNodePoolHandle *)data;
    if (handle == NULL) {
        return;
    }
    if (handle->ptr != NULL) {
        if (handle->kind == C2_MEM_FFI_NODE_REQUEST_POOL) {
            handle->symbols->request_pool_destroy((C2MemFfiRequestPool *)handle->ptr);
        } else if (handle->kind == C2_MEM_FFI_NODE_RESPONSE_POOL) {
            handle->symbols->response_pool_destroy((C2MemFfiResponsePool *)handle->ptr);
        }
        handle->ptr = NULL;
    }
    free(handle);
}

static napi_value make_pool_external(
    napi_env env,
    C2MemFfiNodeSymbols *symbols,
    C2MemFfiNodePoolKind kind,
    void *ptr) {
    C2MemFfiNodePoolHandle *handle = (C2MemFfiNodePoolHandle *)calloc(1, sizeof(C2MemFfiNodePoolHandle));
    if (handle == NULL) {
        return throw_error(env, "Out of memory.");
    }
    handle->symbols = symbols;
    handle->kind = kind;
    handle->ptr = ptr;
    napi_value external;
    if (!check_napi(napi_create_external(env, handle, pool_external_finalize, NULL, &external))) {
        free(handle);
        return NULL;
    }
    return external;
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
    uint64_t *allocation_id) {
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
    if (!get_named_property(env, value, "allocationId", &property) ||
        !check_napi(napi_get_value_bigint_uint64(env, property, allocation_id, &lossless)) ||
        !lossless) {
        throw_type_error(env, "block.allocationId must be a lossless u64 bigint.");
        return false;
    }
    if ((*is_dedicated == 0 && *allocation_id == 0) ||
        (*is_dedicated != 0 && *allocation_id != 0)) {
        throw_type_error(env, "block.allocationId must be nonzero for buddy memory and zero for dedicated memory.");
        return false;
    }
    return true;
}

static bool read_request_block(napi_env env, napi_value value, C2MemFfiRequestBlock *block) {
    uint16_t segment_index = 0;
    uint8_t is_dedicated = 0;
    uint32_t generation = 0;
    uint32_t offset = 0;
    uint32_t byte_length = 0;
    uint64_t allocation_id = 0;
    if (!read_block_common(env, value, &segment_index, &is_dedicated, &generation, &offset, &byte_length, &allocation_id)) {
        return false;
    }
    block->segment_index = segment_index;
    block->is_dedicated = is_dedicated;
    block->reserved = 0;
    block->generation = generation;
    block->offset = offset;
    block->byte_length = byte_length;
    block->allocation_id = allocation_id;
    return true;
}

static bool read_response_block(napi_env env, napi_value value, C2MemFfiResponseBlock *block) {
    uint16_t segment_index = 0;
    uint8_t is_dedicated = 0;
    uint32_t generation = 0;
    uint32_t offset = 0;
    uint32_t byte_length = 0;
    uint64_t allocation_id = 0;
    if (!read_block_common(env, value, &segment_index, &is_dedicated, &generation, &offset, &byte_length, &allocation_id)) {
        return false;
    }
    block->segment_index = segment_index;
    block->is_dedicated = is_dedicated;
    block->reserved = 0;
    block->generation = generation;
    block->offset = offset;
    block->byte_length = byte_length;
    block->allocation_id = allocation_id;
    return true;
}

static napi_value make_request_block(napi_env env, C2MemFfiRequestBlock block) {
    napi_value object;
    if (!check_napi(napi_create_object(env, &object))) {
        return NULL;
    }
    if (!set_named_uint32(env, object, "segmentIndex", block.segment_index) ||
        !set_named_uint32(env, object, "generation", block.generation) ||
        !set_named_uint32(env, object, "offset", block.offset) ||
        !set_named_uint32(env, object, "byteLength", block.byte_length) ||
        !set_named_uint64_bigint(env, object, "allocationId", block.allocation_id) ||
        !set_named_bool(env, object, "dedicated", block.is_dedicated != 0)) {
        return NULL;
    }
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
        data == NULL) {
        throw_type_error(env, label);
        return false;
    }
    *out_data = (uint8_t *)data;
    *out_length = length;
    return true;
}

static napi_value request_pool_new(napi_env env, napi_callback_info info) {
    size_t argc = 4;
    napi_value args[4];
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, (void **)&symbols)) || argc < 4) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_new expects prefix, segmentSize, maxSegments, and minBlockSize.");
    }
    char *prefix = read_string_arg(env, args[0], "prefix must be a string.");
    if (prefix == NULL) {
        return NULL;
    }
    uint32_t segment_size = 0;
    uint32_t max_segments_u32 = 0;
    uint32_t min_block_size = 0;
    if (!read_uint32_arg(env, args[1], "segmentSize must be a u32 integer.", &segment_size) ||
        !read_uint32_arg(env, args[2], "maxSegments must be a u32 integer.", &max_segments_u32) ||
        !read_uint32_arg(env, args[3], "minBlockSize must be a u32 integer.", &min_block_size)) {
        free(prefix);
        return NULL;
    }
    if (max_segments_u32 > UINT16_MAX) {
        free(prefix);
        return throw_type_error(env, "maxSegments must fit in u16.");
    }
    C2MemFfiRequestPool *pool = NULL;
    C2MemFfiStatus status = symbols->request_pool_new(prefix, segment_size, (uint16_t)max_segments_u32, min_block_size, &pool);
    free(prefix);
    napi_value external = NULL;
    if (status == C2_MEM_FFI_STATUS_OK) {
        external = make_pool_external(env, symbols, C2_MEM_FFI_NODE_REQUEST_POOL, pool);
        if (external == NULL) {
            symbols->request_pool_destroy(pool);
            return NULL;
        }
    }
    return make_status_result(env, status, external);
}

static napi_value abi_version_callback(napi_env env, napi_callback_info info) {
    size_t argc = 0;
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, NULL, NULL, (void **)&symbols))) {
        return throw_type_error(env, "c2_mem_ffi_abi_version could not read callback context.");
    }
    napi_value value;
    if (!check_napi(napi_create_uint32(env, symbols->abi_version(), &value))) {
        return NULL;
    }
    return value;
}

static napi_value request_pool_destroy(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 1) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_destroy expects a pool handle.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle)) {
        return NULL;
    }
    handle->symbols->request_pool_destroy((C2MemFfiRequestPool *)handle->ptr);
    handle->ptr = NULL;
    napi_value undefined;
    napi_get_undefined(env, &undefined);
    return undefined;
}

static napi_value request_pool_string(
    napi_env env,
    C2MemFfiRequestPool *pool,
    C2MemFfiStatus (*len_fn)(const C2MemFfiRequestPool *, size_t *),
    C2MemFfiStatus (*copy_fn)(const C2MemFfiRequestPool *, char *, size_t, size_t *)) {
    size_t length = 0;
    C2MemFfiStatus status = len_fn(pool, &length);
    if (status != C2_MEM_FFI_STATUS_OK) {
        return make_status_result(env, status, NULL);
    }
    char *buffer = (char *)malloc(length + 1);
    if (buffer == NULL) {
        return throw_error(env, "Out of memory.");
    }
    size_t written = 0;
    status = copy_fn(pool, buffer, length + 1, &written);
    napi_value value = NULL;
    if (status == C2_MEM_FFI_STATUS_OK) {
        if (!check_napi(napi_create_string_utf8(env, buffer, written, &value))) {
            free(buffer);
            return NULL;
        }
    }
    free(buffer);
    return make_status_result(env, status, value);
}

static napi_value request_pool_prefix(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 1) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_prefix expects a pool handle.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle)) {
        return NULL;
    }
    return request_pool_string(
        env,
        (C2MemFfiRequestPool *)handle->ptr,
        handle->symbols->request_pool_prefix_len,
        handle->symbols->request_pool_prefix_copy);
}

static napi_value request_pool_segment_name(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 2) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_segment_name expects a pool handle and segment index.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    uint32_t index = 0;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle) ||
        !read_uint32_arg(env, args[1], "segmentIndex must be a u32 integer.", &index)) {
        return NULL;
    }
    size_t length = 0;
    C2MemFfiStatus status = handle->symbols->request_pool_segment_name_len((C2MemFfiRequestPool *)handle->ptr, index, &length);
    if (status != C2_MEM_FFI_STATUS_OK) {
        return make_status_result(env, status, NULL);
    }
    char *buffer = (char *)malloc(length + 1);
    if (buffer == NULL) {
        return throw_error(env, "Out of memory.");
    }
    size_t written = 0;
    status = handle->symbols->request_pool_segment_name_copy((C2MemFfiRequestPool *)handle->ptr, index, buffer, length + 1, &written);
    napi_value value = NULL;
    if (status == C2_MEM_FFI_STATUS_OK) {
        if (!check_napi(napi_create_string_utf8(env, buffer, written, &value))) {
            free(buffer);
            return NULL;
        }
    }
    free(buffer);
    return make_status_result(env, status, value);
}

static napi_value request_pool_segment_count(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 1) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_segment_count expects a pool handle.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle)) {
        return NULL;
    }
    size_t count = 0;
    C2MemFfiStatus status = handle->symbols->request_pool_segment_count((C2MemFfiRequestPool *)handle->ptr, &count);
    return make_status_number_result(env, status, (double)count);
}

static napi_value request_pool_segment_data_size(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 2) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_segment_data_size expects a pool handle and segment index.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    uint32_t index = 0;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle) ||
        !read_uint32_arg(env, args[1], "segmentIndex must be a u32 integer.", &index)) {
        return NULL;
    }
    uint32_t size = 0;
    C2MemFfiStatus status = handle->symbols->request_pool_segment_data_size((C2MemFfiRequestPool *)handle->ptr, index, &size);
    return make_status_number_result(env, status, (double)size);
}

static napi_value request_pool_write(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 2) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_write expects a pool handle and Uint8Array payload.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    uint8_t *data = NULL;
    size_t data_len = 0;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle) ||
        !get_uint8_array(env, args[1], "payload must be a Uint8Array.", &data, &data_len)) {
        return NULL;
    }
    C2MemFfiRequestBlock block;
    memset(&block, 0, sizeof(block));
    C2MemFfiStatus status = handle->symbols->request_pool_write((C2MemFfiRequestPool *)handle->ptr, data, data_len, &block);
    napi_value value = NULL;
    if (status == C2_MEM_FFI_STATUS_OK) {
        value = make_request_block(env, block);
        if (value == NULL) {
            return NULL;
        }
    }
    return make_status_result(env, status, value);
}

static napi_value request_pool_read_local(napi_env env, napi_callback_info info) {
    size_t argc = 3;
    napi_value args[3];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 3) {
        return throw_type_error(env, "c2_mem_ffi_request_pool_read_local expects a pool handle, block, and Uint8Array destination.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    C2MemFfiRequestBlock block;
    uint8_t *destination = NULL;
    size_t destination_len = 0;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle) ||
        !read_request_block(env, args[1], &block) ||
        !get_uint8_array(env, args[2], "destination must be a Uint8Array.", &destination, &destination_len)) {
        return NULL;
    }
    size_t out_read = 0;
    C2MemFfiStatus status = handle->symbols->request_pool_read_local((C2MemFfiRequestPool *)handle->ptr, block, destination, destination_len, &out_read);
    return make_status_number_result(env, status, (double)out_read);
}

static napi_value request_pool_release_like(napi_env env, napi_callback_info info, bool forget_consumed) {
    size_t argc = 2;
    napi_value args[2];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 2) {
        return throw_type_error(env, "c2_mem_ffi request pool ownership call expects a pool handle and block.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    C2MemFfiRequestBlock block;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_REQUEST_POOL, &handle) ||
        !read_request_block(env, args[1], &block)) {
        return NULL;
    }
    C2MemFfiStatus status = forget_consumed
        ? handle->symbols->request_pool_forget_consumed((C2MemFfiRequestPool *)handle->ptr, block)
        : handle->symbols->request_pool_release((C2MemFfiRequestPool *)handle->ptr, block);
    return make_status_result(env, status, NULL);
}

static napi_value request_pool_release(napi_env env, napi_callback_info info) {
    return request_pool_release_like(env, info, false);
}

static napi_value request_pool_forget_consumed(napi_env env, napi_callback_info info) {
    return request_pool_release_like(env, info, true);
}

static napi_value response_pool_new(napi_env env, napi_callback_info info) {
    size_t argc = 4;
    napi_value args[4];
    C2MemFfiNodeSymbols *symbols = NULL;
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, (void **)&symbols)) || argc < 4) {
        return throw_type_error(env, "c2_mem_ffi_response_pool_new expects prefix, segmentSize, maxSegments, and minBlockSize.");
    }
    char *prefix = read_string_arg(env, args[0], "prefix must be a string.");
    if (prefix == NULL) {
        return NULL;
    }
    uint32_t segment_size = 0;
    uint32_t max_segments_u32 = 0;
    uint32_t min_block_size = 0;
    if (!read_uint32_arg(env, args[1], "segmentSize must be a u32 integer.", &segment_size) ||
        !read_uint32_arg(env, args[2], "maxSegments must be a u32 integer.", &max_segments_u32) ||
        !read_uint32_arg(env, args[3], "minBlockSize must be a u32 integer.", &min_block_size)) {
        free(prefix);
        return NULL;
    }
    if (max_segments_u32 > UINT16_MAX) {
        free(prefix);
        return throw_type_error(env, "maxSegments must fit in u16.");
    }
    C2MemFfiResponsePool *pool = NULL;
    C2MemFfiStatus status = symbols->response_pool_new(prefix, segment_size, (uint16_t)max_segments_u32, min_block_size, &pool);
    free(prefix);
    napi_value external = NULL;
    if (status == C2_MEM_FFI_STATUS_OK) {
        external = make_pool_external(env, symbols, C2_MEM_FFI_NODE_RESPONSE_POOL, pool);
        if (external == NULL) {
            symbols->response_pool_destroy(pool);
            return NULL;
        }
    }
    return make_status_result(env, status, external);
}

static napi_value response_pool_destroy(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value args[1];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 1) {
        return throw_type_error(env, "c2_mem_ffi_response_pool_destroy expects a pool handle.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_RESPONSE_POOL, &handle)) {
        return NULL;
    }
    handle->symbols->response_pool_destroy((C2MemFfiResponsePool *)handle->ptr);
    handle->ptr = NULL;
    napi_value undefined;
    napi_get_undefined(env, &undefined);
    return undefined;
}

static napi_value response_pool_read(napi_env env, napi_callback_info info) {
    size_t argc = 3;
    napi_value args[3];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 3) {
        return throw_type_error(env, "c2_mem_ffi_response_pool_read expects a pool handle, block, and Uint8Array destination.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    C2MemFfiResponseBlock block;
    uint8_t *destination = NULL;
    size_t destination_len = 0;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_RESPONSE_POOL, &handle) ||
        !read_response_block(env, args[1], &block) ||
        !get_uint8_array(env, args[2], "destination must be a Uint8Array.", &destination, &destination_len)) {
        return NULL;
    }
    size_t out_read = 0;
    C2MemFfiStatus status = handle->symbols->response_pool_read((C2MemFfiResponsePool *)handle->ptr, block, destination, destination_len, &out_read);
    return make_status_number_result(env, status, (double)out_read);
}

static napi_value response_pool_release(napi_env env, napi_callback_info info) {
    size_t argc = 2;
    napi_value args[2];
    if (!check_napi(napi_get_cb_info(env, info, &argc, args, NULL, NULL)) || argc < 2) {
        return throw_type_error(env, "c2_mem_ffi_response_pool_release expects a pool handle and block.");
    }
    C2MemFfiNodePoolHandle *handle = NULL;
    C2MemFfiResponseBlock block;
    if (!get_external_handle(env, args[0], C2_MEM_FFI_NODE_RESPONSE_POOL, &handle) ||
        !read_response_block(env, args[1], &block)) {
        return NULL;
    }
    C2MemFfiStatus status = handle->symbols->response_pool_release((C2MemFfiResponsePool *)handle->ptr, block);
    return make_status_result(env, status, NULL);
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
typedef struct NativeSocketFd { int32_t fd; } NativeSocketFd;
static const napi_type_tag socket_fd_tag = {
    UINT64_C(0xb82cd6be1d234b99), UINT64_C(0x9b4e465211132206)
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

static napi_value socket_fd_object(napi_env env, int32_t fd, NativeSocketFd **out_owner) {
    NativeSocketFd *owner = (NativeSocketFd *)malloc(sizeof(*owner));
    if (owner == NULL) { close_socket_fd(fd); return throw_error(env, "Out of memory."); }
    owner->fd = fd;
    napi_value object;
    napi_property_descriptor methods[] = {
        { "takeFd", NULL, socket_fd_take, NULL, NULL, NULL, napi_default, NULL },
        { "close", NULL, socket_fd_close, NULL, NULL, NULL, napi_default, NULL },
    };
    if (!check_napi(napi_create_object(env, &object)) ||
        !check_napi(napi_define_properties(env, object, 2, methods)) ||
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
        value = socket_fd_object(env, fd, &owner);
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
} NativeWindowsSocket;
static const napi_type_tag windows_socket_tag = {
    UINT64_C(0x2f6b97d84c0a51e3), UINT64_C(0x6a1c40fe93b7d208)
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

static napi_value windows_socket_object(napi_env env, int32_t fd, uintptr_t process, NativeWindowsSocket **out_owner) {
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
    napi_value object;
    napi_property_descriptor methods[] = {
        { "takeFd", NULL, windows_socket_take, NULL, NULL, NULL, napi_default, NULL },
        { "close", NULL, windows_socket_close, NULL, NULL, NULL, napi_default, NULL },
    };
    if (!check_napi(napi_create_object(env, &object)) ||
        !check_napi(napi_define_properties(env, object, 2, methods)) ||
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
            value = windows_socket_object(env, fd, process, &owner);
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
    LOAD_REQUIRED(request_pool_new, "c2_mem_ffi_request_pool_new");
    LOAD_REQUIRED(request_pool_destroy, "c2_mem_ffi_request_pool_destroy");
    LOAD_REQUIRED(request_pool_prefix_len, "c2_mem_ffi_request_pool_prefix_len");
    LOAD_REQUIRED(request_pool_prefix_copy, "c2_mem_ffi_request_pool_prefix_copy");
    LOAD_REQUIRED(request_pool_segment_count, "c2_mem_ffi_request_pool_segment_count");
    LOAD_REQUIRED(request_pool_segment_name_len, "c2_mem_ffi_request_pool_segment_name_len");
    LOAD_REQUIRED(request_pool_segment_name_copy, "c2_mem_ffi_request_pool_segment_name_copy");
    LOAD_REQUIRED(request_pool_segment_data_size, "c2_mem_ffi_request_pool_segment_data_size");
    LOAD_REQUIRED(request_pool_write, "c2_mem_ffi_request_pool_write");
    LOAD_REQUIRED(request_pool_read_local, "c2_mem_ffi_request_pool_read_local");
    LOAD_REQUIRED(request_pool_release, "c2_mem_ffi_request_pool_release");
    LOAD_REQUIRED(request_pool_forget_consumed, "c2_mem_ffi_request_pool_forget_consumed");
    LOAD_REQUIRED(response_pool_new, "c2_mem_ffi_response_pool_new");
    LOAD_REQUIRED(response_pool_destroy, "c2_mem_ffi_response_pool_destroy");
    LOAD_REQUIRED(response_pool_read, "c2_mem_ffi_response_pool_read");
    LOAD_REQUIRED(response_pool_release, "c2_mem_ffi_response_pool_release");
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
    if (!set_function(env, object, "c2_mem_ffi_request_pool_new", request_pool_new, symbols) ||
        !set_function(env, object, "c2_mem_ffi_abi_version", abi_version_callback, symbols) ||
        !set_function(env, object, "c2_mem_ffi_local_endpoint", local_endpoint, symbols) ||
        !set_function(env, object, "c2_mem_ffi_local_endpoint_context_capture", endpoint_context_capture, symbols) ||
        !set_function(env, object, "c2_mem_ffi_endpoint_context_connect_windows", endpoint_context_connect_windows_verified, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_destroy", request_pool_destroy, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_prefix", request_pool_prefix, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_segment_count", request_pool_segment_count, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_segment_name", request_pool_segment_name, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_segment_data_size", request_pool_segment_data_size, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_write", request_pool_write, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_read_local", request_pool_read_local, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_release", request_pool_release, symbols) ||
        !set_function(env, object, "c2_mem_ffi_request_pool_forget_consumed", request_pool_forget_consumed, symbols) ||
        !set_function(env, object, "c2_mem_ffi_response_pool_new", response_pool_new, symbols) ||
        !set_function(env, object, "c2_mem_ffi_response_pool_destroy", response_pool_destroy, symbols) ||
        !set_function(env, object, "c2_mem_ffi_response_pool_read", response_pool_read, symbols) ||
        !set_function(env, object, "c2_mem_ffi_response_pool_release", response_pool_release, symbols)) {
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
