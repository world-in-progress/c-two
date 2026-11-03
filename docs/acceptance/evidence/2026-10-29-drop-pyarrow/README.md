# Remove the remaining example dependency

Local validation uses base commit `b995c567429786c67ed3cee30a0c58a68cd028fa` plus the 15 candidate file hashes in `verification.json`. The runner was macOS ARM64 with Python 3.12.12, Rust 1.99.0, uv 0.10.12 and actionlint 1.7.12. This record covers the targeted checks below; the Host owns the full workflow-policy suite and hosted cross-platform CI.

`NestedGrid` now initializes its in-memory grid directly. File persistence and the unused Redis reader `_get_grid_info()` were removed; the Host explicitly authorized the latter after confirming it had no callers or initialized Redis client. AST comparison confirms that the 13 retained methods are unchanged and all 11 retained constants still have readers.

Parsing `uv.lock` before and after `uv lock` gives 46 and 45 package entries: only `pyarrow 24.0.0` is removed, nothing is added and every retained version is unchanged. Reproduce that comparison from the repository root:

```sh
python3.12 - <<'PY'
import subprocess
import tomllib
from pathlib import Path

base = 'b995c567429786c67ed3cee30a0c58a68cd028fa'
before = tomllib.loads(subprocess.check_output(
    ['git', 'show', f'{base}:uv.lock'], text=True))
after = tomllib.loads(Path('uv.lock').read_text())
inventory = lambda lock: sorted((p['name'], p['version']) for p in lock['package'])
old, new = inventory(before), inventory(after)
assert sorted(set(old) - set(new)) == [('pyarrow', '24.0.0')]
assert not set(new) - set(old)
assert len(old) - len(new) == 1
print(f'{len(old)} -> {len(new)} packages; only pyarrow 24.0.0 removed')
PY
```

All build and test processes used one task-owned `CARGO_TARGET_DIR`, represented as `$TASK_TMP/target` in the command records. The dev environment, uv cache, copied Cargo cache, IPC directory and test outputs also lived under `$TASK_TMP`. `uv sync --locked --group dev --no-editable` built this checkout's native extension into that environment. The installed SDK's 33 Python source files matched the checkout. After validation, the complete task directory and its build outputs were deleted.

The initial native build failed because clang's default C++ header directory was missing while the selected SDK contained the standard headers. A minimal `algorithm`/`vector` syntax probe reproduced the failure with exit 1 and passed with exit 0 after setting task-only `CPLUS_INCLUDE_PATH="$(xcrun --show-sdk-path)/usr/include/c++/v1"`. The subsequent native extension and c3 builds passed using that environment setting.

| Check | Exit code | Result |
| --- | --- | --- |
| `uv lock` | 0 | Only pyarrow 24.0.0 removed |
| Parsed lock inventory comparison | 0 | All 45 retained name/version pairs unchanged |
| `uv lock --locked` | 0 | Lock is current |
| `uv sync --locked --group dev --no-editable` | 0 | Isolated dev environment and candidate native extension built |
| `cargo build --manifest-path cli/Cargo.toml --bin c3 --locked` | 0 | Candidate c3 built in the same target directory |
| `$TASK_TMP/dev/bin/python -c 'import pyarrow'` | 1, expected | `ModuleNotFoundError`; distribution also absent |
| Three requested pytest files | 0 | 12 passed in 5.78 seconds; 0 skipped |
| `actionlint .github/workflows/ci.yml` | 0 | No diagnostics |
| Reference and CI variable checks | 0 | Matches confined to historical directories; all three removed CI variables absent |
| `git diff --check` | 0 | No whitespace errors |

The pytest command, individual test names, environment versions, native/c3 hashes, exit codes and summaries are retained in `verification.json`. The tests include thread-local calls, direct IPC with an unavailable relay, explicit HTTP relay calls, runnable IPC/relay examples and contract inference. Every requested test executed; no test skipped because of pyarrow or any other dependency.
