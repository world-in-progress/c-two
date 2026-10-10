# C-Two 传输内存策略用户指南

[English](memory-policy.en.md) · 简体中文

本文说明 IPC 内存预算、惰性分配与层级回退。Rust resolver 统一提供默认值及校验，Python 提供类型化覆盖门面。

## 1. 三个有限预算单元

C-Two 对自有 IPC 后备与在途组装维护三个有限的字节预算。三个覆盖键在服务端与客户端的 IPC 覆盖中均可使用：

| 覆盖键 | 原生默认 | 计费范围 |
| --- | --- | --- |
| `shm_backing_budget_bytes` | 8 GiB | 本方向创建的 buddy 与 dedicated SHM 映射，含 header/对齐开销 |
| `file_backing_budget_bytes` | 16 GiB | 本方向创建的文件后备长度 |
| `live_reassembly_budget_bytes` | 8 GiB | 未完成及完成后仍被排队、执行或 held 保留的分块重组容量 |

零值语义：`0` 表示该单元拒绝一切正数预留，它不是"无限制"。全部 `u64` 范围都是合法配置。

预算边界：只覆盖 C-Two 自有的 IPC 数据后备与在途组装，不约束 Python/FastDB 堆、HTTP 缓冲、接收端代开的对端映射，也不等于进程 RSS。既有的单消息限制（`max_payload_size`、`max_reassembly_bytes`、`max_total_chunks`、`max_pool_segments` 等）继续独立生效，预算不替代它们。另有独立的调用延续预算（`C2_CALL_MAX_OUTSTANDING`、`C2_CALL_RETAINED_INPUT_BUDGET_BYTES`）：它只约束有限期限调用为保留请求输入而取得的准入名额与字节预留，不属于这三个 IPC 预算单元，也不约束无限调用。

预算方向：服务端方向（响应池、组装池、响应预热）共享一个预算；Runtime 出站客户端域内所有缓存连接的请求池与组装池共享另一个预算。两个方向相互独立，独立的 Runtime 互不共享。创建 backing 前先原子预留预算：预留失败不会产生任何映射或文件；复用已计费段中的空闲块不二次计费；dedicated 段等待对端 `read_done`/GC 期间保持计费。

## 2. 默认惰性分配行为

`pool_prewarm_segments` 默认 `0`：完全惰性。注册或连接时不映射任何 buddy 段，直到第一次需要大缓冲的分配才创建后备。预热是显式动作，设置后按段数立即映射。

`pool_min_retained_segments` 默认 `0`：空闲 buddy 段在衰减窗口（`pool_decay_seconds`，服务端与客户端默认均为 60 秒）后可被回收，允许池退回零映射；段代计数与活跃分配不受影响。直接使用原生 `MemPool` 的默认保留 1 段，SDK 的 IPC 传输投影为 0，以上描述的是 SDK 行为。

创建新的 SHM 后备（buddy 扩展或 dedicated 创建）时还会经过一个 OS 内存压力启发式：候选映射总字节超过观测可用内存的 `spill_threshold`（IPC 投影固定 0.8，非 IPC 覆盖键）即拒绝；有限且 `≥1.0` 的值关闭启发式，`≤0` 或非有限值把所有新映射推向文件后备或显式错误。已映射段内的复用永远允许，不受启发式与预算之外的任何新计费影响。

## 3. 请求与响应的层级回退

发送端（客户端请求）保留以下回退顺序：

1. 未选择请求池 SHM 且负载不大于 `chunk_size` 时走 inline 帧；更大的负载走分块。是否选择 SHM 还取决于 `shm_threshold`（默认 4096 字节）和线格式上限。
2. 负载大于 `shm_threshold` 且不超过 buddy 线格式上限时，走 SHM 指针传输：数据写入本进程请求池，线上携带 15 字节 `BuddyPayload` 位置与 generation 元数据，接收端按前缀/索引/代惰性打开。
3. SHM 不可用（预算拒绝、压力拒绝或超线格式上限）时，回退到检查过的分块传输（`chunk_size` 默认 128 KiB，受 `max_total_chunks`/`max_reassembly_bytes` 约束）；小负载回退 inline。不引入第三种传输栈。

服务端响应对称：不大于 `shm_threshold` 走 inline；否则先尝试 buddy 复用，再尝试 buddy 池扩展，再尝试 dedicated SHM；都不可用或负载无法被 buddy 元数据表示时走分块回退。

接收端组装是完整预留：按 `total_chunks × chunk_size` 一次性预留全部容量，存储层级为 buddy 复用 → GC 后复用 → buddy 扩展 → dedicated SHM → 文件 mmap 最终回退。组装完成时逻辑长度可以裁剪，但容量计费保持到存储真正释放。注意这是"接收全量、组装完成后一次交给资源"，不是流式或增量资源输入。

FastDB 载荷内容对该层不透明：C-Two 只搬运字节或共享内存指针，嵌套规格作为不透明 JSON 委托 FastDB Core，这是保持中立的边界。

## 4. `pool_enabled=False` 只关闭 buddy 层

`pool_enabled=False` 跳过 buddy 的复用与扩展（包括已缓存的段），分配直达 dedicated SHM；dedicated、分块传输、inline、文件后备全部保留。接收端组装同样进入 dedicated + 文件后备路径，不会因此跳过组装。因此它不是"零共享内存"开关：dedicated 路径仍会创建 SHM 映射，仍受 `shm_backing_budget_bytes` 计费。

`shm_threshold` 同样不是禁用 SHM 的开关。它只决定 inline 与 SHM 的选择阈值，且不是角色级 `ipc_overrides` 键（Rust 侧目录将其列为禁止键）；进程级调整请使用 `cc.set_transport_policy(shm_threshold=...)`。调大它只会让更多负载改走 inline 或分块帧，接收端组装池与 dedicated 路径仍可能映射 SHM，预算单元照常生效。

校验约束：`pool_enabled=False` 时 `pool_prewarm_segments` 必须为 0（禁用的 buddy 池必须保持惰性），非法组合在 Rust 解析层被拒绝；`pool_min_retained_segments` 不得超过 `max_pool_segments` 与 `reassembly_max_segments`。

## 5. 池段容量与单消息上限相互独立

`pool_segment_size` 与 `max_payload_size` 是相互独立的维度，对 buddy 开启和关闭两种情况一致：一个较大的池段可以容纳多个较小的消息，池段总容量不必小于每条消息上限。校验约束构造与线格式：尺寸为正、索引可表示、段数有限、乘法不溢出。实际超过 `max_payload_size` 的请求与响应仍会被拒绝，不会因池段足够大而放行。

以下配置合法：限制单条消息 32 MiB、保留默认 256 MiB 池段，并且关闭 buddy 时不创建任何 buddy 后备，dedicated SHM 仍按需可用：

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

分块、组装与各预算单元的既有回退全部保持原样；本项变更不改变层级回退顺序，也不引入流式或增量资源输入。

## 6. 最小配置示例：低负载、禁用 buddy、保持惰性

```python
import c_two as cc

# 必须在 cc.register() 之前调用 set_server，在 cc.connect() 之前调用 set_client。
cc.set_server(ipc_overrides={'pool_enabled': False})
cc.set_client(ipc_overrides={'pool_enabled': False})
```

键与形状核对：`pool_enabled` 是基础/服务端/客户端三级覆盖目录都接受的布尔键。`pool_prewarm_segments` 与 `pool_min_retained_segments` 默认已是 0，无需写出，也绝不能在禁用 buddy 时设置预热。需要进一步收缩时才追加 `pool_segment_size`、`max_pool_segments` 等键。服务端与客户端是两个独立预算域，两侧都要设置。

## 7. 显式有限预算示例：三个规范字段

```python
import c_two as cc

BUDGET = {
    'shm_backing_budget_bytes': 2 * 1024**3,    # 2 GiB
    'file_backing_budget_bytes': 4 * 1024**3,   # 4 GiB
    'live_reassembly_budget_bytes': 1 * 1024**3,  # 1 GiB
}
cc.set_server(ipc_overrides=dict(BUDGET))
cc.set_client(ipc_overrides=dict(BUDGET))
```

原生有限默认为 8 GiB / 16 GiB / 8 GiB；这是对先前聚合上限宽松负载的刻意行为变化，不承诺所有科学计算负载都天然适配默认值。`0` 可用于显式关闭某个单元的正数预留（是拒绝，不是无限）。超限拒绝发生在分配之前，计入该单元的 `rejected_allocations`/`rejected_bytes`，并沿传输层既有错误路径上报；ping、心跳等控制帧不经过数据后备分配，不占用这些单元。

同样的三个键也可通过环境变量 `C2_IPC_SHM_BACKING_BUDGET_BYTES`、`C2_IPC_FILE_BACKING_BUDGET_BYTES`、`C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` 设置，解析优先级为显式代码覆盖 > 进程环境/`.env` > Rust 默认。客户端 Runtime 的配置在首次连接尝试前冻结，包括失败的首次尝试；缓存命中要求完整的已解析策略相等。需要更改配置时，先关闭旧 Runtime，再为新 Runtime 设置配置。旧 held 数据仍由其原预算域计账。

### 独立 relay 进程

`c3 relay` 在启动时通过同一个 Rust 配置解析器解析上游 IPC 策略，固定在 `RelayConfig.upstream_ipc` 中。所有数据面上游连接的请求池和组装池共享该 relay 实例的预算，重连继续使用同一策略与预算。应用进程中的 `cc.set_client()` 不会配置另一个 relay 进程；需要在启动 relay 的进程环境或命令行设置策略。

```bash
c3 relay --bind 127.0.0.1:8080 \
  --ipc-pool-enabled false \
  --ipc-shm-backing-budget-bytes 134217728 \
  --ipc-file-backing-budget-bytes 268435456 \
  --ipc-live-reassembly-budget-bytes 134217728
```

四个命令行覆盖分别对应 `C2_IPC_POOL_ENABLED` 和上述三个预算环境变量，命令行优先于进程环境/`.env`。其他 client IPC 设置（例如 `C2_IPC_POOL_PREWARM_SEGMENTS`、`C2_IPC_POOL_DECAY_SECONDS`、`C2_SHM_THRESHOLD`）也经既有解析器生效；`c3 relay --dry-run` 可查看 buddy、预热和预算的已解析值。禁用 buddy 时预热必须为零；零预算保留“拒绝正数预留”的含义。

此范围只包含 relay 自有的数据面 IPC 后备与组装。注册证明和控制 watch 使用独立的惰性默认上下文，对端映射和 HTTP 缓冲不计入该预算。HTTP 响应仍会全量物化后分片发送；小的发送分片不等于小的响应内存占用。Relay 的转发准入另行约束事务数量与保留输入，见[配置指南](configuration.md#relay-与-c3)。

## 8. 只读统计：`cc.memory_stats()` 与 `cc.hold_stats()`

`cc.memory_stats()` 返回只读、按方向标注的快照：`runtime_outgoing`（本 Runtime 出站客户端域，首次连接尝试时冻结；观察它不会触发冻结或连接）、`server`（本进程服务端方向，无主机时为 `None`）、`retired`（先前会话关闭时仍有持有者的域，`state="retired"`）、`holds`（与 `cc.hold_stats()` 同源的保留租约计数）与 `budget_cells_note`。每个域含 `role`、`state`、`limits`（三个已解析上限）与 `cells`（`shm`/`file`/`reassembly`），每格为 `limit_bytes`、`used_bytes`、`peak_bytes`、`rejected_allocations`、`rejected_bytes`，峰值跨释放保留高水位。

`cc.hold_stats()` 返回 `active_holds`、`total_held_bytes`、`oldest_hold_seconds` 与 `by_storage`；inline、SHM、handle、文件后备都是可保留租约，hold 不是 SHM 专属。

读数纪律：三个单元是独立的计费口径，分别描述后备与组装/保留容量，不要把任何单元格求和当成物理内存占用，也不要把它们当成零拷贝证明——适配层仍可能有拷贝路径。retired 域的呈现方式属于实现中的行为，本文不对其细节作稳定性承诺。

## 9. held 生命周期与 FastDB 边界

`cc.hold(proxy.method)(args)` 返回 `HeldResult`，提供 `.value`、`.unsafe_buffer` 与 `.release()`；安全层为显式 `release()`、`with` 上下文与 `__del__` 兜底。`release()` 的顺序固定：先执行 FastDB 检查视图失效回调，再释放传输租约。对便携方法，`held.value` 是 FastDB `Payload` 持有者，释放持有会使该持有者与其检查视图失效。需要在租约范围之外长期保存的数据，必须先通过 FastDB 官方 API 物化；`unsafe_buffer` 是显式不安全逃生口，无法机械失效用户自行导出的裸指针或 NumPy 别名。

关闭后的持有生命周期：`cc.shutdown()` 不强制释放仍被持有的数据，也不清零计账。其预算计费与保留租约转入 retired 域，继续出现在 `cc.memory_stats()['retired']`，只要旧代理、在途调用或 held 仍持有计数所有者，记录就继续可观察；计费归零与记录删除是两个时刻。最后一个真实所有者消失后，弱观察记录才移除，统计自身不会挽留池、连接、Runtime、回调或 payload。

## 10. 证据指针

- `core/foundation/c2-config/src/memory.rs` — 三字段、有限默认与零值语义；`core/foundation/c2-config/src/ipc.rs` — 已解析默认值、覆盖键目录、`FORBIDDEN_IPC_OVERRIDE_KEYS` 与校验规则。
- `core/foundation/c2-mem/src/budget.rs`、`pool.rs`、`pressure.rs`、`dedicated.rs`、`buddy_segment.rs`、`spill.rs` — 预留原语、层级回退、压力启发式与后备级 guard。
- `core/transport/c2-ipc/src/client.rs` — `choose_request_transport` 与请求回退；`core/transport/c2-server/src/server.rs` — `smart_reply_with_data`、buddy 回复与共享服务端预算、显式预热。
- `core/protocol/c2-wire/src/assembler.rs`、`chunk/registry.rs` — 完整容量组装预留与文件后备保留。
- `core/runtime/c2-core/src/memory.rs` 与 `sdk/python/native/src/runtime_session_ffi.rs` — 作用域/retired 快照投影。
- `sdk/python/src/c_two/config/ipc.py`、`settings.py` — 类型化覆盖模式与进程策略；`sdk/python/src/c_two/mem/__init__.py`、`transport/registry.py` — `memory_stats`/`hold_stats` 门面。
- `sdk/python/src/c_two/crm/transferable.py` — `HeldResult` 释放顺序；`.env.example` — `C2_IPC_POOL_ENABLED`/`PREWARM`/`MIN_RETAINED` 键。
- `docs/reports/memory-budget-contract.md` 与 `docs/plans/2026-09-26-memory-policy.md` — 目标契约与计划背景。
- [内存阶段验收](reports/memory-native-final-validation.md) — 该阶段固定源码与开发产物证据，保留其历史状态。
- [0.7.0 统一验收](reports/canonical-local-endpoint-validation.md) — 该版本唯一端点的确切源码、Windows/Linux 门禁与开发产物哈希，保留为历史记录。
- [0.7.4 发布记录](reports/0.7.4-publication.zh-CN.md) — 当前发布源码、平台门禁与公开产物验证。
