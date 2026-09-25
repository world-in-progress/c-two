<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  面向资源的 RPC runtime — 将有状态类转变为位置透明的分布式资源。
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

## C-Two 是什么

C-Two 是面向分布式科学计算的 resource-oriented RPC runtime。它不要求把对象中的状态拆成无状态服务，而是保留有状态的类 — 模拟器、索引、仪器控制器 — 并通过 CRM（Core Resource Model）契约暴露。任何 client 都可以像访问本地对象一样调用 resource，无论它在同进程、同主机还是其他机器上。

- **面向 resource 而非 service** — CRM 契约类声明 resource 暴露*哪些*方法；普通 Python 类带着真实状态实现它们；任何调用 `cc.connect(...)` 的代码都是 client。
- **显式 transport** — 同进程调用直接传递 Python 对象、零序列化；同主机 IPC 使用 Unix domain socket 或 Windows Named Pipe，载荷字节可经原生共享内存传输；跨机器调用经由独立 `c3` CLI 运行的 HTTP relay。
- **Portable 载荷** — 需要跨语言的方法用 `@cc.transfer(...)` 显式绑定官方 FastDB `Payload`；普通 Python 值仍可用于 Python 范围内的原型开发。
- **单一 Rust core** — 路由、client/host 调用、重试分类、wire codec、共享内存、relay transport 和配置都实现在语言中立的 Rust crate 中；Python 与 Rust SDK 只是同一 runtime 的 facade。

## 现状

C-Two 是处于活跃开发中的 0.x 软件。以下事实截至 2026-09-25：

| 方面 | 状态 |
| --- | --- |
| 已发布 Python 包 | `pip install c-two` 安装 0.5.x 稳定线（最新 0.5.1）。它早于下文描述的 portable FastDB 载荷面、原生 Windows IPC 和 Rust SDK。 |
| 本仓库 | 正在准备 `c-two` 0.6.0 与 `c3` 0.2.0。尚未发布；release-candidate CI 仍在进行中。见 [docs/releases/0.6.0.md](docs/releases/0.6.0.md)。 |
| FastDB 依赖 | [FastDB 0.2.1](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.1) 已正式发布；本 checkout 固定 `fastdb4py==0.2.1` 与 Rust `fastdb = "=0.2.1"`。 |
| Windows | 源码构建已在 Windows Server 2022/2025 x64（CPython 3.12）上按固定旧版源码组合完成验证 — 见[验证报告](docs/reports/windows-native-final-validation.md)。尚未发布 Windows wheel 或 CLI 二进制；Windows 11 桌面、ARM64 与 Windows 服务仍是未验证目标。 |
| Rust SDK | `sdk/rust` 是 Cargo 包 `c-two` 0.1.0，`publish = false`。没有 crates.io 上的 C-Two 发布。 |
| 基准测试 | portable `Payload` API 尚无经过评审的吞吐基准。来自已移除集成的历史数字不能作为当前架构的性能声明。 |

0.x 版本线优先干净切割而非兼容垫片：portable 契约为 `c-two.contract.v2`，相互通信的 client、host 与 relay 必须一起升级。

## 安装

### 稳定线（PyPI）

```bash
pip install c-two
```

安装已发布的 0.5.x runtime。预编译 wheel 覆盖 manylinux x86_64/aarch64 与 macOS aarch64/x86_64 上的 CPython 3.10–3.14 及自由线程 3.14t，其他平台使用 sdist（源码构建需要 [Rust 工具链](https://rustup.rs)）；`fastdb4py` 依赖会自动安装。

> 0.5.x 早于本 README 描述的 portable FastDB 载荷面、原生 Windows IPC transport 和 Rust SDK。这些能力目前可从源码 checkout 使用，并正在为 0.6.0 发布做准备；`pip install` 成功不代表它们已经发布。

### 开发环境（源码 checkout）

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
# 完整互操作测试、golden fixture 与 TypeScript fixture 还需要将固定版本的
# FastDB 源码 checkout 放在同级目录：
git clone --branch v0.2.1 --depth 1 https://github.com/world-in-progress/fastdb.git ../fastdb
# Core 与 Rust SDK 测试使用已发布的 Rust binding 和 system 链接模式。
# 请先从 FastDB 0.2.1 发布页解压匹配平台的 Core SDK：
export FASTDB_PAYLOAD_LINK_MODE=system
export FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/absolute/path/to/fastdb-core-sdk/lib
case "$(uname -s)" in
  Darwin) export DYLD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
  Linux) export LD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
esac
uv sync                                       # 安装依赖 + 编译 Rust 扩展
uv sync --group examples                      # 可选：示例所需的 pandas/pyarrow
python tools/dev/c3_tool.py --build --link    # 构建并链接原生 c3 CLI
cp .env.example .env                          # 可选：本地环境配置
```

需要 [uv](https://github.com/astral-sh/uv) 和 Rust 工具链。上面的配置用于通过 Core SDK 开发和测试。用于分发的 CLI 与 Python 扩展构建选择 `FASTDB_PAYLOAD_LINK_MODE=source`，静态链接同一份固定的 FastDB 0.2.1 源码；Core/Rust SDK 则保留 system 链接约定。用 `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q` 运行 Python 测试，用 `cargo test --manifest-path core/Cargo.toml --workspace` 与 `cargo test --manifest-path sdk/rust/Cargo.toml --all-features` 运行 Rust 测试。Python 3.10 仍是最小支持版本。Windows 请改用 [Windows 构建指南](docs/windows-native-usage.md)。

### c3 CLI

`c3` CLI 运行 relay server 与契约工具。最新已发布版本是 c3 0.1.4，附带 Linux 与 macOS 二进制：

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

尚无已发布的 Windows 二进制 — 请按 [Windows 构建指南](docs/windows-native-usage.md)从源码构建。源码 checkout 可用 `python tools/dev/c3_tool.py --build --link` 构建并链接当前开发版 CLI。

## 快速开始

将以下内容保存为 `counter.py`：

```python
import c_two as cc


@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...


class CounterResource:
    """普通 Python 类 — 状态 + 领域逻辑，无需装饰器。"""

    def __init__(self, initial: int = 0):
        self._value = initial

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def value(self) -> int:
        return self._value


cc.register(Counter, CounterResource(initial=10), name='counter')

with cc.connect(Counter, name='counter') as counter:
    print(counter.increment(3))   # 13
    print(counter.increment(4))   # 17
    print(counter.value())        # 17

cc.unregister('counter')
cc.shutdown()
```

运行方式：

```bash
pip install c-two
python counter.py
```

预期输出：

```text
13
17
17
```

该示例假定未配置 relay。如果你的环境设置了 `C2_RELAY_ANCHOR_ADDRESS` 而 relay 未运行，请用 `C2_RELAY_ANCHOR_ADDRESS= python counter.py` 启动，使注册保持本地。

各部分与模型的对应关系：

- **CRM 契约** — `Counter` 是接口：用 `@cc.crm(namespace=..., version=...)` 装饰，方法体为 `...`。只有契约中的方法可被远程调用。`@cc.read` 标记方法允许并发访问；未标记的方法取独占写访问。
- **Resource** — `CounterResource` 是持有真实状态（`self._value`）的普通类。框架通过注册时绑定的 CRM 契约发现其方法；不需要装饰器。
- **Server** — `cc.register(...)` 以路由 `name` 托管 resource 并自动绑定 IPC 端点；若要为其他进程提供服务，可调用 `cc.serve()` 阻塞在请求循环上。一个进程可以同时注册一些 resource 并连接另一些。
- **Client** — `cc.connect(Counter, name='counter')` 返回类型化、位置透明的代理。不带地址且无 relay 时，它解析到同进程 resource 并直接传递 Python 对象，完全跳过序列化。

同一个 CRM 契约可原样用于 IPC 或 relay：

```python
# 同主机直连 IPC — 地址来自托管进程（cc.server_address()）或你自己的 ipc:// 指定：
counter = cc.connect(Counter, name='counter', address='ipc://my_server')

# 跨机器 — 两端指向同一个 relay，然后按名称连接：
cc.set_relay_anchor('http://relay-host:8080')
counter = cc.connect(Counter, name='counter')
```

完整的双进程与 relay mesh 布局见[可运行示例](#可运行示例)。

## 传输方式

| 模式 | 选择方式 | 传输 | 适用场景 |
| --- | --- | --- | --- |
| 同进程 | `cc.connect(...)` 不带地址且无 relay，目标已在本地注册 | 直接调用，零序列化 | 测试、单进程内组合 resource |
| IPC | 显式 `address='ipc://...'` | Unix domain socket / Windows Named Pipe；载荷字节可经原生共享内存 | 同主机多进程 |
| HTTP relay | `cc.set_relay_anchor(...)` 或 `C2_RELAY_ANCHOR_ADDRESS`，然后按名称连接 | HTTP 连向 relay 解析出的路由 | 跨机器调用与基于名称的发现 |

直连 IPC 不依赖 relay：显式 `ipc://` 地址会绕过 relay 发现，在未配置 relay 时也可用。使用 relay 时，解析是契约作用域的 — client 从其 CRM 类派生预期路由契约，runtime 在任何调用前匹配路由名、CRM tag、ABI hash 与 signature hash。relay 响应默认选择 HTTP，仅当 anchor 是 loopback 时才可能在通过身份验证后选择直连 IPC 快路径。

relay 是你自己运行和监控的独立进程 — `c3 relay`、Docker Compose 或你的编排系统；Python SDK 不会内嵌 relay。多个 relay 可通过 gossip 组成 mesh，mesh 内任意 relay 都能解析整个集群注册的路由。relay 端点面向可信网络边界：请用私有网络、防火墙等基础设施限制访问。完整调优面（超时、分块大小、内存池上限、路由尝试次数等）见 [.env.example](.env.example)。

大体量载荷在 IPC 与 relay 路径上以有界分块传输。这是请求/响应模型下的传输分批，不是 streaming-RPC API — 流式调用语义尚未实现。

## Portable 载荷（FastDB）

本节需要正在准备 0.6.0 的开发分支。已发布的 0.5.1 包没有 `cc.transfer`；前面的 Counter 快速开始示例也可用于该已发布版本。

需要跨语言传递结构化数据的方法显式绑定 FastDB specification。FastDB Core 拥有嵌套载荷语义 — 校验、canonical identity、二进制布局、builder、view、失效与 payload-only codegen — C-Two 拥有外层契约、路由、transport 与生命周期：

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

portable 方法在每个方向承载零或一个 `Payload` envelope；C-Two 将每个嵌套 specification 作为 opaque JSON value 嵌入，绝不重新解释它。没有显式绑定的方法仍可使用普通 Python 值做 Python 范围的原型开发，但 portable descriptor export 与 codegen 会诊断并拒绝它们。

## 载荷生命周期

已证明的 portable 接收路径打开 **copy-backed** FastDB owner。`cc.hold()` 是生命周期契约，不是零拷贝声明：它将 C-Two response lease 与 payload owner 一起保留，并保证释放时*先*使 FastDB owner 及其 checked view 失效，再归还 lease。

```python
with cc.hold(echo.echo)(source) as held:
    payload = held.value                    # FastDB Payload owner；checked view 保持有效
    ...
# 离开 with 块（或 held.release()）会先失效 owner 与 view，再释放 lease
```

释放机制分层：显式 `.release()`、`with` 上下文管理器，以及两者都遗漏时发出警告的 `__del__` 兜底。`cc.hold_stats()` 报告活跃 hold，用于监控。

`held.unsafe_buffer` 将保留的原始 wire buffer 作为 `memoryview` escape hatch 暴露。由此派生的 NumPy array 或指针会绕过 FastDB 的 checked owner/view 模型，**无法被机械撤销** — 需要跨越 hold 作用域保存的逻辑值，请先通过 FastDB materialize。

服务端 portable 输入默认为 owned。注册时使用 `cc.register(..., input_lifetime={...: cc.InputLifetime.BORROWED})` 可让指定方法选择 call-scoped borrowed input：调用返回或抛错时，C-Two 会先使该 payload 及其 checked view 失效，再释放 request lease。不得在方法返回后保留 borrowed payload 或 view。

## 入口

### Python SDK

主要用户面，即本 README 各处展示的用法。顶层 `cc` 命名空间按功能分组：

- 契约编写：`@cc.crm`、`@cc.read`、`@cc.write`、`@cc.on_shutdown`、`@cc.transfer`、`cc.hold`、`cc.InputLifetime`
- 注册与连接：`cc.register`、`cc.connect`、`cc.close`、`cc.unregister`、`cc.serve`、`cc.shutdown`、`cc.server_address`、`cc.set_server`、`cc.set_client`、`cc.set_relay_anchor`、`cc.set_transport_policy`
- 契约工具：`cc.export_contract_descriptor`、`cc.export_contract_release_ref`、`cc.compile_contract_artifacts`、`cc.infer_crm_from_resource`
- 监控：`cc.hold_stats`

### Rust SDK

用户面 Rust SDK 位于 [`sdk/rust`](sdk/rust/README.md)：Cargo 包 `c-two`，导入为 `c_two`，版本 0.1.0，`publish = false`（本地候选 — 没有 crates.io 发布）。它与 Python 复用同一个 `c2-core` runtime 进行直连 IPC、显式 relay 与 relay-aware 调用，并提供生成的类型化 client 与 service trait。portable 值保持官方 `fastdb::Payload` owner 身份：

```rust
use c_two::{Connect, ContractLimits, ContractRelease, Runtime};
use fastdb::Payload;
```

构建它与上方开发环境使用相同的 FastDB Core SDK system-link 环境变量。在 C-Two checkout 中：

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

### c3 CLI

`c3` 是跨语言原生 CLI（由根目录 `cli/` 包构建）：

```bash
# 在网络可达的任意节点启动 relay
c3 relay --bind 0.0.0.0:8080
# 预注册 upstream：NAME=SERVER_ID@ADDRESS（ID 必须与 IPC handshake 一致）
c3 relay --upstream grid=my_server@ipc://my_server

# 契约工具 — 从 Python CRM 类导出、校验、派生身份、生成代码
c3 contract export mypkg.contracts:Geometry --python .venv/bin/python --out geometry.contract.json
c3 contract validate geometry.contract.json
c3 contract release-ref geometry.contract.json
c3 contract codegen rust geometry.contract.json --out-dir generated-rust   # 亦支持：python、typescript
```

`c3 contract diagnose` 会在 portable export 失败前报告哪些方法仍是 Python-only。生成目录包含校验过的契约元数据与 FastDB Core 拥有的 payload 模块；目标目录必须尚不存在。完整选项见 `c3 --help` 与 `c3 relay --help`。

## 可运行示例

| 场景 | 入口 |
| --- | --- |
| 同进程本地调用 | [`examples/python/local.py`](examples/python/local.py) |
| 直连 IPC resource/client | [`examples/python/ipc_resource.py`](examples/python/ipc_resource.py), [`examples/python/ipc_client.py`](examples/python/ipc_client.py) |
| Relay resource/client | [`examples/python/relay_resource.py`](examples/python/relay_resource.py), [`examples/python/relay_client.py`](examples/python/relay_client.py) |
| Relay mesh | [`examples/python/relay_mesh/`](examples/python/relay_mesh/) |
| Python-only grid 原型 | [`examples/python/grid/`](examples/python/grid/) |
| portable 载荷 runtime 与生命周期证明 | [`sdk/python/tests/integration/test_portable_payload_runtime.py`](sdk/python/tests/integration/test_portable_payload_runtime.py) |
| Rust/Python 生成物互操作证明 | [`sdk/python/tests/integration/test_portable_payload_cross_language.py`](sdk/python/tests/integration/test_portable_payload_cross_language.py) |

## 文档

| 主题 | 文档 |
| --- | --- |
| 贡献指南 | [CONTRIBUTING.md](CONTRIBUTING.md) |
| 面向 agent 与维护者的仓库指南 | [AGENTS.md](AGENTS.md) |
| 路线图 | [docs/roadmap.md](docs/roadmap.md) · [中文](docs/roadmap.zh-CN.md) |
| 变更日志 | [CHANGELOG.md](CHANGELOG.md) |
| 0.6.0 / c3 0.2.0 发布准备 | [docs/releases/0.6.0.md](docs/releases/0.6.0.md) |
| Windows 构建与使用 | [docs/windows-native-usage.md](docs/windows-native-usage.md) |
| Windows 实现记录 | [docs/windows-native-implementation.md](docs/windows-native-implementation.md) |
| 环境变量参考 | [.env.example](.env.example) |
| Rust SDK 边界 | [sdk/rust/README.md](sdk/rust/README.md) |
| 延后能力与开放边界 | [docs/issues/contract-release-deferred-capabilities.md](docs/issues/contract-release-deferred-capabilities.md) |

## 开源协议

[MIT](LICENSE)

---

<p align="center">为资源导向的计算而生，由 Rust 驱动。</p>
