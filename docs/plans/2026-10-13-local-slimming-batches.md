# C-Two 局部精简实施计划

依据：[二轮复核](../reports/2026-10-13-defensive-programming-regression-reassessment.md)。起点为 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad`，实施分支 `socu/slimming-batches`。用户授权先实施证据充分、保持行为的局部精简；每批完成针对性回归和独立审查。Host 负责实现、验证与最终验收，独立审查通过 buddy 的只读固定快照执行。

本轮 B1–B6 已完成，18 个非文档文件新增 7 行、删除 594 行，净减少 587 行；退出 13 项只服务已删除实现的测试。每批均有独立提交、现有行为回归及固定候选独立审查，审查未发现阻塞问题，6 个审查工作树均已验收回收。运行证据来自本机 macOS arm64；未执行 Windows/Linux hosted CI、性能基准或发布，不推导跨平台全面通过或性能收益。relay journal 及 GC 运维日志保持，公开 API/ABI 和 P1 项目仍按下述边界另行推进。

| 批次 | 实现提交 | 实际回归 |
| --- | --- | --- |
| B1 | `306a922cd256631f80e20dede0b44815b7db90aa` | c2-server 168 passed |
| B2 | `d4d0e9012a653e2f867dbfe128d28ff707325453` | Python 30 passed |
| B3 | `dcb72f590c6cf6b9172011a8b5798af224975ff7` | codegen 2 + generated Rust SDK 4 passed |
| B4 | `b4c2a4ae7a084db00fc073e9e49500bba085ce2e` | c2-mem 177 + c2-wire 146 + c2-server 168 + 重建后 Python 30 passed |
| B5 | `77b3d9bd73c2b6163706891d7f2bdd25710ee187` | config、local candidate、consumer 及 RuntimeSession 113 passed |
| B6 | `8295266fe679c4aba52e31e97c4019a3a253fbc3` | workflow-policy 完整 12 文件清单 506 passed |

以上为各批实际运行次数，重复运行的测试不计为新增独立覆盖。两份历史报告的 280 个源码链接已固定到原审查提交并核对文件与行号，避免删除或移行后丢失依据。

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

## 第二轮安排

规划输入为首轮完成后的 `08955f0a1c1e94e21da2a45ca5c04abba071dc49`，计划提交为 `60bac6541e2747adaa0c22e6f256adb34fe76589`。用户已同意执行 B7/B8，实际状态与验证见下方执行记录。继续保留 relay journal、公开 API/ABI 和现有错误语义；P1 具体接口/布局/错误投影仍按独立设计边界讨论，不混入本轮局部精简。实施前重新核对源提交与工作树，若前提发生变化，先更新对应边界。

### B7：剩余局部精简

B7 按下表顺序拆成三个独立提交；每项完成针对性回归和固定快照独立审查后再进入下一项。目标是撤去已失去作用的内部状态与等待，不合并不同层的重试或资源协议。

| 子项 | 精确范围与目标形态 | 必须保持的行为 | 回归与出口 |
| --- | --- | --- | --- |
| B7a · R36 | [IPC acquisition](../../core/transport/c2-ipc/src/client.rs#L4615) 的 lookup_route_contract_for_acquire、acquire_route、acquire_route_token：删除 last_not_found/last_unbound 和不可达尾部；循环消费固定退避序列，无下一次重试时直接返回当前实际结果 | 每层原有 6 次迭代、10/25/50/100/200 ms 退避、错误分类与最终错误内容；compaction/watch 恢复的额外查询；每次获取 live authority；外层原始 deadline。不得把两层预算合成一层，或以 unreachable/panic 代替尾部兜底 | c2-ipc 的 lib/tests，启用 test-support 使 pending 清理断言实际执行；c2-http 的 relay state acquisition 用例启用 relay feature。重点验收 deadline 内重试、缓存 token 仍做 live lookup、关闭/移除/同名重登记、最后一次成功与耗尽返回实际错误；仅在现有用例未覆盖后两项时补行为测试 |
| B7b · R36 | [TypeScript binding](../../core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts#L438)：删除无后续读取的 options = loadOptions；normalizeRequestPool 直接调用 normalizeSegment，删除仅转发的 normalizeRequestSegment | addonPath 捕获、endpoint context 生命周期、segments 快照和冻结、完全相同的错误标签及元数据校验；包 exports、声明接口和 ABI 3 不变 | npm run typecheck 与 npm test（包含既有原生 addon 构建）；核对请求 metadata snapshot、非法 segment 拒绝、release/forgetConsumed/close。无需为删赋值或转发函数新增镜像测试 |
| B7c · R38，含窄 T04 判据修正 | 两份 IPC integration 文件中 register 后的 3 处 sleep(0.3)；segment_size_benchmark.py 中普通 bytes/dict 结果的 4 处 hasattr/release 探测。并发用例先补线程全部结束及 9 次调用完成的判据，再去对应等待 | register 的同步 native readiness；真实 IPC 数据完整性和清理；保留 GC/业务/跨进程等待以及真实 ResponseBuffer/HeldResult.release；benchmark 的结果持有、计时区间、选路与 close/unregister 保持 | test_zero_copy_ipc.py、test_ipc_buddy_reply.py、test_direct_ipc_control.py；检查启动返回即 ready、重启和重复启动失败路径。benchmark 仅做小负载 bytes/dict 调用 smoke，不运行全矩阵或据此宣称性能提升 |

B7a 的两项直接验收入口是 `core/transport/c2-ipc/tests/connect_timeout.rs` 中的 route_publication_retry_sleep_is_inside_connect_budget 与 matching_cached_route_token_still_has_live_lookup_deadline；relay 还需 exact_token_ready_to_closed/removed/reregistered 的真实调用拒绝及 callback 为 0。Rust 改动后重新构建 Python native 再运行 B7c，避免沿用旧扩展的通过记录。平台声明以实际 gate 为准，保留既有 Linux/macOS/Windows CI，不把本地 macOS 运行替代跨平台通过。

B7c 的第三处等待位于 `test_ipc_buddy_reply.py::TestBuddyReply::test_concurrent_large_calls`；现有 join(timeout=20) 后仅断言 errors 为空，不能证明工作完成。补完成判据属于防止假通过的测试修缮，应独立核对清理和总超时，不通过增加 sleep 或放宽 timeout 消除失败。其他线程测试和广泛 T04 改造不随本项扩展。

### B8：私有状态收敛

R31 的独立只读复核 run 为 `eba0201a-513f-4df7-817c-ceee4d7bbc56`，固定输入与本轮规划一致。Host 已复核关键构造/释放/锁路径，验收报告并回收审查工作树。下面三项可按保持现有行为的方案各自落地；没有要求把全部额外测试缺口一次性铺开。

| 子项 | 最小设计 | 关键回归与边界 |
| --- | --- | --- |
| B8a · BorrowedPayload | 直接持有 Payload；payload() 直接借用；Drop 仍先 invalidate，再让字段析构 | sdk/rust 的 payload_lifetime 及 codegen generated_rust_sdk；正常、业务 Err、输出编码失败和 panic 出口的 checked view 失效。保持生成接口和 copy-backed 语义，不用字段形状测试代替生命周期测试 |
| B8b · client_config_frozen | 删除 RuntimeState 的重复 bool，以 frozen_client_config.is_some() 派生；公开 getter、setter 与生命周期转移两端的检查同步改投影 | 保留原 mutex、revision、锁外解析后的重验、原始 deadline；配置解析失败不冻结，首次有效尝试即冻结，随后连接失败或 shutdown 都不解冻。复用 client_modes/native_endpoint/endpoint_context/connect_timeout 与 Python RuntimeSession 用例；必要时补非法配置可修正的行为。若不改 revision 机制，不为删除 bool 另造同步 hook |
| B8c · PyEndpointSweep | Option<OpenSweep> 统一 iterator/lease/closed；OpenSweep 字段顺序为 iterator 后 lease；context、budget、batches 留在外层 | close/Drop/unwind 均先销毁 iterator，再归还 projection lease；EOF 或 round_interrupted 不等于公开 close；关闭后 context/protocol 仍可读。复用 endpoint maintenance/context 用例，补“旧 owner 二次 close/drop 不释放新 owner 的 lease”和“EOF 后仍占 lease”；保持 PyO3 借用门禁，不能用 GIL 假定替代同步 |

Held<T> 暂不并入 B8。两个 Option 的稳定状态确实配对，但直接保留 tuple 局部会把 invalidator panic 时的析构顺序从 response→value 改为 value→response。Host 的最小 Rust 探针验证了这一语言机制，日志在 `/tmp/c2-next-batch-plan-20261013/drop-order-probe.log`；它不是实际 transport 释放通过证据。先在原实现补真实 SDK Held<T> 的 invalidation Err、transport Err/组合错误和 unwind 清理顺序，再使用配对 Option，并分别声明局部以保持现有析构顺序。现有 Core 的替身 guard 和 SDK 的 owned-bytes 用例不足以单独证明真实响应租约释放。

### P1 的独立设计与实施入口

| 单元 | 先讨论的具体设计 | 修复前应固定的反例与出口 |
| --- | --- | --- |
| R01 + R02 · 分配身份与唯一写所有权 | 分配器记录精确活分配状态；区分入站原始坐标与 allocator 创建的私有 owner，切片借用绑定 owner。统一评估 SHM 布局、公开 Rust 接口及 FFI 影响；不能只改一个方法为 &mut self 或只隐藏 handle 字段 | R01 非法子区间释放被拒且 bitmap/count/free_bytes 不变，活块不退休或重叠；R02 非法可变别名由 safe API 无法构造，覆盖重复打开同 backing。保持正常读写、trim 原容量释放、generation 与多进程共享路径 |
| R03 · shutdown 发送阶段 | 区分 NotSent 与 DispatchUncertain，覆盖外层 timeout。发送前保持重试；开始控制写后丢确认不重发到同地址的新实例。先确定沿用现有未确认结果还是调整错误投影；ACK 仍只代表 initiate | 发送前暂不可用后成功；部分/完整发送后丢 ACK；原实例退出后同地址替换实例不收到重发；原始总预算不重置。该修复与 B7a 的业务 route acquisition 循环分开 |
| R04 · 开放登记事务 | token 绑定 route UID/具体登记；开放与 OwnerMissing/关闭共享同步边界，并核对待开放状态。保持 shutdown generation；状态锁不跨 callback drain | A 的旧 token 不能打开同名 B；OwnerMissing 与最终开放交错后 callback 为 0；正常 relay 最终发布仍可开放；catalog 与 scheduler 状态一致。先固定真实行为反例，再改事务和替换仅检查签名的测试 |

以上 P1 设计尚待讨论，优先级不因 B7 的简单程度而降低。它们独立提交、独立回归与审查，不混入精简提交。R08 配置错误策略、R23 timeout、R24 旧 transfer 参数、R25 公开适配协议，以及 RouteAck/Connection/Heartbeat 等公开观察面继续沿原清单另行讨论。

## 执行记录

### B1：调度器死计数

仅删除 scheduler.rs 的 waiting_writers、writer_wait_registered 及其维护分支，共 36 行；queue/ticket/notify/pending/active、SHM 载体和公开 snapshot 未变。基线 `6868ef91`，独立审查固定快照 `301e4dcff8a64727536149a1db85f4d9cd8cbb92`，Host 已逐字节确认候选源文件与审查快照一致。

验证：`CARGO_TARGET_DIR=/tmp/c2-slim-implementation-20261013/target cargo test --manifest-path core/Cargo.toml -p c2-server --lib --locked --offline`：168 passed，0 failed，包含 scheduler FIFO/关闭/取消及 buddy_dispatch_passes_shm_request_to_callback_without_inline_materialization。`rustfmt --check --edition 2024 core/transport/c2-server/src/scheduler.rs` 和 `git diff --check` 通过。日志：`/tmp/c2-slim-implementation-20261013/b1-server.log`。

独立审查 run `87bc99c8-6725-4629-9068-cb946ff8d38f` 未发现需修复问题，Host 已验收并回收审查工作树。审查为静态核验，168 项运行证据由 Host 提供。后续 Python 环境准备遇到本机 clang 缺省 C++ 头搜索路径问题，独立于本批 Rust 回归；最终进程级修复与失败尝试见 B2。

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

### B7a：IPC acquisition 重试尾部

实现基线 `60bac6541e2747adaa0c22e6f256adb34fe76589`。三个循环改为消费固定延迟 iterator，在无下次重试时返回当前实际错误，生产代码净减少 35 行。新增一个真实 LocalStream 回归，以 6 个场景验证两入口的末次 Ready/NotFound/ContractMismatch；该测试先在旧实现通过，再在候选上通过。它不单独证明 lookup 与 bind 之间的竞争，或 watch/compaction 的恢复行为。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-ipc --features test-support --lib --tests --locked --offline`：203 + 9 passed；`cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib relay::state::tests --locked --offline`：45 passed，含旧 token 拒绝、owner 替换与共享在途调用。rustfmt/diff check 通过。各命令用既有 CARGO_TARGET_DIR 与空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS；日志 `/tmp/c2-slim-round2-20261013/b7a-baseline.log`、`b7a-ipc.log`、`b7a-relay.log`。后续 Python 验证前已以既有 source mode/CPLUS_INCLUDE_PATH 重建 native，日志 `b7-native-build.log`。

buddy 初始 health 正常，但提交时 launcher 返回 UPGRADE_IN_PROGRESS，未创建 run、未进入路由。按原生只读审查替代，由 `/root/b7a_independent_review`（gpt-6-sol/high）完成独立审查，无 actionable findings。Host 及 reviewer 起止均核对固定 diff SHA256 `5c02f2d9b894edbae86ae1ecafe27e0ccb35f8760ac9976594f1f84a5e029d59` 和两个文件 hash，一致后验收；记录 `b7a-review-manifest.json`、`b7a-independent-review.txt`。审查未执行测试，不将源码等价性结论当运行证据。

### B7b：TypeScript 内部转发

B7a 实现提交为 `e4b947a4c57ccbdc15f07e1f8aed331b5aa8c4c0`。本项唯一实现文件 index.ts，新增 1 行、删除 11 行。独立审查 run `64e48cca-ff00-49bd-86e3-7046d400e398`，固定快照 `f8de43786eda0a2d49542e6ba3f471101d65094d`，无 actionable findings；Host 核对字节一致后验收回收。buddy 此时已恢复为 0.32.0。参数绑定/closure、getter 顺序与次数、this、错误类型与 label、冻结和释放均保持；删除 helper 会改变调试栈帧，不承诺源码反射文本不变。

验证：在 TypeScript 包目录运行 `npm run typecheck` 成功；以既有 CARGO_TARGET_DIR、CARGO_NET_OFFLINE=true 和空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS 运行 `npm test`，含原生 Rust/C addon 构建：59 passed、0 failed、1 skipped（需要真实 Windows 的 current-SID 测试）。Node v25.8.1、npm 11.11.0。生成 index.d.ts 与删除前逐字节一致，SHA256 `a5f6e769498bbd868f1090a44e3fef7e21ece469e7b7b0e024cd409d7167b0cb`。日志 `/tmp/c2-slim-round2-20261013/b7b-typecheck.log`、`b7b-node.log`，diff check 通过。
