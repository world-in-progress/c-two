<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  RPC for stateful resources.
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a>
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/Python-3.10%2B-blue" alt="Python 3.10+" /></a>
  <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" />
  <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.zh-CN.md">中文版</a>
</p>

---

## What is C-Two?

C-Two is a resource-oriented RPC runtime for distributed scientific computation. It exposes stateful objects—simulations, spatial indexes and data processors—across processes and machines. Clients use the same method interface for local and remote resources.

A **CRM contract** declares the callable methods and their namespace/version. A **resource** is a plain class that implements those methods and holds state. A **client** uses `cc.connect(...)` to obtain a typed proxy. A process can host several resources and connect to other resources at the same time.

This README describes **C-Two 0.7.4**, with Python 3.10+ and FastDB 0.2.1.

## Features

- **Local and remote calls:** same-process calls pass Python objects directly; same-host IPC uses Unix domain sockets or Windows Named Pipes; cross-machine calls use HTTP relay. Explicit IPC connections work independently of relay discovery.
- **State and concurrency:** resources keep their state between calls. `@cc.read` and `@cc.write` declare access semantics, with writes as the default. Native route scheduling enforces concurrency and pending-work limits across local and remote invocation.
- **Portable contracts and payloads:** `@cc.transfer(...)` binds FastDB `Payload` specifications. Contract tooling validates descriptors and generates Python, Rust and TypeScript bindings; ordinary Python values also support Python-to-Python calls.
- **Contract-scoped routing:** clients match the route name, CRM identity and contract hashes. Relay discovery selects a verified local IPC path or an HTTP target; `c3` runs the relay and supports relay meshes.
- **Native memory transport:** IPC uses lazy buddy pools, dedicated shared memory, checked chunk transfer and file spill. Budgets and idle reclamation control memory and file usage for large payloads. See [memory policy](docs/memory-policy.en.md).
- **Explicit payload lifetimes:** `cc.hold()` retains response leases, while registration can select borrowed input lifetimes. Release invalidates FastDB owners and checked views before releasing transport storage. See [Python payload usage](docs/python-usage.md).
- **Resource lifecycle management:** Resources use `Persistent` or controller-owned `OwnerBound` lifecycles; native shutdown drains work, and scoped endpoint maintenance cleans up confirmed exits. See [lifecycle integration](docs/local-endpoint-lifecycle.en.md).
- **Shared Rust core:** Python and Rust use the same routing, transport, memory, error and lifecycle mechanisms. The native c3 CLI handles relay, contract tools and endpoint maintenance on Linux, macOS and Windows x64.

## Install

```bash
uv pip install c-two
```

The same command works in Windows PowerShell. For c3 on Linux or macOS:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-installer.sh | sh -s -- --version 0.3.3
```

On Windows x64, use the [executable](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-x86_64-pc-windows-msvc.exe) or [PowerShell installer](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.3/c3-installer.ps1). Installation and checksum verification are in the [Windows guide](docs/windows-native-usage.md#install-python-and-c3).

For source builds and tests, see the [development guide](docs/development.md).

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

Run with `python counter.py`. This example uses the same-process path; keep `C2_RELAY_ANCHOR_ADDRESS` unset for local registration.

For another process on the same host, pass the resource process's `cc.server_address()` as the client's `address`. The [IPC example](examples/python/ipc_resource.py) shows both sides.

For relay routing, start `c3 relay --bind 0.0.0.0:8300`, set `cc.set_relay_anchor('http://relay-host:8300')` on both sides, then connect by name. The relay is a separate process. Explicit IPC addresses bypass relay discovery.

## Examples and documentation

| Topic | Guide |
| --- | --- |
| Local calls | [local.py](examples/python/local.py) |
| Direct IPC | [resource](examples/python/ipc_resource.py), [client](examples/python/ipc_client.py) |
| Relay | [resource](examples/python/relay_resource.py), [client](examples/python/relay_client.py), [mesh](examples/python/relay_mesh/README.md) |
| Configuration: code, environment and `.env` | [Configuration guide](docs/configuration.en.md) |
| Python contracts, portable payloads and hold | [Python SDK guide](docs/python-usage.md) |
| Rust SDK | [Rust SDK guide](sdk/rust/README.md) |
| Relay and contract tools | [c3 CLI guide](cli/README.md) |
| Endpoint directories and lifecycle | [Lifecycle guide](docs/local-endpoint-lifecycle.en.md) |
| Windows | [Build and usage](docs/windows-native-usage.md) |
| Environment variables | [.env.example](.env.example) |
| Development | [Build and tests](docs/development.md), [contributing](CONTRIBUTING.md) |

## License

[MIT](LICENSE).
