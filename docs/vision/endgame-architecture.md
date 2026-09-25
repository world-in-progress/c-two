# C-Two Endgame — 分布式资源运行时架构

> **Status:** Vision (living document)
> **Scope:** 为 C-Two 的最终形态、下游框架（Toodle）与应用（Gridmen）定义分层边界
> **Audience:** C-Two 核心开发者、Toodle / Gridmen 贡献者、Agent / Extension 作者

## 0. TL;DR

C-Two 的 endgame 不是“更快的 RPC”，而是一个**以运行时资源为一等公民的分布式协议**，让跨进程、跨机、跨语言的资源对象以统一 CRM 契约被调用。

**Toodle**（Trust-Oriented Open Distributed Linking Environment）是更广义的可信开放资源环境：它拥有持久 Resource identity/revision、资源图关系、可选的资源树视图、tag/search 投影、policy、Resource Service 声明、runtime activation 与 federation。C-Two 提供 CRM 契约与运行时机制；活跃 CRM route 是 Resource Service 的运行时投影，不是持久 Toodle Resource 本身。

Toodle 的治理底座不限定 GIS，但它也不是吞并所有领域习惯与工业特性的“大而全”框架；地理、ML、金融或 IoT 等领域仍需各自定义资源语义、服务契约、算法边界与信任约束。**Gridmen** 是地理领域资源与服务之上的人在回路智能编辑器。

本文按四层刻画 endgame，并专门回应四组容易被误读的问题：

1. **Toodle 是不是 C-Two 的一部分？** —— 不是。C-Two 提供通用 CRM/runtime mechanism；Toodle 独立拥有资源治理、信任与策略。
2. **Toodle 是不是 GIS 专用或大而全？** —— 都不是。治理抽象可跨领域复用，但每个领域的约定、壁垒和特殊问题由领域层承担。
3. **Agent 在架构里扮演什么？** —— CRM 可投影为 Agent tool schema，但权限来自 Toodle，领域任务的最终责任仍由人和领域系统承担。
4. **“建模扩展”到底是什么形式？** —— 在 C-Two 运行时中，它可表现为既注册资源又消费外部资源的复合进程；在 Toodle 中，它仍需对应持久 Resource/Resource Service 声明和 policy。

---

## 1. 定位：谁站在哪一层

整个体系分成四层，越往下越稳定、越薄、越领域无关；越往上越业务化、越可替换。

```
┌──────────────────────────────────────────────────────────────────────┐
│ L4  Gridmen — 人在回路的智能地理编辑器                                │
│     • Web + Electron 外壳  • 图层树 / 符号 / 视口                     │
│     • Extension Host（加载 Agent / 建模扩展 / UI 面板）               │
│     • Human-in-the-loop：用户是地理数据的第一生产者和责任人           │
├──────────────────────────────────────────────────────────────────────┤
│ L3  Geospatial CRM Catalog — 通用地理资源原语（领域实例化）           │
│     • VectorLayer · RasterLayer · TiledGrid · DEM · PointCloud        │
│     • Topology · GridSchema · STAC-backed RemoteLayer                 │
│     • 以 FastDB Core-owned portable payload 保持跨语言语义一致性       │
│     • 注：其他领域（ML / 金融 / IoT）可以有自己的 L3 Catalog          │
├──────────────────────────────────────────────────────────────────────┤
│ L2  Toodle — Trust-Oriented Open Distributed Linking Environment      │
│     • Catalog：Resource / Resource Service identity、revision、graph  │
│     • Optional tree views + tag/search/semantic discovery projections │
│     • Policy、Service declarations、runtime activation、federation    │
│     • 面向人和 Agent 的可信资源访问；领域规则由上层 Resource Service 承担│
├──────────────────────────────────────────────────────────────────────┤
│ L1  c-two — 分布式资源运行时协议                                      │
│     • c-two.contract.v2 / ContractReleaseRef / payload composition   │
│     • 注册-获取  • IPC/HTTP 传输  • Relay mesh  • exact route contract │
│     • Python SDK；auth metadata、Rust SDK 与 TypeScript SDK 仍按 roadmap 推进│
└──────────────────────────────────────────────────────────────────────┘

正交视图：计算模型（见 §3）
  客户端 (无状态)  ↔  资源 (有状态)  ↔  复合 (托管+消费)
  —— 可出现在任何一层的任何进程里
```

**三个关键的边界原则：**

1. **C-Two 只做通用 CRM/runtime 机制，不做治理策略。** 它定义契约、精确 route identity、传输和运行时生命周期；Authority identity、policy、Catalog 和 federation 属于 Toodle 等上层系统。
2. **Toodle 不吞并领域模型或 UI。** 它管理可信资源环境的通用治理对象；地理、ML、金融等领域的格式、算法、服务行为和行业约束由领域 Resource Service 承担。
3. **Gridmen 是地理领域的一种客户端体验。** CLI、Notebook、Agent 或其他客户端可以消费同一批受治理 Resource/Resource Service，但不要求共用固定 GUI 或工作流。

## 2. L1 · c-two 协议层：机制非策略

### 2.1 形态

C-Two 是一个**资源运行时协议**。这里的 runtime Resource 是实现 CRM 契约的有状态对象，与 Toodle Catalog 中持久、可修订的 Resource 是不同层次的概念。C-Two 的最小集合只有三件东西：

- **Resource**：实现 CRM 契约、持有状态与领域逻辑的运行时对象；当前 Python SDK 中是普通 Python 类实例。
- **CRM 契约**（Core Resource Model contract）：带命名空间和版本的接口声明；`c-two.contract.v2` 是 canonical descriptor，`ContractReleaseRef` 是它的持久、route-independent 精确引用。
- **传输与运行时发现**：注册/连接、IPC、HTTP 与 Relay Mesh，负责把带精确 contract expectation 的调用送到活跃 route。

### 2.2 为什么 c-two 不该再扩张

边界判断不能把所有上层概念都推给 Toodle：跨领域可复用的 CRM/runtime mechanism 属于 C-Two，持久资源治理与 trust policy 属于 Toodle，文件格式、算法、事务和行业规则属于具体领域 Resource Service。具体边界在 [§9](#9-协议边界清单c-two-做什么不做什么) 里详列：

| 放进 c-two | 不放进 c-two |
|---|---|
| canonical CRM descriptor、`ContractReleaseRef`、精确 route contract | Catalog、Resource identity/revision、trust envelope |
| 注册/连接、IPC/HTTP/relay、buffer 与 lease 生命周期 | Authority、认证、授权、租户与 policy |
| 方法级并发（`@cc.read`/`@cc.write`）和 runtime lifecycle | Service activation 策略、federation catalog、业务一致性算法 |
| Opaque nested FastDB specification 的契约与 artifact 编排 | FastDB 内部 schema/binary/runtime、领域文件格式与算法 |

### 2.3 当前仍需补齐的通用机制

| 能力 | 当前边界 | 对应目标 |
|---|---|---|
| **Contract compatibility** | 已有精确 `ContractReleaseRef`，尚无 semver/range 匹配 | 在不削弱精确校验的前提下解析兼容 release |
| **`auth_hook` + call metadata** | 尚未形成完整公共契约；C-Two 只提供机制，Toodle 解释身份与 policy | 让受信 Authority 在所有 transport 上执行一致准入 |
| **完整 Rust SDK** | lower-level public Rust crates 与 FastDB payload runtime 已完成真实双向证明，但尚未收敛为覆盖 IPC/HTTP/relay/lifecycle 的统一受支持 facade | 在不获得 Python 缺失能力的前提下提供完整 Rust 用户面 |
| **TypeScript SDK** | 生成 transport/codec 基础持续演进，完整发布与浏览器边界仍未收敛 | Web/Node 消费同一 CRM contract 与 payload ABI |
| **Async、streaming、backpressure** | 属于后续 runtime workstream，不能以 chunking 冒充 streaming | 长任务与多语言调用的可取消、有界执行 |

这些能力按 [`docs/roadmap.md`](../roadmap.md) 推进；ContractRelease 相关延后边界见 [`contract-release-deferred-capabilities.md`](../issues/contract-release-deferred-capabilities.md)。Toodle 可以先消费稳定的 exact release identity，但不得以私有 transport、Python sidecar 或自定义 FastDB parser 填补上游缺口。

## 3. 计算模型：无状态客户端 · 有状态资源 · 复合服务

这一节**正交于**上面的四层 —— 它描述的是"一个 c-two 进程可以扮演什么角色"。不理解这个
正交视图，就很难把 c-two 和主流无状态架构对接。

### 3.1 与主流架构的一一映射

```
主流架构                    c-two
─────────────────────────────────────────────────────
Stateless Service       ≈   Client 进程   (纯消费 + 纯计算)
Database / State Store  ≈   Resource     (被注册的有状态对象)
gRPC / SDK 接口          ≈   CRM 契约     (类型化调用合同)
Service Mesh            ≈   Relay Mesh   (发现 + 路由)
Control Plane           ≈   Toodle       (策略 + 治理)
```

**纯客户端进程就是 c-two 里的"无状态一等公民服务"**。它的语义和 Lambda / 无状态微服务几乎
重合：

- **入参无持久状态**：所有需要的状态通过 `cc.connect(CRM, ...)` 从外部资源获得
- **可暴露内部状态**：运行时的瞬时计算、进度、部分结果（通过返回值、streaming、metrics）
- **可产生新状态**：作为 factory 注册新的资源（`cc.register(...)`）
- **可更新已有状态**：调用外部资源的 `@cc.write` 方法
- **可水平扩展 / 可重启**：重启不丢数据，数据都在它连接的资源里

与无状态服务唯一的差别是：客户端进程是个**类型化 Python 函数**（或 TS 函数），不是 HTTP 端点。
它的"协议面"是 CRM 契约签名，不是 OpenAPI spec。

### 3.2 唯一的二元边界：是否调用 `cc.register`？

判定一个 c-two 进程处于哪种模式，**只需回答一个问题**：这个进程是否调用了 `cc.register(...)`？

- **调用了** → 这是一个**资源托管进程**（Resource Host）。它持有有状态对象并通过 CRM 契约对外暴露。
- **没调用** → 这是一个**客户端进程**（Client）。它可能通过 `cc.connect(...)` 消费外部资源，
  也可能只是纯本地计算。

这条硬边界就是 c-two 的全部"角色"语义。过去引入的 "客户端" 概念已被删除 —— 它既不是可被
外部观察的生命周期对象，也没有独立的注册动作，只是"一个恰好调了 `cc.connect` 的普通函数"。

> **进一步的正交维度（描述性，非硬边界）**：一个资源托管进程自己也可以**同时**调用
> `cc.connect(...)` 去消费别的资源 —— 这就得到了传统架构里"有自己 DB + 调用其他微服务的微
> 服务"那种复合形态。对 c-two 而言，这不是第三种角色，而是*同一个进程同时承担两种职责*。

### 3.3 关键推论：c-two 允许角色自由叠加

一个进程可以**同时**：
- 注册若干资源（扮演资源提供者）
- 调用 `cc.connect(...)` 消费其它资源（扮演客户端 / 计算者）
- 递归地：一个资源的方法内部也可以 `cc.connect(...)` 去调别的资源

这带来两个重要能力：

- **部署拓扑无关**：同一份代码，单进程跑（所有资源 + 客户端代码在一个 Python 进程，用
  `thread://` 直连）或者分布式跑（每个资源一台机、客户端分散），完全不用改业务逻辑。
- **计算-资源局部性**：一个建模扩展持有的资源既直接参与它自己的求解计算（零 IPC），又能
  对外暴露服务。这是 [`sota-patterns.md`](./sota-patterns.md) 的核心论点。

### 3.4 这对设计的约束

理解了这个模型之后，很多设计决策会变清晰：

- **客户端不该被"注册"**。它没有生命周期需要被外部观察。只有资源被注册。
- **客户端之间直接调用**（Python 函数调函数）—— 不走 RPC，没有意义。
- **CRM 契约是跨角色的唯一契约**：不管对方进程是纯资源托管还是既托管又消费，只要提供
  同一个资源，消费者看到的就是一样的。这是多态性。
- **Agent 看到的世界只有资源**：Agent 通过 CRM 契约调度外部能力；Agent 自己是一个客户端
  （无状态、可重启），但它调用的一切都是资源。

---

## 4. L2 · Toodle：可信开放资源环境

### 4.1 命名由来

**Toodle = Trust-Oriented Open Distributed Linking Environment**。它不是 C-Two 的 Catalog 插件，也不以路径树、tag 或活跃 route 作为唯一资源身份；它是一个自治、可联邦的资源权威，为人和 Agent 提供持久身份、关系、策略与受限运行能力。

**一句话版本：**

> Toodle 管理“什么资源长期存在、它如何演化和关联、谁能以什么权力使用它，以及一个已声明 Resource Service 何时被激活”；C-Two 定义并承载该服务在运行时暴露的 CRM 契约。

Toodle 的核心模型不硬编码地理语义，但这不意味着用一套抽象抹平所有领域。地理领域的格式、时空规则、计算模型、服务行为与治理约束住在 L3 Resource/Resource Service 实现中；其他领域可以复用治理底座，同时保留自己的行业边界。

### 4.2 资源图（Resource Graph）

Toodle Catalog 维护持久资源图；图中的身份和关系在没有运行时进程、没有 relay route 时仍然成立：

- **Resource 节点**：受 Toodle 治理的文件、表、数据库、代码、模型、配置、实时源或结果，并具有明确 revision/update model。
- **Resource Service 节点**：独立于 Resource 的逻辑服务声明，绑定一个或多个 Resource，并声明 CRM contract set、policy projection 与 activation policy。
- **边**：`depends_on`、`derived_from`、`references`、`binds` 等持久语义关系；具体关系必须由领域契约或 Catalog 规则解释，不能从 route 名称猜测。
- **资源树**：面向项目、目录或 GUI 的可选视图，可以从图关系和查询结果构建；它不是第二套身份系统，也不是所有资源必须加入的结构。

活跃 CRM route 只是某个 Resource Service 被激活后产生的 RuntimeInstance 投影。Catalog 中的 Service 可以 inactive 而 relay 中没有 route；route 消失也不会删除 Resource、Resource Service 或它们的 revision history。

### 4.3 发现投影与信任模型

Tag、时空索引、全文/语义检索和 Agent/LLM 检索都是资源发现与视图投影，不是 Catalog identity 或授权事实。不同来源的投影必须保留不同的信任边界：

| 类别 | 例子 | 来源与约束 | 是否可单独授权 |
|---|---|---|---|
| **描述与发现投影** | `domain=hydro`、bbox、时间范围、embedding | 用户、ingestor 或派生索引可以提供，但必须保留 provenance 与 revision | ❌ |
| **身份与 policy 属性** | `owner=user_123`、Authority、tenant、visibility | 由 Toodle Authority 和 policy 管理，不能接受调用者自报 | 仅经 PolicyDecision |
| **CRM/capability 投影** | CRM namespace/version、可调用方法、受限能力 | 从验证后的 Resource Service/CRM descriptor 与 policy projection 派生，不能作为用户自报事实 | 仅经 descriptor + policy 校验 |

C-Two 不拥有上述索引、语义或授权解释。它只提供 canonical CRM contract、精确 runtime route contract 和调用机制；Toodle 在自己的 Catalog/Policy 边界内决定哪些持久对象可被发现、激活和连接。

### 4.4 Workspace 与协作

Workspace 是 Git project/worktree 与 Toodle 治理对象的用户工作环境投影，而不是把 live CRM route 收集进一个容器。一个 workspace 可以包含：

- 一组持久 Resource 或 Resource Service 引用及其选定 revision；
- 资源图查询、可选资源树、tag/search 过滤和领域 UI 状态等视图；
- ServiceDefinition、ResourceBindings 与 Extension 启用声明；
- 成员、policy 与审计上下文。

当用户连接 Resource Service 时，Toodle 先依据声明与 policy 决定是否允许/激活，再通过 C-Two 将已验证的 CRM release 投影为精确 runtime route contract。普通 Resource 不会被隐式服务化，route 缺失也不改变其持久身份。

C-Two 的方法级读写并发只提供单个 runtime resource 的调度原语，并不自动解决业务事务、跨资源一致性、OT/CRDT 或任意地理资源合并；这些行为必须由具体 Resource Service 和领域规则定义，Toodle 不宣称通用自动合并。

---

## 5. L3 · 地理资源目录：领域 CRM 原语

L3 不是一个“crate”，而是一组领域约定：哪些地理资源类型、行为、精度、格式和治理约束应被标准化，使 Toodle、Agent、Extension 与 UI 可以依赖可预期的语义。Toodle 只提供持久身份、关系、policy、activation 与 federation 的治理环境，不拥有这些地理语义；ML、金融等领域可以复用治理机制，同时维护各自独立的领域模型。

### 5.1 标准 CRM 清单（提案）

| 类别 | CRM | 核心语义 | 建议底层 |
|---|---|---|---|
| 矢量 | `IVectorLayer` | feature 集合、空间索引、属性访问 | fastdb / GeoArrow |
| 栅格 | `IRasterLayer` | 窗口读、重采样、波段 | COG / Zarr / fastdb |
| 瓦片 | `ITiledGrid` | z/x/y 访问、多级金字塔 | MBTiles / COG / c-two 自身 |
| 数字高程 | `IDEM` | 点采样、视域、坡度 | GeoTIFF / COG |
| 点云 | `IPointCloud` | 范围查询、LOD、属性 | Entwine / COPC |
| 拓扑 | `ITopology` | 邻接、面片、分裂-合并 | c-two 自身（现有 Grid） |
| 远程图层 | `IRemoteLayer` | STAC / WFS / OGC API 的薄适配 | 外部服务 |

**准入门槛不是"覆盖所有人"，而是"语义稳定"。** 一个 CRM 一旦进入 L3，它的方法签名就是
公共合同，加字段要走版本，删字段要走弃用通道。这是生态可扩展的前提。

### 5.2 为什么不把建模场景也放进 L3

因为建模（洪水、交通、土地利用）的 CRM **永远是领域特定的**，而且演化速度远快于 L3 原语。
把建模 CRM 放进 L3 会让核心 catalog 被领域细节拖累。正确的做法是：建模 CRM 住在**扩展包**
（[§8](#modeling-extension-composition)），通过**依赖 L3 CRM** 来读写通用资源。

## 6. L4 · Gridmen：人在回路的智能地理编辑器

### 5.1 Gridmen 不是开源 ArcGIS

开源 ArcGIS / QGIS 的核心范式是**单机桌面 + 进程内插件**。Gridmen 的定位差异在三点：

1. **计算与编辑解耦**：所有重型资源（栅格、矢量、模型状态）作为 CRM 运行在别处；
   Gridmen 前端只做渲染、编辑意图、面板。"插件"不是 Python 模块，而是一个带 CRM 的
   外部进程。
2. **天然协作**：连同一个资源 就是多人编辑同一资源，不需要额外服务器。
3. **天然联邦**：通过 c-two relay mesh，把另一个机构公开的 CRM 挂进自己工作区，在技术上
   等价于本地资源。

更贴切的类比是 **"VS Code for Geospatial"**：核心薄、扩展丰富、计算住在外部进程、多端一致。

### 5.2 Human-in-the-Loop 是刻意的，不是临时的

尽管 VLM 近几年进展快速，但**生产级的遥感矢量化、亚米级目视解译、复杂地理语义推理**在可
预见的未来仍需要专用 CV 模型 + 人工 QA（见 §11-Q1 的研判）。因此 Gridmen 的设计原则是：

> **用户是地理数据的第一生产者和责任人，Agent 是加速器，不是替代者。**

具体表现为：

- **Agent 永远不单独提交地理数据的写操作**。重要写入走"Agent 提案 → 用户审批 → 生效"。
- **Agent 的能力边界明确可见**：用户随时能看到"这个 Agent 能调用哪些 CRM 方法"。
- **不可逆操作需要双重确认**：删除图层、覆盖已发布数据、跨租户写入。
- **审计轨迹是一等公民**：谁（人还是 Agent）在什么时间、以什么身份、调用了哪个 CRM 方法，
  全部由 Toodle 持久化。

### 5.3 用户最小心智模型

一个普通地理工作者看到的 Gridmen，只需要理解三件事：

1. **资源**（图层、模型）：在 workspace 里打开、编辑、关闭。
2. **扩展**：装了就多出一些面板、按钮、模型。
3. **Agent**：可以帮我把多个操作串起来，但最终按钮是我按。

CRM / Toodle / c-two 这些对普通用户是**不可见的**。它们是基础设施，不是 UX。

## 7. Agent 集成：CRM 作为 Tool Schema

### 6.1 为什么 CRM 天然适合做 Agent Tool Schema

当前 Agent 生态（OpenAI tool calling、MCP、LangGraph 等）都要求"工具"满足四个条件：
**命名、类型化参数、版本、权限边界**。CRM 天生带齐这四样：

| Agent Tool 要求 | CRM 对应 |
|---|---|
| 命名 | `@cc.crm(namespace='hydro.swmm', version='0.3.0')` + method name |
| 类型化参数 | canonical descriptor 中的方法签名与显式 nested FastDB binding |
| 版本 | `c-two.contract.v2` + exact `ContractReleaseRef`；范围兼容仍待实现 |
| 权限边界 | `@cc.read` / `@cc.write` 是调度元数据；授权由 Toodle PolicyDecision 负责 |

这意味着 Toodle 可以把 Catalog 中已获授权 Resource Service 的 validated CRM contract 投影为标准 tool schema（例如 MCP），但投影不能把“可描述”误当作“已授权”，也不能把普通 Resource 隐式服务化。

### 6.2 Agent Runtime 的三层调度

```
┌────────────────────────────────────────────────┐
│ Planner (LLM)                                  │
│   输入: 用户自然语言目标 + 当前 workspace 快照 │
│   输出: 一个 CRM 方法调用的 DAG              │
├────────────────────────────────────────────────┤
│ Toodle Agent Runtime                           │
│   • 从 Catalog/search projection 发现候选 Service│
│   • 用 CRM schema 校验调用类型                │
│   • 经 PolicyDecision / 人工审批控制调用       │
│   • 执行、收集结果、回传给 Planner            │
├────────────────────────────────────────────────┤
│ c-two Protocol                                 │
│   • auth_hook / metadata（roadmap capability） │
│   • 承载精确 contract-bound runtime call       │
│   • 实际执行 资源方法                          │
└────────────────────────────────────────────────┘
```

### 6.3 Agent 该做什么，不该做什么

| Agent 擅长 | Agent 不擅长（交给人） |
|---|---|
| 工作流编排（"导入 DEM → 跑洪水模型 → 导出高风险区"） | 目视解译（遥感矢量化、地物识别） |
| 元数据搜索（"找到所有 `domain=hydro` 的 CRM"） | 地理美学决策（符号设计、视觉泛化） |
| 单调重复任务（批量投影变换、命名规范化） | 不完整/模糊数据的创造性判断 |
| 文档 / 教程 / 代码生成 | 跨域责任判断（"这条堤坝该不该开"） |
| 执行已审批的 DAG | 提交未审批的写操作 |

### 6.4 对 c-two 协议的具体要求

为了让上面的运行时顺畅工作，c-two 需要提供（当前已部分具备）：

1. **CRM schema 导出**：能把 CRM 序列化为机器可读的 schema（JSON Schema / MCP tool spec）。
2. **Call metadata 透传**：Agent 身份、trace id、审批 token 必须能随调用传递。
3. **Dry-run / 审批钩子**：写方法应该能"只校验不执行"，让 Toodle 在用户审批前得到影响预估。
   （这是新需求，需要进入 roadmap。）
4. **Streaming 返回**：长任务（跑一个模型）需要流式进度，Agent 才能实时反馈给用户。

<a id="modeling-extension-composition"></a>

## 8. 建模扩展 = 复合进程：对称的自包含与依赖

### 8.1 关键洞察

"建模场景扩展"本质上**就是 c-two 文档里的 客户端 概念被放大后的形态**。一个客户端
是资源的消费者；一个"建模扩展"是**既消费又提供**资源的复合进程。

三种出现形式都是合法的：

```
┌─────────────────────────────────────────────────────────────┐
│ 模式 A · 纯消费者                                           │
│   只通过 CRM 调用外部资源，自己不持有长生命周期状态         │
│   例: 一个面积计算工具扩展                                   │
│   def compute_area(addr: str) -> float:                     │
│       with cc.connect(VectorLayer, address=addr) as layer:  │
│           return layer.area()                               │
├─────────────────────────────────────────────────────────────┤
│ 模式 B · 自包含资源                                         │
│   扩展内部持有资源（如潜水方程求解器），自行管理运行时状态   │
│   状态不暴露（或部分暴露）给外部                            │
│   例: 轻量一次性地形分析                                     │
├─────────────────────────────────────────────────────────────┤
│ 模式 C · 自包含资源 + 对外 CRM                             │
│   扩展持有资源 + 注册它的 CRM 契约 到 relay                     │
│   外部（其他扩展、Agent、用户前端）可通过 CRM 查看/驱动模型│
│   例: 耦合水动力模型 —— 浅水方程 CRM + SWMM CRM 互相连接    │
│       同时用户前端通过该资源 暴露的 CRM 暂停/干预仿真       │
└─────────────────────────────────────────────────────────────┘
```

这三种形态在 c-two 层面**是同构的**：都是"一个进程，里面有若干资源 + 若干 CRM 连接"。
区别只是边界选择。

### 8.2 自包含资源为什么重要

这是 [`sota-patterns.md`](./sota-patterns.md) 里反复强调的一点：

> 实现 CRM 的 runtime Resource 可以直接参与计算；只有显式 `cc.register(...)` 后，它才通过 C-Two 对外提供 runtime route。

这意味着**建模扩展不需要拆成"计算进程 + 资源进程"两个部分**。潜水方程求解器的网格状态既
是它自己计算的对象，又是外部可访问的资源；C-Two 的 registry 只是让后者成为可能，而不强制
前者和后者分离。

**端到端的好处：**
- 最优局部性：计算和资源同进程，零 IPC 开销
- 外部协作不失：其他扩展、Agent、前端可在 policy 允许且 route 活跃时通过 CRM 访问状态
- 生命周期清晰：模型进程活着 → runtime route 可用；模型进程结束 → route 注销，但 Toodle Resource/Resource Service identity 不因此消失

### 8.3 扩展声明与 Toodle 的契约

Extension 不是 C-Two 本体。它的代码、模型和 UI artifact 首先是 Toodle Resource；需要运行时能力时，再由明确的 Resource Service declaration 绑定 Resource、CRM contract release、activation policy、权限需求和可审计的 Agent/UI projection。字段名和安装格式属于 Toodle 的独立协议，不在本文中复制定义。

C-Two 只拥有两段通用机制：以 canonical `c-two.contract.v2` / `ContractReleaseRef` 表达精确 CRM release，以及在激活后以 `ExpectedRouteContract` 注册、解析和调用 runtime route。Extension 安装、ResourceBindings、PolicyDecision、Activator 算法和是否允许 Agent 调用都不属于 C-Two。

### 8.4 扩展之间的依赖

Toodle 可以在自己的声明中表达“需要某类 Resource Service/CRM contract”的目标态需求，但当前 C-Two 只实现精确 release identity，不实现 semver/range compatibility。一个可审计的解析过程应当：

1. 在 Toodle Catalog 中查找满足 policy 和领域约束的 Resource Service declaration，而不是搜索 live route 充当持久依赖。
2. 将兼容性需求解析并锁定为一个确切 `ContractReleaseRef`；在 C-Two 的范围兼容规则完成前，调用方必须显式 pin 精确 release。
3. 需要运行时调用时，由 Toodle 决定是否激活已声明 Service，再用已验证 release 和 route name 构造精确 `ExpectedRouteContract`。
4. 多个 release 可以对应不同的 Resource Service revision 或逻辑服务并存，但 route name 只解决运行时寻址，不自动解决 ABI 兼容、数据迁移或领域冲突。

版本范围与 Rust SDK 等缺口的当前限制、影响、owner 和退出条件记录在 [`contract-release-deferred-capabilities.md`](../issues/contract-release-deferred-capabilities.md)。

## 9. 协议边界清单：c-two 做什么、不做什么

这张表是评判任何新特性要不要进 c-two 的检验器。**放错层比不实现更糟糕**，因为错层的抽象
会污染所有下游框架。

| 能力 | 住在哪一层 | 理由 |
|---|---|---|
| canonical `c-two.contract.v2` descriptor 与 `ContractReleaseRef` | **C-Two** | CRM contract semantics 与精确、route-independent release identity 属于协议机制 |
| CRM 注册与获取 | **c-two** | 资源运行时的最基本机制 |
| IPC / HTTP / Relay 传输 | **c-two** | 跨进程/跨机能力 |
| `ExpectedRouteContract` 与 contract-scoped route resolve | **C-Two** | 活跃 RuntimeInstance 的精确寻址与调用准入 |
| 方法级读写并发 (`@cc.read`/`@cc.write`) | **c-two** | 单 CRM 内部的调度 |
| Buffer/lease 生命周期、SHM transport 与 Hold | **c-two** | Transport 与 lifetime 原语；不替代 FastDB payload owner |
| `auth_hook` + call metadata 透传 | **c-two（待补）** | 让 Toodle 构建安全层的钩子 |
| Dry-run / 审批预估 | **c-two（待补）** | 让 Toodle / Agent 做影响分析 |
| Streaming 返回 | **c-two（待补）** | 长任务进度回传 |
| — | — | — |
| 认证 (AuthN) | **Toodle** | JWT / OIDC / mTLS 属于策略 |
| 授权 (AuthZ) / ACL / 租户隔离 | **Toodle** | 身份解释权属于 Toodle |
| Resource identity/revision 与 Resource Service declaration | **Toodle** | 持久治理身份不能由 live route 代替 |
| Resource graph、可选 tree views、tag/search projections | **Toodle** | Catalog 关系与发现视图不是 C-Two runtime registry |
| Policy、activation、federation 与审计 | **Toodle** | Authority 决定权力并保留治理真相 |
| 主从仲裁 / leader election | **Toodle 或 K8s** | 业务决策，c-two 不假设 |
| Workspace / Project 绑定 | **Toodle + Git** | Git repository/worktree 是历史与工作环境，Toodle 绑定治理对象 |
| 扩展安装 / 依赖解析 | **Toodle** | 生态治理 |
| Agent tool schema 导出 | **Toodle** | 但 c-two 需支持 CRM 自省 |
| — | — | — |
| FastDB nested schema、binary、builder、view、materialize、invalidate 与 payload codegen | **FastDB** | Portable payload semantic authority 不属于 C-Two contract/runtime |
| 图层符号 / 渲染 / 视口 | **Gridmen** | UX |
| 文件格式（GeoTIFF / Shapefile 等） | **L3 CRM 实现** | 与协议无关 |
| 目视解译、矢量化 | **Gridmen + 专用 CV 模型** | 不是协议能解决的 |
| CRDT / OT 协作算法 | **具体 CRM 的实现** | 视资源特性而定 |

## 10. 演进路线（非日程，仅相对顺序）

本节只表达跨仓库依赖方向；C-Two 的可执行顺序以 [`docs/roadmap.md`](../roadmap.md) 为准，Toodle 与 FastDB 分别在自己的仓库维护计划。里程碑按相对依赖而非时间点刻画。

### Milestone M1 · c-two 自立（当前）
- canonical `c-two.contract.v2`、exact `ContractReleaseRef`、opaque nested FastDB delegation、artifact composition、contract-scoped route、transport 与 runtime lifecycle 构成稳定地基
- 后续按 roadmap 补齐 compatibility、call metadata/auth hook、dry-run、async、backpressure 与 streaming，不以私有 SDK 旁路替代

### Milestone M2 · 完整 Rust SDK 与不可变依赖分发
- 已有 lower-level Rust client/host + official FastDB crate 的 Rust↔Python/Rust payload-bearing proof
- 将它收敛为受支持 Rust SDK 前，先完成一致的 lifecycle/error/HTTP/relay surface，不能只包装现有 internals
- 经单独授权发布并 pin FastDB Rust/Python/TypeScript artifacts，移除 sibling-checkout 分发限制

### Milestone M3 · TypeScript SDK + FastDB codec
- TypeScript 消费同一个 canonical CRM descriptor、release identity、route contract 与 FastDB binding
- 浏览器/Node 支持边界、runtime packaging 与 retained-view lifetime 必须由端到端 proof 收敛

### Milestone M4 · L3 Geospatial CRM Catalog 起步
- 先确定 2–3 个最核心的 CRM（建议：`IVectorLayer`、`IRasterLayer`、`ITiledGrid`）
- 给出参考实现 + 版本治理流程
- 这是上层 Toodle / Gridmen 能稳定迭代的前提

### Milestone M5 · Toodle Authority 起步
- Rust authority kernel：Catalog、Resource/Resource Service identity 与 revision、resource graph 和 optional tree views
- Policy、Service declaration、runtime activation 与 federation 的同一对象模型；本地与集群只更换宿主 adapter
- Extension/Agent 作为受治理 Resource 与 Resource Service 消费者，不创建第二套运行时协议

### Milestone M6 · Gridmen endgame
- Electron + Web 双端外壳
- Extension host
- Agent runtime（基于 CRM 自动导出 MCP tool schema）
- 人在回路审批流

### Milestone M7 · 联邦与跨 Authority 运行时
- 公共 relay 上托管的 CRM 可被多机构消费
- C-Two 提供 route/transport authenticity mechanism；Toodle Authority 负责 peer trust、policy、审计与 federation catalog
- 远程调用仍先经过本地 Authority/Policy，不能让 peer 绕过治理层直接触发部署根能力

## 11. 开放问题

### Q1 · LLM / VLM 在 GIS 场景的真实能力边界在哪？（研判）

**结论**：可预见的未来（2025–2027），通用 VLM **无法**生产级完成遥感矢量化、亚米级目视解译、
多波段（NIR/SWIR）解译等任务。根据 2024–2025 的公开研究与基准：

- LLM（纯文本）没有图像通道，直接处理遥感影像不可能。
- 通用 VLM（GPT-4V / Gemini / LLaVA 家族）只能给 RGB 影像做描述级解译；输出是 raster mask
  或 heatmap，**不是 GIS-valid polygons**；对多光谱、高分辨率的支持欠缺。
- 真正能做建筑物/道路提取的仍是**领域专用 CV 模型**（U-Net/Mask R-CNN/SAM 的遥感微调版），
  它们需要规模化标注和长期迭代。
- 最现实的形态是"**专用 CV 跑像素级 → VLM 做语境 QA → 人工审核定稿**"的混合工作流。

**对 Gridmen 的含义**：Agent 在空间任务里的角色是**编排者和副驾驶**，不是**主创**。把这条
内化为产品原则，可以避免做出"Agent 自动画矢量"这类会在真实数据上崩掉的功能。

参考：SpaceNet benchmarks；arXiv 2304.06159 "GeoAI Foundation Models"；
arXiv 2308.14600 "SAM for Remote Sensing"；Nature 2023 "Foundation Models for EO"。

### Q2 · Offline-first 还是 online-first？

Gridmen 需要同时支持本地工作环境与远程/团队 Authority。两种形态使用相同的 Toodle Resource、Resource Service、revision、policy 与 activation 模型；差异落在 Authority 部署、内容位置和 MetaTrigger/transport adapter，而不能把本地模式退化为只保存 live route 的第二套产品。

### Q3 · 数据如何"入库"？

用户硬盘上的 GeoTIFF、Shapefile 文件组或其他内容进入 Toodle 后首先是 Resource，不会自动“变成 CRM”。Authority 可以按 policy 将内容上传/快照到受管对象存储，也可以登记受限 remote/local reference；只有用户或系统显式声明 Resource Service 并绑定这些 Resource 时，Activator 才构造领域运行时对象并通过 C-Two 注册 CRM route。敏感数据可留在受控位置，由 data black box 式 Resource Service 暴露受限算力/模型/数据访问，而不是要求上传原始内容或隐式服务化。

### Q4 · 版本依赖地狱

两个消费者分别要求 `IVectorLayer@1.x` 和 `IVectorLayer@2.x` 时，C-Two 当前只验证精确 `ContractReleaseRef`，并未实现范围求解。Toodle 可以持久化多个 Resource Service revision，运行时也可给各自实例分配不同 route，但选择兼容 release、执行数据迁移、呈现视图和决定 Agent 权限仍需显式的 compatibility 与 policy 规则；route name 并不能自动解决版本依赖。

### Q5 · 签名路由的开销 vs 安全

Route/transport authenticity mechanism 与 Authority trust decision 必须分开：C-Two 可以承载可验证 route provenance 或 transport identity，但它不能决定哪个 publisher/Authority 值得信任，也不能用 `ContractReleaseRef` digest 代替签名、授权或 revocation。具体 wire mechanism 需先定义威胁模型、密钥轮换和 relay mesh 传播语义，再进入 C-Two roadmap；Toodle 等 Authority 层负责解释信任与 policy。

### Q6 · Agent 的"责任归属"

如果 Agent 代用户执行写操作并产生错误，Toodle 必须在 PolicyDecision 与 audit context 中区分请求者、Agent、授权者、审批者和实际 Resource Service revision；这些身份不能由 C-Two route 或调用者自报 tag 推导。具体责任与合规规则属于 Authority/领域制度，C-Two 只应可靠透传经过定义的 call metadata 并保留机制层 trace correlation。

## 附录 A · 术语

| 术语 | 含义 |
|---|---|
| **CRM** | Core Resource Model — 接口契约。描述一个资源对外方法签名的接口类，带命名空间和版本（`@cc.crm(namespace, version)`）。 |
| **Resource（C-Two runtime）** | 实现 CRM 契约的运行时对象。当前 Python SDK 中是一个有状态、有方法的普通 Python 类，通过 `cc.register(...)` 暴露给外界。 |
| **Resource（Toodle durable）** | 由 Toodle Catalog 治理的持久内容或状态身份，具有 revision/update model；它不是 live CRM route，也不会被 `connect` 隐式服务化。 |
| **Resource Service** | Toodle 中绑定 Resource、CRM contract set、policy projection 与 activation policy 的逻辑服务；激活后才产生 C-Two RuntimeInstance route。 |
| **c-two** | 分布式资源运行时协议（本仓库）。只做机制，不做策略。 |
| **Toodle** | **T**rust-**O**riented **O**pen **D**istributed **L**inking **E**nvironment。独立的可信开放资源环境，拥有 Catalog、Resource/Resource Service identity、revision、graph/tree views、policy、activation 与 federation。 |
| **Gridmen** | 基于 Toodle 的人在回路智能地理编辑器，面向地理数据工作者。 |
| **Resource Graph** | Toodle Catalog 维护的持久 Resource/Resource Service identity 与关系图；资源树、tag 和搜索结果是其可选视图或投影。 |
| **Client** | 任何调用 `cc.connect(...)` 消费资源的代码（脚本、函数、Agent）。不注册自己的资源，就只是一个客户端进程。是 c-two 里"无状态一等公民服务"的对应物（见 §3）。 |
| **复合进程** | 一个既调用 `cc.register(...)` 托管自己的资源、又调用 `cc.connect(...)` 消费外部资源的进程；建模扩展最常见的形态（见 §3 / §8）。在 c-two 里这不是独立角色，而是"同一个进程同时承担托管者和客户端两种职责"。 |
| **Extension** | 在 Toodle / Gridmen 中可装卸的功能单元，通常包含资源提供 + CRM 消费 + UI 面板。 |
| **Agent Tool Schema** | Agent 可理解的工具合同；CRM 契约可自动导出为 MCP 等 tool schema。 |
| **Human-in-the-loop (HITL)** | 任何 Agent 发起的写操作在生效前需经用户确认的工作流。 |
| **fastdb** | 零反序列化 bytes→ORM→feature 库，c-two 的跨语言 codec 主干。 |
