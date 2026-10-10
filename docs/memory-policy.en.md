# C-Two transport memory policy

English · [简体中文](memory-policy.md)

Rust owns IPC budget accounting, lazy allocation and tiered fallback. SDKs expose typed overrides and read-only statistics. See the [configuration guide](configuration.en.md) for the full option reference.

## Finite budget cells

Both server and client IPC overrides accept these independent byte budgets:

| Override | Native default | Charged storage |
| --- | --- | --- |
| `shm_backing_budget_bytes` | 8 GiB | Locally created buddy and dedicated SHM mappings, including headers and alignment |
| `file_backing_budget_bytes` | 16 GiB | Locally created file backing length |
| `live_reassembly_budget_bytes` | 8 GiB | Reassembly capacity retained by incomplete, queued, executing or held payloads |

Zero rejects every positive reservation. The entire `u64` range is valid; zero does not mean unlimited. Per-message, reassembly, chunk-count and segment-count limits also apply independently. Budgets cover C-Two-owned IPC backing and assembly, without bounding Python/FastDB heaps, HTTP buffers, receiver-opened peer mappings or total process RSS.

Server response pools, reassembly and prewarming share one server-direction budget. Cached request and reassembly pools in one Runtime's outgoing client domain share another. Directions and separate Runtimes are independent. Reservation occurs atomically before backing creation: refusal creates no mapping or file. Reusing free blocks in charged backing does not charge it again. Dedicated storage remains charged while waiting for peer `read_done` or owner GC.

Finite-deadline calls have separate admission limits, `C2_CALL_MAX_OUTSTANDING` and `C2_CALL_RETAINED_INPUT_BUDGET_BYTES`. These account for retained request input and continuing native transactions after caller timeout. Unlimited calls are outside that finite-call admission domain; these settings are separate from the three IPC budgets above.

## Lazy allocation and reclamation

`pool_prewarm_segments=0` is the default. Registering or connecting creates no buddy mapping until a large-buffer allocation needs it. Explicit prewarming maps the selected number of segments immediately.

IPC uses `pool_min_retained_segments=0`, allowing idle buddy segments to retire after `pool_decay_seconds` (60 seconds for both server and client). The pool can return to zero mappings while preserving generation counters and live allocations. Direct native `MemPool` defaults retain one segment; the SDK IPC projection selects zero.

New buddy or dedicated backing also passes an OS memory-pressure heuristic. The IPC projection uses `spill_threshold=0.8`: a candidate mapping larger than that fraction of observed available memory is refused. This is not an IPC override key. In direct native use, a finite value at least 1 disables the heuristic; nonpositive or non-finite values refuse new mappings. Reuse within existing backing remains available.

## Allocation and transport fallback

Client requests choose inline or checked chunk transfer when request-pool SHM is not selected. SHM selection depends on `shm_threshold` (4 KiB by default) and wire representation limits. SHM requests write into the local request pool and transmit the 15-byte `BuddyPayload` coordinates and generation; the peer lazily opens the exact prefix, index and generation. If SHM is unavailable because of budget, pressure or representation limits, small requests fall back to inline and larger requests use checked chunks. The default chunk size is 128 KiB, bounded by chunk-count and reassembly limits.

Server responses up to `shm_threshold` use inline frames. Larger responses try buddy reuse, buddy expansion and dedicated SHM before checked chunk fallback. Payloads that cannot fit buddy wire metadata also use the checked fallback.

Receiver assembly reserves full capacity, `total_chunks × chunk_size`, before accepting the storage. Allocation tries buddy reuse, reuse after GC, buddy expansion, dedicated SHM and finally file mmap. Completion may trim logical length while the capacity charge remains until storage release. Resources receive the complete assembled input; chunk transfer does not provide incremental resource input.

FastDB content remains opaque at this layer. C-Two transports bytes or SHM coordinates and delegates nested payload specifications to FastDB Core.

## Disabling buddy

`pool_enabled=False` skips buddy reuse and expansion, including cached segments. Dedicated SHM, inline, chunks and file backing remain available. Receiver assembly also uses dedicated SHM and file fallback. Dedicated mappings still consume the SHM backing budget.

`shm_threshold` selects the inline/SHM boundary and does not disable every SHM path. Set it through `cc.set_transport_policy(shm_threshold=...)`, not role-specific IPC overrides. Raising the threshold can move sender traffic to inline or chunks while receiver assembly still needs backing.

Disabled buddy requires `pool_prewarm_segments=0`. `pool_min_retained_segments` cannot exceed either applicable request/response or reassembly segment-count limit. Native parsing rejects invalid combinations.

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

```python
BUDGET = {
    'shm_backing_budget_bytes': 2 * 1024**3,
    'file_backing_budget_bytes': 4 * 1024**3,
    'live_reassembly_budget_bytes': 1024**3,
}
cc.set_server(ipc_overrides=dict(BUDGET))
cc.set_client(ipc_overrides=dict(BUDGET))
```

Equivalent environment variables are `C2_IPC_SHM_BACKING_BUDGET_BYTES`, `C2_IPC_FILE_BACKING_BUDGET_BYTES` and `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES`. Code overrides take precedence over process environment, `.env` and native defaults. Outgoing client policy freezes on its first connection attempt, including failure; cache reuse requires equality of the complete resolved policy. Configure a new Runtime to select different limits. Older retained payloads remain charged to their original domain.

The finite defaults need adjustment for workloads exceeding their limits. Refusal happens before allocation and increments `rejected_allocations` / `rejected_bytes` in the relevant cell. Control frames such as ping and heartbeat allocate no data backing and consume none of these budget cells.

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

This accounting covers relay-owned data-plane IPC backing and assembly. Registration proofs and control watches use separate lazy contexts. Peer mappings and HTTP buffers are outside these IPC budgets. HTTP response materialization can still consume the full response before batched sending. Relay forwarding admission separately bounds continuing transactions and retained input; see [relay configuration](configuration.en.md#relay-and-c3).

## Statistics and payload lifetime

`cc.memory_stats()` returns read-only snapshots for `runtime_outgoing`, `server`, `retired`, `holds` and `budget_cells_note`. Observing outgoing statistics does not connect or freeze policy. `server` is `None` when no host exists. Retired domains remain observable while older owners retain storage. Each domain reports `role`, `state`, three resolved `limits`, and `shm` / `file` / `reassembly` cells with `limit_bytes`, `used_bytes`, `peak_bytes`, `rejected_allocations` and `rejected_bytes`. `holds` and `cc.hold_stats()` share native lease accounting.

`cc.hold(proxy.method)(args)` returns a `HeldResult` with `.value`, `.unsafe_buffer` and `.release()`. Use explicit release or a context manager; `__del__` is the fallback. Release invalidates the FastDB owner and checked views before freeing the transport lease. Materialize values through FastDB before keeping them beyond that lease. `unsafe_buffer` permits raw views whose exported pointers or NumPy aliases cannot be mechanically invalidated. See [Python payload usage](python-usage.md).

`cc.shutdown()` does not force-release retained payloads or erase their accounting. Charges and retained leases remain in retired domains. A zero charge and the removal of an observation record can occur at different times; the weak observation disappears after the last actual owner is gone. Statistics do not keep pools, connections, Runtimes, callbacks or payloads alive.

## Implementation references

- [Native configuration](../core/foundation/c2-config/src/memory.rs) and [IPC validation](../core/foundation/c2-config/src/ipc.rs).
- [Budget reservation](../core/foundation/c2-mem/src/budget.rs), [pool allocation](../core/foundation/c2-mem/src/pool.rs) and [memory pressure](../core/foundation/c2-mem/src/pressure.rs).
- [Request transport](../core/transport/c2-ipc/src/client.rs), [server responses](../core/transport/c2-server/src/server.rs) and [reassembly](../core/protocol/c2-wire/src/assembler.rs).
- [Runtime observations](../core/runtime/c2-core/src/memory.rs) and [Python IPC overrides](../sdk/python/src/c_two/config/ipc.py).

The [0.7.4 publication record](reports/0.7.4-publication.md) identifies the current published source pair, platforms and public-artifact verification. Historical validation reports retain their original source pairs. Chunking and hold semantics do not establish incremental resource input, direct FastDB response construction, zero-copy decoding or a measured performance gain.
