#ifndef C2_MEM_FFI_H
#define C2_MEM_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define C2_MEM_FFI_MAX_SHM_PREFIX_LEN 255u
#define C2_MEM_FFI_ABI_VERSION 7u

typedef enum C2MemFfiStatus {
    C2_MEM_FFI_STATUS_OK = 0,
    C2_MEM_FFI_STATUS_NULL_POINTER = 1,
    C2_MEM_FFI_STATUS_INVALID_ARGUMENT = 2,
    C2_MEM_FFI_STATUS_POOL_ERROR = 3,
    C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER = 4,
    C2_MEM_FFI_STATUS_IO_ERROR = 5,
    C2_MEM_FFI_STATUS_PERMISSION_DENIED = 6,
    C2_MEM_FFI_STATUS_IDENTITY_MISMATCH = 7,
} C2MemFfiStatus;

/* IPC 15 descriptor. Both kinds have nonzero generation and loan_id. */
typedef struct C2MemFfiBlock {
    uint16_t segment_index;
    uint8_t is_dedicated;
    uint8_t reserved;
    uint32_t generation;
    uint32_t offset;
    uint32_t byte_length;
    uint64_t loan_id;
} C2MemFfiBlock;

typedef struct C2MemFfiMemoryContext C2MemFfiMemoryContext;
typedef struct C2MemFfiRequest C2MemFfiRequest;
typedef struct C2MemFfiResponse C2MemFfiResponse;
typedef struct C2MemFfiWrite C2MemFfiWrite;
typedef struct C2MemFfiLocalEndpointContext C2MemFfiLocalEndpointContext;

uint32_t c2_mem_ffi_abi_version(void);

/*
 * Resolve a logical ipc:// address through c2-config's native endpoint owner.
 * The backend is selected automatically by the native platform.
 * Each call resolves process/.env configuration independently; a length/copy
 * pair is stable only when it uses the captured-context API below.
 */
C2MemFfiStatus c2_mem_ffi_local_endpoint_len(
    const char *address, size_t *out_len);
C2MemFfiStatus c2_mem_ffi_local_endpoint_copy(
    const char *address, char *dst, size_t dst_len, size_t *out_written);

/*
 * ABI 7 requires the complete context API, including both connect entries.
 *
 * Capture c2-config's complete immutable endpoint context exactly once.
 * NULL unix_root inherits process env/.env/platform default; a non-NULL UTF-8
 * root is the explicit code override and does not read process env/.env.
 * Unix roots are validated without
 * creating or checking directories; the caller prepares a root before bind.
 * Windows uses the current TokenUser SID and rejects every non-NULL root override.
 * This context does not change SHM names or pool layouts.
 *
 * On failure a valid out_context is set to NULL. The successful pointer is
 * owned by the caller and must be freed once after all queries have ended.
 * free(NULL) is safe; other pointers must be live results from capture.
 * Dangling, forged, and multiply freed pointers are not valid inputs.
 */
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_capture(
    const char *unix_root, C2MemFfiLocalEndpointContext **out_context);
void c2_mem_ffi_local_endpoint_context_free(
    C2MemFfiLocalEndpointContext *context);

/*
 * Names and namespace ids come from the same captured context, independent
 * of later env/.env changes. Length/written counts exclude the final NUL;
 * copy needs len + 1 bytes. On failure valid out_len/out_written are set to 0
 * and dst is unchanged. Input/output storage must not overlap. Non-NULL
 * context pointers must stay live for the entire query.
 */
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_name_len(
    const C2MemFfiLocalEndpointContext *context,
    const char *address,
    size_t *out_len);
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_name_copy(
    const C2MemFfiLocalEndpointContext *context,
    const char *address,
    char *dst,
    size_t dst_len,
    size_t *out_written);
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_namespace_id_len(
    const C2MemFfiLocalEndpointContext *context,
    size_t *out_len);
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_namespace_id_copy(
    const C2MemFfiLocalEndpointContext *context,
    char *dst,
    size_t dst_len,
    size_t *out_written);

/*
 * Connect through c2-local's verified directory addressing. Unix only.
 * Call on a worker. NULL timeout_millis selects the native five-second phase
 * guard; a non-NULL value supplies the remaining budget for this connection
 * attempt, including directory gate and identity verification. The caller
 * accounts for time spent queued before this call. context/address and a
 * non-NULL timeout_millis must remain live throughout the call; output
 * storage must not overlap. On success out_fd is an owned nonblocking,
 * close-on-exec descriptor to adopt or close exactly once. On failure it is
 * -1 and out_os_error is zero or native errno. Windows returns INVALID_ARGUMENT;
 * its Node connector uses the native Named Pipe API. Endpoint identity
 * verification refusals return IDENTITY_MISMATCH; other IO failures IO_ERROR.
 */
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_connect_unix(
    const C2MemFfiLocalEndpointContext *context, const char *address,
    const uint64_t *timeout_millis,
    int32_t *out_fd, int32_t *out_os_error);

/*
 * Connect through c2-local's verified TokenUser-scoped Named Pipe addressing
 * and transfer the pipe plus its pinned server process. Windows only; other
 * platforms return INVALID_ARGUMENT. Call on a worker: completion is bounded
 * by the same default or supplied remaining budget as the Unix bridge.
 * NULL timeout_millis selects the native five-second phase guard. The returned pipe
 * handle is overlapped-capable and not registered with any IOCP or foreign
 * event loop; adopt it before any registration. context/address must remain
 * live throughout the call; output storage must not overlap. On success
 * out_pipe and out_process hold raw HANDLE values to close or adopt exactly
 * once each. On failure both are 0 and out_os_error is zero or the native OS
 * error. Server identity verification happens before the first byte and
 * fails with IDENTITY_MISMATCH; other IO failures return IO_ERROR.
 */
C2MemFfiStatus c2_mem_ffi_local_endpoint_context_connect_windows(
    const C2MemFfiLocalEndpointContext *context, const char *address,
    const uint64_t *timeout_millis,
    uintptr_t *out_pipe, uintptr_t *out_process, int32_t *out_os_error);

/* Borrow a c2-local-verified OS socket/pipe, duplicating its reference.
 * On Windows connection is a HANDLE, not a CRT fd; process is its verified
 * server process pin. Unix requires process=0. Both Windows handles are
 * duplicated and retained through the last admitted read. This is not a connector.
 * The driver alone reads and writes; this API performs no transport I/O. */
C2MemFfiStatus c2_mem_ffi_memory_context_new(uintptr_t connection, uintptr_t process, C2MemFfiMemoryContext **out);
void c2_mem_ffi_memory_context_free(C2MemFfiMemoryContext *context);
/* Sweep idle local/peer buddy mappings throughout the connection, including
 * idle periods. Returns the next delay in Node-compatible milliseconds. */
C2MemFfiStatus c2_mem_ffi_memory_context_gc(const C2MemFfiMemoryContext *context, uint32_t *out_delay_millis);
/* Resolved C2_IPC_WRITE_TIMEOUT in seconds, including environment/.env precedence. */
C2MemFfiStatus c2_mem_ffi_memory_context_write_timeout(const C2MemFfiMemoryContext *context, double *out_seconds);
C2MemFfiStatus c2_mem_ffi_memory_context_prefix_len(const C2MemFfiMemoryContext *context, size_t *out);
C2MemFfiStatus c2_mem_ffi_memory_context_prefix_copy(const C2MemFfiMemoryContext *context, char *dst, size_t len, size_t *out);
/* Only after metadata ACK; NULL prefix disables response SHM. */
C2MemFfiStatus c2_mem_ffi_memory_context_finish_handshake(const C2MemFfiMemoryContext *context, const char *prefix);
C2MemFfiStatus c2_mem_ffi_request_prepare(const C2MemFfiMemoryContext *context, const uint8_t *data, size_t len, C2MemFfiRequest **out, C2MemFfiBlock *block);
void c2_mem_ffi_request_discard(C2MemFfiRequest *request);
/* Admit each received descriptor once. A decodable nonzero loan ID queues
 * release even on invalid descriptor or mapping failure; these errors fail only
 * that call. A zero loan ID deactivates new work; it cannot be released. */
C2MemFfiStatus c2_mem_ffi_response_admit(const C2MemFfiMemoryContext *context, C2MemFfiBlock block, C2MemFfiResponse **out);
/* Consumes response even on NULL/short dst or out. Copy -> end read -> release. */
C2MemFfiStatus c2_mem_ffi_response_consume(C2MemFfiResponse *response, uint8_t *dst, size_t len, size_t *out);
void c2_mem_ffi_response_discard(C2MemFfiResponse *response);
C2MemFfiStatus c2_mem_ffi_memory_context_receive_release(const C2MemFfiMemoryContext *context, const uint8_t *bytes, size_t len);
/* kind=0 business, 1 control, 2 releases only. Consumes request on every path.
 * NULL write means no releases queued. A write owns the unique writer until
 * complete or discard. It can contain multiple release frames plus one frame. */
C2MemFfiStatus c2_mem_ffi_write_begin(const C2MemFfiMemoryContext *context, C2MemFfiRequest *request, uint8_t kind, C2MemFfiWrite **out);
C2MemFfiStatus c2_mem_ffi_write_prefix_len(const C2MemFfiWrite *write, size_t *out);
C2MemFfiStatus c2_mem_ffi_write_prefix_copy(const C2MemFfiWrite *write, uint8_t *dst, size_t len);
C2MemFfiStatus c2_mem_ffi_write_start(C2MemFfiWrite *write);
/* success=1 proves all frames and flush finished; otherwise framing is broken. */
C2MemFfiStatus c2_mem_ffi_write_complete(C2MemFfiWrite *write, uint8_t success);
void c2_mem_ffi_write_discard(C2MemFfiWrite *write);
C2MemFfiStatus c2_mem_ffi_memory_context_deactivate(const C2MemFfiMemoryContext *context);
/* Only read EOF or Unix ECONNRESET; not local close/write failure/deadline. */
C2MemFfiStatus c2_mem_ffi_memory_context_peer_end(const C2MemFfiMemoryContext *context);
C2MemFfiStatus c2_mem_ffi_memory_context_try_close(const C2MemFfiMemoryContext *context, uint8_t *out);

#ifdef __cplusplus
}
#endif
#endif
