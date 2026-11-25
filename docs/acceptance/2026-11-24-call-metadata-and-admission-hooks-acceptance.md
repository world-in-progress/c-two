# 调用元数据与准入钩子验收记录

这份记录验收 [ADR 0007](../decisions/0007-call-metadata-and-admission-hooks.md)（调研背景见[一手调研](../research/2026-11-23-call-metadata-and-admission-hooks-survey.md)），并结清[待办](../design/backlog.md)中“调用元数据与准入钩子”条目。

特性分支 `feat/call-metadata-and-admission-hooks` 从上游 `main` 的 `22bea02e` 切出，按 0.x 干净切断原则完成全栈实施（并入未发布 IPC 18 与 C/FFI ABI 10；relay 升至 Mesh 8、`application/vnd.c-two.topic-stream;version=2` 与统一 `/_c2/*` 数据面路由；退役 Rust `UnaryCallOptions` 及旧 `POST /{route}/{method}`、`POST /{route}/_topics/*` 路径）：

- **调用元数据、错误码 `724` 与 IPC 18 扩展（`c2-config`、`c2-error`、`c2-wire`）：** 引入有界单值有序映射 `CallMetadata`（最多 64 条、键 `1..=64` 字节小写 ASCII `[a-z0-9_.-]`、值 `0..=4096` 字节可打印 ASCII 且禁止首尾空格、键值总和 `<= 16,384` 字节）；将 `CallOptions` 统一为三端可复用视图选项并退役 `UnaryCallOptions`；新增规范错误码 `724 AdmissionDenied`（`CallStatus::NotExecuted`、HTTP `403`、`pre_dispatch`），扩展 `ErrorCode::is_host_control()` 以在回调边界把嵌套未捕获 `724` 隔离为码 3；IPC 18 增补服务端单向握手位 `CAP_ADMISSION_HOOK`、全 `OPEN` 与 `QueryCall` 的紧凑规范元数据段、16 字节 `READY (start_seq, head_seq)` 及独立非阻塞控制记录对 `AdmitTopic = 13` / `AdmitTopicResult = 14`。
- **Host 准入钩子状态机、只读调用上下文与 IPC 客户端（`c2-server`、`c2-ipc`）：** 新增 `AdmissionHook` / `AdmissionContext` / `AdmissionDecision` / `CallContext` / `current_call_context()`；在 `derive_for_host` 中纳入准入并发配额 `A = clamp(P, 4, 64)`；在 `dispatcher.read()` 内同步预登记 `PendingAdmission` 并采用非淘汰角色内存暂存与每连接业务 FIFO 准入队列，`QueryCall` 与 `AdmitTopic` 走双非阻塞控制配额 `Q_ctrl + A` 且严格遵守 `dispatcher.read() → conn.calls` 锁序；`FlightGuard` 从预登记到执行与排空全程连续传递。
- **Relay Mesh 8、统一 `/_c2/*` 路由、`TopicHub` v2 与 Core 集成（`c2-http`、`c2-core`）：** `PROTOCOL_VERSION` 升至 8，六条数据面端点统一收口至 `/_c2/calls/{call,keyed,recover,outcome}` 与 `/_c2/topics/{subscribe,confirm}`，透传并严格校验 `x-c2-meta-*`（非法报 `400 + 713 stage=call_metadata_validation`），独立 4 KiB 有界控制错误通道不向业务内存 `M` 借配额；`TopicHub` v2 落实创建者私有拒收 `opening_abandoned` + `RetryJoin` 与 `x-c2-topic-upstream-token` 后期确认隔离；`HostOptions::with_admission_hook` 随服务端角色冻结并在停机重启交接中保留。
- **跨语言 SDK、C/FFI ABI 10、代码生成与文档（`sdk/rust`、`sdk/python`、`c2-mem-ffi`、`c2-codegen`、`cli`、`docs/`、`AGENTS.md`）：** Rust、Python（`cc.with_call_options`、`cc.set_server(admission_hook=...)`、`cc.current_call_context()`、同进程 `thread_local` 准入与嵌套上下文遮蔽）与生成的 TypeScript（`ContractClient.withCallOptions`、`x-c2-meta-` 前缀保留、订阅超时优先级）三端一致；C/FFI ABI 10 为 `call_begin` / `recover_call` / `query_call` 增加可空 `metadata_json` 并为 `subscription_open` 增加可选 `metadata`（拒绝重复 JSON 键）；同步全部现行参考文档、`unreleased.md` 与 `AGENTS.md`，并顺带修复 `cc.serve()` 信号处理器与 `threading.Event` 非重入锁的自死锁及 `test_local_stream_owns_the_host_slot` 预取竞态。

## 确切候选与全平台 CI

PR [#243](https://github.com/world-in-progress/c-two/pull/243) 包含 `3e959915` 至 `9a771fc0` 共 8 个提交，在四平台 `Source Validation` 工作流上 **9/9 项全部通过**，随后以普通合并（`--merge`）合入 `main`。

| 项目 | 记录 |
| --- | --- |
| PR | [#243](https://github.com/world-in-progress/c-two/pull/243) |
| 确切验证源提交 | `9a771fc0`（`9a771fc068d98a42069642a1079b2b2c3c5dacb1`） |
| `main` 合并提交 | `d023f51a`（`d023f51a1d551bdcbd0ab289566f662b98cb6d42`） |
| `Source Validation` 流水线 | [45539971394](https://github.com/world-in-progress/c-two/actions/runs/45539971394)（9/9 项全部通过） |

`Source Validation` [45539971394](https://github.com/world-in-progress/c-two/actions/runs/45539971394) 通过的全部作业：

- `changes`、`workflow policy`
- `source (ubuntu-24.04, py3.12)`（含 `python310-suite`）、`source (ubuntu-24.04, py3.14t)`
- `source (macos-15, py3.12)`
- `source (windows-2025, py3.12)`、`source (windows-11-arm, py3.12)`
- `test (py3.12)`、`test (py3.14t)`

## 独立核对与验证边界

在候选提交合入前，已在 macOS arm64 上完成分批与全量独立核对，覆盖以下核心测试与关键不变量：

1. **Rust 工作区、Rust SDK 与 `c3` CLI：** `core/` 全工作区（`--workspace --locked`，含 `c2-config` 49、`c2-error` 18、`c2-wire` 211、`c2-server` 189、`c2-ipc` 337、`c2-http --features relay` 567 + 1 ignored、`c2-core` 374）、`sdk/rust --all-features`（90 项）与示例 `client` / `host`、`cli`（85 项）全部通过，零警告。
2. **Node / TypeScript（C/FFI ABI 10）与 Codegen：** `core/foundation/c2-mem-ffi/bindings/typescript` 下 `npm test` 与 `npm run typecheck` 全通过（1 项 Windows 专属跳过），`c2-codegen` TypeScript 套件（62 项）与 `tests/repo/test_c2_mem_ffi_symbols.py` 全部通过。
3. **Python SDK、跨语言矩阵与仓库门禁：**
   - `tools/dev/test_native.py`（37 项）；
   - `sdk/python/tests/` 单测与集成套件（1,607 项全通过；提交 `9a771fc0` 针对 `test_serve`、`test_runtime`、`test_role_config`、Py3.10 语法与文档卫生复跑 331 通过、1 Windows 专属跳过）；
   - 18 行 Rust/Python 编译态便携载荷直连与 relay 矩阵（`test_portable_payload_cross_language.py` 与 `test_portable_payload_matrix.py`，25 项全部通过）；
   - Python 3.10 最低支持语法检查与仓库门禁（`test_docs_hygiene.py`、`test_c2_mem_ffi_symbols.py`、`test_portable_matrix_receipt.py`，304 项全部通过；`AGENTS.md` 为 32,609 字节 `<= 32,768`）。
4. **重点核验的不变量与边界场景：**
   - **元数据边界与去重隔离：** Rust / Python / TypeScript SDK、IPC 线协议与 C/FFI ABI 严格拒绝超条数、大写键、首尾空格、重复键（含自定义 JSON 反序列化拒绝重复键）与超字节总和；HTTP 路径上 Axum/Hyper 按 RFC 9110/9112 将字段名小写化并裁剪 HTTP OWS 后，relay 逐头检查拒绝重复 `x-c2-meta-*` 键并校验规范化后的键值与总字节上限；`KeyedUnary` 去重摘要排除元数据以便刷新令牌；非规范裸 `404` / `403` 在全部六条 `/_c2/*` 路由上维持 `dispatch_uncertain`；`724` 与单次调用 `call_metadata_validation` `713` 绝不触发共享目标重新获取。
   - **回调边界隔离与 fail-closed：** 嵌套调用抛出的 `724` 在四处回调出口均包装为码 3；Python 钩子未捕获异常或非法返回值一律 fail-closed 为 `724`（`reason=hook_error`）；同进程嵌套调用始终遮蔽外层 `CallContext`。
   - **共享主题汇聚准入与排空：** 单跳 A→H 与两跳 B→A→H 下创建者私有拒收不连坐并发等待者（`opening_abandoned` + `RetryJoin`），后期加入者经 `AdmitTopic` / `x-c2-topic-upstream-token` 逐人准入，令牌吊销仅隔离 B 的死上游（`707`）而不影响 A 本地订阅者。

**验证边界说明：** 本机独立运行 `pytest` 时未构建外部 `fastdb4ts` tarball，4 个 `test_typescript_*.py` 联调套件及四平台跨语言门禁均已在上述 `Source Validation` 流水线 [45539971394](https://github.com/world-in-progress/c-two/actions/runs/45539971394) 中全绿通过：四平台（Linux x64、macOS arm64、Windows x64、Windows ARM64）均以 Python 3.12 执行 `full` 范围（含普通套件、18 行便携载荷矩阵与 TypeScript 联调套件），Linux x64 另行执行 Python 3.10 `--ordinary-only` 套件（不含便携载荷矩阵与 `test_typescript_real_calls.py`）与 Python 3.14t `scope=python` 套件。
