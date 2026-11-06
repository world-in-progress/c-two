#ifndef C2_MEM_FFI_H
#define C2_MEM_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define C2_MEM_FFI_ABI_VERSION 8u

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

typedef struct C2MemFfiLocalEndpointContext C2MemFfiLocalEndpointContext;
typedef struct C2MemFfiClientDomain C2MemFfiClientDomain;
typedef struct C2MemFfiEndpointContext C2MemFfiEndpointContext;
typedef struct C2MemFfiCall C2MemFfiCall;
typedef struct C2MemFfiBuffer C2MemFfiBuffer;
typedef struct C2MemFfiError C2MemFfiError;

/* Every callback owns its result. Transfer a member by setting it to NULL
 * before result_free. JSON is borrowed until result_free; buffers and errors
 * have their own owners. Callbacks run on a Rust executor thread. */
typedef struct C2MemFfiResult {
    C2MemFfiBuffer *buffer;
    C2MemFfiError *error;
    const char *json;
} C2MemFfiResult;
typedef void (*C2MemFfiComplete)(void *user, C2MemFfiResult *result);

uint32_t c2_mem_ffi_abi_version(void);

/*
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

/* ABI 8 native client domain. Captured endpoint context is borrowed only
 * during new; NULL inherits native configuration. The factory owns one Runtime
 * budget domain across all physical connections and reconnects. Free releases
 * only the factory reference; live endpoints/calls/buffers keep their owners.
 * options_json is a UTF-8 object with optional unsigned requestShmThreshold and
 * requestChunkSize byte counts. These Runtime overrides are fixed by new;
 * omitted values inherit native configuration. Unknown options are rejected. */
C2MemFfiStatus c2_mem_ffi_client_domain_new(const C2MemFfiLocalEndpointContext *context, const char *options_json, C2MemFfiClientDomain **out, C2MemFfiError **error);
void c2_mem_ffi_client_domain_free(C2MemFfiClientDomain *domain);
C2MemFfiStatus c2_mem_ffi_client_domain_stats(const C2MemFfiClientDomain *domain, C2MemFfiResult **out);
/* new performs no I/O. prepare connects using Rust's sole IPC reader/writer,
 * validates the complete contract and pins server identity and route UID.
 * JSON options/contract are UTF-8. Endpoint options are connectTimeoutMs,
 * expectedServerIdentity; transport policy belongs to the client domain.
 * prepare takes an optional expected route token JSON ({"routeUid":"..."});
 * NULL omits it. All input strings are borrowed for the call.
 * prepare/close complete asynchronously. Deactivate stops new calls only. */
C2MemFfiStatus c2_mem_ffi_endpoint_context_new(const C2MemFfiClientDomain *domain, const char *address, const char *options_json, C2MemFfiEndpointContext **out, C2MemFfiError **error);
C2MemFfiStatus c2_mem_ffi_endpoint_context_prepare(const C2MemFfiEndpointContext *endpoint, const char *route, const char *contract_json, const char *expected_route_token_json, C2MemFfiComplete complete, void *user);
C2MemFfiStatus c2_mem_ffi_endpoint_context_deactivate(const C2MemFfiEndpointContext *endpoint);
C2MemFfiStatus c2_mem_ffi_endpoint_context_close(const C2MemFfiEndpointContext *endpoint, C2MemFfiComplete complete, void *user);
void c2_mem_ffi_endpoint_context_free(C2MemFfiEndpointContext *endpoint);
/* begin precedes serialization. NULL call timeout means unlimited; a value is
 * one absolute budget for encoding, admission and the whole iteration.
 * charge_input reserves before dispatch copies the borrowed bytes. Every
 * asynchronous entry clones native ownership before returning. Exactly one
 * next may be pending. cancel/free submit stream cancellation without waiting.
 * A dispatched unary is never cancelled by dropping its facade. */
C2MemFfiStatus c2_mem_ffi_call_begin(const C2MemFfiEndpointContext *endpoint, const char *route, const char *method, uint8_t server_stream, const uint64_t *timeout_millis, C2MemFfiCall **out, C2MemFfiError **error);
C2MemFfiStatus c2_mem_ffi_call_charge_input(const C2MemFfiCall *call, uint64_t nbytes, C2MemFfiComplete complete, void *user);
C2MemFfiStatus c2_mem_ffi_call_dispatch(const C2MemFfiCall *call, const uint8_t *data, size_t len, C2MemFfiComplete complete, void *user);
C2MemFfiStatus c2_mem_ffi_call_next(const C2MemFfiCall *call, C2MemFfiComplete complete, void *user);
C2MemFfiStatus c2_mem_ffi_call_cancel(const C2MemFfiCall *call);
/* observe submits cancellation, then observes the original record; NULL is
 * infinite, zero observes once. JSON: {completed,error,completeItems}, with a
 * decimal u64 string for the count, or NULL if unavailable. Timeout bounds
 * observation only. The caller supplies the public default 30000 ms. */
C2MemFfiStatus c2_mem_ffi_call_observe(const C2MemFfiCall *call, const uint64_t *timeout_millis, C2MemFfiComplete complete, void *user);
C2MemFfiStatus c2_mem_ffi_call_check_deadline(const C2MemFfiCall *call, C2MemFfiError **error);
void c2_mem_ffi_call_free(C2MemFfiCall *call);
/* Local bytes only: transport leases have already ended. SDK checked owners
 * must be invalidated and external views detached before buffer_release. */
C2MemFfiStatus c2_mem_ffi_buffer_data(const C2MemFfiBuffer *buffer, const uint8_t **out_data, size_t *out_len);
void c2_mem_ffi_buffer_release(C2MemFfiBuffer *buffer);
/* Borrowed structured error JSON, never parse a display string for phase. */
const char *c2_mem_ffi_error_parts(const C2MemFfiError *error);
void c2_mem_ffi_error_free(C2MemFfiError *error);
void c2_mem_ffi_result_free(C2MemFfiResult *result);

#ifdef __cplusplus
}
#endif
#endif
