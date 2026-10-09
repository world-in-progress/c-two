<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向有状态资源的 RPC 运行时。
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a>
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/Python-3.10%2B-blue" alt="Python 3.10+" /></a>
  <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" />
  <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

---

## C-Two 是什么？

C-Two 是面向分布式科学计算的资源 RPC 运行时。它把模拟器、空间索引和数据处理器等有状态对象暴露给其他进程和机器，客户端通过相同的方法接口调用本地或远程资源。

**CRM 契约**声明可调用的方法以及 namespace、version；**resource** 是实现这些方法并持有状态的普通类；**client** 通过 `cc.connect(...)` 获得类型化代理。一个进程可以同时承载多个资源，并连接其他资源。

本文介绍 **C-Two 0.7.4**，支持 Python 3.10+，依赖 FastDB 0.2.1。

## 特点

- **本地与远程调用**：同进程直接传递 Python 对象，同机 IPC 使用 Unix domain socket 或 Windows Named Pipe，跨机器使用 HTTP relay。显式 IPC 连接独立于 relay 发现。
- **状态与并发**：资源在调用之间保留状态。`@cc.read`、`@cc.write` 声明访问语义，默认采用写访问；原生路由调度统一约束本地和远程调用的并发与排队数量。
- **跨语言契约与载荷**：`@cc.transfer(...)` 绑定 FastDB `Payload` 规格。契约工具验证描述符并生成 Python、Rust、TypeScript 绑定；Python 之间也支持普通 Python 值。
- **按契约路由**：客户端匹配路由名称、CRM 身份和契约哈希。Relay 发现选择经过验证的本机 IPC 路径或 HTTP 目标；`c3` 独立运行 relay，并支持 relay mesh。
- **原生内存传输**：IPC 使用按需 buddy 池、独占共享内存、检查过的分块传输及文件溢出。通过预算和空闲回收控制大载荷的内存与文件用量。详见[内存策略](docs/memory-policy.md)。
- **明确的载荷生命周期**：`cc.hold()` 保留响应租约，资源注册可选择借用输入；释放时先使 FastDB owner 和 checked view 失效，再释放传输存储。详见 [Python 载荷用法](docs/python-usage.md)。
- **资源生命周期管理**：资源采用 `Persistent` 或控制器持有的 `OwnerBound` 生命周期；原生 shutdown 排空工作，限定范围的端点维护清理已确认退出的端点。详见[生命周期指南](docs/local-endpoint-lifecycle.md)。
- **统一 Rust core**：Python 与 Rust 共用路由、传输、内存、错误和生命周期机制。原生 c3 CLI 在 Linux、macOS、Windows x64 上提供 relay、契约工具和端点维护。

## 安装

```bash
uv pip install c-two
```

Windows PowerShell 使用相同命令。Linux、macOS 的 c3 安装命令：

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-installer.sh | sh -s -- --version 0.3.3
```

Windows x64 使用[可执行文件](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-x86_64-pc-windows-msvc.exe)或 [PowerShell 安装器](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-installer.ps1)。安装与校验步骤见 [Windows 指南](docs/windows-native-usage.md#install-python-and-c3)。

源码构建与测试见[开发指南](docs/development.md)。

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

运行 `python counter.py`。示例使用同进程调用；本地注册时保持 `C2_RELAY_ANCHOR_ADDRESS` 未设置。

同机的其他进程将资源进程的 `cc.server_address()` 传给客户端的 `address` 参数。两端的完整用法见 [IPC 示例](examples/python/ipc_resource.py)。

使用 relay 时，启动 `c3 relay --bind 0.0.0.0:8300`，在两端设置 `cc.set_relay_anchor('http://relay-host:8300')`，再按名称连接。Relay 独立运行；显式 IPC 地址直接连接端点。

## 示例与文档

| 内容 | 入口 |
| --- | --- |
| 同进程调用 | [local.py](examples/python/local.py) |
| 直连 IPC | [资源端](examples/python/ipc_resource.py)、[客户端](examples/python/ipc_client.py) |
| Relay | [资源端](examples/python/relay_resource.py)、[客户端](examples/python/relay_client.py)、[mesh](examples/python/relay_mesh/README.md) |
| 配置：代码、环境变量与 `.env` | [配置指南](docs/configuration.md) |
| Python 契约、跨语言载荷与 hold | [Python SDK 指南](docs/python-usage.md) |
| Rust SDK | [Rust SDK 指南](sdk/rust/README.md) |
| Relay 与契约工具 | [c3 CLI 指南](cli/README.md) |
| 端点目录与生命周期 | [生命周期指南](docs/local-endpoint-lifecycle.md) |
| Windows | [构建与使用](docs/windows-native-usage.md) |
| 环境变量 | [.env.example](.env.example) |
| 开发 | [构建与测试](docs/development.md)、[贡献指南](CONTRIBUTING.md) |

## 许可证

[MIT](LICENSE)。
