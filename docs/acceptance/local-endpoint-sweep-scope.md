# Explicit logical-address scope for endpoint maintenance

Source base: `276f075bc541e37778b7f6d55287e6233acf1a4a`.

`cc.sweep_endpoints(protocol, *, addresses=None, max_entries=None, max_ms=None)`
and `PyEndpointSweep` now delegate an optional address selection to c2-local.
`None` retains operator-requested whole-namespace behavior. An explicit empty
list selects no endpoint slots. Up to 4,096 logical IPC addresses are accepted;
c2-config derives their protocol endpoints and c2-local validates a shared
canonical namespace before iterator acquisition. Arbitrary OS paths and invalid
addresses fail before the maintenance lease is taken.

Rust derives selected socket and ownership names with the existing
`endpoint_names` / `managed_names` helpers. Filtering occurs before endpoint
inspection, reaping, or lease retirement. Untargeted directory entries count
against `entries_visited` and the batch budget, but are never inspected or
modified. Directory iteration remains incremental. Existing lease, EOF,
interruption, namespace replacement, and Windows NotApplicable behavior remain
in place. No automatic sweep or recursive deletion was added.

Every genuine native sweep constructor in `test_endpoint_maintenance.py` now
uses a fresh explicit logical-address scope. All previous test functions and
parameter decorators are retained. Invalid-protocol constructors still reject
before opening a namespace. The old decoded-credential case now starts a real
managed listener, retains its credential, shuts down, binds a new incarnation at
the same fresh address, and confirms the old credential cannot delete it. Cleanup
uses native shutdown and the exact current credential. The Windows codec and
kernel namespace branches remain executable without skips.

Validation on macOS:

- `cargo test --manifest-path core/Cargo.toml -p c2-local sweep_scope -- --nocapture`:
  4 passed, 0 failed. All scope tests use private temporary directories. They
  cover two real orphan listeners, selected orphan socket/lease retirement,
  unchanged unselected socket/lease identity and record bytes, active selected
  protection, single-entry budgets, finite EOF, repeated scopes, empty scope,
  invalid/unbounded address input, and interrupted namespace replacement.
- Real native extension rebuild: `uv sync --reinstall-package c-two`, exit 0.
- `uv run --no-sync pytest sdk/python/tests/unit/test_endpoint_maintenance.py -q --timeout=30`:
  66 passed (62 prior cases plus 4 added cases), no skips.
- `git diff --check`: passed. AST comparison confirms every previous test
  function and parametrization decorator remains present.

Build/test environment: exclusive Cargo target `/tmp/c2-endpoint-targets/sweep-scope`,
venv `/tmp/c2-endpoint-sweep-scope-venv`, `FASTDB_PAYLOAD_LINK_MODE=system`,
`FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib`, and
`DYLD_LIBRARY_PATH=/private/tmp/c2-memory-fastdb-sdk/lib`. System-mode import
requires that DYLD path. The full Host suite and actual Linux/Windows workers are
outside this bounded validation. No historical shared-root endpoints were
selected for destructive maintenance.

Evidence logs and SHA-256:

- `/tmp/c2-sweep-scope-native.log`: `d88b7624bf05ee875bd0830e8a879ddcbd340ec061bf5bd9a7634ddfb1d0d9ec`
- `/tmp/c2-sweep-scope-build.log`: `a9b4c569dafe2be49ee5540a50e26c84bcd33326d5bd7c8e2c4bece3e83e8172`
- `/tmp/c2-sweep-scope-python.log`: `a8ae48ed56cb9114dc56ea70e484b8393fab5b8a80cd09f8cb95e33542718204`

Rebuilt extension: `sdk/python/src/c_two/_native.cpython-313-darwin.so`,
SHA-256 `8b3474417c02f19f105e109ea7c7a732c9e1f375f4db0fcef493ebbafd40e17e`.
This is a local system-linked validation artifact and is not committed.
