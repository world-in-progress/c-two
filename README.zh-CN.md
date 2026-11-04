<p align="center">
  <img src="docs/assets/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向有状态资源的 RPC 运行时。
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a> <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/Python-3.10%2B-blue" alt="Python 3.10+" /></a> <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" /> <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a> <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

---

## C-Two 是什么？

C-Two 是为有状态资源设计的 RPC 框架，主要用于分布式科学计算。它的位置介于 ZeroMQ 这类消息库与 gRPC 这类服务框架之间：对象保持原来的写法，由 C-Two 把对它们的调用送到其他进程和机器，并负责消息库交给应用自己处理的部分，包括资源的状态、对资源的并发访问和每次调用的结果。客户端通过相同的方法接口调用本地或远程资源。

**CRM 契约**声明可调用的方法以及 namespace、version；**resource** 是实现这些方法并持有状态的普通类；**client** 通过 `cc.connect(...)` 获得类型化代理。一个进程可以同时承载多个资源，并连接其他资源。名字取自 *CRM to Call*（Core Resource Model to Call），意思是有状态的资源通过它的 CRM 被其他进程和机器调用。

**C-Two 0.10.1** 支持 Python 3.10+。普通调用无 Python 运行时依赖；FastDB 载荷使用 `c-two[fastdb]`（`fastdb4py==0.2.4`）。IPC 握手 14 把版本不匹配报告为结构化错误，Unix 端点命名空间改用格式 3 与带版本号的默认目录，Python 新增独立的 `cc.Runtime()` 实例与 `cc.endpoint.server_credential()`。内存释放失败时由原持有者保留以便重试；已无法交还时保守放弃并计数。见 [0.10.0 更新说明](docs/releases/0.10.0.md)；0.10.1 让 `cc.serve()` 在标准输出编码不了其消息时也不退出（[说明](docs/releases/0.10.1.md)）。

## 有状态 RPC 有什么不同

无状态服务可以把每个请求交给任意一个实例处理，状态存放在外部存储中；C-Two 的调用总是到达同一个资源对象，并可能改变它的状态。两者的差别如下表。

| | 无状态的 RPC 服务 | C-Two 的资源 |
| --- | --- | --- |
| 调用到达哪里 | 服务的任意一个实例，状态通常存放在外部存储中 | 同一个资源对象，状态保存在这个对象里 |
| 并发 | 请求彼此独立 | 方法默认按写处理，同一资源上一次只执行一个；标为 `@cc.read` 的方法可以并行 |
| 出错之后 | 可以通过配置自动重试或对冲 | 可能已经执行过的调用不会被自动重试或重放，Python 与 Rust 的错误会说明调用是否可能已经执行 |
| 关闭时 | 通常有一段宽限期，期满后取消在途请求 | 不再接收新调用，已经发出的调用照常执行完并交付结果 |
| 大数据 | 每条消息整体在内存中编解码，数据较大时通常由应用自行切分 | 单次调用可以传输数 GB 的数组；同机时经共享内存，必要时分片或溢出到文件，上限可以配置 |

## 特点

- **本地与远程调用**：同进程直接传递 Python 对象，同机 IPC 使用 Unix domain socket 或 Windows Named Pipe，跨机器经 HTTP relay。Relay 发现会选择经过验证的本机 IPC 路径或 HTTP 目标；显式 IPC 连接独立于 relay 发现。
- **状态与并发**：资源在调用之间保留状态。`@cc.read`、`@cc.write` 声明访问语义，默认采用写访问；原生路由调度统一约束本地和远程调用的并发与排队数量。
- **载荷与内存**：`@cc.transfer(...)` 绑定 FastDB `Payload` 规格，Python 之间也可以直接传递普通 Python 值。IPC 通过按需 buddy 池、独占共享内存、检查过的分片传输和文件溢出传输大载荷，并受内存预算约束。`cc.hold()` 保留响应的本地副本，注册时可以为方法选择借用输入；两者都只保留本地副本，释放时先让载荷所有者和 checked view 失效。详见[内存策略](docs/reference/memory-policy.md)与 [Python 载荷用法](docs/reference/python-usage.zh-CN.md)。
- **跨语言契约**：契约工具验证可移植描述符，并生成 Python、Rust、TypeScript 绑定；客户端会匹配路由名称、CRM 身份和契约哈希。Python 与 Rust 共用同一个 Rust core 的路由、传输、内存、错误和生命周期机制。
- **relay、工具与生命周期**：原生 c3 CLI 在 Linux、macOS、Windows x64 / ARM64 上提供 relay 与 relay mesh、契约工具和端点维护。`cc.observe.connection_events()` 把客户端和服务端的物理 IPC 连接汇总给一个可关闭的消费者，原因只描述连接事实，不能证明进程已经退出。原生 shutdown 会排空工作，限定范围的端点维护会清理已确认退出的端点，详见[生命周期指南](docs/reference/local-endpoint-lifecycle.md)。

## 安装

```bash
uv pip install c-two
# FastDB 载荷：
uv pip install 'c-two[fastdb]'
```

Windows PowerShell 使用相同命令。Linux、macOS 的 c3 安装命令：

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-installer.sh | sh -s -- --version 0.6.0
```

Windows x64 或 ARM64 使用 [x64 可执行文件](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-x86_64-pc-windows-msvc.exe)、[ARM64 可执行文件](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-aarch64-pc-windows-msvc.exe)或 [PowerShell 安装器](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-installer.ps1)。安装与校验步骤见 [Windows 指南](docs/reference/windows-native-usage.zh-CN.md#安装-python-包和-c3)。

C3 0.6.0 配合 C-Two 0.10.x 使用。IPC 握手 14 与 0.9.x（IPC 13）的对端无法互通，Unix 端点命名空间格式 3 使用新的默认目录，因此同一主机上的 Python 与 Rust SDK、c3 和 Node 使用方要一起升级；先停止旧服务，再升级并重启。C/FFI ABI 6 与 SHM 3 的格式保持不变。见 [0.10.0 迁移说明](docs/releases/0.10.0.md)、[握手规范](docs/reference/ipc-handshake.md)和[端点生命周期指南](docs/reference/local-endpoint-lifecycle.md)。

源码构建需要 Rust 1.95 或更新版本；构建与测试见[开发指南](docs/reference/development.md)。

## 快速开始

保存为 `counter.py`：

```python
import c_two as cc

@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

class CounterResource:
    def __init__(self):
        self.value = 0

    def increment(self, amount: int) -> int:
        self.value += amount
        return self.value

cc.register(Counter, CounterResource(), name='counter')
try:
    with cc.connect(Counter, name='counter') as counter:
        print(counter.increment(3))  # 3
finally:
    cc.shutdown()
```

运行 `python counter.py`。计数器的 `value` 在多次调用之间保存在资源中。示例使用同进程调用；本地注册时保持 `C2_RELAY_ANCHOR_ADDRESS` 未设置。

同机的其他进程将资源进程的 `cc.server_address()` 传给客户端的 `address` 参数。两端的完整用法见 [IPC 示例](examples/python/ipc_resource.py)。资源进程还可以在注册后取得 `credential = cc.endpoint.server_credential()`，交给它的控制方供之后精确清理，见[生命周期指南](docs/reference/local-endpoint-lifecycle.md)。

`runtime = cc.Runtime()` 让应用分别拥有自己的 Python 配置、路由和原生会话。用 `runtime.set_*()` 配置后，可以调用 `runtime.start()` 等待空 Host 就绪，或用 `runtime.register()` 承载资源。模块函数继续使用默认 Runtime；`cc.serve()` 负责默认 Runtime 的进程信号处理，没有实例版。应用结束时调用 `runtime.shutdown()` 并检查结构化结果；若结果为 pending，再调用 shutdown 观察完成并换代。见[独立 Runtime 示例](examples/python/independent_runtime.py)及 [Python API 指南](docs/reference/python-usage.zh-CN.md#独立的-python-runtime)。

使用 relay 时，启动 `c3 relay --bind 0.0.0.0:8300`，在两端设置 `cc.set_relay_anchor('http://relay-host:8300')`，再按名称连接。Relay 独立运行；显式 IPC 地址直接连接端点。

## 示例与文档

| 内容 | 入口 |
| --- | --- |
| 同进程调用 | [local.py](examples/python/local.py) |
| 直连 IPC | [资源端](examples/python/ipc_resource.py)、[客户端](examples/python/ipc_client.py) |
| 独立 Python Runtime | [应用自有端点预检](examples/python/independent_runtime.py) |
| Relay | [资源端](examples/python/relay_resource.py)、[客户端](examples/python/relay_client.py)、[mesh](examples/python/relay_mesh/README.md) |
| 配置：代码、环境变量与 `.env` | [配置指南](docs/reference/configuration.zh-CN.md) |
| Python 契约、跨语言载荷与 API 索引 | [Python SDK 指南](docs/reference/python-usage.zh-CN.md) |
| Rust SDK 与 API 迁移 | [Rust SDK 指南](sdk/rust/README.md) |
| Relay 与契约工具 | [c3 CLI 指南](cli/README.md) |
| 连接与端点 API | [生命周期指南](docs/reference/local-endpoint-lifecycle.md) |
| Windows | [构建与使用](docs/reference/windows-native-usage.zh-CN.md) |
| 环境变量 | [.env.example](.env.example) |
| 开发 | [构建与测试](docs/reference/development.md)、[CI 与发行](docs/reference/ci-cd.md)、[贡献指南](CONTRIBUTING.md) |

## 许可证

[MIT](LICENSE)。
