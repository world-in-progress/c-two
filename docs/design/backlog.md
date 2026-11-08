# 待办

尚未完成的工作与待维护者决定的事项都列在这里，细节写在各自的文档里。做完的条目从这里删掉，并在验收或发行记录里注明。顺序由维护者定；建议都标明是建议。

## 接下来（建议的顺序，待维护者确认）

同一块代码里的事合成一批做，免得先做的被后面的批次推翻。

1. **路由身份**（[ADR 0004](../decisions/0004-route-name-identity.md)，2026-11-06 采用；[执行计划](2026-11-08-route-identity-plan.md) 2026-11-08 批准，进行中）：路由名加契约身份是一份资源状态的唯一身份，代理跟随路由名，同一个端点或 relay 上的重新注册对调用方透明；relay 网格内同一个名字同时只有一次在线注册，名字仍归失联的 relay，直到它回来后撤下或部署方对它强制离开，同名的多次在线注册报路由冲突（719 `RouteConflict`），由部署方注销其中一份。计划分七块：IPC 与 ABI 8 的调用标识去掉 route UID；relay 的 HTTP 调用与探测去掉 route UID，退役 704 并删除 706 与 711；代理跟随路由名，先在 Core、Python 与 Rust SDK，再在生成的 TypeScript 客户端；强制离开；网格内名字唯一；交叉验收。块 1（IPC 与 ABI 8 的调用标识）已验收，代码 `f7cb6acd`，TypeScript 金样 `7e96b64a`；块 2（relay 的 HTTP 调用与探测，退役 704、706、711）已验收，代码 `974a6883`，金样 `2151d6d5`；块 5（强制离开）已验收，代码 `674642aa`；块 3（代理跟随路由名）已验收，代码 `25bb3290`；接着做块 4。fork 上第一次全平台 CI（run 42473351631，`25a93160`，含块 1、2、5）没有通过，下面几处在本批里修：块 2 加的测试夹具在测试里现场编译一个 Rust 小程序，冷缓存的 macOS 上超过每个测试 30 秒的时限；流式块 9b4 的 relay 测试帧转发把对端重置当作错误，Linux 上偶发失败；Windows x64 上 c2-core 的 `server_streams` 测试进程以堆损坏（0xc0000374）退出，正在测试机上复现与定位；Linux 3.14t 一条是下载 FastDB SDK 超时。relay 把上游获取时的 RouteRemoved（410）、RouteClosed（503）、ContractMismatch（409）投影成没有 `dispatch_phase` 的错误体，客户端当作派发不确定，也不据此跟随，放进块 6。main 从 `a5ab5eaa`（world-in-progress/c-two#219）起包含 IPC 16 与流式块 9 测出的已知问题 D5（见[验收记录](../acceptance/2026-11-08-streaming-acceptance.md)），这一批同时解决 D5。它要在任何包含 IPC 16 的版本发布之前完成，这样调用标识的改动不必再升协议版本；它合入之前不从 main 发版。
2. **派发后的结果查询与去重**（维护者 2026-11-02 认为值得做）：事发那一刻进程内兜不住，结果未知时如实报派发不确定；这一项让调用方事后能查清一个派发不确定的调用是否执行过，或带同一个调用标识安全重试而不重复执行。业界有数据库的事务结果查询（例如 Oracle 的逻辑事务 ID）与 HTTP 幂等键，待一手来源调研。协议批之后先写设计与 ADR：调用标识跨重连与进程的稳定性、服务端结果的保留期限与存储、与有状态调用顺序的关系、与流式 RPC 的交叉。
3. **按区域的结构调整与重复校验。** 不碰协议，各区先出方案：
   - **Core 与 Python：** E1、E2、E4、E13；C5、C6；G5、G9；R06 回调重入导致自锁的风险。块 9c3 的检查在 `sdk/python/native/src/runtime_session_ffi.rs` 里又记下两类持锁碰 Python 的写法，与 9c3 修好的 `invalidate_then_release` 属同一类：在 `server_bridge` 的锁内释放 Python 对象（第 924、1287 行的 `take()`，第 1039 行替换旧值），最后一个引用的析构可能执行 Python 代码；在 host 的锁内投影 Python 异常（第 952、966 行）。只做了静态核对，还没有复现；修法同样是先在锁内取出，放开锁之后再碰 Python。
   - **本地端点：** tmpfiles（D-tmp）先在 Linux 上复现 systemd-tmpfiles 清掉 gate 与 marker 的情形，再实施目录 flock（本机与 Windows 测试机都不是 Linux）；之后 D1、D2、D5、D6、D10 的复查只留在所有权、发布与 unlink 边界；R17 剩下的 gate/context 与目录 pin 的错误分类。顺带：c2-local 的 Drop 在门锁短暂争用后由后台线程完成退役，`managed_drop_after_gate_contention_settles_ownership` 两种终态都接受，要有一个能观察后台重试的点才能确定地守住。另一个端点测试 `final_lease_stat_io_preserves_stale_bind_reap_and_sweep` 在 2026-11-06 的 Linux CI（fork run 42254418051 第一次尝试）里偶发失败一次：注入 `FinalLeaseStat` 的那次操作之后，紧接着的 `reap_managed_at` 没有得到 `Reaped`；是不是同一类后台退役时序还没核实，碰到这块时一并查。
   - **relay：** Runtime 客户端连接池（`core/transport/c2-ipc/src/pool.rs`）与 relay 的 `ConnectionPool`（`core/transport/c2-http/src/relay/conn_pool.rs`）各自实现闲置驱逐与同一端点并发建连的合并（块 9b11、9b13 之后两边行为一致），合成一个机制。F2（删 watch 闸门，轮询降到秒级）、F3、F6；F4、F8、F10；F7 剩下的 `/_admin/unregister` 与注销 202/204/404 的区分，等维护者答复。`relay/conn_pool.rs` 为替代 P4 删掉的 `force_connected`，在生产代码里加了只在测试构建生效的连通状态覆盖（`test_assume_connected` 等），看能否改用真实连接，移出生产路径。`c3 relay --upstream` 只在 relay 启动时登记一次：路由那时还没开放就报 "does not export route"，`c3 relay` 直接退出；登记成功之后，上游重启（身份改变）或路由被注销时，relay 会把它撤下，也不会再自己重新接手，只能重启 relay 或手工 `POST /_register` 补上。维护者 2026-11-01 要求记入：改为启动后持续重试、路由开放后接手、上游重启后重新接手；待定的是重试的节奏与上限、启动时登记失败是否仍让 `c3 relay` 退出、以及与资源进程经 relay anchor 自行登记同一路由时谁优先。
   - **服务端回调面：** `CrmCallback` 与流回调都收一个 `ResponsePool`，生产实现（`core/runtime/c2-core/src/host.rs`）全部不用，流的 SHM 编码也由服务端自己取池；内存策略文档仍承诺它是低层回调直接填充响应的扩展点。先裁决是否保留这个外部扩展点，撤下就一并收回参数、公开 helper 与 `ShmAlloc` 分支。排在流式块 7、8 之后（2026-11-05 架构审查）。
   - **契约、代码生成与 CLI：** H1、H4、H6、H8（用校验过的构造器加私有字段，取代 C01）、H9；R29 等剩下的 codegen 同次缓存与 typed projection。
4. **配置收敛**（[接口审查](2026-10-31-api-surface-review.md)第三节）：逐项表定下之后进行。与 B8、H3、H7、H12、C4 交叉，R09、R10 的 EnvCatalog 多次加载一并处理。`chunk_threshold_ratio`（`C2_IPC_CHUNK_THRESHOLD_RATIO`）已证实没有作用：只在 c2-config 里解析与校验，c2-ipc 只在比较两份客户端池配置时用到它，分片判断不读它；连同环境变量、Python 字段与测试一起删除。
5. **错误类型**（接口审查第四节）：协议批之后。R32、R33 等剩下的结构化错误内部序列化、Python 错误常量与孤立的 `wrap_error`。
6. **TypeScript 与 Node：** 流式块 7 已让 Node 经 C/FFI ABI 8 调用 Rust Core 的客户端，R16 剩下的 Node→Core 投影迁移随之完成；还没做的是解码不透明的响应 allocator。2026-11-05 的架构审查补了两项，都排在流式块 8 之后：一是生成的 TypeScript 文件内嵌整份 transport，契约不变也要随 IPC 或 ABI 升级重新生成，应把运行时拆成独立的 `sdk/typescript` 包，生成器只输出契约胶水；二是 `core/foundation/c2-mem-ffi` 名为 foundation 层，块 7 之后实际依赖 c2-core、c2-ipc、c2-contract 与 c2-error，工作区里没有别的 crate 依赖它，Python 也不用它，只服务 Node，应把它的原生库与 TypeScript 绑定一起归入 `sdk/typescript`，按客户端的职责改名，仍出一个动态库，同步 loader、C 头文件、npm 包与 CI 路径。原流式块 8b（生成的 TypeScript 经 relay 的 HTTP 流）2026-11-05 经维护者同意移到这一批，免得先在生成的 transport 里手写一遍、重构时再改；在此之前，生成的 TypeScript 客户端经 relay 的 HTTP 调用流方法，照旧在派发前报 `UnsupportedCallMode`，经直接 IPC 照常可用。Node 作为后端客户端维持（维护者 2026-11-05）；Node 能加载原生插件，它的 HTTP 路径也可以像 IPC 一样经 ABI 8 走 Rust 客户端，TypeScript 里不再手写这套机制，这是 Host 的推论，留到这一批设计时定。
7. **发版工具，方案 B**（2026-11-05 架构审查补充的先后：先把仍在用的两种回执迁到 `tools/testing`（`tools/dev/test_python.py` 与正式矩阵还在调用），把 Unix 跨 UID 验证并入 Source Validation，再删旧候选三件套、它们的自测与 `local-endpoint-validation.yml`；那个工作流的理由还写着已由 ADR 0002 退役的 owner 机制）（[发版敏捷性审查](2026-10-30-release-agility-review.md)第三、四节）：与产品代码无关，可以随时插入。B1 候选重整、B2 删除约 8 千行（`tools/local_rc` 与 `local-endpoint-validation.yml` 仍在）、B3 FastDB 版本只写一处、B5 精简各条线的 gate、B6 修 `PROBE_BUDGET` 的偶发失败。顺带修配对基准的两处：收据只给 p99 的比值，而流式设计第十二节第 7 条的门槛用 p95，补上 p95；`run_matrix` 在行控制器退出时若看到 Windows Job 仍计着一个活进程，就把这一行报成超过等待时限并改写真实退出码，也不记下剩下的是哪个进程（流式 run 42349023784 的 Windows x64 作业出现过一次，那一行 200 次调用全部成功、清理完整），要记下这个进程并如实报告。
8. **路线图上的其余产品能力**（完成标准见[路线图](roadmap.zh-CN.md#产品能力)）：官方不可变的包分发、契约兼容、调用元数据与准入钩子、dry-run、async unary、遥测与背压、可发布的 TypeScript SDK、发现与命名空间治理。顺序待维护者定。
9. **文档里的协作流程用词**（2026-11-04 维护者同意）：设计文档里用 Host、Worker、微任务、看板、委派块、任务书指协作流程，约 60 处，分布在 9 份设计文档里；仓库外的读者不知道它们指谁，Host 还与术语表里的 Host 撞名，worker 与执行 worker 撞名。改为不写角色，例如“Host 建议”只写“建议”。指拍板者的“用户”改为“维护者”，约 120 处；“开发者”在这里指 C-Two 的使用者，不用来指维护者。冻结的验收记录不改。
10. **下一次发版时：** 根 README、SDK README 与 `cli/README.md` 里的版本说明（IPC 14、SHM 3、ABI 6）改为发版时的版本，现在是 IPC 16、SHM 4、C/FFI ABI 8 与 mesh 7，路由身份那一批若升 mesh 版本以它为准；`CHANGELOG.md` 的 [Unreleased] 里 IPC 15 与 IPC 16 两条协议改动合成一条，从 0.10.1 的版本直接写到发版时的版本。

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
- Python 客户端每次调用都执行 `inspect.Signature.bind` 与 `apply_defaults`（`sdk/python/src/c_two/crm/transferable.py` 的调用包装），本机约 2 µs，约占一次 1 字节往返的 2.5%；分支起点 `d772ca3a` 就有，与流式的小调用退化无关。
- 暂不暴露、有真实需要再做的写死数值（2026-11-01 清点）：连接池宽限与每个 host 的空闲连接数、relay 控制请求超时与注册重试和缓存、mesh 心跳与探测、relay 请求体上限、`c3 registry` 超时、codegen 产物上限。

## 将来

- **批量只读的无序并发原语。** 给 `@cc.read` 方法提供一次提交多组参数、按完成先后返回结果的客户端原语，让逐元素遍历（例如逐像素读 DEM）的往返互相重叠；文档里同时说明细粒度逐元素调用的代价，推荐批量读或在资源方法里就地计算。来源：协议批的配对基准显示，小调用在 CI runner 上每次约要 0.2–0.4 ms，本批又多了 40–70 μs；写路径快速通道实验只能收回其中约 9%（见[实验记录](../research/2026-11-02-ipc-write-fast-path-experiment.md)）。

## 待维护者决定

- 配置项的逐项表。
- relay 轮询的间隔（复审建议秒级，不做成配置）。
- `/_admin/unregister` 有没有人手动使用（F7）。路由身份计划的决定 D（2026-11-08 批准）把它定为 Host 崩溃之后部署方在原 relay 上撤下名字的途径，它因此保留；第 3 项里注销 202/204/404 的区分仍待答复。
- `crm.client = <对象>` 与 `HeldResult(release_cb=…)` 算不算扩展点（G2）。
- `c_two.config.settings` 的直接写入是否受支持（G8）。
- R21：Resource 模板生成器输出的仍是装饰并继承 CRM 的旧模型，c2-codegen 没有等价的 Resource 骨架；修好这个工具，还是退役这项功能（约 613 行）。
- 发版方案 B 的决定 3、7。
- IPC 客户端收内联帧没有可配置的长度上限：`recv_loop` 只检查帧长不小于帧头，随后照帧头的 u32 `total_len` 分配；服务端有 `max_frame_size`，客户端没有对应的设置。对端是经身份校验的同一用户进程，2026-11-05 块 8 计划阶段才发现。要不要给客户端对称的上限，或者由握手告知，待定。
- 经 relay 的流的提前量：HTTP 不传 CREDIT，relay 只在 HTTP 库要下一段正文时才向上游要下一条，消费方停止读取后，资源还能多推进 HTTP 与 TCP 缓冲装得下的那么多条。块 8 的实测（macOS 本机回环、一跳、B=256 KiB）：64 字节条目最多 5179 步，128 KiB 条目 10 步，空条目在 20 万条内一直推进到底。第十二节第 5 条预想的是“多推进几步”。文档已照实写明，并建议需要逐步等消费方的流走直接 IPC；要不要给 HTTP 加一条按条目的需求通道，待定。
- TypeScript 的浏览器端：[终局架构](endgame-architecture.md)设想 Web 与 Node 消费同一契约，维护者 2026-11-05 认为 Web 端的做法存疑，需要讨论。若要支持，可以把 Rust 里不碰 I/O 的部分（编解码、调用终态与截止时间、路由解析）编成 wasm，fastdb4ts 已经带一个 wasm 模块；IPC 做不进 wasm。
- Host 关闭返回时，没有调用、没有借出的已接受连接可能还没关完：块 9b17 在受控复现里看到 shutdown 无错误、`finished` 为真，约 97 ms 后这条连接才发布 `Closed`（`server_shutdown`）。现行文档只把监听、调用与借出算进关闭完成，所以测试改为等事件到齐。要不要让 shutdown 也等空闲连接关完，待定。

## 下游跟进（由维护者决定）

- **hey-my-buddy：** 导入改到新的子模块，`ConcurrencyMode` 改为字符串。配置收敛之后，它设的 9 个 IPC 覆盖键大多会消失；流式 RPC 的块 4 把其中的 `max_total_chunks` 改名为 `max_inflight_chunk_frames`，不留别名。
- **toodle 与一个水文模拟后端：** 升级时核对导入；后者用到了 `ConcurrencyMode`，要改为字符串。
