# Keep completed file-backed reassembly in its original storage

Baseline: `8777e2dd157942fe6f3cc90389763af35f5056bd`. Part of the approved [memory plan](../plans/2026-09-26-memory-policy.md).

`ChunkRegistry::finish` now returns a completed file-backed handle directly, preserving the existing logical-length trim. The unused `promote_to_shm` function, module and export are removed. This eliminates the former payload-sized temporary `Vec` and avoids immediately reversing a memory-pressure spill. Buddy, dedicated and file allocation fallback remain available.

The worker's first attempt timed out before sealed delivery. Its continuation delivered artifact `f2c178d8-b162-4188-b4e9-5e9fabd3a144`, with final source `4ae665059fc882e492a23f8014b29625cdf13f7a`. The Host reviewed the whole original-input-to-final-source diff, including changes made before continuation. Integration includes Host corrections below; the worker report alone was not acceptance.

## Ownership review

An independent review traced server request admission/cancellation through `RequestLease`, client replies through `ResponseLease`, and Rust/Python held owners. Both transport leases can copy a FileSpill via `MemPool::copy_handle_data`. Their release helpers consume/drop FileSpill handles directly, closing the owned `SpillMapping`; they do not call `MemPool::release_handle` for this variant. Checked FastDB owner invalidation remains above the transport lease.

The original consumer tests exercised SHM, so they did not prove file-backed consumption. The Host added explicit request and response FileSpill tests, covering bytes, full/short/empty logical lengths, idempotent release and rejection of reads after release. On Windows these tests also assert that the file exists while retained and is absent after release; those assertions still require Windows CI.

The registry's new tests force `spill_threshold=0.0`, check out-of-order chunks and short final chunks, and prove that finishing creates no buddy/dedicated mapping. Abort, incomplete-finish and timeout tests verify registry removal and continued usability. They do not independently measure live file mappings on Unix; actual closure there remains supported by the unchanged owned mapping/file drop implementation. A stale comment naming the deleted promotion helper was corrected by the Host.

## Host validation

Executed on macOS after applying the whole worker patch:

```sh
CARGO_TARGET_DIR=/tmp/c2-memory-host-build cargo test --manifest-path core/Cargo.toml -p c2-wire -p c2-ipc -p c2-server
# c2-wire: 119 passed; c2-ipc: 59 passed; c2-server: 141 passed; no failures
```

After adding the two consumer tests:

```sh
CARGO_TARGET_DIR=/tmp/c2-memory-host-build cargo test --manifest-path core/Cargo.toml -p c2-ipc -p c2-server file_spill_
# request consumer: 1 passed; response consumer: 1 passed
```

Both staged and unstaged `git diff --check` passed. No performance percentage or Windows success is inferred from these checks. Budget accounting and integrated cross-platform verification follow in the subsequent changes.
