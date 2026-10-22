# Relay Large Payload Transport Design

**Date:** 2026-05-14
**Status:** Phase 1 and Phase 2 request path implemented. Phase 3 benchmark
truthfulness and large-relay row enablement implemented with explicit
request/response materialization labels. Remote payload chunk sizing is a
Rust-core transport policy shared by relay HTTP today and future remote
protocols; full HTTP response streaming remains a future optimization.

## Scope

This document covers the relay data-plane path for large CRM payloads:

- Python SDK serialized call payloads entering Rust through PyO3.
- Relay-aware HTTP clients sending CRM calls through `c2-http`.
- The Rust relay receiving HTTP CRM calls and forwarding them to upstream IPC.
- Protocol-neutral remote payload batching policy used by relay HTTP today and
  intended for future long-lived TCP or HTTP/2 remote data planes.
- `c2-ipc` request transport selection across inline, buddy SHM, preallocated
  SHM, and chunked fallback.
- Benchmark labeling and guardrails for relay large-payload results.

This is a Rust-core transport design. Language SDKs may expose typed facades and
serialized bytes, but they must not own relay routing, payload selection,
streaming policy, retry semantics, or SHM lifecycle.

## Original Code Evidence

This section records the pre-implementation evidence that motivated the
change. It is intentionally historical; see "Implementation Status" for the
current source-to-sink state.

The original relay path was not a valid large-payload benchmark path.

- `core/transport/c2-http/src/relay/router.rs:951-997` uses
  `call_handler(..., body: Bytes)` and then calls
  `client.call(&route_name, &method_name, &body).await`.
- `core/transport/c2-ipc/src/client.rs:579-598` documents
  `IpcClient::call(...)` as "inline path only" and delegates to
  `call_inline(...)`.
- `core/transport/c2-ipc/src/client.rs:600-638` has the automatic transport
  selection in `call_full(...)`, not in `call(...)`.
- `core/transport/c2-ipc/src/sync_client.rs:73-82` already maps the synchronous
  `SyncClient::call(...)` API to `inner.call_full(...)`.
- `sdk/python/native/src/client_ffi.rs:384-426` has a PyO3 direct-IPC fast path
  that writes large Python payloads directly into client SHM before invoking
  `SyncClient::call_prealloc(...)`; this path must not be removed accidentally
  when unifying `IpcClient::call`.
- `core/transport/c2-http/src/client/client.rs:103-132` sends HTTP request
  bodies with `.body(data.to_vec())` and materializes response bodies with
  `resp.bytes().await`.
- `sdk/python/native/src/http_ffi.rs:62-72` clones the Python payload into a
  Rust `Vec<u8>` before calling the relay-aware HTTP client.
- `core/foundation/c2-config/src/ipc.rs:85-93` defines
  `ServerIpcConfig.max_payload_size`, while
  `core/foundation/c2-config/src/ipc.rs:102-107` shows that
  `ClientIpcConfig` has no matching request payload limit today.
- `core/transport/c2-ipc/src/client.rs:280-294` shows the relay-held
  `IpcClient` owns only a `ClientIpcConfig` plus private client SHM pool state.
  `core/transport/c2-ipc/src/client.rs:810-816` keeps preallocated SHM send as
  `pub(crate)`, so `c2-http` cannot and must not reach into IPC internals.
- `core/transport/c2-http/src/relay/router.rs:997-1041` materializes upstream
  IPC responses before returning HTTP, and
  `core/transport/c2-ipc/src/response.rs:49-76` documents that relay response
  forwarding currently copies SHM or handle responses into owned `Vec<u8>`.
- `sdk/python/benchmarks/three_mode_benchmark.py:68-80` declares payloads up to
  1GB, but `sdk/python/benchmarks/three_mode_benchmark.py:457` currently skips
  relay above 100MB.

There is one especially important consequence: with `body: Bytes`, Axum has
already materialized the full HTTP request body before the handler body runs.
Therefore the current header validation inside
`call_handler(...)` is too late to prove "validate contract before allocating or
reading a large payload". The relay handler signature itself must change before
we can claim that invariant.

## Implementation Status

As of 2026-05-14:

- `IpcClient::call(...)` is the single public semantic IPC call. It selects
  inline, buddy SHM, or chunked transport through a shared
  `RequestTransportKind` selector.
- Public `call_full(...)` has been removed from production IPC code.
  `SyncClient::call(...)`, PyO3 direct IPC fallback, and relay forwarding all
  route through the canonical call surface.
- `IpcClient::call_sized_stream(...)` is the safe Rust-core streaming boundary
  for known-length producers. It validates the route method and route payload
  limit, then streams into client SHM when a pool is available or uses checked
  chunked IPC fallback otherwise.
- `SyncClient::connect(...)` preserves the supplied `ClientIpcConfig` even when
  no external client pool is supplied. The PyO3 direct-IPC preallocated SHM fast
  path still exists, but `SyncClient::call_prealloc(...)` now enforces the
  route `max_payload_size` and releases any preallocated SHM on pre-dispatch
  validation failure.
- `IpcClient::with_config(...)` creates an owned client SHM pool when
  `ClientIpcConfig.pool_enabled` is true. Relay production registration,
  command registration, and lazy reconnect now use this constructor, so
  known-length large relay requests can reach upstream IPC as
  `RequestData::Shm` without `c2-http` touching `MemPool` internals.
- Route payload capability is Rust-owned and propagated through IPC handshake,
  pending-route attestation, relay registration, route table state, resolve
  responses, route digest, gossip route announce, digest diff, and full sync.
- Relay data-plane handlers now accept `Body`, validate expected CRM headers
  and route payload capability before polling, reject invalid/duplicate
  `Content-Length`, reject oversized known-length bodies before polling, and
  keep unknown-length bodies bounded to the small fallback limit.
- Response forwarding is still materialized through `ResponseData` to HTTP
  bytes. Phase 3 must keep benchmark labels honest or implement response
  streaming before claiming bidirectional relay streaming.
- The three-mode benchmark now reports
  `relay_http_request_stream_to_shm_response_materialized` and splits the path
  into client request HTTP materialized, relay request-to-IPC streamed, and
  response HTTP materialized. It no longer skips 500MB and 1GB relay rows by a
  hard-coded `<= 100MB` cap.
- Full local relay benchmark evidence for this path is recorded in
  `docs/reports/2026-05-14-full-http-relay-benchmark.md`. That run completed
  all declared relay rows, including 500MB and 1GB.
- `C2_REMOTE_PAYLOAD_CHUNK_SIZE` is resolved and validated by `c2-config`, not
  Python. The setting is deliberately named `REMOTE` rather than `RELAY`: it
  controls C-Two's protocol-level payload batching for remote data planes. HTTP
  relay uses it for outbound request bodies and materialized response-body
  streaming today; future TCP or HTTP/2 remote transports must consume the same
  resolved config instead of adding protocol-specific chunk variables.
- `C2_IPC_CHUNK_SIZE` remains IPC wire chunking and reassembly policy.
  `C2_REMOTE_PAYLOAD_CHUNK_SIZE` is a separate remote transport body batching
  policy. Neither variable promises kernel TCP packet sizes, HTTP/1 chunk
  boundaries, or HTTP/2 DATA frame sizes.

## Decisions

### 1. Make IPC `call` the canonical semantic request API

`IpcClient::call(...)` should mean "perform one CRM call using the configured
transport policy". It must select buddy SHM, chunked, or inline as appropriate.
The current inline-only behavior must move behind an explicitly private helper
such as `call_inline(...)`.

`call_full(...)` should not remain as a second public semantic call. Keeping both
names would invite future relay or SDK code to choose the wrong API. During the
implementation phase, either remove `call_full(...)` or make it a temporary
private alias with a guardrail test that prevents production call sites from
using it.

### 2. Keep preallocated SHM as a separate internal API

Preallocated SHM is not a separate user-visible call semantic. It is an
optimization for producers that can write directly into the client pool before
sending the IPC frame.

The existing PyO3 direct-IPC fast path uses this to avoid an extra Rust-owned
payload copy. The future relay HTTP streaming path should also use this shape
when it can validate the content length and allocate once before consuming the
body stream.

The API should stay explicit, internal, and lifecycle-oriented, for example
`call_with_preallocated_shm(...)`, not a second "full call" entry point.

### 3. Split relay large-payload work into two honest phases

Phase 1 fixes relay-to-upstream IPC selection:

- Relay forwarding must use the canonical IPC semantic call.
- Large payloads received by the relay can use upstream IPC SHM or chunked paths.
- HTTP request and response bodies may still be materialized in relay/client
  memory.

Phase 1 is enough to remove the current "relay always forwards inline IPC"
limitation, but it is not enough to claim memory-pressure relief for HTTP.
Benchmarks enabled after Phase 1 must be labeled as materialized HTTP relay
benchmarks.

Phase 2 adds request-side HTTP streaming:

- The relay must validate route and expected CRM headers before consuming the
  body stream.
- The relay must use `Content-Length` when present only as an untrusted size
  hint, validate it against configured payload limits, and preallocate SHM only
  after that validation.
- The relay must write each arriving HTTP chunk directly into the chosen SHM
  allocation when the payload is eligible.
- Unknown-length HTTP bodies must not be allowed to grow unbounded in memory.
  Phase 2 accepts them only while they fit the configured small-payload buffer;
  once that bound would be exceeded, the relay rejects instead of forwarding a
  large streaming call.

### 4. Do not introduce `X-C2-Payload-Size` as the primary size contract

HTTP already has `Content-Length`. A custom size header would duplicate a
standard transport contract and create disagreement cases. If a future
diagnostic header is needed, it must be cross-checked against `Content-Length`
and the actual streamed byte count. It must not drive allocation by itself.

### 5. Keep SDK call APIs simple

Python currently exposes a single `CRMProxy.call(method_name, data)` path for
serialized cross-process calls. That shape should remain. Internal Rust should
choose the transport strategy.

Do not add public Python APIs like `call_full`, `call_shm`, or
`call_streaming`. The same principle must hold for future C++, Go, Fortran, and
Java SDKs: SDKs pass a serialized payload or a future replayable/streamable
payload source; Rust core owns transport selection.

### 6. Treat payload limits as upstream route capabilities

The relay must not invent a Python-side or relay-local payload-size policy for a
CRM route. The data-plane limit used before reading an HTTP body should be the
upstream IPC server's resolved request payload capability.

Current code does not expose that capability to the relay-held `IpcClient`:
`ServerIpcConfig` has `max_payload_size`, but `ClientIpcConfig` does not. Phase 2
therefore needs a Rust-owned propagation point, such as IPC attestation or route
registration metadata, that lets the owning relay store a per-route
`max_request_payload_size` before accepting data-plane calls.

If this capability is not available for a route, the relay must not claim
header-before-body payload-limit enforcement for that route. In the 0.x line,
the preferred implementation is a clean metadata extension, not a compatibility
fallback that silently uses an unrelated default.

### 7. Expose a safe IPC streaming/preallocation boundary

The future relay streaming path should not manipulate `MemPool` or
`PoolAllocation` directly from `c2-http`. `c2-ipc` should expose a safe internal
transport API for producers that have a stream, for example a prepared call or
streaming request writer that:

- validates route and method once;
- chooses SHM, chunked, or bounded inline behavior from `ClientIpcConfig`;
- owns any allocation until finish or abort;
- checks `Content-Length` and actual byte count before sending;
- releases allocations on all error paths;
- returns `ResponseData` through the same pending-call machinery as
  `IpcClient::call(...)`.

This keeps transport policy in `c2-ipc` and prevents the relay from becoming a
second SHM implementation.

### 8. Keep request and response streaming claims separate

Request streaming and response streaming are separate work items. Phase 2 can
reduce relay request-body memory pressure without fixing response
materialization. Phase 3 must either implement response streaming or keep
benchmark labels explicit enough to say that responses are still materialized.

## Frozen Invariants

1. **Single semantic IPC call invariant**
   - Any production CRM request through `c2-ipc` uses one canonical semantic
     call API.
   - Prevents relay or SDK code from accidentally choosing inline-only behavior.

2. **No relay inline-only upstream invariant**
   - The relay HTTP data-plane must not forward upstream IPC calls through an
     inline-only API.
   - Prevents large relay payloads from bypassing SHM/chunked transport.

3. **Header-before-body invariant**
   - Relay data-plane route and expected CRM validation must happen before the
     HTTP body is consumed or any large allocation is made.
   - Prevents unauthenticated or contract-mismatched large bodies from forcing
     memory pressure before rejection.

4. **Bounded HTTP body invariant**
   - The relay must not materialize an unbounded HTTP request body.
   - Prevents memory blowups from `DefaultBodyLimit::disable()` plus
     `body: Bytes`.

5. **Checked size invariant**
   - All conversions from `usize` or `u64` payload lengths into wire fields must
     be checked before allocation or frame construction.
   - Prevents truncation around `BuddyPayload.data_size: u32` and inline frame
     length fields.

6. **Allocation ownership invariant**
   - Any SHM allocation created for a request must have a single release path on
     send failure, receive failure, CRM failure, and success.
   - Prevents leaks and double-free behavior in preallocated relay and PyO3
     paths.

7. **Replay-aware retry invariant**
   - Relay-aware retry and stale-route fallback may only retry bodies that are
     replayable.
   - Prevents partially consumed streaming bodies from being silently reused on
     another route.

8. **SDK-neutral invariant**
   - Transport selection, relay streaming, and retry policy live in Rust core or
     native FFI, not in Python-only code.
   - Prevents future C++, Go, Fortran, and Java SDKs from reimplementing
     inconsistent transport behavior.

9. **Benchmark truthfulness invariant**
   - Benchmarks must label request and response transport behavior separately.
   - Prevents benchmark results from being interpreted as proof for an
     optimization path that was not actually exercised.

10. **Route payload capability invariant**
    - The relay data-plane payload limit comes from a Rust-owned upstream route
      capability, not from Python SDK policy or an unrelated relay default.
    - Prevents oversized HTTP bodies from being accepted because the relay did
      not know the upstream server's real limit.

11. **IPC ownership boundary invariant**
    - `c2-http` may request a streaming/preallocated IPC call, but it must not
      allocate, free, or write IPC SHM through private pool internals.
    - Prevents duplicate SHM lifecycle rules across crates.

12. **Response-path honesty invariant**
    - Any relay benchmark or implementation phase must distinguish request
      streaming from response streaming.
    - Prevents a request-side optimization from being reported as full
      bidirectional streaming.

13. **Remote chunk policy invariant**
    - Remote payload chunk size is generated, validated, stored, and forwarded
      through Rust `c2-config` / runtime / transport state under
      `C2_REMOTE_PAYLOAD_CHUNK_SIZE`.
    - Prevents HTTP relay, future TCP, future HTTP/2, or individual SDKs from
      growing divergent chunk-size knobs or Python-only defaults.

## Source-To-Sink Matrix

| Invariant | Producer | Storage | Consumer | Network / Wire | FFI / SDK | Test / Support | Legacy / Bypass risk |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Single semantic IPC call | `IpcClient::call` | `ClientIpcConfig`, route method table | relay router, `SyncClient`, PyO3 | IPC frames: inline, buddy, chunked | `PyRustClient.call` remains method-only | `c2-ipc` unit and integration tests | Public `call_full` remains callable |
| No relay inline-only upstream | relay `call_handler` | acquired IPC lease | upstream `SyncClient` / `IpcClient` | HTTP to IPC bridge | none SDK-specific | relay integration test with payload above `shm_threshold` | `client.call` remains inline-only |
| Header-before-body | relay router request parts | route table and expected contract | streaming body consumer | HTTP request headers before body | relay-aware client headers | test body stream that fails if polled before validation | handler keeps `body: Bytes` |
| Bounded HTTP body | streaming extractor / `Request<Body>` | configured max payload limit | SHM writer or chunk forwarder | HTTP chunk stream | future streamable SDK source | oversized and unknown-length tests | `DefaultBodyLimit::disable()` with `Bytes` |
| Checked size | IPC transport selector | wire frame builders | server decoder | buddy `data_size`, inline length, chunk metadata | PyO3 prealloc and relay prealloc | boundary tests over u32 and frame limits | unchecked `as u32` / `as u64` casts |
| Allocation ownership | SHM allocator | client pool allocation | `call_with_prealloc` / streaming IPC writer | buddy frame coordinates | PyO3 direct IPC, relay streamer | failure-injection tests | separate PyO3 and relay cleanup rules diverge; prealloc validation returns before freeing |
| Replay-aware retry | relay-aware HTTP client | route cache and excluded routes | retry loop | HTTP body send | Python bytes now; future stream source later | stale-route retry tests with replayable and non-replayable body | retry loop consumes stream once then retries |
| SDK-neutral | Rust core config and transport | runtime session and pools | language bindings | IPC / HTTP | Python, future SDKs | SDK boundary tests | Python-only `call_full` or mode knobs |
| Benchmark truthfulness | benchmark harness | result table and labels | performance reports | selected relay path | Python benchmark script | smoke benchmark with path counters | enabling 1GB relay before path evidence |
| Route payload capability | IPC server config or attestation | owning relay route state | relay data-plane pre-body validator | IPC registration/attestation; optionally resolve metadata | all SDKs observe via Rust route resolution | registration, mesh, and data-plane tests | relay falls back to unrelated default |
| IPC ownership boundary | `c2-ipc` prepared/streaming call API | IPC client pool and pending map | relay streaming bridge | IPC request frames | PyO3 direct IPC and relay share ownership model | failure-injection tests | `c2-http` writes `MemPool` directly |
| Response-path honesty | `ResponseData` from upstream IPC | server pool, reassembly pool, HTTP body | relay HTTP response | IPC response to HTTP response | HTTP FFI materializes today | benchmark path counters | request streaming label hides response copy |
| Remote chunk policy | `c2-config` resolver from explicit override / env / default | `ResolvedRuntimeConfig`, `ResolvedRelayClientConfig`, `RelayConfig`, HTTP client pool entry | relay-aware HTTP client, explicit HTTP client, relay response materializer, future remote transports | HTTP body stream today; future TCP / HTTP2 payload batches | PyO3 only forwards native-resolved value; SDK code may expose typed override | Rust config tests, HTTP pool mismatch test, relay response chunk test, Python resolver test | protocol-specific env vars, Python-only tables, HTTP pool reuse across mismatched chunk policy |

Mesh note: Phase 2 enforcement can be local to the owning relay data-plane
handler. If route payload capabilities are exposed to clients or used for route
selection by peer relays, the fields must be carried through route registration,
route table storage, full sync, digest diff, and test fixtures. A local-only
field must not leak into resolve responses as if mesh propagation were complete.

## Targeted Call-Site Impact Map

Changing `IpcClient::call(...)` is not local. The implementation phase must
update and review these linked surfaces together.

### `core/transport/c2-ipc/src/client.rs`

- Change `IpcClient::call(...)` from inline-only to canonical policy-selected
  behavior.
- Remove or privatize `call_full(...)`.
- Keep `call_inline(...)`, `call_buddy(...)`, `call_chunked(...)`, and
  `call_with_prealloc(...)` as implementation details.
- Replace unchecked payload-size casts with checked conversions before building
  inline or buddy frames.
- Keep `call_with_prealloc(...)` internal and document its ownership contract;
  a future rename such as `call_with_preallocated_shm(...)` is optional, not a
  prerequisite, once the release semantics are mechanically covered.
- Add a safe prepared/streaming request API for `c2-http` instead of exposing
  `MemPool`, `PoolAllocation`, or private route tables across crate boundaries.
- Extract transport choice into a testable selector, such as
  `RequestTransportKind`, so Phase 1 tests can prove selected behavior without
  depending on a live UDS connection.

### `core/transport/c2-ipc/src/sync_client.rs`

- Change `SyncClient::call(...)` to delegate to the canonical
  `IpcClient::call(...)`, not `call_full(...)`.
- Keep `pool_alloc_and_write(...)` and `call_prealloc(...)` for PyO3 and future
  relay streaming producers.
- `SyncClient::connect(address, None, config)` must use
  `IpcClient::with_config(...)`, not `IpcClient::new(...)`, so caller-supplied
  thresholds and pool policy are not silently replaced by defaults.
- `call_prealloc(...)` must enforce the same route `max_payload_size` contract
  as canonical `IpcClient::call(...)` and must free the already-written SHM
  allocation on all validation failures before dispatch.
- Update comments that currently describe fallback as "inline call"; the
  fallback is canonical IPC call selection.

### `sdk/python/native/src/client_ffi.rs`

- Preserve the direct PyO3 preallocated SHM fast path at
  `call_sync_client(...)`.
- Ensure the fallback call invokes the canonical `SyncClient::call(...)`.
- Keep Python-visible `PyRustClient.call(method_name, data)` route-bound and
  method-only; do not reintroduce route-name parameters.
- Add SDK-level evidence that an oversized direct-IPC request through the PyO3
  preallocated SHM fast path is rejected before the Python resource method runs.

### `core/transport/c2-http/src/relay/router.rs`

- Phase 1: replace relay upstream forwarding with the canonical IPC semantic
  call after body materialization.
- Phase 2: replace `body: Bytes` with a request/parts plus streaming body
  extractor so expected CRM validation and route acquisition happen before body
  consumption.
- Keep expected CRM checks before any streaming allocation or body polling.
- Use the acquired route's Rust-owned `max_request_payload_size` before polling
  the body. `Content-Length` larger than this limit must fail without reading
  the body.
- Verify that the actual streamed byte count exactly matches `Content-Length`
  when present. Short reads, extra bytes, and stream errors must abort and
  release any prepared IPC allocation.
- Add explicit payload limits; do not rely on disabled Axum body limits for the
  data-plane route.

### `core/transport/c2-http/src/relay/{authority,state,route_table,types}.rs`

- Add route payload capability storage only if the data-plane validator needs it
  before body consumption.
- If the capability is local-only, keep it out of mesh resolve responses.
- If clients or peer relays consume it, propagate it through registration,
  route table state, full sync, digest diff, and relay test fixtures in the same
  implementation slice.

### `core/transport/c2-http/src/client/client.rs`

- Request body construction uses `C2_REMOTE_PAYLOAD_CHUNK_SIZE` through the
  Rust `RelayAwareClientConfig` / `HttpClient` policy, not a hard-coded HTTP
  constant.
- Current outbound client payloads are still replayable because the SDK passes
  serialized bytes; chunking changes HTTP body batching, not the SDK call
  semantic.
- Phase 3 response streaming must not break stale-route retry behavior.
- If a streaming request body abstraction is introduced, route retry and
  fallback must check replayability before each attempt.

### `core/foundation/c2-config` remote payload policy

- `C2_REMOTE_PAYLOAD_CHUNK_SIZE` is the single env key for remote payload body
  batching.
- Default: `1048576` bytes.
- Hard maximum: `134217728` bytes.
- Zero is invalid.
- The validator and constants belong to a protocol-neutral config module. Relay
  config may store the resolved value, but must not be the authority for the
  setting.
- Future TCP / HTTP2 remote transports must accept this resolved value through
  Rust runtime or transport config. They must not introduce
  `C2_TCP_CHUNK_SIZE`, `C2_HTTP2_CHUNK_SIZE`, or SDK-specific defaults unless a
  separate lower-level protocol knob is explicitly justified and documented.

### `core/transport/c2-http/src/client/relay_aware.rs`

- Preserve expected-contract route resolution and headers.
- If a streaming request body abstraction is introduced, route retry and
  fallback must check replayability before each attempt.
- Existing tests around stale-route retry must be extended to cover large body
  behavior.

### `sdk/python/native/src/http_ffi.rs`

- Current PyO3 HTTP FFI copies `data` into an owned Rust `Vec<u8>`.
- Phase 1 may keep this because the benchmark should be labeled as materialized
  HTTP.
- Phase 2 can reduce the Rust-side copy, but it cannot remove the Python
  serialized payload allocation until the SDK serialization contract supports a
  streamable or buffer-backed source.

### `sdk/python/src/c_two/transport/client/proxy.py`

- Keep the Python proxy API as `call(method_name, data)`.
- Do not expose transport strategy knobs or a `call_full` public surface.
- Existing test doubles that implement `.call(method_name, data)` should remain
  valid.

### `sdk/python/src/c_two/crm/transferable.py`

- The standard cross-process path should continue to call
  `client.call(method_name, serialized_args)`.
- No CRM-level API should branch on relay vs IPC payload strategy.

### `sdk/python/benchmarks/three_mode_benchmark.py`

- Do not simply remove the `<= 100MB` relay guard after Phase 1 and present the
  numbers as streaming results.
- Add labels or columns that distinguish:
  - `relay_http_materialized_ipc_selected`
  - `relay_http_request_stream_to_shm_response_materialized`
  - `relay_http_bidirectional_streaming`
- Only enable 500MB and 1GB relay rows by default after there is path evidence
  for the selected label.

## Implementation Phases

### Phase 1: Canonical IPC call semantics

Required changes:

- Move automatic transport selection into `IpcClient::call(...)`.
- Remove or lock down `call_full(...)`.
- Update `SyncClient::call(...)` and relay forwarding.
- Add guardrails preventing relay code from calling inline-only APIs.
- Extract or expose enough path-selection evidence to test canonical selection
  without depending only on comments or threshold arithmetic.

Acceptance evidence:

- A focused Rust test proves `IpcClient::call(...)` selects non-inline transport
  for payloads above `shm_threshold` when a pool is available.
- A relay integration test proves an HTTP relay call above `shm_threshold`
  reaches upstream IPC through the canonical selected path.
- A source guard fails if production relay code calls `call_inline`,
  `call_full`, or an inline-only helper.

### Phase 2: HTTP request streaming into upstream transport

Required changes:

- Replace `body: Bytes` in relay data-plane handlers.
- Validate expected CRM headers and route contract before polling the body.
- Propagate a Rust-owned per-route request payload limit to the owning relay, or
  block the streaming claim until that capability exists.
- Use `Content-Length` as an untrusted allocation hint and reject values above
  the route payload limit before polling the body.
- Stream HTTP chunks through a safe `c2-ipc` prepared/streaming call API, not by
  touching IPC pool internals from `c2-http`.
- Require `Content-Length` for large request-side streaming. Unknown-length
  bodies may be buffered only up to the configured inline/small-payload bound;
  once that bound would be exceeded, reject instead of growing memory or
  pretending SHM preallocation is possible.
- Check every wire-size conversion before frame construction.

Acceptance evidence:

- A test body stream records whether it was polled; contract mismatch must
  return before the first poll.
- A large known-length body writes incrementally into SHM without an
  intermediate full `Bytes` aggregation in the relay handler.
- `Content-Length` mismatch cases release the prepared IPC allocation and return
  an error without dispatching a partial CRM call.
- Unknown-length large bodies are rejected before unbounded buffering. They are
  not forwarded as large streaming calls until a separate chunk-to-IPC streaming
  implementation exists.
- Failure-injection tests prove SHM allocations are released on send failure,
  upstream failure, and client disconnect.
- A source guard proves production `c2-http` does not access `MemPool` or
  `PoolAllocation` directly.

### Phase 3: Relay response streaming and benchmark enablement

Required changes:

- Either implement HTTP response streaming from `ResponseData`, or explicitly
  keep the benchmark label as request-streaming-only. The current implementation
  chooses the latter.
- Add path counters, labels, or debug assertions that benchmark runs can report.
  The current implementation reports:
  - `relay_path = relay_http_request_stream_to_shm_response_materialized`
  - `relay_client_request_http = materialized`
  - `relay_request_to_ipc = streamed_to_upstream_ipc`
  - `relay_response_http = materialized`
- Enable 500MB and 1GB relay benchmark rows only for paths with verified labels.
  The current implementation enables all declared relay sizes under the explicit
  path label above.

Acceptance evidence:

- Benchmark output states whether request and response HTTP bodies were
  materialized or streamed.
- Relay benchmark does not silently skip large payloads unless the selected
  transport label is unavailable.
- If response streaming is implemented, tests prove SHM and handle responses are
  streamed to HTTP without `ResponseData::into_bytes()` materialization.

Current residual boundary: the relay still calls `ResponseData::into_bytes()` before
building the HTTP response, so no benchmark result from this phase should be
described as bidirectional streaming.

## Guardrails

Add mechanical guardrails with the implementation:

- `c2-ipc` production code must not expose two public semantic request APIs
  named `call` and `call_full`.
- `SyncClient::connect(address, None, config)` must not route through
  `IpcClient::new(...)`.
- `SyncClient::call_prealloc(...)` must reject payloads above the route
  `max_payload_size` and free the preallocated pool entry on that error path.
- Relay production code must not call `call_inline`.
- Relay data-plane production handler must not use `body: Bytes` after Phase 2.
- Relay streaming production code must not import or reference `MemPool` or
  `PoolAllocation` from `c2-http`.
- Public Python SDK must not expose `call_full`, route-name-bearing native
  calls, or transport-strategy call variants.
- HTTP expected CRM headers must remain required on probe and call paths.
- Payload-size conversions into wire fields must use checked conversions.
- If route payload capabilities are exposed beyond the owning relay, guardrails
  must check registration, route table, full sync, digest diff, and test support
  carriers together.

## Non-Goals

- No Python-only workaround for relay large payloads.
- No custom payload-size header as the primary allocation contract.
- No public SDK transport-mode call variants.
- No unknown-length large HTTP body streaming in Phase 2.
- No benchmark claim that request streaming proves response streaming.
- No compatibility shim for the old inline-only `IpcClient::call(...)`
  semantics.
