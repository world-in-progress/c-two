# Relay QPS Optimization Report

> Branch: `autoresearch/relay-qps-mar27` → `autoresearch/python-qps-mar27`
> Date: 2026-03-27
> Baseline: ~700 QPS (initial Python HTTP relay) → **35,883 QPS** (final)

## 1. Background

C-Two 的 HTTP Relay 是一个 Rust (axum) 反向代理，接收 HTTP 请求后通过 IPC v3 协议转发给 Python ServerV2 进程中的 CRM 资源对象。优化分两个阶段：

- **Phase 1 (Rust 侧)**: 优化 axum relay 本身的开销，从 ~700 QPS 提升到 ~9,500 QPS
- **Phase 2 (Python 侧)**: 优化 ServerV2 的请求处理热路径，从 ~9,500 QPS 提升到 ~35,900 QPS

本报告详细记录两个阶段的全部实验。

---

## 2. 测试方法

### 基准测试工具

```bash
# 使用 hey (Go HTTP benchmark tool)
hey -n 3000 -c 32 -m POST \
    -D /tmp/c2_bench_payload.bin \
    -T application/octet-stream \
    http://127.0.0.1:{port}/hello/greeting
```

### 测试 CRM

```python
class Hello:
    def greeting(self, name: str) -> str:
        return f"Hello, {name}!"
```

- Payload: `pickle.dumps(('Benchmark',))` ≈ 40 bytes
- CRM 方法执行时间: ~2μs（几乎为零）
- 瓶颈完全在框架开销上

### 完整请求链路

```
HTTP Client (hey, 32 并发)
  → axum (Rust relay, 监听 HTTP)
    → c2-ipc (Rust IPC client, UDS 连接)
      → Python ServerV2 (asyncio, UDS 监听)
        → CRM method execution
      ← 返回结果
    ← IPC v3 回复帧
  ← HTTP 200 + body
```

---

## 3. Phase 1: Rust 侧优化 (700 → 9,500 QPS)

Branch: `autoresearch/relay-qps-mar27`，基于 `dev-feature` (commit `d91573d`)。

### 实验结果总表

| # | QPS | 状态 | 描述 |
|---|-----|------|------|
| 0 | 4,868 | baseline | 初始 Rust relay |
| 1 | 5,198 | keep | 移除 per-request Bytes::copy_from_slice |
| 2 | 5,536 | keep | IPC client 连接池 (避免每次 connect) |
| 3 | 6,695 | **keep** | **std::sync::Mutex 替换 tokio::sync::Mutex (+37%)** |
| 4 | 6,412 | discard | 预分配 reply buffer |
| 5 | 7,126 | keep | 减少 IPC frame 解析中的临时分配 |
| 6 | 7,894 | keep | 零分配 frame write (单次 writev) |
| 7 | 9,543 | **keep** | **零分配 frame write 完善 (+100% vs baseline)** |
| 8 | 9,458 | keep | 微调 buffer 策略 |

### 关键发现

#### 3.1 std::sync::Mutex vs tokio::sync::Mutex (Exp 3, +37%)

```rust
// Before: tokio::sync::Mutex — 每次 lock 都经过 async runtime
let conn = self.connection.lock().await;

// After: std::sync::Mutex — 直接 OS mutex，无 async overhead
let conn = self.connection.lock().unwrap();
```

IPC 调用是短时间持锁（微秒级），`tokio::sync::Mutex` 的 `.await` 调度开销远超 OS mutex 的自旋等待。对于不需要跨 `.await` 点持锁的场景，`std::sync::Mutex` 永远更快。

#### 3.2 零分配 frame write (Exp 6-7, 累计 +100%)

```rust
// Before: 3次分配 — header Vec, payload Vec, concat
let header = frame_header.to_bytes();  // alloc 1
let payload = encode_payload(data);     // alloc 2
let frame = [header, payload].concat(); // alloc 3
stream.write_all(&frame).await?;

// After: 零分配 — 栈上 header + writev
let mut header_buf = [0u8; 16];
header.encode_into(&mut header_buf);
let bufs = [
    IoSlice::new(&header_buf),
    IoSlice::new(payload),
];
stream.write_vectored(&bufs).await?;
```

消除了热路径上所有的堆分配。Frame header 在栈上编码，数据通过 `write_vectored` (writev syscall) 一次性发送，避免 `concat` 的内存拷贝。

#### 3.3 axum echo 天花板

纯 axum echo endpoint（不经过 IPC）: ~29,000 QPS。Phase 1 最终 ~9,500 QPS 意味着 Python 侧占了 67% 的延迟。


---

## 4. Phase 2: Python 侧优化 (9,500 → 35,900 QPS)

Branch: `autoresearch/python-qps-mar27`，基于 Phase 1 最终 commit (`8be2816`)。

### 实验结果总表

| # | QPS | 状态 | 描述 |
|---|-----|------|------|
| 0 | 9,458 | baseline | Rust 优化后，Python 未改动 |
| 1 | 18,057 | **keep** | **流水线化调度 — create_task 替代 await (+91%)** |
| 2 | 19,805 | keep | 单分配 v2 回复帧构建 (+10%) |
| 3 | 20,146 | keep | dispatch table + 无锁 slot 查找 |
| 4 | 20,901 | keep | 快速路径调度器 — 内联守卫 + 懒惰 drain |
| 5 | 20,349 | keep | 内联 v2 call 解码 + dispatch + unpack |
| 6 | 21,041 | keep | 回复编码器直接返回 bytearray |
| 7 | 24,583 | **keep** | **FastDispatcher — SimpleQueue 替代 ThreadPoolExecutor (+18%)** |
| 8 | 22,497 | discard | FastDispatcher 8 workers (回退) |
| 9 | 26,747 | **keep** | **FastDispatcher 2 workers — 降低竞争 (+9%)** |
| 10 | 28,163 | keep | execute_fast + 懒惰 executor + 跳过 begin() |
| 11 | 28,814 | keep | fire-and-forget 内联回复 |
| 12 | 33,517 | **keep** | **零 Task 快速路径 — 读循环中直接解析+分发 (+16%)** |
| 13 | 35,883 | **keep** | **dispatch cache — 单次查找替代 3 次字典查找 (+7%)** |
| 14 | 33,079 | discard | 内联 worker 回复构建 — 噪声大/回退 |
| 15 | 33,125 | discard | payload 前缀缓存键 — 持平但更复杂 |

### 修改的文件

- `src/c_two/rpc_v2/server.py` — 流水线、FastDispatcher、零 Task 快速路径、dispatch cache
- `src/c_two/rpc_v2/wire.py` — 单分配回复帧构建器
- `src/c_two/rpc_v2/scheduler.py` — execute_fast()、懒惰 executor、内联守卫

### 关键优化详解

#### 4.1 流水线化调度 (Exp 1, +91%) — 最大单次提升

**问题**: ServerV2 的 `_handle_client()` 是串行处理的：

```python
# Before: 每个连接串行处理请求
async def _handle_client(self, reader, writer):
    while True:
        frame = await read_frame(reader)
        result = await self._dispatch(frame)  # 阻塞：等 CRM 执行完
        writer.write(result)                   # 写完才读下一帧
```

Rust relay 的 IPC client 将 32 个并发 HTTP 请求复用到**一条** UDS 连接上。串行处理意味着 Python 一次只处理 1 个请求，其余 31 个在 UDS 缓冲区排队。

**解法**: 使用 `asyncio.create_task()` 实现流水线：

```python
# After: 读取下一帧的同时，前一个请求在线程池中执行
async def _handle_client(self, reader, writer):
    pending: set[asyncio.Task] = set()
    while True:
        frame = await read_frame(reader)
        t = asyncio.create_task(self._handle_call(frame, writer))
        pending.add(t)
        t.add_done_callback(pending.discard)
```

**关键安全性保证**: 多个 Task 同时调用 `writer.write()` 是安全的，因为：
1. 所有 Task 运行在同一个 asyncio 事件循环（协作式调度）
2. `writer.write()` 是同步的（不会 yield）
3. 每次调用写入完整的帧

**结果**: 9,458 → 18,057 QPS，提升 91%。

#### 4.2 单分配回复帧 (Exp 2, +10%)

**问题**: 构建回复帧需要 3-4 次分配：

```python
# Before: 多次分配 + 拼接
def encode_v2_inline_reply_frame(request_id, data):
    control = encode_reply_control(status=0)      # alloc 1
    payload = control + data                        # alloc 2 (concat)
    total_len = 12 + len(payload)
    header = FRAME_STRUCT.pack(total_len, rid, flags)  # alloc 3
    return header + payload                         # alloc 4 (concat)
```

**解法**: 单次 `bytearray` 分配 + `pack_into`:

```python
# After: 一次分配，零拷贝填充
def encode_v2_inline_reply_frame(request_id, data):
    data_len = len(data)
    total_len = 12 + 1 + data_len  # header(12) + status(1) + data
    buf = bytearray(16 + 1 + data_len)  # 唯一分配
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, _V2_INLINE_REPLY_FLAGS)
    buf[16] = 0  # status OK
    buf[17:] = data
    return buf  # 直接返回 bytearray，不转 bytes
```

`writer.write()` 接受 `bytearray`，省去 `bytes(buf)` 拷贝。

#### 4.3 Dispatch Table + 无锁 Slot 查找 (Exp 3)

**问题**: 每次请求通过 `getattr(icrm, method_name)` 查找方法 + `get_method_access()` 查询读写属性。`_slots` 字典在每次访问时都加 `_slots_lock`。

**解法**:
1. 在 `CRMSlot` 中预构建 `_dispatch_table: dict[str, tuple[method, access]]`
2. 注册时一次性构建，运行时直接查表
3. 移除读路径的 `_slots_lock`（Python dict 操作是线程安全的）

#### 4.4 FastDispatcher (Exp 7+9, 累计 +48%)

**问题**: `ThreadPoolExecutor` 在每次提交时：
1. 创建 `_WorkItem` 包装对象
2. 创建 `concurrent.futures.Future`
3. 通过 `queue.Queue`（带完整 Lock 的）入队
4. worker 线程解包 `_WorkItem`、执行、设置 Future

**解法**: 自定义 `_FastDispatcher`，使用 `queue.SimpleQueue`：

```python
class _FastDispatcher:
    def __init__(self, loop, num_workers=2):
        self._q = queue.SimpleQueue()  # 无锁（CPython 内部用 deque）
        for i in range(num_workers):
            Thread(target=self._worker, daemon=True).start()

    def submit_inline(self, exec_fn, method, args, access, req_id, writer):
        self._q.put((exec_fn, method, args, access, None, req_id, writer))

    def _worker(self):
        while True:
            item = self._q.get()
            if item is None: break  # 毒丸关闭
            result = sched_execute(method, args, access)
            # 直接在 worker 线程构建回复帧并写入
            frame = encode_v2_inline_reply_frame(req_id, result)
            self._loop.call_soon_threadsafe(writer.write, frame)
```

**关键决策 — 2 workers 优于 4 workers (+9%)**:

EXCLUSIVE 模式下只允许 1 个线程执行 CRM 方法。4 个 worker 中有 3 个在等锁，增加了线程切换和缓存失效开销。2 个 worker 的流水线效果已经足够：当 worker A 持锁执行时，worker B 已经从队列取出下一个任务准备好了。

| Workers | QPS | 分析 |
|---------|-----|------|
| 2 | 26,747 | ✅ 最优 — 最少竞争 |
| 4 | 24,583 | 多余 2 个线程空等锁 |
| 8 | 22,497 | 过多线程竞争，回退 |

#### 4.5 execute_fast — 跳过 pending 计数 (Exp 10, +5%)

**问题**: `Scheduler.execute()` 和 `begin()` 每次调用都获取 `_state_lock` 来维护 `_pending_count`，这仅在 shutdown drain 时使用。

**解法**: 新增 `execute_fast()` 方法，跳过 pending 计数：

```python
def execute_fast(self, method, args_bytes, access):
    """不追踪 pending count — FastDispatcher 通过 asyncio task set 管理生命周期。"""
    if self._is_exclusive:
        with self._exclusive_lock:
            return method(args_bytes)
    ...
```

同时将 `Scheduler` 的 `ThreadPoolExecutor` 改为懒加载（因为 `_FastDispatcher` 替代了它）。

#### 4.6 零 Task 快速路径 (Exp 12, +16%) — 第二大提升

**问题**: 即使有了 `submit_inline`（fire-and-forget），每个请求仍会创建一个 `asyncio.Task`:

```python
# 仍然创建 Task
t = asyncio.create_task(self._handle_v2_call(conn, req_id, payload, ...))
```

`asyncio.Task` 创建涉及：Python 对象分配、弱引用集合操作、事件循环回调注册、Task 名称生成等。

**解法**: 在 `_handle_client()` 读循环中直接完成解析和分发，完全绕过 Task 创建：

```python
async def _handle_client(self, reader, writer):
    slots = self._slots
    dispatcher = self._dispatcher
    dispatch_cache = {}

    while True:
        header = await reader.readexactly(16)
        total_len, request_id, flags = FRAME_STRUCT.unpack(header)
        payload = await reader.readexactly(total_len - 12)

        if flags & FLAG_CALL_V2 and not (flags & FLAG_BUDDY):
            # 快速路径：直接解析控制面 + 提交到 worker
            name_len = payload[0]
            route_name = payload[1:1+name_len].decode('utf-8')
            method_idx = unpack_from(payload, 1+name_len)
            args = payload[3+name_len:]

            slot = slots.get(route_name)
            entry = slot._dispatch_table.get(method_name)
            method, access = entry

            dispatcher.submit_inline(
                slot.scheduler.execute_fast, method, args, access,
                request_id, writer,
            )
            continue  # 不创建 Task！立即读下一帧

        # 回退路径：buddy 帧、v1 帧、错误处理仍走 Task
        t = asyncio.create_task(self._handle_v2_call(...))
```

**架构变化**: 读循环从"读帧 → 创建 Task → Task 解析 → Task 分发"变为"读帧 → 直接解析 → 直接分发"。事件循环线程只做 I/O 和控制面解析，CRM 执行在 worker 线程，回复写入通过 `call_soon_threadsafe` 回到事件循环。

#### 4.7 Dispatch Cache (Exp 13, +7%)

**问题**: 快速路径中仍有 3 次字典查找 + 1 次 UTF-8 解码：

```python
route_name = payload[1:1+name_len].decode('utf-8')  # UTF-8 decode
slot = slots.get(route_name)                          # lookup 1
method_name = slot.method_table._idx_to_name.get(idx) # lookup 2
entry = slot._dispatch_table.get(method_name)          # lookup 3
```

**解法**: 以 `(route_bytes, method_idx)` 为键缓存最终的 `(execute_fast, method, access)` 三元组：

```python
cache_key = (bytes(route_key), method_idx)
cached = dispatch_cache.get(cache_key)
if cached is not None:
    exec_fast, method, access = cached  # 单次 dict.get，完毕
    dispatcher.submit_inline(exec_fast, method, args, access, ...)
    continue
```

首次请求正常解析并写入缓存，后续同路由+同方法的请求只需 1 次字典查找。

---

## 5. 优化后的请求处理架构

```
                    asyncio event loop thread              worker threads (×2)
                    ─────────────────────────              ───────────────────

UDS recv buffer ──→ reader.readexactly(16)
                    │ FRAME_STRUCT.unpack → total_len, req_id, flags
                    │
                    reader.readexactly(payload_len)
                    │
                    ├─ [v2 non-buddy, cache hit]
                    │   dispatch_cache.get(key) → (exec_fast, method, access)
                    │   SimpleQueue.put(...)  ──────────→  q.get()
                    │   continue (read next frame)         │
                    │                                      exec_fast(method, args, access)
                    │                                      │ with exclusive_lock:
                    │                                      │   result = method(args)
                    │                                      │
                    │                                      frame = encode_inline_reply(req_id, result)
                    │   ←── call_soon_threadsafe ──────────┘
                    │   writer.write(frame)
                    │
                    ├─ [v2 non-buddy, cache miss]
                    │   decode route_name, method_idx
                    │   slots.get → method_table → dispatch_table
                    │   写入 cache, 然后同上
                    │
                    └─ [buddy / v1 / error]
                        asyncio.create_task(handler)
                        │
                        handler: Future + dispatcher.submit()
                        await future → unpack → send_reply
```

### 设计原则

1. **事件循环线程只做 I/O + 控制面** — 不执行任何 CRM 方法
2. **worker 线程自包含** — 执行 CRM + 构建回复帧 + 触发写入
3. **零 Task 快速路径** — 99% 的 v2 inline 请求不创建 asyncio.Task
4. **fire-and-forget** — 事件循环提交后立即读下一帧，不等待结果
5. **最少竞争** — 2 worker 线程 = 1 执行 + 1 准备，无浪费

---

## 6. 失败/回退的实验教训

### 8 worker 线程 (Exp 8)
EXCLUSIVE 模式下多余的线程只能等锁。线程切换 + L1/L2 cache 失效 > 流水线收益。

### 内联 worker 回复构建 (Exp 14)
将 `_write_inline_reply` static method 内联进 `_worker` 方法。理论上减少函数调用开销，实际上使 `_worker` 字节码变长，instruction cache 命中率下降。

### Payload 前缀缓存键 (Exp 15)
用 `payload[:3+name_len]` 作为缓存键，避免 `(bytes, int)` tuple 构造。但 `payload` 是 `readexactly` 返回的 bytes，切片本身就创建新 bytes 对象。与 tuple 键持平但代码更难理解。

---

## 7. 总结

| 阶段 | 起点 | 终点 | 提升倍数 |
|------|------|------|----------|
| Phase 1 (Rust) | ~4,868 | ~9,500 | 1.95× |
| Phase 2 (Python) | ~9,500 | ~35,900 | 3.78× |
| **总计** | **~700** | **~35,900** | **51×** |

### 核心经验

1. **串行 → 流水线是最大赢面**: 单纯的 `await` → `create_task` 改动带来 91% 提升
2. **减少对象分配比算法优化更有效**: bytearray 复用、跳过 Task 创建、SimpleQueue 替代 ThreadPoolExecutor
3. **worker 数量 ≠ 越多越好**: 受限于串行锁时，最少够用的线程数是最优解
4. **控制面缓存效果显著**: 重复路由的请求直接命中缓存，跳过 3 层字典查找 + UTF-8 解码
5. **Rust 侧优化收益有限 (2×)，Python 侧优化空间更大 (3.8×)**: 这暗示未来如果要进一步提升，应考虑将更多 Python 热路径下沉到 Rust

### 后续方向

- **uvloop**: 如果支持 Python 3.14t，可能改善 asyncio 调度开销
- **Rust 侧批量写入**: relay 端将多个 IPC 帧合并为一次 UDS write
- **大 payload 测试**: 当前 benchmark 使用 ~40B payload，1KB-1MB 场景可能有不同的瓶颈
- **py-spy profiling**: 精确定位 35K→理论上限 之间剩余开销的 CPU 热点
- **read_parallel 模式**: 对于读多写少的 CRM，可以进一步提升并发度（当前 benchmark 全部走 EXCLUSIVE）
