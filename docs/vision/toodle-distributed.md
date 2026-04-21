# Toodle 分布式架构 — M4 设计愿景

> **Status:** Vision (living document)
> **Scope:** 定义 Toodle (L2) 的 M4 核心架构，澄清与 c-two (L1) 的边界，并明确 fastdb 作为生态准入契约的地位
> **Audience:** Toodle 核心开发者、c-two 维护者、fastdb 维护者、L3 领域目录作者、Gridmen 等业务前端开发者

## 0. TL;DR

Toodle 是 c-two 之上的**领域无关策略层**，提供资源图、工作区、认证授权和扩展注册。本文给出 Toodle 的 M4 架构方案，并强调一个**关键启动顺序**：

> **Toodle 的启动空间取决于 fastdb 的能力到位。** fastdb 是 Toodle 生态唯一允许的 CRM codec，决定了协议分发、跨语言一致性、Schema Registry 的可行性。fastdb 不到位，Toodle 无法启动。

M4 核心范围：**资源图（Resource Graph） + 认证授权（AuthN/AuthZ） + 工作区（Workspace）**。Agent Runtime、Extension Registry 推迟到 M5+。

实现选型：
- **语言：** Go（团队 Go 经验充足，K8s Operator 生态原生 Go，控制面无需 Rust 性能）
- **存储：** PostgreSQL（Docker 开发，生产托管或自建均可）
- **架构：** 模块化单体（单 Go 二进制，内部 package 边界清晰，未来可拆分 Agent sidecar）
- **API：** HTTP first，CLI 与各语言 SDK 都是 HTTP 客户端
- **Codec：** 强制 fastdb Feature，禁止业务自写 `@cc.transferable`

关键 API 分层：CRM 实现者使用 `import c_two as cc`；业务消费者使用 `import toodle as td`，业务侧不感知 c-two 内部细节。

---

## 1. 定位与边界

### 1.1 在四层架构中的位置

```
┌────────────────────────────────────────────────────────────────────┐
│ L4  业务前端：Gridmen / CLI / Jupyter / 移动端                      │
│      消费 Toodle 提供的资源图，通过 td.connect() 访问 CRM            │
├────────────────────────────────────────────────────────────────────┤
│ L3  领域 CRM 目录：Geospatial / ML / 金融 / IoT                     │
│      纯类型定义包（fastdb Feature 子类 + CRM 契约 interface）        │
│      不携带运行时序列化代码，依赖 fastdb 提供 codec                   │
├────────────────────────────────────────────────────────────────────┤
│ L2  Toodle —— 本文件主题                                            │
│      • Resource Graph（资源图 + Workspace Tree View 投影）           │
│      • AuthN/AuthZ（可插拔 AuthProvider）                            │
│      • Workspace（命名空间 + 隔离 + 跨 workspace 引用）               │
│      • Schema Registry（fastdb Schema JSON 存储与版本校验）           │
│      • c-two Bridge（auth_hook 注入 + relay 转发）                   │
├────────────────────────────────────────────────────────────────────┤
│ L1  c-two —— 资源运行时协议（机制层）                                │
│      CRM 契约、注册-获取、IPC/HTTP/Relay、call metadata 透传         │
├────────────────────────────────────────────────────────────────────┤
│ L0  fastdb —— 跨语言 codec（Toodle 生态准入契约）                    │
│      Feature 类型系统、零拷贝列式访问、Schema JSON、codegen          │
└────────────────────────────────────────────────────────────────────┘
```

**注意 L0 的位置：** fastdb 在依赖关系上**横跨** c-two 和 Toodle。c-two 不强制要求 fastdb（保持机制层中立，pickle fallback 仍可用），但 Toodle **强制要求** fastdb——这是 Toodle 生态的"准入契约"。详见 §4。

### 1.2 Toodle 不做什么

| Toodle 做 | Toodle 不做（由其他层负责） |
|-----------|----------------------------|
| 资源图组织、Workspace、ACL | 实际的资源计算（c-two 资源进程做） |
| AuthN/AuthZ 决策 | 用户身份的最终来源（AuthProvider 插件做） |
| Schema JSON 存储与版本校验 | Schema 序列化/反序列化实现（fastdb 做） |
| CRM 发现与路由 | CRM 之间的消息传输（c-two relay 做） |
| 资源生命周期元数据（manifest） | 资源进程的拉起与监控（M4 手工，M5+ 由 K8s Operator） |
| 跨 workspace 引用图 | 文件格式、地理语义（L3 领域目录做） |
| 三类 Tag 的语义解释 | UI 树视图渲染（L4 前端做） |

### 1.3 三个关键边界原则

1. **Toodle 不重新发明 c-two 已有的机制。** 并发控制（`@cc.read`/`@cc.write`）、buffer 生命周期、CRM 版本协商都在 c-two；Toodle 只在其上叠加策略。
2. **Toodle 不持有领域知识。** "什么是图层"、"什么是 DEM" 是 L3 的事；Toodle 只看到"一个 CRM 实例 + 一组 tag + 一个所有者 workspace"。
3. **Toodle 是 c-two 的下游。** Toodle 包含 c-two（业务侧只引入 toodle 即可），不存在 `c3 toodle` 这种倒置关系。Toodle 有自己的 CLI 入口（`toodle`），自己的 SDK（`toodle-py`、`toodle-ts`、`toodle-go`）。


## 2. 核心抽象

### 2.1 Resource（资源）

**Toodle 视角下的资源**是一个三元组：

```
Resource = (crm_contract_ref, runtime_address, metadata)
   crm_contract_ref:  指向 Schema Registry 中的 CRM 契约版本
   runtime_address:   c-two 寻址信息（ipc:// 或 http://relay/...）
   metadata:          owner_workspace, tags[], visibility, manifest, ...
```

资源在 Toodle 中**只是元数据**，真实状态由对应的 c-two 资源进程持有。Toodle 不缓存资源数据，不做读写代理——所有数据流量都由业务侧通过 `td.connect(Grid, path=...)` 直连 c-two relay。

### 2.2 Resource Graph（资源图）

资源之间的关系用**有向图**建模：

- **节点：** Resource（外部 CRM 实例）或 Group（纯逻辑分组节点）
- **边：** 三类
  - `contains`：父子从属（用于 Workspace Tree View 投影）
  - `depends_on`：业务依赖（一个资源消费另一个资源的输出）
  - `references`：跨 workspace 引用（详见 §7）

图存储是**真相之源**。Workspace Tree View 是基于 `contains` 边和 tag 过滤的**虚拟投影**，用于驱动 Gridmen 等前端的资源浏览器（类似 VSCode Explorer）。同一个资源可以出现在多个 Tree View 中。

### 2.3 三类 Tag（沿用 vision/endgame-architecture.md）

| Tag 类型 | 例子 | 谁解读 |
|---------|------|--------|
| **descriptive** | `region:east-china`, `crs:wgs84`, `year:2024` | UI 过滤、Agent 上下文 |
| **identity** | `owner:alice`, `tenant:lab-gis` | Toodle AuthZ 评估 |
| **capability** | `read:public`, `write:owner-only` | Toodle AuthZ 评估 |

Tag 是**不可变的**——修改 tag = 创建新版本。这避免了"标签飘移"导致的权限语义混乱。

### 2.4 Workspace（工作区）

Workspace 是 Toodle 的**租户单位**与**命名空间根**：

- 每个 Resource 必须属于且仅属于一个 owner workspace
- Workspace 内资源用路径访问：`td.connect(Grid, path='my-ws/terrain/dem_30m')`
- Workspace 之间默认隔离；公共可见性资源可被其他 workspace 只读引用（§7）
- Workspace 是 ACL 评估的边界

### 2.5 AuthProvider（可插拔认证）

```go
type AuthProvider interface {
    // 验证凭据，返回签名后的 Identity
    Authenticate(ctx context.Context, credentials []byte) (*Identity, error)
    // 验证已签发的 token，返回 Identity
    ValidateToken(ctx context.Context, token string) (*Identity, error)
}

type Identity struct {
    Subject   string            // 用户/服务唯一 ID
    Tenant    string            // 所属租户
    Claims    map[string]string // 自定义 claim
    ExpiresAt time.Time
}
```

M4 内置 `JWTProvider`；后续实验室 IdP 实现同一接口即可挂入，不改 Toodle 核心代码。


## 3. 架构方案：模块化单体

### 3.1 设计选择

经过对单体（A）、模块化单体（B）、微服务（C）三种方案对比后，M4 采用**方案 B：模块化单体**。

理由：
1. **M4 RPS 不需要拆服务** —— 控制面流量（图查询 + 鉴权 + workspace CRUD）远低于需要微服务化的阈值
2. **模块边界从第一天清晰** —— 通过 Go internal package + interface 强制边界，未来拆 Agent sidecar 时只改 `api/` 和 `main.go` 装配
3. **运维负担最小** —— 单二进制 + PostgreSQL，便于本地开发、CI 测试和早期部署
4. **演进路径清晰** —— M5 加 Agent sidecar 时，`schema/`、`graph/` 直接被 sidecar import 复用

### 3.2 内部模块划分

```
toodle/                        # 单 Go 二进制
├── cmd/toodle/main.go         # 装配入口：依赖注入、启动 HTTP server
├── internal/
│   ├── api/                   # HTTP Gateway（业务侧 SDK 唯一入口）
│   │   ├── graph_handler.go   # /v1/graph/*
│   │   ├── auth_handler.go    # /v1/auth/*
│   │   ├── workspace_handler.go
│   │   ├── schema_handler.go  # /v1/schema/*（Schema Registry）
│   │   └── c2bridge_handler.go # /v1/internal/c2-bridge/*
│   ├── graph/                 # Resource Graph Engine
│   │   ├── engine.go          # CRUD、邻接查询、路径解析
│   │   └── treeview.go        # Workspace Tree View 投影
│   ├── auth/                  # 可插拔 AuthProvider
│   │   ├── provider.go        # Provider interface
│   │   ├── jwt_provider.go    # M4 内置实现
│   │   └── authz.go           # ACL 评估器
│   ├── workspace/             # Workspace 生命周期与跨 ws 引用
│   │   ├── manager.go
│   │   └── reference.go       # 跨 workspace 引用 + capability grant
│   ├── schema/                # fastdb Schema JSON Registry
│   │   ├── registry.go        # 存储 + 查询
│   │   └── version.go         # 版本兼容性校验
│   ├── store/                 # Repository 层（PostgreSQL）
│   │   ├── graph_repo.go
│   │   ├── auth_repo.go
│   │   ├── workspace_repo.go
│   │   └── schema_repo.go
│   └── c2bridge/              # 与 c-two relay 的桥接
│       ├── auth_hook.go       # 把 Toodle Identity 注入 c-two metadata
│       └── relay_proxy.go     # name 解析转 ipc/http 地址
└── pkg/
    └── apitypes/              # 公开的 HTTP API 类型定义（被 SDK import）
```

### 3.3 关键架构原则

1. **Repository pattern 强制** —— 上层模块（`graph/`, `auth/`, `workspace/`, `schema/`）**禁止**直接写 SQL，所有 PostgreSQL 访问走 `store/` 层。便于 mock 测试、未来切换存储后端。
2. **接口隔离** —— 模块间通过 interface 交互，例如 `auth.Provider`、`store.GraphRepository`。便于扩展和测试。
3. **HTTP API first** —— 没有"内部 Go API"和"外部 HTTP API"两套——CLI、Python SDK、TS SDK 都是 HTTP 客户端，确保多语言一致性。
4. **可观测性内建** —— `internal/` 下每个模块对外暴露 metrics（Prometheus）和 trace（OpenTelemetry），从第一天就集成。

### 3.4 与 c-two relay 的关系

Toodle 不替代 c-two relay。两者职责清晰分离：

| 关注点 | Toodle | c-two relay |
|-------|--------|-------------|
| 资源元数据存储 | ✅ PostgreSQL | ❌ |
| name → 地址解析 | ✅ 路径解析 + 投影 | ✅ 已有的 name 路由 |
| 鉴权决策 | ✅ AuthZ 评估 | ❌（执行 auth_hook） |
| 实际 RPC 转发 | ❌ | ✅ |
| call metadata 透传 | 注入 Identity | 透传 |

业务侧调用流程：
```
td.connect(Grid, path='ws-a/terrain/dem')
  ↓ HTTP
Toodle: 解析 path → 查 graph → 评估 ACL → 返回 c-two 地址 + signed token
  ↓ 直连
c-two relay: 路由到资源进程，auth_hook 验证 token
  ↓
业务后续所有 RPC 调用直走 c-two，**不经过 Toodle**
```

这保证了 Toodle 不在数据流热路径上，控制面流量稳定可控。


## 4. fastdb 作为生态准入契约

> **本节是整个文档的核心。Toodle 能否启动，取决于 fastdb 能力是否到位。**

### 4.1 为什么 CRM 不能用 Protobuf

c-two 的 `@cc.transferable` 与 Protobuf 在本质上不同：

| 维度 | Protobuf | c-two transferable |
|------|----------|---------------------|
| 描述形态 | `.proto` schema | Python class + 序列化代码 |
| 序列化代码 | 由 `protoc` 自动生成 | 用户手写 `serialize`/`deserialize`/`from_buffer` |
| 数据形态 | 业务级类型（嵌套消息） | 科学计算类型（numpy 数组、列式数据、零拷贝 buffer） |
| 跨语言一致性 | 编译期保证 | 仅 Python，且 server/client 必须共享代码 |
| 大数据支持 | 弱（repeated 字段效率低） | 强（buffer protocol + SHM） |
| 运行时依赖 | 仅 protoc 生成代码 | 携带 numpy/pyarrow 等领域依赖 |

**结论：** c-two 的 transferable 携带运行时代码与领域依赖，根本无法像 protobuf 那样"生成 stub 然后各方独立维护"。client 和 server 必须**共享同一份实现代码**。这导致协议分发陷入两难：
- 把协议打成 Python 包 → TypeScript / Go / C++ 客户端没法用
- 让每种语言重新实现 → 协议变更同步噩梦，版本永远不一致

### 4.2 fastdb 解法

fastdb 已经具备的能力（已在 [fastdb 仓库](https://github.com/world-in-progress/fastdb)中验证）：

| 能力 | 现状 |
|------|------|
| 声明式 schema（Python type hints） | ✅ `class Point(Feature): x: F64` |
| 零拷贝列式访问 | ✅ `table.column.x[:]` ~100ns |
| 大数组/tensor 高效传输 | ✅ numpy buffer protocol + SHM |
| 对象引用图 | ✅ `ref(ClassName)` 跨 table 引用 |
| Python → TS codegen | ✅ `fdb codegen --ts` 已可用 |
| Go binding | 🚧 `fastdb4go` 目录已存在，能力待补 |
| Schema JSON 中间格式 | 🚧 codegen 内部已有逻辑，未对外暴露 |
| 跨语言版本兼容性校验 | 🚧 待设计 |

### 4.3 Toodle 生态的"准入契约"

**强制要求：** 所有进入 Toodle 生态的 CRM 协议包必须满足：

1. 所有 transferable 类型必须是 `fastdb4py.Feature` 子类
2. 协议包**只包含类型声明**，不包含序列化代码
3. 协议包通过 `fdb codegen` 生成各语言 stub（Python / TS / Go / C++）
4. 协议包注册到 Toodle Schema Registry，提交 Schema JSON

```python
# ❌ 老式 transferable 写法（Toodle 生态不接受）
@cc.transferable
class GridAttribute:
    values: np.ndarray
    crs: str
    def serialize(data): ...      # 手写序列化
    def deserialize(b): ...
    def from_buffer(buf): ...

# ✅ Toodle 生态写法
from fastdb4py import Feature, F64, STR, list as fdb_list

class GridAttribute(Feature):
    values: fdb_list[F64]   # fastdb 自动列式零拷贝
    crs: STR
# 无需手写任何序列化代码；
# fdb codegen 自动产出 TypeScript / Go / C++ 等价类型；
# FastSerializer 统一处理 wire format
```

### 4.4 c-two 与 Toodle 的不同立场

| 层 | 对 fastdb 的态度 |
|----|------------------|
| **c-two L1** | 不强制。`@cc.transferable` + pickle fallback 仍然可用。c-two 是机制层，不规定 codec |
| **Toodle L2** | 强制。任何想被 Toodle 管理、被 Agent 调用、被跨语言消费的 CRM 必须用 fastdb |

类比：TCP/IP 不规定你传什么数据格式（L1 机制），但 HTTP 规定了 JSON/HTML 这套生态约定（L2 策略）。

### 4.5 协议分发流水线

```
┌────────────────────────────────────────┐
│ 上游 CRM 协议作者                       │
│   定义 Feature 类型 + CRM contract     │
│   pip 发布 cc-geo-grid==0.3.1          │
└─────────────┬──────────────────────────┘
              │ fdb codegen
              ▼
┌────────────────────────────────────────┐
│ 多语言 stub 包（自动生成）              │
│   cc-geo-grid-ts@0.3.1                 │
│   cc-geo-grid-go v0.3.1                │
└─────────────┬──────────────────────────┘
              │ 注册
              ▼
┌────────────────────────────────────────┐
│ Toodle Schema Registry                 │
│   存储 Schema JSON（类型 + 版本 + 兼容性）│
│   暴露 /v1/schema/{ns}/{name}/{ver}    │
└────────────────────────────────────────┘
```

### 4.6 Schema JSON 格式（草案）

```json
{
  "namespace": "geo.grid",
  "name": "GridAttribute",
  "version": "0.3.1",
  "compat_with": ["0.3.0", "0.2.x"],
  "fields": [
    {"name": "values", "type": "list", "element": "f64", "columnar": true},
    {"name": "crs", "type": "str"}
  ],
  "refs": [
    {"name": "parent_grid", "target": "geo.grid.GridInfo", "version": "^0.3.0"}
  ]
}
```

Toodle 用此 JSON：
- 校验客户端与资源端的版本兼容性
- 自动导出 MCP tool spec 给 Agent Runtime（M5+）
- 驱动 UI 的资源属性面板渲染


## 5. API 分层与多语言

### 5.1 两套清晰的导入

```python
# CRM 实现者（资源进程作者，少数人）
import c_two as cc
from fastdb4py import Feature, F64, STR

class GridAttribute(Feature):
    values: list[F64]
    crs: STR

@cc.crm(namespace='geo.grid', version='0.3.1')
class Grid:
    @cc.transfer(input=GridAttribute, output=GridAttribute, buffer='hold')
    def subdivide(self, attr: GridAttribute) -> GridAttribute: ...

class NestedGrid:
    def subdivide(self, attr): ...

cc.register(Grid, NestedGrid(), name='dem_30m')
cc.serve()
```

```python
# 业务消费者（应用开发者，多数人）
import toodle as td
from cc_geo_grid import Grid          # 从协议包 import 类型契约（用于类型提示与运行时校验）

td.login(token='...')                  # 通过 Toodle AuthProvider 拿到 session
with td.connect(Grid, path='ws-a/terrain/dem_30m') as grid:
    result = grid.subdivide(my_attr)   # IDE 自动补全；类型检查器可校验
```

**业务侧不出现 `import c_two`。** 所有 c-two 概念被 Toodle SDK 封装。Toodle SDK 内部会调用 c-two 客户端，但这是实现细节。

### 5.2 td.connect() 的类型化签名

CRM 类型参数是**必需的**，不可省略：

```python
def connect(crm_contract: Type[T], path: str, **opts) -> T: ...
```

理由：
- **静态类型提示** —— IDE 自动补全方法、参数、返回值类型
- **运行时校验** —— Toodle 拿到资源地址后，校验资源端注册的 namespace + version 与客户端期望一致
- **拒绝隐式契约** —— 强制业务侧明确"我要消费的是哪个 CRM"，避免协议漂移

### 5.3 多语言 SDK 矩阵

| 语言 | SDK 名称 | 实现方式 |
|------|---------|---------|
| Python | `toodle-py` | 包装 c-two Python 客户端 |
| TypeScript | `toodle-ts` | 包装 c-two TS 客户端（待 c-two TS 客户端到位） |
| Go | `toodle-go` | 直接走 c-two Rust core 的 Go binding（或调 HTTP relay） |
| C++ | `toodle-cpp` | 走 c-two HTTP relay（避免 Rust ABI 复杂性） |

**统一接入方式：** 所有 SDK 都先调 Toodle HTTP API 拿到资源地址 + signed token，再用本语言的 c-two 客户端直连资源。

### 5.4 c-two 与 Toodle 的依赖方向

```
toodle-py     ──depends_on──► c-two Python client + fastdb4py
toodle-ts     ──depends_on──► c-two TS client     + fastdb4ts
toodle-go     ──depends_on──► c-two Go binding    + fastdb4go
                                  │
                                  └─► fastdb（共享 codec）
```

**c-two 不感知 Toodle**。Toodle 是 c-two 的下游消费者。c-two 升级不需要等 Toodle，反之 Toodle 升级时声明依赖的 c-two 版本即可。


## 6. 部署模型

### 6.1 双角色：Control Plane + Agent

Toodle 设计为**双角色**部署：

```
┌─────────────────────────┐         ┌─────────────────────────┐
│   Toodle Control Plane  │◄────────┤   Toodle Agent (sidecar)│
│   (独立服务，全局唯一)    │  HTTP   │   (与资源进程同节点部署) │
│                         │         │                         │
│  • Resource Graph       │         │  • 本地资源生命周期管理  │
│  • Auth                 │         │  • metrics/log 上报      │
│  • Workspace            │         │  • 健康检查              │
│  • Schema Registry      │         │  • 与 Control Plane 心跳 │
└─────────────────────────┘         └─────────────────────────┘
```

但 **M4 只交付 Control Plane**，Agent 推迟到 M5。理由：
- M4 资源拉起用手工 CLI（`toodle resource start --manifest=...`）足以验证设计
- Agent 涉及节点级权限、镜像拉取、容器编排，复杂度高
- K8s Operator 是 Agent 的最佳载体，应在 K8s 化时一并设计，不要先做"裸机版 Agent"再重写

### 6.2 M4 部署形态

```
                 ┌──────────────┐
                 │  toodle 二进制│
                 │  (Control    │
                 │   Plane)     │
                 └──────┬───────┘
                        │
                        ▼
                 ┌──────────────┐
                 │  PostgreSQL  │
                 └──────────────┘
                        ▲
                        │
        手工 CLI 注册资源 │
                        │
        ┌───────────────┴───────────────┐
        │                               │
   ┌────────────┐                ┌────────────┐
   │ 资源进程 1 │                │ 资源进程 N │
   │ (cc.serve) │                │ (cc.serve) │
   └────────────┘                └────────────┘
        │                               │
        └───────► c-two relay ◄─────────┘
```

启动顺序：
1. `docker compose up postgres`
2. `toodle serve` 启动控制面
3. `toodle workspace create my-ws`
4. `toodle resource register --workspace my-ws --crm geo.grid.Grid@0.3.1 --address ipc:///tmp/g1 --path terrain/dem`
5. 手工拉起资源进程：`python my_grid_resource.py`（其中调用 `cc.set_address('ipc:///tmp/g1')` + `cc.register(Grid, NestedGrid())` + `cc.serve()`）
6. 业务侧 `td.connect(Grid, path='my-ws/terrain/dem')` 即可访问

### 6.3 K8s Operator（M5+）

Agent 与 K8s Operator 一并设计：
- CRD: `Resource`、`Workspace`、`SchemaVersion`
- Operator 监听 `Resource` CRD，根据 manifest 拉起 Pod，注入 Toodle Agent sidecar
- Agent 向 Control Plane 注册资源、上报心跳、接收 lifecycle 命令


## 7. 跨工作区资源共享

### 7.1 可见性

每个 Resource 有 `visibility` 字段：
- `private`（默认）：仅 owner workspace 可见
- `public`：任何 workspace 可发现 + 只读访问

### 7.2 引用模型

Workspace B 可以"引用" Workspace A 的公共资源，在 B 的 Tree View 中显示：

```
ws-a/                              ws-b/
├── terrain/                       ├── my-analysis/
│   └── dem_30m  [public]   ◄──────│   └── shared_dem  → ws-a/terrain/dem_30m
└── ...                            └── ...
```

引用是 graph 中的 `references` 边。在 ws-b 中的 `shared_dem` 是一个**引用节点**，没有独立的资源元数据，只指向 ws-a 的真身。

### 7.3 写权限

默认 public = 只读。如果 ws-b 需要写 ws-a 的资源，ws-a 的 owner 必须显式签发 **capability grant**：

```bash
toodle capability grant \
  --resource ws-a/terrain/dem_30m \
  --to-workspace ws-b \
  --permissions write \
  --expires 2026-12-31
```

Capability grant 是带签名的 token，存储在 Toodle，附带过期时间。Toodle 在评估 ws-b 的写请求时检查是否存在有效 grant。

### 7.4 悬挂引用

如果 ws-a 的资源被删除或改为 private，ws-b 中的引用节点变为 **dangling**：
- Tree View 中以灰色斜线标注，但保留节点（避免树结构突变）
- `td.connect()` 返回明确的 `DanglingReferenceError`
- 可手工 `toodle reference cleanup --workspace ws-b` 清理


## 8. M4 范围与里程碑

### 8.1 必交付

| 能力 | 验收标准 |
|------|---------|
| Resource Graph CRUD | HTTP API 完整、PostgreSQL 持久化、单元测试覆盖 ≥ 80% |
| Workspace 管理 | 创建/删除/列表；路径解析；隔离边界生效 |
| Workspace Tree View 投影 | 一个资源在多个 Tree View 中可见；UI 友好的排序与过滤 |
| AuthN：JWTProvider | login / refresh / validate；token 含 Identity claim |
| AuthZ：ACL 评估器 | 基于 owner_workspace + tag(identity/capability) 决策 |
| Schema Registry | 接收 Schema JSON、版本兼容性校验、按 ns/name/ver 查询 |
| c-two Bridge | name 解析为 c-two 地址；签发 signed token；c-two 端 auth_hook 验证 |
| 跨 workspace 引用 | public 资源只读引用；capability grant 写权限；dangling 标注 |
| `toodle` CLI | serve / workspace / resource / capability / schema 子命令 |
| `toodle-py` SDK | `td.login()` / `td.connect()` / `td.list()` |
| 部署文档 | docker-compose、K8s YAML（M4 是裸 Deployment，不是 Operator） |

### 8.2 明确不做（推迟）

| 能力 | 推迟到 |
|------|-------|
| Toodle Agent sidecar | M5 |
| K8s Operator + CRD | M5/M6 |
| Extension Registry | M5 |
| Agent Runtime（CRM → MCP tool spec） | M5 |
| HITL 工作流引擎 | M6 |
| `toodle-ts` SDK | 等 c-two TS 客户端到位后启动 |
| `toodle-go` SDK | 等 fastdb Go binding 稳定后启动 |
| 资源进程拉起自动化 | M5（Agent 完成后） |

### 8.3 验收 Demo

M4 完成时，可演示如下端到端场景：

1. 启动 Toodle Control Plane + PostgreSQL（docker-compose）
2. 实验室管理员创建两个 workspace：`lab-public`、`alice`
3. 实验室管理员注册一个公共 DEM 资源到 `lab-public/terrain/dem_30m`，可见性 public
4. Alice 在 `alice/research-2026-04` 下注册自己的私有 grid 资源
5. Alice 在自己的 workspace 中**引用** `lab-public/terrain/dem_30m`
6. Alice 用 `td.connect(Grid, path='alice/research-2026-04/shared_dem')` 读取数据
7. Alice 尝试写入该共享资源 → 被 AuthZ 拒绝
8. 实验室管理员签发 capability grant，Alice 写入成功
9. Bob（无任何 grant）尝试访问 `alice/research-2026-04/...` → 被拒绝


## 9. 启动顺序：fastdb 能力缺口（关键路径）

> **本节是整个文档的执行先决条件。** Toodle 不能在 fastdb 能力缺口未填补前启动开发。

### 9.1 阻塞原因

回到 §4 的核心论断：Toodle 强制要求所有生态 CRM 用 fastdb 作为 codec，且需要 Schema Registry 跨语言运行。这给 fastdb 提出了几项**硬性前置能力**——这些能力不到位，Toodle 的核心假设就不成立。

### 9.2 fastdb 能力缺口清单

按对 Toodle 的阻塞程度排序：

#### P0（Toodle 启动的硬阻塞）

| 缺口 | 说明 | 当前状态 |
|------|------|---------|
| **Schema JSON 标准化** | fastdb codegen 内部已有解析逻辑，但未对外定义稳定的 JSON 格式与导出 API | 🚧 需设计 |
| **Schema 版本兼容性规则** | 字段增删改时哪些是兼容变更、哪些是破坏性变更，需要规则 + 工具 | 🚧 缺失 |
| **CRM contract 编码** | fastdb 当前只编码 Feature 数据，CRM contract（方法签名 + transferable refs）的统一描述格式缺失 | 🚧 缺失 |
| **`fdb codegen` 输出稳定 stub** | 当前能生成 TS class，但需补全 CRM contract 部分 + 跨版本兼容声明 | 🚧 部分 |

#### P1（Toodle M4 内必需）

| 缺口 | 说明 | 当前状态 |
|------|------|---------|
| **fastdb4go 完整可用** | Go 是 Toodle 后端语言，Schema Registry 需要 Go 侧解析能力 | 🚧 目录已建，待实现 |
| **Schema diff 工具** | `fdb diff old.json new.json` 报告兼容性变化，CI 卡控破坏性变更 | 🚧 缺失 |
| **运行时类型校验 API** | Toodle 在签发 token 前需要校验 client 与 server 的 schema 兼容性，需要 fastdb 提供 API | 🚧 缺失 |

#### P2（Toodle M5+ 必需）

| 缺口 | 说明 |
|------|------|
| **Schema → MCP tool spec 导出** | Agent Runtime 需要把 CRM 自动暴露为 LLM 工具 |
| **fastdb4cpp** | C++ 资源进程（GIS 模拟模型常见）需要 C++ binding |
| **Fortran FFI 桥** | 部分老旧科学计算模型 |

### 9.3 推荐启动顺序

```
阶段 0：fastdb 能力补齐（M4 前置，本仓库不动）
   ├─ P0 任务全部完成
   ├─ P1 任务至少完成 "fastdb4go 完整可用" 和 "运行时类型校验 API"
   └─ 在 fastdb 仓库发版 0.4.0（含 Schema JSON spec 文档）

阶段 1：c-two 协议补齐（与阶段 0 并行）
   ├─ auth_hook + call metadata 透传 API 稳定
   └─ TypeScript 客户端首版

阶段 2：Toodle M4 启动（依赖阶段 0、阶段 1 完成）
   ├─ 单 Go 二进制 + PostgreSQL
   ├─ 模块化单体（方案 B）
   └─ 完成 §8.1 的所有交付项

阶段 3：M5 扩展（基于 M4）
   ├─ Toodle Agent + K8s Operator
   ├─ Extension Registry
   └─ Agent Runtime
```

### 9.4 风险与对策

| 风险 | 对策 |
|------|------|
| fastdb 维护带宽不足导致阻塞 | M4 启动前在 fastdb 仓库开 issue tracker，列出 P0 任务并指派负责人；c-two 维护者参与 review |
| Schema JSON 设计反复 | 在 fastdb 仓库内先发 RFC 文档，等 c-two/Toodle 维护者确认后再实现 |
| CRM contract 编码格式与 c-two 现有 `@cc.crm` 不一致 | fastdb 设计阶段引入 c-two 维护者，确保格式可由 c-two 元数据自动导出 |
| 业务侧已有大量老式 transferable | 提供 `fdb migrate transferable` 工具半自动迁移；老资源用 c-two 直连，不进 Toodle |

### 9.5 检查点

Toodle M4 开发**不得**在以下条件未满足时启动：

- [ ] fastdb 0.4.0 发版，包含 Schema JSON spec 文档
- [ ] `fdb codegen` 能产出稳定 TS + Go stub，含 CRM contract 部分
- [ ] fastdb4go 完成 reader/writer/serializer 端到端
- [ ] fastdb 提供 `fastdb.schema.is_compatible(old, new) -> CompatReport` API
- [ ] 至少一个示例 CRM 协议包用新机制重构完成（例如 `cc-geo-grid-example`）


## 10. 开放问题

以下问题在 M4 设计阶段**留为未决**，需在具体实现前或 M5 规划时回答：

1. **Agent Runtime 的接入位置** —— Agent Runtime 是独立进程还是 Toodle Control Plane 模块？与 HITL 审批如何解耦？
2. **HITL 审批工作流引擎** —— 用自研状态机、还是接入 Temporal / Cadence？权限模型如何与 Toodle Workspace 对齐？
3. **资源进程镜像规范** —— 如何约束"一个 Docker image 对应一个 CRM contract"？镜像标签如何与 schema version 关联？
4. **热升级语义** —— 业务运行中 CRM contract 发版，Toodle 如何协调 client/server 的版本切换？
5. **Schema Registry 多实例一致性** —— 未来 Toodle Control Plane 若水平扩展，Schema Registry 是否需要单独的一致性协议？
6. **与 c-two relay mesh 的集成** —— Toodle 查到的地址是否直接用 mesh 名，还是由 Toodle 完成 mesh → 具体节点的展开？
7. **Gridmen 等前端的 offline 体验** —— Workspace Tree View 能否在离线时从本地缓存渲染？
8. **多租户资源配额** —— 按 workspace 限制资源数量、存储容量、并发连接？

## 附录 A · 术语对照

| Toodle 术语 | c-two 对应 | 业界主流对应 |
|------------|-----------|------------|
| Workspace | （无，c-two 不关心） | Kubernetes Namespace / Project |
| Resource | `cc.register()` 注册的资源 | 服务注册中心的 Service Instance |
| Resource Graph | （无） | Service Topology / Dependency Graph |
| Tree View | （无） | File Explorer / Tag-based Catalog |
| Schema Registry | （无）；与 CRM contract 版本协商结合 | Confluent Schema Registry / Buf Schema Registry |
| Capability Grant | （无） | OAuth Scope + Resource-level ACL |
| AuthProvider | `auth_hook` 的上游 | OIDC Provider / SAML IdP |
| Identity (Toodle) | `call metadata` 透传内容 | JWT claims / Principal |
| fastdb Feature | `@cc.transferable` 的"生态版" | Protobuf Message（但支持列式/零拷贝） |
| Schema JSON | （无） | `.proto` 文件 + FileDescriptorSet |

## 附录 B · 与现有文档的关系

| 文档 | 关系 |
|------|------|
| [`endgame-architecture.md`](./endgame-architecture.md) | 本文件是其 L2 Toodle 部分的详细 M4 落地方案 |
| [`sota-patterns.md`](./sota-patterns.md) | 本文件继承其 RPC 模式，叠加策略层 |
| `docs/superpowers/specs/YYYY-MM-DD-toodle-m4.md` (未来) | 本文件是 vision，具体实现 spec 在 superpowers 目录另写 |
| fastdb 仓库的 Schema JSON RFC (待建) | 本文件 §9 对应的前置工作，放在 fastdb 仓库内 |
