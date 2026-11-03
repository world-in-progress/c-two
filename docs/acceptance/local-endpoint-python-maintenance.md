# 本地端点维护的 Python 薄投影

2026-10-06。实现源码为 C-Two `2ab09639a9db945ebd229669aff37aaf6fed96bb`。本报告只描述该提交之上的 Python/PyO3 投影层，不把文档提交当作二进制源码。

## 范围与边界

本轮只做 P1/P2 endpoint native API 的 PyO3/Python 薄投影。未改动 OwnerBound、Core 算法、CLI、registry、server native、allocator 或任何包版本；没有派生 PR，也没有 push。

新增或修改的文件仅限：

| 文件 | 作用 |
| --- | --- |
| `sdk/python/native/src/endpoint_ffi.rs` | 新增 PyO3 投影：`inspect_endpoint_endpoint`、`reap_endpoint_credential`、`PyEndpointCredential`、`PyEndpointSweep` |
| `sdk/python/native/src/lib.rs` | 注册 `endpoint_ffi` 模块 |
| `sdk/python/native/Cargo.lock` | 同步 `c2-local` 依赖边（`c2-local` 的 `serde` / `uuid` 均为工作区已有 crate） |
| `sdk/python/src/c_two/transport/endpoint.py` | Python 门面 |
| `sdk/python/src/c_two/__init__.py` | 顶层导出 |
| `sdk/python/tests/unit/test_endpoint_maintenance.py` | 聚焦单元测试 |
| `docs/reports/local-endpoint-python-maintenance.md` | 本报告 |

`Cargo.toml` 未改动：`c2-core` 已经重导出 `c2-local` 的端点生命周期面，因此投影层不需要新增直接依赖，也不需要新的 crate 版本。

## 实现的接口

`inspect_endpoint(address, *, endpoint_protocol=None)`：`endpoint_protocol=None` 时经 `ConfigResolver::resolve_client_ipc` + `ConfigSources::from_process()` 解析配置的进程协议，不使用编译期常量；显式值由 Rust `LocalEndpointProtocol` 枚举解析。端点由 `LocalEndpoint::from_address_with_protocol` 派生，不探测路径。

`reap_endpoint(address, credential)`：先比较逻辑地址，再按凭证自带的协议重新派生端点。凭证记录的协议优先于进程默认协议，因此 managed-v2 凭证不会因为进程默认是 legacy-v1 而被误判为 stale。native 身份校验决定最终结果。

`PyEndpointCredential`：opaque、frozen，内部只持有 native `EndpointCredential`。`from_json()` / `to_json()` 完全委托唯一的 Rust 解析器，严格执行 4096 字节上限、未知字段拒绝、schema 版本拒绝与 v1/v2 一致性检查。Python 侧没有字段校验表，也不拼接路径。metadata（`address` / `protocol` / `platform`）不是 secret，也不是 liveness 证明。

`PyEndpointSweep(protocol, *, max_entries=None, max_ms=None)`：持有恰好一个 native iterator，并把本次 sweep 的默认 batch 预算作为 native `SweepBudget` 保存在 Rust 对象内（`None` 即 `c2-local` 的 `SweepBudget::default()`，即 64 entries / 10 ms）。构造时先用同一个 native gate 验证显式覆盖值，再取进程维护租约、再打开 iterator：被拒绝的预算不可能留下已持有的租约或半开的 sweep。`next_batch(*, max_entries=None, max_ms=None)` 的 `None` 维度复用 open 阶段已由 native 验证的默认值，显式维度由同一 gate 验证后覆盖，且不会重置另一个未指定的维度。拒绝规则：`bool` 与任何非整数是 `TypeError`；非正、超上限（entries 4096 / ms 1000）、以及超出可精确解析宽度的整数一律是干净的 `ValueError`，不截断、不 clamp、不 panic，并且在这些检查之前不做任何 `Duration` 运算。`close()` 释放 native iterator 与进程租约且幂等；`Drop` 覆盖未 `close()` 的遗弃路径。同进程唯一 maintenance lease 完全由 Rust 的原子 compare-exchange 控制；Python 的 `closed`/`close` 只是 native 状态投影，Python 门面没有预算常量、默认值、验证函数或生命周期标志。

`Result dict` 的字段固定为 `status` / `credential` / `reason` / `io_kind` / `raw_os_error` / `retryable`，调用方无需把缺失字段当作隐含失败。成功只报告 native 实际返回的 status：`reaped`、`already-absent` 是唯一的终态成功；`busy`、`stale-target`、`unverified`、`not-applicable`、`io-error` 原样上报，partial cleanup 不会被编造为成功。`KernelManaged`（Windows 命名管道）映射为 `not-applicable` / `kernel-managed`，`NotApplicable` 映射为 `not-applicable` / `no-filesystem-entry`；两者都描述由哪一层拥有端点生命周期，都不当作 alive。

Sweep 每个 batch 返回真实的 native 计数：`entries_visited`、`endpoints_examined`、`reaped`、`already_absent`、`busy`、`stale_target`、`unverified`、`io_errors`、`last_io_error`、`not_applicable`、`leases_retired`，外加 `round_complete`、`round_interrupted`、`namespace_changed`。只检查 `round_complete` 的调用方不会把被中断的一轮误认为完整覆盖，因为 `round_interrupted` 会持续置位。

`py.detach` 覆盖所有 FS 与 iterator batch 调用（inspect、reap、sweep open、`next_batch`）。没有全目录 collect，没有 `PYTHONPATH` 操作，没有 `shutil` / `rmtree`。

## 预算权威收口（本轮）

Host 复核指出：Python 门面此前自行镜像 `MAX_SWEEP_ENTRIES=4096` / `MAX_SWEEP_MS=1000` 与 `DEFAULT_MAX_ENTRIES` / `DEFAULT_MAX_MS`，并由 `_bounded_int` 充当另一套预算验证权威，与“薄 native projection”不符，不能验收。本轮只改 `endpoint_ffi.rs`、`endpoint.py`、`test_endpoint_maintenance.py` 与本报告：默认 batch 预算改为保存在 Rust `PyEndpointSweep` 内；`None` 走 `SweepBudget::default()`；显式覆盖值在持有租约、打开 iterator 之前由 native gate 验证；`next_batch` 的 `None` 维度复用该已验证默认，显式维度由同一 gate 验证。Python 侧删除全部镜像常量、默认值与 `_bounded_int`，只把 `None` 或调用方给的 typed 值原样传给 native 并委托，`closed`/`close` 保持 native 权威，模块不新增任何 Python 生命周期状态。

native gate 现在自己解析每个整数（先拒绝 `bool`，再用 `i128` 精确解析），因此“超宽”不再由参数转换产生 `OverflowError`，而是与其它越界值统一的 `ValueError`，公开门面与 native 直连的错误契约一致。原测试 ID 全部保留：只有 `test_native_gate_rejects_values_beyond_the_budget_integer_width` 的期望异常按新权威从 `OverflowError` 更新为 `ValueError`；`test_native_gate_rejects_out_of_range_values_without_the_python_precheck` 的注释与形状断言同步更新（该用例名保留，历史名称中的 “python precheck” 已不存在）。`test_endpoint_module_keeps_no_python_field_tables` 增加断言：`MAX_SWEEP_ENTRIES`、`MAX_SWEEP_MS`、`DEFAULT_MAX_ENTRIES`、`DEFAULT_MAX_MS`、`_bounded_int`、`SweepBudget` 均不得出现在门面模块，且 `EndpointSweep.__slots__ == ('_native',)`。新增两个用例：`test_open_validates_every_budget_before_taking_the_process_lease` 证明被拒的 open 不占租约；`test_rejected_batch_override_keeps_the_iterator_and_stored_default` 证明 batch 覆盖被拒后 iterator 未关闭、已存默认仍生效。

此前的真实缺陷回归仍然保留并覆盖：`10**19`、`2**32`、`2**64`、`2**70`、`10**40` 在 open 与 per-batch 两条路径都以 `ValueError` 拒绝，边界值 `max_entries=4096`、`max_ms=1000` 被接受，`bool`/浮点/字符串预算是 `TypeError`。

## 已执行的验证

在独占 target `/tmp/c2-endpoint-targets/python-endpoint` 下，以 system link 模式（`FASTDB_PAYLOAD_LINK_MODE=system`、`FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib`）编译 native crate，产出真实扩展并安装：

- `cargo check --manifest-path sdk/python/native/Cargo.toml --lib`：通过，`endpoint_ffi.rs` 零警告（本轮预算权威收口后再次通过）。
- `cargo metadata --locked --manifest-path sdk/python/native/Cargo.toml`：通过，锁文件与 manifest 一致（`c2-core -> c2-local` 依赖边，以及 `c2-local` 的 `serde` / `serde_json` / `uuid` 依赖）；本轮未改动 `Cargo.toml` / `Cargo.lock`。
- `uv sync --reinstall-package c-two`：真实重编译并安装成功；默认 uv 缓存在沙箱外不可写，故本轮改用 `UV_CACHE_DIR=/tmp/c2-uv-cache`。
- `pytest sdk/python/tests/unit/test_endpoint_maintenance.py`：本轮权威收口后 62 项全部通过（此前 51 项、复核补强后 60 项；原测试 ID 全部保留，新增 2 项）。
- 完整 `sdk/python/tests/unit/` 与 full / cold Cpp 门禁按 Host 约束本轮未重复；上一轮的观察为 693 项通过 / 1 项既存守卫失败（见下，与本投影逻辑无关）。

真实 native 行为已实测：`inspect_endpoint` 对未绑定地址返回 `absent`；默认协议与显式 managed-v2 结果一致；managed-v2 sweep 在真实命名空间读到 18 个条目并报告 `round_complete`；第二个 sweep 被 Rust 租约以 `RuntimeError` 拒绝，`close()` 后租约可复用；凭证 round-trip 逐字节一致；畸形、未知版本、超长文档分别以 `malformed-json`、`unsupported-schema-version`、`too-large` 拒绝；跨地址 reap 返回 `stale-target`，不做跨 root probe。

本轮在同一真实扩展上直接复核了新的预算权威：`None`/`None` 打开默认 sweep 并成功返回 batch 1；`max_entries=0`、`max_ms=True`、`max_ms=2**70`、`max_entries=1.5` 在 open 阶段分别以 `ValueError` / `TypeError` 拒绝，随后同一进程仍可正常打开 sweep，证明被拒的 open 没有占用租约；`next_batch` 的显式覆盖 `max_ms=10**40` 以 `ValueError` 拒绝、`max_entries=True` 以 `TypeError` 拒绝后，iterator 仍返回后续 batch 且 `closed is False`。

上一轮收口复核（预算权威变更前）曾补充针对性负例实测：把 `C2_IPC_ENDPOINT_PROTOCOL` 设为非法值后，`inspect_endpoint(address)`（`endpoint_protocol=None`）以 `ValueError` fail closed，而显式协议仍正常返回，证明 `None` 路径确实读取 Rust resolver 而不是编译期常量；在同一非法进程策略下，带 managed-v2 记录的 credential 仍按记录协议完成 reap（`already-absent`），证明 reap 不读取进程默认协议；两个线程并发打开 sweep 时恰好一个成功、另一个得到 `RuntimeError`，证明同进程唯一租约由一个原子 compare-exchange 控制；预算被拒绝后 `next_batch()` 仍返回第 1 个 batch，证明失败调用不破坏 iterator 与租约。本轮真实扩展上的对应复核见上一段。

复核同时核对了固定源码边界：`endpoint_ffi.rs` 只调用 `c2-core` 重导出的 `inspect_endpoint` / `reap_endpoint` / `EndpointSweep` / `EndpointCredential`，凭证编解码只经 `EndpointCredential::from_json` / `to_json`，OS 端点只经 `LocalEndpoint::from_address_with_protocol` 派生，Python 门面没有 JSON 解析、字段表或路径拼接；inspect / reap / sweep open / `next_batch` 四处都经 `py.detach` 执行真实 FS 与 iterator 工作。

## 未执行与遗留

未运行 full / cold Cpp 门禁，按任务约束如此；本轮也未重复完整 unit 套件。Windows 上的 parse / kernel-managed / managed-v2 rejection 分支由 `cfg` 分支覆盖而非 skip，但本机为 macOS，本轮与之前均未在 Windows 实机执行，仍留给 Host 的 Windows 验收。

收口复核发现并修正了 3 处会在 Windows 上失败的测试分支（均在 `test_endpoint_maintenance.py` 内）：`test_inspect_reports_absent_for_an_unbound_endpoint` 原先无条件断言 `absent`，而 Windows native 对未绑定地址返回 `KernelManaged`（现断言 `not-applicable` / `kernel-managed`）；`test_inspect_defaults_to_the_configured_process_protocol` 原先固定用 managed-v2，而 Windows 无法派生 managed-v2 端点，现按平台选择合法协议并断言对应 status；`test_reap_does_not_accept_a_decoded_credential_for_a_new_incarnation` 的 `pytest.skip` 位于 `from_json` 之后，在 Windows 上不可达且会先失败，现已改为平台 cfg 分支。修正后该文件不再包含任何 `pytest.skip`。Windows 上 managed-v2 文档由 endpoint derivation 先以 `invalid-value` 拒绝（`LocalEndpoint` 的平台检查顺序使然），测试注释已如实说明，未改固定源码。

`test_python_does_not_own_buffer_lease_accounting` 是上一轮观察到、与本投影逻辑无关的唯一失败项：该守卫把 `"_entr" + "ies"` 作为子串扫描整个 `src/c_two`，而任务规定的公共参数名 `max_entries` 必然包含该子串；Host 已说明正在中央把它改成 AST 精确 identifier guard（保留既有测试 ID 与所有 forbidden needle）。`test_sdk_boundary.py` 不在本轮写入范围内，本投影未修改该文件，也未再次运行完整 unit 套件。

`sweep_endpoints` 未提供 `max_batches` 参数：任务只规定 `max_entries` 与 `max_ms`，且 `EndpointSweep` 本身即可显式控制 batch 数。

旧 credential 不能移除新 native incarnation 的完整实证需要真实的两代 listener。本轮核对了固定源码：公开 server / client 路径目前不消费 `endpoint_protocol`——`core/transport/c2-server/src/server.rs` 绑定端点时调用 `LocalEndpoint::from_address(address)`，`core/transport/c2-ipc/src/control.rs` 也同样只用 `from_address`，而 `LocalEndpoint::from_address` 固定 `LocalEndpointProtocol::LegacyV1`。因此 `cc.register` / `cc.set_server` 在当前源码上无法生成 managed-v2 listener，写一个“managed-v2 两代 restart”测试只会实际走 legacy-v1 并制造错误证明；接通该 protocol 需要改 transport，超出本轮写入范围。故该测试不写入门面测试文件，`test_reap_does_not_accept_a_decoded_credential_for_a_new_incarnation` 仍只断言该凭证给出非成功的诚实结果（`already-absent` / `stale-target` / `unverified`），真实两代 slot 的移除拒绝连同 managed-v2 listener 接线留给 Host 同源码 full gate。

Host 侧仍需：完成 `test_sdk_boundary.py` 守卫的 AST 形态收窄；在同源码 final build 上运行统一 Core / Owner / Python full gate，复核 Python 门面与 native 注册；在 Windows 实机执行 cfg 分支；以及在 transport 开始消费 `endpoint_protocol` 后补真实两代 managed-v2 listener 的“旧 credential 不可移除新服务”证明。
