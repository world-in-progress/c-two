# C-Two 局部精简实施计划

依据：[二轮复核](../reports/2026-10-13-defensive-programming-regression-reassessment.md)。起点为 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad`，实施分支 `socu/slimming-batches`。用户授权先实施证据充分、保持行为的局部精简；每批完成针对性回归和独立审查。Host 负责实现、验证与最终验收，独立审查通过 buddy 的只读固定快照执行。

## 批次与出口

各批按顺序推进，上一批实现、测试和独立审查均完成后再进入下一批；每批独立提交。测试失败先与基线和环境证据区分，任何实质性新改动都重跑对应验证。不为删除字段、恒空分支或重复调用编写镜像测试。

| 批次 | 对应条目与实现范围 | 针对性回归与审查边界 | 状态 |
| --- | --- | --- | --- |
| B1 | R27：仅删除 scheduler.waiting_writers、writer_wait_registered 及维护分支 | scheduler 全部测试，特别是读写公平、async/blocking FIFO、取消/关闭；确认 queue/ticket/notify/pending/active 不变；检查 SHM 路径未改变 | 已完成 |
| B2 | R28 Python：bridge 工厂依靠 ResourceBridge 构造校验；PayloadBinding 的 scoped-owner 能力依靠已验证 kind | resource conformance、input lifetime、payload binding 相关用例；非法 hook 错误和 borrowed 门禁不变 | 已完成 |
| B3 | R29：生成 service_definition 复用已有 release.reference() | codegen 单元/生成消费者测试；生成代码仍能编译使用，合同字节及公共生成表面不变 | 已完成 |
| B4 | R36 局部：bitmap 恒空循环、空 handle 紧邻重验、私有 retention 总和蕴含的局部加法检查、chunk 未读 _created_at | c2-mem bitmap/handle/retention 以及 chunk registry 测试；全域预算、错误计数、世代/跨度/锁、GC last_activity 不变 | 已完成 |
| B5 | R33 内部孤儿：私有 _resolve_client_ipc_config、三个 local_registry 闲置 helper、只由自身测试调用的 process_guard | native config 投影、local registry/package consumer 相关测试；保留实际 manifest/receipt 校验与 _stop_host 清理 | 已完成 |
| B6 | R37：退出生产发布流程的 check_version/check_cli_release 与专属测试，移除 CI 中对应测试选择 | 当前 release promotion 与 workflow 测试；自动 skip、手工冲突、partial retry、来源/字节验证不变 | 已完成 |

每批记录精确提交、命令、结果、独立审查结论及必要限制。实际发现前提不成立时，把该子项从本批撤出并记录原因；不以增加兼容层或放宽测试完成批次。

## 单独推进的范围

relay journal 保留。公开 API/ABI/语义变化先讨论：旧 transfer buffer 参数、timeout 签名、公开 client/scheduler 适配协议、ERROR_CODE、RouteAck/RouteContract、Connection/Heartbeat 观察面、ABI 3/预分配能力、配置解析错误策略、EOF/退出策略、Node 接收容量和基准选路。wrap_error 虽无调用但在活动指导中列出，本轮暂不把它随内部私有 helper 删除。

P1 的 R01 精确分配所有权、R02 safe Rust 别名、R03 shutdown 发送阶段和 R04 路由开放登记身份独立列项，保留复现/时序证据，先形成各自最小修复方案与回归要求，不混入上述精简提交。RegistrationPlan、复制/GC/轮询、跨 await 权威获取、OwnedHandle 等条件重构也另行设计。其余局部机会（IPC 重试尾部、内部状态配对、TypeScript 转发、已就绪 fixture 等待）在本轮各批稳定后按相同标准另立批次。

## 执行记录

### B1：调度器死计数

仅删除 scheduler.rs 的 waiting_writers、writer_wait_registered 及其维护分支，共 36 行；queue/ticket/notify/pending/active、SHM 载体和公开 snapshot 未变。基线 `6868ef91`，独立审查固定快照 `301e4dcff8a64727536149a1db85f4d9cd8cbb92`，Host 已逐字节确认候选源文件与审查快照一致。

验证：`CARGO_TARGET_DIR=/tmp/c2-slim-implementation-20261013/target cargo test --manifest-path core/Cargo.toml -p c2-server --lib --locked --offline`：168 passed，0 failed，包含 scheduler FIFO/关闭/取消及 buddy_dispatch_passes_shm_request_to_callback_without_inline_materialization。`rustfmt --check --edition 2024 core/transport/c2-server/src/scheduler.rs` 和 `git diff --check` 通过。日志：`/tmp/c2-slim-implementation-20261013/b1-server.log`。

独立审查 run `87bc99c8-6725-4629-9068-cb946ff8d38f` 未发现需修复问题，Host 已验收并回收审查工作树。审查为静态核验，168 项运行证据由 Host 提供。后续 Python 环境准备遇到本机 clang 缺省 C++ 头搜索路径问题，独立于本批 Rust 回归，正在以进程级 CXXFLAGS 验证修复。

### B2：Python 构造不变量

B1 实现提交为 `306a922cd256631f80e20dede0b44815b7db90aa`。B2 仅修改 bridge.py 与 payload_plan.py，保留构造器、frozen、公开签名、错误类型/文案和独立 borrowed 门禁；合计新增 1 行、删除 9 行。独立审查快照 `48bfdf16ba814f88d4a04ae94c88fc87176d8580`，run `464176d5-5a81-4183-906b-d85a3f8c9bee`，无 actionable findings，Host 已验收并核对文件一致。

验证：Python 3.13.3；当前工作树的 Python 源码及本地构建的 `_native.cpython-313-darwin.so`。`C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/unit/test_resource_conformance.py sdk/python/tests/unit/test_input_lifetime.py sdk/python/tests/unit/test_portable_payload_runtime.py sdk/python/tests/integration/test_portable_payload_runtime.py -q --timeout=30`：30 passed。初次命令误写不存在的 test_resource_bridge.py，未收集测试；更正后结果日志为 `/tmp/c2-slim-implementation-20261013/b2-python-r2.log`。没有修改或新增测试；并非宣称所有 borrowed 签名组合都已动态覆盖。

构建环境：本机默认 clang++ 无法发现 SDK 的 C++ 标准头；仅设置 SDKROOT 也失败。限定本轮命令的 `CPLUS_INCLUDE_PATH=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/c++/v1` 后，最小编译及 `uv sync --frozen --offline` 成功。CXXFLAGS 的首次尝试被已有空 CMake cache 保留，失败未计入通过。CARGO_TARGET_DIR 为 `/tmp/c2-slim-implementation-20261013/target`；没有改系统/用户全局配置或依赖版本。生成测试使用 Cargo 已锁定的 FastDB 提交 `4f99f86a662b0e950a0dd29800c25a1c9fca4def` 的只读 sibling fixture 链接。

### B3：生成服务复用已接纳 release

B2 实现提交为 `d4d0e9012a653e2f867dbfe128d28ff707325453`。B3 仅将生成 service_definition 的第二次 contract_release_ref() 改为已有 release.reference()，公开签名和 MethodBinding.index 类型未变。独立审查快照 `704090018e6896f6cfdfa9efdc5a9120f4456c6e`，run `640dc776-b95e-4476-bef5-44b2e356e5e0`，无 actionable findings，Host 已验收并核对文件一致。隐藏的 debug admission probe 仍按实际接纳次数计数，重复工作由两次减为一次；release 模式不维护该计数。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-codegen --lib --test generated_rust_sdk --locked --offline`：lib 2 passed、generated Rust SDK 4 passed，含真实生成消费者编译/调用、错误 payload 类型拒绝及 portable fixture 编译。targets.rs 的 rustfmt 和 diff check 通过。日志 `/tmp/c2-slim-implementation-20261013/b3-codegen-r2.log`。生成 Rust 源码一行及其内容哈希预期改变；canonical 合同身份/元数据、公开接口及运行语义保持。

初次 Core 构建因 packaged fastdb-sys 禁止 source mode 而失败；随后运行仓库 prepare_fastdb_sdk.py 校验官方 CoreSDK，并限定该命令使用 FASTDB_PAYLOAD_LINK_MODE=system、FASTDB_PAYLOAD_SYSTEM_LIB_DIR 和 DYLD_LIBRARY_PATH。验证包为 `fastdb-core-0.2.1-aarch64-apple-darwin.tar.gz`，源 SHA `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，归档 SHA256 `5f346177032c3c13838449db9ef30f43f3bd2b6d91241d28d2ac1edcd025ad33`；未改依赖版本。

### B4：局部内存与分块不变量

B3 实现提交为 `dcb72f590c6cf6b9172011a8b5798af224975ff7`。本批 4 文件新增 5 行、删除 15 行。bitmap 的循环上下界恒等；空 handle 保留入口首次完整校验；retention 的私有 permit charge 始终包含在同锁域总量中，全域加法通过即保证局部加法不溢出；chunk 只删除未读创建时间，活动时间与全部断言保留。独立审查快照 `a7113e1d863d9b1fdb0fc1df566fd246020b5fb8`，run `4fecae0c-6c4a-43bc-a927-7c1c031a5b3f`，无 actionable findings。Host 已核对全部文件一致、验收并回收审查工作树。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-mem -p c2-wire --lib --locked --offline`：177 + 146 passed；`cargo test --manifest-path core/Cargo.toml -p c2-server --lib --locked --offline`：168 passed，包含 SHM 无 inline materialization 回归。各命令设置本轮 CARGO_TARGET_DIR、空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS。4 文件 rustfmt 与 diff check 通过。重建 Python 原生扩展后，B2 同一组 4 文件测试再次 30 passed。日志分别为 `/tmp/c2-slim-implementation-20261013/b4-memory-wire.log`、`b4-server.log`、`b4-python-r2.log`。

Python 原生扩展初次 system mode 重建成功，但默认加载因缺少 libfastdb 的 LC_RPATH 失败，未收集测试。使用该 SDK 原有的 Git 依赖 source mode、限定进程的 CPLUS_INCLUDE_PATH 重新执行 `uv sync --frozen --offline --reinstall-package c-two` 成功；随后默认加载及上述 Python 回归通过，没有新增加载器配置或修改依赖。Core codegen 的已验证官方 SDK system mode 与 Python 原有源码构建分别遵守其既有依赖边界。

### B5：内部孤儿 helper

B4 实现提交为 `b4c2a4ae7a084db00fc073e9e49500bba085ce2e`。本批 4 文件仅删除 123 行，包括 process_guard 的唯一专属测试及两个失去用途的 import；没有新增实现或测试。Host 与独立审查均检查引用、动态加载、导出及活动指导，未发现运行消费者。native config、真实 manifest/receipt 校验和两条实际进程清理路径保留。独立审查快照 `63f3918f05617642d7844de02321220b609961d2`，run `7497f52d-994a-4be0-a67f-8605ef4c2fa3`，无 actionable findings。审查只读沙箱拒绝 here-document 临时文件，因此补充 AST 比对由 Host 执行：扣除明确删除的定义、专属测试和 imports 后，保留 AST 完全相同，候选文件与审查快照字节一致；Host 已验收并回收审查工作树。

验证：`C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest tests/repo/test_local_registry.py tests/repo/test_package_consumers.py tests/repo/test_local_candidate_manifest.py sdk/python/tests/unit/test_ipc_config.py sdk/python/tests/unit/test_runtime_session.py::test_native_runtime_session_projects_client_ipc_overrides_copy sdk/python/tests/unit/test_runtime_session.py::test_native_runtime_session_resolves_client_ipc_config -q --timeout=30`：113 passed，diff check 通过。日志 `/tmp/c2-slim-implementation-20261013/b5-regression.log`，结构比对 `/tmp/c2-slim-implementation-20261013/b5-structure-check.json`。实际 package_consumers.py 与 source_validation.py 字节未变；该结果不宣称 Windows 进程树终止已获动态证明，也不为仓外私有导入建立兼容承诺。

### B6：退役发布脚本

B5 实现提交为 `77b3d9bd73c2b6163706891d7f2bdd25710ee187`。本批 6 文件只删除 410 行，包括两个旧脚本、12 项专属测试、CI 中两个旧选择和 4 个旧路径断言。两个现行发布 workflow 与 promote_release_candidate.py 相对基线字节未变。独立审查快照 `49a132425f546d6d8262ee6db0fa914f46e3e1b0`，run `0aea2c40-e715-4dc5-8640-23e571806d8e`，无 actionable findings；Host 已核对所有候选路径与快照一致、验收并回收审查工作树。

验证按更新后 `.github/workflows/ci.yml` 的 workflow-policy 完整 12 文件选择列表执行，使用 `C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-project --offline --with pytest --with pytest-timeout --with packaging --with pyyaml --with maturin pytest --confcutdir=tests/repo ... -q --timeout=30`：506 passed，0 failed，53.91 秒；唯一 warning 来自故意构造重复 ZIP member 的负例。日志 `/tmp/c2-slim-implementation-20261013/b6-workflow-policy.log`，完整命令 `/tmp/c2-slim-implementation-20261013/b6-regression-command.txt`。包含 fake API 和实际 workflow shell 配合 fake gh 的 promotion 行为测试；本地通过不等于 hosted CI 或真实发布。diff check 通过，活动源码/测试/CI 中已无旧脚本引用。
