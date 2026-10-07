# Shared transport memory contexts

Implementation source: `ac4ca86fa1d298e11baf9ee5337e5e6084dcd82c`. This records the integrated shared-budget and observation changes. Request-send cancellation remains a separately reviewed change; this report does not claim that work or Windows acceptance is complete.

Each Core Runtime has one outgoing `MemoryBudget`, shared by every cached connection's request and reassembly pools. Independent Runtimes have independent budgets. The resolved client configuration freezes before the first connection attempt, including an attempt that fails. Cached connections require the complete resolved `ClientIpcConfig` to match; matching only the three budget limits is insufficient.

A Server owns a separate budget shared by its response pool, reassembly pool and explicit prewarm. A relay's upstream clients share its finite context. Standalone clients create a finite context; injected pools must carry compatible ownership, limits and policy before any connection I/O. Shutdown never resets outstanding charges.

Preallocated requests carry their exact issuing pool through selection, allocation and release. A replacement pool cannot receive a free for an older incarnation. On confirmed close the client drops its idle request pool and releases its reference to a registry with no unfinished assemblies. Finished `ReassemblyBacking` carriers keep their original pool and complete reservation alive. Held file and buddy responses can therefore survive shutdown and release their backing while the closed proxy is still referenced.

`Runtime::memory_stats()` and `cc.memory_stats()` return values, not allocator authority. The two active domains are `runtime_outgoing` and `server`; retired domains remain observable through the native owner-lifetime mechanism described in [memory-retired-observation.md](memory-retired-observation.md). Observing does not connect, prewarm, create a host or freeze unused configuration.

Each domain exposes the resolved limits and the independent `shm`, `file` and `reassembly` cells, including current and peak bytes and rejected reservations. `holds` uses the same Rust lease metadata as `cc.hold_stats()`. These are backing and lease accounting, not process RSS; adding the three cells is not a physical-memory measurement.

## Host verification

The integrated tree was independently reviewed and tested with the official FastDB 0.2.1 source/SDK. Host checks, rather than Worker-reported totals, establish this checkpoint:

- 105 IPC tests passed after composing the shared context with the reassembly carrier and confirmed-close detachment.
- 384 Core, memory and server tests passed at the preceding observation checkpoint. After the final weak-observer and read-only-interface corrections, the affected Core/memory suites passed 222 tests plus one compile-fail interface test.
- The native extension was rebuilt from the integrated source. All 622 Python unit tests passed, including the Python 3.10 example syntax gate, with no skipped tests.
- Fourteen focused public API and statistics tests passed. The five public integration cases in `sdk/python/tests/integration/test_memory_budget_lifetime.py` cover buddy/file held responses across shutdown, pending-call cancellation, a delivered response paused before held registration, and failed session replacement followed by a successful retry.

The last two public cases failed on the earlier implementation and passed after correction. The file and buddy cases assert the full rounded chunk capacity remains charged through logical trimming and shutdown, then verify FastDB checked-view invalidation and return of backing/reassembly charges before dropping the proxy.

Logs retained locally: `/tmp/c2-memory-merge-ipc-suite.log`, `/tmp/c2-memory-observer-integrated-rust.log`, `/tmp/c2-memory-observer-owners-rust.log`, `/tmp/c2-memory-observer-owners-public-final.log`, and `/tmp/c2-memory-observer-owners-python-unit.log`. These are local checkpoint results. Complete cross-language, benchmark and Windows evidence belongs to the final validation record.
