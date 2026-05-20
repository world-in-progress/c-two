#ifndef C2_MEM_FFI_H
#define C2_MEM_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define C2_MEM_FFI_MAX_SHM_PREFIX_LEN 24u
#define C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS 16u
#define C2_MEM_FFI_ABI_VERSION 1u

typedef enum C2MemFfiStatus {
    C2_MEM_FFI_STATUS_OK = 0,
    C2_MEM_FFI_STATUS_NULL_POINTER = 1,
    C2_MEM_FFI_STATUS_INVALID_ARGUMENT = 2,
    C2_MEM_FFI_STATUS_POOL_ERROR = 3,
    C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER = 4,
} C2MemFfiStatus;

/* Non-dedicated buddy block descriptor. is_dedicated must be 0. */
typedef struct C2MemFfiRequestBlock {
    uint16_t segment_index;
    uint8_t is_dedicated;
    uint8_t reserved;
    uint32_t offset;
    uint32_t byte_length;
} C2MemFfiRequestBlock;

/* Non-dedicated buddy block descriptor. is_dedicated must be 0. */
typedef struct C2MemFfiResponseBlock {
    uint16_t segment_index;
    uint8_t is_dedicated;
    uint8_t reserved;
    uint32_t offset;
    uint32_t byte_length;
} C2MemFfiResponseBlock;

typedef struct C2MemFfiRequestPool C2MemFfiRequestPool;
typedef struct C2MemFfiResponsePool C2MemFfiResponsePool;

uint32_t c2_mem_ffi_abi_version(void);

/* Creates a non-dedicated buddy request pool. max_segments must be 1..16. */
C2MemFfiStatus c2_mem_ffi_request_pool_new(
    const char *prefix,
    uint32_t segment_size,
    uint16_t max_segments,
    uint32_t min_block_size,
    C2MemFfiRequestPool **out_pool);

void c2_mem_ffi_request_pool_destroy(C2MemFfiRequestPool *pool);

C2MemFfiStatus c2_mem_ffi_request_pool_prefix_len(
    const C2MemFfiRequestPool *pool,
    size_t *out_len);

C2MemFfiStatus c2_mem_ffi_request_pool_prefix_copy(
    const C2MemFfiRequestPool *pool,
    char *dst,
    size_t dst_len,
    size_t *out_written);

C2MemFfiStatus c2_mem_ffi_request_pool_segment_count(
    const C2MemFfiRequestPool *pool,
    size_t *out_count);

C2MemFfiStatus c2_mem_ffi_request_pool_segment_name_len(
    const C2MemFfiRequestPool *pool,
    size_t segment_index,
    size_t *out_len);

C2MemFfiStatus c2_mem_ffi_request_pool_segment_name_copy(
    const C2MemFfiRequestPool *pool,
    size_t segment_index,
    char *dst,
    size_t dst_len,
    size_t *out_written);

C2MemFfiStatus c2_mem_ffi_request_pool_segment_data_size(
    const C2MemFfiRequestPool *pool,
    size_t segment_index,
    uint32_t *out_size);

C2MemFfiStatus c2_mem_ffi_request_pool_write(
    C2MemFfiRequestPool *pool,
    const uint8_t *data,
    size_t data_len,
    C2MemFfiRequestBlock *out_block);

C2MemFfiStatus c2_mem_ffi_request_pool_read_local(
    const C2MemFfiRequestPool *pool,
    C2MemFfiRequestBlock block,
    uint8_t *dst,
    size_t dst_len,
    size_t *out_read);

C2MemFfiStatus c2_mem_ffi_request_pool_release(
    C2MemFfiRequestPool *pool,
    C2MemFfiRequestBlock block);

C2MemFfiStatus c2_mem_ffi_request_pool_forget_consumed(
    C2MemFfiRequestPool *pool,
    C2MemFfiRequestBlock block);

/* Opens a non-dedicated buddy response pool over Rust server segments. */
C2MemFfiStatus c2_mem_ffi_response_pool_new(
    const char *prefix,
    uint32_t segment_size,
    uint16_t max_segments,
    uint32_t min_block_size,
    C2MemFfiResponsePool **out_pool);

void c2_mem_ffi_response_pool_destroy(C2MemFfiResponsePool *pool);

/*
 * Copies a valid non-dedicated response block from the Rust server buddy pool
 * into caller-owned memory. Successfully read blocks are tracked so destroy can
 * release them as a leak guardrail when callers forget to release explicitly.
 */
C2MemFfiStatus c2_mem_ffi_response_pool_read(
    C2MemFfiResponsePool *pool,
    C2MemFfiResponseBlock block,
    uint8_t *dst,
    size_t dst_len,
    size_t *out_read);

/*
 * Releases a valid read or unread non-dedicated response block exactly once.
 * This is valid before read when the caller cannot allocate or copy the
 * response payload after receiving block metadata.
 */
C2MemFfiStatus c2_mem_ffi_response_pool_release(
    C2MemFfiResponsePool *pool,
    C2MemFfiResponseBlock block);

#ifdef __cplusplus
}
#endif

#endif
