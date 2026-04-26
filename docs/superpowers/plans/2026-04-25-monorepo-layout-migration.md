# Monorepo Layout Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move C-Two from a Python-package-centered layout to a `core/` + `sdk/python/` monorepo layout without regressing the existing Python SDK.

**Architecture:** The Rust workspace moves from `src/c_two/_native/` to `core/` with crate names and internal crate grouping unchanged. The real Python package moves to `sdk/python/`, while the repository root becomes a uv workspace/development entry point. Examples remain at the root under `examples/python/` because they are user-facing demos rather than Python package internals.

**Tech Stack:** Python 3.10+, uv workspace, maturin/PyO3, pytest, Rust/Cargo workspace.

---

## Reference Spec

Implement the approved design in:

`docs/superpowers/specs/2026-04-25-monorepo-layout-migration-design.md`

## File Structure

### Create

| Path | Responsibility |
|---|---|
| `sdk/python/pyproject.toml` | Real Python package metadata and maturin configuration for published `c-two` package |
| `sdk/python/README.md` | Python SDK-specific install, test, examples, and benchmark notes |
| `sdk/python/LICENSE` | Package-local copy of the root MIT license for Python distribution metadata |
| `sdk/python/src/c_two/` | Moved Python package source |
| `sdk/python/tests/` | Moved Python SDK tests |
| `sdk/python/benchmarks/` | Moved Python-specific benchmarks |
| `examples/python/` | Moved Python examples |
| `core/` | Moved Rust workspace |

### Modify

| Path | Responsibility |
|---|---|
| `pyproject.toml` | Root uv workspace/development entry point, not the published Python package |
| `uv.lock` | Updated uv lockfile after workspace split |
| `.github/copilot-instructions.md` | Active agent/developer instructions with new paths |
| `CONTRIBUTING.md` | Contributor commands and project structure |
| `README.md` | Repository-level paths to examples and benchmarks |
| `README.zh-CN.md` | Repository-level paths to examples and benchmarks |
| `sdk/python/src/c_two/cli.py` | Dev environment detection and banner output path after source move |
| `sdk/python/tests/README.md` | Test and benchmark commands after tests/benchmarks move |
| `examples/python/*.py` | Remove stale `../src` path assumptions and point grid imports at `examples/python/` |
| `examples/python/relay_mesh/*.py` | Remove stale `../src` path assumptions and point grid imports at `examples/python/` |
| `examples/python/relay_mesh/README.md` | New example command paths |
| `sdk/python/benchmarks/*.py` | Benchmark command docstrings and relative output paths |
| `sdk/python/benchmarks/run_kostya_sweep.sh` | Benchmark script paths |

### Do Not Create

Do not create `spec/`, `sdk/fortran/`, or `sdk/typescript-browser/` in this phase.

---

## Task 1: Baseline Verification

**Files:**
- Read: `pyproject.toml`
- Read: `src/c_two/_native/Cargo.toml`
- Read: `tests/`
- Read: `examples/`
- Read: `benchmarks/`

- [ ] **Step 1: Confirm the worktree starts clean**

Run:

```bash
git status --short
```

Expected: no output.

- [ ] **Step 2: Verify the current Python package imports**

Run:

```bash
uv run python -c "import c_two as cc; print(cc.__version__)"
```

Expected: prints `0.4.7`.

- [ ] **Step 3: Verify the current Rust workspace**

Run:

```bash
cd src/c_two/_native && cargo check --workspace
```

Expected: command exits successfully.

- [ ] **Step 4: Verify the current Python test harness**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest tests/unit/test_check_version.py tests/unit/test_wire.py -q --timeout=30
```

Expected: selected tests pass.

- [ ] **Step 5: Verify a current lightweight example**

Run:

```bash
uv run python examples/local.py
```

Expected: command prints successful local CRM calls and exits with status 0.

- [ ] **Step 6: Commit only if baseline artifacts changed**

No commit is expected for this task. If a baseline command updates generated files unexpectedly, inspect them with:

```bash
git status --short
git diff --stat
```

Expected: no tracked file changes.

---

## Task 2: Move the Rust Workspace to `core/`

**Files:**
- Move: `src/c_two/_native/` -> `core/`
- Modify: `pyproject.toml`

- [ ] **Step 1: Move the Rust workspace**

Run:

```bash
git mv src/c_two/_native core
```

Expected: `core/Cargo.toml` exists and `src/c_two/_native/` no longer exists.

- [ ] **Step 2: Temporarily update root maturin paths**

In `pyproject.toml`, replace the existing `[tool.maturin]` and `[tool.uv]` sections with:

```toml
[tool.maturin]
manifest-path = "core/bridge/c2-ffi/Cargo.toml"
module-name = "c_two._native"
python-source = "src"
features = ["pyo3/extension-module"]
include = ["LICENSE"]

[tool.uv]
package = true
cache-keys = [
    { file = "pyproject.toml" },
    { file = "core/**/*.rs" },
    { file = "core/**/Cargo.toml" },
]
```

Keep the rest of `pyproject.toml` unchanged in this task. The root project is still the Python package until Task 3.

- [ ] **Step 3: Verify Cargo paths**

Run:

```bash
cd core && cargo check --workspace
```

Expected: command exits successfully.

- [ ] **Step 4: Verify maturin still builds the extension from root**

Run:

```bash
uv sync --reinstall-package c-two
uv run python -c "import c_two as cc; print(cc.__version__)"
```

Expected: import succeeds and prints `0.4.7`.

- [ ] **Step 5: Search for stale native workspace references in active config**

Run:

```bash
rg -n "src/c_two/_native" pyproject.toml .github/copilot-instructions.md CONTRIBUTING.md README.md README.zh-CN.md
```

Expected: references remain in docs at this point; `pyproject.toml` should not be listed.

- [ ] **Step 6: Commit the core move**

Run:

```bash
git add core pyproject.toml
git commit -m "refactor: move rust core workspace"
```

Expected: commit succeeds.

---

## Task 3: Move the Python SDK Into `sdk/python/`

**Files:**
- Move: `src/c_two/` -> `sdk/python/src/c_two/`
- Move: `tests/` -> `sdk/python/tests/`
- Move: `benchmarks/` -> `sdk/python/benchmarks/`
- Create: `sdk/python/pyproject.toml`
- Create: `sdk/python/README.md`
- Create: `sdk/python/LICENSE`
- Replace: `pyproject.toml`
- Modify: `uv.lock`

- [ ] **Step 1: Create Python SDK directories**

Run:

```bash
mkdir -p sdk/python/src
```

Expected: `sdk/python/src/` exists.

- [ ] **Step 2: Move Python package source, tests, and benchmarks**

Run:

```bash
git mv src/c_two sdk/python/src/c_two
git mv tests sdk/python/tests
git mv benchmarks sdk/python/benchmarks
```

Expected:

```bash
test -d sdk/python/src/c_two
test -d sdk/python/tests
test -d sdk/python/benchmarks
test ! -e src/c_two
test ! -e tests
test ! -e benchmarks
```

- [ ] **Step 3: Write the Python package `pyproject.toml`**

Create `sdk/python/pyproject.toml` with:

```toml
[project]
name = "c-two"
version = "0.4.7"
description = "C-Two is a resource-RPC framework that enables resource-oriented classes to be remotely invoked across different processes or machines."
readme = "README.md"
license-files = ["LICENSE"]
requires-python = ">=3.10"
classifiers = [
    "Programming Language :: Python :: 3",
    "Programming Language :: Python :: 3.10",
    "Programming Language :: Python :: 3.11",
    "Programming Language :: Python :: 3.12",
    "Programming Language :: Python :: 3.13",
    "Programming Language :: Python :: 3.14",
    "Programming Language :: Rust",
    "License :: OSI Approved :: MIT License",
    "Operating System :: POSIX",
    "Operating System :: MacOS",
    "Topic :: Scientific/Engineering",
    "Topic :: System :: Distributed Computing",
]
dependencies = [
    "click>=8.2.1",
    "pydantic-settings>=2.0.0",
]

[build-system]
requires = ["maturin>=1.7,<2.0"]
build-backend = "maturin"

[tool.maturin]
manifest-path = "../../core/bridge/c2-ffi/Cargo.toml"
module-name = "c_two._native"
python-source = "src"
features = ["pyo3/extension-module"]
include = ["LICENSE"]

[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py"]
python_classes = ["Test*"]
python_functions = ["test_*"]
timeout = 30

[tool.uv]
package = true
cache-keys = [
    { file = "pyproject.toml" },
    { file = "../../core/**/*.rs" },
    { file = "../../core/**/Cargo.toml" },
]

[project.scripts]
c3 = "c_two.cli:cli"
```

- [ ] **Step 4: Replace the root `pyproject.toml` with the uv workspace entry**

Replace root `pyproject.toml` with:

```toml
[project]
name = "c-two-workspace"
version = "0.0.0"
description = "Workspace root for C-Two."
readme = "README.md"
requires-python = ">=3.10"
dependencies = [
    "c-two",
]

[dependency-groups]
dev = [
    "httpx>=0.28.1",
    "numpy>=2.2.4",
    "pillow>=12.1.1",
    "pyarrow>=23.0.1",
    "pytest>=8.0.0",
    "pytest-timeout>=2.0.0",
]
examples = [
    "numpy>=2.2.4",
    "pandas>=2.2.0",
    "pyarrow>=23.0.1",
]

[tool.uv]
package = false
cache-keys = [
    { file = "pyproject.toml" },
    { file = "sdk/python/pyproject.toml" },
    { file = "core/**/*.rs" },
    { file = "core/**/Cargo.toml" },
]

[tool.uv.sources]
c-two = { workspace = true }

[tool.uv.workspace]
members = ["sdk/python"]

[tool.pytest.ini_options]
testpaths = ["sdk/python/tests"]
python_files = ["test_*.py"]
python_classes = ["Test*"]
python_functions = ["test_*"]
timeout = 30
```

- [ ] **Step 5: Create the Python SDK license copy**

Copy the root license into the Python package directory:

```bash
cp LICENSE sdk/python/LICENSE
git add sdk/python/LICENSE
```

Expected: `sdk/python/LICENSE` matches the root `LICENSE`.

- [ ] **Step 6: Create the Python SDK README**

Create `sdk/python/README.md` with:

````markdown
# C-Two Python SDK

This directory contains the published Python package for C-Two.

## Development

From the repository root:

```bash
uv sync
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests -q --timeout=30
```

Run Rust checks from the repository root:

```bash
cd core && cargo check --workspace
```

## Examples

Python examples live under `../../examples/python/`:

```bash
uv run python examples/python/local.py
```

## Benchmarks

Python-specific benchmarks live in `benchmarks/`:

```bash
C2_RELAY_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
```
````

- [ ] **Step 7: Refresh the uv lockfile**

Run:

```bash
uv lock
```

Expected: command exits successfully and updates `uv.lock` if workspace metadata changed.

- [ ] **Step 8: Rebuild and verify import from root**

Run:

```bash
uv sync --reinstall-package c-two
uv run python -c "import c_two as cc; print(cc.__version__)"
```

Expected: import succeeds and prints `0.4.7`.

- [ ] **Step 9: Run focused tests from the new location**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_check_version.py sdk/python/tests/unit/test_wire.py -q --timeout=30
```

Expected: selected tests pass.

- [ ] **Step 10: Commit the Python SDK move**

Run:

```bash
git add pyproject.toml uv.lock sdk/python
git commit -m "refactor: move python sdk into sdk/python"
```

Expected: commit succeeds.

---

## Task 4: Move Python Examples Under `examples/python/`

**Files:**
- Move: `examples/client.py` -> `examples/python/client.py`
- Move: `examples/crm_process.py` -> `examples/python/crm_process.py`
- Move: `examples/local.py` -> `examples/python/local.py`
- Move: `examples/relay_client.py` -> `examples/python/relay_client.py`
- Move: `examples/grid/` -> `examples/python/grid/`
- Move: `examples/relay_mesh/` -> `examples/python/relay_mesh/`
- Modify: `examples/python/*.py`
- Modify: `examples/python/relay_mesh/*.py`
- Modify: `examples/python/relay_mesh/README.md`

- [ ] **Step 1: Create language-scoped example directory**

Run:

```bash
mkdir -p examples/python
```

Expected: `examples/python/` exists.

- [ ] **Step 2: Move existing examples**

Run:

```bash
git mv examples/client.py examples/python/client.py
git mv examples/crm_process.py examples/python/crm_process.py
git mv examples/local.py examples/python/local.py
git mv examples/relay_client.py examples/python/relay_client.py
git mv examples/grid examples/python/grid
git mv examples/relay_mesh examples/python/relay_mesh
```

Expected: old top-level Python example files are gone and the moved files exist under `examples/python/`.

- [ ] **Step 3: Update root-level Python examples to stop assuming `../src`**

In `examples/python/client.py`, replace the two `sys.path.insert(...)` lines with:

```python
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))
```

In `examples/python/crm_process.py`, replace the two `sys.path.insert(...)` lines with:

```python
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))
```

In `examples/python/relay_client.py`, replace the two `sys.path.insert(...)` lines with:

```python
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))
```

In `examples/python/local.py`, remove the `import os, sys` line and the `sys.path.insert(...)` line because `c_two` should come from the installed workspace package.

- [ ] **Step 4: Update relay mesh examples to point at `examples/python/`**

In both `examples/python/relay_mesh/client.py` and `examples/python/relay_mesh/resource.py`, replace the `_examples_dir` setup block with:

```python
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXAMPLES_ROOT))
```

Keep `import c_two as cc` below the path setup.

In `examples/python/relay_mesh/resource.py`, also replace the broken import pair:

```python
from grid.grid_contract import Grid
from grid.nested_grid import Grid
```

with:

```python
from grid.grid_contract import Grid
from grid.nested_grid import NestedGrid
```

This preserves the intended example behavior while fixing an existing typo that prevents the resource script from importing.

- [ ] **Step 5: Update relay mesh README commands**

In `examples/python/relay_mesh/README.md`, replace command paths with:

```markdown
# Terminal 2 — start the Grid CRM (auto-registers with relay)
uv run python examples/python/relay_mesh/resource.py

# Terminal 3 — client discovers and uses Grid via relay
uv run python examples/python/relay_mesh/client.py
```

Also replace `examples/grid/` text references with `examples/python/grid/`.

- [ ] **Step 6: Verify lightweight example from the new location**

Run:

```bash
uv run python examples/python/local.py
```

Expected: command prints successful local CRM calls and exits with status 0.

- [ ] **Step 7: Verify grid example importability**

Run:

```bash
uv run python - <<'PY'
from pathlib import Path
import sys
sys.path.insert(0, str(Path("examples/python").resolve()))
from grid.grid_contract import Grid
from grid.nested_grid import NestedGrid
print(Grid.__name__, NestedGrid.__name__)
PY
uv run python - <<'PY'
import importlib.util
from pathlib import Path

path = Path("examples/python/relay_mesh/resource.py")
spec = importlib.util.spec_from_file_location("relay_mesh_resource_smoke", path)
module = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(module)
print(module.Grid.__name__, module.NestedGrid.__name__)
PY
```

Expected: first smoke prints `Grid NestedGrid`; second smoke also prints `Grid NestedGrid` without starting the server.

- [ ] **Step 8: Commit the example move**

Run:

```bash
git add examples
git commit -m "refactor: move python examples under examples/python"
```

Expected: commit succeeds.

---

## Task 5: Update Active Tooling and Documentation Paths

**Files:**
- Modify: `.github/copilot-instructions.md`
- Modify: `CONTRIBUTING.md`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `sdk/python/src/c_two/cli.py`
- Modify: `sdk/python/tests/README.md`
- Modify: `sdk/python/benchmarks/run_kostya_sweep.sh`
- Modify: `sdk/python/benchmarks/*.py`

- [ ] **Step 1: Update `c3 dev` project detection**

In `sdk/python/src/c_two/cli.py`, update `_is_dev_environment()` to check the new source path:

```python
def _is_dev_environment() -> bool:
    """Check if we're running from a development checkout."""
    try:
        root = Path.cwd()
        for parent in [root, *root.parents]:
            if (
                (parent / "pyproject.toml").exists()
                and (parent / "sdk" / "python" / "src" / "c_two").is_dir()
            ):
                return True
    except OSError:
        pass
    return False
```

- [ ] **Step 2: Update `c3 dev generate-banner` output path**

In `sdk/python/src/c_two/cli.py`, replace:

```python
pkg_dir = root / "src" / "c_two"
```

with:

```python
pkg_dir = root / "sdk" / "python" / "src" / "c_two"
```

Also update the command docstring line:

```text
Output: sdk/python/src/c_two/banner_unicode.txt
```

- [ ] **Step 3: Update Copilot instructions active commands**

In `.github/copilot-instructions.md`, update active paths and commands:

```markdown
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
uv run pytest sdk/python/tests/unit/test_wire.py -q
uv run pytest sdk/python/tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q
cd core && cargo check --workspace
cd core && cargo test -p c2-mem -p c2-wire
uv run python examples/python/local.py
uv run python examples/python/crm_process.py
uv run python examples/python/client.py
uv run python examples/python/relay_mesh/resource.py
uv run python examples/python/relay_mesh/client.py
```

Update architecture headings from `src/c_two/...` to:

```markdown
### 1. CRM Layer (`sdk/python/src/c_two/crm/`)
### 3. Config Layer (`sdk/python/src/c_two/config/`)
### 4. Transport Layer (`sdk/python/src/c_two/transport/`)
### 5. Rust Native Layer (`core/`)
### CLI (`sdk/python/src/c_two/cli.py`)
```

Keep the Rust crate table unchanged except for the workspace path.

- [ ] **Step 4: Update contributor docs**

In `CONTRIBUTING.md`, update test and Rust commands to:

```markdown
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
uv run pytest sdk/python/tests/unit/test_wire.py -q
uv run pytest sdk/python/tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q
cd core && cargo check --workspace
cd core && cargo test -p c2-mem -p c2-wire
```

Replace the project structure block with:

```text
core/
├── foundation/
├── protocol/
├── transport/
└── bridge/
sdk/python/src/c_two/
├── crm/
├── transport/
├── config/
├── mem/
└── cli.py
sdk/python/tests/
├── unit/
├── integration/
└── fixtures/
examples/python/
```

- [ ] **Step 5: Update root README example and benchmark paths**

In `README.md`, update active links and commands:

```markdown
[`sdk/python/benchmarks/unified_numpy_benchmark.py`](sdk/python/benchmarks/unified_numpy_benchmark.py)
[`examples/python/`](examples/python/)
[`examples/python/relay_mesh/`](examples/python/relay_mesh/)
```

Replace command examples:

```bash
uv run python examples/python/local.py
uv run python examples/python/crm_process.py
uv run python examples/python/client.py
uv run python examples/python/relay_mesh/resource.py
uv run python examples/python/relay_mesh/client.py
```

- [ ] **Step 6: Update Chinese README paths**

In `README.zh-CN.md`, mirror the path updates from Step 5:

```markdown
[`sdk/python/benchmarks/unified_numpy_benchmark.py`](sdk/python/benchmarks/unified_numpy_benchmark.py)
[`examples/python/`](examples/python/)
[`examples/python/relay_mesh/`](examples/python/relay_mesh/)
```

Use the same command path replacements under Chinese prose.

- [ ] **Step 7: Update Python tests README**

In `sdk/python/tests/README.md`, update command examples:

```markdown
uv run pytest sdk/python/tests/unit/test_wire.py -q
uv run pytest sdk/python/tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q
uv run python sdk/python/benchmarks/<script>.py
cd core && cargo test -p c2-mem -p c2-wire
```

Also update headings to refer to `sdk/python/tests/unit/`, `sdk/python/tests/integration/`, and `sdk/python/tests/fixtures/`.

- [ ] **Step 8: Update benchmark command strings and output paths**

Run this search:

```bash
rg -n "benchmarks/|examples/" sdk/python/benchmarks
```

For benchmark docstrings and shell commands, replace root-relative benchmark paths with `sdk/python/benchmarks/...`.

Remove stale benchmark source-path injection lines like:

```python
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
```

The workspace install should provide `c_two`; benchmarks should not depend on a sibling `src/` directory.

In `sdk/python/benchmarks/run_kostya_sweep.sh`, replace:

```bash
OUT="benchmarks/results/kostya_ctwo.txt"
mkdir -p benchmarks/results
uv run python benchmarks/kostya_ctwo_benchmark.py
```

with:

```bash
OUT="sdk/python/benchmarks/results/kostya_ctwo.txt"
mkdir -p sdk/python/benchmarks/results
uv run python sdk/python/benchmarks/kostya_ctwo_benchmark.py
```

- [ ] **Step 9: Search active files for stale paths**

Run:

```bash
rg -n "src/c_two/_native|src/c_two|tests/|benchmarks/|examples/" \
  .github/copilot-instructions.md CONTRIBUTING.md README.md README.zh-CN.md \
  sdk/python/README.md sdk/python/tests/README.md examples/python sdk/python/benchmarks \
  -g '!**/__pycache__/**'
```

Expected: only intentional references remain, and any root-level `examples/` references point to `examples/python/`.

- [ ] **Step 10: Verify CLI dev command is present in checkout**

Run:

```bash
uv run c3 dev generate-banner --help
```

Expected: help text for `generate-banner` is printed.

- [ ] **Step 11: Commit docs and tooling updates**

Run:

```bash
git add .github/copilot-instructions.md CONTRIBUTING.md README.md README.zh-CN.md \
        sdk/python/src/c_two/cli.py sdk/python/tests/README.md \
        sdk/python/benchmarks examples/python
git commit -m "docs: update paths for monorepo layout"
```

Expected: commit succeeds.

---

## Task 6: Full Regression Validation and Cleanup

**Files:**
- Verify: `pyproject.toml`
- Verify: `sdk/python/pyproject.toml`
- Verify: `core/Cargo.toml`
- Verify: `sdk/python/tests/`
- Verify: `examples/python/`
- Verify: `sdk/python/benchmarks/`

- [ ] **Step 1: Confirm no forbidden placeholder directories exist**

Run:

```bash
test ! -e spec
test ! -e sdk/fortran
test ! -e sdk/typescript-browser
```

Expected: all commands exit successfully.

- [ ] **Step 2: Confirm source directories are in the target locations**

Run:

```bash
test -f core/Cargo.toml
test -f sdk/python/pyproject.toml
test -d sdk/python/src/c_two
test -d sdk/python/tests
test -d sdk/python/benchmarks
test -d examples/python
```

Expected: all commands exit successfully.

- [ ] **Step 3: Rebuild the Python package**

Run:

```bash
uv sync --reinstall-package c-two
```

Expected: command exits successfully.

- [ ] **Step 4: Verify package import and CLI entry point**

Run:

```bash
uv run python -c "import c_two as cc; print(cc.__version__)"
uv run c3 --version
```

Expected: Python import prints `0.4.7`; `c3 --version` exits successfully.

- [ ] **Step 5: Run Rust workspace checks**

Run:

```bash
cd core && cargo check --workspace
```

Expected: command exits successfully.

- [ ] **Step 6: Run focused Rust tests**

Run:

```bash
cd core && cargo test -p c2-mem -p c2-wire
```

Expected: command exits successfully.

- [ ] **Step 7: Run the Python test suite**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests -q --timeout=30
```

Expected: full Python test suite passes.

- [ ] **Step 8: Run Python example smoke tests**

Run:

```bash
uv run python examples/python/local.py
uv run python - <<'PY'
from pathlib import Path
import sys
sys.path.insert(0, str(Path("examples/python").resolve()))
from grid.grid_contract import Grid
print(Grid.__name__)
PY
```

Expected: `local.py` exits successfully and the import smoke prints `Grid`.

- [ ] **Step 9: Run one lightweight benchmark import smoke test**

Run:

```bash
C2_RELAY_ADDRESS= uv run python - <<'PY'
import importlib.util
from pathlib import Path

path = Path("sdk/python/benchmarks/segment_size_benchmark.py")
spec = importlib.util.spec_from_file_location("segment_size_benchmark_smoke", path)
module = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(module)
print(module.__name__)
PY
```

Expected: command prints `segment_size_benchmark_smoke` without import/path errors. This intentionally imports the script without running the full benchmark matrix.

- [ ] **Step 10: Check for stale active paths**

Run:

```bash
rg -n "src/c_two/_native|cd src/c_two/_native|uv run pytest tests/|uv run python examples/|uv run python benchmarks/" \
  .github/copilot-instructions.md CONTRIBUTING.md README.md README.zh-CN.md \
  sdk/python/README.md sdk/python/tests/README.md examples/python sdk/python/benchmarks \
  -g '!**/__pycache__/**'
```

Expected: no output, except historical prose that explicitly explains the pre-migration layout. Do not edit frozen historical docs under `docs/logs/`, `docs/reports/`, or old `docs/superpowers/` artifacts for this task.

- [ ] **Step 11: Inspect final diff**

Run:

```bash
git status --short
git diff --stat HEAD
```

Expected: only intended migration changes remain unstaged.

- [ ] **Step 12: Commit final cleanup if needed**

If Task 6 produced any additional fixes, commit them:

```bash
git add .
git commit -m "chore: validate monorepo layout migration"
```

Expected: commit succeeds if there were changes. If there were no changes, skip this commit.

---

## Self-Review

- Spec coverage: The plan covers `core/`, `sdk/python/`, root uv workspace, examples under `examples/python/`, no phase-1 `spec/`, no Fortran/TypeScript-browser scaffolding, Rust crate prefix retention, validation commands, and README ownership.
- Placeholder scan: The plan has no `TBD`, `TODO`, or unspecified implementation steps.
- Type/path consistency: All new active paths use `core/`, `sdk/python/`, and `examples/python/`. Validation commands run from the repository root unless explicitly using `cd core`.
