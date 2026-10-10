# C-Two 实现精简审查

审查日期：2026-10-09。源码基线：`4cc5b4d229d21f00fc25619e8490169b2246edd1`。本报告记录静态审查、调用者核对和发行产物副本实验，未实施重构。此前记录的 [0.7.4 首项待办](../plans/0.7.4.zh-CN.md) 保持独立；下列排期为建议。

同日回归风险复核已修正第 3、4、5、7 项及实施顺序，详见[逐项复核与 Relay 能力约束](2026-10-09-slimming-regression-review.md)。旧接口、相似代码和仅有测试调用都不能单独证明某项能力可以删除。

2026-10-10 的[历史闲置故障复核](2026-10-10-idle-eviction-regression-audit.md)确认当前 Rust 精确 token acquire 与 TS 持久直连仍有缓存/绑定缺口，并列出共享连接清理和诊断保留问题。相关路由、连接池和 TS transport 收敛以该报告的修复及回归门禁为前提。

当前最有价值的精简方向是删除已经退出运行链的接口、合并重复机制、让相同职责只有一个实现。内存回退、跨平台传输、资源生命周期和跨语言载荷语义仍有实际用途。单纯减少 crate 数、移动大文件或删测试，不能证明维护成本下降。

## 规模与测量口径

统计基于 Git 跟踪文件，排除 `target`、虚拟环境、`node_modules` 等本机构建产物。代码统计包含 Rust、Python、TypeScript、JavaScript、C/C++，按物理行计数，包含注释与空行；用测试目录/文件名和 Rust `cfg(test)` 块估算测试占比，少量测试辅助代码仍可能计入实现。因此行数用于定位，不作为删除配额。数字与产物身份见[测量记录](../reports/evidence/2026-10-09-slimming-measurements.json)。

| 对象 | 观察 |
| --- | --- |
| 跟踪文件 | 734 个，约 16.43 MiB；其中 docs 约 8.40 MiB |
| 代码文件 | 397 个，202,321 行；约 108,913 行属于测试，约占 53.8% |
| Core、SDK 与 CLI 非测试实现 | 约 78,310 行，含代码生成模板和适配构建脚本 |
| Core Rust 非测试实现 | 约 55,392 行 |
| Python / PyO3 实现 | 分别约 6,816 / 6,973 行 |
| TypeScript transport 模板 | 3,977 行，包含通用协议和路由机制 |
| 已下载的 macOS ARM64 CPython 3.13 wheel | 压缩包 4,083,027 字节，约 3.89 MiB；原生扩展 10,213,376 字节，约 9.74 MiB |

本机 Core、SDK、CLI 目录合计约 4.8 GB 包含构建缓存，不能当作框架源码或安装包体积。当前更突出的是实现与维护工作量；发行体积也有可直接测量的改进空间。

## 可以优先实施的精简

### 1. 删除旧协议表面与仅供测试调用的运行时接口

`c2-wire/src/ctrl.rs` 中的 segment announce、buddy announce、consumed 编解码已无传输生产调用，引用只剩自身测试和 `sdk/python/native/src/wire_ffi.rs` 的导出。当前 Rust 客户端/服务端的 `FLAG_CTRL` 用于其他仍有效的控制消息，不能连同该标志一起删除。

Python `transport/protocol.py` 在 SDK 生产代码中没有导入者；`transport/wire.py` 的生产调用只使用 `MethodTable`。其握手/帧封装，以及 `wire_ffi.rs` 中相应 PyO3 类与编解码导出，构成了约 1,000 行的候选审查范围。并非全部可删：`wire_ffi.rs` 后部的 contract canonicalization、fingerprints 和 release-ref 投影仍有生产调用，应归入合同投影；`MethodTable` 仍服务 Python dispatch。`payload_total_size()` 的注释声称 proxy 使用它，当前搜索没有这个调用。

另一个小项是 `c2-ipc::ResponseData::into_inline_bytes()`：当前只有 5 个 relay 内存测试调用，遇到 SHM/handle 会 panic。可以将这些测试改为显式匹配 `Inline` 并取值，再删除该接口；不能直接换成接受所有 carrier 的 materialization，否则会掩盖传输路径变化。

建议清理完整调用链和已失去产品意义的测试，保留 Rust 协议边界测试、真实 SDK/IPC 互操作及异常输入覆盖。删除 Python 导出会改变低层内部表面，实施时应扫描项目内生成代码和公开文档，不保留仅为旧实验入口服务的别名。

依据：[旧控制编解码](../../core/protocol/c2-wire/src/ctrl.rs)、[Python wire 投影](../../sdk/python/native/src/wire_ffi.rs)、[Python 协议门面](../../sdk/python/src/c_two/transport/protocol.py)、[MethodTable 与旧包装](../../sdk/python/src/c_two/transport/wire.py)、[ResponseData](../../core/transport/c2-ipc/src/response.rs)。

### 2. 清理 Python 分发链中的空参数和重复结果转换

`PyCoreService::invoke()` 在 `core_ffi.rs:598` 固定传入第四个参数 `None`。Python dispatcher 继续传递该 `response_allocator`，直到 `crm/transferable.py:366–368` 接收 `_c2_output_allocator` 后立即删除。这个参数没有提供分配能力，可以删除整条传递链；响应分配继续由 native/Core 负责。

`server/native.py:585` 和 `:608` 的两个 `release_fn` 经 AST 比较，函数体一致。可以共享回调的构造，保持 view 与 borrowed 各自的释放时机。`core_error_ffi.rs:202、213` 与 `runtime_session_ffi.rs:1258、1269` 对相同 `RelayCleanupError`、`RouteCloseOutcome` 维护了两套相同字典字段，应合为一组 PyO3 私有转换函数。`core_error_ffi.rs:87、93` 还存在重复的 `ConfigFrozen` match arm。

验收重点是 callback 参数、解码/执行/序列化失败时的恰好一次释放、borrowed owner 先失效后释放，以及 registration rollback、unregister、shutdown 的错误属性和字典保持一致。不要把 Python 字典转换移入语言中立 Core。

依据：[原生 callback](../../sdk/python/native/src/core_ffi.rs)、[Python dispatcher](../../sdk/python/src/c_two/transport/server/native.py)、[transfer 包装](../../sdk/python/src/c_two/crm/transferable.py)、[错误投影](../../sdk/python/native/src/core_error_ffi.rs)、[会话投影](../../sdk/python/native/src/runtime_session_ffi.rs)。

### 3. 单次注册快照需要先明确元数据采集时机

`NativeServerBridge.register_crm()` 在 `native.py:243` 经 `_route_contract_hashes()`、`build_contract_fingerprints()` 构造 descriptor 取得指纹，随后在同次注册的 `:294–306` 再做 diagnostics、descriptor 构造与序列化。portable 与 pickle 路径都存在这项重复工作。

建议在一次注册内生成一个 descriptor 快照，再派生 slot 元数据、指纹及传给 native 的 JSON。但 `get_type_hints()` 可以执行字符串 annotation，减少构造次数会改变求值次数与部分异常时序，因此这是快照语义重构，不能按纯删除处理。快照应在现有 CRM 实例/bridge 构造之后形成，保留 portable 与 pickle 各自的指纹派生、方法顺序、发布前校验和错误优先级。connect/export 仍保留独立入口；不引入全局缓存。

验收 portable/pickle 的指纹、方法索引与 access、shutdown 方法排除、注册失败顺序和发布前拒绝不匹配。依据：[注册流程](../../sdk/python/src/c_two/transport/server/native.py)、[descriptor 与指纹](../../sdk/python/src/c_two/crm/descriptor.py)、[Core host 校验](../../core/runtime/c2-core/src/host.rs)。

### 4. Node 连接适配器存在行为差异，不能直接互换

生成模板 `typescript_transport.ts:977–1261` 实现了一套 Node 连接、读缓冲、写入与关闭适配；`c2-mem-ffi/bindings/typescript/src/index.ts:484、918` 另有一套。真实 TypeScript 调用夹具注入后者的 `nodeRuntime.connect`，生成模板中的工厂在当前实现及运行夹具中没有调用者，生成测试只检查其导出存在。后者还保留了旧 ABI3 库缺少端点上下文接口时的 `legacySymbols` 分支（`:525–535`）。

复核撤回直接用 binding 替代模板工厂的建议。模板允许结构化 socket 的 `write` 返回 void、缺少 `off/end/destroy`；binding 要求更强的对象结构。必须先明确并保留自定义连接注入能力，再决定共享哪些内部缓冲与错误处理。`legacySymbols` 是已测试的旧库能力降级，删除它会改变同 ABI3 下的必要符号集合，需要同步 C addon 的符号组检查，且 Unix `connectUnix` 要求不能套到 Windows。

两套写入适配行为存在区别：模板仅在 `write()` 返回 false 时等待回调，binding 始终等待。本次已运行一个反例：自定义 socket 返回 false，并仅通过回调报告错误时，模板拒绝写入，binding 却成功返回。先补齐 callback-only 错误、结构化注入、EOF/关闭与回调时序覆盖，再讨论合并。现有 binding 已有至少 512 字节长目录下真实连接与往返测试，应保留该证明。

依据：[生成 transport](../../core/foundation/c2-codegen/assets/typescript_transport.ts)、[Node binding](../../core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts)、[真实调用夹具](../../sdk/python/tests/fixtures/typescript_real_call.mjs)、[生成导出检查](../../core/foundation/c2-codegen/tests/generated_targets.rs)。

## 需要集中重构的部分

### 5. 响应 SHM 准备和发送汇入同一条实现

`c2-server/src/response.rs:19` 的 `try_prepare_shm_response()` 只有自身测试及 relay forwarding 测试调用。其分配、取指针、写入失败释放，与 `server.rs:4030` 的 `write_buddy_reply_with_data()` 重复；后者的 buddy 组帧、发送失败处理和 dedicated 消费处理，又与 `send_response_meta()` 的 `ShmAlloc` 分支（`:4185`）重复。

复核确认 `try_prepare_shm_response()` 服务于公共 `CrmCallback` 的直接填充响应能力，不能因只有测试直接调用而删除。两条准备路径的错误分类也不同：owned bytes 的指针失败可以回退，公共填充 helper 的指针/填充失败返回错误。可先共享内部 allocation/fill 原语及 prepared SHM 组帧，保留调用者的错误分类和等价直接填充入口。`ResponseMeta::ShmAlloc`、payload/u32 限制、generation、方向隔离与 dedicated `read_done` 均保留。

取消、部分发送和发送成功后的所有权不能用一个无条件 Drop 统一：是否允许释放取决于发送阶段及消费证明。验收应覆盖现有两条 reply 写失败、prepared SHM、超限释放、buddy 禁用后的 dedicated/chunk 回退，以及 relay 晚结果处理。

依据：[旧准备 helper](../../core/transport/c2-server/src/response.rs)、[生产发送路径](../../core/transport/c2-server/src/server.rs)、[relay prepared-response 覆盖](../../core/transport/c2-http/src/relay/forwarding_tests.rs)。

### 6. 合并纯组帧逻辑，保留传输调度和生命周期差异

`c2-ipc/src/client.rs:3678–3696` 与 `:3915–3928` 重复构造请求 chunk 的 flags、header、首块 control、payload 与 frame；服务端也有手工组 frame header 的路径。可以由 `c2-wire` 提供检查长度、写入调用方 buffer/分片的统一编码入口。

这项合并不应顺带统一 writer 锁范围、首字节之后的 deadline 行为或请求/响应 chunk 几何。验收编码字节一致、边界溢出、首末块、部分帧取消后的连接隔离和流源中途失败；减少分配的收益需要实测，不能只凭 helper 数量推断。

依据：[请求分块](../../core/transport/c2-ipc/src/client.rs)、[frame 编码](../../core/protocol/c2-wire/src/frame.rs)、[服务端发送](../../core/transport/c2-server/src/server.rs)。

### 7. Relay 类型可以整理，缺省域和一致性分支不能直接删除

`client/control.rs:164–220` 为保持原 `RelayRouteInfo` 字面量写法而添加 `ResolvedRouteWithNamespace`/`RelayResolvedRoutes`；响应同时合并 header 与 JSON 的域信息，维护部分缺失和冲突的组合。`client/relay_aware.rs:202`、`relay/router.rs:739、1412` 又保留缺少域信息时推断为默认域的行为。

复核撤回直接删除缺省域分支的建议：当前生成 TypeScript transport 没有自动传递 endpoint namespace，真实 `relay-aware-local-ipc` 消费夹具也没有注入该 header，现有默认域 IPC 快路依赖此分支。Rust 包装类型可以整理，但先保持 HTTP 数组形状、缺省域语义、header/JSON 冲突时抑制 IPC 而保留 HTTP 的行为。只有所有当前消费者都获得等价域投影、严格矩阵仍证明原路径后，才能讨论撤掉旧表达。域信息不能从 peer relay 泄露为本机 IPC 权限。

Relay 的 route UID/revision、server instance、owner generation/lease epoch、tombstone、两类 catalog revision、最终 owner 探测及连接状态分别承担不同责任。本报告没有找到可直接删除这些一致性机制的依据；其约束与反例已列入[复核报告](2026-10-09-slimming-regression-review.md)。

依据：[解析与包装](../../core/transport/c2-http/src/client/control.rs)、[客户端选择](../../core/transport/c2-http/src/client/relay_aware.rs)、[relay 注册与发现](../../core/transport/c2-http/src/relay/router.rs)。

## 依赖、运行和发布成本

### 8. 先修正发行构建配置，再决定功能裁剪

当前只有 `core/Cargo.toml` 设置了 release 的 fat LTO 和单 codegen unit。CLI、Python native、Rust SDK 有各自的 manifest/lockfile，不能假定它们继承 Core workspace 的 profile；Cargo 只读取当前 workspace 根的 profile。[Cargo 官方说明](https://doc.rust-lang.org/cargo/reference/profiles.html)

对已下载且校验身份的 0.7.3 macOS CPython 3.13 原生扩展副本执行 `strip -S -x`，文件由 10,213,376 字节降为 7,895,376 字节，减少约 22.7%；相同 ZIP deflate 参数下由 3,840,106 降为 3,426,249 字节，减少约 10.8%。两个副本分别在独立 Python 进程导入，118 个导出名称及错误注册表一致。这仅证明该产物的符号裁剪潜力；没有证明完整 RPC、性能或其他平台通过。所有实验文件只位于 `/tmp`。

建议在真实发行入口统一 profile/strip 策略，保留可追溯的调试符号，先测 strip，再比较 thin/fat LTO 的体积、链接耗时和运行性能。不要为缩小体积直接改为 `panic=abort` 或取消异常/清理能力。修改产物的步骤必须发生在最终哈希、收据和消费验证之前。

依赖方面，离线 `cargo tree` 确认 Python native 的普通依赖树包含 `c2-codegen → fastdb → fastdb-sys`，Python 包同时依赖 `fastdb4py`。代码生成导致 Python runtime 包还携带一份原生 FastDB 构建，但 `cc.compile_contract_artifacts` 是真实 API，不能当死代码直接删。将工具能力从默认运行时分离值得评估，收益要用真实构建差分测量。`c2-core` 无条件依赖 HTTP 客户端，`c2-http` 启用 Tokio `full`，也有 feature 收敛空间；relay 服务端已有独立 feature，不能误称 Python 无条件嵌入完整 relay 服务。

四个 Cargo workspace 的 FastDB 链接/发布边界并不完全相同，直接合成单 workspace 可能改变 source/System CoreSDK 行为。先统一需要一致的配置和复用可验证的构建缓存，避免仅为少几个 manifest 引入新的条件分支。

### 9. 执行器和 TypeScript 通用运行时需要独立实验

IPC sync client、HTTP client 和 Core owned-call 各有一个按需创建、包含两个 worker 的进程级 Tokio runtime。全部启用时配置合计六个 worker，不是每连接创建六个线程。可以实验由统一运行时提供 handle，测量线程、RSS、调用耗时和关闭语义；保持 deadline 之后的原生任务存活及晚结果释放。本次未测运行时 RSS 或 CPU，不能将三个执行器直接认定为性能故障。

更大的重复在 TypeScript：当前模板自行实现握手、帧/chunk、连接状态、请求序号、relay cache、身份判断和 retry 分类；Rust Core 已有对应机制。对于已经使用 native memory/endpoint binding 的 Node 路径，可实验一个 payload-owner-neutral 的 Core encoded-client 投影，再让生成客户端只保留类型、FastDB JS owner/view、Promise 与异常适配。当前没有这个投影，3,977 行 TS transport 不能立即整体删除，迁移也不会自动使 browser runtime 可用。

验收需要现有严格 12 行 TypeScript 矩阵、direct IPC 的 relay 独立性、远端 anchor 行为、身份与合同不匹配、dispatch-uncertain 不重放、held/borrowed 失效及内存回退，并比较并发与复制量。该方向有较大长期收益，也有最高的实现风险。

依据：[IPC executor](../../core/transport/c2-ipc/src/sync_client.rs)、[HTTP executor](../../core/transport/c2-http/src/client/http_client.rs)、[owned-call executor](../../core/runtime/c2-core/src/call_execution.rs)、[TypeScript transport](../../core/foundation/c2-codegen/assets/typescript_transport.ts)。

### 10. 收敛验证脚本和活动文档入口

Windows `local-platform` 与 `full` 两个 scope 在同一 OS 上重复准备环境并执行重叠 Core 测试；同时存在一般 CI、专项 endpoint workflow 和 release candidate。可以抽取共享环境/构建步骤和门禁定义，复用相同源码对、工具链与配置的产物。实际覆盖、不可变产物哈希、18 行 Rust/Python 与 12 行 TypeScript 收据、非管理员 wheel 消费及清理证明应保持。专项 workflow 本来就是手动/特定分支触发，不能把所有文件都算成每次 PR 的重复成本。

活动文档存在过期入口：`AGENTS.md:237` 仍写 Unix `v2.2` 目录，roadmap 开头仍以 0.7.0/0.3.0 为基线并描述尚余发行矩阵。建议让活动规则指向当前配置、生命周期和发布指南，阶段记录与冻结收据仅作为历史索引；保留证据原文。删除历史文档不会减少运行时负担，修复活动入口才能减少误读和重复实现。

依据：[Windows gates](../../tools/ci/windows_native.py)、[Windows workflow](../../.github/workflows/windows-native.yml)、[专项验证](../../.github/workflows/local-endpoint-validation.yml)、[发行候选](../../.github/workflows/release-candidate.yml)、[项目规则](../../AGENTS.md)、[路线图](../roadmap.md)。

## 建议顺序与验收原则

复核后的首批候选缩小为：无效 allocator 参数、重复字典投影/不可达 match arm、已确认退出当前协议的旧编解码，以及活动文档修正。旧 codec 删除前要完成畸形输入测试的保留/迁移清单。发行符号裁剪仍是独立候选，需完成各目标的安装消费、异常展开和最终字节验证。注册快照、Node 适配器、SHM 准备、Relay 域信息、TypeScript/Core 与执行器统一均需先完成复核中列出的行为约束和缺口测试，不再把前四项整体列为可立即实施。

每项交付记录实际删除的生产入口/重复状态、保留的行为、原测试到替代测试的映射和测得的体积/性能变化。现阶段不承诺“整体砍掉某个百分比”：本报告定位了候选范围，净删除量必须来自实际 patch。单纯把测试挪到其他文件，只计可读性改善。

明确保留：Python 同进程对象直传；Rust SDK 的薄 facade；FastDB owner 与 C-Two lease 的不同责任；请求/响应内存方向隔离；buddy generation、dedicated、chunk、file 回退；Windows Named Pipe/mapping；listener closure、callback drain 和 retained lease 的独立观察；直接 IPC 与 relay 的独立性；dispatch 阶段和禁止盲目重放的规则。

## 本次核验与协作状态

Host 核验了 Git 跟踪体积、测试/实现分类、Cargo 普通依赖树、关键调用者、Python AST 重复、PyO3 字段映射，以及产物副本的 strip/import 实验。原生 6.1 Sol 独立复核传输/内存候选后，Host 再检查实际代码。未运行完整测试，也未以此报告声称任何重构通过。

Buddy 使用已安装的 0.27.0 skill。三个只读任务中，传输与构建任务先遇 GLM 429，恢复后遇 `HARNESS_PREMODEL_FAILED`；SDK 任务留下最终审查文本，但因 native shutdown 协议错误未封存交付。Host 将该文本仅作线索并独立核对，不作为已验收 artifact。三个任务按失败记录收尾，并按各自受管工作树清理计划回收；没有把报告文本的 completed 声明当作系统交付成功。
