# 待办

尚未完成的工作与待维护者决定的事项都列在这里，细节写在各自的文档里。做完的条目从这里删掉，并在验收或发行记录里注明。顺序由维护者定；建议都标明是建议。

## 进行中

- **流式 RPC**（[设计](2026-11-03-streaming-rpc-design.md)于 2026-11-04 批准，[ADR 0003](../decisions/0003-server-streams.md)；执行计划是设计第十节）：第一阶段（块 1 至 4）已于 2026-11-04 验收：确切候选 `787d4205` 的全平台 CI 通过，见[验收记录](../acceptance/2026-11-04-streaming-phase-1-acceptance.md)；分支 `socu/streaming` 在 fork 上，尚未合入上游 main。块 5a（IPC 16 调用记录）已于 2026-11-05 合入整合分支，确切候选 `b5b4853c` 的全平台 CI（run 41911808218）通过；块 5b（服务端流的传输）同日合入（`02b4c008`），确切候选 `d6c808f9` 的全平台 CI（run 41936786846）在重跑失败的任务后全部通过。块 6a（Runtime 与 Rust SDK 的服务端流）同日合入（`fd913fad`），确切候选 `ae4b0647` 的全平台 CI（run 41962137139）首次即全部通过。块 6b（Python 的服务端流）同日合入（`b2ee739b`），确切候选 `293bbe37` 的全平台 CI（run 41987564596）在修正两处测试竞态、重跑一个失败任务后全部通过；块 6 至此完成。块 7（Node 的服务端流与 C/FFI ABI 8）同日合入（`0406b7c9`）。确切候选 `d543846b` 的全平台 CI（run 42014116482）在 Linux 与 macOS 上通过，两个 Windows 任务都失败在同样两处：Node loader 测试改成在本进程加载临时目录里的 DLL，Windows 删不掉仍被加载的 DLL；块 7 新增的两个 c2-ipc 握手测试，假对端写回复时得到 232。两处都是测试的问题，已修好合入（`d742c0fe`）。修复后的全平台 CI（run 42022054809）在 Windows 上又失败在块 7 新增的 `test_typescript_server_stream.py`：它的夹具直接启动 `npm`，Windows 上 npm 是 `npm.cmd`，20 个用例在夹具阶段报找不到文件；改用现成的 `process_control.portable_command`，作为块 8 的顺带项修，块 7 在 Windows 上的验证随块 8 的全平台 CI 完成。块 8（relay 的服务端流，只做 relay 与 Rust、Python 客户端，即计划里的 8a）2026-11-06 合入（`d23c7180`），宿主检查通过，独立审查的四处缺陷已在合入前修好。确切候选 `ffe9ee4f` 的全平台 CI（run 42084831707）在 macOS、Windows x64 与 ARM64、Linux 3.14t 上通过（块 7 在 Windows 上的验证随之完成），Linux 3.12 上三个 Rust 测试失败：c2-ipc 转发接收方的 `completion()` 在等待循环里用 `try_lock`，终态在持锁时通知，等待方可能永远等不到（8a 的产品缺陷，relay 在下游取消或断开后用它等终态）；c2-http 一个测试读到回复后立刻断言 B 为零，而 8a 起响应片要等 HTTP 库丢掉最后一个引用才退款（测试竞态）；第三个是下面“要核实的事”里那个 c2-server 关闭测试。另外 8a 的慢读测量让 c2-http 的单元测试从约 10 秒变成 267 秒（CI 的 Linux 上 764 秒），要移出常规测试。前两处与慢读测量已修好合入（`3e6cec65`），宿主检查通过，c2-http 的单元测试回到约 10 秒；新候选的全平台 CI 待跑。生成的 TypeScript 经 relay 的流移到下面第 5 项。下一步是块 9（交叉验收）。块 6 也分两步：6a 做 Rust Core 与 Rust SDK，6b 做 Python。块 5 与块 6 分步的理由见设计第十节表格后的补记。

## 接下来（建议的顺序，待维护者确认）

同一块代码里的事合成一批做，免得先做的被后面的批次推翻。

1. **派发后的结果查询与去重**（维护者 2026-11-02 认为值得做）：事发那一刻进程内兜不住，结果未知时如实报派发不确定；这一项让调用方事后能查清一个派发不确定的调用是否执行过，或带同一个调用标识安全重试而不重复执行。业界有数据库的事务结果查询（例如 Oracle 的逻辑事务 ID）与 HTTP 幂等键，待一手来源调研。协议批之后先写设计与 ADR：调用标识跨重连与进程的稳定性、服务端结果的保留期限与存储、与有状态调用顺序的关系、与流式 RPC 的交叉。
2. **按区域的结构调整与重复校验。** 不碰协议，各区先出方案：
   - **Core 与 Python：** E1、E2、E4、E13；C5、C6；G5、G9；R06 回调重入导致自锁的风险。
   - **本地端点：** tmpfiles（D-tmp）先在 Linux 上复现 systemd-tmpfiles 清掉 gate 与 marker 的情形，再实施目录 flock（本机与 Windows 测试机都不是 Linux）；之后 D1、D2、D5、D6、D10 的复查只留在所有权、发布与 unlink 边界；R17 剩下的 gate/context 与目录 pin 的错误分类。顺带：c2-local 的 Drop 在门锁短暂争用后由后台线程完成退役，`managed_drop_after_gate_contention_settles_ownership` 两种终态都接受，要有一个能观察后台重试的点才能确定地守住。
   - **relay：** F2（删 watch 闸门，轮询降到秒级）、F3、F6；F4、F8、F10；relay-aware 调用没有每次调用的路由回执，`observed_route` 仍是连接时的值；F7 剩下的 `/_admin/unregister` 与注销 202/204/404 的区分，等维护者答复。`relay/conn_pool.rs` 为替代 P4 删掉的 `force_connected`，在生产代码里加了只在测试构建生效的连通状态覆盖（`test_assume_connected` 等），看能否改用真实连接，移出生产路径。`c3 relay --upstream` 只在 relay 启动时登记一次：路由那时还没开放就报 "does not export route"，`c3 relay` 直接退出；登记成功之后，上游重启（身份改变）或路由被注销时，relay 会把它撤下，也不会再自己重新接手，只能重启 relay 或手工 `POST /_register` 补上。维护者 2026-11-01 要求记入：改为启动后持续重试、路由开放后接手、上游重启后重新接手；待定的是重试的节奏与上限、启动时登记失败是否仍让 `c3 relay` 退出、以及与资源进程经 relay anchor 自行登记同一路由时谁优先。
   - **服务端回调面：** `CrmCallback` 与流回调都收一个 `ResponsePool`，生产实现（`core/runtime/c2-core/src/host.rs`）全部不用，流的 SHM 编码也由服务端自己取池；内存策略文档仍承诺它是低层回调直接填充响应的扩展点。先裁决是否保留这个外部扩展点，撤下就一并收回参数、公开 helper 与 `ShmAlloc` 分支。排在流式块 7、8 之后（2026-11-05 架构审查）。
   - **契约、代码生成与 CLI：** H1、H4、H6、H8（用校验过的构造器加私有字段，取代 C01）、H9；R29 等剩下的 codegen 同次缓存与 typed projection。
3. **配置收敛**（[接口审查](2026-10-31-api-surface-review.md)第三节）：逐项表定下之后进行。与 B8、H3、H7、H12、C4 交叉，R09、R10 的 EnvCatalog 多次加载一并处理。`chunk_threshold_ratio`（`C2_IPC_CHUNK_THRESHOLD_RATIO`）已证实没有作用：只在 c2-config 里解析与校验，c2-ipc 只在比较两份客户端池配置时用到它，分片判断不读它；连同环境变量、Python 字段与测试一起删除。
4. **错误类型**（接口审查第四节）：协议批之后。一并：删除从没产生过的错误码 706 `WriteConflict`，连同 Python 的错误类、映射与相关测试（维护者 2026-10-31 同意；704 是还在使用的 `RouteStale`，不动）；711 `RouteCatalogCompacted` 在协议批删掉 IPC 路由目录之后也没有生产者了（2026-11-03 核实，只剩注册表、Python 错误类、映射与 `__all__`），建议一并删除；`sdk/python/src/c_two/error.py` 里重复注册错误的 docstring 写成 resource name，改为 route name；R32、R33 等剩下的结构化错误内部序列化、Python 错误常量与孤立的 `wrap_error`。
5. **TypeScript 与 Node：** 流式块 7 已让 Node 经 C/FFI ABI 8 调用 Rust Core 的客户端，R16 剩下的 Node→Core 投影迁移随之完成；还没做的是解码不透明的响应 allocator。2026-11-05 的架构审查补了两项，都排在流式块 8 之后：一是生成的 TypeScript 文件内嵌整份 transport，契约不变也要随 IPC 或 ABI 升级重新生成，应把运行时拆成独立的 `sdk/typescript` 包，生成器只输出契约胶水；二是 `core/foundation/c2-mem-ffi` 名为 foundation 层，块 7 之后实际依赖 c2-core、c2-ipc、c2-contract 与 c2-error，工作区里没有别的 crate 依赖它，Python 也不用它，只服务 Node，应把它的原生库与 TypeScript 绑定一起归入 `sdk/typescript`，按客户端的职责改名，仍出一个动态库，同步 loader、C 头文件、npm 包与 CI 路径。原流式块 8b（生成的 TypeScript 经 relay 的 HTTP 流）2026-11-05 经维护者同意移到这一批，免得先在生成的 transport 里手写一遍、重构时再改；在此之前，生成的 TypeScript 客户端经 relay 的 HTTP 调用流方法，照旧在派发前报 `UnsupportedCallMode`，经直接 IPC 照常可用。Node 作为后端客户端维持（维护者 2026-11-05）；Node 能加载原生插件，它的 HTTP 路径也可以像 IPC 一样经 ABI 8 走 Rust 客户端，TypeScript 里不再手写这套机制，这是 Host 的推论，留到这一批设计时定。
6. **发版工具，方案 B**（2026-11-05 架构审查补充的先后：先把仍在用的两种回执迁到 `tools/testing`（`tools/dev/test_python.py` 与正式矩阵还在调用），把 Unix 跨 UID 验证并入 Source Validation，再删旧候选三件套、它们的自测与 `local-endpoint-validation.yml`；那个工作流的理由还写着已由 ADR 0002 退役的 owner 机制）（[发版敏捷性审查](2026-10-30-release-agility-review.md)第三、四节）：与产品代码无关，可以随时插入。B1 候选重整、B2 删除约 8 千行（`tools/local_rc` 与 `local-endpoint-validation.yml` 仍在）、B3 FastDB 版本只写一处、B5 精简各条线的 gate、B6 修 `PROBE_BUDGET` 的偶发失败。
7. **路线图上的其余产品能力**（完成标准见[路线图](roadmap.zh-CN.md#产品能力)）：官方不可变的包分发、契约兼容、调用元数据与准入钩子、dry-run、async unary、遥测与背压、可发布的 TypeScript SDK、发现与命名空间治理。顺序待维护者定。
8. **文档里的协作流程用词**（2026-11-04 维护者同意）：设计文档里用 Host、Worker、微任务、看板、委派块、任务书指协作流程，约 60 处，分布在 9 份设计文档里；仓库外的读者不知道它们指谁，Host 还与术语表里的 Host 撞名，worker 与执行 worker 撞名。改为不写角色，例如“Host 建议”只写“建议”。指拍板者的“用户”改为“维护者”，约 120 处；“开发者”在这里指 C-Two 的使用者，不用来指维护者。冻结的验收记录不改。
9. **下一次发版时：** 根 README、SDK README 与 `cli/README.md` 里的版本说明（IPC 14、SHM 3、ABI 6）改为发版时的 IPC 16、SHM 4、C/FFI ABI（块 7 之后为 8）与 mesh 7。

各批一并删除只钉源码文本或已删名字的测试（复审第六节）；复审附录里各区未裁决的待议项，实施到对应区域时再定。[瘦身账本](2026-10-13-local-slimming-batches.md)暂缓表与条目边界表里的其余各项仍按账本写明的条件。

## 要核实的事

- Node 绑定的测试 `ABI 8 unary encoding consumes the native pre-dispatch deadline`（`core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-protocol.test.mjs`）在 2026-11-05 修 Windows CI 的 Worker 那里，Windows 11 x64 上跑三轮失败过一次，单独连跑三次都通过。测试用 1 ms 的调用截止时间加 20 ms 的忙等编码，没有留下失败时的输出，原因未知；CI 上再出现就派 Windows Worker 复现。
- CI 的 sccache 以 GitHub Actions 缓存为后端，每个编译产物各占一个缓存条目：#215 的两次全量 PR CI 在 PR ref 下留下 795 个条目、8.37 GiB，fork 的 `socu/protocol-batch` 分支累计 3,434 个、9.80 GiB（2026-11-03 计数）。仓库的缓存上限是 10 GB，满了按最久未访问淘汰，几次全量 PR CI 就可能把 main 的缓存挤掉；条目这样多也没法手工清理，删除要逐条调用 API，3,434 条约占每小时额度的七成。先核实 main 与 PR 的 sccache 命中率是否因此下降，再决定怎样处理。
- [内存策略的验收记录](../acceptance/2026-09-26-memory-policy.md)里，探针退出后客户端的 backing 仍能打开；那次探针经过透明 socket 转发器，还没有做不经转发器的对照。协议批已改了 backing 的所有权（[ADR 0001](../decisions/0001-single-owner-release.md)），这一点仍待核实。

- Windows x64 上 c2-server 的 `shutdown_preserves_active_reply_refuses_new_calls_and_fences_lifecycle_hook` 偶发失败一次：run 41936786846 第一次运行（job 126947267484）读回复等了 5 秒超时，日志看不出是哪一次读；重跑失败的任务即通过。macOS 上单独跑 60 次、整套 c2-server 测试跑 15 次都没有复现。块 5b 没有改到 unary 的写端、调度与连接关闭，这个测试从 ABI 7 起就有。它守的是 shutdown 时进行中的调用照样回复，如果回复真的丢了就是产品缺陷，要在 Windows 测试机上循环复现、查清是哪一次读没等到回复。协议批修过同类的 Windows 管道问题（服务端在客户端读到确认之前就放掉管道，见[协议批验收记录](../acceptance/2026-11-03-protocol-batch-acceptance.md)）。2026-11-05 派 Worker 在 Windows 11 x64 测试机上基于 `ae4b0647` 循环复现：单独跑、与整套 c2-server 测试并行跑、再加 12 个忙循环线程，共 1,796 次（7,184 次迭代），0 次失败；加了收帧、写出与 flush 的临时观察也没有超时现场，macOS 上 201 次同样通过。CI 那次卡在哪一次读仍不确定，Windows Server 2025 上没有直接验证。同日修 DISCONNECT 的 Worker 在带修复的候选上循环 3217 次，这个超时仍出现 2 次（约 0.06%），与 DISCONNECT 不是同一个根因。2026-11-06 它在 Linux x64 上也出现一次（run 42084831707，Python 3.12 那条线），所以不是 Windows 特有的。
- Windows 上出现过一次堆损坏退出：修 DISCONNECT 的 Worker 在修复前的候选（基于 `ca405b34`）上循环 c2-server 测试时，一次进程以 0xC0000374（STATUS_HEAP_CORRUPTION）结束，没有归因。这可能是 unsafe 或 FFI 代码里的内存安全问题，要在 Windows 上复现，并用页堆（gflags 的 page heap）或 Application Verifier 定位。

## 不排期

有了使用者或测得需要时，再登记成具体条目：

- [契约发布的延后能力](contract-release-deferred-capabilities.md)里仍开着的几项：Python portable 参数形状的扩展、原始 `unsafe_buffer` 别名的访问边界、产物里的非普通文件、C++ SDK，以及 descriptor 存储、签名与信任这类下游 catalog 的事。
- 内存：buddy 中间空洞的复用；逐处复制字节的归因测量。
- 暂不暴露、有真实需要再做的写死数值（2026-11-01 清点）：连接池宽限与每个 host 的空闲连接数、relay 控制请求超时与注册重试和缓存、mesh 心跳与探测、relay 请求体上限、`c3 registry` 超时、codegen 产物上限。

## 将来

- **批量只读的无序并发原语。** 给 `@cc.read` 方法提供一次提交多组参数、按完成先后返回结果的客户端原语，让逐元素遍历（例如逐像素读 DEM）的往返互相重叠；文档里同时说明细粒度逐元素调用的代价，推荐批量读或在资源方法里就地计算。来源：协议批的配对基准显示，小调用在 CI runner 上每次约要 0.2–0.4 ms，本批又多了 40–70 μs；写路径快速通道实验只能收回其中约 9%（见[实验记录](../research/2026-11-02-ipc-write-fast-path-experiment.md)）。
- **多副本一致性**（relay mesh 原先设想的第二、三阶段）：2026-07 的草稿设想过同一资源在多个节点上的副本：路由带版本号，写成功后经 gossip 通知其他副本过期，写之前广播意图做跨节点写锁，读到过期副本时按策略处理，并提供内置的刷新方式与一致性级别。这些从没实现，现在不同节点上的同名路由彼此不知道对方。维护者 2026-10-31 认为那套做法不一定是最佳策略，留作蓝图：协议批与流式 RPC 落地之后，或者有下游需要同一资源的多个副本时，再从头讨论设计。原文见上游的[固定版本](https://github.com/world-in-progress/c-two/blob/606b8f13290c15ebff3a04a02e2ac5c6463afa7c/docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md)第二、三阶段。

## 待维护者决定

- 配置项的逐项表。
- relay 轮询的间隔（复审建议秒级，不做成配置）。
- `/_admin/unregister` 有没有人手动使用（F7）。
- `crm.client = <对象>` 与 `HeldResult(release_cb=…)` 算不算扩展点（G2）。
- `c_two.config.settings` 的直接写入是否受支持（G8）。
- R21：Resource 模板生成器输出的仍是装饰并继承 CRM 的旧模型，c2-codegen 没有等价的 Resource 骨架；修好这个工具，还是退役这项功能（约 613 行）。
- 发版方案 B 的决定 3、7。
- IPC 客户端收内联帧没有可配置的长度上限：`recv_loop` 只检查帧长不小于帧头，随后照帧头的 u32 `total_len` 分配；服务端有 `max_frame_size`，客户端没有对应的设置。对端是经身份校验的同一用户进程，2026-11-05 块 8 计划阶段才发现。要不要给客户端对称的上限，或者由握手告知，待定。
- 经 relay 的流的提前量：HTTP 不传 CREDIT，relay 只在 HTTP 库要下一段正文时才向上游要下一条，消费方停止读取后，资源还能多推进 HTTP 与 TCP 缓冲装得下的那么多条。块 8 的实测（macOS 本机回环、一跳、B=256 KiB）：64 字节条目最多 5179 步，128 KiB 条目 10 步，空条目在 20 万条内一直推进到底。第十二节第 5 条预想的是“多推进几步”。文档已照实写明，并建议需要逐步等消费方的流走直接 IPC；要不要给 HTTP 加一条按条目的需求通道，待定。
- relay 按请求的 `Accept` 头决定把调用当 unary 还是服务端流转发，它自己不知道方法的 mode。头与方法不符时（例如用 curl 调流方法），错误在派发之后才发现：流方法照 unary 转发，资源已推进一步，调用报派发不确定，relay 的上游连接因协议错误被停用，进行中的其他调用照常完成；unary 方法带流的头，unary 已执行，结果丢失，同样报派发不确定（Host 2026-11-06 用 relay 测试夹具复现）。SDK 客户端总是发对的头，不受影响。建议 IPC 16 发布之前让 OPEN 带上调用方认定的 mode，服务端按契约核对，不符时报派发前的 `UnsupportedCallMode`；这会推翻块 5a“OPEN 不带 mode”的决定（当时假定每个 IPC 客户端都知道契约的 mode，relay 不知道）。待定。
- TypeScript 的浏览器端：[终局架构](endgame-architecture.md)设想 Web 与 Node 消费同一契约，维护者 2026-11-05 认为 Web 端的做法存疑，需要讨论。若要支持，可以把 Rust 里不碰 I/O 的部分（编解码、调用终态与截止时间、路由解析）编成 wasm，fastdb4ts 已经带一个 wasm 模块；IPC 做不进 wasm。

## 顺带项

下一批碰到测试运行器时顺带做掉：

- 本地直接跑 pytest 时，TypeScript 测试要先构建 c2-mem-ffi 的 Node 绑定（`core/foundation/c2-mem-ffi/bindings/typescript` 下 `npm ci && npm run build`）。没构建时，有 2 项报错、约 30 项被跳过，看上去像回归。可以让夹具缺这个绑定时直接报出来，或者在开发文档里写明这一步。
- `tests/repo/test_python_test_runner.py::test_parent_exit_with_live_descendant_is_not_success` 在 2026-11-01 P0 验收的本机全量里失败过一次：期望"进程组仍存活"，实际得到 `[Errno 1] Operation not permitted`，单独重跑通过。进程组退出与发信号之间可能有竞态，碰到运行器时核实。
- 直接用 pytest 跑 Python 套件时，`test_typescript_persistent_ipc.py` 依赖别的测试先构建好 fastdb4ts；运行顺序不同就会报"尚未构建"。CI 的运行器会预先构建，所以只在本地出现。

## 下游跟进（由维护者决定）

- **hey-my-buddy：** 导入改到新的子模块，`ConcurrencyMode` 改为字符串。配置收敛之后，它设的 9 个 IPC 覆盖键大多会消失；流式 RPC 的块 4 把其中的 `max_total_chunks` 改名为 `max_inflight_chunk_frames`，不留别名。
- **toodle 与一个水文模拟后端：** 升级时核对导入；后者用到了 `ConcurrencyMode`，要改为字符串。
