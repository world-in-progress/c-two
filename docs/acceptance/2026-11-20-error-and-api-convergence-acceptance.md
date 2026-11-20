# 错误类型与残留接口收敛验收记录

这份记录验收[错误类型与残留接口收敛执行计划](../design/2026-11-19-error-and-api-convergence-plan.md)，覆盖原[待办](../design/backlog.md)第 3 项（错误体系收敛与 `R32`/`R33`）及待维护者决定 `G8`（`c_two.config` 退役）与 `R21`（`generate_crm_template` 退役）。特性分支 `socu/error-convergence` 从上游 `main` 的 `7b642fcd` 切出，最终以确切候选 `cc7d20a3fd883aba0e0f5d618703faf9e8711190` 经 [PR #230](https://github.com/world-in-progress/c-two/pull/230) 普通合并入 `main`（合并提交 `07c56c0f`），同步顺手根治了在四平台门禁中暴露的一处 `main` 既有 `c2-http` 流弃读窗口释放次序竞态。

## 落地内容摘要

1. **跨语言错误单一权威与阶段归一：**
   - `c2-error::ErrorCode` 单一宏派生 `ErrorSide`（`Client`、`Resource`）与 `ErrorStage`（`Serialize`、`Deserialize`、`FromBuffer`、`Execute`、`Call`），保留 `C2E1` v1 线格式与既有错误码数值（永久封存 `704`、`706`、`711`）。
   - `c2-server` 校正未知方法与请求进入适配器前的解码/存储拒绝为 `713 ProtocolViolation`，一元响应容量拒绝校正为 `717 CallCapacityExceeded`；`c2-http` relay 业务派发前 `712 RouteWatchUnavailable` 补齐 `dispatch_phase="pre_dispatch"`。
   - `c2_core::Error::transport_phase()`、`sdk/rust` 顶层导出、`c2-mem-ffi`（C ABI 8）、`c2-codegen` 生成的 25 码 TypeScript transport 及 Python `CCBaseError.transport_phase` 统一外层传输事实优先的只读派发阶段权威。
2. **Python `c_two.error` 收敛与消除进程内 `C2E1` 二次编解码（`R32`/`R33`）：**
   - 删除 `ERROR_Code` 与硬编码字面量，由 `_native.error_registry()` 派生标准 `ErrorCode(IntEnum)`，公开异常面收敛为 19 个类（以 `ResourceExecutionError` 与 `SerializationError` 取代原 8 个 `一侧 × 阶段` 笛卡尔积类，收回无生产者的 `705`/`712` 子类）。
   - 删除 `CoreError.error_bytes`、`CCError.serialize/deserialize`、`_native.encode_error_wire` / `decode_error_wire_parts`、`CrmCallError`、`_ResourceFailure` 及 `sdk/python/src/c_two/transport/server/reply.py`，改由不可变语义快照与无环原生投影传递异常，并对齐一元、流式、同进程直调与生成 Python 客户端的 `SerializationError` 归因及借用清理隔离。
3. **残留接口整块退役（`G8`、`R21`）：**
   - 删除 `sdk/python/src/c_two/config/`，由 `RuntimeSession` 成为唯一配置权威，`set_relay_anchor(None)` 清除代码覆盖；迁移示例、基准与测试。
   - 删除 `generate_crm_template` 及其导出、模板文件与测试。
   - `G2`（`crm.client` 装配点与 `HeldResult(release_cb=...)`）与 Rust 内部 `CrmError::UserError(Vec<u8>)` 回调表示按计划不并入本批。
4. **`c2-http` 流弃读窗口释放次序根因修复（`cc7d20a3`）：**
   - `PullBody::drop` 严格按 `window.take()` → `requests.take()` → `cancellation.cancel(CancelCause::Consumer)` 顺序释放，既保留 `Consumer` 首因记录，又确保 `drive_stream` 观察到弃读时 `PullBody` 的窗口引用已释放且通道已关闭。
   - `drive_stream` 外层/内层 `select!` 及 `request.send(result)` 失败路径均在等待 `requests` 关闭并释放 `pull` / `pending_data` / `window` 之后再发上游 `Consumer` 取消；`DELETE /_calls/{token}` 在 `!data_open` 进入 `stream.completion().await` 前即释放驱动侧 `window`。

## 确切候选与 CI

四平台门禁在 [PR #230](https://github.com/world-in-progress/c-two/pull/230) 上运行 `Source Validation`（覆盖 Linux x64 的 Python 3.12〔含 Python 3.10 全量普通套件〕与 3.14t、macOS arm64、Windows x64 与 Windows ARM64，以及 `workflow policy`）。

| 轮次 | 候选提交 | Source Validation | 结果与根因归档 |
| --- | --- | --- | --- |
| 第 1 轮 | `eefbc4ff` | [44661579050](https://github.com/world-in-progress/c-two/actions/runs/44661579050) | `macos-15`、`windows-2025`、`windows-11-arm`、`workflow policy` 与 `ubuntu-24.04 / py3.12` 主套全部通过；暴露 2 处测试问题：(1) `ubuntu-24.04 / py3.10` 因 `test_error_convergence.py` 上非平台 `@pytest.mark.skipif(sys.version_info < (3, 11))` 触发 `tools/dev/test_python.py` 拒绝意外 skip（其余 1,602 项全过）；(2) `ubuntu-24.04 / py3.14t` 上新增 `test_unary_upstream_crash_reads_dispatch_uncertain_through_unified_phase[relay_aware_relay]` 在双 relay 拓扑连接前缺少 `lab.wait_route("unary-crash")` 导致偶发抢在 mesh gossip 前收到 `ResourceNotFound`。在 `73490904` 一并修复并补齐 `Peer` 日志刷盘 `entered` 同步与线程池退出清理顺序 |
| 第 2 轮 | `73490904` | [44671926287](https://github.com/world-in-progress/c-two/actions/runs/44671926287) | `workflow policy`、`ubuntu-24.04 (py3.14t)`、`macos-15`、`windows-2025`、`windows-11-arm` 全部通过，`ubuntu-24.04 (py3.12)` 下的 Python（3.10 与 3.12）、Rust SDK、Node、CLI 与内存矩阵门禁也全部通过；仅 `ubuntu-24.04 (py3.12)` 的 `core-test` 命中 1 处 `main` 既有竞态 `c2-http::relay::stream_tests::abandoned_data_header_refunds_its_payload_before_blocked_cleanup`（`used_bytes` 期望 `0` 实得 `16368`）。在干净 `main`（`7b642fcd`）隔离实验中 100% 复现 `PullBody::drop` 先于 `_window` / `requests` 释放触发取消的调度窗口，并在 `cc7d20a3` 根治 |
| 确切候选 | `cc7d20a3`（`cc7d20a3fd883aba0e0f5d618703faf9e8711190`） | [44706989477](https://github.com/world-in-progress/c-two/actions/runs/44706989477) | **9/9 项作业一次全部通过**（`changes`、`workflow policy`、`source (ubuntu-24.04, py3.12)`〔含 `python310-suite`〕、`source (ubuntu-24.04, py3.14t)`、`source (macos-15, py3.12)`、`source (windows-2025, py3.12)`、`source (windows-11-arm, py3.12)`、`test (py3.12)`、`test (py3.14t)`） |

## 本机复核与针对性验证

1. **本机全栈门禁（macOS arm64）：**
   - `tools/dev/test_native.py`（23 项）、`c2-codegen`（33 项，含 golden 与 TypeScript transport 契约）、`sdk/rust --all-features`（76 项）、4 份 `Cargo.lock`（`core`、`sdk/rust`、`sdk/python/native`、`cli`）`--frozen`、`@c-two/c2-mem-ffi`（123 项 + `pack:check`）、Python 3.13 核心与错误收敛套件（1,212 项）、Python 3.10.17 `py_compile` 与运行检查、跨语言可移植载荷 18 行直接/中继矩阵（25 项）以及 `tests/repo/` 全部通过。
2. **`c2-http` 流弃读根因隔离复现与回归验证：**
   - 在基于干净 `main`（`7b642fcd`）的独立临时 worktree 中，于 `PullBody::drop` 触发 `cancellation.cancel(Consumer)` 后注入 20ms 抢占延迟，单次即复现与 Run `44671926287` 完全一致的 `stream_tests.rs:1610:5`（`left: 16368 / right: 0`）失败。
   - 合入 `cc7d20a3` 修复并补充 `abandoned_mid_poll_refunds_window_before_blocked_cleanup` 回归测试后，`cargo check -p c2-http --all-features --all-targets --frozen` 零警告通过，`c2-http` 526 项单测全过，`cli` 85 项测试全过，两条弃读测试各 200 次并发压测（单次 5 秒硬超时）均为 `0` 超时、`0` 失败。

## 后续边界

- `CrmError::UserError(Vec<u8>)` 内部回调表示仍留待[待办](../design/backlog.md)第 2 项“服务端回调面”与 `ResponsePool` 裁剪一并处理；`G2` 维持待维护者决定。
- 本批与[配置收敛验收](2026-11-18-config-convergence-acceptance.md)均已合入主线，发版前仅剩[待办](../design/backlog.md)第 7 项列出的 `C-Two 0.11.0` / `c3 0.7.0` 发行说明整合与发版流程。
