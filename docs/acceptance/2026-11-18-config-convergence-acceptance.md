# 配置收敛验收记录

这份记录验收[配置收敛与角色预算域执行计划](../design/2026-11-16-config-convergence-plan.md)，并结清[待办](../design/backlog.md)中的“配置收敛的集成验收”条目。特性分支 `socu/config-convergence` 从上游 `main` 的 `c7fa356d` 切出，按计划分四块实施并统一收敛：删除 27 个旧环境变量、去前缀整合 4 个环境变量（现存 13 个公开环境变量），在 Core 中落地按需懒创建的独立 `client`、`server` 与 `relay` 角色预算域 `(M, F, N)`、单 `Host` 状态槽、段几何与服务端阻塞线程池推导公式、对称只读观测快照 `memory_stats()` / `call_execution_snapshot()`，以及 Node C/FFI ABI 8 客户端域与无副作用 `stats()` 接口，不保留任何旧名字别名或双读兼容垫片。

## 确切候选与 CI

全平台 CI 在 fork 上运行 `Source Validation`（覆盖 Linux x64 的 Python 3.12〔含 Python 3.10 最低支持语法检查〕与 3.14t、macOS arm64、Windows x64 与 Windows ARM64）及 `IPC Memory Paired Benchmark`（覆盖 Linux x64、macOS arm64、Windows x64 与 Windows ARM64）。

| 轮次 | 候选提交 | Source Validation | IPC Memory Paired Benchmark | 结果与根因归档 |
| --- | --- | --- | --- | --- |
| 第 1 轮 | `c1533151` | [44323008313](https://github.com/Dsssyc/c-two/actions/runs/44323008313) | [44323016181](https://github.com/Dsssyc/c-two/actions/runs/44323016181) | 配对基准在 Linux、Windows x64 与 Windows ARM64 三端通过，`macos-15` 因托管队列未分配到机器而未启动。`Source Validation` 暴露出 5 项具体问题：(1) Windows 双架构上 `c2-ipc` 的 `server_executor_teardown_reports_peer_end_as_dispatch_uncertain` 5 秒超时——`mio` 在 Windows 下为每次挂起异步读克隆并 `mem::forget` 一份持有管道 `HANDLE` 的 `Arc<Inner>`，此前 `Server` 结构体强持有 `copy_coordinator` 的 `JoinHandle`，在测试拆除服务端执行器但栈上保留 `Arc<Server>` 时钉住了已取消运行时的任务槽与 IOCP 状态，阻碍管道句柄关闭；(2) `macos-15` 上 `test_persistent_client_lifecycle.py` 清理阶段少观测一条 `closed` 事件——`Host` 停机返回后空闲入站连接异步退役，单次非阻塞 `drain()` 存在时间窗；(3) `test_nested_error_phase.py` 读取的夹具公开字段缺失且独立宿主二进制在 `-D warnings` 下存在未用测试辅助成员；(4) `test_benchmark_relay_helpers.py` 仍引用旧常量名 `_SEGMENT_SIZE`；(5) Linux 下 `c2-local` 非 macOS 编译分支留有无调用方的 `set_thread_directory` 桩。另有 `windows-11-arm` 上 `route_identity_mesh` 单次停机屏障超时，记入下文遗留核实项 |
| 第 2 轮 | `e54e6115` | [44433471538](https://github.com/Dsssyc/c-two/actions/runs/44433471538) | [44433476819](https://github.com/Dsssyc/c-two/actions/runs/44433476819) | 配对基准在全部四个平台上一次通过。`Source Validation` 在 Linux（3.12 与 3.14t）、macOS arm64、Windows ARM64 四条通道全部通过；仅 `windows-2025` 的 `core-test` 中单测 `c2-ipc::control::tests::shutdown_ack_only_proves_initiation_and_retries_before_sending` 失败（返回 `Unconfirmed` 而非 `Initiated`，其余 28 项门禁全过）。根因：生产 `c2-server` 写出控制 ACK 后会等待单次交换客户端读完关闭，而 `c2-ipc/src/control.rs` 测试夹具中的模拟服务端 `responder` 与 `shutdown_reply` 在写完后立即释放了命名管道句柄，且调用了带 Windows 零字节 flush 探针的 `LocalStream::write_all`，双核竞争调度下或提前丢弃未读缓冲字节、或与快速关闭的客户端发生 flush 竞态。`0b3db66c` 与 `6e8c77a7` 只改两处 `#[cfg(test)]` 模块修好该测试夹具并补齐一处 `#[cfg(unix)]` 导入条件 |
| 确切候选 | `6e8c77a7`（`6e8c77a7d3e483b99defd45b7d7cfa1bf7ecee8a`） | [44448056332](https://github.com/Dsssyc/c-two/actions/runs/44448056332) | [44448059935](https://github.com/Dsssyc/c-two/actions/runs/44448059935) | 两条流水线第一次尝试均全部通过（`Source Validation` 9/9 项全绿，`IPC Memory Paired Benchmark` 5/5 项全绿） |

## 独立核对与针对性验证

1. **服务端分片复制协调器作用域状态机（`c2-server`）：** 移除 `Server` 上的 `JoinHandle`，在 `CopyScheduler` 内以单锁维护 `active_scopes` 与 `coordinator_running`，由 `Server::run_inner` 与每个首次轮询的 `handle_connection` 共同持有 `CopyCoordinatorScope`，并在 `tokio::spawn` 前同步构造 `CoordinatorRunningGuard` 移入协调器 future：
   - 单独取消 `Server::run()` 时，仍在工作的连接继续保活协调器直至已接受的分片请求完成；
   - 正常归零退出时在同一把锁内清空队列、复位运行标志并解除守卫职责，不泄漏 `Arc<CopyScheduler>`；
   - 任务在首次轮询前即被执行器丢弃时，守卫析构同步复位 `coordinator_running`，后续作用域可在同一 `Arc<Server>` 上正常重启协调器；
   - 整体拆除服务端执行器且栈上保留 `Arc<Server>` 时，Windows 11 x64（Rust 1.99.1）实机连续运行 `server_executor_teardown_reports_peer_end_as_dispatch_uncertain` 10/10 次均在 `0.01s` 返回 `DispatchUncertain`（`server_streams` 27/27 通过，`c2-server` 新增 4 项确定性回归单测及全量单测全部通过）。
2. **Windows 命名管道控制夹具竞态复现与压测（`c2-ipc`）：** 在 Windows 11 x64（Rust 1.99.1）双核 CPU 亲和度加 4 个自旋负载进程的竞争条件下，先对 `e54e6115` 基线隔离复现：`shutdown_ack_only_proves_initiation_and_retries_before_sending` 连续 100 次出现 31 次 `Unconfirmed` 失败，全套 `control::tests` 50 轮出现 35 轮失败（同时复现 `shutdown_reply` 零字节 flush 探针的对端已关闭 panic）。合入 `6e8c77a7` 后，同一双核高负载条件下 `RUSTFLAGS="-D warnings" cargo check -p c2-ipc --all-targets` 零警告退出 `0`，`control::tests` 连续 **50/50 轮全部通过（`0` 失败）**。
3. **本机全量与边界核对（macOS arm64）：**
   - 空 `HOME`、空白环境变量与临时私有端点根下核验角色域懒激活（纯构造与 `memory_stats()` / `call_execution_snapshot()` 只读观测不读取环境、不建池、不冻结）、`M = 0` 新调用闭锁但保留释放与关闭排空、同进程 Python `thread_local` 直调计入目标 `Host` 的 `N` 并剥离内层 `pre_dispatch` 标记；
   - 开启 `RUSTFLAGS="-D warnings"` 跑通 `core` 工作区、`sdk/rust` 全特性目标、`cli`、Python 独立 Rust 嵌套错误宿主二进制及 `tests/repo/` 仓库与工作流卫生检查。

## 配对基准

由于 0.x 干净切断移除了旧版 `set_server` / `set_client` 传输覆盖键及旧 `memory_stats()` 的 `runtime_outgoing` / `payload_buffer` 字段，不为基准脚本保留跨代双读兼容垫片，合并前 `ipc-memory-paired.yml` 将 `baseline_ref` 与 `candidate_ref` 同钉在收敛后的确切候选 `6e8c77a7d3e483b99defd45b7d7cfa1bf7ecee8a`（合并入 `main` 后默认 `baseline_ref: main` 即恢复跨提交配对基线），并由前置 `resolve-refs` 作业把两个输入统一解析为精确 40 位提交 SHA 下发给四台平台机器。

在[配对基准 44448059935](https://github.com/Dsssyc/c-two/actions/runs/44448059935)（以及生产代码字节一致的 `e54e6115` 上的 [44433476819](https://github.com/Dsssyc/c-two/actions/runs/44433476819)）中，Linux x64（`ubuntu-24.04`）、macOS arm64（`macos-15`）、Windows x64（`windows-2025`）与 Windows ARM64（`windows-11-arm`）四个平台的配对收据与流式对比收据全部为 `ok: true`：
- 每个平台完整跑完 **5 组配对 × 9 行内存矩阵**（`idle_small_rpc_{roomy,bounded}_memory`、`burst4_shared_{roomy,bounded}_memory`、`science_payload_pairs_{roomy,bounded}_memory`、`reply_memory_reassembly`、`reply_file_spill`、`reply_budget_exhausted`）与 **22 项异常/压力控制场景**（每平台 `220` 次异常控制运行全部 `ok: true`，`failures: []`，`blocking_failures: []`）；
- 每个平台完整跑完 **5 组 `ipc-stream` 一元与服务端流对比全矩阵**（`full_matrix: true`，`ok: true`）；
- 同提交自配对下，Linux 与两个 Windows 平台的 `1 B` 与大载荷行组间中位比值均落在 `0.98–1.02` 的运行噪声带内，验证了新 `(M, F, N)` 预算域、真实可达文件溢出 `F` 与流式门控在四平台上的稳定性与可重复性。

## 留给后续的

第 1 轮 CI（[44323008313](https://github.com/Dsssyc/c-two/actions/runs/44323008313)）里，`windows-11-arm` 上的 `sdk/rust/tests/route_identity_mesh.rs` 出现过一次 `host_a.shutdown_with_timeout(WAIT)` 超时；在 Windows 11 x64 测试机含 12 核并发负载在内连续复跑 49 轮均为 `0` 失败，且第 2 轮（[44433471538](https://github.com/Dsssyc/c-two/actions/runs/44433471538)）与确切候选（[44448056332](https://github.com/Dsssyc/c-two/actions/runs/44448056332)）在 `windows-11-arm` 上均一次通过。因机制尚未复现，未对生产停机路径做猜测性改动；已在 `route_identity_mesh.rs` 补充 `assert_host_shutdown` 诊断输出（断言失败时打印 `host.lifecycle_snapshot()` 与 `c3.stderr.log`），并记入[待办](../design/backlog.md)“要核实的事”。
