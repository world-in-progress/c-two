# Per-Connection Write Barrier for ServerV2 Fast Path

> **Status: ✅ SUPERSEDED** — Write barrier was implemented in the Python server, which has since been replaced by Rust `c2-server` (Phase 3). The equivalent concurrency control now lives in Rust.

> Date: 2026-03-28
> Branch: `autoresearch/python-qps-mar27`
> Status: Approved

## Problem

ServerV2 的零 Task 快速路径（`_handle_client` 读循环）在 READ_PARALLEL 模式下，将请求 fire-and-forget 提交到 `_FastDispatcher` 的 `SimpleQueue` 后立即读下一帧。由于从 "worker 出队" 到 "获取 RW 锁" 之间存在线程调度不确定性，同一连接内先提交的 `@cc.write` 请求可能晚于后提交的 `@cc.read` 请求执行，导致 read 看不到 write 的效果。

这不是并发安全问题（RW 锁保证了互斥），而是同一连接内的因果序问题。

## Design

### Core Mechanism

读循环提交 `@cc.write` 请求后，`await` 一个 `asyncio.Event` 屏障，暂停续读直到 worker 执行完 write 并释放屏障。`@cc.read` 请求不触发屏障，保持 fire-and-forget。

### Scope

- **Per-connection**：屏障只阻塞当前连接的读循环，其他连接完全不受影响。
- **不区分路由**：write 阻塞整个连接的读循环（包括其他路由的请求）。当前主场景为单路由（Rust relay），多路由隔离作为后续增强项。
- **所有并发模式**：EXCLUSIVE / READ_PARALLEL / PARALLEL 下均生效。对于 EXCLUSIVE 只是额外的因果序保证；对于 PARALLEL 由 CRM 自行管理并发，屏障保证连接内因果序。

### Changes — `src/c_two/rpc_v2/server.py` only

#### 1. `_FastDispatcher.submit_inline` — 增加 barrier 参数

```python
def submit_inline(
    self,
    sched_execute,
    method,
    args_bytes,
    access,
    request_id: int,
    writer,
    barrier: asyncio.Event | None = None,   # NEW
) -> None:
    self._q.put((sched_execute, method, args_bytes, access,
                 None, request_id, writer, barrier))
```

#### 2. `_FastDispatcher._worker` — finally 中释放 barrier

```python
def _worker(self) -> None:
    loop = self._loop
    q = self._q
    call_soon = loop.call_soon_threadsafe
    while True:
        item = q.get()
        if item is _SENTINEL:
            break
        sched_execute, method, args_bytes, access, future, request_id, writer, barrier = item
        try:
            result = sched_execute(method, args_bytes, access)
            if future is not None:
                call_soon(future.set_result, result)
            else:
                self._write_inline_reply(call_soon, writer, request_id, result)
        except BaseException as exc:
            if future is not None:
                call_soon(future.set_exception, exc)
            else:
                self._write_error_reply(call_soon, writer, request_id, exc)
        finally:
            if barrier is not None:
                call_soon(barrier.set)
```

#### 3. `_FastDispatcher.submit` — tuple 长度对齐

`submit` 也需要补齐 barrier 字段（传 None），保持 work tuple 结构一致：

```python
def submit(self, sched_execute, method, args_bytes, access, future) -> None:
    self._q.put((sched_execute, method, args_bytes, access,
                 future, None, None, None))
```

#### 4. `_handle_client` 快速路径 — write 时创建 barrier 并 await

Cache hit 路径：
```python
if cached is not None:
    exec_fast, method, access = cached
    if access is MethodAccess.WRITE:
        barrier = asyncio.Event()
        dispatcher.submit_inline(
            exec_fast, method, args_bytes, access,
            request_id, writer, barrier,
        )
        await barrier.wait()
    else:
        dispatcher.submit_inline(
            exec_fast, method, args_bytes, access,
            request_id, writer,
        )
    continue
```

Cache miss 路径（同样逻辑）：
```python
method, access = entry
dispatch_cache[cache_key] = (slot.scheduler.execute_fast, method, access)
if access is MethodAccess.WRITE:
    barrier = asyncio.Event()
    dispatcher.submit_inline(
        slot.scheduler.execute_fast, method,
        args_bytes, access, request_id, writer, barrier,
    )
    await barrier.wait()
else:
    dispatcher.submit_inline(
        slot.scheduler.execute_fast, method,
        args_bytes, access, request_id, writer,
    )
continue
```

### Edge Cases

| Scenario | Behavior |
|---|---|
| Write execution raises exception | `finally` block guarantees `barrier.set()`. Read loop resumes. Error reply sent to client. |
| Connection drops during `await barrier.wait()` | `CancelledError` propagates, `finally` cleanup runs. Worker still completes, `call_soon_threadsafe(barrier.set)` is safe (no-op if loop closed). |
| EXCLUSIVE mode | Barrier still fires on write. Extra ~1μs per write, no behavioral change since exclusive lock already serializes. |
| PARALLEL mode | Barrier guarantees per-connection causal order. CRM still responsible for its own thread safety. |
| All-read workload | Zero overhead. Only one `access is MethodAccess.WRITE` comparison per request (always False). |
| Multi-route, write on route A | Blocks entire connection's read loop (including route B requests) until write completes. Acceptable for current single-route Rust relay scenario. |

### Performance Impact

| Path | Overhead |
|---|---|
| Read request (common case) | +1 enum identity comparison (`is`), < 1ns |
| Write request (rare case) | +1 `asyncio.Event()` (~0.5μs) + 1 `call_soon_threadsafe` (~0.5μs) + event loop resume (~1μs) |

### Not Changed

- `Scheduler` — no changes to locking, `execute_fast`, or shutdown logic.
- `_WriterPriorityReadWriteLock` — unchanged, still provides mutual exclusion.
- `wire.py` — unchanged.
- Fallback Task path (`_handle_v2_call`) — unchanged; it already uses `await future` which provides natural ordering.

### Future Enhancement

Per-route barrier isolation: maintain `pending_writes: dict[bytes, asyncio.Event]` in the read loop so that a write on route A only blocks route A's subsequent requests while route B continues unblocked. Upgrade path from this design is straightforward.
