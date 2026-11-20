# 错误类型与残留接口收敛执行计划

日期：2026-11-19（维护者批准定稿）。依据：[交互面精简审查](2026-10-31-api-surface-review.md)第四节、[待办](backlog.md)原第 3 项与待维护者决定（`G8`、`R21`）、[瘦身账本](2026-10-13-local-slimming-batches.md)（`R21`、`R32`、`R33`）。状态：本机实现与测试通过，待独立审查与全平台门禁验证。

本计划在 `C-Two 0.11.0` 与 `c3 0.7.0` 发版前完成最后一批破坏性接口收敛：保留 `C2E1` v1 线格式与 `c2-error::ErrorCode` 数值身份，在 Core 建立 `(side, stage)` 与派发阶段（`transport_phase`）的唯一派生权威；校正 `c2-server` 误用码 `1 / 2 / 3` 的传输与容量拒绝；把 Python `c_two.error` 的 8 个 `一侧 × 阶段` 笛卡尔积类与无生产者的 `705 / 712` 子类收敛为可据以行动的 19 个公开异常类与标准 `ErrorCode`；连根删除 Python↔PyO3 进程内 `C2E1` 字节往返（`CoreError.error_bytes`、`CCError.serialize/deserialize`、`server/reply.py`）；同时整块退役残留的 `c_two.config`（`G8`）与输出旧模型的 `generate_crm_template`（`R21`）。所有分阶段仅为实施顺序，不保留任何旧名字、兼容别名或双轨分支。

## 一、核心设计与边界

### 1. 线上 `C2E1` 与 Rust 错误权威

- **保留线格式与错误码数值：** `C2E1` v1（magic、版本、`code`、`name`、`message`、`details`）与 `c2-error::ErrorCode` 的现有数值（`0`、`1..=8`、`701..=719`；永久封存 `704`、`706`、`711`）保持不变。
- **单一宏派生 `side` 与 `stage`：** 在 `core/foundation/c2-error` 定义 `ErrorSide`（`Client`、`Resource`）与 `ErrorStage`（`Serialize`、`Deserialize`、`FromBuffer`、`Execute`、`Call`），由 `ErrorCode` 同一份声明派生 `ErrorCode::side(self) -> Option<ErrorSide>` 与 `ErrorCode::stage(self) -> Option<ErrorStage>`（码 `1..=8` 映射对应一侧与阶段，其余返回 `None`），线上与 `details` 均不写入冗余键值。
- **校正 `c2-server` 与 `c2-http` 的错误码归因：**
  - `RouteAdmissionError::UnknownMethod`（资源未执行，`CallStatus::NotExecuted`）由误标 `3 ResourceFunctionExecuting` 校正为 `713 ProtocolViolation`，对齐 `c2-core` 的 `MethodNotFound` 分类。
  - `c2-server` 在请求进入资源适配器前的 RPC 记录解码拒绝、请求 SHM 存储校验失败与入站复制前错误（`server.rs` 的 `rpc_record_admission`、`request_storage`、`queue_input_error` 与 `stream.rs` 的 `FirstError::Input`）由误标 `1 ResourceInputDeserializing` 校正为 `713 ProtocolViolation`（保持 `CallStatus::NotExecuted`），使码 `1` 仅代表真实的资源输入反序列化钩子失败。
  - 一元响应 `ResponseSendError::Capacity`（资源已执行，`CallStatus::Failed`）由误标 `2 ResourceOutputSerializing` 校正为 `717 CallCapacityExceeded`，对齐流式 `ItemSendError::Capacity`；`ResponseSendError::UserVisible`（帧长度溢出等输出编码边界拒绝）保持码 `2`。
  - `c2-http` relay 路由器对业务请求在派发前返回 `712 RouteWatchUnavailable`（HTTP 503）时补齐 `dispatch_phase="pre_dispatch"`；控制面状态查询保持无派发阶段。
- **收窄码 `7 ClientCallingResource` 的产生点：** `c2-core/src/call_execution.rs` 中 `guard_error` 对非截止时间的内部状态异常保留显式禁止 fallback/撤路的诊断且不伪造 `pre_dispatch`；传输与执行器 panic/异常兜底继续携带 `CallScope` 实测阶段。
- **统一 `transport_phase` 判定权与 Rust SDK / C ABI 导出：**
  - 在 `c2_core::Error` 上提供权威的 `pub fn transport_phase(&self) -> Option<TransportPhase>`：外层传输事实优先（`Error::ProtocolVersionMismatch` 为 `Some(PreDispatch)`；`Error::Transport(e)` 取 `Some(e.phase())`；`Error::Contract(_)` 与 `Error::Admission(_)` 在派发前产生，取 `Some(PreDispatch)`；`Error::Lifecycle(LifecycleError::ResponseCopy(_))` 取 `Some(DispatchUncertain)`，其余生命周期配置/宿主管理错误按上下文为 `None` 或调用前 `PreDispatch`；`Error::Semantic(e)` 仅采信经 Core 归一保留的 `pre_dispatch` 或 `dispatch_uncertain` 传输/转发明细——IPC `Failed` 与 HTTP 500 已由 Core 剥离内层阶段，故资源已执行错误恒为 `None`，HTTP 流 `FAILURE` 记录保留 `dispatch_uncertain`）。
  - `sdk/rust` 顶层正式公开重导出 `ErrorCode`、`ErrorSide`、`ErrorStage`、`C2Error` 与 `TransportPhase`，无需借道隐藏的 `c_two::generated`。
  - 修复 `core/foundation/c2-mem-ffi/src/client.rs` 的 `error_value`：当外层 `Error` 已确定 `transportPhase`（如 `Error::Transport` 的 `dispatch_uncertain`）时，禁止被内层 `semantic.details.get("transport_phase")` 覆盖。
- **暂不并入本批的内部边界：** Rust 内部 `c2-core`↔`c2-server`/`c2-ipc` 之间的 `CrmError::UserError(Vec<u8>)` 进程内字节传递不影响任何公开 API、ABI 或线格式，继续留在[待办](backlog.md)第 2 项“服务端回调面”统一处理。

### 2. Python `c_two.error` 公开表面与异常传递契约

- **标准 `ErrorCode` 枚举：** 删除 `ERROR_Code`、手工 `_NATIVE_TO_PY_ERROR_NAMES` 与各子类硬编码的 `ERROR_CODE = 7xx` 整数字面量；由 `_native.error_registry()` 直接生成 `ErrorCode(IntEnum)`，成员名与 `c2-error::ErrorCode` 完全一致（如 `ErrorCode.Unknown`、`ErrorCode.ResourceFunctionExecuting`、`ErrorCode.ResourceNotFound`）。
- **跨家族只读 `transport_phase`：** 在 `CCBaseError` 上提供只读属性 `transport_phase: Literal['pre_dispatch', 'dispatch_uncertain'] | None`。`CoreError` 由原生权威写入内部槽位（同步调整 PyO3 构造逻辑，不再向只读 property 调 `setattr` 忽略失败）；`CCError` 读取经 Core 归一或本地派发前阶段显式赋予的阶段，绝不从任意自定义文本推断。
- **收敛为 19 个公开异常类：**
  - 基础与底层：`CCBaseError`、`CoreError`、`CCError`（暴露 `code: ErrorCode`、`message: str`、`details: dict[str, str]` 以及由原生注册表派生的只读 `side: Literal['client', 'resource'] | None` 与 `stage: Literal['serialize', 'deserialize', 'from_buffer', 'execute', 'call'] | None`）。
  - 执行与编解码（取代原 8 个 `一侧 × 阶段` 类，原类名全部删除）：
    1. `ResourceExecutionError(CCError)`：固定 `ErrorCode.ResourceFunctionExecuting`（码 `3`，`side='resource'`，`stage='execute'`），表达资源方法、流生成器或资源清理自身抛出的异常。
    2. `SerializationError(CCError)`：仅接受编解码与缓冲区视图相关的 6 个码（`1 ResourceInputDeserializing`、`2 ResourceOutputSerializing`、`4 ResourceInputFromBuffer`、`5 ClientInputSerializing`、`6 ClientOutputDeserializing`、`8 ClientOutputFromBuffer`，传入其他码抛 `ValueError`），由 `side` 与 `stage` 严格区分 `serialize`、`deserialize` 与 `from_buffer`。客户端在本次调用尚未进入传输前发生输入编码失败（码 `5`）时携带 `transport_phase='pre_dispatch'`；若码 `5` 由已进入的资源嵌套调用转抛，服务端与同进程出口照常剥离内层阶段，外层 `transport_phase` 为 `None`。
    3. 码 `7 ClientCallingResource` 不设独立子类，按码映射落回基类 `CCError`（`side='client'`，`stage='call'`）。
  - 路由、准入与协议子类（保留 14 个可据以行动的子类，收回 `705 RegistryUnavailable` 与 `712 RouteWatchUnavailable` 落回基类 `CCError`）：
    `ResourceNotFound`（701）、`ResourceUnavailable`（702）、`ResourceAlreadyRegistered`（703）、`ResourceClosed`（707）、`ResourceRemoved`（708）、`ContractMismatch`（709）、`IdentityMismatch`（710）、`ProtocolViolation`（713）、`FallbackDenied`（714）、`CallDeadlineExceeded`（715）、`UnsupportedCallMode`（716）、`CallCapacityExceeded`（717）、`CallCancelled`（718）、`RouteConflict`（719）。`registry.py` 中不可达的未知 target mode 分支改为内部 `RuntimeError`。
- **对齐客户端与服务端异常边界（含生成 Python 客户端）：**
  - 一元客户端包装器（`crm/transferable.py`）在准备、计费与调用阶段与流式（`crm/stream.py`）一致放行 `CCBaseError`（不再把无语义码的 `CoreError` 包进码 `7` 或码 `5`）。
  - 一元服务端包装器（`crm/transferable.py`）对齐 Rust `adapter_error`、Python 流式与同进程直调：资源内抛出的 `CCError`（或携带语义错误的 `CoreError`）保留其原语义码、消息与业务详情，在新投影实例上剥离内层 `transport_phase` / `dispatch_phase`；同进程 `_invoke_local` 不再原地篡改原异常且同步清除外层不适用字段（如 `fallback_eligible`）。
  - 流式解码（`crm/stream.py`）与 Python FastDB 载荷绑定（`crm/payload_plan.py`）及 `c2-codegen` 生成的 Python `ContractClient`（一元与流）统一按实际失败操作区分 `from_buffer`（如 `Payload.open_copy` / 视图构造与失效）与 `deserialize`（如 `pickle.loads`、`require_spec_sha256` 校验），保留 FastDB 六项结构化原因字段（`cause_owner='fastdb'` 等），不再仅凭 `self._held` 把普通 pickle 解码失败标成 `from_buffer`。
  - 显式关闭的 `CRMProxy` 在一元、流、直调与连接观测入口统一抛出本地 `ResourceClosed`（`transport_phase='pre_dispatch'`），与同进程路由关闭后触发重新解析的内部 `_LocalRouteClosed` 严格区分。

### 3. `R32` / `R33`：消除 Python↔PyO3 进程内 `C2E1` 二次编解码

- **无环类型化语义投影：** 删除公开字段 `CoreError.error_bytes: bytes | None`。`CoreError` 内部保存不可变的语义快照（或无环只读投影），`CoreError.semantic`（只读属性或每次构造独立 `CCError` 实例）与 SDK 抛出路径（`raise projected from core_error`）各自持有独立的 `CCError` 实例，杜绝 `semantic.__cause__ -> core_error -> semantic` 循环引用，并保证 `copy.copy` 与 `pickle` 往返完整保留类型、`args`、消息与结构化字段。
- **统一投影入口：** 客户端调用、路由获取、Host 注册（不再用外层文本覆盖语义 `message`）、`Stream.close()` 回执（保留 `CoreError` 为 `__cause__`）及 PyO3 `route_concurrency_ffi`（消除裸 `RuntimeError` 与 `'route closed'` 文本匹配）共用同一套原生语义投影。
- **删除 Python 侧 wire 编解码与二元组协议：** `python_error_to_c2` 直接识别 `CCError` 或 `CoreError` 的语义负载并按原 `(code, message, details)` 重建 `C2Error`（保留合法空消息）；删除 `CCError.serialize`、`CCError.deserialize`、`_native.encode_error_wire`、`_native.decode_error_wire_parts`、`CrmCallError`、`_ResourceFailure` 及整份 `sdk/python/src/c_two/transport/server/reply.py`（含 `wrap_error` 与 `unpack_resource_result`）。一元 `crm_to_com` 正常返回序列化字节（或 `None`），失败直接抛出 `CCError`，严格保持 borrowed 输入在输出序列化完成后失效、单次释放及主异常优先于清理异常的不变量。分派边界对非 SDK 包装器返回的非法类型直接报明确 `TypeError`。

### 4. `G8` 与 `R21` 整块退役（`G2` 保持不动）

- **`G8`（删除 `c_two.config`）：** 删除 `sdk/python/src/c_two/config/`（`C2Settings` 与 `settings`），`RuntimeSession` 成为唯一配置权威。`cc.set_relay_anchor(address: str | None)` 与 `Runtime.set_relay_anchor(address: str | None)` 以 `None` 清除代码覆盖（恢复按环境与 `.env` 解析），对空串或纯空白字符串抛 `ValueError`；`Runtime` 完成 `shutdown` 轮换新会话时在同一把锁内直接读取旧会话的 `relay_anchor_address_override`，删除 `_with_settings` 与 `_sync_relay_override`。`examples/python/relay_config.py` 改为无显式参数时调用原生 `_native.resolve_relay_anchor_address()`；`sdk/python/benchmarks/_relay_support.py` 及相关基准改为创建并交付独立 `cc.Runtime()`，不再读写或恢复全局 `settings`。
- **`R21`（删除 `generate_crm_template`）：** 删除 `sdk/python/src/c_two/crm/template.py`、`c_two/crm/__init__.py` 中的导出、`sdk/python/tests/unit/test_crm_template.py` 与 `sdk/python/tests/template/crm.template.py`。
- **`G2` 边界：** `crm.client` 连接装配点与已公开文档化的 `HeldResult(release_cb=...)` 本批保持不变。

## 二、分阶段实施与门禁

1. **阶段 1（Rust Core、Server、SDK 与 C ABI）：** 实施第一节第 1 条（`c2-error`、`c2-server`、`c2-http`、`c2-core`、`sdk/rust`、`c2-mem-ffi`），更新对应 Rust 与 Node 测试；跑通 `cargo test --manifest-path core/Cargo.toml --workspace`、`cargo test --manifest-path sdk/rust/Cargo.toml --all-features` 及 Node ABI 测试。
2. **阶段 2（`G8` 与 `R21` 退役）：** 删除 `c_two.config` 与 `generate_crm_template` 整链，迁移示例、基准与所有引用 `settings` 的 Python 测试；重编扩展并跑通受影响 Python 单测与集成测试。
3. **阶段 3（Python `c_two.error`、PyO3 无环投影与 `c2-codegen` Python 目标）：** 实施第一节第 2、3 条及 `c2-codegen` 生成 Python 客户端的 `SerializationError` 归一；更新 Python 测试集，跑通 `tools/dev/test_native.py`、全套 Python 单测/集成测试、Python 3.10 语法检查及跨语言可移植载荷 18 行矩阵门禁。
4. **阶段 4（文档、迁移说明与全平台验证）：** 同步 `docs/reference/` 受影响文档、`AGENTS.md` 退役接口表、`docs/releases/unreleased.md`、`CHANGELOG.md`、`docs/design/backlog.md` 与瘦身账本；跑通 `tests/repo/`；经独立只读代码审查与 Windows / GitHub Actions 全平台门禁（`Source Validation`）全部通过后收口。
