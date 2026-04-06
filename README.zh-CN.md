<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向资源的 Python RPC 框架 — 将有状态的 Python 类转变为位置透明的分布式资源。
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a>
  <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" />
  <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

---

## 基本理念

- **面向资源，而非服务** — C-Two 不是暴露 RPC 端点，而是让 Python 类在保持有状态、面向对象特性的同时可被远程访问。

- **本地零拷贝，远程透明** — 同进程调用完全跳过序列化。跨进程调用使用共享内存。跨机器调用走 HTTP。你的代码无需修改。

- **为科学计算而生** — 原生支持 Apache Arrow、NumPy 数组和大体量载荷（超过 256 MB 自动分块流式传输）。专为计算密集型场景设计，而非微服务。

- **Rust 驱动的传输层** — IPC 层使用 Rust buddy 分配器管理共享内存，HTTP 中继基于 Rust 实现高吞吐网络转发。

---

## 快速开始

```bash
pip install c-two
```

### 定义资源接口及其实现

```python
import c_two as cc

# 接口 — 声明哪些方法可被远程访问
@cc.icrm(namespace='demo.counter', version='0.1.0')
class ICounter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...

    def reset(self) -> int: ...


# 实现 — 一个持有状态的普通 Python 类
class Counter:
    def __init__(self, initial: int = 0):
        self._value = initial

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def value(self) -> int:
        return self._value

    def reset(self) -> int:
        old = self._value
        self._value = 0
        return old
```

### 本地使用（零序列化）

```python
cc.register(ICounter, Counter(initial=100), name='counter')
counter = cc.connect(ICounter, name='counter')

counter.increment(10)    # → 110
counter.value()          # → 110
counter.reset()          # → 110（返回旧值）
counter.value()          # → 0

cc.close(counter)
```

### 远程使用 — 相同 API，不同地址

```python
# 服务端进程
cc.set_address('ipc:///tmp/my_server')
cc.register(ICounter, Counter(), name='counter')

# 客户端进程（另一个终端）
counter = cc.connect(ICounter, name='counter', address='ipc:///tmp/my_server')
counter.increment(5)     # 用法完全一致
cc.close(counter)
```

> **完整可运行示例请参阅 [`examples/`](examples/) 目录。**

---

## 核心概念

### ICRM — 接口

ICRM（Interface of Core Resource Model）声明了哪些 CRM 方法可被远程访问。使用 `@cc.icrm()` 装饰，方法体为 `...`（纯接口，无实现）。

```python
@cc.icrm(namespace='demo.greeter', version='0.1.0')
class IGreeter:
    @cc.read    # 允许并发读取
    def greet(self, name: str) -> str: ...

    @cc.read
    def language(self) -> str: ...
```

方法可标注 `@cc.read`（允许并发访问）或保持默认的 write（独占访问）。

### CRM — 核心资源模型

CRM 是一个普通的 Python 类，持有状态并实现领域逻辑。它**不需要**任何装饰器 — 框架通过 ICRM 接口发现其方法。

```python
class Greeter:
    def __init__(self, lang: str = 'en'):
        self._lang = lang
        self._templates = {'en': 'Hello, {}!', 'zh': '你好, {}!'}

    def greet(self, name: str) -> str:
        return self._templates.get(self._lang, 'Hi, {}!').format(name)

    def language(self) -> str:
        return self._lang
```

### Component — 消费者

Component 通过 ICRM 代理消费 CRM 资源。代理是位置透明的 — 无论 CRM 运行在同进程还是远程机器，用法完全相同。

```python
greeter = cc.connect(IGreeter, name='greeter')
greeter.greet('World')     # → '你好, World!'
cc.close(greeter)
```

### @transferable — 自定义序列化

对于需要跨进程传输的自定义数据类型，使用 `@cc.transferable`。未使用此装饰器时，默认使用 pickle。

```python
@cc.transferable
class GridAttribute:
    level: int
    global_id: int
    elevation: float

    def serialize(data: 'GridAttribute') -> bytes:
        # 自定义序列化（如 Apache Arrow）
        ...

    def deserialize(raw: bytes) -> 'GridAttribute':
        ...
```

> `serialize` 和 `deserialize` 以实例方法风格编写，但框架会自动将它们转换为 `@staticmethod`。

---

## 使用示例

### 单进程 — 线程偏好模式

当 `cc.connect()` 目标是同进程中注册的 CRM 时，代理直接调用方法，**零序列化开销**。

```python
import c_two as cc

# 在同一进程中注册两个 CRM
cc.register(IGreeter, Greeter(lang='zh'), name='greeter')
cc.register(ICounter, Counter(initial=100), name='counter')

# 连接 — 零序列化的线程本地代理
greeter = cc.connect(IGreeter, name='greeter')
counter = cc.connect(ICounter, name='counter')

print(greeter.greet('World'))    # → 你好, World!
print(counter.value())           # → 100
counter.increment(10)

# 清理
cc.close(greeter)
cc.close(counter)
cc.shutdown()
```

> **适用场景：** 本地原型开发、测试、单机计算。

### 多进程 — IPC

独立的服务端和客户端进程，通过 Unix 域套接字 + 共享内存通信。

**服务端** (`server.py`)：
```python
import c_two as cc

cc.set_address('ipc://my_server')
cc.register(IGrid, grid_instance, name='grid')
print(f'监听地址: {cc.server_address()}')

cc.serve()  # 阻塞直到中断
```

**客户端** (`client.py`)：
```python
import c_two as cc

grid = cc.connect(IGrid, name='grid', address='ipc://my_server')
infos = grid.get_grid_infos(1, [0, 1, 2])
keys = grid.subdivide_grids([1], [0])

cc.close(grid)
cc.shutdown()
```

> **适用场景：** 同主机多进程、Worker 隔离、高吞吐本地 IPC。

### 跨机器 — HTTP 中继

HTTP 中继将网络请求桥接到运行在 IPC 上的 CRM 进程。

**CRM 服务端** (`resource.py`)：
```python
import c_two as cc

cc.set_address('ipc://grid_server')
cc.register(IGrid, grid_instance, name='grid')
# 设置 C2_RELAY_ADDRESS 环境变量后自动注册到中继
cc.serve()
```

**中继** (`relay.py`)：
```python
from c_two.relay import NativeRelay

relay = NativeRelay('127.0.0.1:8080')
relay.start()
relay.register_upstream('grid', 'ipc://grid_server')
```

**客户端** (`client.py`)：
```python
import c_two as cc

# 相同 API — 只需更改地址
grid = cc.connect(IGrid, name='grid', address='http://127.0.0.1:8080')
grid.get_grid_infos(1, [0])

cc.close(grid)
```

> **适用场景：** 网络可访问的服务、Web 集成、跨机器部署。

---

## 架构

**C-Two 的设计哲学不是定义服务，而是赋能资源。**

在科学计算中，封装复杂状态和领域特定操作的资源需要被组织为内聚的单元。我们称这些为 **核心资源模型（CRM）**。应用程序更关心的是 *如何与资源交互*，而非 *资源在哪里*。我们称资源消费者为 **组件（Component）**。C-Two 提供位置透明和统一的资源访问，使组件能够像访问本地对象一样与 CRM 交互。

```mermaid
graph LR
    subgraph 组件层
        C1[Component] -->|cc.connect| P1[ICRM 代理]
        C2[Component] -->|cc.connect| P2[ICRM 代理]
    end

    subgraph 传输层
        P1 --> T{协议<br/>自动检测}
        P2 --> T
        T -->|thread://| TH[线程<br/>直接调用]
        T -->|ipc://| IPC[IPC<br/>UDS + SHM]
        T -->|http://| HTTP[HTTP<br/>中继]
    end

    subgraph CRM 层
        TH --> CRM1[CRM 实例]
        IPC --> CRM1
        HTTP --> CRM1
    end
```

### 组件层

客户端消费者通过 ICRM 代理访问远程资源。代理提供完整的类型安全和位置透明性 — 组件不需要知道（也不关心）CRM 运行在哪里。

- **脚本式**：`cc.connect(ICRMClass, name='...', address='...')` 返回类型化的 ICRM 代理
- **函数式**：`@cc.runtime.connect` 装饰器自动注入 ICRM 代理作为第一个参数

### CRM 层

服务端有状态资源，通过标准化的 ICRM 接口暴露。

- **CRM**：普通 Python 类 — 状态 + 领域逻辑，无需装饰器。
- **ICRM**：使用 `@cc.icrm()` 装饰的接口类。只有在此声明的方法才可被远程访问。
- **`@transferable`**：领域数据类型的自定义序列化（如 Apache Arrow）。
- **`@cc.read` / `@cc.write`**：并发注解 — 并行读取，独占写入。

### 传输层

协议无关的通信，基于地址方案自动检测协议：

| 协议方案 | 传输方式 | 适用场景 |
|----------|----------|----------|
| `thread://` | 进程内直接调用 | 零序列化、测试 |
| `ipc:///path` | Unix 域套接字 + 共享内存 | 多进程、同主机 |
| `http://host:port` | HTTP 中继 | 跨机器、Web 兼容 |

IPC 传输采用 **控制面 / 数据面分离**：方法路由通过 UDS 内联帧传输，载荷字节通过共享内存交换 — 数据路径上零拷贝。

### Rust 原生层

性能关键组件使用 Rust 实现，通过 [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) 编译为 Python 扩展：

Rust 工作空间包含 7 个 crate，按 4 层架构组织（基础层 → 协议层 → 传输层 → 桥接层）：

- **伙伴分配器** — IPC 传输的零系统调用共享内存分配。跨进程，快速路径上无锁。
- **线协议** — 帧编码、分块组装和分块注册表，管理大载荷的生命周期。
- **HTTP 中继** — 基于 [axum](https://github.com/tokio-rs/axum) 的高吞吐网关，桥接 HTTP 到 IPC。处理连接池和请求多路复用。

Rust 扩展在 `pip install c-two`（从预编译 wheel）或 `uv sync`（从源码）时自动编译。

---

## 安装

### 从 PyPI 安装

```bash
pip install c-two
```

预编译 wheel 支持：
- **Linux**：x86_64、aarch64
- **macOS**：Apple Silicon (aarch64)、Intel (x86_64)
- **Python**：3.10、3.11、3.12、3.13、3.14、3.14t（自由线程）

如果没有适合你平台的预编译 wheel，pip 将从源码编译（需要 [Rust 工具链](https://rustup.rs)）。

### 开发环境

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
uv sync          # 安装依赖 + 编译 Rust 扩展
uv run pytest    # 运行测试套件
```

> 需要 [uv](https://github.com/astral-sh/uv) 和 Rust 工具链。

---

## 路线图

| 功能 | 状态 |
|------|------|
| 核心 RPC 框架（CRM / ICRM / Component） | ✅ 稳定 |
| IPC 传输 + SHM 伙伴分配器 | ✅ 稳定 |
| HTTP 中继（Rust 驱动） | ✅ 稳定 |
| 分块流式传输（载荷 > 256 MB） | ✅ 稳定 |
| 心跳与连接管理 | ✅ 稳定 |
| 读/写并发控制 | ✅ 稳定 |
| 统一配置架构（Python 单一事实源） | ✅ 稳定 |
| CI/CD 与多平台 PyPI 发布 | ✅ 稳定 |
| 异步接口 | 🔜 规划中 |
| 极端载荷磁盘溢出 | ✅ 稳定 |
| 跨语言客户端（Rust/C++） | 🔮 远期 |

详见[完整路线图](docs/plans/c-two-rpc-v2-roadmap.md)。

---

## 开源协议

[MIT](LICENSE)

---

<p align="center">为科学计算而生，由 Rust 驱动。</p>
