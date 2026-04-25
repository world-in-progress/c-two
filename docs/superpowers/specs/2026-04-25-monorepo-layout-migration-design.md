# Monorepo Layout Migration Design

**Date**: 2026-04-25  
**Status**: Approved for implementation planning  
**Scope**: Repository layout migration only. No new Fortran or TypeScript-browser runtime support in this phase.

## Problem

C-Two is currently structured as a Python package with a Rust native workspace nested under `src/c_two/_native/`. This made sense while Python was the only supported SDK, but it creates the wrong ownership model for cross-language support:

- The Rust core appears to be private implementation detail of the Python package.
- Python package metadata, Python source, tests, examples, benchmarks, and Rust crates share one package-oriented root layout.
- Future SDKs would have to be added around a Python-centered structure instead of beside it.

The first cross-language step should therefore be a repository structure migration, not the implementation of new language SDKs.

## Goals

- Move the Rust native workspace to a root-level `core/` directory.
- Move the Python package to `sdk/python/`.
- Keep the published Python package name and import surface stable.
- Preserve existing Python behavior, tests, examples, and benchmark entry points.
- Introduce a small `spec/` area for protocol and compatibility contract material.
- Keep Fortran and TypeScript-browser as follow-up work, not phase 1 implementation work.

## Non-Goals

- Do not implement `sdk/fortran/`.
- Do not implement `sdk/typescript-browser/`.
- Do not redesign the wire format, transfer system, CRM API, or Rust crate boundaries.
- Do not introduce a broad canonical value model.
- Do not move language-independent conformance suites before they exist.

## Target Structure

```text
c-two/
├── core/
│   ├── Cargo.toml
│   ├── foundation/
│   ├── protocol/
│   ├── transport/
│   └── bridge/
├── sdk/
│   └── python/
│       ├── pyproject.toml
│       ├── src/c_two/
│       ├── tests/
│       └── benchmarks/
├── examples/
│   └── python/
├── spec/
│   └── README.md
├── docs/
├── pyproject.toml
└── uv.lock
```

## Directory Responsibilities

### `core/`

`core/` is the canonical Rust implementation. Phase 1 moves the existing `src/c_two/_native/` workspace here and keeps the current internal Rust layout:

```text
core/
├── Cargo.toml
├── foundation/
├── protocol/
├── transport/
└── bridge/
```

The existing crate names remain `c2-*`. The root directory name `core/` describes the repository role, while crate names keep the project namespace and avoid collision with Rust's standard `core` crate.

### `sdk/python/`

`sdk/python/` owns the real Python package:

- `sdk/python/pyproject.toml` contains `[project] name = "c-two"` and maturin build metadata.
- `sdk/python/src/c_two/` contains the Python package.
- `sdk/python/tests/` contains Python SDK tests.
- `sdk/python/benchmarks/` contains Python-specific benchmarks.

The Python import path remains `import c_two`. The native extension module remains `c_two._native`.

### Root `pyproject.toml`

The root `pyproject.toml` becomes a uv workspace and development entry point, not the published `c-two` package:

```toml
[project]
name = "c-two-workspace"
version = "0.0.0"
requires-python = ">=3.10"
dependencies = ["c-two"]

[tool.uv]
package = false

[tool.uv.sources]
c-two = { workspace = true }

[tool.uv.workspace]
members = ["sdk/python"]
```

This keeps root-level development workflows usable while putting Python package ownership in the Python SDK directory.

### `examples/`

Examples stay at the repository root because they are user-facing demonstrations, not Python package internals. They are grouped by language:

```text
examples/
└── python/
```

Future language examples can be added beside Python:

```text
examples/
├── python/
├── fortran/
├── typescript-browser/
└── cross-language/
```

Phase 1 only moves existing examples to `examples/python/`.

### `spec/`

`spec/` is the source of compatibility contracts, not a runtime implementation layer. Phase 1 adds only a minimal README to establish ownership.

Future content may include:

- C-Two wire ABI notes
- Error code definitions
- Capability matrix
- Golden fixtures
- Conformance case descriptions

Runtime implementation stays in `core/`; language adaptation stays in `sdk/*`.

## Configuration Updates

The Python package `pyproject.toml` moves to `sdk/python/pyproject.toml`.

Its maturin configuration points back to the Rust core:

```toml
[tool.maturin]
manifest-path = "../../core/bridge/c2-ffi/Cargo.toml"
python-source = "src"
module-name = "c_two._native"
features = ["pyo3/extension-module"]
```

The uv cache keys must be updated to watch the new paths:

```toml
[tool.uv]
cache-keys = [
    { file = "pyproject.toml" },
    { file = "../../core/**/*.rs" },
    { file = "../../core/**/Cargo.toml" },
]
```

Root-level pytest behavior should continue to work through the workspace package. The root command should explicitly target Python SDK tests:

```bash
uv run pytest sdk/python/tests
```

## Migration Plan

1. Move `src/c_two/_native/` to `core/`.
2. Move `src/c_two/` to `sdk/python/src/c_two/`.
3. Move `tests/` to `sdk/python/tests/`.
4. Move `benchmarks/` to `sdk/python/benchmarks/`.
5. Move existing example files and directories to `examples/python/`.
6. Move Python package metadata from root `pyproject.toml` to `sdk/python/pyproject.toml`.
7. Replace root `pyproject.toml` with a uv workspace/development entry point.
8. Add `spec/README.md`.
9. Update path references in docs, tests, examples, benchmarks, and build configuration.

## Validation

Phase 1 is complete only if the migrated layout passes these checks from the repository root:

```bash
uv run python -c "import c_two; print(c_two.__version__)"
uv run pytest sdk/python/tests
cd core && cargo check --workspace
uv run python examples/python/client.py
```

At least one lightweight benchmark should also be smoke-tested to verify imports and helper paths still work after migration. Full benchmark matrix execution is not required for the layout phase.

## Risks and Mitigations

### uv workspace and maturin path handling

Moving `pyproject.toml` changes relative paths for maturin and uv cache keys. The migration should validate local editable builds through `uv run python -c "import c_two"` before running the full test suite.

### Broken hard-coded paths

Tests, benchmarks, docs, and examples may refer to `src/c_two`, `tests`, `examples`, or `benchmarks` directly. The implementation should update these references mechanically and verify with `rg` after the move.

### Python package behavior regression

The Python SDK remains the only functional SDK in phase 1. The implementation must preserve public imports, CLI entry point `c3`, and native extension module name `c_two._native`.

### Premature cross-language scaffolding

Creating empty `sdk/fortran/` or `sdk/typescript-browser/` directories would imply implementation progress that is out of scope. These directories should not be added in phase 1.

## Follow-Up Phases

After the zero-regression layout migration:

1. Define the minimal `spec/` contract needed for cross-language conformance.
2. Add `sdk/fortran/` as a client-only SDK through `ISO_C_BINDING` and a C ABI surface.
3. Add `sdk/typescript-browser/` as a browser client SDK, initially using HTTP/fetch relay transport and optionally WASM for reusable core wire functionality.
