<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向资源的 RPC runtime — 将有状态对象转变为位置透明的分布式资源。
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

## 基本理念

- **面向资源的 RPC** — C-Two 通过语言 SDK 暴露有状态 resource object。Python SDK 让 Python 类在保持面向对象特性的同时具备远程访问能力。

- **显式的 transport 与 payload 生命周期** — 同进程调用跳过序列化。跨进程 IPC 可以使用共享内存承载字节；当前 portable FastDB 接收路径会打开 owned copy，并通过 checked owner/view 执行失效控制，不声称已直接构造到最终 response backing。

- **为科学计算而生** — portable CRM payload 使用 FastDB；Python-only prototype 仍可使用普通 Python 值。超过 256 MB 的大体量载荷使用分块传输。runtime 面向计算工作流和有状态科学资源设计。

- **Rust 驱动的 runtime core** — 共享 transport、memory、wire codec、route-contract validation、relay 和配置解析都在 Rust 中实现，让后续 SDK 复用同一套 runtime contract。

---

## 性能

Portable-payload 地基目前已经具备 correctness、deterministic codegen、生命周期与真实 Rust/Python 互操作证明，但尚未完成针对显式 `Payload` API、retained owner 或 direct/staged backing 的审阅后吞吐基准。

仓库中的 Kostya benchmark 当前只保留真实的 Python-only `pickle-records` 和 `pickle-arrays` baseline。旧 annotation inference 集成产生的结果属于历史证据，不能作为当前架构的性能声明。可复现的 portable-payload benchmark 仍是明确的[延后能力](docs/issues/contract-release-deferred-capabilities.md)，其 closure 必须准确记录 copy/direct/staged、环境与统计口径。

---

## 快速开始

> **开发构建要求：** 当前代码使用 FastDB 0.2.0。示例需要本 C-Two checkout 或配套开发构建；已发布的 C-Two 包尚未包含完整 portable-payload 与 Windows 集成。请参阅[开发环境](#开发环境)、[Windows 构建与使用说明](docs/windows-native-usage.md)和[历史候选包报告](docs/reports/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md)。

### 定义显式 portable-payload 契约

```python
import json

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload


VALUE_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u8", "nullable": False},
        },
    ],
    "components": [],
}


@cc.crm(namespace="demo.payload", version="0.1.0")
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload:
        ...

    def ping(self) -> None:
        ...
```

`c-two.contract.v2` 把 `VALUE_SPEC` 作为 opaque nested JSON value 包裹起来。C-Two 拥有外层 method/binding 契约，并将 nested value 原样委托给 FastDB Core；后者拥有校验、canonical identity、binary/runtime 行为和 payload-only codegen。

### 构造并使用 payload

```python
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

    def ping(self) -> None:
        return None


cc.register(Echo, EchoResource(), name="echo")
source = build_value(7)
try:
    with cc.connect(Echo, name="echo") as echo:
        result = echo.echo(source)
        result.close()
        assert echo.ping() is None
finally:
    source.close()
    cc.shutdown()
```

这个最小同进程示例假定没有配置 relay anchor。如果 checkout 的 `.env`
配置了 anchor 而 relay 并未运行，请用 `C2_RELAY_ANCHOR_ADDRESS=` 启动
脚本，使 registration 保持本地。

同一个 CRM 契约可用于进程内、IPC 或 relay。没有显式 portable binding 的普通 Python 方法仍可用 pickle 做 Python-only prototype，但 portable export/codegen 会拒绝它们。

---

## 核心概念

### CRM — 契约

**CRM**（Core Resource Model）声明远程资源暴露*哪些*方法。使用 `@cc.crm()` 装饰，方法体为 `...`（纯接口，无实现）。

```python
@cc.crm(namespace="demo.payload", version="0.1.0")
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload:
        ...
```

方法可标注 `@cc.read`（允许并发访问）或保持默认的 write（独占访问）。Portable method 用 `@cc.transfer(...)` 显式声明 nested FastDB input/output specification，并在每个方向承载零或一个 `Payload` envelope。

### Resource — 运行时实例

**Resource** 是实现 CRM 契约的普通 Python 类，持有状态和领域逻辑。它无需装饰器；框架通过注册时绑定的 CRM 契约发现其方法。命名应体现领域语义，例如 `EchoResource`。

上方示例中的 `EchoResource` 就是 resource object。CRM contract 固定 portable payload 边界；resource implementation 接收和返回官方 FastDB `Payload` owner。

### Client — 消费者

任何调用 `cc.connect(...)` 的代码都是 **client**（即 consumer / 应用代码）。返回的代理是位置透明的 — 无论资源运行在同进程还是远程机器，用法完全相同。

```python
echo = cc.connect(Echo, name="echo")
result = echo.echo(source)
result.close()
cc.close(echo)

# 或使用上下文管理器：
with cc.connect(Echo, name="echo") as echo:
    assert echo.ping() is None
```

### Server — 资源宿主

**Server** 是调用 `cc.register(...)` 托管一个或多个资源、并通常调用 `cc.serve()` 进入请求循环的进程。单个 server 进程可以托管任意多的资源（每个使用唯一的 `name`），并在首次注册时自动绑定一个 IPC 端点。

```python
import c_two as cc

cc.register(Echo, EchoResource(), name="echo")
cc.serve()                                     # 阻塞；Ctrl-C 触发优雅关闭
```

- **Server ID** 标识本地 IPC server 实例。C-Two 会在首次注册时自动生成；也可以在注册前调用 `cc.set_server(server_id=...)` 显式设置。
- **地址**（`ipc://...`）是由 Server ID 推导出的内部本地传输端点；只有同主机进程需要直连时才需要用 `cc.server_address()` 查看。
- **`cc.serve()` 是可选的** — 如果宿主进程已有自己的事件循环（Web 服务、GUI、模拟器），你可以只注册资源，让它们在后台服务，同时你的主循环照常运行。
- 一个进程可以**同时**是 server 和 client（注册一些资源，同时连接另一些）。

### Relay — 分布式发现

**HTTP 中继**（`c3 relay`）是一个轻量级代理，让客户端可以按**路由名和 CRM 契约**跨机器访问服务端。服务端在注册时向中继通告自己的 IPC 地址、CRM tag 和契约指纹；客户端使用路由名以及从 `cc.connect(CRMClass, name='...')` 派生出的预期 CRM 契约查询中继。

`c3` 是 C-Two 的跨语言原生 CLI。从源码 checkout 开发时，可以用 `python tools/dev/c3_tool.py --build --link` 构建并链接本地开发二进制。正式发布版可以通过 installer 安装最新 `c3`：

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

Python SDK 将 relay 生命周期交给 `c3 relay`、Docker Compose 或编排系统。Python 代码通过 `C2_RELAY_ANCHOR_ADDRESS` 或 `cc.set_relay_anchor()` 连接 relay anchor。anchor 是控制面注册和名称解析端点；远程 HTTP 调用仍会直连解析得到的 `relay_url`，只有 anchor 是 loopback/local 端点时才会选择本地 direct IPC。relay-aware 客户端会在首次调用前预检 route，并在收到结构化 stale-route 响应时重新解析路由；可通过 `C2_RELAY_ROUTE_MAX_ATTEMPTS` 调整最大 route acquisition 尝试次数（默认 `3`，有效范围 `1..=32`，`0` 按 `1` 处理）。可通过 `C2_RELAY_CALL_TIMEOUT` 调整 CRM 调用超时秒数（默认 `300`；`0` 禁用 reqwest 总超时）。可通过 `C2_REMOTE_PAYLOAD_CHUNK_SIZE` 调整 relay HTTP 和未来远程协议的 C-Two 远程载荷批大小（默认 `1048576`；最大 `134217728`）。这个设置控制 C-Two payload batching，独立于 TCP packet、HTTP/1 chunk 或 HTTP/2 DATA frame 边界。语义不明确的数据面失败需要调用方自定义 retry policy。relay resolve、probe 和 call 路径要求 route name 与 CRM contract 同时匹配，并拒绝契约不匹配的路由。

```bash
# 在网络可达的任意节点启动中继
c3 relay --bind 0.0.0.0:8080
```

Relay HTTP 与 mesh 端点应只暴露在可信网络边界内。生产部署应通过私有网络、防火墙、Kubernetes NetworkPolicy、service mesh 策略或 ingress 认证等基础设施限制访问。

```python
# 服务端 — 将资源通告给中继
cc.set_relay_anchor('http://relay-host:8080')
cc.register(Echo, EchoResource(), name='echo')
cc.serve()

# 客户端 — 按路由名和 CRM 契约解析，无需显式地址
cc.set_relay_anchor('http://relay-host:8080')
echo = cc.connect(Echo, name='echo')
```

多个中继可以通过 gossip 协议组成**网格集群** — 网格中任意中继都能解析整个集群内注册的任何资源。可运行示例见 [可运行示例](#可运行示例)。

> **何时需要中继？** 跨机器访问或按路由名与 CRM 契约发现时使用 relay。同进程和同主机 IPC 可以直接连接。

### Contract Release — 持久身份

Runtime route 标识一个活跃 resource instance，并不是持久 CRM contract
release。C-Two 从 canonical、已校验的 `c-two.contract.v2` descriptor
派生 route-independent `ContractReleaseRef`，使 catalog 和 lockfile
可以在 route 出现前以及消失后继续固定精确契约身份。

```python
descriptor = cc.export_contract_descriptor(Echo)
release_ref = cc.export_contract_release_ref(Echo)
```

语言中立的 Rust CLI 可以直接从 descriptor JSON 产生相同引用，无需启动
Python：

```bash
c3 contract release-ref contract.json
```

该引用只包含 descriptor schema、CRM namespace/name/version 与 canonical
descriptor SHA-256，刻意不包含 route。Digest 证明内容身份与完整性，不证明
publisher identity、authorization、revocation status 或 trust。C-Two
也不提供 contract registry、storage location 或 resolver；consumer 必须通过
自己的 catalog/deployment layer 解析 descriptor bytes，重建并校验
`ContractRelease`，之后才能加入 runtime route name。兼容性、信任、Rust
SDK 与 FastDB 分发边界详见
[延后能力 Issue](docs/issues/contract-release-deferred-capabilities.md)。

### Payload model — 显式 FastDB 委托

Portable method 使用显式 nested `fastdb.payload.v1` specification 和官方 `fastdb4py.payload.Payload` owner。`c-two.contract.v2` 是 super-schema：它拥有 CRM method、parameter、return shape 与 input/output binding 关系；每个 binding 的 `spec` 在 FastDB Core 编译之前始终只是 opaque JSON value。

FastDB Core 是 nested schema/profile/type、canonical bytes/digest、binary layout、builder、record/object-graph view、materialize、invalidate 与 payload-only C++/Rust/Python/TypeScript codegen 的唯一权威。C-Two 拥有 route/release identity、transport、scheduler/lease 生命周期、generated CRM adapter 与最终多 owner artifact composition，不复制 FastDB parser 或 runtime。

Portable method 在每个方向只承载零或一个 payload envelope：

```python
@cc.crm(namespace="demo.payload", version="0.1.0")
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload: ...
```

Python-only resource 仍可使用普通 Python annotation 和 pickle 做本地 prototype。这类方法会被诊断为 nonportable，并由 portable descriptor export/codegen 拒绝。

### cc.hold() — 客户端 retained ownership

当前已证明的 portable receive path 会打开 copy-backed FastDB `Payload`。`cc.hold()` 同时保留 C-Two response lease 与 payload owner，并保证 `held.release()` 先使 FastDB owner 及其 checked views 失效，再释放 lease。这是生命周期保证，不代表 FastDB 已直接在 response SHM 中构造或读取。返回的 `cc.Held[Payload]` 也把 retained raw wire buffer 作为 `.unsafe_buffer` 暴露给高级用户。

1. **显式 `.release()`** — 推荐用于同时持有多个缓冲区的复杂工作流
2. **上下文管理器（`with`）** — 推荐用于单缓冲区作用域
3. **`__del__` 兜底** — 最后手段，若忘记释放会触发 `ResourceWarning`

```python
echo = cc.connect(Echo, name='echo', address='ipc://server')

# Normal call — 返回 owned Payload。
result = echo.echo(source)

# Retained call — checked view 在 release 前有效。
with cc.hold(echo.echo)(source) as held:
    payload = held.value
    with payload.entry_view(0) as values:
        with values.at(0) as value:
            assert value.get_u8() == 7
```

`held.value` 是普通 API；FastDB checked view 会在 `held.release()` 后 fail fast。`held.unsafe_buffer` 是 raw `memoryview` escape hatch；由此产生的 NumPy array 或 pointer 会绕过 FastDB owner check，无法被机械撤销。需要跨越 hold scope 保留逻辑值时，应通过 FastDB materialize。

---

### InputLifetime — 服务端 borrowed input

服务端 portable input 默认由 owner 管理。`cc.InputLifetime.BORROWED` 是显式的 call-scoped lifetime policy，仅用于 CRM signature 接收 `Payload` 的 resource method；调用返回或抛错时，C-Two 会先使该 payload 及其 checked views 失效，再释放 request lease。

不得在 method 返回后保留 borrowed payload 或 checked view；需要的 FastDB logical value 必须在调用期间 materialize。Raw pointer 或 buffer alias 始终是显式 unsafe escape。

---

## 可运行示例

快速开始已经展示完整 authoring pattern。仓库中的 examples 提供可直接运行的进程布局：

| 场景 | 入口 |
| --- | --- |
| 同进程本地调用 | [`examples/python/local.py`](examples/python/local.py) |
| Direct IPC resource/client | [`examples/python/ipc_resource.py`](examples/python/ipc_resource.py), [`examples/python/ipc_client.py`](examples/python/ipc_client.py) |
| Relay mesh | [`examples/python/relay_mesh/`](examples/python/relay_mesh/) |
| Python-only grid prototype | [`examples/python/grid/`](examples/python/grid/) |
| Portable payload runtime 与 lifetime proof | [`sdk/python/tests/integration/test_portable_payload_runtime.py`](sdk/python/tests/integration/test_portable_payload_runtime.py) |
| Rust/Python generated-artifact interoperability proof | [`sdk/python/tests/integration/test_portable_payload_cross_language.py`](sdk/python/tests/integration/test_portable_payload_cross_language.py) |

### 服务端监控

使用 `cc.hold_stats()` 监控 retained response-buffer lease：

```python
stats = cc.hold_stats()
# {'active_holds': 3, 'total_held_bytes': 52428800, 'oldest_hold_seconds': 12.5}
```

---

## 架构

**C-Two 围绕 resource 组织分布式程序。**

在科学计算中，封装复杂状态和领域特定操作的资源需要被组织为内聚的单元。我们称描述这些资源的契约为 **核心资源模型（CRM）**。应用程序以 *如何与资源交互* 为中心，同时由 C-Two 处理资源所在位置带来的访问差异。C-Two 提供位置透明和统一的资源访问，使任何 **client** 都能像访问本地对象一样与资源交互。

<p align="center">
  <img src="docs/images/architecture.png" alt="C-Two 架构图" width="100%">
</p>

### 客户端层

调用 `cc.connect(...)` 消费资源的任何代码。返回的代理提供完整的类型安全和位置透明性，client code 可以在不跟踪进程或机器位置的情况下使用 resource。

- `cc.connect(CRMClass, name='...', address='...')` 返回类型化的 CRM 代理
- 代理支持上下文管理：`with cc.connect(...) as x:` 自动关闭
- 对 IPC 与 relay 路径，SDK 从 CRM class 推导 expected route contract，native 层在调用前校验 route name、CRM tag、ABI hash 和 signature hash。

### 资源层

服务端有状态的实例，通过标准化的 CRM 契约暴露。

- **CRM 契约**：使用 `@cc.crm()` 装饰的接口类。只有在此声明的方法才可被远程访问。
- **Resource**：实现契约的普通 Python 类 — 状态 + 领域逻辑，无需装饰器。
- **Portable payload**：`@cc.transfer(...)` 把一个显式 nested FastDB specification 绑定到 `fastdb4py.payload.Payload` input/output。
- **Python fallback**：普通 Python 类型可用于 Python-only prototype，但 portable export/codegen 会拒绝 pickle fallback。
- **`@cc.read` / `@cc.write`**：并发注解 — 并行读取，独占写入。
- **`@cc.on_shutdown`**：生命周期回调，在资源被注销时调用；它位于 RPC surface 之外。

### 传输层

协议无关的通信，基于地址方案自动检测协议：

| 协议方案 | 传输方式 | 适用场景 |
|----------|----------|----------|
| `thread://` | 进程内直接调用 | 零序列化、测试 |
| `ipc://server` | Unix 域套接字或 Windows Named Pipes + 原生共享内存 | 多进程、同主机 |
| `http://host:port` | HTTP 中继 | 跨机器、Web 兼容 |

IPC 传输采用 **控制面 / 数据面分离**：方法路由通过 Rust `c2-local` 的本地流传输，Unix 使用 UDS，Windows 使用字节模式 Named Pipes；payload bytes 可以通过原生共享映射交换。当前 portable FastDB receive path 是 copy-backed；`cc.hold()` 与 `cc.InputLifetime.BORROWED` 提供显式 invalidation/lease 边界，但不代表 FastDB 已直接构造在 transport memory 中。Windows 的实际测试结果见[实现记录](docs/windows-native-implementation.md)。

### Rust 原生层

核心 runtime 是语言中立的 Rust，SDK 绑定到同一套 core contract。性能关键组件通过 [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) 暴露给 Python：

Rust 工作空间按 4 层组织（foundation → protocol → transport → runtime），Python PyO3 extension 位于 `sdk/python/native/`：

- **Contract Core (`c2-contract`)** — 语言中立的 `c-two.contract.v2` validation、canonical descriptor hashing、release identity 与 opaque nested-spec extraction。
- **Contract Codegen (`c2-codegen`)** — 委托官方 FastDB Rust projection、校验返回 artifacts、确定性组合 C-Two/FastDB 输出并发布完整新目录。
- **伙伴分配器** — 通过共享内存内的跨进程原子锁保护分配与释放。拒绝继续修改持有者崩溃或 panic 后可能未完成的分配器状态。
- **线协议** — 帧编码、分块组装和分块注册表，管理大载荷的生命周期。
- **HTTP 中继** — 基于 [axum](https://github.com/tokio-rs/axum) 的高吞吐网关，桥接 HTTP 到 IPC。处理连接池和请求多路复用。

已发布的 Rust 扩展会由 `pip install c-two` 从预编译 wheel 安装，或由
`uv sync` 从源码构建。这不改变上方的 portable package 分发限制：完整
v2/FastDB 集成目前仍只在源码 checkout 中可复现。

`c3` 作为原生 CLI 二进制分发，并由根目录下的 `cli/` 包构建。正式发布版可以通过 installer 安装：

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

源码 checkout 开发时可以通过 `python tools/dev/c3_tool.py --build --link` 链接本地开发二进制；正式 CLI 产物由独立的 CLI release 流水线负责。

Portable CRM descriptor 可以从 Python CRM 类导出，并在作为 codegen 输入前交给 Rust CLI 校验：

```bash
uv run python -m c_two.cli.contract export mypkg.contracts:Geometry --out geometry.contract.json
c3 contract diagnose mypkg.contracts:Geometry --python .venv/bin/python --pretty
c3 contract export mypkg.contracts:Geometry --python .venv/bin/python --out geometry.contract.json
c3 contract validate geometry.contract.json
c3 contract release-ref geometry.contract.json --out geometry.release-ref.json
```

`c3 contract diagnose` 会在 portable export 失败前报告 Python-only pickle method，Rust CLI 也会在写出前校验 diagnostic payload。经过校验的 v2 descriptor 已经包含每个 nested FastDB specification，不存在独立 sidecar 或第二份 payload-schema input。针对一个受支持目标生成完整的新项目树：

```bash
c3 contract codegen rust geometry.contract.json --out-dir generated-rust
c3 contract codegen python geometry.contract.json --out-dir generated-python
c3 contract codegen typescript geometry.contract.json --out-dir generated-typescript
```

每个 destination 必须尚不存在。生成流程先校验 outer contract，将所有 nested value 委托给 FastDB Core，校验 hash/path，再发布一个确定性目录，其中包含：

- `metadata/contract.json`
- `metadata/contract-release-ref.json`
- `metadata/composition-manifest.json`
- target-specific C-Two contract module
- binding-specific `payloads/` 路径中的 FastDB Core-owned payload modules

Python 可以消费同一个 in-memory authority path：

```python
artifacts = cc.compile_contract_artifacts(descriptor, target="rust")
```

对于 resource-first 项目，`c3 contract infer ... --diagnose` 可以说明被选择的普通 Python method 为何仍是 Python-only。Portable contract 必须通过 `@cc.transfer(...)` 显式 author；inference 不会从 domain annotation 合成 FastDB 结构。

---

## 安装

### 从 PyPI 安装

```bash
pip install c-two
```

该命令安装最新已发布的 C-Two runtime，尚不包含上文完整的
portable-payload 用户面。Registry 安装成功不能作为审计后开发分支集成已
可用的证据。

预编译 wheel 支持：

- **Linux**：x86_64、aarch64
- **macOS**：Apple Silicon (aarch64)、Intel (x86_64)
- **Python**：3.10、3.11、3.12、3.13、3.14、3.14t（自由线程）

如果没有适合你平台的预编译 wheel，pip 将从源码编译（需要 [Rust 工具链](https://rustup.rs)）。

### 开发环境

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
# 将 FastDB 0.2.0 源码 checkout 放在 ../fastdb。
# Windows 还需要 docs/windows-native-usage.md 中记录的 MSVC 修复源；
# windows-native.yml 固定了实际使用的精确提交。
cp .env.example .env               # 配置环境变量（可选）
uv sync                            # 安装依赖 + 编译 Rust 扩展
uv sync --group examples           # 安装示例依赖（pandas、pyarrow）
python tools/dev/c3_tool.py --build --link  # 在源码检出中构建并链接原生 c3 CLI
uv run pytest                      # 运行测试套件

# Python 3.10 compatibility check. 下游 Taichi 等栈仍可能固定在 3.10。
uv python install 3.10
uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs
```

> 需要 [uv](https://github.com/astral-sh/uv) 和 Rust 工具链。

---

## 路线图

| 能力 | 状态 |
|------|------|
| 核心 RPC 框架（CRM + Resource + Client） | ✅ 稳定 |
| IPC 传输 + SHM 伙伴分配器 | ✅ 稳定 |
| HTTP 中继（Rust 驱动） | ✅ 稳定 |
| 中继网格与 gossip 路由发现 | ✅ 稳定 |
| 分块载荷传输（载荷 > 256 MB） | ✅ 稳定 |
| 心跳与连接管理 | ✅ 稳定 |
| 读/写并发控制 | ✅ 稳定 |
| 统一配置架构（Rust resolver 单一事实源） | ✅ 稳定 |
| CI/CD 与多平台 PyPI 发布 | ✅ 稳定 |
| 极端载荷磁盘溢出 | ✅ 稳定 |
| `c-two.contract.v2` + Core-owned artifact composition | ✅ 本地已证明 |
| `cc.hold()` 的 FastDB owner/view invalidation | ✅ 本地已证明 |
| 共享内存驻留监控（`cc.hold_stats()`） | ✅ 稳定 |
| Route-independent contract release identity | ✅ 稳定 |
| Immutable portable-package distribution | 🔜 规划中 |
| 契约版本兼容协商 | 🔜 规划中 |
| `auth_hook` 与 call metadata | 🔜 规划中 |
| dry-run 钩子 | 🔜 规划中 |
| 异步接口 | 🔜 规划中 |
| 自适应内存生命周期策略 | 🔜 规划中 |
| Streaming RPC / pipeline 语义 | 🔜 规划中 |
| 跨语言客户端（Rust 优先，TypeScript 随后） | 🔮 远期 |
| 全局发现与命名空间治理 | 🔮 远期 |

详见[当前路线图](docs/roadmap.zh-CN.md)。历史路线图笔记仍归档在 `docs/plans/` 下。

---

## 开源协议

[MIT](LICENSE)

---

<p align="center">为科学计算而生，由 Rust 驱动。</p>
