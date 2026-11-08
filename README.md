<p align="center">
  <img src="docs/assets/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  RPC for stateful resources.
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a> <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/Python-3.10%2B-blue" alt="Python 3.10+" /></a> <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" /> <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a> <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.zh-CN.md">中文版</a>
</p>

---

## What is C-Two?

C-Two is an RPC framework for stateful resources, built for distributed scientific computation. It sits between message libraries such as ZeroMQ and service frameworks such as gRPC. Your objects keep their usual form, and C-Two carries calls to them across processes and machines; it also handles what a message library leaves to the application, including the resource's state, concurrent access to it and the outcome of each call. Clients use the same method interface for local and remote resources.

A **CRM contract** declares the callable methods and their namespace/version. A **resource** is a plain class that implements those methods and holds state. A **client** uses `cc.connect(...)` to obtain a typed proxy. A process can host several resources and connect to other resources at the same time. The name stands for *CRM to Call* (Core Resource Model to Call): a stateful resource is called from other processes and machines through its CRM.

**C-Two 0.10.1** supports Python 3.10+. Ordinary calls have no Python runtime dependencies; FastDB payloads use `c-two[fastdb]` (`fastdb4py==0.2.4`). IPC handshake 14 reports a version mismatch as a typed error, Unix endpoint namespaces use format 3 with versioned default directories, and Python adds independent `cc.Runtime()` instances and `cc.endpoint.server_credential()`. A failed memory release stays with its owner for retry, or is abandoned conservatively and counted when it can no longer be returned. See the [0.10.0 release notes](docs/releases/0.10.0.md); 0.10.1 keeps `cc.serve()` running when stdout cannot encode its messages ([notes](docs/releases/0.10.1.md)).

## Why stateful RPC is different

A stateless service can hand each request to any instance and keeps its state in an external store. A C-Two call always reaches the same resource object and may change its state. The table compares the two.

| | Stateless RPC services | C-Two resources |
| --- | --- | --- |
| Where a call goes | Any instance of the service; state is usually kept in an external store | The same resource object, which keeps the state |
| Concurrency | Requests are independent of each other | Methods are writes by default and run one at a time on a resource; methods marked `@cc.read` can run in parallel |
| After a failure | Calls can be configured to be retried or hedged automatically | A call that may already have run is not retried or replayed automatically; Python and Rust errors report whether it may have run |
| Shutdown | Usually a grace period, after which in-flight requests are cancelled | New calls are refused; unary calls already sent run to completion and deliver their results, and open server streams end between steps after the items already produced |
| Large data | Each message is encoded whole in memory; large data is usually split by the application | A single call can carry arrays of several gigabytes; on one host they travel through shared memory, with chunking or file spill when needed, and the limits are configurable |

## Features

- **Local and remote calls:** same-process calls pass Python objects directly; same-host IPC uses Unix domain sockets or Windows Named Pipes; cross-machine calls go through the HTTP relay. Relay discovery selects a verified local IPC path or an HTTP target, and explicit IPC connections work independently of relay discovery.
- **State and concurrency:** resources keep their state between calls. `@cc.read` and `@cc.write` declare access semantics, with writes as the default. Native route scheduling enforces concurrency and pending-work limits across local and remote invocation.
- **Payloads and memory:** `@cc.transfer(...)` binds FastDB `Payload` specifications, and ordinary Python values also work between Python peers. IPC moves large payloads through lazy buddy pools, dedicated shared memory, checked chunk transfer and file spill, within memory budgets. `cc.hold()` retains a response's local copy, and registration can select borrowed inputs; both keep only local copies, and release invalidates payload owners and checked views first. See [memory policy](docs/reference/memory-policy.md) and [Python payload usage](docs/reference/python-usage.md).
- **Server streams:** a method marked `@cc.stream` returns zero or more items for one input. Python and Rust resources produce items one step at a time and hold the route permit only within a step; clients iterate, hold or close the stream. Streams work within one process, over IPC and, for Rust and Python clients, through a relay; generated TypeScript clients consume them over IPC and do not read them through a relay yet. See [Python server streams](docs/reference/python-usage.md#server-streams).
- **Contracts across languages:** contract tooling validates portable descriptors and generates Python, Rust and TypeScript bindings. Clients match the route name, CRM identity and contract hashes. Python and Rust use the same Rust core for routing, transport, memory, errors and lifecycle.
- **Relay, tools and lifecycle:** the native c3 CLI runs the relay and relay meshes, contract tools and endpoint maintenance on Linux, macOS and Windows x64 and ARM64. `cc.observe.connection_events()` aggregates outgoing and accepted physical IPC connections for one cancellable consumer; its reasons describe transport facts, not proof of process death. Native shutdown and route unregistration let unary calls already sent finish and end open server streams between their steps with `CallCancelled` after the items already produced; scoped endpoint maintenance cleans up confirmed exits. See [lifecycle integration](docs/reference/local-endpoint-lifecycle.md).

## Install

```bash
uv pip install c-two
# For FastDB payloads:
uv pip install 'c-two[fastdb]'
```

The same command works in Windows PowerShell. For c3 on Linux or macOS:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-installer.sh | sh -s -- --version 0.6.0
```

On Windows x64 or ARM64, use the [x64 executable](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-x86_64-pc-windows-msvc.exe), [ARM64 executable](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-aarch64-pc-windows-msvc.exe) or [PowerShell installer](https://github.com/world-in-progress/c-two/releases/download/c3-v0.6.0/c3-installer.ps1). Installation and checksum verification are in the [Windows guide](docs/reference/windows-native-usage.md#install-python-and-c3).

C3 0.6.0 pairs with C-Two 0.10.x. IPC handshake 14 cannot interoperate with 0.9.x peers (IPC 13), and Unix endpoint namespace format 3 uses new default directories, so upgrade the Python and Rust SDKs, c3 and Node consumers on one host together; stop old services before upgrading and restarting. C/FFI ABI 6 and SHM 3 keep their formats. See the [0.10.0 migration notes](docs/releases/0.10.0.md), the [handshake spec](docs/reference/ipc-handshake.md) and the [endpoint lifecycle guide](docs/reference/local-endpoint-lifecycle.md).

Source builds need Rust 1.95 or newer; for builds and tests, see the [development guide](docs/reference/development.md).

## Quickstart

Save as `counter.py`:

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

Run with `python counter.py`. The counter's `value` is kept in the resource between calls. This example uses the same-process path; keep `C2_RELAY_ANCHOR_ADDRESS` unset for local registration.

For another process on the same host, pass the resource process's `cc.server_address()` as the client's `address`; the [IPC example](examples/python/ipc_resource.py) shows this connection. The resource process can also capture `credential = cc.endpoint.server_credential()` after registration and pass it to its controller for later exact cleanup, as described in the [lifecycle guide](docs/reference/local-endpoint-lifecycle.md#exact-cleanup-and-bounded-sweeps).

`runtime = cc.Runtime()` creates an application-owned Python Runtime with its own configuration, routes and native session. Configure it through `runtime.set_*()`, then use `runtime.start()` for an empty Host readiness check or `runtime.register()` for resources. The module functions continue to use the default runtime; `cc.serve()` owns its process signal handling and has no instance equivalent. Call `runtime.shutdown()` and inspect its structured outcome when the application is done; if it returns pending, call shutdown again to observe completion and replace the session. See the [independent runtime example](examples/python/independent_runtime.py) and [Python API guide](docs/reference/python-usage.md#independent-python-runtime-instances).

For relay routing, start `c3 relay --bind 0.0.0.0:8300`, set `cc.set_relay_anchor('http://relay-host:8300')` on both sides, then connect by name. The relay is a separate process. Explicit IPC addresses bypass relay discovery.

## Examples and documentation

| Topic | Guide |
| --- | --- |
| Local calls | [local.py](examples/python/local.py) |
| Direct IPC | [resource](examples/python/ipc_resource.py), [client](examples/python/ipc_client.py) |
| Independent Python Runtime | [owned endpoint preflight](examples/python/independent_runtime.py) |
| Relay | [resource](examples/python/relay_resource.py), [client](examples/python/relay_client.py), [mesh](examples/python/relay_mesh/README.md) |
| Configuration: code, environment and `.env` | [Configuration guide](docs/reference/configuration.md) |
| Python contracts, portable payloads and API index | [Python SDK guide](docs/reference/python-usage.md) |
| Rust SDK and API migration | [Rust SDK guide](sdk/rust/README.md) |
| Relay and contract tools | [c3 CLI guide](cli/README.md) |
| Connection and endpoint APIs | [Lifecycle guide](docs/reference/local-endpoint-lifecycle.md) |
| IPC handshake protocol | [Handshake spec](docs/reference/ipc-handshake.md) |
| Windows | [Build and usage](docs/reference/windows-native-usage.md) |
| Environment variables | [.env.example](.env.example) |
| Development | [Build and tests](docs/reference/development.md), [CI and releases](docs/reference/ci-cd.md), [contributing](CONTRIBUTING.md) |

## License

[MIT](LICENSE).
