# Python 3.14.3t Free-Threading 审计报告

## 1. 审计范围

本次审计聚焦于 **CRM Worker 未来运行在 `Python 3.14.3t` 自由线程环境** 时，框架层会暴露出的共享状态和跨线程访问问题。

重点检查文件：

- `src/c_two/rpc/server.py`
- `src/c_two/rpc/http/http_server.py`
- `src/c_two/rpc/thread/__init__.py`
- `src/c_two/rpc/thread/thread_server.py`
- `src/c_two/rpc/transferable.py`
- `src/c_two/mcp/mcp_util.py`
- `src/c_two/rpc/event/event_queue.py`
- `src/c_two/compo/runtime_connect.py`
- `src/c_two/rpc/zmq/zmq_server.py`

本报告只讨论 **free-threading 相关的线程安全与执行模型风险**，不讨论风格问题，也不把下游 CRM 的业务规范（如 cancel / rollback 约束）混进来。

---

## 2. 环境探针结论

### 2.1 当前机器上已经存在自由线程解释器

本机存在：

- `/opt/homebrew/bin/python3.14t`

并且运行时探针显示：

- `sys._is_gil_enabled() == False`

这说明本地已经具备真实的 free-threading 解释器，可用于 Worker 路径验证。

### 2.2 关于 `uv`

这里原先记录有误，正确命令应为：

- `uv python pin 3.14.3t`

实际检查当前机器上的 `uv` 后确认：

- 当前安装的 `uv` **支持 `uv python pin`**
- `uv run --python /opt/homebrew/bin/python3.14t ...` 也确实会选择 freethreaded 解释器

### 2.3 当前环境下的额外限制

在本机上做完整 `uv run --python /opt/homebrew/bin/python3.14t` 项目导入验证时，出现了两类与审计本身无关、但会影响后续实测的环境问题：

1. `uv` 解析到 dev 依赖后，`pyarrow` 本地构建缺少若干 CMake 依赖，导致环境启动失败。
2. 直接用 `PYTHONPATH=src python3.14t` 导入项目时，`src/c_two/__init__.py` 依赖 `dotenv`，但当前 `pyproject.toml` 中并未声明该依赖。

因此：

- 本报告的主结论来自 **静态代码审计 + 局部运行时探针**
- 后续真正跑 Worker 级回归时，应先把 3.14t 环境的依赖安装路径收敛好

这里还有两个范围判断需要固定下来：

1. **`pyarrow` 不是第一阶段 free-threading 验证的阻塞项。**  
   如果 `pyarrow` 在 `3.14t` 下尚不稳定，第一阶段完全可以使用现有 `pickle` fallback 验证核心并发路径。

2. **未来可以考虑以 `fastdb` 取代当前默认序列化路径。**  
   这更适合作为后续增强项：一方面提高自由线程兼容性，另一方面为 Python 与未来 TS 前端侧的一致访问提供统一基础。

---

## 3. 审计总览

| 编号 | 严重级别 | 位置 | 结论 |
|---|---|---|---|
| FTA-01 | Critical | `rpc/http/http_server.py` | `response_futures` 的跨线程访问与 `Future` 生命周期管理不满足 free-threading 要求 |
| FTA-02 | Critical | `rpc/thread/thread_server.py` | `responses` / `response_condition` 采用分裂锁协议，读写条件判断不一致 |
| FTA-03 | High | `rpc/transferable.py` | `_TRANSFERABLE_MAP` / `_TRANSFERABLE_INFOS` 全局注册表无保护，存在并发注册与遍历风险 |
| FTA-04 | Low (deferred) | `mcp/mcp_util.py` | `_MCP_FUNCTION_REGISTRY` 技术上有竞态空间，但 MCP 当前属于外围玩具功能，可延后处理 |
| FTA-05 | Medium | `rpc/server.py` | `ServerState` 的锁纪律主要靠调用方约定，当前勉强成立，后续引入 Scheduler 后会变脆弱 |
| FTA-06 | Low-Medium | `rpc/event/event_queue.py` | `shutdown()` 的 drain 逻辑存在 TOCTOU，更多是关闭语义问题，不是核心数据竞争 |
| FTA-07 | Architectural blocker | `rpc/zmq/zmq_server.py` | `REP/REQ` 模型不适合未来的多线程 Worker 异步回包，即使不考虑 free-threading 也必须调整 |
| FTA-08 | Positive controls | `rpc/thread/__init__.py` / `compo/runtime_connect.py` | 线程注册表和 thread-local 用法总体正确，可作为保留模式 |

---

## 4. 详细结论

## FTA-01 `HttpServer.response_futures` 不满足 free-threading 要求

**位置**

- `src/c_two/rpc/http/http_server.py:31`
- `src/c_two/rpc/http/http_server.py:92`
- `src/c_two/rpc/http/http_server.py:104`
- `src/c_two/rpc/http/http_server.py:136-145`
- `src/c_two/rpc/http/http_server.py:216-236`
- `src/c_two/rpc/http/http_server.py:275-278`

**当前结构**

```python
self.response_futures: dict[str, asyncio.Future] = {}
```

它被以下不同线程或上下文共同访问：

1. HTTP 请求处理协程：写入 `response_futures[request_id] = future`
2. HTTP 请求收尾逻辑：`pop(request_id, None)`
3. Worker 回包线程：`reply()` 中 `get(event.request_id)`
4. 关闭路径：`_cleanup_futures()` 中遍历、`future.cancel()`、`clear()`
5. `cancel_all_calls()`：可能从事件循环线程之外直接调用 `_cleanup_futures()`

**为什么这是 free-threading blocker**

这不是单纯的“字典需要加锁”问题，而是两个问题叠在一起：

### 问题 A：共享 dict 的跨线程读写未同步

`response_futures` 在多个线程之间读、写、清空，当前没有任何锁，也没有“单线程所有权”。在 `3.14t` 下，这会导致：

- 取到已经被清理的 future
- `get()` 与 `clear()` / `pop()` 交错
- 关闭阶段的 dangling future 或重复完成

### 问题 B：`asyncio.Future` 的状态变更不应该从任意线程直接操作

当前 `_cleanup_futures()` 里直接：

```python
future.cancel()
```

但 `Future` 应由其所属 event loop 线程驱动。`reply()` 中已经正确使用了：

```python
self.event_loop.call_soon_threadsafe(future.set_result, response_data)
```

可 `cancel_all_calls()` -> `_cleanup_futures()` 这条路径却没有遵循同样规则。

**结论**

这是 **必须先修** 的问题；否则只要 Worker 开始真实并发回包，在 `3.14t` 下会同时踩到：

- dict 共享访问竞态
- future 生命周期跨线程错误驱动

**建议修法**

推荐优先采用“**event loop 单线程所有权**”而不是“粗暴大锁 + 任何线程都能碰 future”模式：

1. `response_futures` 的查找、插入、删除、取消都尽量收敛到 event loop 线程执行。
2. `reply()` 只做 `call_soon_threadsafe()`，把“查 dict + set_result”一起放到 loop 线程回调中。
3. `cancel_all_calls()` 不直接 `future.cancel()`，而是 `call_soon_threadsafe(self._cleanup_futures_on_loop)`。
4. 如果确实还需要跨线程读 dict，则只让跨线程代码接触“元数据快照”，不要直接碰 `Future` 对象本体。

---

## FTA-02 `ThreadServer` 的响应协调协议在 free-threading 下不安全

**位置**

- `src/c_two/rpc/thread/thread_server.py:19-24`
- `src/c_two/rpc/thread/thread_server.py:35-44`
- `src/c_two/rpc/thread/thread_server.py:67-70`
- `src/c_two/rpc/thread/thread_server.py:82-100`

**当前结构**

```python
self.responses: dict[str, Event] = {}
self.response_lock = threading.RLock()
self.response_condition: dict[str, threading.Condition] = {}
self.conditions_lock = threading.Lock()
```

这套结构的问题不是“有没有锁”，而是：**状态谓词与通知机制使用了不同的锁域**。

### 具体风险 1：`get_response()` 在未持有 `response_lock` 的情况下读取 `self.responses`

```python
while request_id not in self.responses:
    if not condition.wait(...):
        break
```

这里对 `self.responses` 的检查没有放在 `response_lock` 里，而 `reply()` 对 `self.responses` 的写入却在 `response_lock` 中完成。  
在 GIL 版 Python 下通常“能跑”，但在 `3.14t` 下，这种无保护 predicate read 是典型竞态点。

### 具体风险 2：`response_condition` 的创建、获取、遍历和删除虽然部分受 `conditions_lock` 保护，但通知依赖的是每个 `Condition` 自己的内部锁

也就是说当前代码里同时存在三套同步域：

- `response_lock`
- `conditions_lock`
- 每个 `Condition` 自身的锁

这会导致协议非常脆弱：

- 状态写入和通知不是在同一个同步域里完成
- waiter 检查条件时也不是在同一个同步域里完成
- `cancel_all_calls()` 中遍历 `response_condition.values()` 时，字典生命周期与每个 condition 本体是分离的

### 具体风险 3：`cancel_all_calls()` 的 dict 遍历与 `get_response()` 的 `pop()` 未来会冲突

当前：

```python
for condition in self.response_condition.values():
    with condition:
        condition.notify_all()
```

而 `get_response()` 结束时会：

```python
self.response_condition.pop(request_id, None)
```

虽然当前代码在 `cancel_all_calls()` 的这段遍历外层持有 `conditions_lock`，在现有实现中通常能挡住并发 `pop()`，但这套结构依然过于脆弱，而且 `response_condition` 字典和 `responses` 字典并未统一在一个可证明安全的协议下。

**结论**

`ThreadServer` 当前的同步策略在 free-threading 下应视为 **必须重构**，而不是做几处小补丁。

**建议修法**

推荐改成“**每个 request_id 一个 pending entry**”的统一模型，例如：

```python
pending[request_id] = {
    'event': threading.Event(),
    'response': None,
}
```

并满足：

1. `pending` 字典生命周期由单一锁保护。
2. `response` 的写入与 `Event.set()` 由统一协议保证顺序。
3. `get_response()` 不再在无锁条件下读 `responses`。
4. 避免“一个 dict 存状态，另一个 dict 存条件变量”的双结构设计。

---

## FTA-03 `transferable.py` 的全局注册表必须收敛为“加锁”或“启动后冻结”

**位置**

- `src/c_two/rpc/transferable.py:19-20`
- `src/c_two/rpc/transferable.py:51-63`
- `src/c_two/rpc/transferable.py:238-244`
- `src/c_two/rpc/transferable.py:385-409`
- `src/c_two/rpc/transferable.py:440-446`

**当前结构**

```python
_TRANSFERABLE_MAP: dict[str, Transferable] = {}
_TRANSFERABLE_INFOS: list[dict[str, dict[str, type] | str]] = []
```

这些全局结构会在以下路径被访问：

1. `TransferableMeta.__new__()` 在类定义时写入注册表
2. `register_transferable()` 写 `_TRANSFERABLE_MAP`
3. `_TRANSFERABLE_INFOS.append(...)` 写列表
4. `auto_transfer()` 迭代 `_TRANSFERABLE_INFOS`
5. `auto_transfer()` / `get_transferable()` 读取 `_TRANSFERABLE_MAP`

**为什么这是高风险点**

当前语义非常依赖“Transferable 的注册只发生在单线程启动阶段”这一隐含假设。  
但只要未来出现以下任一情况，这个假设就会被打破：

- 并发导入模块
- 热加载 CRM / plugin
- Router/Worker 按需加载模块
- 运行期动态注册新的 Transferable

在 `3.14t` 下，这会暴露为：

- 列表遍历时被 append
- 读 map 时正好有新 key 写入
- 匹配逻辑拿到半完成状态的注册信息

**结论**

这是 **Worker 启用 3.14t 前必须有明确策略** 的模块。  
不过它和 `HttpServer` 不同：这里的最佳解不一定是“所有访问都加锁”，而是更建议引入 **启动期收敛**。

**建议修法**

推荐两档方案：

### 方案 A：单线程注册 + 启动后冻结（优先推荐）

1. 所有 Transferable 注册只允许发生在 bootstrap 阶段。
2. Worker 启动完成后，把 `_TRANSFERABLE_INFOS` 冻结成 tuple / snapshot。
3. 运行期不再允许新增注册；若必须新增，则走显式 reload 流程。

### 方案 B：显式加锁 + snapshot 遍历

1. 引入 `_TRANSFERABLE_LOCK`。
2. 注册时在锁内写 map / list。
3. 遍历匹配时先复制 snapshot，再在锁外遍历。

如果未来确实支持插件热加载，则 B 比 A 更现实；如果 Worker 的模块集在启动时就固定，A 更干净。

---

## FTA-04 `MCP` 全局函数注册表属于“条件安全”，但当前阶段可以延后

**位置**

- `src/c_two/mcp/mcp_util.py:8`
- `src/c_two/mcp/mcp_util.py:12-15`
- `src/c_two/mcp/mcp_util.py:88`
- `src/c_two/mcp/mcp_util.py:126`

**当前结构**

```python
_MCP_FUNCTION_REGISTRY: dict = {}
```

**风险点**

- `adapt_compo_func_for_mcp()` 往全局 dict 写入函数
- `flow()` 从该 dict `copy()` 出 namespace
- 如果 MCP 工具注册和 `flow()` 执行并发发生，则存在 dict 读写竞态

**结论**

从纯技术角度，它确实有竞态空间；但从当前产品边界看，MCP 还是外围实验性功能，并不在这轮核心并发改造范围内。  
因此这里的结论应当是：

- **记录风险**
- **不纳入第一阶段主阻塞项**

但如果后续引入：

- 动态装载 component module
- 热更新 MCP tool
- 多线程并发初始化多个 module

它就会变成真问题。

**建议修法**

简单直接即可：

1. 增加 `_MCP_REGISTRY_LOCK`
2. 注册和 `copy()` 都在锁内进行
3. 如果 MCP tool 在启动后固定不变，也可以在启动完成后冻结注册表，只保留只读访问

---

## FTA-05 `ServerState` 当前主要靠“外部调用方自觉持锁”，这在引入 Scheduler 后会变脆弱

**位置**

- `src/c_two/rpc/server.py:66-89`
- `src/c_two/rpc/server.py:93-103`
- `src/c_two/rpc/server.py:123-140`
- `src/c_two/rpc/server.py:178-193`

**当前状态**

`ServerState` 本身有：

```python
self.lock = threading.RLock()
self.stage = ServerStage.STOPPED
self.shutdown_events = [self.termination_event]
```

目前很多关键字段的读写确实大多发生在 `state.lock` 保护下。  
但它的问题是：**锁纪律主要是隐含在调用路径里的，而不是被 helper 自己封装保证的。**

例如：

- `_stop()` 在锁内 append `shutdown_events`
- `_begin_shutdown_once()` 在锁内改 `stage`
- `_process_event_and_continue()` 在锁内处理 `on_shutdown`
- `_stop_serving()` 自己不持锁，但当前是从锁内路径调用进去

**结论**

在当前“单线程执行平面”里，这还勉强成立；但一旦下一阶段引入 Scheduler / worker pool：

- shutdown
- reply cancellation
- timeout
- stop / destroy

这些路径的并发度会提升，靠“外层记得拿锁”会越来越危险。

**建议修法**

在进入真正并发实现前，建议把 `ServerState` 的关键 helper 改成明确的锁边界：

1. `_stop_serving()` 自己内部获取或复制 `shutdown_events` snapshot
2. `stage` 的修改只允许通过统一 helper
3. `on_shutdown` / `crm` 的释放路径集中管理
4. 在文档层明确哪些字段允许在哪个线程访问

也就是说，这里不一定是当前马上会炸的 race，但它是 **下一阶段并发改造的热点前置项**。

---

## FTA-06 `EventQueue` 的问题主要在关闭语义，不在底层容器本身

**位置**

- `src/c_two/rpc/event/event_queue.py:8`
- `src/c_two/rpc/event/event_queue.py:11-16`
- `src/c_two/rpc/event/event_queue.py:34-45`

**判断**

这里要区分两层：

### 底层容器层面：相对安全

- `queue.Queue()` 本身是线程安全的
- `threading.Event()` 也是标准同步原语

所以 `EventQueue` 不是这次审计里最危险的共享状态点。

### 关闭语义层面：仍然有 TOCTOU

当前 `shutdown()`：

```python
while not self._queue.empty():
    try:
        self._queue.get_nowait()
    except queue.Empty:
        break
```

这里 `empty()` 不是可靠同步原语。  
在 free-threading 下，这种“先看 empty 再 get_nowait”的写法只是更容易暴露语义问题。

**结论**

这是 **低到中等级别** 的问题，不会是 Worker free-threading 的第一阻塞项，但在并发改造时应该顺手修正。

**建议修法**

- drain 逻辑不要依赖 `empty()`
- 明确 shutdown 后是否允许并发 `put`
- 最好把“关闭后拒绝新事件”和“清空剩余事件”整理成可证明的状态机

---

## FTA-07 `ZmqServer` 不是典型 free-threading 数据竞争，但它是未来并发 Worker 的结构性阻塞点

**位置**

- `src/c_two/rpc/zmq/zmq_server.py:37`
- `src/c_two/rpc/zmq/zmq_server.py:66-67`

**当前实现**

- Server 使用 `zmq.REP`
- Client 使用 `zmq.REQ`

**为什么必须写进审计**

即使完全不讨论 `3.14t`，只要你要做“Worker 内部线程池并发执行，然后异步回包”，`REP/REQ` 模型就会成为硬阻塞：

1. `REP` socket 不是给任意 worker 线程随时 `send()` 用的
2. ZMQ socket 本身默认就不应跨线程乱用
3. 当前 `reply()` 直接 `self.socket.send(...)`，无法平滑接到未来的多 worker 回复路径上

**结论**

这不是“以后再看”的小问题，而是并发实现前必须做的协议层决策：

- 改成 `ROUTER/DEALER`
- 或接收线程 / 回复线程分离
- 或让 ZMQ 退居非主通路

否则 ZMQ 路径会在设计层面卡死未来的 Scheduler。

---

## FTA-08 已确认的正向模式

审计里也有一些结构是值得保留的：

### A. `threading.local()` 的当前用法是合理的

**位置**：`src/c_two/compo/runtime_connect.py:10`

```python
_local = threading.local()
```

`connect_crm()` / `get_current_client()` 依赖 thread-local 保存当前 client。  
这类状态天然按线程隔离，不是 free-threading 风险点。

### B. `_THREAD_SERVERS` 注册表本身是加锁的

**位置**：`src/c_two/rpc/thread/__init__.py:8-29`

```python
_THREAD_SERVERS: dict[str, 'ThreadServer'] = {}
_REGISTRY_LOCK = threading.RLock()
```

这个全局注册表的基本访问模式是对的：

- 注册加锁
- 删除加锁
- 查询加锁
- 列表复制加锁

所以这里的问题不在 registry 本身，而在 `ThreadServer` 内部的响应协调协议。

### C. `queue.Queue` 作为基础队列仍然是可以接受的

当前没有任何证据表明这个项目的首要瓶颈在 `queue.Queue` 本身。  
因此在第一阶段并发实现里，没有必要把“无锁 ring buffer”当成必须项。

---

## 5. 优先级排序

## 必须在 `3.14t Worker` 落地前解决

1. `HttpServer.response_futures` 的线程所有权与 Future 生命周期问题
2. `ThreadServer` 的响应协调协议重构
3. `transferable.py` 全局注册表的锁或冻结策略
4. `ZmqServer` 的并发回包模型决策

## 应在同一轮并发改造中解决

5. `MCP` 注册表保护或冻结
6. `ServerState` 的锁纪律显式化
7. `EventQueue` 的 shutdown 语义整理

如果严格按当前用户边界执行，则第 5 项可以再向后顺延，因为 MCP 当前并不进入核心 Worker 并发路径。

## 可以后置，但要预留接口

8. 请求取消语义
9. 更高层的阶段分离 / CoW 资源治理

---

## 6. 我建议的修复策略，不建议只靠“大锁糊住”

我更推荐以下三种模式组合，而不是简单地“哪里有问题就套一把锁”：

### 模式 1：单线程所有权

适用于：

- `HttpServer.response_futures`
- 所有 `asyncio.Future` 生命周期管理

原则：

- 归 event loop 线程独占
- 其它线程只通过 `call_soon_threadsafe()` 递交操作

### 模式 2：启动期构建，运行期冻结

适用于：

- `_TRANSFERABLE_MAP`
- `_TRANSFERABLE_INFOS`
- `_MCP_FUNCTION_REGISTRY`

原则：

- 能在 bootstrap 完成的，就不要带着“可变全局注册表”进入 Worker 热路径

### 模式 3：单结构统一同步协议

适用于：

- `ThreadServer` 的 per-request 响应等待与通知

原则：

- 不要把“状态”“条件”“生命周期”拆到多个 dict 和多个锁域里
- 尽量合并成一个 per-request pending record

---

## 7. 最终结论

如果目标是让 **单 CRM 单 Worker** 在 `Python 3.14.3t` 下安全启用真实线程并发，那么当前仓库并不是“只差一个线程池”。

最先必须处理的不是业务 CRM 本身，而是框架层这些共享状态：

- `HttpServer.response_futures`
- `ThreadServer` 的响应协调协议
- `transferable.py` 的全局注册表
- `ZmqServer` 的回包模型

换句话说，**free-threading 在这个仓库里真正暴露出来的，不是“某个 list 没加锁”这么简单，而是若干处状态所有权设计还不够清晰。**

只要先把这些点处理干净，后续再去上 Scheduler、Router、IPC v2，架构会稳很多；否则并发越强，隐藏问题暴露越快。
