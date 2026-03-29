# README Restructure Design Spec

**Date**: 2025-07-26
**Scope**: Rewrite README.md (English, primary) + create README.zh-CN.md (Chinese mirror)
**Constraint**: No "SOTA" terminology. No MCP/Seed/CRM Template sections.

## 1. Guiding Principles

- **Audience**: Both scientific/HPC developers and general Python backend developers; lean scientific
- **Tone**: Professional, concise, code-first. Let the API speak.
- **Language**: English primary (`README.md`), Chinese mirror (`README.zh-CN.md`), cross-linked
- **API**: Only show new API (`cc.register`/`cc.connect`/`cc.close`). Legacy API not mentioned.
- **Architecture**: Retain current level of detail from existing README, minus removed features

## 2. Document Structure

### README.md (English)

```
1. Hero           — logo, one-line description, badges, link to 中文版
2. Why C-Two?     — 4 bullet points (core value proposition)
3. Quick Start    — pip install → define resource → use locally → use remotely
4. Core Concepts  — ICRM / CRM / Component / @transferable (concise, with code)
5. Examples       — Progressive: local → IPC → HTTP relay
6. Architecture   — Three-tier deep dive + transport protocol table + Rust layer
7. Installation   — pip install + development setup (uv sync)
8. Roadmap        — Minimal status table + link to full roadmap
9. License        — MIT
```

### README.zh-CN.md (Chinese)

Identical structure, fully translated. Header links to English version.

## 3. Section Details

### §1 Hero

```markdown
<p align="center">
  <img src="docs/images/c2_logo_transparent.png" width="200">
</p>

<h1 align="center">C-Two</h1>
<p align="center">
  A resource-oriented RPC framework for Python — turn stateful classes
  into location-transparent distributed resources.
</p>
<p align="center">
  [PyPI badge] [Python version badge] [Free-threading 3.14t badge] [License badge]
</p>

> [中文版](README.zh-CN.md)
```

Badges:
- PyPI version: `https://img.shields.io/pypi/v/c-two`
- Python versions: `https://img.shields.io/pypi/pyversions/c-two`
- Free-threading: static badge `3.14t`
- License: `https://img.shields.io/github/license/world-in-progress/c-two`

### §2 Why C-Two?

Four bullet points, no jargon:

1. **Resources, not services** — C-Two doesn't expose RPC endpoints. It makes Python classes remotely accessible while preserving their stateful, object-oriented nature.
2. **Zero-copy when local, transparent when remote** — Same-process calls skip serialization entirely. Cross-process calls use shared memory. Cross-machine calls go over HTTP. Your code doesn't change.
3. **Built for scientific Python** — Native support for Apache Arrow, NumPy arrays, and large payloads (chunked streaming for data beyond 256 MB). Designed for computational workloads, not microservices.
4. **Rust-powered transport** — The IPC layer uses a Rust buddy allocator for shared memory and a Rust HTTP relay for high-throughput networking. Python-friendly API, native performance where it counts.

### §3 Quick Start

Minimal example showing the full workflow in ~25 lines total:

1. Define ICRM interface (5 lines)
2. Implement CRM class (8 lines)
3. Use locally with `cc.register()` + `cc.connect()` (5 lines)
4. Switch to remote with `cc.set_address()` (show diff from local — just 1 line changes)

Use a simple `Counter` example (increment/get) — universally understandable, no domain knowledge needed.

### §4 Core Concepts

Three subsections, each with a short paragraph + code snippet:

**ICRM (Interface)**
- Decorated with `@cc.icrm(namespace='...', version='...')`
- Method bodies are `...` (pure interface declaration)
- Methods can be annotated `@cc.read` or `@cc.write` for concurrency control
- ~5 lines of code

**CRM (Core Resource Model)**
- Plain Python class implementing the interface
- Holds state, implements domain logic
- Not decorated — decoupled from framework
- ~8 lines of code

**Component (Consumer)**
- `cc.connect(ICRMClass, name='...')` returns a typed proxy
- Proxy is location-transparent (works same locally and remotely)
- ~3 lines of code

**@transferable (Custom Serialization)**
- For custom data types crossing the wire
- `serialize` and `deserialize` methods (auto-converted to staticmethod)
- Without it, pickle is used as fallback
- ~6 lines of code

### §5 Progressive Examples

Three examples showing deployment progression:

**Example 1: Single Process**
- `cc.register()` + `cc.connect()` in same process
- Thread preference: zero serialization, direct method calls
- Use case: local prototyping, testing, single-machine computation
- Source: adapted from `examples/local.py`
- ~15 lines

**Example 2: Multi-Process (IPC)**
- Server: `cc.set_address()` + `cc.register()`
- Client: `cc.connect(address='ipc://...')`
- Use case: multi-process on same host, worker isolation
- Source: adapted from `examples/server.py` + `examples/client.py`
- ~20 lines (split server/client)

**Example 3: HTTP Relay (Cross-Machine)**
- Server registers CRMs
- Relay bridges HTTP ↔ IPC
- Client connects via HTTP URL
- Use case: network-accessible services, web integration
- Source: adapted from `examples/relay/`
- ~25 lines (split server/relay/client)

Each example ends with: `> **Best for:** <one-line use case description>`

### §6 Architecture

Retain and clean up from current README:

**Overview paragraph** — Three-tier design philosophy (not services, but resources). Each tier has a clear responsibility with well-defined interfaces.

**Three-tier diagram** — Mermaid diagram showing:
```
Component ←→ ICRM Interface ←→ Transport ←→ CRM
```

**Component Layer** (`c_two/compo/`)
- Script-based: `cc.connect()` context manager
- Function-based: `@cc.runtime.connect` decorator
- Brief description, 1 code example

**CRM Layer** (`c_two/crm/`)
- ICRM: interface declaration with `@cc.icrm()`
- CRM: plain implementation class
- `@transferable`: custom serialization
- `@cc.read` / `@cc.write`: concurrency annotations

**Transport Layer** (`c_two/transport/`)
- Protocol table:

| Scheme | Transport | Use case |
|--------|-----------|----------|
| `thread://` | In-process | Zero serialization, testing |
| `ipc:///path` | Unix domain socket + SHM | Multi-process, same host |
| `http://host:port` | HTTP relay | Cross-machine, web-compatible |

- Note: protocol is auto-detected from address prefix

**Rust Native Layer** (`c_two/_native/`)
- Buddy allocator: zero-syscall SHM allocation for IPC
- HTTP relay: high-throughput axum-based HTTP ↔ IPC bridge
- Compiled automatically via maturin during `pip install` or `uv sync`

**Remove from current README:**
- MCP Integration section
- Seed/CLI section (c3 build)
- CRM Template functionality
- All "SOTA API" / "Wire v2" / "Handshake v5" labels
- "What's New in v0.2.7" section
- Legacy API examples (`cc.rpc.Server`, `cc.rpc.ServerConfig`)
- Event system section

### §7 Installation & Development

**Install from PyPI:**
```bash
pip install c-two
```

Requires Python ≥ 3.10. Pre-built wheels available for Linux (x86_64, aarch64) and macOS (x86_64, aarch64). Free-threading (Python 3.14t) supported.

**Development setup:**
```bash
git clone https://github.com/world-in-progress/c-two
cd c-two
uv sync       # installs deps + compiles Rust extensions
uv run pytest  # run tests
```

Requires Rust toolchain for building native extensions.

### §8 Roadmap

Minimal status table:

| Feature | Status |
|---------|--------|
| Core RPC framework (CRM/ICRM/Component) | ✅ Stable |
| IPC transport with SHM buddy allocator | ✅ Stable |
| HTTP relay (Rust-powered) | ✅ Stable |
| Chunked streaming (payloads > 256 MB) | ✅ Stable |
| Heartbeat & connection management | ✅ Stable |
| CI/CD & multi-platform PyPI publishing | ✅ Stable |
| Read/write concurrency control | ✅ Stable |
| Async interfaces | 🔜 Planned |
| Disk spill for extreme payloads | 🔜 Planned |
| Cross-language clients (Rust/C++) | 🔮 Future |

Link: `See [full roadmap](docs/plans/c-two-rpc-v2-roadmap.md) for details.`

### §9 License

MIT — one line.

## 4. Files to Create/Modify

| File | Action |
|------|--------|
| `README.md` | Rewrite entirely |
| `README.zh-CN.md` | Create (Chinese translation) |

## 5. Content to Remove

From the current README, the following sections/concepts are **explicitly excluded**:

1. "What's New in v0.2.7" — version changelog doesn't belong in README
2. "SOTA API" label — just call it the API, no special naming
3. Legacy API examples — `cc.rpc.Server()`, `cc.rpc.ServerConfig()`, `cc.compo.runtime.connect_crm()`
4. MCP Integration — experimental, not core
5. Seed/CLI (`c3 build`) — peripheral tool
6. CRM Template — unvalidated feature
7. Event system — internal/legacy
8. Wire protocol details — too internal for README
9. "Wire v2", "Handshake v5", "Protocol v3.1" labels — already cleaned from code, clean from docs too
10. WIP section — move to roadmap or remove

## 6. Architecture Diagram Style

Use Mermaid for the main architecture diagram (renders natively on GitHub). Retain any existing images in `docs/images/` that are still relevant but don't depend on external image hosting.

## 7. Quality Checklist

Before finalizing:
- [ ] No "SOTA" anywhere in the document
- [ ] No MCP/Seed/Template references
- [ ] No legacy API shown
- [ ] No internal protocol version labels (Wire v2, Handshake v5, etc.)
- [ ] All code examples are runnable (based on actual `examples/` files)
- [ ] Chinese version is complete and structurally identical
- [ ] Cross-links between EN and CN versions work
- [ ] Badges point to correct PyPI/GitHub URLs
