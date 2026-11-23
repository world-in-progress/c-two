# 派发后调用结局查询与去重验收记录

这份记录验收 [ADR 0006](../decisions/0006-post-dispatch-outcome-and-dedup.md)（调研背景见[一手调研](../research/2026-11-23-post-dispatch-outcome-and-dedup-survey.md)），并结清[待办](../design/backlog.md)中“派发后的结果查询与去重”条目。

特性分支 `feat/post-dispatch-outcome-and-dedup` 从上游 `main` 的 `261c27a9` 切出，按 0.x 干净切断原则完成全栈实施（不保留 IPC 17 或 C/FFI ABI 9 兼容垫片）：

- **规范错误码与回调边界隔离（`c2-error`、`c2-core`、Rust/Python SDK）：** 新增 `721 CallIdConflict`、`722 CallResultUnavailable`、`723 CallOutcomeUnknown`；嵌套调用中下游原样抛出的 `721` / `722` / `723` 在回调边界统一包装为 `ResourceExecutionError`（码 3，原码与详情归入 `cause`），确保顶层 `721`–`723` 仅由当前 Host 结局表独占铸造。
- **Host 单锁结局表、协议 IPC 18 与受压淘汰（`c2-config`、`c2-wire`、`c2-mem`、`c2-ipc`、`c2-server`）：** 在 `HostOptions` 中引入全注册共享的 `outcome_max_entries`（默认 `4096`）与 `outcome_max_retained_bytes`（默认按 `min(64 MiB, server.M / 4)` 推导，允许显式设 `0`）及环境变量 `C2_OUTCOME_MAX_ENTRIES`、`C2_OUTCOME_MAX_RETAINED_BYTES`；落地 `HostOutcomeStore`（`settled_fifo` / `retained_fifo`）、`CallMode::KeyedUnary = 3` / `CallMode::Recover = 4` 与非阻塞控制记录对 `QueryCall = 11` / `QueryCallResult = 12`、唯一 `enter_resource()` CAS 资源入口裁决、`OutcomeExecutionGuard` 覆盖单路由注销与 Host 停机排空、锁外选择性 `shed_for_pressure` 受压淘汰与单次重试，以及共享 `Arc<SealedShmBlock>` 的逐连接独立 `loan_id` `CALL_END` 交付（遵循 [ADR 0001](../decisions/0001-single-owner-release.md)）。
- **`c3 relay` HTTP 路由与 Core 运行时（`c2-http`、`c2-core`）：** 新增固定三段路径 `POST /_c2/calls/keyed`、`POST /_c2/calls/recover` 与 `GET /_c2/calls/outcome`（查询参数传 `route` / `method`，单值头 `x-c2-call-id`），彻底隔离契约允许的 `.`、`..`、`/`、`_resolve`、`_probe` 与非 ASCII UTF-8 路由名；三层（客户端、Host、relay）均在 `Q_ctrl = clamp(N, 1, 64)` 有界控制并发下绕过业务槽 `N` 执行只读查询。
- **跨语言 SDK、C/FFI ABI 10、代码生成与文档（`sdk/python`、`sdk/rust`、`c2-mem-ffi`、`c2-codegen`、`cli`、`docs/`、`AGENTS.md`）：** Python 提供 `cc.with_call_id`、`cc.recover_call`、`cc.query_call`（同进程带 `call_id` 调用统一走本机 IPC 编码路径）；Rust 提供 `UnaryCallOptions` 与 `call_unary_with_options` / `recover_unary_call` / `query_call`；Node / TypeScript 与 `c2-mem-ffi` 切至 C/FFI ABI 10 并同步生成器输出与全部参考文档及未发布变更说明。

## 确切候选与全平台 CI

PR [#240](https://github.com/world-in-progress/c-two/pull/240) 包含 `0f9da099` 至 `99514e73` 共 8 个提交，在四平台 `Source Validation` 工作流上**一次全部通过（9/9 项全绿）**，随后以普通合并（`--merge`）合入 `main`。

| 项目 | 记录 |
| --- | --- |
| PR | [#240](https://github.com/world-in-progress/c-two/pull/240) |
| 确切验证源提交 | `99514e73`（`99514e73bb9343665ad8ffad26aa4b10840c82cd`） |
| `main` 合并提交 | `0feee11d`（`0feee11d6cae36000eb888d65bee37e8b3814300`） |
| `Source Validation` 流水线 | [45156909943](https://github.com/world-in-progress/c-two/actions/runs/45156909943)（9/9 项一次通过） |

`Source Validation` [45156909943](https://github.com/world-in-progress/c-two/actions/runs/45156909943) 通过的全部作业：

- `changes`、`workflow policy`
- `source (ubuntu-24.04, py3.12)`、`source (ubuntu-24.04, py3.14t)`
- `source (macos-15, py3.12)`
- `source (windows-2025, py3.12)`、`source (windows-11-arm, py3.12)`
- `test (py3.12)`、`test (py3.14t)`

## 独立核对与验证边界

在提交 `99514e73` 合入前，已在 macOS arm64 上完成分批与全量独立核对，覆盖以下核心测试与关键不变量：

1. **Rust 工作区、Rust SDK 与 `c3` CLI：** `core/` 全工作区（含 `c2-ipc` 216 项、`c2-http --features relay` 554 项、`c2-core` 367 项以及 `c2-error` / `c2-config` / `c2-wire` / `c2-mem` / `c2-server` / `c2-codegen` / `c2-mem-ffi`）、`sdk/rust --all-features`（88 项）与 `cli`（85 项）全部通过，零失败。
2. **Node / TypeScript（C/FFI ABI 10）：** `core/foundation/c2-mem-ffi/bindings/typescript` 下 `npm test`（132 项，覆盖 ABI 10 带键调用、有限截止时间 `recover_call`、非阻塞 `query_call`、流拒收 `call_id` 与 `endpoint.close()` 排空）、`npm run typecheck`、`npm run pack:check` 及 `tests/repo/test_c2_mem_ffi_symbols.py` 全部通过。
3. **Python SDK、跨语言矩阵与仓库检查：**
   - `tools/dev/test_native.py`（37 项）；
   - `sdk/python/tests/` 全量单测与集成测试（1,595 项，含源码静态链接 FastDB 下受 macOS 沙箱约束的原生绑定拒绝测试）；
   - 18 行 Rust/Python 编译态便携载荷直连与 relay 矩阵（`test_portable_payload_cross_language.py` 与 `test_portable_payload_matrix.py`，25 项全部通过）；
   - Python 3.10 最低支持语法检查（1 项，实际执行未跳过）；
   - 仓库门禁（`test_portable_matrix_receipt.py`、`test_docs_hygiene.py`、`test_c2_mem_ffi_symbols.py`，共 301 项）。
4. **重点核验的不变量与边界场景：**
   - **唯一入口裁决与代际撤销：** 原连接断开与 `enter_resource()` CAS 竞态下绝不双执行，也不滞留幽灵等待者；精确代际撤销后 `recover_call` 返回 `723`、重复提交返回 `pre_dispatch`。
   - **零保留、容量淘汰与在途附着交付：** `outcome_max_retained_bytes = 0` 及第 4097 条终态淘汰时，已在 `InFlight` 附着的等待者通过一次性通道完整收到结果，后续迟到 `recover_call` 按实际原因返回 `722`（`not_retained` / `evicted`）或 `723`（`unknown`）。
   - **满载下非阻塞查证：** `N_client = N_server = N_relay = 1` 且长写调用占满业务槽期间，`query_call` 在直连与经 relay 路径下均无阻塞返回 `in_flight`，且 writer 公平调度不饿死业务 `CALL_END`。
   - **回调边界隔离与路由名免疫：** 嵌套资源调用抛出的 `721` / `722` / `723` 被包装为码 3；`.`、`..`、`_resolve`、`_probe`、`a/b` 与非 ASCII UTF-8 路由名经 relay `/_c2/calls/*` 路由无路径归一冲突，符合 [ADR 0004](../decisions/0004-route-name-identity.md)。
   - **受压无自锁淘汰与多借出交付隔离：** `shed_for_pressure` 在锁外丢弃淘汰载体并单次重试，无池/预算重入死锁；共享 `Arc<SealedShmBlock>` 向多连接交付时单连接写入失败或断开不影响已发布结局与其他接收方。
   - **流与主题拒收边界：** `@cc.stream` 与 `@cc.topic` 在 Rust/TypeScript 类型系统、Python 包装入口、C ABI 10 与 IPC 18 解码边界一律拒收 `call_id`。

**验证边界说明：** 本机未放置相邻 `fastdb4ts` 检出，因此依赖外部 `fastdb4ts` 的 TypeScript 持久客户端与流式交叉夹具未在本机执行；该部分及 Linux x64（Python 3.12 / 3.14t）、macOS arm64、Windows x64 与 Windows ARM64 全量跨语言门禁均已在上述 `Source Validation` 流水线 [45156909943](https://github.com/world-in-progress/c-two/actions/runs/45156909943) 中全绿通过。
