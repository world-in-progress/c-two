# C-Two RPC v2 后续计划

> Date: 2026-03-28
> 基于: `doc/log/sota.md`（SOTA 设计文档）+ 当前代码库实现状态（v0.2.7+, branch `autoresearch/python-qps-mar27`）
> 目的: 系统梳理 SOTA 设计目标与已有实现之间的差距，规划后续工作优先级

---

## 0. 当前实现状态总览

### 已完成 ✓

| SOTA 设计点 | 实现位置 | 状态 |
|---|---|---|
| 进程级注册器 `cc.register()` / `cc.connect()` / `cc.unregister()` | `rpc_v2/registry.py` | ✓ 完整实现 |
| 隐式 IPC-v2 服务启动（register 时后台线程启动 ServerV2） | `registry.py:register()` → `ServerV2.start()` | ✓ |
| 线程优惠（同进程直接返回 CRM 实例，零序列化） | `registry.py:connect()` → `ICRMProxy.thread_local()` | ✓ |
| 多 CRM 单 Server（单端口多路复用） | `server.py:ServerV2` 多 `CRMSlot` | ✓ — 满足 sota.md §4.1 |
| Wire v2 控制面/数据面分离 | `wire.py` + `protocol.py` | ✓ |
| Handshake v5 能力协商 + 方法索引 | `protocol.py:HandshakeV5` | ✓ |
| Buddy Pool SHM 零系统调用分配 | `c2-buddy` (Rust crate) | ✓ |
| SharedClient（N 消费者 → 1 UDS + 1 Pool） | `client.py:SharedClient` | ✓ |
| 统一代理对象（thread / ipc / http 三模式） | `proxy.py:ICRMProxy` | ✓ — 满足 sota.md §4.2 |
| HTTP relay（Python + Rust） | `relay.py:RelayV2` + `c2-relay` (Rust crate) | ✓ |
| HTTP client + 跨节点访问 | `http_client.py:HttpClient` | ✓ |
| Rust SDK 基础（c2-wire, c2-ipc, c2-buddy, c2-relay, c2-ffi） | `src/c_two/_native/` workspace | ✓ |
| @cc.read / @cc.write 并发控制 | `scheduler.py:Scheduler` + `_WriterPriorityReadWriteLock` | ✓ |
| Per-connection write barrier（同连接因果序保证） | `server.py:_handle_client` + barrier 机制 | ✓ (2026-03-28) |
| dispatch_cache 代际失效 | `server.py:_slots_generation` | ✓ (2026-03-28) |
| 连接级 in-flight 计数器（writer.close 安全性） | `server.py:_Connection.flight_inc/dec/wait_idle` | ✓ (2026-03-28) |
| 分块流式传输（Chunked Streaming Transfer） | `wire.py` + `client.py` + `server.py` | ✓ (2026-03-28) |

### 部分实现 ⚠

| 设计点 | 差距 |
|---|---|
| 线程优惠的并发控制 | `ICRMProxy.thread_local()` 返回的是**裸 CRM 实例**，不经过 `Scheduler` 的 RW 锁。若多个线程通过 `cc.connect()` 获取同一 CRM 并行调用，read/write 隔离**不生效** |
| Rust relay 集成 | Rust relay (`c2-relay`) 代码完整，但未嵌入 Python 发布流程（无 wheel 分发、无 `cc.router.start()` 封装） |

### 未实现 ✗

| 设计点 | sota.md 章节 | 说明 |
|---|---|---|
| `shutdown_callback` 语义 | §4.4 | `cc.register()` 无 callback 参数，shutdown 时的清理行为未定义 |
| 版本兼容性检查 | §1.2 | `cc.connect()` 不接受版本参数，ICRM 的 `version` 字段未用于选择 |
| 异步接口 | §4.3 | 无 `cc.connect_async()` 或 `async with cc.connect()` |
| SHM Pool 动态扩容 | 附录 A.2 | Pool 满时退化到 per-request SHM（5 个系统调用），无 segment chain |
| 心跳 + 进程存活检测 | 附录 A.3 | GC 仍依赖 120s 超时扫描，无 heartbeat / `os.kill(pid, 0)` |
| 自适应分代 GC | 附录 A.3 | per-request SHM 与 pool SHM 使用相同超时 |
| 跨语言 CRM Client（Rust/C++） | 附录 Rust §代码组织 | `c2-wire` + `c2-ipc` 已存在，但无 Rust 端的 ICRM 代码生成 |

---

## 1. P0 — 必须修复的设计缺陷

### 1.1 线程优惠的并发控制缺失

**问题**: `cc.connect()` 对同进程 CRM 返回 `ICRMProxy.thread_local(crm_instance)`。调用方直接操作 CRM 实例，不经过 `Scheduler`。如果多个线程并行调用同一 CRM 的 `@cc.write` 和 `@cc.read` 方法，读写隔离不生效。

**严重性**: 这是 SOTA 设计中明确要求但未实现的核心保证——"仍需遵守读写并发规则"。

**方案**:

`ICRMProxy.thread_local()` 模式下，调用链应为：

```
proxy.method(args) → Scheduler.execute_fast(method, args, access) → method(args)
```

而非当前的：

```
proxy.method(args) → crm.method(args)   # 绕过 Scheduler
```

实现路径：
1. `ICRMProxy.thread_local()` 构造时接收 `Scheduler` 引用
2. `call_direct()` 内部包装为 `scheduler.execute_fast(method, args, access)`
3. 线程优惠保留零序列化优势，但增加了锁开销（EXCLUSIVE 模式下约 ~100ns/call）

**改动范围**: `proxy.py` + `registry.py`（传递 Scheduler）

---

### 1.2 @cc.shutdown 装饰器 — 声明式 CRM 生命周期回调

**问题**: sota.md §4.4 要求 shutdown callback 支持，但当前 API 无此能力。当 CRM 被 unregister 或进程退出时，CRM 无法执行清理逻辑（如关闭文件句柄、flush 缓冲区）。

**方案**: 采用装饰器模式（与 `@cc.read` / `@cc.write` 一致），而非 `cc.register()` 的 callback 参数：

```python
@cc.icrm
class IGrid:
    @cc.read
    def get_grid(self, level, ids): ...

    @cc.write
    def update_grid(self, data): ...

    @cc.shutdown
    def cleanup(self):
        """CRM 被 unregister 或进程退出时自动调用。"""
        self.flush_cache()
        self.close_handles()
```

**设计优势**:
- 声明式：shutdown 行为定义在 ICRM 接口上，代码自文档化
- 模式统一：`@cc.read` / `@cc.write` / `@cc.shutdown` 构成完整的 CRM 生命周期声明
- 自动发现：`register_crm()` 扫描 ICRM class 即可，调用方无需显式传参
- `@cc.shutdown` 标记的方法**不**进入 dispatch_table（不可被 RPC 调用），仅在生命周期事件中本地执行

**触发场景**:
- `cc.unregister(name)` → 在 CRM 实例上调用 `@cc.shutdown` 方法
- `cc.shutdown()` (atexit) → 对所有注册 CRM 调用
- 远程 SHUTDOWN_CLIENT 信号到达 → 调用

**约束**: 每个 ICRM 最多一个 `@cc.shutdown` 方法（多个则注册时报错）

**改动范围**:
- `crm/meta.py` — 新增 `shutdown()` 装饰器，设 `_cc_shutdown = True` 属性
- `server.py:CRMSlot` — `build_dispatch_table()` 时扫描并记录 shutdown 方法
- `registry.py` — `unregister()` / `shutdown()` 时调用

---

## 2. P1 — 性能与可靠性增强

### 2.1 SHM Pool 动态扩容（Segment Chain）— 已部分替代

**原始问题**: Pool 满时退化到 per-request SHM（5 个系统调用/request），这在大 payload 场景下造成显著性能下降。sota.md 附录 A.2 建议动态扩容替代磁盘溢出。

**当前状态**: 分块流式传输（Chunked Streaming Transfer）已实现，将超过 `pool_segment_size * 0.9` 的大 payload 拆分为 `segment_size // 2` 大小的独立帧，每帧独立 buddy alloc → SHM write → send → free。理论上限 65535 chunks × 128 MB = 8 TB。此方案消除了单次 RPC 256 MB 限制这一最大痛点。

**剩余价值**: Segment Chain 仍可减少 per-request SHM 退化频率（多段 pool 提高容量），但优先级降低。如有需求可在 P2 阶段实施。

**改动范围**: `c2-buddy` (Rust) + `c2-ffi` (PyO3 绑定) + `client.py` / `server.py`（握手时协商 segment 列表）

### 2.2 心跳检测 + 进程存活检测

**问题**: 客户端被 SIGKILL 时 UDS socket 未关闭，Pool SHM 成为孤儿。当前依赖 120s 超时扫描，延迟过长。

**方案**:
1. **Server 端心跳**: 在 `_handle_client` 中设置 idle timeout（默认 30s）。超时无帧到达则发送 PING，PING 无响应则视为断连。
2. **Owner PID 检测**: SHM segment 名中嵌入创建者 PID。GC 扫描时 `os.kill(pid, 0)` 检测进程存活。
3. **Server 端 idle 超时**: 连接无请求超过 idle timeout → 主动 close + cleanup。

**改动范围**: `server.py`（心跳逻辑）+ `c2-buddy`（PID 嵌入）+ GC 模块（PID 检测）

### 2.3 Rust Relay 发布集成

**问题**: Rust relay (`c2-relay`) 性能远超 Python relay（估计 50K-200K vs 1K-5K QPS），但未集成到 Python 包分发中。

**方案**: 参考 `ruff` 的模式：
1. `c2-relay` 编译为独立二进制
2. GitHub Action CI 矩阵构建（Linux x86_64/aarch64, macOS x86_64/aarch64）
3. 二进制打包进 platform-specific wheel
4. Python 端 `cc.relay.start()` 通过 `subprocess` 启动，传入 IPC 地址和端口
5. `pip install c-two` 自动获取预编译 relay

**改动范围**: CI/CD 配置 + `c2-relay` 的独立 binary 构建 + Python thin wrapper

---

## 3. P2 — 功能完善

### 3.0 磁盘溢写兜底（Plan C）

**背景**: 分块流式传输解决了大 payload 突破 256 MB 限制的问题，但当系统物理内存不足以容纳 reassembly buffer（接收端使用 `mmap.mmap(-1, total_size)` 匿名映射）时，仍需要磁盘级兜底策略。

**方案**: 当 mmap 分配失败或检测到系统可用内存低于阈值时，`_ChunkAssembler` / `_ReplyChunkAssembler` 切换到文件支持的 mmap（`mmap.mmap(fd, size)`），将 reassembly buffer 溢写到临时文件。具体设计参见 spike 文档 `doc/spikes/performance-shm-streaming-large-payload-spike.md` 中的 Plan C。

**触发条件**: 可通过 `IPCConfig.disk_spill_threshold` 配置（默认 None = 不启用），当 `total_payload > threshold` 或 `mmap(-1, ...)` 抛出 `OSError` 时自动降级。

**改动范围**: `server.py:_ChunkAssembler` + `client.py:_ReplyChunkAssembler`（增加文件 mmap 分支） + `IPCConfig`（新增配置项）

### 3.1 版本兼容性检查

**问题**: `cc.connect()` 不验证目标 CRM 的 ICRM 版本。耦合模型 A 依赖模型 B 的 v2 接口，但 B 注册的是 v1，调用会静默失败。

**方案**:

```python
crm_ref = cc.connect(IGrid, name='grid', version='>=2.0')
```

实现路径：
1. `register()` 时将 ICRM 的 `__tag__` 中的 version 存入注册表
2. `connect()` 时校验版本约束（semver 匹配）
3. 跨节点场景：HTTP relay 的 `/_routes` 端点返回版本信息

**改动范围**: `registry.py` + `relay.py`（路由表增加版本字段）+ `proxy.py`

### 3.2 异步接口

**问题**: 当前 API 全部同步。在耦合模拟中，模型 A 需要模型 B 的数据但 B 尚未就绪时，同步调用阻塞。

**方案**: 双模式 API：

```python
# 同步（现有）
crm = cc.connect(IGrid, name='grid')
result = crm.get_grid(level, ids)

# 异步（新增）
async with cc.connect_async(IGrid, name='grid') as crm:
    result = await crm.get_grid(level, ids)
```

实现路径：
1. `ICRMProxy` 增加 async 变体（`__aenter__` / `__aexit__`）
2. IPC 路径：底层已是 asyncio，暴露 async call 接口
3. HTTP 路径：使用 `httpx.AsyncClient`
4. Thread-local 路径：`await loop.run_in_executor(None, method, args)`

**改动范围**: `proxy.py` + `client.py` + `http_client.py` + `registry.py`

### 3.3 自适应分代 GC

**问题**: per-request SHM 和 pool SHM 使用相同的 120s 超时。高频 per-request SHM 场景下泄漏 segment 累积可耗尽 `/dev/shm`。

**方案**:

```
per-request SHM: timeout = max(30s, 2 × 最近 N 次 RPC 的 P99 延迟)
pool SHM:        timeout = 120s（不变）
```

附加：维护 C-Two 自身创建的 SHM segment 列表（基于 `cc` / `ccpr_` 前缀过滤），只对自己管理的部分做压力判断，避免为其他进程的 SHM 消耗买单。

**改动范围**: GC 模块（超时自适应）+ SHM 清理逻辑

---

## 4. P3 — 长期演进

### 4.1 跨语言 CRM Client（Rust/C++）

`c2-wire` + `c2-ipc` 已经是完整的 Rust IPC client 实现。下一步是：

1. **Rust ICRM 代码生成**: 从 Python `@cc.icrm` 装饰器生成 Rust client stub
2. **C++ 绑定**: 通过 `c2-ipc` 的 C-FFI 暴露给 C++ 计算模型
3. **典型应用**: Fortran/C++ PDE 求解器通过 Rust FFI 访问 Python 端的网格 CRM

### 4.2 Streaming RPC / Pipeline

当前是纯请求-响应模式。GIS 场景中的大数据传输（如逐 tile 流式返回 mesh 数据）需要：

1. Server-streaming RPC: 一个请求，多个响应帧
2. Client-streaming RPC: 多个请求帧，一个响应
3. 需要 wire v3 协议扩展（frame flags 中标记 stream continuation）

前置依赖: §2.1 SHM Pool 动态扩容 + 附录 A.1 的 slot 化引用计数

### 4.3 服务发现与命名空间治理

当前 `cc.connect(name='grid')` 依赖：
- 同进程: 本地注册表查找
- 跨进程: 调用方显式指定 `address` 参数
- 跨节点: 调用方显式指定 relay URL

SOTA 设计未覆盖的场景：
- 多节点环境中的**自动发现**（无需显式 address）
- 命名冲突解决（两个节点注册同名 CRM）

可选方案：
- 轻量级：基于环境变量/配置文件的静态路由表
- 中量级：etcd/consul 作为服务注册中心
- 重量级：自建 gossip 协议的 P2P 发现

建议从轻量级方案开始，仅在需求明确时升级。

---

## 5. 实施时间线建议

```
P0 — 设计缺陷修复 ✅
├─ 1.1 线程优惠并发控制      ✅ 已完成
└─ 1.2 shutdown_callback     ✅ 已完成

P0.5 — 大 payload 支持 ✅
└─ 分块流式传输 (Plan A)     ✅ 已完成 (2026-03-28)
   ├─ 协议层: FLAG_CHUNKED / FLAG_CHUNK_LAST / CAP_CHUNKED
   ├─ 客户端: _call_chunked + _ReplyChunkAssembler
   ├─ 服务端: _ChunkAssembler + _send_chunked_v2_reply
   └─ 基准: 256 MB chunked echo ≈ 1.9 GB/s

P1 — 性能与可靠性增强（v0.3.0 里程碑）
├─ 2.1 SHM Pool 动态扩容     优先级降低（分块传输已消除最大痛点）
├─ 2.2 心跳 + PID 存活检测    预计 1-2d
└─ 2.3 Rust Relay 发布集成    预计 2-3d (CI/CD)

P2 — 功能完善（v0.4.0 里程碑）
├─ 3.0 磁盘溢写兜底 (Plan C)  预计 1-2d
├─ 3.1 版本兼容性检查         预计 1d
├─ 3.2 异步接口               预计 2-3d
└─ 3.3 自适应分代 GC          预计 1-2d

P3 — 长期演进（v1.0 方向）
├─ 4.1 跨语言 CRM Client      评估阶段
├─ 4.2 Streaming RPC          设计阶段
└─ 4.3 服务发现               需求驱动
```

---

## 6. 与优化报告的关联

本次 relay QPS 优化（`doc/log/relay-qps-optimization.md`）的成果和遗留问题对后续计划的影响：

| 优化成果 | 对后续计划的影响 |
|---|---|
| `_FastDispatcher` + `SimpleQueue` 替代 `ThreadPoolExecutor` | §1.1 线程优惠并发控制可复用 `execute_fast` 路径 |
| Per-connection write barrier | §3.2 异步接口需要兼容 barrier 机制 |
| dispatch_cache + `_slots_generation` | §3.1 版本兼容性检查可复用 generation 失效机制 |
| Rust relay 35K+ QPS | §2.3 集成后即可作为生产级 HTTP 路由层 |
| flight_inc/dec/wait_idle | §2.2 心跳检测可复用 flight counter 判断连接活跃度 |
| Chunked Streaming Transfer | 消除 256 MB 单次 RPC 限制，§2.1 SHM Pool 扩容优先级降低 |

### 优化报告中建议但未覆盖的后续方向

| 优化报告建议 | 对应本计划章节 |
|---|---|
| uvloop 支持 | 不列入计划——Python 3.14t (free-threaded) 兼容性未知 |
| Rust 侧批量写入 | §4.2 Streaming RPC 的前置技术 |
| 大 payload 测试 | §2.1 SHM Pool 动态扩容完成后作为验证 benchmark |
| py-spy profiling | 作为 §P1 阶段的辅助手段，不单独列为计划项 |
| read_parallel 模式优化 | §1.1 修复线程优惠并发控制后自然获得 |
