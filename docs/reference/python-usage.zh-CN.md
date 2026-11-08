# Python SDK 用法

本指南说明 C-Two 0.10.1 / c3 0.6.0 的 Python SDK 用法，包括 CRM 编写、调用、可移植的 FastDB 0.2.4 载荷和 Runtime 所有权。

[English](python-usage.md) · 简体中文

[0.10.0 发布说明](../releases/0.10.0.md)介绍从 0.9.x 迁移的事项，包括 `cc.Runtime()`、`cc.endpoint.server_credential()`、Unix schema 4 和额外的绑定原因。CRM 契约通过同进程调用、本地 IPC 和 HTTP relay 暴露有状态资源；可移植 Python 载荷使用可选的 FastDB 0.2.4 extra。安装步骤及可运行的入门示例见[快速开始](../../README.md#quickstart)。

## 可移植载荷（FastDB）

可移植方法使用下面明确声明的 FastDB 绑定。

需要跨语言边界传输结构化数据的方法必须绑定明确的 FastDB 规格。FastDB Core 负责嵌套载荷语义，包括验证、规范身份、二进制布局、构建器、视图、失效处理以及仅针对载荷的代码生成；C-Two 负责外层契约、路由、传输和生命周期：

```python
import json

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

VALUE_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one",
         "type": {"kind": "u8", "nullable": False}},
    ],
    "components": [],
}


@cc.crm(namespace='demo.payload', version='0.1.0')
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload: ...


def build_value(value: int) -> Payload:
    spec = CompiledSpec.compile(json.dumps(VALUE_SPEC).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


class EchoResource:
    def echo(self, payload: Payload) -> Payload:
        return payload


cc.register(Echo, EchoResource(), name='echo')
source = build_value(7)
try:
    with cc.connect(Echo, name='echo') as echo:
        result = echo.echo(source)
        result.close()
finally:
    source.close()
    cc.unregister('echo')
    cc.shutdown()
```

可移植方法在每个方向携带零个或一个 `Payload` 封套；C-Two 将每个嵌套规格作为不透明 JSON 值嵌入，且不重新解释它。没有明确绑定的方法仍可使用普通 Python 值进行仅限 Python 的原型开发，但可移植描述符导出与代码生成会诊断并拒绝这些方法。

`cc.hold(echo.echo)(source)` 将响应保留为 `HeldResult`，它持有一份本地副本：复制完成后传输内存就已释放，所以 hold 不会占住服务端的共享内存。显式 `release()` 让载荷所有者失效一次，再释放本地副本；失效操作不会重放，此后该值不可访问。如果导出被拒绝，先释放额外导出的 memoryview 再重试；拒绝发生在失效操作之前。手工构造的 `HeldResult(release_cb=...)` 回调仍只运行一次。常规作用域使用 `with cc.hold(method)(args) as held`，需要超出作用域存活的值应实体化。

`cc.observe.memory_stats()['holds']` 统计这些本地副本：`active_holds`、`total_held_bytes`（逻辑字节，不是 SHM 容量）、`oldest_hold_seconds` 与 `by_direction`。backing、重组与载荷缓冲预算的各项及其核算边界见[内存策略](memory-policy.md#statistics-and-payload-lifetime)。

## 远程连接与截止时间

直接 IPC 使用资源进程的 `cc.server_address()` 值。直接 IPC 无需 relay。若要按路由名发现，在注册或连接前设置 `cc.set_relay_anchor(...)`，并单独运行 `c3 relay`。注册所在进程的调用在未指定地址时优先使用同进程分派。

```python
source = build_value(7)
try:
    # Replace the address with the resource process's cc.server_address().
    with cc.connect(Echo, name='echo', address='ipc://resource-process', timeout=1.0) as echo:
        reply = cc.with_call_options(echo, timeout=5.0).echo(source)
        reply.close()
finally:
    source.close()
```

连接的 `timeout` 是以秒为单位的单一预算，覆盖连接池等待、操作系统连接、握手、当前路由查找和 relay 获取。省略或传入 `None` 不设置调用方截止时间；零会立即过期。给了 `timeout` 时，它取代操作系统连接与握手各自固定的 5 秒守卫，命名空间门锁或命名管道的等待也以它为限；不给时保留这些守卫。负数和非有限值会被拒绝。过期时抛出 `CallDeadlineExceeded`，包含 `operation=connect`、`transport_phase=pre_dispatch` 及失败的 `stage`；资源方法尚未执行。

业务调用的等待独立于连接等待。`cc.with_call_options` 返回现有连接的视图；其超时不会成为资源方法参数。调用截止时间可能在分派后结束调用方等待，此时原生任务仍持有输入直到完成。它不会取消正在运行的资源方法，也不允许重放。直接 IPC 默认无限等待；HTTP 使用已配置的逻辑调用预算。同进程同步调用接受继承的等待上限或无限等待。默认值和有限调用准入限制见[配置](configuration.zh-CN.md#调用等待与准入)。

共享一个 IPC 域的 Unix 进程使用相同的已配置根目录（`cc.endpoint.set_local_endpoint(root=...)` 或 `C2_IPC_ROOT`）。应用负责创建自定义根目录；在确认所有权及免受其他用户修改的保护后，可以接受 `0755`。默认权限为 `0700`，套接字权限为 `0600`。Windows 在跨登录会话时使用 TokenUser 范围内的 Named Pipe，并拒绝 Unix 根目录覆盖值。原生连接要求同一用户且服务端完整性级别至少为 Medium；跨会话载荷自动避开无法访问的 Local 映射。C-Two 0.10.0 将 RPC 握手从 13 升至 14，并在前面增加永久固定的版本前导信息，因此通信双方应一起升级。两个版本都实现固定前导信息但版本不同时，会抛出 `CoreError`，其中 `category='protocol_version_mismatch'`，`local_protocol_version` / `peer_protocol_version` 非零（其他错误中均为 `None`）；与 0.9.x 混用时仍是一般连接或协议失败。参见 [0.10.0 迁移说明](../releases/0.10.0.md)及[握手规格](ipc-handshake.md)，以及[端点配置](configuration.zh-CN.md#本地端点位置与平台差异)和[生命周期管理](local-endpoint-lifecycle.md)。

## 底层服务端桥接

`c_two.transport.Server` 是绑定到一个原生 `RuntimeSession` 的桥接对象。应用通常使用模块级 `cc.register()` / `cc.shutdown()` 或显式的 `cc.Runtime()` 实例。底层 Host 可以从明确配置的会话取得桥接对象：

```python
from c_two._native import RuntimeSession

session = RuntimeSession(server_id='local-host', use_process_relay_anchor=False)
server = session.ensure_server_bridge()
```

会话拥有身份、IPC 覆盖值和 relay 配置。`Server(runtime_session, ...)` 不再接受 `bind_address`、`ipc_overrides`、`server_id` 或 `server_instance_id`；注册、注销及关闭也不再接受替换会话或按操作指定的 relay anchor。独立会话应显式配置，而非继承 Python 进程设置。`start()` 不接受超时参数；`shutdown(timeout=...)` 保留 Core 拥有的截止时间及完成结果。Host 用完后通过 `shutdown()` 关闭桥接对象。

## 载荷生命周期

已经证实的可移植接收路径打开的是**由副本支持**的载荷所有者。`cc.hold()` 是生命周期契约，不构成零拷贝声明：它一同保留响应的本地副本与载荷所有者，并保证释放本地副本*之前*，先让载荷所有者及其受检查的视图失效。

```python
with cc.hold(echo.echo)(source) as held:
    payload = held.value                    # FastDB Payload owner; checked views stay valid
    ...
# leaving the block (or held.release()) invalidates the payload owner and checked views, then frees the local copy
```

释放有三层方式：显式 `.release()`、`with` 上下文管理器，以及遗漏前两者时发出警告的 `__del__` 兜底。`cc.observe.memory_stats()["holds"]` 报告监控中的活动 hold 数量。

`held.unsafe_buffer` 将保留的原始线格式缓冲区作为 `memoryview` 逃逸入口暴露。从中派生的原始 NumPy 数组或指针绕开 FastDB 的载荷所有者与受检查视图，**无法通过机制撤销**；要把值保存到 hold 作用域之外，应先通过 FastDB 实体化。

服务端的可移植输入默认由服务端拥有。以 `cc.register(..., input_lifetime={...: 'borrowed'})` 注册，可将指定方法设为仅在调用期间借用输入：请求租约在资源方法运行前就已释放。方法返回或抛出异常时，C-Two 先使载荷所有者及其受检查视图失效，再释放本地输入副本。方法返回后不要保留借用的载荷或视图。

`cc.transfer` 只接受 `input` 和 `output`。原有 `buffer` 关键字已移除，包括 `None` 和 `"view"`；传入它会抛出 `TypeError`。应删除该关键字，并通过注册级 `input_lifetime` 选择借用输入。模块级 `auto_transfer` 遵循相同规则。

## 服务端流

标了 `@cc.stream` 的 CRM 方法对一份输入返回零条或多条条目。返回类型写成 `Iterator[T]` 或 `Generator[T, None, None]`，`T` 是一条条目；`@cc.transfer(output=...)` 描述一条条目的编码，与 unary 的返回值一样。`@cc.stream` 可以和 `@cc.read`、`@cc.write`、`@cc.transfer` 一起用。资源把这个方法写成生成器，或者返回一个迭代器：

```python
from collections.abc import Iterator

@cc.crm(namespace='science.sim', version='0.1.0')
class Simulation:
    @cc.stream
    @cc.transfer(input=PARAMETERS, output=STATE)
    def steps(self, parameters: Payload) -> Iterator[Payload]: ...

class BasinSimulation:
    def steps(self, parameters: Payload) -> Iterator[Payload]:
        model = initialize(parameters)
        try:
            while model.has_next():
                model.advance()
                yield model.export_owned_state()
        finally:
            model.finish()

cc.register(Simulation, BasinSimulation(), name='basin')

with cc.connect(Simulation, name='basin', address='ipc://resource-process') as sim:
    results = sim.steps(parameters)
    with results:
        for state in results:
            consume(state)
    outcome = results.close()
```

生成器的每一次 `next()` 是一个资源推进：只在推进期间持有路由许可与 GIL，经 IPC 时还持有一个执行 worker，条目之间什么都不占。一条流开始时复制一次 context，建立生成器、每次 `next()` 与最后的 `close()` 都在这一个 context 里运行，所以生成器里设置的 `contextvars` 与 `decimal` context 跨 worker 跟着它走，也不会漏进 worker 自己的 context。经 IPC 时每条条目在它的推进里编码，生成器 yield 之后可以复用或修改同一个对象；在同一进程里，消费方拿到的就是 yield 出来的那个对象。借用的输入（`input_lifetime='borrowed'`）一直有效，到生成器关闭之后才先失效、再释放。

调用返回一个 `cc.Stream`，迭代得到解码后的条目。离开 `with` 块或丢弃这个对象会取消它，不等待。`close(timeout=30.0)` 在流还在运行时先取消，再等原调用结束：`None` 一直等，`0` 只观察一次。它返回一个 dict，含 `completed`、`error` 与 `complete_items`；`completed` 为 False 时清理还在进行，之后可以再调用 `close()` 观察。`error` 是这条流结束时的异常，自然结束时为 `None`；服务端或 relay 结束的流报 `CallCancelled`，消息写明是 shutdown、注销还是 relay 关闭。`cc.hold(sim.steps)(parameters)` 返回由 `HeldResult` 组成的流，每条各自保留本地副本直到释放；多份 hold 可以同时存在，用满载荷缓冲预算时，下一条等到某一份释放。`cc.with_call_options` 给的截止时间涵盖整条迭代，不为每条重置；到期只结束调用方的等待。每条流占一个 `max_outstanding_calls` 名额，直到它的真实终态。

在注册资源的进程里，流直接传递 yield 出来的对象，不编码，按路由自己的并发限制推进；有限的调用截止时间在派发前被拒绝。经 IPC 时，Python 与 Rust 的资源和客户端可以互相开流；经 relay 时它们经 HTTP 开流，`cc.Stream`、`close` 与 hold 的行为相同。经 relay 时，消费方停止读取后，资源最多还能多推进 HTTP 与 TCP 缓冲装得下的那么多条，小条目可达数千条。推进、取消与 shutdown 见 [Runtime 生命周期](runtime-lifecycle.md#execution-admission)。

## 入口

### Python SDK

`c_two` 根模块只容纳日常 API：契约装饰器、`hold` / `HeldResult`、register / connect / close / unregister / serve / shutdown、`server_address`、`with_call_options`、各 setter、`Runtime`、`ConcurrencyConfig` 和 `error`。端点维护、诊断与契约工具分别位于 `c_two.endpoint`、`c_two.observe` 和 `c_two.contract`；Rust SDK 在 `c_two::` 下使用相同的三个模块。不要为这些能力新增根模块名称、别名或第二种拼写；并发模式和输入生命周期等选项值使用字符串字面量。

应用以 `import c_two as cc` 开始。需要时导入专用模块：

```python
from c_two import endpoint, observe, contract
```

Rust 使用者从面向用户的 `c_two` 包导入，不使用 Core 或传输 crate 名称：

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

生成的 Rust 模块可以使用隐藏的 `c_two::generated` 接缝。普通应用代码不得直接组装 `c2-ipc`、`c2-http`、`c2-server`、`SyncClient` 或路由绑定。

契约导出不选择 Runtime。并发模式为 `'parallel'`、`'exclusive'` 和 `'read_parallel'`；注册输入生命周期为 `'materialized'` 和 `'borrowed'`。

- 编写契约：`@cc.crm`、`@cc.read`、`@cc.write`、`@cc.on_shutdown`、`@cc.transfer`、`cc.hold`
- 调用：`cc.with_call_options`（按调用设置截止时间的视图，见[远程连接与截止时间](#远程连接与截止时间)）
- Runtime：`cc.Runtime`，其实例方法包括 `set_local_endpoint`、`set_server`、`set_client`、`set_transport_policy`、`set_relay_anchor`、`set_call_execution_limits`、`register`、`connect`、`unregister`、`start`、`server_id`、`server_address`、`server_credential`、`local_endpoint_context`、`shutdown`、`connection_events`、`lifecycle_snapshot`、`shutdown_outcome`、`memory_stats` 和 `call_execution_snapshot`
- 默认 Runtime：`cc.register`、`cc.connect`、`cc.unregister`、`cc.serve`、`cc.shutdown`、`cc.server_address`、`cc.endpoint.server_credential`、`cc.set_server`、`cc.set_client`、`cc.set_relay_anchor`、`cc.set_transport_policy`、`cc.endpoint.set_local_endpoint`；`cc.close(proxy)` 跟随代理自身的连接
- 契约：`cc.contract.export_descriptor`、`cc.contract.release_ref`、`cc.contract.compile_artifacts`、`cc.contract.diagnostics`
- 端点维护：`cc.endpoint.inspect_endpoint`、`cc.endpoint.reap_endpoint`、`cc.endpoint.sweep_endpoints`、`cc.endpoint.EndpointCredential`（不可变的原生值；显式 JSON 导入/导出）
- 连接观察：`cc.observe.connection_observation`、`cc.observe.connection_events`、`cc.observe.ConnectionObservation`、`cc.observe.ConnectionClosed`、`cc.observe.ConnectionEvent`、`cc.observe.ConnectionEvents`、`cc.observe.ConnectionEventsLagged`
- 错误：`cc.error.CoreError`（结构化本地/Core 错误）、`cc.error.CCError`（语义错误），共同基类为 `cc.error.CCBaseError`
- 监控：`cc.observe.memory_stats`、`cc.observe.call_execution_snapshot`、`cc.observe.lifecycle_snapshot`、`cc.observe.shutdown_outcome`（只读）

在注册前使用 `cc.set_server(startup_timeout=...)` 选择原生启动等待。启动超时包含 `CoreError.lifecycle_kind == 'startup_timeout'`、`startup_stage` 和 `timeout_seconds`；参见[启动配置与错误](configuration.zh-CN.md#服务启动超时)。

监听器达到 Ready 后，`cc.endpoint.server_credential() -> cc.endpoint.EndpointCredential | None` 从现有生命周期观察器返回其原生凭据，无需检查或文件系统 I/O。启动前以及终止关闭后返回 `None`；快照不能证明服务仍存活，也不能授予清理权限。参见[生命周期指南](local-endpoint-lifecycle.md#exact-cleanup-and-bounded-sweeps)。

Runtime 范围内的 IPC 策略、调用准入限制及 relay 设置见[配置指南](configuration.zh-CN.md)。

[生命周期 API 指南](local-endpoint-lifecycle.md)规定观察返回值、超时/结束/滞后行为以及错误结果。其中的[绑定/连接章节](local-endpoint-lifecycle.md#bind-and-connect-failures)记录公共 `cc.error.CoreError` 上的结构化操作系统字段。这些观察不能证明进程退出，也不能授权重放或清理。

### CRM 契约、客户端与名称

CRM 契约类是接口，其方法体为 `...`。`@cc.crm()` 要求 `namespace` 和 `version`；契约类名不加 `I` 前缀。资源实现是以领域命名的普通类，例如 `NestedGrid` 或 `PostgresVectorLayer`；生成代码可以使用 `{ContractName}Impl`。可传输类使用 `GridAttribute` 等描述性数据名称。

```python
@cc.crm(namespace='cc.demo', version='0.1.0')
class Grid:
    def some_method(self, arg: int) -> str:
        ...
```

任何调用 `cc.connect(CRMClass, name='...', address='...')` 的代码都是客户端。返回的类型化代理与 CRM 契约匹配，并支持通过上下文管理器关闭：

```python
with cc.connect(Grid, name='grid', address='ipc://server') as grid:
    result = grid.some_method(1)
```

不存在独立的 `compo/` 模块、`@cc.runtime.connect` 装饰器或 `Component` 类型。路由名称是传给 `cc.register(name=...)` 和 `cc.connect(name=...)` 的用户自选字符串，与 CRM `namespace` 有别。不同 CRM 契约，或同一契约的不同实例，可使用不同名称并存。对远程 IPC 和 relay 路径，原生 Runtime 还会匹配从客户端 CRM 类派生的预期 CRM 标签及契约哈希。

### 默认 Runtime API 模式

模块级注册、连接、配置及关闭委托给默认 Python Runtime。`cc.close(proxy)` 跟随创建该代理的 Runtime。`cc.Runtime()` 创建应用拥有的实例，提供相同的注册/连接/配置/关闭方法，以及用于空 Host 的 `start()`：

```python
import c_two as cc

# Server side
cc.set_relay_anchor('http://relay-host:8080')
cc.set_transport_policy(shm_threshold=64 * 1024)
cc.set_server(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
cc.register(Grid, grid_instance, name='grid')
cc.register(Network, net_instance, name='network')
cc.serve()

# Client side, same process: thread preference
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side, direct IPC
cc.set_client(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
grid = cc.connect(Grid, name='grid', address='ipc://my_server')
grid.some_method(arg)
cc.close(grid)

# Client side, relay-based name resolution and routing
cc.set_relay_anchor('http://relay-host:8080')
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Cleanup
cc.unregister('grid')
cc.shutdown()
```

### 独立的 Python Runtime

`cc.Runtime()` 在一个原生会话之上创建独立的 Python 绑定。它的 `set_*`、`register`、`connect`、`unregister` 和 `shutdown` 方法与模块级函数签名相同。模块函数使用另一个默认 Runtime。每个显式实例的代码覆盖值初始为空；进程环境和 `.env` 仍是共享配置来源，由 Core 在通常的冻结点解析。不同 Runtime 需要不同设置时，使用各自的实例 setter。独立性意味着在同一进程内分别拥有路由、连接、观察及配置。

`runtime.start()` 启动未注册 CRM 的 Host，并等待原生就绪及现有 IPC PING/PONG。随后 `runtime.register()` 使用该 Host。没有显式地址时，`runtime.connect()` 仅搜索该实例的本地路由；跨实例 IPC 调用应使用 `address=other.server_address()`，并采用相同的 Unix 端点上下文。`cc.close(proxy)`、代理的上下文管理器、`cc.hold()` 和 `cc.with_call_options()` 均跟随创建该代理的 Runtime。显式实例没有 `serve()` 方法，也没有自动信号或 atexit 清理；`cc.serve()` 处理模块级默认 Runtime。

调用 `runtime.shutdown(timeout=...)` 会为该 SDK 会话启动 Core Runtime 唯一且永久的退役作用域；模块级 `cc.shutdown()` 对默认 Runtime 执行相同操作。该作用域封闭 Core 客户端池，并停止接受新的注册、启动和远程连接任务。待定观察保留旧会话及其槽位；Core 此后自行完成也不会替换它们。应再次调用 SDK shutdown，消费已完成的回执并进行会话交接。零或较短的等待预算只限制当前调用方的观察，不取消驱动任务。如果交接退役之前截止时间到期，shutdown 抛出 `CoreError`，包含 `lifecycle_kind='shutdown_wait_timeout'`、`details.stage` 和 `timeout_seconds`；这不是待定回执。Python 的数值转换在取得 SDK 实例锁前发生。等待取得该锁不计入 shutdown 截止时间；取得锁后，Native Host、Core 与连接池等待共用一个绝对截止时间。关闭错误投影和关闭回调在 Python 绑定锁外运行。回调在会话替换后运行；如果构造替换会话失败，原始槽位及日志仍可用。

关闭从不让已发出的一元调用失败：其他线程上的在途调用照常完成并拿到结果，它们还在执行时，关闭报告 `completed=False`。这个 Runtime 作为资源方的服务端流是例外：关闭在两步之间结束它，先交付已产出的条目，再执行清理，最后以写明 shutdown 的 `CallCancelled` 结束；关闭要等这次清理结束才算完成。这个 Runtime 作为客户端消费的流不会被它自己的关闭结束，要显式关闭或丢弃。一个没设调用期限、在等一个停住的服务端的线程会继续等下去，而 Python 退出前会等所有非守护线程；这类调用请用 `cc.with_call_options(timeout=...)` 设期限。

`cc.serve(shutdown_timeout=30.0)` 在开始服务之前就拒绝无效的预算，之后阻塞到 Unix 上收到 SIGINT 或 SIGTERM、Windows 上收到 SIGINT 或 SIGBREAK，或原生 Host 自己停止（例如收到直接 IPC 关停请求），再以 `shutdown(timeout=shutdown_timeout)` 调用一次关闭并返回其结果。结果为待定时，清理在后台继续，之后再调用 `cc.shutdown()` 可以观察到它完成。`blocking=False` 时立即返回 `None`。

传给远程 `connect()` 的截止时间也约束 SDK 最终发布检查。若原截止时间在发布前到期，connect 抛出对应的截止时间错误，仅释放自身的逻辑客户端，且不重试。其他情况下成功的连接继续正常复用其池化客户端。`runtime_retired` 生命周期错误没有 I/O 原因。仅使用 HTTP 不会创建 IPC 池，shutdown 也不保证每个 HTTP keepalive 套接字都已物理关闭。Rust 底层 Host 的关闭/重启生命周期仍独立且可重启；永久退役仅适用于 Python SDK Runtime 作用域。

完成交接后，已配置的端点选择、传输策略、调用限制和 relay 覆盖值保留现有交接行为，而服务端身份、IPC 覆盖值和启动超时会重置。现有的 `connection_events()` 订阅只观察原会话；新会话需要重新订阅。返回值是正在关闭的会话的结果；已保留的载荷继续遵循自身的生命周期。应用拥有自己创建的任何自定义端点目录；Runtime 不会移除该目录。[预检示例](../../examples/python/independent_runtime.py)在删除新建的私有目录前检查关闭结果。
