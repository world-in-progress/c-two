# 待办

尚未完成的工作与待用户决定的事项都列在这里，细节写在各自的文档里。做完的条目从这里删掉，并在验收或发行记录里注明。顺序由用户定；Host 的建议标明是建议。

## 进行中

- **发版前的软工作**（用户 2026-11-03 定：先不发版）：在 main（`d772ca3a`）上补跑配对基准；盘点公开接口与各类文档的出入并修正（README、参考页、发行说明、示例）；路线图只写方向与完成标准，顺序并进本待办。

## 接下来（Host 建议的顺序，待用户确认）

同一块代码里的事合成一批做，免得先做的被后面的批次推翻。

1. **流式 RPC**（[路线图](roadmap.zh-CN.md#产品能力)上的产品能力，用户确认是必备能力）：排在协议批之后，设计要回答单一属主评估"与流式 RPC 的关系"一节的八个问题，并定下同一连接内调用的顺序契约。现状（2026-11-01 核实，写在[运行时生命周期](../reference/runtime-lifecycle.md)）：非分片的 IPC 调用在接收循环里按帧序预约路由，`exclusive` 与 `read_parallel` 下后到的调用不会先于先到的写执行；`parallel` 与不同路由之间没有顺序；分片请求的首片在另一个任务里才预约，后到的非分片调用可能先执行。旧 Python 服务端的连接级写屏障连跨路由也保证顺序，Rust 里没有。一并设计：分片的结构调整（B2：先缩小重组池锁，再把首片准入搬回接收循环、删排序闸门，分片请求也就按帧序预约了；以及 C03、C04、R14 的剩余；以及分片拒绝修复留下的两点：已结算记录在 `chunk_assembler_timeout` 后被 GC 删除，延迟到那之后才到的续片会再引来一次错误回复，Node 客户端按 request id 严格匹配，可能因此让下一次调用失败，要根治需要协议层的请求生命周期，独立审查确认恢复代际守卫解决不了；`server.rs` 里一个正常状态机走不到的 Pending 返回分支）；relay 的 HTTP 响应现在整体物化，改为流式发送；HTTP 载荷的内存预算与消费生命周期（[提案](2026-10-06-http-memory-budget.md)，未实施）。
   - 待定：`max_total_chunks` 一个值管两件事：服务端同时处理的分片帧数（拿不到名额的分片让它的请求失败；首片准入门的条目数也由它限住。门上的容量检查并不多余：首片任务归还名额之后、拆除围栏还没放开的短暂窗口里可能触发，`chunk_ordering.rs` 模块注释说它"只是重复不变量"不对），以及重组表提前清扫的触发点（从不拒绝）。原先这里记成"只是软触发、从不拒绝"，漏看了服务端的名额（2026-11-03 核实）。名字说的是分片总数，与两种用途都不完全相符；改名还是拆成两项，以及那段模块注释，随分片的结构调整（B2）一起定。
2. **派发后的结果查询与去重**（用户 2026-11-02 认为值得做）：事发那一刻进程内兜不住，结果未知时如实报派发不确定；这一项让调用方事后能查清一个派发不确定的调用是否执行过，或带同一个调用标识安全重试而不重复执行。业界有数据库的事务结果查询（例如 Oracle 的逻辑事务 ID）与 HTTP 幂等键，待一手来源调研。协议批之后先写设计与 ADR：调用标识跨重连与进程的稳定性、服务端结果的保留期限与存储、与有状态调用顺序的关系、与流式 RPC 的交叉。
3. **按区域的结构调整与重复校验。** 不碰协议，各区先出方案：
   - **Core 与 Python：** E1、E2、E4、E13；C5、C6；G5、G9；R06 回调重入导致自锁的风险。顺带：`register` 的签名写 `Literal['materialized', 'borrowed']`，服务端桥里另有私有别名 `_InputLifetime`，统一成一处。
   - **本地端点：** tmpfiles（D-tmp）先在 Linux 上复现 systemd-tmpfiles 清掉 gate 与 marker 的情形，再实施目录 flock（本机与 Windows 测试机都不是 Linux）；之后 D1、D2、D5、D6、D10 的复查只留在所有权、发布与 unlink 边界；R17 剩下的 gate/context 与目录 pin 的错误分类。顺带：c2-local 的 Drop 在门锁短暂争用后由后台线程完成退役，`managed_drop_after_gate_contention_settles_ownership` 两种终态都接受，要有一个能观察后台重试的点才能确定地守住。
   - **relay：** F2（删 watch 闸门，轮询降到秒级）、F3、F6；F4、F8、F10；relay-aware 调用没有每次调用的路由回执，`observed_route` 仍是连接时的值；F7 剩下的 `/_admin/unregister` 与注销 202/204/404 的区分，等用户答复。顺带：`LocalEndpointNamespaceRequired` 的消息写成 "the resource namespace"，应为资源一方的本地端点命名空间。`relay/conn_pool.rs` 为替代 P4 删掉的 `force_connected`，在生产代码里加了只在测试构建生效的连通状态覆盖（`test_assume_connected` 等），看能否改用真实连接，移出生产路径。`c3 relay --upstream` 只在 relay 启动时登记一次：路由那时还没开放就报 "does not export route"，`c3 relay` 直接退出；登记成功之后，上游重启（身份改变）或路由被注销时，relay 会把它撤下，也不会再自己重新接手，只能重启 relay 或手工 `POST /_register` 补上。用户 2026-11-01 要求记入：改为启动后持续重试、路由开放后接手、上游重启后重新接手；待定的是重试的节奏与上限、启动时登记失败是否仍让 `c3 relay` 退出、以及与资源进程经 relay anchor 自行登记同一路由时谁优先。
   - **契约、代码生成与 CLI：** H1、H4、H6、H8（用校验过的构造器加私有字段，取代 C01）、H9；R29 等剩下的 codegen 同次缓存与 typed projection。
4. **配置收敛**（[接口审查](2026-10-31-api-surface-review.md)第三节）：逐项表定下之后进行。与 B8、H3、H7、H12、C4 交叉，R09、R10 的 EnvCatalog 多次加载一并处理。`chunk_threshold_ratio`（`C2_IPC_CHUNK_THRESHOLD_RATIO`）已证实没有作用：只在 c2-config 里解析与校验，c2-ipc 只在比较两份客户端池配置时用到它，分片判断不读它；连同环境变量、Python 字段与测试一起删除。
5. **错误类型**（接口审查第四节）：协议批之后。一并：删除从没产生过的错误码 706 `WriteConflict`，连同 Python 的错误类、映射与相关测试（用户 2026-10-31 同意；704 是还在使用的 `RouteStale`，不动）；711 `RouteCatalogCompacted` 在协议批删掉 IPC 路由目录之后也没有生产者了（2026-11-03 核实，只剩注册表、Python 错误类、映射与 `__all__`），Host 建议一并删除；`sdk/python/src/c_two/error.py` 里重复注册错误的 docstring 写成 resource name，改为 route name；TypeScript 公共错误暴露 `pre_dispatch` 与 `dispatch_uncertain` 的区分；R32、R33 等剩下的结构化错误内部序列化、Python 错误常量与孤立的 `wrap_error`。
6. **TypeScript 与 Node：** R16 剩下的完整 Node→Core 投影迁移；解码不透明的响应 allocator。
7. **发版工具，方案 B**（[发版敏捷性审查](2026-10-30-release-agility-review.md)第三、四节）：与产品代码无关，可以随时插入。B1 候选重整、B2 删除约 8 千行（`tools/local_rc` 与 `local-endpoint-validation.yml` 仍在）、B3 FastDB 版本只写一处、B5 精简各条线的 gate、B6 修 `PROBE_BUDGET` 的偶发失败。
8. **路线图上的其余产品能力**（完成标准见[路线图](roadmap.zh-CN.md#产品能力)）：官方不可变的包分发、契约兼容、调用元数据与准入钩子、dry-run、async unary、遥测与背压、可发布的 TypeScript SDK、发现与命名空间治理。顺序待用户定。
9. **下一次发版时：** 根 README、SDK README 与 `cli/README.md` 里的版本说明（IPC 14、SHM 3、ABI 6）改为 IPC 15、SHM 4、ABI 7、mesh 7。

各批一并删除只钉源码文本或已删名字的测试（复审第六节）；复审附录里各区未裁决的待议项，实施到对应区域时再定。[瘦身账本](2026-10-13-local-slimming-batches.md)暂缓表与条目边界表里的其余各项仍按账本写明的条件。

## 要核实的事

- CI 的 sccache 以 GitHub Actions 缓存为后端，每个编译产物各占一个缓存条目：#215 的两次全量 PR CI 在 PR ref 下留下 795 个条目、8.37 GiB，fork 的 `socu/protocol-batch` 分支累计 3,434 个、9.80 GiB（2026-11-03 计数）。仓库的缓存上限是 10 GB，满了按最久未访问淘汰，几次全量 PR CI 就可能把 main 的缓存挤掉；条目这样多也没法手工清理，删除要逐条调用 API，3,434 条约占每小时额度的七成。先核实 main 与 PR 的 sccache 命中率是否因此下降，再决定怎样处理。
- [内存策略的验收记录](../acceptance/2026-09-26-memory-policy.md)里，探针退出后客户端的 backing 仍能打开；那次探针经过透明 socket 转发器，还没有做不经转发器的对照。协议批已改了 backing 的所有权（[ADR 0001](../decisions/0001-single-owner-release.md)），这一点仍待核实。

## 不排期

有了使用者或测得需要时，再登记成具体条目：

- [契约发布的延后能力](contract-release-deferred-capabilities.md)里仍开着的几项：Python portable 参数形状的扩展、原始 `unsafe_buffer` 别名的访问边界、产物里的非普通文件、C++ SDK，以及 descriptor 存储、签名与信任这类下游 catalog 的事。
- 内存：buddy 中间空洞的复用；逐处复制字节的归因测量。
- 暂不暴露、有真实需要再做的写死数值（2026-11-01 清点）：连接池宽限与每个 host 的空闲连接数、relay 控制请求超时与注册重试和缓存、mesh 心跳与探测、relay 请求体上限、`c3 registry` 超时、codegen 产物上限。

## 将来

- **批量只读的无序并发原语。** 给 `@cc.read` 方法提供一次提交多组参数、按完成先后返回结果的客户端原语，让逐元素遍历（例如逐像素读 DEM）的往返互相重叠；文档里同时说明细粒度逐元素调用的代价，推荐批量读或在资源方法里就地计算。来源：协议批的配对基准显示，小调用在 CI runner 上每次约要 0.2–0.4 ms，本批又多了 40–70 μs；写路径快速通道实验只能收回其中约 9%（见[实验记录](../research/2026-11-02-ipc-write-fast-path-experiment.md)）。
- **多副本一致性**（relay mesh 原先设想的第二、三阶段）：2026-07 的草稿设想过同一资源在多个节点上的副本：路由带版本号，写成功后经 gossip 通知其他副本过期，写之前广播意图做跨节点写锁，读到过期副本时按策略处理，并提供内置的刷新方式与一致性级别。这些从没实现，现在不同节点上的同名路由彼此不知道对方。用户 2026-10-31 认为那套做法不一定是最佳策略，留作蓝图：协议批与流式 RPC 落地之后，或者有下游需要同一资源的多个副本时，再从头讨论设计。原文见上游的[固定版本](https://github.com/world-in-progress/c-two/blob/606b8f13290c15ebff3a04a02e2ac5c6463afa7c/docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md)第二、三阶段。

## 待用户决定

- 下一批的顺序：流式 RPC 是否先于路线图上的 async unary 与遥测和背压（流式第三问要求处理函数等输入时不占执行线程，Host 推论这在 Python 里离不开 async 或回调式的处理函数），以及路线图上其余产品能力的先后。
- 配置项的逐项表。
- relay 轮询的间隔（复审建议秒级，不做成配置）。
- `/_admin/unregister` 有没有人手动使用（F7）。
- `crm.client = <对象>` 与 `HeldResult(release_cb=…)` 算不算扩展点（G2）。
- `c_two.config.settings` 的直接写入是否受支持（G8）。
- R21：Resource 模板生成器输出的仍是装饰并继承 CRM 的旧模型，c2-codegen 没有等价的 Resource 骨架；修好这个工具，还是退役这项功能（约 613 行）。
- 发版方案 B 的决定 3、7。

## 顺带项

下一批碰到测试运行器时顺带做掉：

- 本地直接跑 pytest 时，TypeScript 测试要先构建 c2-mem-ffi 的 Node 绑定（`core/foundation/c2-mem-ffi/bindings/typescript` 下 `npm ci && npm run build`）。没构建时，有 2 项报错、约 30 项被跳过，看上去像回归。可以让夹具缺这个绑定时直接报出来，或者在开发文档里写明这一步。
- `tests/repo/test_python_test_runner.py::test_parent_exit_with_live_descendant_is_not_success` 在 2026-11-01 P0 验收的本机全量里失败过一次：期望"进程组仍存活"，实际得到 `[Errno 1] Operation not permitted`，单独重跑通过。进程组退出与发信号之间可能有竞态，碰到运行器时核实。
- c2-ipc 判定零借出 ID 时匹配 wire 解码错误里的字段名 `"loan id"`，应改为 c2-wire 的专用错误类型。
- c2-mem-ffi 连接入口的 Rust 文档注释与 C 头文件对身份校验失败的状态码写法不一致。
- c2-wire 里给已接纳调用的回复也走 `WriteRequest::Control`（F3 以此保证停用后仍可回复），名字与"控制帧"的本义不符；结构调整时考虑分出"已接纳调用的帧"。
- 直接用 pytest 跑 Python 套件时，`test_typescript_persistent_ipc.py` 依赖别的测试先构建好 fastdb4ts；运行顺序不同就会报"尚未构建"。CI 的运行器会预先构建，所以只在本地出现。
- c2-server 在构造时预热响应池，启动时（包括重启）又预热一次；启动时那一次已经够用，构造时的可以删掉。
- rustc 1.99 把 `AtomicU64::fetch_update` 标为弃用（改名 `try_update`），c2-mem 与 c2-local 的干净构建各出一条警告；改用 `try_update` 之前先确认它在最低支持的 Rust 1.95 上可用。

## 下游跟进（由用户决定）

- **hey-my-buddy：** 导入改到新的子模块，`ConcurrencyMode` 改为字符串。配置收敛之后，它设的 9 个 IPC 覆盖键大多会消失。
- **toodle 与一个水文模拟后端：** 升级时核对导入；后者用到了 `ConcurrencyMode`，要改为字符串。
