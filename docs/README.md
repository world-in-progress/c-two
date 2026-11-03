# Documentation index

This page routes each subject to the document that owns it. It states no behavior of its own: current behavior lives in `reference/`, open work and plans in `design/`, and verification in `acceptance/`.

## Entry points

| Document | Role |
| --- | --- |
| [README.md](../README.md) | English entry point: what C-Two does, installation and a first resource |
| [README.zh-CN.md](../README.zh-CN.md) | Chinese entry point with the same content |
| [CONTEXT.md](../CONTEXT.md) | The glossary: one word per concept, with its Chinese and English names and the words to avoid |
| [AGENTS.md](../AGENTS.md) | Instructions for coding agents: architecture map, commands, invariants, retired interfaces and the table of documents to read before changing each path |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | How to contribute and validate a change locally |
| [CHANGELOG.md](../CHANGELOG.md) | Release history; per-release notes are in [releases/](releases/) |

## Repository layout

| Path | Contents |
| --- | --- |
| `core/` | The language-neutral Rust workspace: foundation crates (contract, codegen, config, error, memory and the C ABI), the wire protocol, transports (local endpoints, IPC client and server, HTTP and relay) and the `c2-core` runtime |
| `sdk/python/` | The Python SDK, `c_two`, with its PyO3 extension in `native/` |
| `sdk/rust/` | The user-facing Rust SDK, the `c-two` crate imported as `c_two` |
| `cli/` | The `c3` command, including `c3 relay` |
| `examples/` | Runnable Python examples |
| `tests/repo/` | Repository checks: release tooling, CI scope and documentation hygiene |
| `tools/` | Development, CI, benchmark and release tools that run from a source checkout |

## Current references

| Document | Role |
| --- | --- |
| [Python usage](reference/python-usage.md) ([中文](reference/python-usage.zh-CN.md)) | CRM contracts, resources, clients, Runtime, hold, FastDB payloads and observation from Python |
| [Configuration](reference/configuration.md) ([中文](reference/configuration.zh-CN.md)) | Every setting, its environment variable and default, precedence, call deadlines and endpoint directories |
| [Windows usage](reference/windows-native-usage.md) ([中文](reference/windows-native-usage.zh-CN.md)) | Installing and building on Windows x64 and ARM64 |
| [Contracts and routing](reference/contracts-and-routing.md) | Contract identity, portable contracts, route catalogs and bindings, relay discovery, forwarding and trust |
| [Runtime lifecycle](reference/runtime-lifecycle.md) | Registration, startup, execution admission, connection deadlines, shutdown and retirement |
| [Payload and buffer lifetime](reference/payload-lifetime.md) | Held responses, borrowed inputs, FastDB view invalidation and the dispatch copy path |
| [Memory policy](reference/memory-policy.md) | Budgets, buddy and dedicated SHM, file spill, fallback, memory pressure and statistics |
| [Local endpoint lifecycle](reference/local-endpoint-lifecycle.md) | Endpoint namespaces, credentials, bind and connect failures, cleanup and sweeps, admin shutdown and connection observation |
| [IPC handshake](reference/ipc-handshake.md) | The fixed version preamble, negotiation and connection roles |
| [Errors](reference/errors.md) | Error codes, the wire codec and diagnostic fields |
| [C ABI](reference/c-abi.md) | The `c2-mem-ffi` ABI and its consumers |
| [Windows native implementation](reference/windows-native-implementation.md) | Named pipes, mappings, identity checks and Windows-specific limits |
| [Development](reference/development.md) | Building and testing from source, and the local release-candidate qualification |
| [Local development testing](reference/local-development-testing.md) | The local test runner and its sidecar rules |
| [CI and release qualification](reference/ci-cd.md) | CI lanes, scope detection and release gates |

## Open work and plans

| Document | Role |
| --- | --- |
| [Backlog](design/backlog.md) | The only list of open work and pending decisions |
| [Roadmap](design/roadmap.md) ([中文](design/roadmap.zh-CN.md)) | Direction of the 0.x line and exit criteria of the product capabilities; their order is in the backlog |
| [Endgame architecture](design/endgame-architecture.md) | The long-range architecture the roadmap works toward |
| [Contract-release deferred capabilities](design/contract-release-deferred-capabilities.md) | Capabilities deliberately left out of the portable contract release, with their exit conditions |

The dated plans and reviews in `design/` belong to work still in progress; each states its own status, and the backlog links to them.

## Decisions

`decisions/` holds one ADR per decision; later changes add dated notes instead of rewriting it.

| Decision | Subject |
| --- | --- |
| [0001](decisions/0001-single-owner-release.md) | Single-owner release of shared memory |
| [0002](decisions/0002-remove-owner-subsystem.md) | Removal of the owner subsystem |

## Records

- `acceptance/` holds the verification records of completed work, release qualification reports and reviews, with frozen manifests, receipts and logs under `acceptance/evidence/`. Records are not rewritten; later changes update their links only.
- `research/` holds notes made before a decision: surveys of other projects and feasibility studies.
- `releases/` holds the notes for each release; `unreleased.md` collects the changes since the last one.
