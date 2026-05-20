#include <node_api.h>

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "c2_mem_ffi.h"

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

typedef struct C2MemFfiNodeSymbols {
    void *library;
    abi_version_fn abi_version;
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
    uint32_t *offset,
    uint32_t *byte_length) {
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
    if (!get_named_property(env, value, "offset", &property) ||
        !read_uint32_arg(env, property, "block.offset must be a u32 integer.", offset)) {
        return false;
    }
    if (!get_named_property(env, value, "byteLength", &property) ||
        !read_uint32_arg(env, property, "block.byteLength must be a u32 integer.", byte_length)) {
        return false;
    }
    return true;
}

static bool read_request_block(napi_env env, napi_value value, C2MemFfiRequestBlock *block) {
    uint16_t segment_index = 0;
    uint8_t is_dedicated = 0;
    uint32_t offset = 0;
    uint32_t byte_length = 0;
    if (!read_block_common(env, value, &segment_index, &is_dedicated, &offset, &byte_length)) {
        return false;
    }
    block->segment_index = segment_index;
    block->is_dedicated = is_dedicated;
    block->reserved = 0;
    block->offset = offset;
    block->byte_length = byte_length;
    return true;
}

static bool read_response_block(napi_env env, napi_value value, C2MemFfiResponseBlock *block) {
    uint16_t segment_index = 0;
    uint8_t is_dedicated = 0;
    uint32_t offset = 0;
    uint32_t byte_length = 0;
    if (!read_block_common(env, value, &segment_index, &is_dedicated, &offset, &byte_length)) {
        return false;
    }
    block->segment_index = segment_index;
    block->is_dedicated = is_dedicated;
    block->reserved = 0;
    block->offset = offset;
    block->byte_length = byte_length;
    return true;
}

static napi_value make_request_block(napi_env env, C2MemFfiRequestBlock block) {
    napi_value object;
    if (!check_napi(napi_create_object(env, &object))) {
        return NULL;
    }
    if (!set_named_uint32(env, object, "segmentIndex", block.segment_index) ||
        !set_named_uint32(env, object, "offset", block.offset) ||
        !set_named_uint32(env, object, "byteLength", block.byte_length) ||
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

static bool load_symbol(napi_env env, void *library, const char *name, void **out) {
    dlerror();
    void *symbol = dlsym(library, name);
    const char *error = dlerror();
    if (error != NULL || symbol == NULL) {
        char message[512];
        snprintf(message, sizeof(message), "Failed to load c2-mem-ffi symbol %s: %s", name, error == NULL ? "not found" : error);
        throw_error(env, message);
        return false;
    }
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
    void *library = dlopen(library_path, RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) {
        char message[512];
        snprintf(message, sizeof(message), "Failed to load c2_mem_ffi shared library: %s", dlerror());
        free(library_path);
        return throw_error(env, message);
    }
    free(library_path);
    C2MemFfiNodeSymbols *symbols = (C2MemFfiNodeSymbols *)calloc(1, sizeof(C2MemFfiNodeSymbols));
    if (symbols == NULL) {
        dlclose(library);
        return throw_error(env, "Out of memory.");
    }
    symbols->library = library;
    if (!load_all_symbols(env, symbols)) {
        dlclose(library);
        free(symbols);
        return NULL;
    }
    uint32_t loaded_abi_version = symbols->abi_version();
    if (loaded_abi_version != C2_MEM_FFI_ABI_VERSION) {
        char message[256];
        snprintf(message, sizeof(message), "c2_mem_ffi ABI version %u is incompatible with expected version %u.", loaded_abi_version, C2_MEM_FFI_ABI_VERSION);
        dlclose(library);
        free(symbols);
        return throw_error(env, message);
    }
    napi_value object;
    if (!check_napi(napi_create_object(env, &object))) {
        dlclose(library);
        free(symbols);
        return NULL;
    }
    if (!set_function(env, object, "c2_mem_ffi_request_pool_new", request_pool_new, symbols) ||
        !set_function(env, object, "c2_mem_ffi_abi_version", abi_version_callback, symbols) ||
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
        dlclose(library);
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
