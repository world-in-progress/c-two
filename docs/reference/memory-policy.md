# C-Two transport memory policy

This page defines IPC memory budgets, allocation ownership, transport fallback and retained-payload accounting.

The Rust config resolver supplies defaults and validates overrides; Rust owns budget accounting, lazy allocation and tiered fallback, while SDKs expose typed overrides and read-only statistics. See the [configuration guide](configuration.md) for the full option reference.

On Windows, native pipe session facts and negotiated directional capabilities govern shared references. Different sessions use the existing owned/inline/chunked path, including copies of explicitly prepared SHM payloads with release through their original owner. The configured buddy-pool switch does not decide this reachability; reassembly may still use its own local memory pool.

## Finite budget cells

Both server and client IPC overrides accept these independent byte budgets:

| Override | Native default | Charged storage |
| --- | --- | --- |
| `shm_backing_budget_bytes` | 8 GiB | Locally created buddy and dedicated SHM mappings, including alignment |
| `file_backing_budget_bytes` | 16 GiB | Locally created file backing length |
| `live_reassembly_budget_bytes` | 8 GiB | Incomplete reassembly and assembled payloads not yet copied out; copied before the resource method runs or a hold is delivered, then released immediately |

Zero rejects every positive reservation. The entire `u64` range is valid; zero does not mean unlimited. Per-message `max_payload_size` and `max_reassembly_bytes` limits apply independently; `max_inflight_chunk_frames` limits the chunk frames a server processes at once across requests, and `max_pool_segments` limits pool segments. These budgets cover C-Two-owned IPC backing and reassembly. They do not bound Python or FastDB heaps, receiver-opened peer mappings or total process RSS, and HTTP buffers belong to the payload buffer budget below.

Server response pools, reassembly and prewarming share one server-direction budget. Cached request and reassembly pools in one Runtime's outgoing client domain share another. Directions and separate Runtimes are independent. Reservation occurs atomically before backing creation: refusal creates no mapping or file. Reusing free blocks in charged backing does not charge it again. Dedicated storage remains charged until its loan is settled.

## Payload buffer budget

Each Runtime and each c3 relay has one payload buffer budget for the payload buffers C-Two keeps for calls. Set it with Python `cc.set_call_execution_limits(payload_buffer_budget_bytes=...)`, the environment variable `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES` or the relay flag `--call-payload-buffer-budget-bytes`. The default is 16 GiB and the whole `u64` range is valid; zero rejects every positive reservation and does not mean unlimited. A Node connector owns one native client domain, whose budget covers every connection and reconnect the connector makes; it reads the budget and `C2_CALL_MAX_OUTSTANDING` from the environment, like a Runtime without overrides.

The budget charges a call's owned input from serialization until the transport has sent or copied it, that is, until the IPC frame is written, the SHM copy is made, the last chunk is sent or the HTTP request body is sent; the input is released then, not at the reply. It also charges the HTTP request and response bodies C-Two buffers for relay calls, and the relay's copies of forwarded request bodies. Calls with and without a deadline use the same budget. Each reservation lasts until the last owner of that storage drops it. The budget is separate from the three IPC budgets above, which charge SHM, file and reassembly backing, and it does not bound serializer workspaces, application objects, peer mappings, HTTP library or kernel buffers, or process RSS. The relay does not yet charge the successful upstream responses it sends back over HTTP.

When the budget is short, a call waits for other calls to release capacity, bounded by its own deadline; a call without a deadline can wait without bound. A request larger than the whole budget fails at once. Only the first reservation of a piece of data may wait: when data of unknown length grows past what it has reserved, the growth fails at once instead of waiting, because a call that waits while holding part of its own data can wait on itself, and several such calls can wait on each other. A budget failure raises `CallCapacityExceeded` with `stage=payload_buffer_admission`, and a wait that reaches the call's deadline raises `CallDeadlineExceeded`; both carry the transport phase in which they happened, so a failure while receiving a response is reported as dispatch-uncertain, not pre-dispatch. A hold of an HTTP response keeps its reservation until the hold is released, so retained HTTP holds can fill the budget and make later calls wait; release holds that are no longer needed, or size the budget for the data kept at the same time. The local copies behind IPC responses and their holds are counted in `memory_stats()['holds']` and are not charged to this budget.

A server-stream item is charged in full before it is received. When the item's ITEM_BEGIN arrives, the IPC client reserves its whole length and only then grants the server storage for it, and the reservation lasts until the consumer drops the item; an item longer than the whole budget fails with `CallCapacityExceeded` and cancels the stream. Because the item was produced by a resource step, the failure is dispatch-uncertain, never pre-dispatch. The server charges nothing to this budget for stream items: until it sends an item, the item stays the resource's own memory; it then writes an SHM item straight into its shared-memory allocation, which the server's SHM budget charges, or copies an inline item one frame at a time, at most `chunk_size` bytes each. A Python resource's item is encoded once inside its step, and the server keeps that encoded copy, also uncharged, until the item's last frame is written. A Runtime that streams to itself over IPC therefore never waits on the reservation it holds for the same item.

`C2_CALL_MAX_OUTSTANDING` separately limits the unfinished calls with a finite deadline, including calls that continue after their caller's deadline, and every server stream from its start until its real terminal, with or without a deadline, because a stream's record outlives a dropped handle; unary calls without a deadline are not counted. Closing a Runtime stops new finite calls, while calls already admitted may still reserve the budget for their real storage.

## Lazy allocation and reclamation

`pool_prewarm_segments=0` is the default. Registering or connecting creates no buddy mapping until a large-buffer allocation needs it. Explicit prewarming maps the selected number of segments immediately.

IPC uses `pool_min_retained_segments=0`, allowing idle buddy segments to retire after `pool_decay_seconds` (60 seconds for both server and client). The pool can return to zero mappings while preserving generation counters and live allocations. Direct native `MemPool` defaults retain one segment; the SDK IPC projection selects zero.

Dedicated SHM, like buddy SHM, has no in-segment header. A dedicated segment and its budget charge stay with the owner until the borrower's release record or the connection's peer end settles the loan; its slot is then reused under a new checked generation.

New buddy or dedicated backing also passes an OS memory-pressure heuristic. The `spill_threshold` IPC key (default 0.8, from 0 to 1) sets it: a candidate's total mapped bytes exceeding that fraction of observed available memory is refused, and 1 disables the check. In direct native use, a finite value at least 1 disables the heuristic; nonpositive or non-finite values send new mapping attempts to file backing or an explicit error. Reuse within existing backing remains available without a new reservation.

On Linux, overcommit can let mapping creation succeed while physical pages are committed only on write, so a successful `mmap` is not evidence that a later payload write has enough memory. The native pressure check therefore runs at new buddy and dedicated backing creation using the mapped size; it is a heuristic alongside finite C-Two budgets, not a process RSS or future-write guarantee. It leaves existing backing reuse available, can admit a smaller dedicated mapping after refusing a full buddy segment, and permits file backing where that allocation path supports it. When pressure appears, first lower concurrent or individual payload demand, inspect `cc.observe.memory_stats()` for backing, reassembly and retained-owner charges, and adjust the relevant typed IPC budget or pool size/count only after identifying the constrained domain. Increase reassembly capacity only when receivers are expected to accept larger complete payloads.

## Allocation and transport fallback

Client requests choose inline when request-pool SHM is not selected and the payload fits `chunk_size`; larger requests use checked chunks. SHM selection depends on `shm_threshold` (4 KiB by default) and wire representation limits. SHM requests write into the local request pool and transmit a 23-byte descriptor (`slot`, `generation`, `offset`, `data_size`, `kind`, `loan_id`); the peer lazily opens the exact prefix, slot and generation, copies the bytes and returns the loan in a RELEASE record. If SHM is unavailable because of budget, pressure or representation limits, small requests fall back to inline and larger requests use checked chunks. The default chunk size is 128 KiB. Payload and reassembly sizes bound each request; `max_inflight_chunk_frames` bounds the chunk frames a server processes at once across requests.

Server responses up to `shm_threshold` use inline frames. Larger responses try buddy reuse, buddy expansion and dedicated SHM before checked chunk fallback. Payloads that cannot fit buddy wire metadata also use the checked fallback.

SHM 4 segments carry no header: data starts at offset 0, the format comes from the handshake, and each segment's kind and generation are part of its OS name. The bitmap, live records, counters and lock stay with the owner, so a segment does not describe itself. A receiver checks a descriptor's range once against the actual OS view span with checked arithmetic. Peer mappings are read-only; only the owner maps a segment writable, and its local lock serializes allocation without waiting on a peer, application code or I/O. Logical sizes still round up to the allocation level, and trimming preserves the original release capacity. IPC 16 uses the [fixed preamble and call records](ipc-handshake.md) with SHM 4 and C/FFI ABI 7; retired handshakes, SHM layouts and ABIs are rejected.

Receiver reassembly reserves the value's full chunk capacity, the number of chunks implied by the total length and the first chunk times the first chunk's length, plus one byte per chunk for arrival flags, before accepting the storage. Allocation tries buddy reuse, reuse after GC, buddy expansion, dedicated SHM and finally file mmap. Completion may trim logical length while the capacity charge remains until storage release. Resources receive the complete assembled input; chunk transfer does not provide incremental resource input.

A server processes at most `max_inflight_chunk_frames` chunk frames at once. When that many reassemblies are in flight, admitting another first sweeps expired assemblies; the sweep never refuses a request, and no other setting controls it. A chunked request is refused at most once, whether a chunk finds no free slot, its first chunk exceeds the size or reassembly limits, a chunk cannot be read or copied, or the receive loop and a chunk task refuse it at the same time: the server sends one CALL_END with status NotExecuted, and the resource method does not run. A chunk task refuses its request before it returns its frame slot, so a later chunk of a refused request cannot complete it. The connection and its other calls continue. An assembly that stays incomplete for `chunk_assembler_timeout` is refused the same way when the GC sweep finds it, without waiting for another chunk. Later chunks of a refused or finished request are discarded at the frame boundary by the connection's [request-ID high-water mark](ipc-handshake.md#call-records-version-16) before they take a slot, returning any SHM they carry, and draw no further reply however late they arrive; no per-request marker is kept.

Rust callers free a local allocation with `MemPool::release_handle`, which consumes the move-only handle. A loaned block is freed only by its owner, when a release record or the connection's peer end settles the loan; nothing frees a block from peer coordinates. A receiver refunds its reassembly reservation as soon as it has copied the assembled payload; cached mapping charges still follow the existing GC policy.

Loans, release records and connection ends are described below. These paths never replay a business call.

FastDB content remains opaque at this layer. C-Two transports bytes or SHM coordinates and delegates nested payload specifications to FastDB Core.

`MemHandle` is the private, move-only owner for buddy SHM, dedicated SHM and file-spill allocations. SHM authority is tied to a non-reused process-local pool identity; `PoolAllocation` and wire coordinates contain metadata, not ownership. Raw copy, free and dedicated-mapping adoption require explicit `unsafe` ownership and lifetime contracts; a raw pointer query alone grants no safe memory access. Transport callers use `alloc_shm_handle()`, which keeps `alloc()`'s GC and error behavior. A file-spill owner closes its mapping before its backing file, including Windows delete-on-close cleanup.

A received SHM descriptor is admitted once into a carrier bound to the connection's actual OS reference and mapping. The carrier copies the bytes, ends the remote read and hands its release to the connection, after which it can no longer be read; neither an SDK hold nor a borrowed input keeps it. Do not expose replaceable receive-pool slots or constructors that turn coordinates into a carrier. A sent SHM block enters the connection's loan book before its first byte is written: cancellation before that frees it locally, and afterwards the loan waits for the release record or peer end. Server callbacks receive a fixed `ResponsePool` owner; normal response preparation uses `try_prepare_shm_response`, and raw mutable pool access requires `unsafe`. Read access returns a mapped guard; an ordinary read guard can reveal its original writable lock through `RwLockReadGuard::rwlock`.

`c2-core` `LocalCopy*` accounting records metadata for the local copies kept by SDK holds and borrowed inputs, without reading payload bytes or allocating a second buffer; it has no authority over transport memory. Python export admission shares the local-copy owner boundary: `PyCoreResponse` and `PyCoreRequestBuffer` each check exports, release and tracking under their own lock. Active exports reject release before invalidation or removal, and tracking after release cannot revive a copy. View-release counter decrements remain atomic.

Each owner pool adds a fresh PID/UUID incarnation to its logical label, and only `MemPool` derives bounded OS backing names, which carry the incarnation, kind, slot and generation; SDKs must not append suffixes. The handshake exchanges only the prefix and the real directional SHM capabilities, with no segment list or announcement; a receiver opens the exact prefix, slot and generation lazily when a descriptor arrives. A `loan_id` is unique within the lending process (a checked counter that never uses 0), while loan books are kept per physical connection, so an ID from another connection is unknown here. A release frame carries loan IDs only; a zero, unknown, duplicate or already released ID is a protocol violation that deactivates the connection without undoing releases already settled. A slot is reused only after its local allocation and loan are clear; an exhausted generation retires only that slot, and when every slot has retired, transfers fall back to chunks or file backing without changing pool, prefix or connection. Dedicated indices never wrap beyond the wire's `u16` range.

Python no longer exports manual `c_two.mem.ChunkAssembler` or `MemHandle` wrappers or their `_native` names. Automatic IPC chunking remains in Rust's `ChunkRegistry` / `ChunkAssembler` / `ReassemblyBacking` chain, while Rust memory-pool APIs remain available. Do not restore Python consumed/released wrappers or aliases for the retired manual entry points.

Fallback uses the existing IPC inline/chunk and SHM paths; do not introduce a third transport stack. Capacity-limit failures propagate through the transport layer’s existing error paths; do not add a retry queue for refused allocations.

## Disabling buddy

`pool_enabled=False` skips buddy reuse and expansion, including cached segments. Dedicated SHM, inline, chunks and file backing remain available. Receiver reassembly also uses dedicated SHM and file fallback. Dedicated mappings still consume the SHM backing budget.

`pool_enabled` is accepted by `BaseIPCOverrides`, `ServerIPCOverrides` and `ClientIPCOverrides`. This minimal configuration disables only buddy, with dedicated SHM and the existing fallback paths still available:

```python
import c_two as cc

# Configure each side before its first registration or connection.
cc.set_server(ipc_overrides={'pool_enabled': False})
cc.set_client(ipc_overrides={'pool_enabled': False})
```

`shm_threshold` selects the inline/SHM boundary and does not disable every SHM path. The native parser rejects `shm_threshold` in `ipc_overrides`; configure it through `cc.set_transport_policy(shm_threshold=...)`. Raising the threshold can move sender traffic to inline or chunks while receiver reassembly still needs backing.

Disabled buddy requires `pool_prewarm_segments=0`. `pool_min_retained_segments` cannot exceed `max_pool_segments` or `reassembly_max_segments`. Native parsing rejects invalid combinations.

Pool segment capacity and `max_payload_size` are independent for enabled and disabled buddy. A segment can hold several smaller messages. Positive size, representable indices, bounded segment counts and checked arithmetic remain required; requests and responses exceeding the payload limit are rejected regardless of available segment capacity.

```python
import c_two as cc

cc.set_server(ipc_overrides={
    'pool_enabled': False,
    'max_payload_size': 32 * 1024 * 1024,
})
cc.set_client(ipc_overrides={
    'pool_enabled': False,
    'max_payload_size': 32 * 1024 * 1024,
})
```

## Setting budgets

Call `cc.set_server()` before registration and `cc.set_client()` before connection; server and client use independent budget domains, so configure both when both directions need the limits.

```python
import c_two as cc

BUDGET = {
    'shm_backing_budget_bytes': 2 * 1024**3,
    'file_backing_budget_bytes': 4 * 1024**3,
    'live_reassembly_budget_bytes': 1024**3,
}
cc.set_server(ipc_overrides=dict(BUDGET))
cc.set_client(ipc_overrides=dict(BUDGET))
```

Equivalent environment variables are `C2_IPC_SHM_BACKING_BUDGET_BYTES`, `C2_IPC_FILE_BACKING_BUDGET_BYTES` and `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES`. Code overrides take precedence over process environment, `.env` and native defaults. Outgoing client policy freezes on its first connection attempt, including failure; cache reuse requires equality of the complete resolved policy. Close the old Runtime and configure a new Runtime to select different limits. Older retained payloads remain charged to their original domain.

The finite defaults deliberately change the earlier aggregate limit and may need adjustment for workloads exceeding them; they do not guarantee that every scientific workload fits. Refusal happens before allocation and increments `rejected_allocations` / `rejected_bytes` in the relevant cell. Control frames such as ping allocate no data backing and consume none of these budget cells.

### Standalone relay

Relay resolves upstream IPC policy at startup through the same native resolver and stores it in `RelayConfig.upstream_ipc`. All data-plane upstream request and reassembly pools share the relay instance's budget, including reconnects. Configure the relay process through its CLI or environment.

```bash
c3 relay --bind 127.0.0.1:8080 \
  --ipc-pool-enabled false \
  --ipc-shm-backing-budget-bytes 134217728 \
  --ipc-file-backing-budget-bytes 268435456 \
  --ipc-live-reassembly-budget-bytes 134217728
```

These flags override `C2_IPC_POOL_ENABLED` and the three budget variables. Other client IPC settings, including prewarming, decay and `C2_SHM_THRESHOLD`, use the existing resolver. `c3 relay --dry-run` reports resolved policy. Disabled buddy still requires zero prewarming, and zero budgets still reject positive reservations.

This accounting covers relay-owned data-plane IPC backing and reassembly. Registration proofs and control watches use separate lazy contexts. Peer mappings are outside these IPC budgets. The relay charges each forwarded request body to its payload buffer budget before reading it, and an inline upstream IPC call reserves its own copy of the body before allocating it. The upstream response is materialized in full before batched sending and is not charged yet. `C2_CALL_MAX_OUTSTANDING` separately bounds the relay's continuing transactions; see [relay configuration](configuration.md#relay-and-c3).

## Statistics and payload lifetime

`cc.observe.memory_stats()` returns read-only snapshots for `runtime_outgoing`, `server`, `retired`, `payload_buffer`, `retired_payload_buffers` and `holds`. The outgoing domain freezes on its first connection attempt; observing it does not connect or freeze policy. `server` is `None` when no host exists. Retired domains have `state="retired"` and remain observable while an old proxy or in-flight call retains a real accounting owner; this presentation is implementation behavior rather than a stable shape guarantee. Each domain reports `role`, `state`, three resolved `limits`, and `shm` / `file` / `reassembly` cells with `limit_bytes`, `used_bytes`, `peak_bytes`, `rejected_allocations` and `rejected_bytes`; peaks retain their high-water mark after release. `payload_buffer` is the Runtime's payload buffer budget as one cell with the same five counters; it is `None` until the first business call or HTTP acquisition creates the call domain, and `retired_payload_buffers` lists the budgets of earlier sessions while they still have owners. A wait for capacity is not counted as a rejection. `holds` reports `active_holds`, `total_held_bytes`, `oldest_hold_seconds` and `by_direction` for the local copies kept by SDK holds and borrowed inputs; `total_held_bytes` counts their logical bytes, not SHM capacity or RSS, and local copies never keep a retired domain alive. These counters are separate charges for C-Two-owned IPC backing, live reassembly capacity and payload buffers: summing cells does not measure physical memory or prove zero-copy transport.

`cc.hold(proxy.method)(args)` returns a `HeldResult` with `.value`, `.unsafe_buffer` and `.release()`. Use explicit release or a context manager; `__del__` is the fallback. For a portable method, `.value` is a FastDB `Payload` owner. A hold owns only a local copy, because the transport memory is released right after the copy. Release invalidates that owner and its checked views, then frees the local buffer. Materialize values through FastDB before keeping them beyond the hold. `unsafe_buffer` permits raw views whose exported pointers or NumPy aliases cannot be mechanically invalidated. See [Python payload usage](python-usage.md).

The FastDB invalidator is marked attempted before entry and runs at most once, including when it raises or Rust unwinds. After invalidation starts, `.value` and `.unsafe_buffer` stay inaccessible. An active export rejects release before invalidation; release the extra view and retry. A view already released during that attempt is never restored. Manually supplied `HeldResult(release_cb=...)` callbacks remain once-only user callbacks. The primary error and the cleanup cause remain in the existing exception chain on Python 3.10.

A borrower that has not copied, does not read its connection or blocks the release path keeps its loans: the owner reuses a published block only after its release record or the connection's peer end, never after a wait or timeout. Peer end means the read side's own end: EOF, or a read error reporting that the peer has closed (ECONNRESET or EPIPE on Unix, 109 or 232 on Windows); a local cancel, timeout, write failure, ordinary I/O error or DISCONNECT does not settle loans. When the read side fails without proof of an end, the remaining loans stay in the original pool until it is destroyed. A process that inherited the connection keeps it alive until its last copy closes. Copy-then-release limits loans to calls in progress and their release window, but it is not a byte limit and does not isolate one connection's traffic from another's; when the shared pool is full, allocation falls back through dedicated SHM, inline or chunked transfer and file reassembly, and returns the original capacity error without truncation when every budget is exhausted.

A prepared SHM reply becomes a loan once it enters the loan book before its first byte. A later write failure, deadline or cancellation leaves it to the release record or peer end and never frees peer-visible storage early. Fill errors, panic, cancellation while waiting for the writer and failure before the write begins free the block locally; an inline/chunk fallback cannot hide their original failure.

`cc.shutdown()` does not invalidate the local copies of SDK holds; they stay readable until released. Backing charges remain in retired domains while a real owner keeps them. A zero charge and the removal of an observation record can occur at different times; the weak observation disappears after the last actual owner is gone. Statistics do not keep pools, connections, Runtimes, callbacks or payloads alive.

## Implementation references

- [Native configuration](../../core/foundation/c2-config/src/memory.rs) and [IPC validation](../../core/foundation/c2-config/src/ipc.rs).
- [Budget reservation](../../core/foundation/c2-mem/src/budget.rs), [pool allocation](../../core/foundation/c2-mem/src/pool.rs), [memory pressure](../../core/foundation/c2-mem/src/pressure.rs), [dedicated backing](../../core/foundation/c2-mem/src/dedicated.rs), [buddy segments](../../core/foundation/c2-mem/src/buddy_segment.rs) and [file spill](../../core/foundation/c2-mem/src/spill.rs).
- [Request transport](../../core/transport/c2-ipc/src/client.rs), [server responses](../../core/transport/c2-server/src/server.rs), [reassembly](../../core/protocol/c2-wire/src/assembler.rs) and [chunk registry](../../core/protocol/c2-wire/src/chunk/registry.rs).
- [Runtime observations](../../core/runtime/c2-core/src/memory.rs), [native session projection](../../sdk/python/native/src/runtime_session_ffi.rs), [Python IPC overrides](../../sdk/python/src/c_two/config/ipc.py), [native override parsing](../../sdk/python/native/src/config_ffi.rs), [settings](../../sdk/python/src/c_two/config/settings.py) and [HeldResult](../../sdk/python/src/c_two/crm/transferable.py).
- [Memory budget contract](../acceptance/memory-budget-contract.md), [memory policy plan](../acceptance/2026-09-26-memory-policy.md), [native memory validation](../acceptance/memory-native-final-validation.md) and [canonical endpoint validation](../acceptance/canonical-local-endpoint-validation.md) retain their original scopes.

[ADR 0001](../decisions/0001-single-owner-release.md) records the single-owner release model; the [0.10.0 release notes](../releases/0.10.0.md) describe the failed-release ownership it replaced, and the [0.8.0 release notes](../releases/0.8.0.md) introduced the chunk-admission and response-path model that remains. The [0.7.4 publication record](../acceptance/0.7.4-publication.md) retains that release's source pair, platforms and public-artifact verification. Historical validation reports retain their original source pairs. Chunking and hold semantics do not establish incremental resource input, direct FastDB response construction, zero-copy decoding or a measured performance gain.
