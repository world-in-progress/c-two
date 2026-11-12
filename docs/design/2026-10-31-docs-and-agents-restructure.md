# 文档与 AGENTS.md 重组的执行计划

日期：2026-10-31。输入：分支 `socu/over-defense-batches`（`c4c53c08`）。依据：[接口审查](2026-10-31-api-surface-review.md)第五节用户已同意的 AGENTS.md 重组；用户 2026-10-31 同意先瘦 AGENTS.md、再按 reference、decisions、design、acceptance 与 research 的分法整理 `docs/`，两步都叠在 `socu/over-defense-batches` 上做，上游另开 PR，暂不建嵌套的 AGENTS.md。状态：两步都已完成并验收，第一步合入为 `a86403af`，第二步合入为 `143c11e3`；`CONTEXT.md` 术语表用户 2026-10-31 确认，同一次提交统一了现行文档的用词。fork 全量 CI [41218012397](https://github.com/Dsssyc/c-two/actions/runs/41218012397)（`08b8c65c`）在 Linux、macOS、Windows x64 与 ARM64 上通过。第五节第 1、3 项用户已于 2026-10-31 确认。

## 一、量过的问题

- **根 AGENTS.md 太大。** 526 行、64,905 字节。Codex 只加载从项目根到当前目录这一路上的 AGENTS.md，合计上限是 `project_doc_max_bytes`，默认 32 KiB，超出的部分截断，只记一条警告（openai/codex main `67f0994b53` 的 `codex-rs/core/src/agents_md.rs`，官方文档的说法相同）。在仓库根启动的 Codex 因此读不到第 32 KiB 之后的内容：Rust Native Layer 的大部分、CLI、Key Conventions（FastDB 写法、hold、视图生命周期、错误、命名、性能）、环境变量表、Python 版本，以及全部 Agent Working Rules。截断是读源码和文档得出的，没有起会话实测。Claude Code（官方文档，v2.1.277 起）在没有 CLAUDE.md 时读 AGENTS.md（本仓库的会话里整份载入），子目录里的 AGENTS.md 要等它打开该目录的文件时才加载，并建议单个文件少于 200 行。
- **机制细节只在 AGENTS.md 里。** `open_peer`、`allocation_id`、`ServerStartupTimeout`、`ExpectedRouteContract`、错误码分类等，在现有参考文档里找不到；环境变量表则与 `configuration.md` 重复。
- **`docs/` 没有索引，现行文档与历史记录混放。** 490 个受版本管理的文件、约 11 MB：顶层 18 个文件，加 `designs`、`images`、`issues`、`logs`、`plans`、`releases`、`reports`、`reviews`、`spikes`、`superpowers`、`vision` 十一个目录。没有术语表。
- **双语文档有两套命名。** `configuration`、`local-endpoint-lifecycle`、`memory-policy` 是 `X.md`（中文）配 `X.en.md`；README 与路线图是 `X.md`（英文）配 `X.zh-CN.md`。`ipc-handshake.md` 只有中文。
- **被代码、测试或入口文档引用的。** `reports/evidence/`（124 个，AGENTS.md 引用；代码与测试都不按路径读它）；`releases/`（CHANGELOG、各 README、`tests/repo/test_ci_scope.py`）；`images/`（各 README、`tools/dev/generate_banner.py`）；`issues/` 与 `roadmap.md`（`core/runtime/c2-core/tests/error_normalization.rs` 扫描它们，查一个已删除的 crate 名）；两份 0.7.4 报告（`sdk/python/tests/README.md`）。

对照的做法：AGENTS.md 只写不变量、布局、验证与文档规则；`docs/README.md` 是索引；文档按 `reference`、`decisions`、`design`、`acceptance`、`research` 分放；根目录有术语表 `CONTEXT.md`；文档卫生检查要求相对链接能解析、不出现本机路径。

## 二、目标结构

| 位置 | 内容 |
| --- | --- |
| `AGENTS.md` | 架构地图、构建与测试命令、跨模块不变量、已退役接口的短清单、文档规则、工作规则、路由表；不超过 32 KiB，目标 24 KiB 以内 |
| `CONTEXT.md` | 术语表，中英成对 |
| `docs/README.md` | 索引：每个主题由哪份文档负责；自身不陈述行为 |
| `docs/reference/` | 现行行为与契约。现有的配置、IPC 握手、本地端点生命周期、内存策略、Python 用法、Windows、CI、开发与本地测试文档，加上从 AGENTS.md 挪出的机制说明 |
| `docs/decisions/` | ADR，一个决定一份，以后不改写，只追加带日期的修订。从下一个决定（单一属主释放）开始，不补写旧决定 |
| `docs/design/` | 待办、路线图、进行中的计划与评估、仍然作数的远景文档 |
| `docs/acceptance/` | 验收记录与证据：现在的 `reports/`（含 `evidence/`）与 `reviews/` |
| `docs/research/` | 决定之前的调研笔记 |
| `docs/releases/`、`docs/assets/` | 发行说明保持原位；`images/` 改名为 `assets/` |

嵌套的 AGENTS.md 先不建：Codex 不会自动读当前目录以下的文件，根文件里还是要写明先读哪份，模块的事实就会有两处要同步。拆完以后，某个子树有自己的工具链和规则、根文件放不下时，再单独为它建。

## 三、第一步：AGENTS.md 瘦身

### 根文件留下的

1. 0.x 约束、项目概述（CRM、Resource、Client）；
2. 架构地图：crate 表，每层一句话说明归属；
3. 构建、测试与运行命令；
4. 跨模块的不变量，每条一句：Rust core 拥有通用运行时机制，SDK 只是外观；直接 IPC 不依赖 relay；解析与调用按路由加契约身份；Core 拥有截止时间、就绪与生命周期；Python 不保留 Rust 权威的第二份状态；请求快照与传输租约的顺序，以及不多拷贝；`c2-core` 不依赖 FastDB；同进程调用直接传 Python 对象；
5. 已退役接口：合成一张表，旧名字对应现在的做法；
6. 文档规则与路由表（措辞见下）；
7. Agent Working Rules，原样保留。

### 各节的去处

一事一处：先合并进已有的文档，没有才新建。下表是暂定的，实施方核对已有文档的覆盖面后可以调整，调整写进报告。

| 现在的小节 | 去处 |
| --- | --- |
| Local Release-Candidate Evidence | 已有的验收记录；根文件只留一句链接 |
| CRM Layer、契约与路由相关的 Transport 段落（契约身份、relay 解析、relay 发现的 IPC 身份校验、直接 IPC） | 新建 `reference/contracts-and-routing.md` |
| Config Layer、Environment Variables | `reference/configuration.md`；核对两份表一致后，删掉 AGENTS.md 里的副本 |
| Transport Layer 的会话、注册、启动、关闭、退役与连接截止时间 | 新建 `reference/runtime-lifecycle.md` |
| Rust Native Layer 的内存子系统 | 合并进 `reference/memory-policy.md` |
| Rust Native Layer 的本地端点、凭据与所有者控制 | 合并进 `reference/local-endpoint-lifecycle.md` |
| c2-mem-ffi ABI | 新建 `reference/c-abi.md` |
| CLI 选项表 | `cli/README.md`（已有 relay 选项） |
| FastDB CRM Pattern、Hold Mode、FastDB View Lifetime Boundary、Performance-Sensitive Code 的分派路径 | 新建 `reference/payload-lifetime.md` |
| Error Handling | 新建 `reference/errors.md` |
| SOTA API Pattern、Import Style、Naming | 用法进 `reference/python-usage.md`；根文件只留导入写法与命名各一句 |
| Python Version | 根文件一句 |

### 新规则的措辞（待用户确认）

> **Instruction size.** Keep this file within 32 KiB: Codex reads at most `project_doc_max_bytes` (32 KiB by default) of the AGENTS.md files from the project root to its working directory and drops the rest. `tests/repo/test_docs_hygiene.py` fails above the limit. Mechanism detail belongs in `docs/reference/`, routed from the table below.

> **Read before changing.** Before changing files under a path in this table, read the documents it names; they own that mechanism and its invariants. Update the owning document in the same change as the behavior.

> **Documentation layout.** `docs/README.md` routes each subject to the document that owns it. `docs/reference/` states current behavior; `docs/decisions/` holds one ADR per decision, amended with dated notes rather than rewritten; `docs/design/` holds the backlog, the roadmap and plans in progress; `docs/acceptance/` holds verification records and evidence; `docs/research/` holds notes made before a decision. Before deleting a superseded document, move what still holds into its owning page. Name locations with `~` or a placeholder, never a machine's home directory.

### 检查

- `tests/repo/test_docs_hygiene.py` 新增两项：根 AGENTS.md 不超过 32,768 字节；AGENTS.md、README 与 `docs/reference/` 中的相对链接都能解析。
- 另请一名审查者独立核对：旧 AGENTS.md 的每一条规则都在新的根文件或某份参考文档里有着落，没有丢失，也没有前后矛盾。
- `uv run pytest tests/repo -q`。

## 四、第二步：docs 重组

### 盘点

只读，分批进行，每批产出一张表：每份文档归入现行参考、仍有效的决定理由、未完成的工作、验收记录、已过时五类之一；对前三类，写明仍然成立的内容和去处。按风险分档，不逐篇细读：

| 档 | 范围 | 做法 |
| --- | --- | --- |
| 细读 | `superpowers/specs`（39 个，661 KiB）、`vision/`、`issues/`、`designs/`、顶层的 `architecture-review-v030.md` 与 `memory-pressure-backpressure.md` | 逐篇读，找还没写进现行文档的设计理由与未完成的事项 |
| 读状态 | `plans/`（73 个；2026-10 之前 34 个）、`superpowers/plans` 与 `superpowers/reports` | 读标题、状态与结尾的遗留项；遗留项进待办 |
| 按类处理 | `logs/`、`spikes/` | 抽查是否有被引用或未记录的结论，没有就整批删除 |
| 整批迁移 | `reports/`（含 `evidence/`）、`reviews/` | 挪进 `acceptance/`，不删；只剔除明显的临时文件 |

### 第一步留下的事

- `tests/repo/test_docs_hygiene.py` 手写了 Markdown 链接解析；把检查扩到全部 Markdown 时，改用现成的解析库（例如 markdown-it-py）。
- 工作规则里写着待办在 `docs/backlog.md`，文档规则却说它在 `docs/design/`；移动待办时把工作规则一并改掉。

### 处置规则

- 仍然成立的内容，先挪进它的去处（参考文档、ADR 或待办），再删原文件；git 历史保留原文。
- 未完成的事项一律进 `docs/design/backlog.md`；`issues/` 里仍然有效的说明作为待办条目的细节文档，挪进 `design/`。
- 有链接指向的文档，删除或移动时一并改链接：README、CHANGELOG、各 SDK 的 README、测试与代码。
- 各目录自己的 README（`plans/`、`spikes/`、`reviews/`、`vision/`、`logs/`）由 `docs/README.md` 取代。
- `error_normalization.rs` 里扫描文档查旧 crate 名的测试，只钉一个已删除的名字，随重组删除。

### 实施

1. 按盘点结果移动、合并与删除，改写所有链接；
2. 写 `docs/README.md` 索引；
3. 按第五节的决定统一双语命名；
4. 起草 `CONTEXT.md` 术语表，用户确认后提交，再统一代码、文档与界面里的用词；
5. 文档卫生检查扩展到全部受版本管理的 Markdown：相对链接能解析；任何受版本管理的文件都不出现运行检查的那台机器的主目录路径。

### 检查

- `uv run pytest tests/repo -q`；`cargo test --manifest-path core/Cargo.toml -p c2-core --test error_normalization`。
- 链接检查覆盖全部 Markdown，零失败。
- 抽查五份被删文档，确认其中仍然成立的内容都已有去处。

## 五、待用户决定

1. **参考文档的语言（用户 2026-10-31 已确认）。** Host 建议：面向使用者的指南（配置、Python 用法、Windows 用法）保留中英双语，统一用 README 的命名（`X.md` 英文、`X.zh-CN.md` 中文）；机制参考（IPC 握手、本地端点生命周期、内存策略、运行时生命周期、契约与路由、错误、载荷生命周期、C ABI）只留英文一份；计划、验收与待办这类工作记录用中文。按这个做法，`ipc-handshake.md` 要译成英文，`memory-policy.md` 与 `local-endpoint-lifecycle.md` 的中文版在核对英文版完整之后删除。理由是两份同样的机制文档要一直同步，现在中英两版的篇幅已经对不上。
2. **清退清单。** 方向已同意：`spikes/`、`logs/`、`superpowers/`、已完成的计划、`designs/` 与 v0.3.0 的架构评审。逐份的清单在盘点之后给出，由用户过目。
3. **第三节的三段新规则措辞（用户 2026-10-31 已确认，已照原文写入）。**
4. **术语表的词条**，在第二步起草后确认。

## 六、分工与顺序

- 第一步：实施方按第三节移动与合并，并写新的根文件；Host 读完差异，核对新规则措辞与用户确认的一致，并请第二名审查者独立核对"规则没有丢失"。
- 第二步：盘点按第四节分三到四批只读进行；Host 据此定处置表，交用户过目清退清单；然后由实施方执行移动、删除与链接改写；`docs/README.md` 与 `CONTEXT.md` 由 Host 写。
- 分支：两步在 `socu/docs-restructure` 上做，它从 `socu/over-defense-batches` 拉出；向上游的 PR 在代码批次的 PR 之后单独开。
- 顺带项：两步各自顺带做掉同一块的待办。

## 七、退出条件

- 根 AGENTS.md 不超过 32 KiB，测试守住；审查者确认旧 AGENTS.md 的每一条规则都有着落。
- `docs/` 只剩第二节的目录，`docs/README.md` 列出每份现行文档；全部相对链接能解析；没有本机路径。
- 待办里没有遗漏盘点中发现的未完成事项。
- `CONTEXT.md` 经用户确认。

## 八、第二步的盘点结果与处置（2026-10-31）

盘点分三批只读进行，覆盖本节列出的全部历史文档。会改变处置或指出现行文档有错的结论，Host 都复现过。本节是第二步实施的依据；逐份路径按目录列出，未列出的一律按所在目录的规则处理。

### 总览

| 去向 | 份数 |
| --- | --- |
| 直接删除（已过时） | 151 |
| 先把仍然成立的内容挪进现行文档，再删除 | 15 |
| 移入 `docs/acceptance/`（另有 `evidence/` 124 个文件随之移动） | 149 |
| 移入 `docs/research/` | 6 |
| 移入 `docs/design/` | 11，另加 `backlog.md` 与路线图两份 |

### 先挪内容、再删除的 15 份

| 文档 | 要挪的内容 | 去处 |
| --- | --- | --- |
| `superpowers/specs/2026-04-01-rust-transport-sink-design.md` §6.2 | 新写的 `#[pyclass]` 默认 `frozen`，内部可变性按场景选用，只持有 `Py<T>`；现有两处例外 | `reference/development.md` 新增"原生扩展约定" |
| `superpowers/specs/2026-04-02-dedicated-lifecycle-design.md` | dedicated 段由 SHM 头的 `read_done` 驱动回收，时间只作对端崩溃时的兜底 | `reference/memory-policy.md`"Lazy allocation and reclamation" |
| `superpowers/specs/2026-04-25-resource-registration-error-design.md` | 重名注册：经 relay 返回 409 并映射为 703；无 relay 时不做跨进程防重、不加文件锁；同进程重名报本地错误；relay 拒绝后回滚本地注册 | `reference/contracts-and-routing.md` relay 一节；`reference/errors.md` 补 703 |
| `superpowers/specs/2026-04-29-relay-peer-trust-boundary-design.md` | 消息体里的 `sender_relay_id` 只是声明，IP 白名单也不算身份；只校验已知成员与路由归属一致，保护本地路由，清除只在本地可见的 IPC 字段；HMAC 不在范围内 | `reference/contracts-and-routing.md` relay 一节 |
| `superpowers/specs/2026-05-12-server-execution-backpressure-design.md` | 不变量 I1–I6、I10、I13、I14、I19：两级预算、排队与拒绝的界线、本地调用共享路由并发状态而不占全局执行线程、写者不被饿死、连接关闭取消未开始的排队调用、未拼完的分片不计入准入、注册过提交闸门 | `reference/runtime-lifecycle.md` 新增"执行准入" |
| `superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md` | 同一 relay 重名先 ping 旧进程，活着返回 409，否则覆盖；跨节点允许重名；解析 LOCAL 优先，其余按 `(registered_at, relay_id)` 排序，墙钟偏差是已知限制 | `reference/contracts-and-routing.md` relay 一节 |
| `superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md` | §5.2–5.5 外层校验、方法形状、两种指纹投影、生命周期不进规格；§7 FastDB 错误字段；§8.2–8.4 产物路径约束与上限、目录布局、只发布到新目录树 | `reference/contracts-and-routing.md` 新增"可移植契约 v2" |
| `superpowers/specs/2026-07-24-rust-sdk-portable-payload-local-release-candidate-design.md` | §11 路由状态错误表与"只在派发前回退"；§12 ContractLimits V1；§10.3 FastDB cause 字段；§15 18 行矩阵的定义 | §11、§12 进 `contracts-and-routing.md`；§10.3 进 `errors.md`；§15 进 `development.md` |
| `designs/0.7.1-runtime-configuration-and-call-deadlines.md` | 截止时间到期不撤路由、不触发 OwnerBound 关闭；Windows 不用连接级 `CancelIoEx` 取消单次调用 | `reference/configuration.md`"Call deadlines"；`reference/windows-native-implementation.md` |
| `issues/relay-large-payload-transport-design.md` | 先验 header 再读 body；body 有界；上限取上游路由的能力；只重试可重放的 body；c2-http 不碰 MemPool；远程分片只有一个配置项 | `reference/contracts-and-routing.md` relay 转发一节；`reference/configuration.md` |
| `issues/relay-stale-owner-revival-race.md` | 替换旧注册者需要认证之后的最终探测证据；租约到期不等于放弃所有权；注册者新鲜度只在本 relay 内，不随 gossip 传播 | `reference/contracts-and-routing.md` relay 一节 |
| `vision/route-token-endpoint-connection-architecture.md` | 一条连接对应一个服务实例，不按路由分；watch/list 不承担正确性；只有语义错误能改路由权威，I/O 错误只影响那条连接 | `reference/contracts-and-routing.md` 新增"Route bindings and endpoint connections" |
| `plans/2026-06-05-route-catalog-watch-redesign.md` | 路由是服务端实例拥有的带版本资源，连接只是载体；目录有版本、可订阅，出现历史缺口时整表重建；调用时由服务端裁决。写入前对照代码：RouteWatch 不是持续推送 | `reference/contracts-and-routing.md` 新增"Route catalog" |
| `spikes/architecture-ipc-oom-protection-spike.md` | Linux overcommit 下建段几乎不会失败，真正的 OOM 发生在写入时，所以要主动感知内存压力 | `reference/memory-policy.md` 压力一节 |
| `memory-pressure-backpressure.md`（顶层） | "观察到内存压力时"的排查步骤 | `reference/memory-policy.md` |

每一条写入前都对照现在的代码；代码里已经不成立的，不写，并在报告里说明。

### 按目录的处置

- **`docs/superpowers/`：** 整个目录删除。先按上表挪出 8 份规格的内容；`plans/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md` 移入 `acceptance/`。
- **`docs/spikes/`、`docs/logs/`：** 整个目录删除。`spikes/performance-http-shm-zero-copy-spike.md` 移入 `research/`；OOM spike 按上表处理。
- **`docs/vision/`：** 删除 `README.md`、`cross-language-contract-codec-architecture.md`、`fastdb-neutral-allocator-integration.md`、`sota-patterns.md`、`toodle-distributed.md`；route-token 按上表处理；`endgame-architecture.md` 移入 `design/`，同时更新 §2.3 与 M2 里已过时的 Rust SDK 描述，删掉指向 sota 的链接。目录删除。
- **`docs/issues/`：** 删除 `chunk-management-gaps.md`、`ipc-route-contract-stale-snapshot.md`、`local-endpoint-lifecycle-plan.md`；两份 relay 文档按上表处理；`contract-release-deferred-capabilities.md` 删去已关闭的行后移入 `design/`，仍开着的行进待办。目录删除，`.gitignore` 第 206 行的注释一并改。
- **`docs/designs/`：** `0.7.3-local-endpoint-paths.md` 删除，0.7.1 按上表处理，目录删除。
- **`docs/plans/`：**
  - 移入 `design/`（9）：`2026-10-06-http-memory-budget.md`、`2026-10-13-local-slimming-batches.md`、`2026-10-23-r35-heartbeat-task-ownership.md`、`2026-10-30-release-agility-review.md`、`2026-10-31-api-surface-review.md`、`2026-10-31-defect-and-dead-code-batches.md`、`2026-10-31-docs-and-agents-restructure.md`、`2026-10-31-over-defense-review.md`、`2026-10-31-single-owner-release-evaluation.md`。其中 `2026-10-13-local-slimming-batches.md` 只把暂缓表与条目边界表留在 `design/`，执行记录拆进 `acceptance/`。
  - 移入 `acceptance/`（29）：`0.7.4.md`、`0.7.4.zh-CN.md`、`2026-10-06-relay-memory-followup.md`、`2026-10-16-r01-allocation-release-evaluation.md`、`2026-10-20-r02-unique-write-ownership-evaluation.md`、`2026-10-21-connection-observation-owner-takeover.md`、`2026-10-21-local-io-cause.md`、`2026-10-23-081-startup-errors.md`、`2026-10-23-client-events-public-errors.md`、`2026-10-23-orphan-shm-buffer-retirement.md`、`2026-10-23-r13-inline-response-ownership.md`、`2026-10-23-r16-node-backpressure.md`、`2026-10-23-r32-native-endpoint-credential.md`、`2026-10-24-r06-python-buffer-lifecycle-synchronization.md`、`2026-10-24-r33-python-manual-assembly-retirement.md`、`2026-10-24-windows-user-scope-ipc.md`、`2026-10-25-ipc-version-mismatch.md`、`2026-10-25-r15-chunk-gc-evaluation.md`、`2026-10-26-c04-notification-reuse-review.md`、`2026-10-26-r07-failed-release-ownership-evaluation.md`、`2026-10-26-r17-slot-io-evaluation.md`、`2026-10-26-r17b-lease-io-repair.md`、`2026-10-27-failed-start-cleanup-observation.md`、`2026-10-27-public-independent-runtime.md`、`2026-10-27-r17c-final-lease-stat-evaluation.md`、`2026-10-27-unix-namespace-identity-and-own-credential.md`、`2026-10-28-r07-complete-release-followup.md`、`2026-10-28-runtime-retirement-core-plan.md`，以及 `2026-09-26-memory-policy.md`（它的遗留项进待办）。
  - 移入 `research/`：`2026-10-31-cross-process-release-survey.md`。
  - 删除（34）：`2026-03-28-transport-refactor.md`、`2026-03-29-wire-v1-removal.md`、`2026-04-27-cross-language-residue-review.md`、`2026-05-04-direct-ipc-control-helpers-rust.md`、`2026-05-04-remote-ipc-scheduler-rust-config.md`、`2026-05-04-route-concurrency-rust-authority.md`、`2026-05-04-thin-sdk-rust-core-boundary.md`、`2026-05-05-runtime-session-rust-authority.md`、`2026-05-06-canonical-error-rust-authority.md`、`2026-05-06-transfer-hook-error-taxonomy-repair.md`、`2026-05-06-unified-buffer-lease-rust-authority.md`、`2026-05-07-ipc-config-validation-rust-authority.md`、`2026-05-07-relay-ipc-endpoint-identity.md`、`2026-05-07-response-allocation-rust-authority.md`、`2026-05-07-server-readiness-rust-authority.md`、`2026-05-08-chunk-lifecycle-hardening.md`、`2026-05-08-crm-contract-rust-authority.md`、`2026-05-08-crm-tag-resource-discovery.md`、`2026-05-08-relay-crm-attestation-hardening.md`、`2026-05-14-direct-ipc-shutdown-two-phase.md`、`2026-05-18-fastdb-first-crm-abi-and-bridge.md`、`2026-05-20-fastdb-first-closure-review-packet.md`、`2026-05-20-typescript-ipc-shm-runtime-adapter-requirements.md`、`2026-05-21-fastdb-borrowed-memory-goal.md`、`2026-05-24-c-two-fastdb-0-1-18-lifetime-integration.md`、`2026-05-25-fastdb-call-db-runtime-boundary-remediation.md`、`2026-05-26-fastdb-batch-array-payload-semantics.md`、`2026-05-26-fastdb-direct-shm-sink-integration.md`、`2026-05-27-fastdb-require-envelope-integration.md`、`2026-05-29-fastdb-first-crm-surface-consolidation.md`、`2026-10-27-independent-local-ipc-probe.md`、`2026-10-28-runtime-shutdown-publication.md`、`README.md`、`c-two-rpc-v2-roadmap.md`。
  - route-catalog 按上表处理。目录删除。
- **`docs/reports/`：** 整体移入 `docs/acceptance/`，`evidence/` 照原样跟着走。例外：`research-notes/` 下两份与 `2026-09-24-windows-native-ipc-analysis.zh-CN.md`、`2026-10-12-ci-community-optimization.md` 移入 `research/`；`local-endpoint-ci-runner.md`（脚手架，什么都没执行过）与 `c-two-journey-ppt-self-contained.md`（演示材料）删除。证据 JSON 里冻结的旧路径不改。0.7.4 的三对中英报告都是冻结的验收记录，两种语言都保留。
- **`docs/reviews/`：** 移入 `acceptance/`，`README.md` 删除。
- **顶层：** `architecture-review-v030.md` 删除；`memory-pressure-backpressure.md` 按上表处理；`backlog.md` 与 `roadmap.md`、`roadmap.zh-CN.md` 移入 `design/`。AGENTS.md 工作规则里的待办路径改为 `docs/design/backlog.md`。

### 现行文档里要一并改正的

- 路线图：按盘点结果改所有指向被删或被移动文档的链接；删掉指向 `c-two-rpc-v2-roadmap.md` 的行；重写 `roadmap.md` 第 15 行"plans、reviews、superpowers 是保留证据"这句；中英两版开头的参考列表补齐对等。
- `reference/configuration.md` 与中文版：`max_pending_requests` 是整个服务端共用的上限，不是每个连接一个（`c2-server` 构造服务端时只建一个信号量）。
- 指向被移动文档的链接遍及 `reference/`、`releases/`、各 README、CONTRIBUTING、CHANGELOG 与 `sdk/python/tests/README.md`，全部改到新位置。

## 九、D2 执行与交接（2026-10-31）

按第八节执行：直接删除 151 份，15 份核对代码后合并并删除；149 份记录与 124 个 evidence 文件进入 `acceptance/`，6 份进入 `research/`，14 份进入 `design/`，3 个图片进入 `assets/`，均以 `git mv` 移动。CLI 横幅文本保留在 `cli/assets/`，生成器只更新输入图片路径；默认输出与编译嵌入路径保持在 CLI 包内。输入实际有 496 个 docs 文件；第一节 490 个是此前盘点快照，逐项处置与第八节一致。

瘦身账本只保留暂缓表与条目边界表，执行正文拆为 [2026-10-13-local-slimming-execution.md](../acceptance/2026-10-13-local-slimming-execution.md)。7 月本地候选的实施计划命名为 [2026-07-24-rust-sdk-portable-payload-local-release-candidate-plan.md](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate-plan.md)，与同名闭环报告区分。

所有内容迁移均对照源码；使用当前固定契约上限与轮询 RouteWatch，未恢复旧连接写入屏障。延后能力删除已关闭的 FastDB Rust、Python/TypeScript 分发、Host 截止时间投影和 hosted 验证四行；部分关闭的 C-Two 分发、浏览器证明与 SHM 对端权限行按当前边界修正。待办新增两条归组记录，复用已有协议、TypeScript、内存与发布事项；原待办误写的 704 StaleResource 更正为已在生产使用的 RouteStale，保留 706 WriteConflict 决定。错误参考与 crate 地图同步当前 C2E1 编码。

修正后的本地仓库检查为 845 passed、11 skipped、1 warning；规定的 Rust error_normalization 为 8 passed、0 failed。文档检查覆盖全部 219 份受版本管理 Markdown，233 passed；冻结记录核对为 151 份 Markdown 正文一致、122 份非 Markdown 字节一致（其中 67 份 evidence JSON），0 处差异。CLI 包清单有 19 个文件并包含 `assets/banner_unicode.txt`，CLI 构建退出码为 0。本地路径检查包含冻结 evidence；主目录与主检出的可见目录标记同时检查两种分隔符，9 个目录标记用例通过，`GITHUB_ACTIONS=true` 时本项为 1 skipped。报告直接交付给 Host；本地命令日志仍保存在 `target/d2-validation/`。这些数字仅说明本地文档改动验证。Host 继续编写 `docs/README.md` 与 `CONTEXT.md`，并做整体审查与验收。
