# C-Two 后续计划

> Date: 2026-03-28 | Updated: 2026-03-29 (heartbeat §2.2 completed)
> 基于: `doc/log/sota.md`（SOTA 设计文档）+ 当前代码库实现状态
> 目的: 系统梳理 SOTA 设计目标与已有实现之间的差距，规划后续工作优先级

---

## 0. 当前实现状态总览

### 已完成 ✓

| SOTA 设计点 | 实现位置 | 状态 |
|---|---|---|
| 进程级注册器 `cc.register()` / `cc.connect()` / `cc.unregister()` | `transport/registry.py` | ✓ 完整实现 |
| 隐式 IPC 服务启动（register 时后台线程启动 Server） | `registry.py:register()` → `Server.start()` | ✓ |
| 线程优惠（同进程直接返回 CRM 实例，零序列化） | `registry.py:connect()` → `ICRMProxy.thread_local()` | ✓ |
| 多 CRM 单 Server（单端口多路复用） | `server/core.py:Server` 多 `CRMSlot` | ✓ — 满足 sota.md §4.1 |
| Wire 控制面/数据面分离 | `wire.py` + `protocol.py` | ✓ |
| Handshake 能力协商 + 方法索引 | `protocol.py:Handshake` | ✓ |
| Buddy Pool SHM 零系统调用分配 | `c2-buddy` (Rust crate) | ✓ |
| SharedClient（N 消费者 → 1 UDS + 1 Pool） | `client/core.py:SharedClient` | ✓ |
| 统一代理对象（thread / ipc / http 三模式） | `proxy.py:ICRMProxy` | ✓ — 满足 sota.md §4.2 |
| HTTP relay（Python + Rust） | `relay/core.py:Relay` + `c2-relay` (Rust crate) | ✓ |
| HTTP client + 跨节点访问 | `client/http.py:HttpClient` | ✓ |
| Rust SDK 基础（c2-wire, c2-ipc, c2-buddy, c2-relay, c2-ffi） | `src/c_two/_native/` workspace | ✓ |
| @cc.read / @cc.write 并发控制 | `server/scheduler.py:Scheduler` + `_WriterPriorityReadWriteLock` | ✓ |
| Per-connection write barrier（同连接因果序保证） | `server/core.py:_handle_client` + barrier 机制 | ✓ (2026-03-28) |
| dispatch_cache 代际失效 | `server/core.py:_slots_generation` | ✓ (2026-03-28) |
| 连接级 in-flight 计数器（writer.close 安全性） | `server/connection.py:Connection.flight_inc/dec/wait_idle` | ✓ (2026-03-28) |
| 分块流式传输（Chunked Streaming Transfer） | `wire.py` + `client/core.py` + `server/core.py` | ✓ (2026-03-28) |
| 线程优惠的并发控制 | `ICRMProxy.thread_local()` 接收 `Scheduler` + `access_map` | ✓ (2026-03-29) |
| @cc.shutdown 声明式生命周期回调 | `crm/meta.py` + `server/core.py` + `registry.py` | ✓ (2026-03-29) |
| 心跳检测 + 连接空闲清理 + SHM 孤儿回收 | `server/heartbeat.py` + `server/core.py` + `client/core.py` | ✓ (2026-03-29) |

### 仓库重构 (2026-03-29) ✓

| 重构项 | 说明 |
|---|---|
| 传输层统一 | `rpc/` + `rpc_v2/` → `transport/`，消除双模块冗余 |
| Wire 协议统一 | 去除 Wire v1，仅保留控制面/数据面分离的编码格式 |
| 版本标签归一 | `HandshakeV5` → `Handshake`, `ipc-v3://` → `ipc://`, 全面去版本化 |
| Server 解耦 | 1497 行 core.py → 7 模块（core, connection, chunk, reply, dispatcher, handshake, scheduler） |
| 配置暴露 | 11 项硬编码常量移入 `IPCConfig`，3 项僵尸常量删除 |

### 部分实现 ⚠

| 设计点 | 差距 |
|---|---|
| SHM Pool 动态扩容 | Rust buddy 支持多 segment (max_segments=8)，`CTRL_BUDDY_ANNOUNCE` 协议已定义。但 Python 层 **Client 硬编码 1 segment**，Server 不发送扩容通告 |
| Rust relay 集成 | Rust relay (`c2-relay`) 代码完整，但未嵌入 Python 发布流程（无 wheel 分发、无 `cc.router.start()` 封装） |

### 未实现 ✗

| 设计点 | sota.md 章节 | 说明 |
|---|---|---|
| 版本兼容性检查 | §1.2 | `cc.connect()` 不接受版本参数，ICRM 的 `version` 字段未用于选择 |
| 异步接口 | §4.3 | 无 `cc.connect_async()` 或 `async with cc.connect()` |
| 自适应分代 GC | 附录 A.3 | per-request SHM 与 pool SHM 使用相同超时 |
| 跨语言 CRM Client（Rust/C++） | 附录 Rust §代码组织 | `c2-wire` + `c2-ipc` 已存在，但无 Rust 端的 ICRM 代码生成 |

---

## 1. P0 — 必须修复的设计缺陷 ✅ 全部完成

### 1.1 线程优惠的并发控制缺失 — ✅ 已完成 (2026-03-29)

`ICRMProxy.thread_local()` 构造时接收 `Scheduler` + `access_map`，`call_direct()` 内部经过 `scheduler._execution_guard(access)` 保护。线程优惠保留零序列化优势，增加 RW 锁开销（EXCLUSIVE 模式 ~100ns/call）。

### 1.2 @cc.shutdown 装饰器 — ✅ 已完成 (2026-03-29)

声明式 CRM 生命周期回调。`@cc.shutdown` 标记的方法在 `cc.unregister()` / `cc.shutdown()` / atexit 时自动调用。不进入 dispatch_table（不可被 RPC 调用）。

---

## 2. P1 — 性能与可靠性增强

### 2.1 SHM Pool 动态扩容（Segment Chain）— 优先级降低

**原始问题**: Pool 满时退化到 per-request SHM（5 个系统调用/request），这在大 payload 场景下造成显著性能下降。

**当前状态**: 分块流式传输（Chunked Streaming Transfer）已消除最大痛点（256 MB 单次 RPC 限制）。

**实现进度 (~20%)**:
- ✅ Rust buddy allocator 支持多 segment（`max_segments=8`），含 3 层 fallback（现有 → 扩容 → 专用）
- ✅ `CTRL_BUDDY_ANNOUNCE` 协议编解码已定义（`ipc/buddy.py`）
- ❌ Python Client 硬编码 `max_segments=1`（`client/core.py:286`），不支持接收新 segment
- ❌ Server 不发送扩容通告，镜像 Client 的 segment 数量
- ❌ Client `_recv_loop` 不处理 `CTRL_BUDDY_ANNOUNCE`

**剩余价值**: 减少 per-request SHM 退化频率。如有需求可在 P2 阶段实施。

### 2.2 心跳检测 + 进程存活检测 — ✅ 已完成 (2026-03-29)

Server 端主动心跳探测 + 客户端 PONG 自动响应 + SHM 孤儿回收，三阶段全部实现。

**实现内容**:
- `Connection` 增加 `last_activity` / `touch()` / `idle_seconds()` 跟踪连接活跃度
- 新增 `server/heartbeat.py:run_heartbeat()` 协程 — per-connection asyncio task，周期发送 PING
- `SharedClient._recv_loop` 增加 `FLAG_SIGNAL` 检测，自动回复 PONG（通过 `_send_lock` 保护线程安全）
- `_handle_client` 集成心跳任务：每帧 `conn.touch()`，finally 中 cancel + 超时日志
- `Server.shutdown()` 调用 `cleanup_stale_shm('cc')` 回收死进程遗留的 SHM segment
- `heartbeat_interval <= 0` 禁用心跳（已有 `IPCConfig` 验证）

**测试覆盖**: 10 单元 + 4 集成（活跃连接存活、空闲 PONG 存活、死客户端检测、禁用模式）

**详细实施方案**: `docs/superpowers/plans/2025-07-25-heartbeat-detection.md`

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

## 5. 实施时间线

```
P0 — 设计缺陷修复 ✅
├─ 1.1 线程优惠并发控制         ✅ 已完成 (2026-03-29)
└─ 1.2 @cc.shutdown 回调        ✅ 已完成 (2026-03-29)

P0.5 — 大 payload 支持 ✅
└─ 分块流式传输 (Plan A)        ✅ 已完成 (2026-03-28)

仓库重构 ✅ (2026-03-29)
├─ rpc/ + rpc_v2/ → transport/  ✅ 消除双模块
├─ Wire v1 移除                 ✅ 仅保留统一编码
├─ 版本标签归一                 ✅ Handshake/ipc:// 去版本化
├─ Server core 解耦             ✅ 7 模块
└─ 配置暴露                     ✅ IPCConfig 11 项新字段

P1 — 性能与可靠性增强
├─ 2.1 SHM Pool 动态扩容        优先级降低（Rust 侧已就绪，Python 层待实现）
├─ 2.2 心跳 + PID 存活检测      ✅ 已完成 (2026-03-29)
└─ 2.3 Rust Relay 发布集成      ← 下一目标（CI/CD 工程）

P2 — 功能完善（v0.4.0 里程碑）
├─ 3.0 磁盘溢写兜底 (Plan C)
├─ 3.1 版本兼容性检查
├─ 3.2 异步接口
└─ 3.3 自适应分代 GC

P3 — 长期演进（v1.0 方向）
├─ 4.1 跨语言 CRM Client
├─ 4.2 Streaming RPC
└─ 4.3 服务发现
```

---

## 6. 与优化报告的关联

本次 relay QPS 优化（`doc/log/relay-qps-optimization.md`）的成果和遗留问题对后续计划的影响：

| 优化成果 | 对后续计划的影响 |
|---|---|
| `_FastDispatcher` + `SimpleQueue` 替代 `ThreadPoolExecutor` | §1.1 ✅ 线程优惠并发控制已复用 `execute_fast` 路径 |
| Per-connection write barrier | §3.2 异步接口需要兼容 barrier 机制 |
| dispatch_cache + `_slots_generation` | §3.1 版本兼容性检查可复用 generation 失效机制 |
| Rust relay 35K+ QPS | §2.3 集成后即可作为生产级 HTTP 路由层 |
| flight_inc/dec/wait_idle | §2.2 ✅ 心跳检测已复用 flight counter 判断连接活跃度 |
| Chunked Streaming Transfer | ✅ 消除 256 MB 单次 RPC 限制，§2.1 SHM Pool 扩容优先级降低 |

### 优化报告中建议但未覆盖的后续方向

| 优化报告建议 | 对应本计划章节 |
|---|---|
| uvloop 支持 | 不列入计划——Python 3.14t (free-threaded) 兼容性未知 |
| Rust 侧批量写入 | §4.2 Streaming RPC 的前置技术 |
| 大 payload 测试 | §2.1 SHM Pool 动态扩容完成后作为验证 benchmark |
| py-spy profiling | 作为 §P1 阶段的辅助手段，不单独列为计划项 |
| read_parallel 模式优化 | §1.1 修复线程优惠并发控制后自然获得 |
