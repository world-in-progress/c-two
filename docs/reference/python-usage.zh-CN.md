# Python SDK 用法

本指南说明 C-Two 0.12.0 / c3 0.8.0 的 Python SDK 用法，包括 CRM 编写、调用、服务端流、可移植的 FastDB 0.3.0 载荷、发布/订阅主题和 Runtime 所有权。

[English](python-usage.md) · 简体中文

[0.12.0 发布说明](../releases/0.12.0.md)介绍从 0.11.0 迁移的事项，包括资源归属的发布/订阅主题、IPC 17、C/FFI ABI 9、FastDB 0.3.0（`BuildPlan` 直写共享内存与 `ExternalBytes` 零拷贝解码）以及 `register()` 返回 `Registration`。CRM 契约通过同进程调用、本地 IPC 和 HTTP relay 暴露有状态资源；可移植 Python 载荷使用可选的 FastDB 0.3.0 extra。安装步骤及可运行的入门示例见[快速开始](../../README.zh-CN.md#快速开始)。

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
    # 传入 cc.server_address() 走本地 IPC；省略 address 则走同进程直调。
    with cc.connect(Echo, name='echo', address=cc.server_address()) as echo:
        result = echo.echo(source)
        result.close()
finally:
    source.close()
    cc.unregister('echo')
    cc.shutdown()
```

可移植方法在每个方向携带零个或一个 `Payload` 封套；C-Two 将每个嵌套规格作为不透明 JSON 值嵌入，且不重新解释它。在发送侧（客户端请求、unary 返回值、`@cc.stream` 产出项与 `Publisher.publish(...)`），既可传入已构建的 `Payload`，也可直接传入冻结后的 `fastdb4py.payload.BuildPlan`（资源方法可将返回类型注解为 `BuildPlan`、`Payload | BuildPlan` 或 `Iterator[BuildPlan]`）。经 IPC SHM 传输时，满足直写条件的 `BuildPlan` 会直接写入 C-Two 的 `WritableSlab` 共享内存块，无需中间暂存拷贝。没有明确绑定的方法仍可使用普通 Python 值进行仅限 Python 的原型开发，但可移植描述符导出与代码生成会诊断并拒绝这些方法。

经 IPC 或 relay HTTP 调用时，`cc.hold(echo.echo)(source)` 将解码后的响应保留为 `HeldResult`，它持有一份本地副本：复制完成后传输内存就已释放，所以 hold 不会占住服务端的共享内存。显式 `release()` 让载荷所有者失效一次，再释放本地副本；失效操作不会重放，此后该值不可访问。如果导出被拒绝，先释放额外导出的 memoryview 再重试；拒绝发生在失效操作之前。在同进程直调路径（注册同一 Runtime 且省略 `address`）上，`cc.hold()` 只返回包装原 Python 对象的 `HeldResult(result)`，不经过传输复制，释放包装也不会使原 FastDB 对象失效或计入 `holds`。手工构造的 `HeldResult(release_cb=...)` 回调仍只运行一次。常规作用域使用 `with cc.hold(method)(args) as held`，需要超出作用域存活的值应实体化。

`cc.observe.memory_stats()['holds']` 统计经传输解码的本地副本：`active_holds`、`total_held_bytes`（逻辑字节，不是 SHM 容量）、`oldest_hold_seconds` 与 `by_direction`。各角色的内存和文件溢出预算及其核算边界见[内存策略](memory-policy.md#statistics-and-payload-lifetime)。

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

业务调用的等待独立于连接等待。`cc.with_call_options` 返回现有连接的视图；其超时不会成为资源方法参数。调用截止时间可能在派发后结束调用方等待：已派发的调用会继续占用 `max_outstanding_calls` 调用槽直到真实终态，但请求的发送存储和 IPC SHM 借出在传输写出或拷贝完请求后即可更早释放。超时不会取消正在运行的资源方法，也不会发起重放。直接 IPC 和 relay HTTP 默认均无限等待。同进程同步调用接受继承的等待上限或无限等待。默认值和在途调用准入限制见[配置](configuration.zh-CN.md#调用等待与准入)。

连接跟随路由名。C-Two 已经知道它所服务的那次注册不在了时（服务进程结束，或者之前的调用因路由不存在、已撤下、已关闭或契约不同而在派发前被拒），下一次调用在自己的截止时间内按名字重新获取路由，发往当前的注册：直接 IPC 回到原地址，经 relay 的连接通过 relay 锚点重新解析，可能在本地 IPC 与 relay 之间切换，显式 relay 连接仍走 HTTP。只能从这次调用自己的拒绝得知变化时，这次调用在派发前失败、可以安全重试，下一次调用才跟过去。同进程连接在注册被注销后也一样：下一次调用发往这个名字新的本地注册，或者经 relay 发往远程的注册；注册还在时，调用直接进入资源。服务这个名字的进程负责状态的延续；要让调用方察觉状态变了，就换一个名字。身份与协议的拒绝不会引起重新获取。

共享一个 IPC 域的 Unix 进程使用相同的已配置根目录（`cc.endpoint.set_local_endpoint(root=...)` 或 `C2_IPC_ROOT`）。应用负责创建自定义根目录；在确认所有权及免受其他用户修改的保护后，可以接受 `0755`。默认权限为 `0700`，套接字权限为 `0600`。Windows 在跨登录会话时使用 TokenUser 范围内的 Named Pipe，并拒绝 Unix 根目录覆盖值。原生连接要求同一用户且服务端完整性级别至少为 Medium；跨会话载荷自动避开无法访问的 Local 映射。当前源码使用 IPC 18，沿用 0.10.0 引入的永久固定版本前导。通信双方应一起升级。已发布的 0.12.0 使用 IPC 17。两个版本都实现固定前导信息但版本不同时，会抛出 `CoreError`，其中 `category='protocol_version_mismatch'`，`local_protocol_version` / `peer_protocol_version` 非零（其他错误中均为 `None`）；与 0.9.x 混用时仍是一般连接或协议失败。参见[握手规格](ipc-handshake.md)，以及[端点配置](configuration.zh-CN.md#本地端点位置与平台差异)和[生命周期管理](local-endpoint-lifecycle.md)。

## 带键一元调用与调用结局

使用 `cc.with_call_id(proxy.step, call_id)(*args, **kwargs)` 提交带调用标识（call ID）的一元调用。对同一路由的每个逻辑操作使用新的标识。调用标识是由 `[A-Za-z0-9_-]` 中的 1–128 个 ASCII 字符组成的字符串，可跨物理连接和客户端进程使用。重复提交时，如果方法和编码后输入的 SHA-256 摘要一致，就附着到当前注册中匹配的记录，或重放其调用终态。方法或摘要冲突时报 `cc.error.CallIdConflict`（721）。

`cc.query_call(proxy, call_id, *, timeout=...)` 返回只读的 `cc.CallOutcome` 调用结局（call outcome）快照，无需等待资源执行结束，也不占用业务调用槽。省略 `timeout` 时沿用代理的超时策略，传入秒数可限定查询等待时间，传入 `None` 则客户端不设截止时间。

| 字段 | 含义 |
| --- | --- |
| `status` | `in_flight` 包括仍在接收输入或等待进入资源的调用。`succeeded` 和 `failed` 表示调用已结算。`unknown` 表示当前注册没有匹配记录。 |
| `method_name` | 已知调用标识对应的方法，否则为 `None`。 |
| `result_available`、`result_unavailable_reason` | 成功响应是否可恢复。成功但结果不可用时，原因为 `'not_retained'` 或 `'evicted'`，其余情况下原因为 `None`。 |
| `error_code`、`error_details_available` | 失败调用的整数错误码（其余情况下为 `None`），以及能否恢复完整错误。 |

`cc.recover_call(proxy.step, call_id)()` 不接收方法参数。它附着到待执行或正在执行的调用，或重放记录中的响应或错误。调用标识未知时，恢复报 `CallOutcomeUnknown`（723），不进入资源。已确认成功但响应不可用时，报 `CallResultUnavailable`（722）。如果失败调用仅保留错误码，恢复会报该错误码，并附带 `details_evicted='true'`。恢复附着等待期间会占用一元调用的调用槽。见[错误参考](errors.md#call-outcome-errors)。

`cc.hold` 可以包在任一带键操作外侧，也可以先包装绑定的方法。提交时可用 `cc.hold(cc.with_call_id(proxy.step, call_id))(*args)` 或 `cc.with_call_id(cc.hold(proxy.step), call_id)(*args)`。恢复时可用 `cc.hold(cc.recover_call(proxy.step, call_id))()` 或 `cc.recover_call(cc.hold(proxy.step), call_id)()`。这四种写法均返回保留本地响应副本的 `HeldResult`。在同一个可调用对象上组合提交与恢复会引发 `TypeError`。两个带键包装器都要求绑定的一元 CRM 方法，拒绝服务端流和主题，后者仍可单独使用 `cc.hold`。

通过同进程代理对已注册路由执行调用结局操作时，使用该 Runtime 的本机 IPC 编码路径，与远端客户端共用同一个 Host 调用结局表。无调用标识的同进程调用仍直接传递 Python 对象。手工构造且未关联 Runtime 注册路由的 `thread_local` 代理会在派发前拒绝调用结局操作。

如果带调用标识的调用抛出 `cc.error.CCBaseError`，且 `transport_phase == 'dispatch_uncertain'`，可从只读属性 `err.call_id` 读取该标识。为保证至多执行一次，每个调用标识只提交一次，派发不确定时使用查询或恢复。记录可能因淘汰、注销或 Host 重启而消失。未命中记录时，重新提交可能再次执行。遇到 `unknown` 或 723，先用应用的持久化状态对账，再决定是否启动另一次操作。见[调用结局生命周期](runtime-lifecycle.md#keyed-unary-call-lifecycle)。

## 底层服务端桥接

`c_two.transport.Server` 是绑定到一个原生 `RuntimeSession` 的桥接对象。应用通常使用模块级 `cc.register()` / `cc.shutdown()` 或显式的 `cc.Runtime()` 实例。底层 Host 可以从明确配置的会话取得桥接对象：

```python
from c_two._native import RuntimeSession

session = RuntimeSession(server_id='local-host', use_process_relay_anchor=False)
server = session.ensure_server_bridge()
```

`Server(runtime_session, ...)` 使用给定会话的 Host 设置，包括服务端身份、容量及 relay 配置。注册、注销和关闭均使用该会话。请显式配置独立会话。`start()` 不接受超时参数，`shutdown(timeout=...)` 观察 Core 的关闭回执。Host 用完后调用 `shutdown()`。

## 载荷生命周期

经远程传输接收时，C-Two 会将收到的 SHM 或重组帧一次性复制到温池复用的本地缓冲区并立即归还传输借出（ADR 0001），随后通过 `Payload.open_external` / `ExternalBytes` 直接在该本地副本上打开 FastDB `Payload`，不再产生二次解码拷贝。`cc.hold()` 是生命周期契约：它一同保留响应的本地副本与载荷所有者，并保证释放或回收本地副本*之前*，先让载荷所有者及其受检查的视图失效（同进程直调路径上，`cc.hold()` 只用 `HeldResult(result)` 包装原 Python 对象，没有传输副本也不会触发 FastDB 失效）。

```python
with cc.connect(Echo, name='echo', address=cc.server_address()) as echo:
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

生成器的每一次 `next()` 是一个资源推进：只在推进期间持有路由许可与 GIL，经 IPC 时还持有一个执行 worker，条目之间什么都不占。一条流开始时复制一次 context，建立生成器、每次 `next()` 与最后的 `close()` 都在这一个 context 里运行，所以生成器里设置的 `contextvars` 与 `decimal` context 跨 worker 跟着它走，也不会漏进 worker 自己的 context。经 IPC 时每条条目在它的推进里编码，生成器 yield 之后可以复用或修改同一个对象；在同一进程里，消费方拿到的就是 yield 出来的那个对象。经传输解码且以 `cc.register(..., input_lifetime={'steps': 'borrowed'})` 注册时，借用的输入一直有效，到生成器关闭之后才先失效、再释放（同进程直调直接传递调用方对象，不做借用输入失效）。

调用返回一个 `cc.Stream`，迭代得到解码后的条目。离开 `with` 块或丢弃这个对象会取消它，不等待。`close(timeout=30.0)` 在流还在运行时先取消，再等原调用结束：`None` 一直等，`0` 只观察一次。它返回一个 dict，含 `completed`、`error` 与 `complete_items`；`completed` 为 False 时清理还在进行，之后可以再调用 `close()` 观察。`error` 是这条流结束时的异常，自然结束时为 `None`；服务端或 relay 结束的流报 `CallCancelled`，消息写明是 shutdown、注销还是 relay 关闭。经 IPC 或 relay HTTP 时，`cc.hold(sim.steps)(parameters)` 返回由 `HeldResult` 组成的流，每条各自保留本地副本直到释放；多份 hold 可以同时存在，用满客户端域的内存预算时，下一条等到某一份释放（同进程直调路径上，每条只以 `HeldResult(item)` 包装原对象，不经传输复制也不计入 `holds`）。`cc.with_call_options` 给的截止时间涵盖整条迭代，不为每条重置；到期只结束调用方的等待。每条流占一个 `max_outstanding_calls` 名额，直到它的真实终态。

在注册资源的进程里，流直接传递 yield 出来的对象，不编码，按路由自己的并发限制推进；有限的调用截止时间在派发前被拒绝。经 IPC 时，Python 与 Rust 的资源和客户端可以互相开流；经 relay 时它们经 HTTP 开流，`cc.Stream`、`close` 与 hold 的行为相同。经 relay 时，消费方停止读取后，资源最多还能多推进 HTTP 与 TCP 缓冲装得下的那么多条，小条目可达数千条。推进、取消与 shutdown 见 [Runtime 生命周期](runtime-lifecycle.md#execution-admission)。

## 发布/订阅主题

主题是与一元调用、服务端流正交的第三种连接原语：资源发布不可变帧，任意数量的本地与远端订阅者接收它们（[ADR 0005](../decisions/0005-publish-subscribe.md)）。在 CRM 契约上用 `@cc.topic` 声明——零参数、不带 `@cc.read`/`@cc.write`/`@cc.stream` 访问类别、一个描述发布值的返回注解，且资源侧不需要实现。`@cc.transfer(output=...)` 选择可移植的 FastDB 绑定；其他返回注解仅在 Python 对端之间以 Python 私有 pickle 传输，可移植描述符导出会拒绝它。

```python
@cc.crm(namespace='demo.telemetry', version='0.1.0')
class Telemetry:
    @cc.read
    def device_name(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def heartbeat(self) -> int: ...


class TelemetryImpl:
    def device_name(self) -> str:
        return 'sensor-1'
```

`cc.register(...)`（以及 `runtime.register(...)`）返回 `Registration` 而不再是 `str`。Runtime 保持注册存活；丢弃或被回收该句柄绝不会注销路由——注销仍由 `cc.unregister(name)` 完成。句柄暴露 `registration.name`（路由名）、`registration.is_closed`、`registration.active_subscribers` 和 `registration.publisher(topic)`，返回的 `Publisher` 绑定这一次注册：注册结束后 `publish()` 抛出 `ResourceClosed`（707），绝不写入后续同名的新注册。`Publisher.publish(value)` 返回该帧单调递增的注册内 `seq`，且绝不阻塞在慢订阅者上；它还暴露 `topic_name`、`is_closed` 与 `head_seq`。注册接受两个按契约已声明主题校验的选项：`topic_capacity={'state': 4}` 设置各主题的历史环深度（默认 `1`，最大 65,536；始终保留最新成功发布的一帧），`initial_topics={'state': {...}}` 在路由开放前以同一发布逻辑原子提交初始帧——序列化或容量失败会整体回滚，路由不开放。

```python
registration = cc.register(
    Telemetry, TelemetryImpl(), name='telemetry',
    topic_capacity={'state': 4, 'heartbeat': 8},
    initial_topics={'state': {'online': True, 'voltage': 3.3}},
)
state_pub = registration.publisher('state')
state_pub.publish({'online': True, 'voltage': 3.1})  # 返回该帧的 seq
```

客户端在已连接的 CRM 实例上直接调用主题成员，或使用 `proxy.subscribe(topic, ...)` 订阅。两者都接受 `start`（默认 `'latest'`：首条交付是保留的最新帧，含初始帧；或 `'next'`：只收附着后发布的帧）与 `on_lag`（默认 `'skip'`：跳到最新可得帧；或 `'error'`：以 `SubscriptionLagged` 终止）。它们返回一个 `Subscription`，迭代产出不可变的 `Delivery(value, seq, dropped)`；`skip` 下 `dropped` 记录在此之前跳过的帧数。单次订阅至多跨越一次注册：注销路由或停止 Host 会以写明原因的 `CallCancelled`（718）结束它，下一次 `subscribe()` 跟随当前注册。`subscription.start_seq` 报告 READY 序号；`cancel()` 不阻塞地提交取消；`close(timeout=30.0)` 先取消再观察真实终态，返回 `TopicCloseReceipt`（`completed`、`error`、`complete_items`）——与流的形状一致。`cc.with_call_options(...)` 视图承载订阅策略，同进程订阅拒绝有限超时。`cc.hold(telemetry.state)()` 产出 `Delivery[HeldResult[T]]`：每份 `.value` 持有一份保留的本地副本，订阅关闭后仍有效，由其所有者释放。

```python
telemetry = cc.connect(Telemetry, name='telemetry')
latest = telemetry.state()                          # start='latest', on_lag='skip'
following = cc.with_call_options(telemetry).heartbeat(start='next')
delivery = next(latest)                             # Delivery(value, seq, dropped)
state_pub.publish({'online': True, 'voltage': 3.0})
beats = [next(following).value for _ in range(3)]
latest.close(timeout=5.0)
following.close(timeout=5.0)
cc.close(telemetry)
```

同进程路径上，发布在 `publish()` 时对可变 Python 对象取不可变快照，发布者后续修改或单个读者释放都不会影响其他读者。经直连 IPC 时，帧经一块按订阅者借出的封存共享内存块传输；经 relay 时走 relay 的主题 HTTP 转发。订阅全生命周期占用所属角色域的一个 `max_outstanding_calls` 槽并受内部 `N − 1` 守卫约束，不占路由并发许可、不占执行 worker，也不计入方法的 `max_pending`。`publish()` 放不进主题预算时立即以 `CallCapacityExceeded`（717）失败，保留上一帧且不消耗 `seq`——主题历史绝不隐式溢写磁盘。完整可运行示例见 [pubsub.py](../../examples/python/pubsub.py)；线格式规则见[握手规格](ipc-handshake.md#topic-subscriptions)，错误语义见[错误参考](errors.md#semantic-error-wire-and-sdk-projection)。

## 常驻模拟范式

CRM 方法默认按 `@cc.write` 处理。在 `ConcurrencyConfig(mode='read_parallel', max_workers=...)` 下，写调用互斥执行，多个只读调用可在 `max_workers` 额度内并发（遇到排队的写调用时仍按读写锁顺序串行）。对于需要在持续推进的同时响应状态查看、参数干预与实时订阅的常驻科学模拟，不要把长时间阻塞的步进循环塞进单个 `@cc.write` 调用里，建议采用以下模式：

- 在资源对象内部用后台线程驱动时间步进循环，在步界边界同步内部状态与待生效干预。
- 注册路由时采用 `ConcurrencyConfig(mode='read_parallel', max_workers=...)`，让多个只读观察方并发访问而互不阻塞。
- 保持契约方法简短：用 `@cc.read` 返回最新快照，用 `@cc.write` 将干预写入队列交由步进循环消费，用 `@cc.stream` 配合 `@cc.read` 持续推送已发布的状态帧。注意 Core 会先等待正在执行的单次流 `next()` 推进返回，之后 Python 才调用 `@cc.on_shutdown`，因此每个 `next()` 推进必须能自行尽快返回，或在调用 `unregister()` / `shutdown()` 前由应用侧发信号让其退出；`@cc.on_shutdown` 负责在活跃推进排空后做最终资源收尾，而不能用来唤醒一个正阻塞注销的流步骤。
- 需要从在线状态分叉做假设推演时，在应用层克隆资源状态，用 `register()` 动态挂出临时分支路由，推演结束后调用 `unregister()` 撤下；主路由上正在推进的流与并发调用不受影响。

完整的可运行示例见 [resident_simulation.py](../../examples/python/resident_simulation.py)，演示了后台步进、实时流订阅、参数干预、运行期动态分叉注册与清理停机。

### 面向 Web 前端时的部署分层

`c3 relay` 是面向 C-Two 对端的内网路由设施，而不是直接暴露给浏览器的公网接口；浏览器运行在沙盒内，既无法使用本地 IPC，也无法自行拉起 relay。需要用 Web 界面查看、操控或订阅常驻资源时，应在应用层放置一个 Python 后端网关（BFF），由该网关按常规 `cc.connect(...)` 经直连 IPC 或内网 `c3 relay` 接入资源，再由网关向浏览器提供 HTTP 或 WebSocket 接口。

## 入口

### Python SDK

`c_two` 根模块只容纳日常 API：契约装饰器（`crm`、`read`、`write`、`stream`、`topic`、`transfer`、`on_shutdown`）、`Stream` / `StreamCloseReceipt`、`Registration` / `Publisher` / `Subscription` / `Delivery` / `TopicCloseReceipt`、`hold` / `HeldResult`、register / connect / close / unregister / serve / shutdown、`server_address`、`with_call_options`、`with_call_id` / `recover_call` / `query_call` / `CallOutcome`、各 setter、`Runtime`、`ConcurrencyConfig` 和 `error`。端点维护、诊断与契约工具分别位于 `c_two.endpoint`、`c_two.observe` 和 `c_two.contract`；Rust SDK 在 `c_two::` 下使用相同的三个模块。不要为这些能力新增根模块名称、别名或第二种拼写；并发模式和输入生命周期等选项值使用字符串字面量。

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

- 编写契约：`@cc.crm`、`@cc.read`、`@cc.write`、`@cc.stream`、`@cc.topic`、`@cc.on_shutdown`、`@cc.transfer`、`cc.hold`、`cc.HeldResult`、`cc.Stream`、`cc.StreamCloseReceipt`、`cc.Registration`、`cc.Publisher`、`cc.Subscription`、`cc.Delivery`、`cc.TopicCloseReceipt`
- 调用：`cc.with_call_options`（按调用设置截止时间的视图）、`cc.with_call_id`、`cc.recover_call`、`cc.query_call` 和 `cc.CallOutcome`。见[带键一元调用与调用结局](#带键一元调用与调用结局)。
- Runtime：`cc.Runtime`，其实例方法包括 `set_local_endpoint`、`set_server`、`set_client`、`set_relay_anchor`、`register`、`connect`、`unregister`、`start`、`server_id`、`server_address`、`server_credential`、`local_endpoint_context`、`shutdown`、`connection_events`、`lifecycle_snapshot`、`shutdown_outcome`、`memory_stats` 和 `call_execution_snapshot`
- 默认 Runtime：`cc.register`、`cc.connect`、`cc.unregister`、`cc.serve`、`cc.shutdown`、`cc.server_address`、`cc.endpoint.server_credential`、`cc.endpoint.server_id`、`cc.set_server`、`cc.set_client`、`cc.set_relay_anchor`、`cc.endpoint.set_local_endpoint`；`cc.close(proxy)` 跟随代理自身的连接
- 契约：`cc.contract.export_descriptor`、`cc.contract.release_ref`、`cc.contract.compile_artifacts`、`cc.contract.diagnostics`
- 端点维护：`cc.endpoint.inspect_endpoint`、`cc.endpoint.reap_endpoint`、`cc.endpoint.sweep_endpoints`、`cc.endpoint.local_endpoint_context`、`cc.endpoint.LocalEndpointContext`、`cc.endpoint.EndpointCredential`（不可变的原生值；显式 JSON 导入/导出）
- 连接观察：`cc.observe.connection_observation`、`cc.observe.connection_events`、`cc.observe.ConnectionObservation`、`cc.observe.ConnectionClosed`、`cc.observe.ConnectionEvent`、`cc.observe.ConnectionEvents`、`cc.observe.ConnectionEventsLagged`
- 错误：`cc.error.ErrorCode`、`cc.error.CoreError`（结构化本地/Core 错误，含只读 `semantic`）、`cc.error.CCError`（语义错误，含 `side` 与 `stage`，包括 `ResourceExecutionError`、`SerializationError` 及路由/协议子类），共同基类 `cc.error.CCBaseError`（暴露只读 `transport_phase`）
- 监控：`cc.observe.memory_stats`、`cc.observe.call_execution_snapshot`、`cc.observe.lifecycle_snapshot`、`cc.observe.shutdown_outcome`（只读）

在注册前使用 `cc.set_server(startup_timeout=...)` 选择原生启动等待。启动超时包含 `CoreError.lifecycle_kind == 'startup_timeout'`、`startup_stage` 和 `timeout_seconds`；参见[启动配置与错误](configuration.zh-CN.md#服务启动超时)。

监听器达到 Ready 后，`cc.endpoint.server_credential() -> cc.endpoint.EndpointCredential | None` 从现有生命周期观察器返回其原生凭据，无需检查或文件系统 I/O。启动前以及终止关闭后返回 `None`；快照不能证明服务仍存活，也不能授予清理权限。参见[生命周期指南](local-endpoint-lifecycle.md#exact-cleanup-and-bounded-sweeps)。

`set_client` 设置出站角色域的容量，`set_server` 设置入站角色域的容量和 Host 参数。每次调用仅修改显式传入的字段。下面的第二次调用保留第一次设置的内存和调用槽上限，第三次调用清除调用槽覆盖，恢复环境或推导值。

```python
cc.set_client(memory_budget_bytes=128 * 1024**2, max_outstanding_calls=128)
cc.set_client(file_spill_budget_bytes=0)
cc.set_client(max_outstanding_calls=None)

memory = cc.observe.memory_stats()
calls = cc.observe.call_execution_snapshot()
print(memory["client"]["memory"], calls["server"]["used_operations"])
```

快照可在启动或连接前读取。未激活角色的字节计数和调用槽计数返回 `None`，观察不会激活角色。冻结后仍可提交有效值相同的更新，改变有效值会报 `CoreError.lifecycle_kind == 'config_frozen'`。见[角色域配置](configuration.zh-CN.md#角色域与-sdk-配置)和[诊断字段](memory-policy.md#statistics-and-payload-lifetime)。

[生命周期 API 指南](local-endpoint-lifecycle.md)规定观察返回值、超时/结束/滞后行为以及错误结果。其中的[绑定/连接章节](local-endpoint-lifecycle.md#bind-and-connect-failures)记录公共 `cc.error.CoreError` 上的结构化操作系统字段。这些观察不能证明进程退出，也不能授权重放或清理。

### CRM 契约、客户端与名称

CRM 契约类是接口，其方法体为 `...`。`@cc.crm()` 要求 `namespace` 和 `version`；契约类名不加 `I` 前缀。资源实现是以领域命名的普通类，例如 `NestedGrid` 或 `PostgresVectorLayer`；生成代码可以使用 `{ContractName}Impl`。仅在 Python 对端之间传递的数据类型使用 `GridAttribute` 等描述性领域名称。

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
cc.set_server(server_id='my_server', memory_budget_bytes=256 * 1024**2)
cc.register(Grid, grid_instance, name='grid')
cc.register(Network, net_instance, name='network')
cc.serve()

# Client side, same process: thread preference
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side, direct IPC
cc.set_client(memory_budget_bytes=128 * 1024**2, max_outstanding_calls=128)
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

`cc.Runtime()` 在一个原生会话之上创建独立的 Python 绑定。它的 `set_*`、`register`、`connect`、`unregister` 和 `shutdown` 方法与模块级函数签名相同。模块函数使用另一个默认 Runtime。每个显式实例的代码覆盖值初始为空；进程环境和 `.env` 仍是共享配置来源，由 Core 在各角色激活时各解析一次。不同 Runtime 需要不同设置时，使用各自的实例 setter。独立性意味着在同一进程内分别拥有路由、连接、观察及配置。

`runtime.start()` 启动未注册 CRM 的 Host，并等待原生就绪及现有 IPC PING/PONG。随后 `runtime.register()` 使用该 Host。没有显式地址时，`runtime.connect()` 先搜索该实例的本地路由，本地未命中时再经该实例配置的 relay 锚点按名解析；不经 relay 做跨实例直连 IPC 调用时，使用 `address=other.server_address()` 并采用相同的 Unix 端点上下文。`cc.close(proxy)`、代理的上下文管理器、`cc.hold()` 和 `cc.with_call_options()` 均跟随创建该代理的 Runtime。显式实例没有 `serve()` 方法，也没有自动信号或 atexit 清理；`cc.serve()` 处理模块级默认 Runtime。

调用 `runtime.shutdown(timeout=...)` 会为该 SDK 会话启动 Core Runtime 唯一且永久的退役作用域；模块级 `cc.shutdown()` 对默认 Runtime 执行相同操作。该作用域封闭 Core 客户端池，并停止接受新的注册、启动和远程连接任务。待定观察保留旧会话及其槽位；Core 此后自行完成也不会替换它们。应再次调用 SDK shutdown，消费已完成的回执并进行会话交接。零或较短的等待预算只限制当前调用方的观察，不取消驱动任务。如果交接退役之前截止时间到期，shutdown 抛出 `CoreError`，包含 `lifecycle_kind='shutdown_wait_timeout'`、`details.stage` 和 `timeout_seconds`；这不是待定回执。`shutdown()` 先取得 SDK 实例锁并调用原生 `begin_shutdown(timeout_seconds=...)`（完成超时参数校验与启动退役观察），释放该锁后再调用 `observe()`，使观察等待、关闭错误投影和关闭回调都在 Python 绑定锁外运行；启动退役后，Native Host、Core 与连接池等待共用一个绝对截止时间。回调在会话替换后运行；如果构造替换会话失败，原始槽位及日志仍可用。

关闭从不让已发出的一元调用失败：其他线程上的在途调用照常完成并拿到结果，它们还在执行时，关闭报告 `completed=False`。这个 Runtime 作为资源方的服务端流是例外：关闭在两步之间结束它，先交付已产出的条目，再执行清理，最后以写明 shutdown 的 `CallCancelled` 结束；关闭要等这次清理结束才算完成。这个 Runtime 作为客户端消费的流不会被它自己的关闭结束，要显式关闭或丢弃。一个没设调用期限、在等一个停住的服务端的线程会继续等下去，而 Python 退出前会等所有非守护线程；这类调用请用 `cc.with_call_options(timeout=...)` 设期限。

`cc.serve(shutdown_timeout=30.0)` 在开始服务之前就拒绝无效的预算，之后阻塞到 Unix 上收到 SIGINT 或 SIGTERM、Windows 上收到 SIGINT 或 SIGBREAK，或原生 Host 自己停止（例如收到直接 IPC 关停请求），再以 `shutdown(timeout=shutdown_timeout)` 调用一次关闭并返回其结果。结果为待定时，清理在后台继续，之后再调用 `cc.shutdown()` 可以观察到它完成。`blocking=False` 时立即返回 `None`。

传给远程 `connect()` 的截止时间也约束 SDK 最终发布检查。若原截止时间在发布前到期，connect 抛出对应的截止时间错误，仅释放自身的逻辑客户端，且不重试。其他情况下成功的连接继续正常复用其池化客户端。`runtime_retired` 生命周期错误没有 I/O 原因。仅使用 HTTP 不会创建 IPC 池，shutdown 也不保证每个 HTTP keepalive 套接字都已物理关闭。Rust 底层 Host 的关闭/重启生命周期仍独立且可重启；永久退役仅适用于 Python SDK Runtime 作用域。

只有 `completed=True` 时才交接会话。交接原子移交客户端与服务端的显式覆盖，包括 `server_id` 和 `startup_timeout`，并保留 `retired` 与 `holds` 中的实际占用。 现有的 `connection_events()` 订阅只观察原会话；新会话需要重新订阅。返回值是正在关闭的会话的结果；已保留的载荷继续遵循自身的生命周期。应用拥有自己创建的任何自定义端点目录；Runtime 不会移除该目录。[预检示例](../../examples/python/independent_runtime.py)在删除新建的私有目录前检查关闭结果。
