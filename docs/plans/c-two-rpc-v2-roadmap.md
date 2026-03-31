# C-Two 后续计划

> Date: 2026-03-28 | Updated: 2026-03-31 (Rust 模块重构 + 动态扩容完成)
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
| Handshake v6 能力协商 + 方法索引 + 池前缀交换 | `protocol.py:Handshake` | ✓ |
| 统一内存池 (Rust c2-mem) | `_native/c2-mem/` (alloc + segment + pool) | ✓ |
| SHM Pool 动态扩容（Segment Chain） | client 自动扩段 + server 懒加载 + 确定性命名 | ✓ (2026-03-31) |
| SharedClient（N 消费者 → 1 UDS + 1 Pool） | `client/core.py:SharedClient` | ✓ |
| 统一代理对象（thread / ipc / http 三模式） | `proxy.py:ICRMProxy` | ✓ — 满足 sota.md §4.2 |
| HTTP relay（Python + Rust） | `relay/core.py:Relay` + `c2-relay` (Rust crate) | ✓ |
| HTTP client + 跨节点访问 | `client/http.py:HttpClient` | ✓ |
| Rust SDK（c2-mem, c2-wire, c2-ipc, c2-relay, c2-ffi） | `src/c_two/_native/` workspace (5 crates) | ✓ |
| @cc.read / @cc.write 并发控制 | `server/scheduler.py:Scheduler` + `_WriterPriorityReadWriteLock` | ✓ |
| Per-connection write barrier（同连接因果序保证） | `server/core.py:_handle_client` + barrier 机制 | ✓ (2026-03-28) |
| dispatch_cache 代际失效 | `server/core.py:_slots_generation` | ✓ (2026-03-28) |
| 连接级 in-flight 计数器（writer.close 安全性） | `server/connection.py:Connection.flight_inc/dec/wait_idle` | ✓ (2026-03-28) |
| 分块流式传输（Chunked Streaming Transfer） | `wire.py` + `client/core.py` + `server/core.py` | ✓ (2026-03-28) |
| 线程优惠的并发控制 | `ICRMProxy.thread_local()` 接收 `Scheduler` + `access_map` | ✓ (2026-03-29) |
| @cc.shutdown 声明式生命周期回调 | `crm/meta.py` + `server/core.py` + `registry.py` | ✓ (2026-03-29) |
| 心跳检测 + 连接空闲清理 + SHM 孤儿回收 | `server/heartbeat.py` + `server/core.py` + `client/core.py` | ✓ (2026-03-29) |
| 优雅断连协议（DISCONNECT / DISCONNECT_ACK） | `client/core.py` + `server/core.py` + `c2-ipc/client.rs` | ✓ (2026-03-30) |
| Relay 空闲连接回收（Python + Rust 双实现） | `relay/core.py` + `c2-relay/state.rs` + `c2-relay/server.rs` | ✓ (2026-03-30) |
| Relay 死连接主动检测（get() + sweeper） | `relay/core.py:get()` + `c2-relay/state.rs:get()` | ✓ (2026-03-30) |
| Relay 调用失败即驱逐（Rust router） | `c2-relay/router.rs` 传输错误分支立即驱逐死客户端 | ✓ (2026-03-30) |
| DISCONNECT_ACK 显式处理（Rust recv_loop） | `c2-ipc/client.rs:recv_loop` 匹配 ACK 后 clean break | ✓ (2026-03-30) |
| 死代码清理（SHUTDOWN_SERVER 0x06） | `ipc/msg_type.py` 移除未使用的信号类型 | ✓ (2026-03-30) |
| 测试环境 .env 隔离 | `transport/config.py:C2_ENV_FILE` + `tests/conftest.py` | ✓ (2026-03-30) |
| CI 发布鲁棒性（skip-existing + verbose） | `.github/workflows/release.yml` | ✓ (2026-03-30) |
| CLI banner + dev 命令保护 | `cli.py` + `banner_unicode.txt` | ✓ (2026-03-30) |
| GIL-free 声明（Python 3.14t） | `c2-ffi` PyO3 模块声明 `GIL_USED=false` | ✓ (2026-03-30) |
| Rust 模块三层重构 + Python API 重命名 | `c2-mem` (alloc/ + segment/ + pool) + `c_two.mem.MemPool` | ✓ (2026-03-31) |
| uv 自动检测 Rust 源码变更并重编译 | `pyproject.toml:cache-keys` 追踪 `_native/**/*.rs` | ✓ (2026-03-31) |

### 仓库重构 ✓

| 重构项 | 说明 | 日期 |
|---|---|---|
| 传输层统一 | `rpc/` + `rpc_v2/` → `transport/`，消除双模块冗余 | 2026-03-29 |
| Wire 协议统一 | 去除 Wire v1，仅保留控制面/数据面分离的编码格式 | 2026-03-29 |
| 版本标签归一 | `HandshakeV5` → `Handshake`, `ipc-v3://` → `ipc://`, 全面去版本化 | 2026-03-29 |
| Server 解耦 | 1497 行 core.py → 7 模块（core, connection, chunk, reply, dispatcher, handshake, scheduler） | 2026-03-29 |
| 配置暴露 | 11 项硬编码常量移入 `IPCConfig`，3 项僵尸常量删除 | 2026-03-29 |
| Rust 模块重构 | `c2-buddy` → `c2-mem` (alloc/ + segment/ + pool)，Python `c_two.buddy` → `c_two.mem` | 2026-03-31 |
| SHM 帧模块重命名 | `transport/ipc/buddy.py` → `transport/ipc/shm_frame.py` | 2026-03-31 |

### 未实现 ✗

| 设计点 | sota.md 章节 | 说明 |
|---|---|---|
| 版本兼容性检查 | §1.2 | `cc.connect()` 不接受版本参数，ICRM 的 `version` 字段未用于选择 |
| 异步接口 | §4.3 | 无 `cc.connect_async()` 或 `async with cc.connect()` |
| 自适应分代 GC | 附录 A.3 | per-request SHM 与 pool SHM 使用相同超时 |
| 跨语言 CRM Client（Rust/C++） | 附录 Rust §代码组织 | `c2-wire` + `c2-ipc` 已存在，但无 Rust 端的 ICRM 代码生成 |
| 磁盘溢写兜底 | — | Chunk reassembly 匿名 mmap 在物理内存不足时无降级路径 |

---

## 1. P0 — 必须修复的设计缺陷 ✅ 全部完成

### 1.1 线程优惠的并发控制缺失 — ✅ 已完成 (2026-03-29)

`ICRMProxy.thread_local()` 构造时接收 `Scheduler` + `access_map`，`call_direct()` 内部经过 `scheduler._execution_guard(access)` 保护。线程优惠保留零序列化优势，增加 RW 锁开销（EXCLUSIVE 模式 ~100ns/call）。

### 1.2 @cc.shutdown 装饰器 — ✅ 已完成 (2026-03-29)

声明式 CRM 生命周期回调。`@cc.shutdown` 标记的方法在 `cc.unregister()` / `cc.shutdown()` / atexit 时自动调用。不进入 dispatch_table（不可被 RPC 调用）。

---

## 2. P1 — 性能与可靠性增强

### 2.1 SHM Pool 动态扩容（Segment Chain）— ✅ 已完成 (2026-03-31)

**原始问题**: Pool 满时退化到 per-request SHM（5 个系统调用/request），大 payload 场景下性能显著下降。

**实现方案**: Handshake v6 引入池前缀交换 → 确定性 segment 命名 → client 自动扩段 → server 懒加载。

**完成内容**:
- ✅ Handshake v6：双向交换 `prefix` 字段，用于派生 segment 名称 `{prefix}_b{seg_idx:04x}`
- ✅ Rust `c2-mem` 支持多 segment（`max_segments` 可配，含三层 fallback：现有 → 扩容 → 专用）
- ✅ Rust FFI 暴露 `prefix()` + `derive_segment_name()` 给 Python
- ✅ Client 动态扩容：`_try_buddy_alloc()` 检测 pool 自动创建的新 segment，缓存 `seg_views`
- ✅ Server 懒加载：`lazy_open_peer_seg(conn, seg_idx)` 按需打开 client 扩容的 segment
- ✅ `CTRL_BUDDY_ANNOUNCE` 协议编解码（`ipc/shm_frame.py`）— 预留但未在动态扩容中使用（改用确定性命名方案）
- ✅ 集成测试：`tests/integration/test_dynamic_pool.py`（小 pool 触发扩容 + 验证跨 segment 分配）

**设计文档**: `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md`

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

### 2.3 CI/CD 发布集成 — 已完成 ✓

**完成内容** (2025-07-26):
- `.github/workflows/ci.yml` — PR 测试 (Python 3.12 + 3.14t, ubuntu-latest)
- `.github/workflows/release.yml` — 版本门控发布 (4 平台 × 6 解释器 wheel + sdist)
- `.github/scripts/check_version.py` — pyproject.toml vs PyPI 版本比对 (6 单元测试)
- PyPI Trusted Publishing (OIDC) — 零 secret 认证
- 平台: Linux x86_64/aarch64 + macOS x86_64(交叉编译)/aarch64

**增补** (2026-03-30):
- `skip-existing: true` — 部分上传后重跑不再全量 400
- `verbose: true` — PyPI 拒绝时显示完整错误

### 2.4 边界鲁棒性增强（v0.3.0 预发布加固）— ✅ 已完成 (2026-03-30)

v0.3.0 发布前对传输层和 Relay 进行的系统性代码审查与加固。覆盖三类问题：功能缺陷修复、信号协议完善、测试隔离。

#### 2.4.1 优雅断连协议

**问题**: 客户端关闭后，服务器只能等待心跳超时（30s）才能发现连接断开。

**方案**: 新增 DISCONNECT (0x08) / DISCONNECT_ACK (0x09) 信号握手：
1. `SharedClient.terminate()` 发送 `DISCONNECT` 信号帧
2. Server `_handle_client` 收到后回复 `DISCONNECT_ACK`，立即清理连接
3. Rust `IpcClient.close()` 同样发送 DISCONNECT，`recv_loop` 匹配 DISCONNECT_ACK 后 clean break
4. 100ms 宽限期：即使 ACK 未到达，客户端也继续关闭流程

**改动范围**: `client/core.py` + `server/core.py` + `ipc/msg_type.py` + `c2-ipc/client.rs`

#### 2.4.2 Relay 空闲连接回收

**问题**: Relay 的 IPC 长连接（到上游 CRM）一旦建立就永不释放，CRM 进程重启后旧连接变成僵尸。

**方案**: 周期性 sweeper + 懒重连：
- **Python Relay**: `UpstreamPool._sweep_idle()` — asyncio task，可配置 `idle_timeout`（`--idle-timeout` CLI 参数）
- **Rust Relay**: `spawn_idle_sweeper()` — tokio task，相同逻辑
- Sweeper 同时检查**时间空闲**和**连接死亡**（`_closed` / `!is_connected()`）
- `idle_timeout=0` 时仍以 30s 间隔运行，仅做死连接清理
- 驱逐后下次 `get()` 懒重连

**改动范围**: `relay/core.py` + `c2-relay/state.rs` + `c2-relay/server.rs` + `cli.py`

#### 2.4.3 Relay 死连接检测三重保障

**问题**: 上游 CRM 进程意外终止后，Relay 首次请求必然 502（`get()` 返回死客户端）。

**修复**:
1. **`get()` 即时检测**: Python `get()` 检查 `_closed`、Rust `get()` 检查 `is_connected()` — 死客户端不返回，触发即时重连
2. **调用失败即驱逐**: Rust router 传输错误分支立即从 pool 驱逐死客户端（不等 sweeper）
3. **周期性 sweeper 兜底**: 上述两条机制之外，sweeper 仍周期扫描作为最终兜底

**改动范围**: `relay/core.py:get()` + `c2-relay/state.rs:get()` + `c2-relay/router.rs`

#### 2.4.4 信号协议清理

- ✅ 移除 `MsgType.SHUTDOWN_SERVER` (0x06) — 定义但从未发送/处理的死代码，枚举槽位保留为注释
- ✅ Rust `recv_loop` 显式匹配 `DISCONNECT_ACK` — 之前 `#[allow(dead_code)]`，现在 match 后 break
- ✅ 信号处理重构为 `match` 分支结构（替代 if/continue 链），覆盖 PING/DISCONNECT_ACK/unknown

#### 2.4.5 测试加固

| 类别 | 数量 | 覆盖内容 |
|------|------|----------|
| `test_upstream_pool.py` | 22 | 基本生命周期、空闲驱逐、死连接检测、懒重连、关闭流程 |
| `test_cli_relay.py` | 2 | `--idle-timeout` 参数解析 |
| Rust `state.rs` | 2 | 死连接驱逐的 `idle_entries()` 逻辑 |
| 测试环境隔离 | — | `C2_ENV_FILE=''` 禁止 `.env` 污染测试；conftest 在 import 前设置 |

**架构审查文档**: `docs/architecture-review-v030.md` — 记录大文件审计、信号协议一致性、并发模式、延迟改进项

---

## 3. P2 — 功能完善

### 3.0 磁盘溢写兜底（Plan C）

**背景**: 分块流式传输解决了大 payload 突破 256 MB 限制的问题，但当系统物理内存不足以容纳 reassembly buffer（接收端使用 `mmap.mmap(-1, total_size)` 匿名映射）时，仍需要磁盘级兜底策略。

**方案**: 当 mmap 分配失败或检测到系统可用内存低于阈值时，`_ChunkAssembler` / `_ReplyChunkAssembler` 切换到文件支持的 mmap（`mmap.mmap(fd, size)`），将 reassembly buffer 溢写到临时文件。具体设计参见 spike 文档 `docs/spikes/performance-shm-streaming-large-payload-spike.md` 中的 Plan C。

**触发条件**: 可通过 `IPCConfig.disk_spill_threshold` 配置（默认 None = 不启用），当 `total_payload > threshold` 或 `mmap(-1, ...)` 抛出 `OSError` 时自动降级。

**改动范围**: `server/chunk.py:ChunkAssembler` + `client/core.py:_ReplyChunkAssembler`（增加文件 mmap 分支） + `IPCConfig`（新增配置项）

**前置条件**: 动态扩容已完成 ✅，磁盘溢写是 T4 层的最后一环。Rust `c2-mem` 的三层架构（alloc → segment → pool）已为 T4 扩展预留接口。

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

仓库重构 ✅
├─ rpc/ + rpc_v2/ → transport/  ✅ 消除双模块 (2026-03-29)
├─ Wire v1 移除                 ✅ 仅保留统一编码 (2026-03-29)
├─ 版本标签归一                 ✅ Handshake/ipc:// 去版本化 (2026-03-29)
├─ Server core 解耦             ✅ 7 模块 (2026-03-29)
├─ 配置暴露                     ✅ IPCConfig 11 项新字段 (2026-03-29)
├─ Rust 模块重构                ✅ c2-buddy → c2-mem (alloc/segment/pool) (2026-03-31)
├─ Python API 重命名            ✅ c_two.buddy → c_two.mem, BuddyPoolHandle → MemPool (2026-03-31)
└─ SHM 帧模块重命名             ✅ ipc/buddy.py → ipc/shm_frame.py (2026-03-31)

P1 — 性能与可靠性增强 ✅ 全部完成
├─ 2.1 SHM Pool 动态扩容        ✅ 已完成 (2026-03-31)
│   ├─ Handshake v6 前缀交换    ✅
│   ├─ Client 自动扩段          ✅
│   ├─ Server 懒加载            ✅
│   └─ 集成测试                 ✅ test_dynamic_pool.py
├─ 2.2 心跳 + PID 存活检测      ✅ 已完成 (2026-03-29)
├─ 2.3 CI/CD 发布集成           ✅ 已完成 (2025-07-26, 增补 2026-03-30)
└─ 2.4 边界鲁棒性增强           ✅ 已完成 (2026-03-30)
    ├─ 优雅断连协议              ✅ DISCONNECT/DISCONNECT_ACK
    ├─ Relay 空闲连接回收        ✅ sweeper (Python + Rust)
    ├─ Relay 死连接三重检测      ✅ get() + 调用失败驱逐 + sweeper
    ├─ 信号协议清理              ✅ FF-1 DISCONNECT_ACK / FF-2 SHUTDOWN_SERVER
    ├─ 测试隔离                  ✅ C2_ENV_FILE + 26 新单元测试
    └─ CLI 增强                  ✅ banner + --idle-timeout + dev 保护

P2 — 功能完善（v0.4.0 里程碑）
├─ 3.0 磁盘溢写兜底 (Plan C)    待实现
├─ 3.1 版本兼容性检查           待实现
├─ 3.2 异步接口                 待实现
└─ 3.3 自适应分代 GC            待实现

P3 — 长期演进（v1.0 方向）
├─ 4.1 跨语言 CRM Client        待实现
├─ 4.2 Streaming RPC            待实现
└─ 4.3 服务发现                 待实现
```

---

## 6. 与优化报告的关联

本次 relay QPS 优化（`doc/log/relay-qps-optimization.md`）的成果和遗留问题对后续计划的影响：

| 优化成果 | 对后续计划的影响 |
|---|---|
| `_FastDispatcher` + `SimpleQueue` 替代 `ThreadPoolExecutor` | §1.1 ✅ 线程优惠并发控制已复用 `execute_fast` 路径 |
| Per-connection write barrier | §3.2 异步接口需要兼容 barrier 机制 |
| dispatch_cache + `_slots_generation` | §3.1 版本兼容性检查可复用 generation 失效机制 |
| Rust relay 35K+ QPS | §2.3 ✅ 已集成，作为生产级 HTTP 路由层 |
| flight_inc/dec/wait_idle | §2.2 ✅ 心跳检测已复用 flight counter 判断连接活跃度 |
| Chunked Streaming Transfer | ✅ 消除 256 MB 单次 RPC 限制 |
| DISCONNECT / DISCONNECT_ACK | §2.4 ✅ 优雅断连替代心跳超时被动发现 |
| Relay idle sweeper + lazy reconnect | §2.4 ✅ IPC 长连接不再永久驻留，CRM 重启后自动恢复 |
| SHM Pool 动态扩容 | §2.1 ✅ Handshake v6 前缀 + 确定性命名 + Client/Server 双端实现 |
| Rust c2-mem 三层架构 | 为 §3.0 磁盘溢写预留 T4 扩展点 |

### 优化报告中建议但未覆盖的后续方向

| 优化报告建议 | 对应本计划章节 |
|---|---|
| uvloop 支持 | 不列入计划——Python 3.14t (free-threaded) 兼容性未知 |
| Rust 侧批量写入 | §4.2 Streaming RPC 的前置技术 |
| 大 payload 测试 | §2.1 ✅ 已通过 `test_dynamic_pool.py` 覆盖 |
| py-spy profiling | 作为辅助手段，不单独列为计划项 |
| read_parallel 模式优化 | §1.1 ✅ 修复线程优惠并发控制后自然获得 |

---

## 7. Rust 原生模块架构

当前 `src/c_two/_native/` workspace 包含 5 个 crate：

```
c2-mem                    共享内存子系统
├─ src/alloc/             纯 buddy 分配算法（BuddyAllocator, bitmap, spinlock）
├─ src/segment/           POSIX SHM 生命周期（ShmRegion: create/open/unlink）
├─ src/pool.rs            统一池 MemPool = BuddySegment + DedicatedSegment
├─ src/buddy_segment.rs   ShmRegion + BuddyAllocator 组合
├─ src/dedicated.rs       超大分配专用 segment
└─ src/config.rs          PoolConfig, PoolAllocation, PoolStats

c2-wire                   IPC 帧编解码（Handshake v6, 信号帧）
c2-ipc                    Rust async IPC client (tokio, UDS + SHM)
c2-relay                  HTTP relay server (axum → IPC 转发)
c2-ffi                    PyO3 统一入口 → Python c_two._native
```

Python 侧对应关系：

| Rust 层 | Python 模块 | 主要类型 |
|---------|------------|---------|
| `c2-mem` → `c2-ffi:mem_ffi.rs` | `c_two.mem` | `MemPool`, `PoolConfig`, `PoolAlloc`, `PoolStats` |
| `c2-relay` → `c2-ffi:relay_ffi.rs` | `c_two.relay` | `NativeRelay` |
| `c2-mem::alloc` | (内部使用) | `BuddyAllocator`, `ShmSpinlock` |
| `c2-mem::segment` | (内部使用) | `ShmRegion` |

**演进方向**: `c2-mem` 的三层内部结构（alloc → segment → pool）为未来 T4 磁盘溢写层预留了接口——只需在 pool 层新增一种 segment 类型（如 `FileSegment`），无需修改 alloc 或 segment 层。
