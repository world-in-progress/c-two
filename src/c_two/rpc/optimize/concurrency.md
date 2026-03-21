# 并发支持与隔离运行时设计审阅稿

## 0. 文档定位

这份文档不是对原始设想的简单否定，而是基于当前仓库实现、现有 benchmark、你补充的运行时与安全约束，重新整理出来的一份 **可执行设计稿**。核心目标有四个：

1. 让 CRM 真正支持并发访问，而不是只有传输层能“并发接收、串行执行”。
2. 保留你强调的 **CRM 运行环境隔离**，把 `Python 3.14.3t` 作为 CPU 密集型 CRM 的目标运行时。
3. 保留你强调的 **单端口 HTTP 入口 + 内部转发** 思路，避免每个 CRM 占一个公网或宿主端口。
4. 把并发调度、路由/端口管理、同机 IPC、大数据传输这几个问题拆开，但在架构上正确衔接。

下面的结论不是从抽象设想出发，而是从当前代码直接推导出来的。

---

## 1. 从代码出发的当前现状

### 1.1 真正的串行瓶颈在 `src/c_two/rpc/server.py`

当前所有协议最终都汇聚到统一的 `Server` 抽象。真正执行 CRM 调用的是：

- `_serve(state)`：单线程循环从 `EventQueue` 取事件
- `_process_event_and_continue(state, event)`：同步处理 `CRM_CALL`
- `response = method(args_bytes)`：阻塞直到 CRM 方法返回

也就是说，**当前串行瓶颈不在 HTTP、memory、thread 或 ZMQ 的接收端，而在统一执行平面**。

这带来一个重要结论：

- `http://` 现在虽然能同时接受多个请求，但这些请求只是并发进入 `EventQueue`
- 真正的 CRM 执行仍然是一条单线程消费链
- 因此任何并发设计如果不改 `server.py` 的执行模型，最终都只是“入口并发、执行串行”

### 1.2 当前 `routing.py` 还不是一个真正的 Router

当前仓库中：

- `src/c_two/rpc/routing.py` 只有一个 async helper
- `http://` 和 `zmq://` 分支仍是 TODO
- 实际实现的只有 `memory_routing()`

也就是说，当前的 `routing.py` 更接近“异步 relay helper”，**还不是一个系统级的单端口路由服务**。  
如果要走你设想中的“HTTP 单端口入口 -> 内部转发 -> 隔离 CRM Worker”，则需要一个真正的 Router 角色，而不是只靠一个函数。

### 1.3 当前 `memory://` 更适合兼容路径，不适合做未来的高性能热路径

当前 `memory://` 的实现是：

- Client 写请求文件
- Server 用 watchdog + 目录扫描发现请求
- Server 读文件并执行业务
- Server 写响应文件
- Client 用轮询 + 指数退避等待响应

这条路径在语义上可用，但 benchmark 已经明确暴露它的成本：

- 64B P50 约 `30.97ms`
- 1KB P50 约 `28.03ms`
- 小消息固定开销非常高
- 大消息吞吐又受到文件 I/O 天花板限制

因此：

- 如果将来要引入 Router -> Worker 的内部高频转发
- 不建议把当前 `memory:// v1` 作为主热路径
- 它更适合作为兼容层、回退层，或者在早期阶段先跑通流程

### 1.4 当前 `http://` 的回复路径在 free-threading 语境下需要重新审视

`HttpServer` 现在用的是：

- Starlette/uvicorn 接收请求
- 为每个请求创建 `Future`
- 把事件放入 `EventQueue`
- 后台执行完后通过 `call_soon_threadsafe()` 交还结果

这套思路总体是对的，但当前实现里 `response_futures` 是共享 dict。  
在 GIL 版 CPython 里它通常“能工作”，但在 `3.14t` 的自由线程运行时里，**任何共享可变结构都不能继续靠侥幸**。

换句话说，free-threading 不只是“CRM 逻辑能并行”，而是要求我们对框架本身的共享状态做一次审计。

### 1.5 当前 `ZmqServer` 的 `REP/REQ` 模型不适合多线程并发回复

当前 ZMQ 实现：

- Client: `REQ`
- Server: `REP`

这是一个严格的 `recv -> send -> recv -> send` 配对模型。  
它对简单串行 RPC 非常方便，但一旦进入“请求先入队，回复由不同 worker 在线程池异步返回”，问题就出现了：

1. `REP` socket 本身不适合多线程任意时刻回包
2. ZMQ socket 默认不是线程安全对象
3. 只要执行平面改成并发，ZMQ 层就不能继续保持现在的 `REP` 形态

所以并发改造不是只改 `server.py` 就结束了，**ZMQ 传输层必须同步升级**。

### 1.6 仓库已有“镜像构建”能力，但还没有“运行时角色拆分”

`c3 build` 现在已经能为 CRM 项目生成镜像和 Dockerfile。  
这说明仓库已经有了“部署打包”的基础，但当前没有清晰区分：

- Router / Gateway 进程
- CRM Worker 进程
- 标准 Python 控制面
- `3.14.3t` 计算面

而你现在提出的真实需求，恰恰要求这几类角色分开。

---

## 2. 对你现有思路的审视：哪些应保留，哪些应调整

### 2.1 应该保留的部分

#### A. 读写显式声明

这个方向是对的，而且很重要。  
Resource RPC 和传统 stateless RPC 的区别，就在于 CRM 有长期状态。  
如果框架层完全不知道某个方法是读还是写，就无法做出可靠调度。

#### B. 阶段分离 / CoW 语义

你提出的“创建阶段可写、共享阶段只读、需要修改时通过 CoW 派生新资源”的思路，非常适合科学计算和资源血缘管理。  
但它更偏 **资源生命周期治理语义**，不应该和基础并发调度耦在一起。

我的建议是：

- 在本次并发方案里把它作为“上层资源语义模型”
- 而不是把它强塞进底层 RPC 必选机制

#### C. HTTP 单端口入口 + 内部转发

这个思路我现在认为 **不是多余复杂化，而是系统边界上的必要抽象**。  
你补充的两个理由非常关键：

1. 每个 CRM 暴露独立 HTTP 端口，运维复杂度会快速失控
2. 多端口暴露本身会增加安全面

所以从系统工程角度看：

- 统一 HTTP 入口
- 内部转发到真实 CRM Worker
- Worker 不直接暴露公网或宿主端口

这条路线是合理的。

#### D. 让 CPU 密集型 CRM 跑在 `Python 3.14.3t` 隔离环境中

你这里的关键不是“全仓库强依赖 free-threading”，而是：

- **把真正需要 CPU 并行的 CRM Worker 放到 `3.14.3t` 的隔离运行环境**
- 避免外部生态不成熟的问题污染整个控制面和路由面

这点我现在完全认同，而且这是比“全项目直接切到 3.14t”更成熟的工程路线。

### 2.2 需要调整的部分

#### A. “无锁 ring buffer” 不应作为当前主设计

原因不是它在理念上错误，而是它在当前 Python 框架里并不划算：

1. 当前瓶颈首先不是 `queue.Queue`
2. 真正的大头在执行模型、路由设计、IPC 路径和线程安全审计
3. 即使要做 lock-free，Python 层也不是最合适的落点

因此在这轮设计里：

- 执行平面先用 `queue.Queue + ThreadPoolExecutor + 调度器`
- 将来如果 Router 使用 Rust/Go/C++ 重写，再考虑更激进的无锁结构

#### B. 当前 `memory_routing.py` 不应直接承担未来 Router -> Worker 主热路径

原因很简单：它的实现在 benchmark 里已经暴露出高固定开销。  
如果 Router 每个请求都走这个路径，单端口虽然解决了，但性能会被内部 IPC 再次卡住。

所以应该把你的“转发设计”升级为：

- **保留转发架构**
- **替换内部热路径实现**

#### C. 不应要求控制面、路由面、Worker 面全部都运行在 `3.14t`

更合理的拆法是：

- Router / Gateway：标准运行时即可，甚至未来可改成非 Python
- CRM Worker：根据需要选择 `3.14t`
- 这样既能拿到 CPU 并行收益，也把生态风险控制在最小边界内

---

## 3. 我建议采用的总体架构

### 3.1 总体分层

建议把未来系统拆成四个平面：

```text
┌─────────────────────────────────────────────────────┐
│                  Public HTTP Ingress                │
│          单端口入口，负责认证、限流、路由           │
└────────────────────────┬────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│                    Router / Gateway                 │
│     不执行 CRM 逻辑，只负责转发、注册、观测、回包    │
└────────────────────────┬────────────────────────────┘
                         │ Local IPC / Internal RPC
                         ▼
┌─────────────────────────────────────────────────────┐
│               Isolated CRM Worker Runtime           │
│        每个 Worker 可运行在 Python 3.14.3t          │
└────────────────────────┬────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│              Concurrency Scheduler + CRM            │
│       读写调度、线程池、背压、状态一致性控制         │
└─────────────────────────────────────────────────────┘
```

### 这四层各自负责什么

#### 1. Public HTTP Ingress

- 只对外暴露一个或少量稳定入口
- 终止 TLS
- 做鉴权、审计、限流
- 把外部 HTTP 世界和内部 CRM 世界隔离开

#### 2. Router / Gateway

- 把外部请求映射到目标 CRM Worker
- 维护 Worker 注册表、心跳和路由表
- 处理超时、重试、拒绝、回包
- 不执行具体 CRM 方法

#### 3. Isolated CRM Worker Runtime

- 承载实际 CRM 实例
- 可以使用与控制面不同的 Python/runtime
- 对外不暴露公网端口，只暴露内部可达地址
- 推荐粒度为 **一个 CRM 实例对应一个 Worker**

#### 4. Concurrency Scheduler

- Worker 内部的真正并发执行层
- 决定一个请求是串行执行、读并行执行，还是完全并行
- 是这次“并发支持”的真正核心

---

## 4. 关键设计判断

### 4.1 “并发支持”与“单端口路由”不是一回事，但必须协同设计

这两个问题必须拆开理解：

#### 问题 A：CRM 本身如何并发执行？

这解决的是：

- 同一个 CRM 同时来了多个调用，能否并行？
- 读和写如何避免数据竞争？

它的答案在 **Scheduler**。

#### 问题 B：外部如何安全地访问很多 CRM？

这解决的是：

- 是否需要为每个 CRM 暴露独立 HTTP 端口？
- 入口怎么统一认证和转发？

它的答案在 **Router / Gateway**。

因此我的结论是：

- 你对“routing”的坚持是对的
- 但 routing 不能替代并发调度
- 两者应分别设计，再通过内部协议连接起来

### 4.2 `3.14.3t` 的最佳落点是 Worker，不是整个控制平面

这是我根据你补充信息后最重要的调整之一。

推荐策略：

- Router / Gateway：先保持标准运行时
- CRM Worker：允许并鼓励跑在 `3.14.3t`
- 这样 CPU 密集型 CRM 能拿到真实线程并行
- 同时把第三方生态兼容性风险局限在 Worker 边界内

这也是“隔离运行时”的真正价值。

### 4.3 当前仓库需要先做一轮 free-threading 安全审计

如果 Worker 运行在 `3.14t`，框架层必须确保共享状态是安全的。  
我在当前代码里已经能看到几个需要优先审计的点：

- `rpc/http/http_server.py` 中的 `response_futures`
- `rpc/server.py` 中的 `shutdown_events`、`stage`、`on_shutdown`
- `rpc/transferable.py` 中的全局注册表 `_TRANSFERABLE_MAP` / `_TRANSFERABLE_INFOS`
- `rpc/thread/__init__.py` 中的线程服务器注册表
- `mcp/mcp_util.py` 中的 `_MCP_FUNCTION_REGISTRY`

在有 GIL 时，这些地方有些问题不容易显形；  
在 `3.14t` 下，这些共享结构必须要么：

- 用锁保护
- 要么在启动期构建后冻结不再写入
- 要么保证只能在单线程上下文里修改

---

## 5. 我建议的并发执行模型

### 5.1 先定义三种并发模式

不建议一上来只提供“开/关并发”两个状态。  
更合理的是三档：

#### 模式 1：`exclusive`

语义：

- 所有请求串行执行
- 完全兼容当前行为

用途：

- 默认安全模式
- 未经审计的 CRM 先跑这档

#### 模式 2：`read_parallel`

语义：

- 读请求可并行
- 写请求独占
- 当有写请求等待时，新的读请求暂停进入，防止写饥饿

用途：

- 适合大多数“读多写少”的资源型 CRM

#### 模式 3：`parallel`

语义：

- 框架层不再提供读写互斥
- 所有请求可进入线程池并发执行
- 由 CRM 自身保证线程安全

用途：

- 适合内部已经自行加锁
- 或者本身是 immutable / actor-like / lock-free 结构的 CRM

### 5.2 方法级语义建议仍然保留 `@cc.read` / `@cc.write`

推荐在 ICRM 层提供显式标注：

```python
@cc.icrm(namespace='cc.demo', version='1.0.0')
class IModel:
    @cc.read
    def get_state(self, step: int) -> State:
        ...

    @cc.write
    def advance(self, dt: float) -> StepResult:
        ...
```

规则建议：

- 未标注的方法默认视为 `write`
- 这是保守且安全的默认值
- 框架只在 ICRM 层读取元数据，不要求 CRM 实现重复标注

### 5.3 调度器建议

Worker 内部建议采用：

- `EventQueue` 继续作为入口缓冲
- `ThreadPoolExecutor` 作为执行器
- `ReadWriteLock` 作为 `read_parallel` 模式下的核心互斥原语
- 独立的 admission queue / backpressure 机制控制积压

典型流程：

1. Worker 收到 `CRM_CALL`
2. Scheduler 解析 method_name
3. 读取该方法的访问语义（read / write）
4. 按并发模式决定：
   - 直接串行执行
   - 走读锁并发执行
   - 走写锁独占执行
   - 或直接全并行
5. 执行完成后回包

### 5.4 为什么不建议“读直接无锁、写再补锁”

如果简单做成：

- 读完全无锁
- 写时再补锁

那就意味着：

- 写执行时未必能阻止新的读进入
- 资源快照一致性没有边界
- 很容易在长期运行中出现隐蔽竞态

所以更稳妥的设计仍是 **writer-preferred RWLock**。

---

## 6. 我建议的单端口路由模型

### 6.1 为什么必须有 Router

你提的“不要每个 CRM 都暴露单独 HTTP 端口”这件事，在工程上非常成立。  
Router 带来的收益包括：

1. **安全边界统一**
   - 鉴权、TLS、审计、限流集中处理
2. **运维复杂度下降**
   - 不再维护成百上千个外露端口
3. **路由可控**
   - 可按 CRM 类型、实例 ID、租户、命名空间分发
4. **运行时隔离自然成立**
   - Worker 只接内网或本机流量，不直接暴露

### 6.2 推荐请求路径

推荐的最终路径应当是：

```text
HTTP Client
    -> Public HTTP Ingress / Router
    -> Internal Control/Data Plane
    -> Isolated CRM Worker
    -> Scheduler
    -> CRM
    -> Worker Reply
    -> Router HTTP Response
```

也就是说，**HTTP URL 不再直指真实 CRM Server**，而是指向 Router。

部署优先级上，Router 与 Worker 应优先：

- 在 Kubernetes/容器环境中 **同 Pod**
- 在单机场景中 **同主机**

这样内部转发才能默认建立在高性能本地 IPC 之上，而不是先被跨节点网络语义绑架。

### 6.3 当前仓库里的 `routing.py` 该如何演进

建议把当前 `routing.py` 从“一个 helper 函数”升级为两层：

#### 层 1：协议无关的 relay API

保留类似：

```python
async def routing(server_address: str, event_bytes: bytes, timeout: float) -> bytes:
    ...
```

作为客户端或框架侧统一入口。

#### 层 2：真正的 Router 服务

新增一个明确的模块，例如：

```text
src/c_two/rpc/router/
```

里面承载：

- Worker 注册表
- 路由表
- 内部转发器
- HTTP ingress handler
- 健康检查与观测

这样，`routing.py` 负责 API，`router/` 负责进程级服务实现。

---

## 7. 我建议的内部通信方案

### 7.1 不建议 Router -> Worker 默认继续沿用当前 `memory:// v1`

原因已经很明确：

- 文件系统
- watchdog
- 目录扫描
- 轮询 + 指数退避

这些在 benchmark 上已经证明不适合作为高频内部调用热路径。

### 7.2 推荐“控制面 + 数据面”双通道

我建议你的“内部转发”最终做成双通道，而不是单一文件 IPC：

#### 控制面（Control Plane）

职责：

- 请求头
- 方法名
- 元数据
- 小消息 inline 传输
- 回包通知
- 心跳与注册

推荐介质：

- Linux/macOS：Unix Domain Socket
- Windows：Named Pipe 或 Loopback TCP

这里必须从第一阶段就把 Windows 放入目标矩阵，而不是把它留到后补。原因不是平台完整性上的“形式正确”，而是本仓库天然会面对 native 前后端一体应用，Windows 是必须正面支持的主平台。

#### 数据面（Data Plane）

职责：

- 大 payload 传输

推荐介质：

- `multiprocessing.shared_memory`
- 必要时保留 mmap 文件回退

### 7.3 为什么这个组合比当前 `memory:// v1` 更合理

根据现有 benchmark：

- 小消息主要输在固定开销
- 大消息主要输在文件 I/O 带宽天花板

所以：

- 小消息：应优先走 socket/UDS inline
- 大消息：应优先走 SHM

一个合理的经验分流是：

- `<= 1MB`：直接 inline
- `1MB ~ 8MB`：保留自适应空间
- `>= 8MB`：优先 SHM

这里阈值最终仍应由 benchmark 再校准，但方向是清晰的。

### 7.4 `memory:// v1` 的正确定位

我建议把它降级为：

- 兼容路径
- 回退路径
- 过渡实现

而不是未来 Router -> Worker 的默认高速内核。

---

## 8. 传输层需要同步做的改造

### 8.1 `ThreadServer`

这是当前最容易先跑通并发语义的协议：

- 同进程
- 结构简单
- 回复路径已有锁和 condition

建议用它做第一阶段并发语义验证。

### 8.2 `HttpServer`

`HttpServer` 的总体方向是正确的，但需要：

1. 审计 `response_futures` 的线程安全
2. 在 free-threaded 运行时下确保所有共享结构受保护
3. 明确 Router 模式下它是“入口服务”还是“真实 CRM Worker 的协议适配器”

我更倾向于：

- 对外 HTTP：由 Router 使用
- Worker 内部默认不再直接暴露 HTTP

### 8.3 `ZmqServer`

这是当前设计里最明确的一个结构性阻塞点。  
如果要支持并发执行，推荐改为：

- `ROUTER/DEALER`
- 或 `ROUTER + worker identity routing`
- 或最少也要做“接收线程 + 回复线程”分离

但我不建议继续用现在的 `REP`。

### 8.4 `MemoryServer`

它在“并发正确性”上不是最大问题，在“性能与延迟”上才是问题。  
因此它的优先级应排在：

- 先做执行平面并发
- 再做路由
- 再做内部 IPC v2

---

## 9. 资源生命周期语义该放在哪里

你提出的阶段分离 / CoW 思路，我建议这样落位：

### 框架底层负责

- 请求调度
- 并发控制
- 错误传播
- 传输与回包

### 上层资源模型负责

- 资源处于创建阶段还是共享阶段
- 当前是否允许写
- 是否需要通过 CoW 派生新资源

也就是说：

- 读写声明可以进入框架
- 资源生命周期治理不必作为第一阶段强制机制

否则底层 RPC 层会背上太多领域语义。

---

## 10. 推荐的工程实施顺序

这里我给出一个我认为更稳、更可执行的路线。

### Phase 0：设计收敛与审计

目标：

- 明确 Router / Worker / Scheduler 的角色边界
- 做 free-threading 安全审计
- 不急着编码大规模新模块

输出：

- 本文档
- 会话 `plan.md`
- 需要被审计的共享状态列表

### Phase 1：先把执行平面并发做对

目标：

- 不管外部是不是 Router，先让一个 Worker 自己具备并发执行能力

建议改动：

- `server.py` 引入 `ConcurrencyConfig`
- 引入 `Scheduler`
- 引入 `@cc.read` / `@cc.write`
- 提供三种模式：`exclusive` / `read_parallel` / `parallel`
- 优先用 `thread://` 和 `http://` 测试并发语义

为什么先做这个：

- 因为这是整个系统最核心的“执行能力”
- 如果 Worker 自己还不能并发，Router 再漂亮也只是并发排队

### Phase 2：做 free-threading hardening

目标：

- 让 Worker 真能安全运行在 `3.14.3t`

建议改动：

- 审计共享 dict / list / registry
- 能冻结的注册表尽量在启动后冻结
- 必须共享写入的地方补锁
- 为 Worker 提供专门运行时配置

说明：

- 这一阶段不是要求全仓库都切到 `3.14t`
- 而是确保 Worker 路径是安全的

### Phase 3：改造 ZMQ 与协议适配层

目标：

- 让非 HTTP 协议也能跟上新的并发执行模型

建议改动：

- `ZmqServer` 从 `REP` 升级到更适合并发的模式
- 清理 reply 路径中的线程安全隐患

### Phase 4：引入真正的 Router 服务

目标：

- 实现“单端口 HTTP 入口 -> 内部 Worker 转发”

建议改动：

- 新增 `rpc/router/`
- 实现 Worker 注册与路由表
- Router 对外提供统一 HTTP 接口
- Worker 默认只提供内部地址，不再直接暴露公网或宿主端口

### Phase 5：升级内部 IPC 到 v2

目标：

- 让 Router -> Worker 热路径足够快

建议改动：

- 控制面：UDS / loopback
- 数据面：SHM
- `memory:// v1` 保持回退

### Phase 6：基准、观测、文档收口

目标：

- 用 benchmark 证明设计成立
- 为后续实现提供可回归的验收基线

建议输出：

- 并发 benchmark
- Router/Worker 端到端 benchmark
- 观测指标：queue depth、active workers、wait time、reply latency

---

## 11. 代码级落点建议

建议后续实现时优先落在这些位置：

### 核心执行平面

- `src/c_two/rpc/server.py`
- `src/c_two/rpc/concurrency/`

### ICRM 并发元数据

- `src/c_two/crm/`
- `src/c_two/__init__.py`

### HTTP / ZMQ 协议适配

- `src/c_two/rpc/http/http_server.py`
- `src/c_two/rpc/zmq/zmq_server.py`
- `src/c_two/rpc/zmq/zmq_client.py`

### Router 与内部转发

- `src/c_two/rpc/routing.py`
- `src/c_two/rpc/router/`

### 内部 IPC v2

- `src/c_two/rpc/memory/` 下新增 v2 目录
- 或新建 `src/c_two/rpc/local/`

### 运行时与构建

- `src/c_two/seed/docker_builder.py`
- CLI 增加更明确的 Router / Worker 镜像或 profile 概念

---

## 12. 已确认的边界条件

根据你的进一步反馈，当前设计边界已经可以收敛为以下约束：

1. **Worker 粒度：一个 CRM 实例一个 Worker**
   - 这样 Router 可以按命名直接导向目标 Worker
   - Worker 内部无需再做同类 CRM 二次分流
   - 对 GIS、水动力模拟等高资源占用 CRM，更利于内存、磁盘、CPU 的隔离和运维观测

2. **部署关系：优先同 Pod，其次同主机**
   - Router 与 Worker 优先采用本地邻接部署
   - 因此内部转发可以默认建立在 IPC 或其它高性能本地通道上
   - 跨节点行为留给后续二次转发或节点发现机制解决

3. **Windows 是第一阶段目标**
   - 这意味着内部控制面设计不能只围绕 UDS 展开
   - 需要从第一阶段就把 Named Pipe / Loopback TCP 放进正式方案与测试矩阵

4. **请求取消语义应保留扩展位，但不进入当前最高优先级**
   - 原因不是它不重要，而是它会牵涉到下游 CRM 的 cancel / rollback 规范
   - 当前阶段先聚焦并发支持本身

5. **阶段分离语义不进入框架底层**
   - 这是基于 C-Two 的下游应用应主动承担的资源治理责任
   - 框架底层只提供并发、路由、隔离和传输能力

---

## 13. 最终结论

如果只问“这个仓库该怎么把并发支持做对”，我的结论是：

1. **先改执行平面，不要误把入口并发当成 CRM 并发。**
2. **保留 Router 思路，而且要把它提升为真正的系统角色。**
3. **把 `3.14.3t` 放在隔离的 CRM Worker 运行时中，而不是强推整个控制面切换。**
4. **不要把当前 `memory:// v1` 直接当成未来内部热路径；Router -> Worker 需要新的本地高性能通道。**
5. **第一阶段先做 Scheduler + 读写语义 + free-threading 审计；第二阶段再做 Router 与 IPC v2。**

如果只问“你的原始思路是否值得继续”，我的判断是：

- **方向值得继续**
- 但需要从“单点技巧”升级为“角色分层清晰的系统设计”

这是我目前认为最严格、最专业、也最可落地的一条路线。
