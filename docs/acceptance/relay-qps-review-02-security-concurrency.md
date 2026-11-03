# Review Part 2: Security & Concurrency Vulnerabilities

> Reviewer: Claude Code
> Date: 2026-03-27
> Target: `src/c_two/rpc_v2/server.py`, `src/c_two/rpc_v2/scheduler.py`, `src/c_two/rpc_v2/wire.py`
> Branch: `autoresearch/python-qps-mar27`

---

## 1. 并发漏洞

### 1.1 [LOW] `_handle_client` 中多个 Task / worker 线程并发写 `writer`

**位置**: `server.py:169-175`（Exp 1 流水线化）, `server.py:606-618`（fallback Task 路径）, `server.py:234`（`_write_inline_reply` 通过 `call_soon_threadsafe`）

**问题描述**:

报告声称（第 4.1 节）：
> `writer.write()` 是同步的（不会 yield），因此多个 Task 并发调用是安全的

这个陈述**在 asyncio 单线程模型中对 Task 而言是正确的**。CPython asyncio StreamWriter 的 `write()` 只是把数据追加到发送缓冲区，不是 coroutine，不会 yield，所以单线程事件循环中不存在交错问题。

**然而**，`_FastDispatcher._write_inline_reply` 通过 `call_soon_threadsafe(writer.write, frame)` 从 **worker 线程** 触发写操作（`server.py:234`）。`call_soon_threadsafe` 将回调投递到事件循环的线程安全队列，最终在事件循环线程中执行。所以实际执行仍在事件循环线程，并发安全性由 `call_soon_threadsafe` 保证。

**结论**: 此处设计正确，但报告的解释只提到了 Task 路径的安全理由，未提及 worker 线程通过 `call_soon_threadsafe` 写入的安全保证来源。如果未来有人直接在 worker 线程调用 `writer.write()`（而非通过 `call_soon_threadsafe`），将引入真实的并发写冲突。代码中需要更强的注释防止此误用。

---

### 1.2 [MEDIUM] `dispatch_cache` 在 CRM 动态注册/注销时的一致性问题

**位置**: `server.py:533, 578-602`

```python
dispatch_cache: dict[tuple[bytes, int], tuple] = {}
# ...
dispatch_cache[cache_key] = (slot.scheduler.execute_fast, method, access)
```

**问题描述**:

`dispatch_cache` 在每个连接的 `_handle_client` 生命周期内缓存 `(execute_fast, method, access)` 三元组。这三个对象都是从 `CRMSlot` 提取的：
- `execute_fast` = `slot.scheduler.execute_fast`（绑定方法，引用 scheduler 对象）
- `method` = `slot._dispatch_table[method_name][0]`（绑定方法，引用旧 CRM 实例）
- `access` = `slot._dispatch_table[method_name][1]`（枚举值，不可变，安全）

如果在连接存活期间发生 `unregister_crm(name)` 再 `register_crm(..., name=name)`：
1. 旧 `CRMSlot` 被销毁，`slot.scheduler.shutdown()` 被调用（`server.py:390`）
2. 但 `dispatch_cache` 仍然持有旧 `scheduler.execute_fast` 的引用
3. 新请求通过 `dispatch_cache` 命中，调用旧 scheduler 的 `execute_fast`
4. 旧 scheduler 的 `_exclusive_lock` 仍然有效（Python 对象）
5. **旧 CRM 实例的方法被调用**，而非新注册的 CRM

**触发条件**: 长连接（Rust relay 连接池中的持久连接）+ 运行时动态 CRM 替换。

**风险评级**: MEDIUM（当前 benchmark/生产中 CRM 不做动态替换时不触发，但是设计中有此能力）。

**修复建议**:
- 方案 A：为每个 `_handle_client` 调用注入一个 `cache_generation` 版本号，CRM 注册/注销时递增全局版本。每次 cache hit 前检查版本是否匹配。
- 方案 B：`dispatch_cache` 不直接存储 method/access，而是存储 `(CRMSlot, method_name)` 二元组，每次查找时从当前 slot 动态获取。这增加了一次字典查找，但避免了悬挂引用。
- 方案 C（最简单）：文档注明 `dispatch_cache` 不安全于 CRM 动态替换场景，并在 `unregister_crm` 时记录警告日志。

---

### 1.3 [MEDIUM] `execute_fast` 绕过 shutdown 检查，关闭期间可能继续执行 CRM 方法

**位置**: `scheduler.py:192-212`

```python
def execute_fast(self, method, args_bytes, access_mode=MethodAccess.WRITE):
    """Like execute() but without pending-count tracking."""
    if self._is_exclusive:
        with self._exclusive_lock:
            return method(args_bytes)
    ...
```

**问题描述**:

`execute()` 中不存在，`execute_fast()` 也不存在 `_shutdown_flag` 检查。对比 `begin()`（`scheduler.py:214-227`）会在 shutdown 后拒绝新请求。

`_FastDispatcher` 在 shutdown 时：
1. 投递毒丸到 SimpleQueue
2. join worker 线程（timeout=2s，`server.py:186-189`）

但在 `dispatcher.shutdown()` 被调用之前，事件循环可能已经被 cancel（`server.py:479`），而 worker 线程可能还在 `execute_fast` 中执行 CRM 方法。如果 CRM 方法执行时间 > 2s（join timeout），worker 线程会被放弃（daemon 线程，进程退出时强制终止）。

**更具体的风险**: `Scheduler.shutdown()` 调用 `_drain_event.wait()`，但 `execute_fast` 不更新 `_pending_count`，所以 `_drain_event` 可能在 CRM 方法执行完毕前就已经 `set()`（因为 `execute()` 路径从未被调用过，`_pending_count` 始终为 0，`_drain_event` 始终为 set 状态）。

**结论**: 当前的 shutdown 逻辑存在"误以为已经 drain，实际上 worker 还在跑"的竞态。对于快速 CRM 方法（< 2s）实践中不会出问题，但对于长时间 CRM 方法（HPC 模拟场景中可能数十秒到数分钟）有数据完整性风险。

**[第二轮补充 — writer.close() 时序竞态]**:

更严重的问题发生在**连接断开**（非全局 shutdown）场景：当 `_handle_client` 的读循环因 `IncompleteReadError` 或 `CancelledError` 退出时，`finally` 块执行 `conn.cleanup()` → `writer.close()` → `await writer.wait_closed()`（`server.py:627-635`）。但此时 `_FastDispatcher` 的 worker 线程可能仍持有该连接 `writer` 的引用，正在执行已提交但尚未完成的 CRM 方法。

当 worker 最终完成执行，调用 `call_soon_threadsafe(writer.write, frame)` 时，writer 的底层 transport 已关闭。CPython asyncio 中 `Transport.write()` 在 transport 关闭后为静默 no-op（数据丢弃），这意味着：
- 客户端 **永远收不到回复**，只能等超时
- **无异常日志**，完全静默
- 对于 **已经成功执行的 CRM 方法**，结果被丢弃

这不仅影响 shutdown，任何客户端断连+正在执行的 submit_inline 请求都会触发。风险从 MEDIUM 修正为 **MEDIUM-HIGH**。

**修复建议**: `_handle_client` 的 `finally` 块在关闭 writer 之前，应等待 `_FastDispatcher` 中属于该连接的所有 pending 请求完成。可以通过为每个连接维护一个 `threading.Event`-based flight counter 实现。

---

### 1.4 [LOW] `_client_tasks` 列表无锁访问

**位置**: `server.py:329, 522, 636`

```python
self._client_tasks: list[asyncio.Task] = []
# ...
self._client_tasks.append(task)     # in _handle_client (event loop thread)
# ...
self._client_tasks.remove(task)     # in _handle_client (event loop thread)
```

`_async_main` 的 shutdown 路径中也读取了这个列表（`server.py:479`）。所有这些操作都在同一个 asyncio 事件循环线程中执行，因此**不存在并发问题**（asyncio 是协作式单线程调度）。

**但**，`shutdown()` 方法（`server.py:491-507`）从外部线程调用 `self._loop.call_soon_threadsafe(self._shutdown_event.set)`，触发的 `_async_main` shutdown 序列最终在事件循环线程执行 `for task in list(self._client_tasks)`（`server.py:479`）。这是安全的。

**结论**: 此处无实际漏洞，但代码中无注释说明为什么无锁访问是安全的，未来维护者可能误以为需要加锁。

---

### 1.5 [LOW] `_conn_counter` 非原子递增

**位置**: `server.py:518-519`

```python
self._conn_counter += 1
conn_id = self._conn_counter
```

这是在 asyncio 事件循环中执行的（`_handle_client` 是 coroutine），asyncio 保证协程间无并发，因此 `_conn_counter += 1` 是原子的（协作式调度，不会在 `+=` 中间被抢占）。此处无漏洞，但代码形式容易让人产生是否需要线程锁的疑问。

---

## 2. 安全漏洞

### 2.1 [LOW] SHM 段名称校验 — 路径穿越风险不成立（第二轮修正，从 HIGH 降级）

> **第二轮修正**: 第一轮将此项评为 HIGH，理由是"依赖 OS 行为做安全保证"。经仔细复核，该评级错误。

**位置**: `server.py:80, 727-729`

```python
_SHM_NAME_RE = re.compile(r'^/?[A-Za-z0-9_.\-]{1,255}$')
```

**第一轮声称的路径穿越场景 (`/../../../../etc/shadow`) 不可能发生**：

正则表达式的字符类 `[A-Za-z0-9_.\-]` 中**不包含 `/`**。`^/?` 仅允许首字符为可选的 `/`，之后禁止任何 `/`。因此：
- `/../..` — 第二个字符 `/` 不在字符类中，被拒绝
- `/dev/mem` — 内部 `/` 被拒绝
- `../../../../etc/shadow` — 内部 `/` 被拒绝

唯一的边界情况是名称 `/..`（或 `..`）——`..` 中的 `.` 在字符类中。但 `shm_open("/..")` 在 Linux 上映射到 `/dev/shm/..` = `/dev/`（目录），`open(O_RDWR|O_CREAT)` 在目录上返回 `EISDIR`，不会造成安全问题。macOS 的 POSIX SHM 由内核管理，非文件系统路径，同样安全。

**修正后评级**: LOW。正则表达式本身已经有效防止了路径穿越。可以考虑增加 `cc` 前缀白名单作为深度防御，但这是可选的加固而非必修漏洞。

---

### 2.2 ~~[MEDIUM] `_check_signal` 使用字节值匹配，可能被 pickle payload 触发误判~~ — 第二轮撤回，问题不成立

> **第二轮修正**: 此项结论错误，予以撤回。

经复核 v1 wire 协议的 `decode()` 实现（`rpc/util/wire.py:263-307`），v1 wire 格式的第一个字节**始终是** `MsgType` 枚举值（`CRM_CALL=0x03, PING=0x01, SHUTDOWN_CLIENT=0x05` 等）。`_check_signal` 检查的正是这同一个协议字段，是 `decode()` 的等价快速路径。

CRM_CALL 消息首字节永远为 `0x03`，不会等于 `0x01`（PING）或 `0x05`（SHUTDOWN_CLIENT）。**零碰撞风险**。

第一轮提出的 "pickle payload 触发误判" 场景不存在——payload 从未以原始 pickle 格式进入 v1 路径，wire 格式始终有 MsgType 头字节。此项应从漏洞列表中**删除**。

---

### 2.3 [MEDIUM] UDS socket 文件权限问题

**位置**: `server.py:469-472`

```python
self._server = await asyncio.start_unix_server(
    self._handle_client,
    path=self._socket_path,
)
```

`asyncio.start_unix_server` 创建的 UDS socket 默认权限为 `0666`（由进程 umask 控制，通常是 `0666 & ~umask`）。在多用户系统上，其他用户可以连接到该 socket，伪造 IPC 请求。

当前代码中无对 socket 文件权限的显式设置（如 `os.chmod`）。

**风险**: 在单用户开发机或 Docker 容器中风险极低。在多用户 HPC 节点（C-Two 的目标场景）上，同一节点的其他用户可以向 CRM Server 发送任意请求。

**修复建议**: 创建 socket 后立即 `os.chmod(self._socket_path, 0o600)`，并验证连接方的 UID（通过 `SO_PEERCRED` 或 `SCM_CREDENTIALS`）。

---

### 2.4 [LOW] `max_frame_size` 限制缺乏 payload_len 单独验证

**位置**: `server.py:538-541`

```python
if total_len < 12:
    raise ValueError(f'Frame too small: {total_len}')
if total_len > max_frame:
    raise ValueError(f'Frame too large: {total_len}')
```

`payload_len = total_len - 12` 在 `total_len >= 12` 时非负，`readexactly(payload_len)` 是安全的。但如果 `total_len` 字段被恶意客户端设置为 `max_frame`（例如 256MB），服务端会尝试分配 256MB 内存读取 payload，可能导致 OOM。

这是已知的帧长度注入攻击（length field injection）。在 UDS + 受信任内部组件场景下风险可接受，但若 relay 未对输入做充分验证（即 Rust axum relay 可能直接转发了外部 HTTP 请求的 payload 到 Python 侧），则存在 OOM 放大攻击面。

---

## 3. 快速路径中的健壮性问题

### 3.1 [MEDIUM] 快速路径对无效 method_idx 的静默丢弃

**位置**: `server.py:562-602`

```python
if is_v2_call and not is_buddy:
    name_len = payload[0]
    try:
        ...
    except (struct.error, UnicodeDecodeError):
        continue  # 静默丢弃！
    ...
    slot = (slots.get(route_name) if route_name else ...)
    if slot is not None:
        method_name = slot.method_table._idx_to_name.get(method_idx)
        if method_name is not None:
            entry = slot._dispatch_table.get(method_name)
            if entry is not None:
                ...
                continue

    # 如果 slot 为 None / method_name 为 None / entry 为 None：
    # 代码跳到下方 fallback Task 路径
```

当 `name_len = payload[0]` 但 payload 太短，`struct.error` 被捕获后 `continue` 直接跳到下一帧，**不发送任何错误回复**。客户端等待超时，无法区分是"服务端处理中"还是"帧被丢弃"。

同样，当 `slot` 存在但 `method_idx` 无效时，代码 fall through 到 fallback Task 路径。fallback `_handle_v2_call` 会重新解析同一 payload，再次失败并记录 warning，最终发送 error reply。但这个重新解析引入了不必要的开销，且日志中会出现重复的解析警告。

**修复建议**: 快速路径的 `except (struct.error, UnicodeDecodeError)` 分支应发送 error reply（而非 `continue`），与 fallback `_handle_v2_call` 的行为一致。

---

## 4. 漏洞优先级汇总（第三轮修正版 — 含代码修复）

> 标记 ✅ 的为已修复项，🔄 为第二轮修正项，❌ 为撤回项。

| ID | 等级 | 描述 | 状态 |
|---|---|---|---|
| 1.3 🔄 | **MEDIUM-HIGH** | writer.close() 时序竞态导致回复静默丢失 | ✅ **已修复**：`_Connection` 新增 `flight_inc/dec/wait_idle`，`finally` 块在关闭 writer 前 `await conn.wait_idle()` |
| 1.2 | MEDIUM | dispatch_cache 在 CRM 动态替换时持有旧实例引用 | ✅ **已修复**：新增 `_slots_generation` 计数器，CRM 注册/注销时递增，读循环每帧检查并按需清空 cache |
| 2.3 | MEDIUM | UDS socket 权限未限制 | ✅ **已修复**：`start_unix_server` 后 `os.chmod(path, 0o600)` |
| 3.1 | MEDIUM | 快速路径对畸形帧静默丢弃 | ✅ **已修复**：`except` 分支改为发送 `encode_v2_error_reply_frame` 回复 |
| 2.4 | LOW | 大帧可能导致 OOM 放大 | 暂不修复——UDS 受信任内部场景风险可接受 |
| 2.1 🔄 | LOW | SHM 段名校验——正则已有效防止路径穿越 | 无需修复 |
| 1.1 | LOW | call_soon_threadsafe 安全性注释缺失 | 无需修复——代码逻辑正确 |
| 1.4 | LOW | _client_tasks 无锁访问缺乏注释 | 无需修复——非实际漏洞 |
| 1.5 | LOW | _conn_counter 非原子看起来危险实则安全 | 无需修复——非实际漏洞 |
| ~~2.2~~ ❌ | ~~撤回~~ | ~~_check_signal 字节值误判~~ — 问题不存在 | — |

### 额外修复（非 review 内容）

| 描述 | 状态 |
|---|---|
| Per-connection write barrier：`@cc.write` 方法提交后 `await barrier.wait()` 暂停读循环，保证同连接因果序 | ✅ 已实现（spec: `docs/superpowers/specs/2026-03-28-per-conn-write-barrier-design.md`） |
