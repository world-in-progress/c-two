<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向资源的 Python RPC 框架 — 将有状态的 Python 类转变为位置透明的分布式资源。
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a>
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/dm/c-two" alt="Downloads" /></a>
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/python-%E2%89%A53.10-blue" alt="Python" /></a>
  <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" />
  <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

---

## 基本理念

- **面向资源，而非服务** — C-Two 不是暴露 RPC 端点，而是让 Python 类在保持有状态、面向对象特性的同时可被远程访问。

- **从进程到数据的零拷贝** — 同进程调用完全跳过序列化。跨进程 IPC 可以保持共享内存缓冲区存活，让你直接从 SHM 读取列式数据（NumPy、Arrow 等）— 无需反序列化，无需拷贝。

- **为科学计算而生** — 原生支持 Apache Arrow、NumPy 数组和大体量载荷（超过 256 MB 自动分块流式传输）。专为计算密集型场景设计，而非微服务。

- **Rust 驱动的传输层** — IPC 层使用 Rust buddy 分配器管理共享内存，HTTP 中继基于 Rust 实现高吞吐网络转发。

---

## 性能

端到端跨进程 IPC 基准测试 — 相同的 NumPy 载荷（`row_id u32` + `x,y,z f64`），相同机器，相同聚合运算。三种传输模式对比：

| 行数 | C-Two hold (ms) | Ray (ms) | C-Two pickle (ms) | **Hold vs Ray** |
|-----:|---:|---:|---:|---:|
| 1 K | **0.07** | 6.1 | 0.19 | **86×** |
| 10 K | **0.09** | 7.1 | 0.82 | **79×** |
| 100 K | **0.38** | 9.8 | 8.7 | **26×** |
| 1 M | **3.7** | 58 | 150 | **15×** |
| 3 M | **9.7** | 129 | 598 | **13×** |

- **C-Two hold** — SHM 零拷贝，通过 `np.frombuffer` 读取；读端无序列化
- **Ray** — 对象存储，内置 numpy 零拷贝支持（Ray 2.55）
- **C-Two pickle** — 标准 pickle 经由 SHM 传输；展示序列化开销

> Apple M1 Max · Python 3.13 · NumPy 2.4 · 完整方法见 [`benchmarks/unified_numpy_benchmark.py`](benchmarks/unified_numpy_benchmark.py)。

---

## 快速开始

```bash
pip install c-two
```

### 定义资源契约及其实现

```python
import c_two as cc

# CRM 契约 — 声明哪些方法可被远程访问
@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...

    def reset(self) -> int: ...


# Resource — 一个实现契约的普通 Python 类
class CounterImpl:
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
cc.register(Counter, CounterImpl(initial=100), name='counter')
counter = cc.connect(Counter, name='counter')

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
cc.register(Counter, CounterImpl(), name='counter')

# 客户端进程（另一个终端）
counter = cc.connect(Counter, name='counter', address='ipc:///tmp/my_server')
counter.increment(5)     # 用法完全一致
cc.close(counter)
```

> **完整可运行示例请参阅 [`examples/`](examples/) 目录。**

---

## 核心概念

### CRM — 契约

**CRM**（Core Resource Model）声明远程资源暴露*哪些*方法。使用 `@cc.crm()` 装饰，方法体为 `...`（纯接口，无实现）。

```python
@cc.crm(namespace='demo.greeter', version='0.1.0')
class Greeter:
    @cc.read    # 允许并发读取
    def greet(self, name: str) -> str: ...

    @cc.read
    def language(self) -> str: ...
```

方法可标注 `@cc.read`（允许并发访问）或保持默认的 write（独占访问）。

### Resource — 运行时实例

**Resource** 是实现 CRM 契约的普通 Python 类，持有状态和领域逻辑。它**不需要**任何装饰器 — 框架通过所注册的 CRM 契约发现其方法。命名应体现 *它是什么*（`GreeterImpl`、`PostgresGreeter`、`MultilingualGreeter`），而非接口本身。

```python
class GreeterImpl:
    def __init__(self, lang: str = 'en'):
        self._lang = lang
        self._templates = {'en': 'Hello, {}!', 'zh': '你好, {}!'}

    def greet(self, name: str) -> str:
        return self._templates.get(self._lang, 'Hi, {}!').format(name)

    def language(self) -> str:
        return self._lang
```

### Client — 消费者

任何调用 `cc.connect(...)` 的代码都是 **client**（即 consumer / 应用代码）。返回的代理是位置透明的 — 无论资源运行在同进程还是远程机器，用法完全相同。

```python
greeter = cc.connect(Greeter, name='greeter')
greeter.greet('World')     # → '你好, World!'
cc.close(greeter)

# 或使用上下文管理器：
with cc.connect(Greeter, name='greeter') as greeter:
    greeter.greet('World')
```

### Server — 资源宿主

**Server** 是调用 `cc.register(...)` 托管一个或多个资源、并通常调用 `cc.serve()` 进入请求循环的进程。单个 server 进程可以托管任意多的资源（每个使用唯一的 `name`），并在首次注册时自动绑定一个 IPC 端点。

```python
import c_two as cc

cc.set_address('ipc://my_server')              # 可选 — 默认是自动生成的 UUID 路径
cc.register(Greeter, GreeterImpl(), name='greeter')
cc.register(Counter, CounterImpl(), name='counter')
cc.serve()                                     # 阻塞；Ctrl-C 触发优雅关闭
```

- **地址**（`ipc://...`）是本地传输端点。*同进程*内的客户端完全跳过它（零序列化）；*同主机上的其他进程*通过此地址直接连接。
- **`cc.serve()` 是可选的** — 如果宿主进程已有自己的事件循环（Web 服务、GUI、模拟器），你可以只注册资源，让它们在后台服务，同时你的主循环照常运行。
- 一个进程可以**同时**是 server 和 client（注册一些资源，同时连接另一些）。

### Relay — 分布式发现

**HTTP 中继**（`c3 relay`）是一个轻量级代理，让客户端可以**按名称**跨机器访问服务端。服务端在注册时把自己的 IPC 地址通告给中继；客户端只需查询中继即可被透明路由到目标。

```bash
# 在网络可达的任意节点启动中继
c3 relay --bind 0.0.0.0:8080
```

```python
# 服务端 — 将资源通告给中继
cc.set_relay('http://relay-host:8080')
cc.register(MeshStore, MeshStoreImpl(), name='mesh')
cc.serve()

# 客户端 — 按名称解析，无需显式地址
cc.set_relay('http://relay-host:8080')
mesh = cc.connect(MeshStore, name='mesh')
```

多个中继可以通过 gossip 协议组成**网格集群** — 网格中任意中继都能解析整个集群内注册的任何资源。详见下方的 [中继网格示例](#中继网格--多中继集群)。

> **何时需要中继？** 仅用于跨机器或按名称发现的场景。同进程和同主机（IPC）用法完全不需要中继。

### @transferable — 自定义序列化

对于需要跨进程传输的自定义数据类型，使用 `@cc.transferable`。未使用此装饰器时，默认使用 pickle。

一个 transferable 类最多可定义三个静态方法（以普通方法风格编写，无需添加 `@staticmethod` — 框架会自动转换）：

| 方法 | 必需 | 用途 |
|------|------|------|
| `serialize(data) → bytes` | ✅ 是 | 将数据编码为字节用于传输（出站） |
| `deserialize(raw) → T` | ✅ 是 | 将字节解码为拥有所有权的 Python 对象（入站） |
| `from_buffer(buf) → T` | ❌ 可选 | 在原始缓冲区上构建零拷贝视图（入站，持有模式） |

```python
import numpy as np

@cc.transferable
class Matrix:
    rows: int
    cols: int
    data: np.ndarray

    def serialize(mat: 'Matrix') -> bytes:
        header = struct.pack('>II', mat.rows, mat.cols)
        return header + mat.data.tobytes()

    def deserialize(raw: bytes) -> 'Matrix':
        rows, cols = struct.unpack_from('>II', raw)
        arr = np.frombuffer(raw, dtype=np.float64, offset=8).reshape(rows, cols)
        return Matrix(rows=rows, cols=cols, data=arr.copy())  # owned copy

    def from_buffer(buf: memoryview) -> 'Matrix':
        header = bytes(buf[:8])
        rows, cols = struct.unpack('>II', header)
        arr = np.frombuffer(buf[8:], dtype=np.float64).reshape(rows, cols)
        return Matrix(rows=rows, cols=cols, data=arr)  # zero-copy view into SHM
```

当 `from_buffer` 存在时，服务端自动使用**持有模式** — 共享内存缓冲区保持存活，使 `from_buffer` 可以返回零拷贝视图。若无 `from_buffer`，服务端使用**视图模式** — 缓冲区在 `deserialize` 完成后立即释放。

### @cc.transfer — 逐方法控制

在 CRM 契约方法上使用 `@cc.transfer()` 可显式指定由哪个 transferable 类型处理序列化，或覆盖缓冲区模式：

```python
@cc.crm(namespace='demo.compute', version='0.1.0')
class Compute:
    @cc.transfer(input=Matrix, output=Matrix, buffer='hold')
    def transform(self, mat: Matrix) -> Matrix: ...

    @cc.transfer(input=Matrix, buffer='view')  # force copy even if from_buffer exists
    def ingest(self, mat: Matrix) -> None: ...
```

若不使用 `@cc.transfer`，框架会根据函数签名自动匹配已注册的 `@transferable` 类型，并根据输入类型的能力自动解析缓冲区模式。

### cc.hold() — 客户端零拷贝

在客户端，`cc.hold()` 请求响应的共享内存缓冲区保持存活，实现结果的零拷贝读取。返回的 `HeldResult` 包装了值，并提供三层共享内存生命周期安全网：

1. **显式 `.release()`** — 推荐用于同时持有多个缓冲区的复杂工作流
2. **上下文管理器（`with`）** — 推荐用于单缓冲区作用域
3. **`__del__` 兜底** — 最后手段，若忘记释放会触发 `ResourceWarning`

```python
grid = cc.connect(Compute, name='compute', address='ipc://server')

# Normal call — buffer released immediately after deserialize
result = grid.transform(matrix)

# Option 1: Context manager — clean for single holds
with cc.hold(grid.transform)(matrix) as held:
    data = held.value          # zero-copy NumPy array backed by SHM
    process(data)              # read directly from shared memory
# SHM buffer released on context exit

# Option 2: Explicit release — better for multiple concurrent holds
a = cc.hold(grid.transform)(matrix_a)
b = cc.hold(grid.transform)(matrix_b)
try:
    combined = np.concatenate([a.value.data, b.value.data])
    process(combined)
finally:
    a.release()
    b.release()
```

> **何时使用持有模式：** 适用于反序列化开销占主导的大型数组/列式数据。对于小载荷（< 1 MB），跟踪共享内存生命周期的开销超过拷贝成本。

---

## 使用示例

### 单进程 — 线程偏好模式

当 `cc.connect()` 目标是同进程中注册的 CRM 时，代理直接调用方法，**零序列化开销**。

```python
import c_two as cc

cc.register(Greeter, Greeter(lang='en'), name='greeter')
cc.register(Counter, Counter(initial=100), name='counter')

greeter = cc.connect(Greeter, name='greeter')
counter = cc.connect(Counter, name='counter')

print(greeter.greet('World'))    # → Hello, World!
print(counter.value())           # → 100
counter.increment(10)

cc.close(greeter)
cc.close(counter)
cc.shutdown()
```

> **适用场景：** 本地原型开发、测试、单机计算。

### 多进程 — IPC 与自定义 Transferable

独立的服务端和客户端进程，通过 Unix 域套接字 + 共享内存通信。

**共享类型** (`types.py`)：
```python
import c_two as cc
import numpy as np, struct

@cc.transferable
class Mesh:
    n_vertices: int
    positions: np.ndarray   # (N, 3) float64

    def serialize(mesh: 'Mesh') -> bytes:
        header = struct.pack('>I', mesh.n_vertices)
        return header + mesh.positions.tobytes()

    def deserialize(raw: bytes) -> 'Mesh':
        (n,) = struct.unpack_from('>I', raw)
        arr = np.frombuffer(raw, dtype=np.float64, offset=4).reshape(n, 3).copy()
        return Mesh(n_vertices=n, positions=arr)

    def from_buffer(buf: memoryview) -> 'Mesh':
        header = bytes(buf[:4])
        (n,) = struct.unpack('>I', header)
        arr = np.frombuffer(buf[4:], dtype=np.float64).reshape(n, 3)
        return Mesh(n_vertices=n, positions=arr)  # zero-copy view

@cc.crm(namespace='demo.mesh', version='0.1.0')
class MeshStore:
    @cc.read
    def get_mesh(self) -> Mesh: ...

    def update_positions(self, mesh: Mesh) -> int: ...

    @cc.on_shutdown
    def cleanup(self) -> None: ...
```

**服务端** (`server.py`)：
```python
import c_two as cc
from types import MeshStore, Mesh

class MeshStore:
    def __init__(self):
        self._mesh = Mesh(n_vertices=0, positions=np.empty((0, 3)))

    def get_mesh(self) -> Mesh:
        return self._mesh

    def update_positions(self, mesh: Mesh) -> int:
        self._mesh = mesh
        return mesh.n_vertices

    def cleanup(self):
        print('MeshStore shutting down')

cc.set_address('ipc://mesh_server')
cc.register(MeshStore, MeshStore(), name='mesh')
cc.serve()  # blocks until interrupted
```

**客户端** (`client.py`)：
```python
import c_two as cc
from types import MeshStore, Mesh
import numpy as np

mesh_store = cc.connect(MeshStore, name='mesh', address='ipc://mesh_server')

# Upload data
big_mesh = Mesh(n_vertices=1_000_000,
                positions=np.random.randn(1_000_000, 3))
mesh_store.update_positions(big_mesh)

# Read with hold — zero-copy SHM access
with cc.hold(mesh_store.get_mesh)() as held:
    positions = held.value.positions  # np.ndarray backed by SHM, no copy
    centroid = positions.mean(axis=0)
    print(f'Centroid: {centroid}')
# SHM released here

cc.close(mesh_store)
```

> **适用场景：** 同主机多进程、Worker 隔离、高吞吐本地 IPC。

### 跨机器 — HTTP 中继

HTTP 中继将网络请求桥接到运行在 IPC 上的 CRM 进程。CRM 进程向中继注册，客户端通过**名称**发现资源。

**CRM 服务端** (`resource.py`)：
```python
import c_two as cc

cc.set_relay('http://relay-host:8080')
cc.set_address('ipc://mesh_server')
cc.register(MeshStore, MeshStore(), name='mesh')
cc.serve()  # 阻塞，直到 Ctrl-C
```

**中继** — 通过 `c3` CLI 启动：
```bash
# 绑定地址可通过 CLI 参数、环境变量 C2_RELAY_BIND 或 .env 文件配置
c3 relay --bind 0.0.0.0:8080
```

**客户端** (`client.py`)：
```python
import c_two as cc

cc.set_relay('http://relay-host:8080')
mesh = cc.connect(MeshStore, name='mesh')  # 中继解析名称
mesh.get_mesh()
cc.close(mesh)
```

> **适用场景：** 网络可访问的服务、Web 集成、跨机器部署。

### 中继网格 — 多中继集群

多个中继组成**网格网络**，通过 gossip 协议自动传播路由。CRM 向本地中继注册，客户端可跨整个集群发现资源。

```bash
# 启动中继 A（seeds 指向对等中继实现自动加入）
c3 relay --bind 0.0.0.0:8080 --relay-id relay-a \
    --advertise-url http://relay-a:8080 --seeds http://relay-b:8080

# 启动中继 B
c3 relay --bind 0.0.0.0:8080 --relay-id relay-b \
    --advertise-url http://relay-b:8080 --seeds http://relay-a:8080
```

CRM 进程向本地中继注册，网格自动传播路由。客户端可通过网格中的**任意**中继连接。

> **适用场景：** 多节点集群、高可用、地理分布部署。

> **完整可运行的网格示例请参阅 [`examples/relay_mesh/`](examples/relay_mesh/) 目录。**

### 服务端监控

使用 `cc.hold_stats()` 监控持有模式下资源方法所持有的共享内存缓冲区：

```python
stats = cc.hold_stats()
# {'active_holds': 3, 'total_held_bytes': 52428800, 'oldest_hold_seconds': 12.5}
```

---

## 架构

**C-Two 的设计哲学不是定义服务，而是赋能资源。**

在科学计算中，封装复杂状态和领域特定操作的资源需要被组织为内聚的单元。我们称描述这些资源的契约为 **核心资源模型（CRM）**。应用程序更关心的是 *如何与资源交互*，而非 *资源在哪里*。C-Two 提供位置透明和统一的资源访问，使任何 **client** 都能像访问本地对象一样与资源交互。

```mermaid
graph LR
    subgraph 客户端层
        C1[Client] -->|cc.connect| P1[CRM 代理]
        C2[Client] -->|cc.connect| P2[CRM 代理]
    end

    subgraph 传输层
        P1 --> T{协议<br/>自动检测}
        P2 --> T
        T -->|thread://| TH[线程<br/>直接调用]
        T -->|ipc://| IPC[IPC<br/>UDS + SHM]
        T -->|http://| HTTP[HTTP<br/>中继]
    end

    subgraph 资源层
        TH --> CRM1[Resource 实例]
        IPC --> CRM1
        HTTP --> CRM1
    end
```

### 客户端层

调用 `cc.connect(...)` 消费资源的任何代码。返回的代理提供完整的类型安全和位置透明性 — 客户端不需要知道（也不关心）资源运行在哪里。

- `cc.connect(CRMClass, name='...', address='...')` 返回类型化的 CRM 代理
- 代理支持上下文管理：`with cc.connect(...) as x:` 自动关闭

### 资源层

服务端有状态的实例，通过标准化的 CRM 契约暴露。

- **CRM 契约**：使用 `@cc.crm()` 装饰的接口类。只有在此声明的方法才可被远程访问。
- **Resource**：实现契约的普通 Python 类 — 状态 + 领域逻辑，无需装饰器。
- **`@transferable`**：领域数据类型的自定义序列化。可选提供 `from_buffer` 实现零拷贝共享内存视图。
- **`@cc.transfer`**：逐方法控制输入/输出 transferable 类型和缓冲区模式。
- **`@cc.read` / `@cc.write`**：并发注解 — 并行读取，独占写入。
- **`@cc.on_shutdown`**：生命周期回调，在资源被注销时调用（不通过 RPC 暴露）。

### 传输层

协议无关的通信，基于地址方案自动检测协议：

| 协议方案 | 传输方式 | 适用场景 |
|----------|----------|----------|
| `thread://` | 进程内直接调用 | 零序列化、测试 |
| `ipc:///path` | Unix 域套接字 + 共享内存 | 多进程、同主机 |
| `http://host:port` | HTTP 中继 | 跨机器、Web 兼容 |

IPC 传输采用 **控制面 / 数据面分离**：方法路由通过 UDS 内联帧传输，载荷字节通过共享内存交换 — 数据路径上零拷贝。当 `from_buffer` 可用时，**持有模式**会在 CRM 方法调用期间保持共享内存缓冲区存活，使 CRM 可以直接操作共享内存中的数据而无需反序列化。

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
cp .env.example .env               # 配置环境变量（可选）
uv sync                            # 安装依赖 + 编译 Rust 扩展
uv sync --group examples           # 安装示例依赖（pandas、pyarrow）
uv run pytest                      # 运行测试套件
```

> 需要 [uv](https://github.com/astral-sh/uv) 和 Rust 工具链。

---

## 路线图

| 功能 | 状态 |
|------|------|
| 核心 RPC 框架（CRM + Resource + Client） | ✅ 稳定 |
| IPC 传输 + SHM 伙伴分配器 | ✅ 稳定 |
| HTTP 中继（Rust 驱动） | ✅ 稳定 |
| 中继网格与 gossip 路由发现 | ✅ 稳定 |
| 分块流式传输（载荷 > 256 MB） | ✅ 稳定 |
| 心跳与连接管理 | ✅ 稳定 |
| 读/写并发控制 | ✅ 稳定 |
| 统一配置架构（Python 单一事实源） | ✅ 稳定 |
| CI/CD 与多平台 PyPI 发布 | ✅ 稳定 |
| 极端载荷磁盘溢出 | ✅ 稳定 |
| 持有模式与 `from_buffer` 零拷贝 | ✅ 稳定 |
| 共享内存驻留监控（`cc.hold_stats()`） | ✅ 稳定 |
| 异步接口 | 🔜 规划中 |
| 跨语言客户端（TypeScript/Rust） | 🔮 远期 |

详见[完整路线图](docs/plans/c-two-rpc-v2-roadmap.md)。

---

## 开源协议

[MIT](LICENSE)

---

<p align="center">为科学计算而生，由 Rust 驱动。</p>
