# C-Two Endgame — 分布式资源运行时架构

> **Status:** Vision (living document)
> **Scope:** 为 C-Two 的最终形态、下游框架（Toodle）与应用（Gridmen）定义分层边界
> **Audience:** C-Two 核心开发者、Toodle / Gridmen 贡献者、Agent / Extension 作者

## 0. TL;DR

C-Two 的 endgame 不是"更快的 RPC"，而是一个**以资源为一等公民的分布式运行时协议**，
让跨进程、跨机、跨语言的资源对象以**统一的 CRM 契约**被发现、调度和消费。在它之上，
**Toodle**（Tag-Oriented Open Data Linking Environment）是一个**领域无关**的资源图 +
策略层；**地理资源目录**是 Toodle 在 GIS 这一个领域的实例化（未来也可以有 ML、金融、
IoT 目录）；**Gridmen** 则是地理目录之上的**人在回路智能地理编辑器**。

本文按四层刻画 endgame，并专门回应四组容易被误读的问题：

1. **Toodle 是不是 c-two 的一部分？** —— 不是。c-two 只提供机制，Toodle 提供策略。
2. **Toodle 是不是 GIS 专用？** —— 不是。Toodle 领域无关，地理目录是它的一个实例。
3. **Agent 在架构里扮演什么？** —— CRM 是 Agent 的 tool schema，但空间任务的最终责任人是人。
4. **"建模扩展"到底是什么形式？** —— 它是 c-two 中的**复合进程**（既注册资源又消费外部资源），
   可自包含资源、依赖外部资源、对外暴露 CRM。

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
│     • 以 fastdb 作为零反序列化主干，确保跨语言 codec 一致性            │
│     • 注：其他领域（ML / 金融 / IoT）可以有自己的 L3 Catalog          │
├──────────────────────────────────────────────────────────────────────┤
│ L2  Toodle — Tag-Oriented Open Data Linking Environment（领域无关）    │
│     • 资源图（Resource Graph）：以标签组织 CRM 的语义拓扑             │
│     • AuthN / AuthZ：身份、租户前缀、ACL；把签名身份注入 c-two metadata│
│     • Workspace / Project 抽象：元数据持久化、版本、协作              │
│     • Extension Registry：发现、安装、依赖求解、能力声明              │
│     • Agent Runtime：把 CRM 暴露成 tool schema 给 LLM 智能体          │
├──────────────────────────────────────────────────────────────────────┤
│ L1  c-two — 分布式资源运行时协议                                      │
│     • CRM 契约  • 注册-获取  • IPC/HTTP 传输  • Relay mesh       │
│     • auth_hook + call metadata 透传（把安全决策权让给 Toodle）        │
│     • Python / TypeScript / Rust 多语言客户端                          │
└──────────────────────────────────────────────────────────────────────┘

正交视图：计算模型（见 §3）
  客户端 (无状态)  ↔  资源 (有状态)  ↔  复合 (托管+消费)
  —— 可出现在任何一层的任何进程里
```

**三个关键的边界原则：**

1. **c-two 只做机制，不做策略。** 安全、工作区、协作语义全部属于 Toodle；c-two 只提供
   钩子（`auth_hook`、call metadata）让上层实现它们。
2. **Toodle 不做领域、不做 UI。** Toodle 只管"资源图怎么组织、谁能看、谁能写、怎么组合"。
   任何领域专用的 CRM（地理 / ML / 金融）都住在 L3。
3. **Gridmen 是地理目录的一个前端。** 任何其他客户端（CLI、Python 脚本、Jupyter、
   移动端）都应该能用同一套资源图，只是 UX 不同。

## 2. L1 · c-two 协议层：机制非策略

### 2.1 形态

c-two 是一个**资源运行时协议**。"资源"指一个有状态、有方法的对象；"协议"指描述这个对象
并远程调用它的规则。它的最小集合只有三件东西：

- **CRM**（Core Resource Model）：一个普通 Python 对象，持有状态与域逻辑。
- **CRM 契约**（Core Resource Model — Contract）：接口声明，带命名空间和版本（`@cc.crm(namespace, version)`），
  方法体是 `...`。它就是这个资源对外的**契约**。
- **传输与发现**：IPC / HTTP / Relay Mesh，负责把调用送到对的进程。

### 2.2 为什么 c-two 不该再扩张

过去几轮的探索反复证明一件事：**任何看起来"应该加进 c-two"的业务概念，本质上都属于 Toodle**。
具体边界在 [§9](#9-协议边界清单c-two-做什么不做什么) 里详列，核心结论是：

| 放进 c-two | 不放进 c-two |
|---|---|
| 零拷贝、异步、buffer 生命周期 | 认证、授权、租户 |
| 方法级并发（`@cc.read`/`@cc.write`） | 多节点主从仲裁 |
| CRM 版本、Hold 语义、Metadata 透传 | Workspace、Project、图层树 |
| fastdb 零反序列化 codec | 文件格式、地理语义 |
| Tag 作为**路由提示** | Tag 作为**身份或能力** |

### 2.3 两个即将补齐的关键能力（撑起 endgame 的前提）

| 能力 | 作用 | 对应问题 |
|---|---|---|
| **`auth_hook` + call metadata 透传** | 让 Toodle 注入签名身份、拦截非法调用 | Toodle 如何在 c-two 上构建 AuthN/AuthZ |
| **TypeScript 客户端 + fastdb 统一 codec** | 前端一等公民，浏览器直连 CRM 不过翻译层 | 多语言一致性、零反序列化延展到 Web |

这两点一旦到位，后续 Toodle 的演进基本只是"往上加"，不需要回头改 c-two。

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

## 4. L2 · Toodle：标签语义与资源图（领域无关）

### 4.1 命名由来

**Toodle = Tag-Oriented Open Data Linking Environment**。相对于 py-noodle 时期以 **node 树**
（路径字符串为唯一键）为组织骨架，Toodle 的核心骨架是**标签图**（tag graph）。标签给资源
**赋予语义**，CRM 给资源**圈出行为边界**，二者结合形成一个可被人和 Agent 共同导航的资源
网络。

**一句话版本：**

> Toodle 的职责是"给资源赋予语义、管理访问、编排协作"；CRM 定义资源能做什么，
> tag 定义资源在语义上是什么，Toodle 在这两者之上做策略决定。

Toodle **本身不包含任何地理语义**。地理领域的 CRM 合同住在 L3（§5）。同一个 Toodle 实例
理论上可以同时承载地理资源、ML 资源、金融资源等多个领域目录。

### 4.2 资源图（Resource Graph）

Toodle 维护一张图：

- **节点** = 一个 CRM 实例（通过 c-two 注册到 relay，Toodle 拿到它的 name/namespace/version）
- **标签** = 附在节点上的键值对，形如 `domain=hydro`、`tenant=proj_42`、
  `capability=gpu`、`role=primary`、`owner=user_123`
- **边** = 资源之间的语义关系：`depends_on`（建模依赖）、`derived_from`（派生血缘）、
  `mounted_in`（工作区归属）、`references`（弱引用）

**这张图是 Toodle 的核心，不是 c-two 的。** c-two 只提供 tag 作为**路由提示**，Toodle
在它之上做语义、认证、授权、生命周期。

### 4.3 三类 tag 与它们的信任模型

区分这三类是 Toodle 安全模型的基石（详见 [§9](#9-协议边界清单c-two-做什么不做什么)）：

| 类别 | 例子 | 可否客户端声明 | 是否信任 |
|---|---|---|---|
| **描述性 tag**（hint） | `zone=cn-east`、`version=2.1` | ✅ | ❌ 不能用于授权 |
| **身份属性** | `owner=user_123`、`tenant=proj_A` | ❌，Toodle 注入 | ✅ 需签名 |
| **能力授予**（capability） | `acl=read,write`、`role=primary` | ❌，Toodle 颁发 | ✅ 强校验 |

c-two 全程不解释这三者的含义 —— 它只负责**把 tag 安全地从注册端送到查询端**。解释权在 Toodle。

### 4.4 Workspace 与协作

Workspace 是 Toodle 给用户看的"项目"。一个 workspace 包含：

- 一组挂载的 CRM 实例（通过 tag 筛选或显式引用）
- 图层树 / 符号 / 视口等**轻量元数据**（存在 Toodle 的持久化层，而不是资源 CRM 里）
- 一组 Extension 的启用声明
- 一组成员与 ACL

**协作不靠 OT/CRDT，靠 c-two 的方法级并发。** 多用户同时连同一 `IVectorLayer`，
`@cc.read` 并行、`@cc.write` 在同一 feature 粒度上串行；复杂一致性需求（比如 feature
级 CRDT）由具体 CRM 的实现自行承担，Toodle 不在协议层强制。

---

## 5. L3 · 地理资源目录：领域 CRM 原语

L3 不是一个"crate"，而是一组**约定**：哪些地理资源类型应当被标准化，使得 Toodle / Agent /
扩展 / UI 面板都能依赖**可预期的类型语义**。它和 Toodle 的关系是 —— **Toodle 是容器，
地理目录是其中一类装载物**。未来可以并列出现 ML 目录、金融目录等 L3 兄弟。

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
（[§8](#8-建模扩展--复合进程对称的自包含与依赖)），通过**依赖 L3 CRM** 来读写通用资源。

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
| 类型化参数 | `@cc.transfer(input=..., output=...)` + transferable schema |
| 版本 | CRM `version` 字段 + 注册时的 `icrm_ver` |
| 权限边界 | `@cc.read` / `@cc.write` + Toodle 的 tag 过滤 + auth_hook |

这意味着 Toodle 可以**自动把资源图里的 CRM 导出为标准 tool 协议**（MCP 为首选），无需
为每个资源手写 tool wrapper。

### 6.2 Agent Runtime 的三层调度

```
┌────────────────────────────────────────────────┐
│ Planner (LLM)                                  │
│   输入: 用户自然语言目标 + 当前 workspace 快照 │
│   输出: 一个 CRM 方法调用的 DAG              │
├────────────────────────────────────────────────┤
│ Toodle Agent Runtime                           │
│   • 用 tag 过滤出候选 CRM                      │
│   • 用 CRM schema 校验调用类型                │
│   • 对写调用插入"人工审批"检查点              │
│   • 执行、收集结果、回传给 Planner            │
├────────────────────────────────────────────────┤
│ c-two Protocol                                 │
│   • auth_hook 校验调用者是 Agent 身份          │
│   • 将 `caller=agent:xxx` 写入 metadata        │
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

## 8. 建模扩展 = 复合进程：对称的自包含与依赖

### 7.1 关键洞察

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

### 7.2 自包含资源 为什么重要

这是 [`sota-patterns.md`](./sota-patterns.md) 里反复强调的一点：

> CRM 本身就是普通资源对象，它可以直接参与计算业务，同时隐式对外提供资源服务。

这意味着**建模扩展不需要拆成"计算进程 + 资源进程"两个部分**。潜水方程求解器的网格状态既
是它自己计算的对象，又是外部可访问的资源；C-Two 的 registry 只是让后者成为可能，而不强制
前者和后者分离。

**端到端的好处：**
- 最优局部性：计算和资源同进程，零 IPC 开销
- 外部协作不失：其他扩展、Agent、前端依然能通过 CRM 访问状态
- 生命周期清晰：模型进程活着 → 资源可见；模型进程结束 → 资源随之注销

### 7.3 扩展 manifest（与 Toodle 的契约）

一个扩展（简化形式）：

```toml
# toodle.extension.toml
[extension]
name = "swmm-coupler"
version = "1.2.0"

[provides]
# 这个扩展对外提供的 CRM（其他扩展/Agent/UI 可调用）
icrms = ["hydro.SWMMModel@0.3", "hydro.Coupler@0.3"]

[requires]
# 这个扩展需要消费的 CRM 合同
icrms = ["geo.IVectorLayer@>=1.0", "geo.IDEM@>=1.0"]

[panels]
# 前端面板（注入到 Gridmen）
entry = "dist/index.js"
slots = ["sidebar.right", "layer.context-menu"]

[permissions]
# 声明对 workspace 资源的访问要求，由 Toodle 在安装时向用户确认
read = ["geo.IVectorLayer", "geo.IDEM"]
write = ["hydro.*"]

[agent]
# 是否允许 Agent 调度本扩展提供的 CRM 方法
expose_to_agents = true
# 哪些 write 方法必须经过人工审批
require_human_approval = ["run_simulation", "override_boundary"]
```

**此处没有 c-two 的身影**：manifest 描述的是 Toodle 层的契约（安装、授权、Agent 曝光），
c-two 只管 CRM 在 relay 上注册成什么名字、用什么传输。

### 7.4 扩展之间的依赖

`requires.icrms` 是版本范围，Toodle 在安装时：

1. 在资源图里解析：是否已有匹配的 资源实例？
2. 若无，是否有另一个扩展提供该 CRM？有则提示用户一并安装。
3. 若冲突（A 要 `@1.x`，B 要 `@2.x`），启动两个独立 资源实例（c-two 的 name 机制天然支持
   `hydro.SWMMModel-v1` / `hydro.SWMMModel-v2` 并存）。

这是 c-two 现有能力能原生承载的。

## 9. 协议边界清单：c-two 做什么、不做什么

这张表是评判任何新特性要不要进 c-two 的检验器。**放错层比不实现更糟糕**，因为错层的抽象
会污染所有下游框架。

| 能力 | 住在哪一层 | 理由 |
|---|---|---|
| CRM 注册与获取 | **c-two** | 资源运行时的最基本机制 |
| IPC / HTTP / Relay 传输 | **c-two** | 跨进程/跨机能力 |
| 方法级读写并发 (`@cc.read`/`@cc.write`) | **c-two** | 单 CRM 内部的调度 |
| Tag 作为**路由过滤**（不可信 hint） | **c-two** | 仅影响 resolve() 的候选集 |
| Buffer 生命周期、零拷贝、Hold | **c-two** | 性能原语 |
| CRM 版本字段 + 传播到 relay | **c-two** | 让 Toodle 做版本解析 |
| `auth_hook` + call metadata 透传 | **c-two（待补）** | 让 Toodle 构建安全层的钩子 |
| Dry-run / 审批预估 | **c-two（待补）** | 让 Toodle / Agent 做影响分析 |
| Streaming 返回 | **c-two（待补）** | 长任务进度回传 |
| — | — | — |
| 认证 (AuthN) | **Toodle** | JWT / OIDC / mTLS 属于策略 |
| 授权 (AuthZ) / ACL / 租户隔离 | **Toodle** | 身份解释权属于 Toodle |
| Tag 作为**身份/能力**（签名） | **Toodle** | c-two 不解释 tag 语义 |
| 主从仲裁 / leader election | **Toodle 或 K8s** | 业务决策，c-two 不假设 |
| Workspace / Project / 图层树 | **Toodle** | 元数据管理 |
| 扩展安装 / 依赖解析 | **Toodle** | 生态治理 |
| Agent tool schema 导出 | **Toodle** | 但 c-two 需支持 CRM 自省 |
| 审计 / 日志 / 合规 | **Toodle** | 身份绑定 |
| — | — | — |
| 图层符号 / 渲染 / 视口 | **Gridmen** | UX |
| 文件格式（GeoTIFF / Shapefile 等） | **L3 CRM 实现** | 与协议无关 |
| 目视解译、矢量化 | **Gridmen + 专用 CV 模型** | 不是协议能解决的 |
| CRDT / OT 协作算法 | **具体 CRM 的实现** | 视资源特性而定 |

## 10. 演进路线（非日程，仅相对顺序）

按"相对顺序"而非"时间点"刻画，因为每个里程碑的时长取决于人力与外部需求。

### Milestone M1 · c-two 自立（当前）
- Relay mesh 发现、config 架构、transferable redesign —— 基本就绪
- **收尾项**：`auth_hook` + call metadata、dry-run 钩子、streaming 返回

### Milestone M2 · TypeScript 客户端 + fastdb 统一 codec
- 把"跨语言"从口号变成事实
- 这是前端成为一等公民的技术前置
- 依赖：`@cc.transfer` 的 codec 标注（Python 优先，fastdb-eligible 的才曝露给 TS）

### Milestone M3 · L3 Geospatial CRM Catalog 起步
- 先确定 2–3 个最核心的 CRM（建议：`IVectorLayer`、`IRasterLayer`、`ITiledGrid`）
- 给出参考实现 + 版本治理流程
- 这是上层 Toodle / Gridmen 能稳定迭代的前提

### Milestone M4 · Toodle 起步（py-noodle → Toodle 迁移）
- 弃用 py-noodle 的全局 SQLite 锁
- 实现资源图 + 三类 tag 区分 + auth_hook 接入
- Extension manifest & registry

### Milestone M5 · Gridmen endgame
- Electron + Web 双端外壳
- Extension host
- Agent runtime（基于 CRM 自动导出 MCP tool schema）
- 人在回路审批流

### Milestone M6 · 联邦 / 公共 relay / 签名路由
- 公共 relay 上托管的 CRM 可被多机构消费
- Route entry 由 relay 签名以防跨机构伪造
- 这时 c-two 才真正变成"地理资源的互联网协议"

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

QGIS 是纯 offline，Felt 是纯 online。Gridmen 需要同时支持"本地工作区（本地 relay + 本地
CRM）"与"团队工作区（共享 relay + 云端 CRM）"。这两种模式的 Workspace 持久化后端差异巨大
（SQLite vs PostgreSQL），Toodle 的存储抽象需要从一开始就考虑双形态。

### Q3 · 数据如何"入库"？

用户硬盘上一个 GeoTIFF 如何变成 CRM？两条路都需要：
- **A · Upload 到 Toodle blob store → Toodle 启动 CRM 进程持有它**。适合云部署。
- **B · 本地 importer 扩展 → 在本地进程启一个资源 → federation 到远端 workspace**。适合
  "数据重但不能上传"的场景（涉密、超大体积）。

UX 差异很大，需要 Gridmen 层面做统一。

### Q4 · 版本依赖地狱

两个扩展分别要求 `IVectorLayer@1.x` 和 `IVectorLayer@2.x`，同 workspace 能否并存？c-two
的 name 机制支持共存（两个独立的 资源实例），但 Workspace 视图层如何展示、图层树如何合并、
Agent 如何路由，这些都属于 Toodle 的设计空间。

### Q5 · 签名路由的开销 vs 安全

Relay 之间的路由项要不要强制签名？不签名 → 跨机构联邦时有风险；强制签名 → 每跳加开销，
且需要密钥分发基础设施。建议做成可选特性（`c2-http --signing-enabled`）。

### Q6 · Agent 的"责任归属"

如果 Agent 代用户执行了写操作并产生错误，审计链条如何界定责任？短期方案：所有 Agent 发起的
写操作在 Toodle 层落一条签名审计记录，绑定"审批人 = 当时的用户"。长期还需要法律/合规层面的
进一步讨论，这超出 C-Two 本身的范围。

## 附录 A · 术语

| 术语 | 含义 |
|---|---|
| **CRM** | Core Resource Model — 接口契约。描述一个资源对外方法签名的接口类，带命名空间和版本（`@cc.crm(namespace, version)`）。 |
| **Resource** | 实现 CRM 契约的运行时对象。一个有状态、有方法的普通 Python 类，通过 `cc.register(...)` 暴露给外界。命名按领域语义（`NestedGrid`、`PostgresVectorLayer`）。 |
| **c-two** | 分布式资源运行时协议（本仓库）。只做机制，不做策略。 |
| **Toodle** | **T**ag-**O**riented **O**pen **D**ata **L**inking **E**nvironment。构建在 c-two 之上的资源图 / 策略层，py-noodle 的进化形态。 |
| **Gridmen** | 基于 Toodle 的人在回路智能地理编辑器，面向地理数据工作者。 |
| **Resource Graph** | Toodle 维护的 "资源节点 + tag + 语义边" 图结构。 |
| **Client** | 任何调用 `cc.connect(...)` 消费资源的代码（脚本、函数、Agent）。不注册自己的资源，就只是一个客户端进程。是 c-two 里"无状态一等公民服务"的对应物（见 §3）。 |
| **复合进程** | 一个既调用 `cc.register(...)` 托管自己的资源、又调用 `cc.connect(...)` 消费外部资源的进程；建模扩展最常见的形态（见 §3 / §8）。在 c-two 里这不是独立角色，而是"同一个进程同时承担托管者和客户端两种职责"。 |
| **Extension** | 在 Toodle / Gridmen 中可装卸的功能单元，通常包含资源提供 + CRM 消费 + UI 面板。 |
| **Agent Tool Schema** | Agent 可理解的工具合同；CRM 契约可自动导出为 MCP 等 tool schema。 |
| **Human-in-the-loop (HITL)** | 任何 Agent 发起的写操作在生效前需经用户确认的工作流。 |
| **fastdb** | 零反序列化 bytes→ORM→feature 库，c-two 的跨语言 codec 主干。 |
