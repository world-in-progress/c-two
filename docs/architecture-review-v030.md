# v0.3.0 Architecture Review Findings

This document records findings from the pre-release code review. Issues marked as
**deferred** are tracked for the post-0.3.0 roadmap — they are not blockers.

---

## 1. Large File Audit

| File | Lines | Issue | Recommendation |
|------|-------|-------|----------------|
| `transport/client/core.py` | ~1017 | `SharedClient` handles socket I/O, threading, handshake, chunked reassembly, error handling, concurrent call routing — too many responsibilities | Extract `_ReplyChunkAssembler` to `client/chunking.py`; extract socket management to `client/connection.py` |
| `transport/server/core.py` | ~911 | `Server` monolith. Already partially split into sub-modules (dispatcher, heartbeat, handshake, reply, chunk, connection). Remaining core is asyncio event loop + connection handler | Acceptable for now. The sub-module extraction is good. Further split only if server grows |
| `transport/registry.py` | ~731 | Dual concern: `_ProcessRegistry` lifecycle management + module-level API delegation | Split into `registry.py` (public API surface) + `lifecycle.py` (server/client creation/teardown) |
| `transport/wire.py` | ~505 | Mixes codec functions with `MethodTable` class | Extract `MethodTable` to `wire_methods.py` or inline into registry |

## 2. Signal Protocol Consistency

- **`MsgType.SHUTDOWN_SERVER` (0x06):** Defined but never used by any handler. Either remove or document as reserved for future use.
- **`DISCONNECT_ACK` in Rust IPC client:** `SIG_DISCONNECT_ACK` constant defined but `recv_loop` only handles PING signals explicitly. All other signals fall through to `continue`. Functionally correct (`close()` proceeds after 100 ms regardless) but an explicit match would improve clarity.
- **`PONG`:** Never explicitly handled by clients — received PONGs fall through to signal `continue`. Correct for current keepalive-only design.

## 3. Concurrency Patterns

- Python `SharedClient` is synchronous (threading) while `Server` and `Relay` are async (asyncio). No async client variant exists. Consider `AsyncSharedClient` for embedded use cases post-0.3.0.
- `_WriterPriorityReadWriteLock` in `scheduler.py` is a custom implementation. Python 3.13+ has `threading.RWLock` — evaluate migration when minimum Python version is bumped.

## 4. What Was Fixed in This Review

- **BUG-1:** Python `UpstreamPool.get()` returned dead clients (`_closed=True`) without checking → fixed to detect and trigger immediate reconnect.
- **BUG-2:** Rust `UpstreamPool::get()` returned disconnected clients (`!is_connected()`) → fixed to check connection state.
- **BUG-3:** Rust relay router didn't evict dead client on call failure → fixed to evict on transport error.
- Sweeper now detects dead connections proactively (checks `_closed`/`is_connected()` in addition to idle time).
- Sweeper runs even when `idle_timeout=0` for dead-connection cleanup.

## 5. Test Coverage Added

- 22 Python unit tests for `UpstreamPool` (idle eviction, dead detection, reconnect, shutdown).
- 2 CLI tests for `--idle-timeout` option.
- 2 new Rust unit tests for dead-connection eviction in `idle_entries()`.

## 6. Deferred Items

1. ~~Split `client/core.py` — requires careful extraction of socket lifecycle.~~ → **Resolved** (Phase 3-4): Original 1017L `SharedClient` replaced by Rust `RustClient` + 85L thin compatibility wrapper.
2. Split `registry.py` — needs API compatibility preservation. *(Still open)*
3. ~~Extract `MethodTable` from `wire.py` — low risk but low priority.~~ → **Resolved** (Phase 4): `wire.py` slimmed to 93L; `MethodTable` is now the primary content.
4. Add stress/race-condition tests for concurrent sweeper + reconnect. *(Still open)*
5. Add integration test for full idle-evict-reconnect lifecycle. *(Still open)*
6. ~~Clean up unused `SHUTDOWN_SERVER` signal.~~ → **Fixed** (2026-03-30, commit `51b001e`).
7. ~~Explicit `DISCONNECT_ACK` handling in Rust `recv_loop`.~~ → **Fixed** (2026-03-30, commit `51b001e`).

---

## Post-Phase 4 Update (2026-04-01)

The following §1 findings are now resolved by the Phase 3-4 Rust transport sink:

| File (v0.3.0) | Lines | v0.4.0 Status |
|------|-------|---------------|
| `transport/client/core.py` | ~1017 | Replaced by Rust `RustClient` (c2-ipc). 85L thin `SharedClient` wrapper remains for test compat. |
| `transport/server/core.py` | ~911 | **Deleted**. Replaced by Rust `c2-server` + `NativeServerBridge` (325L). |
| `transport/registry.py` | ~731 | 740L — still intact; split deferred. |
| `transport/wire.py` | ~505 | 93L — `MethodTable` + `payload_total_size` + thin FFI wrappers. All codec in Rust. |

§3 concurrency notes: Server is now Rust tokio (not Python asyncio). `SharedClient` is a thin wrapper around `RustClient`. Async client (`RustAsyncClient`) is a future item.
