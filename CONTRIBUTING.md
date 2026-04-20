# Contributing to C-Two

Thanks for your interest in contributing! C-Two is a resource-oriented RPC framework with a Python front-end and Rust native layer. This guide covers everything you need to get started.

## Dev Setup

**Prerequisites:** Python ≥ 3.10, Rust toolchain, [uv](https://docs.astral.sh/uv/) package manager.

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
uv sync  # installs dependencies and compiles the Rust native extension
```

To force-rebuild after Rust source changes:

```bash
uv sync --reinstall-package c-two
```

## Running Tests

```bash
# Full Python test suite (set C2_RELAY_ADDRESS= to avoid env interference)
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30

# Single file / single test
uv run pytest tests/unit/test_encoding.py -q
uv run pytest tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Rust type-check
cd src/c_two/_native && cargo check --workspace

# Rust unit tests (pure Rust crates only — c2-ffi needs Python linkage)
cd src/c_two/_native && cargo test -p c2-mem -p c2-wire
```

All new code **must** include tests. Ensure the full suite passes before opening a PR.

## Submitting Changes

1. **Fork** the repository and clone your fork.
2. **Create a branch** from `main` with a descriptive name (e.g., `feat/grid-pagination`, `fix/shm-leak`).
3. Make your changes in small, focused commits.
4. Push to your fork and **open a Pull Request** against `main`.
5. Describe *what* and *why* in the PR body. Link related issues if applicable.

## Commit Messages

We encourage [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add chunked transfer support for large payloads
fix: prevent double-free in buddy allocator
docs: update CRM contract examples
test: add integration test for relay mesh
refactor: simplify MethodTable index lookup
```

## Code Style

### Python

- **Import alias:** always `import c_two as cc`.
- **Type hints required** — use modern syntax (`list[int]`, `str | None`). No `from __future__` needed (Python ≥ 3.10).
- **CRM contracts:** plain class names, **no `I` prefix** (`Grid`, not `IGrid`). Method bodies are `...` (ellipsis).
- **Resource classes:** named by domain semantics (`NestedGrid`), not by the contract.
- **Transferable types:** do **not** add `@staticmethod` to `serialize`/`deserialize`/`from_buffer` — the metaclass handles it.
- Follow existing formatting conventions in the file you're editing.

### Rust

- Code lives in `src/c_two/_native/` (a Cargo workspace of 7 crates).
- Run `cargo check --workspace` and `cargo test -p c2-mem -p c2-wire` before submitting.
- Prefer zero-copy and single-allocation patterns in wire/transport code.

## Project Structure

```
src/c_two/
├── crm/          # CRM contracts, @transferable, decorators
├── transport/    # Registry, proxy, server bridge, scheduler
├── config/       # Settings (pydantic), IPC config
├── mem/          # Python wrappers for Rust memory subsystem
├── _native/      # Rust workspace (c2-mem, c2-wire, c2-ipc, c2-server, c2-http, c2-config, c2-ffi)
└── cli.py        # `c3` CLI entry point
tests/
├── unit/
├── integration/
└── fixtures/     # Shared test CRM contracts and resources
```

## Environment Variables

Tests may be affected by `C2_*` environment variables. Always prefix test runs with `C2_RELAY_ADDRESS=` to isolate from your local environment. See `.env.example` for the full reference.

## Questions?

Open an issue or start a discussion on the repository. We're happy to help!
