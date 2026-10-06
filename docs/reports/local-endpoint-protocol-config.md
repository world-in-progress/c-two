# Local endpoint protocol configuration slice

Baseline: `0f72d072e3f4f5aa3c51c8251c15d306fc1cafd0`.

This slice adds a Rust-owned `LocalEndpointProtocol` enum with strict
`legacy-v1` and `managed-v2` parsing. IPC configuration defaults to
`legacy-v1`; explicit code overrides take priority over process environment,
which takes priority over `.env`. The environment key is
`C2_IPC_ENDPOINT_PROTOCOL`. Rust owns the override key catalog and the Python
type surface only declares the two accepted strings.

`LocalEndpoint::from_address` remains an explicit legacy derivation. The new
protocol-aware constructor keeps the logical address unchanged and exposes the
selected protocol and OS namespace. Unix managed endpoints hash the complete
server ID with SHA-256 beneath `/tmp/c2-<effective-uid-hex>/v2`; endpoint
derivation does not create directories or files and checks encoded path length
against `sockaddr_un.sun_path`. Windows continues to derive legacy named pipes
and rejects `managed-v2` as unsupported. No listener, cleanup, owner control,
route selection, or fallback behavior is part of this slice.

`managed_v2_derivation_is_private_versioned_bounded_and_pure` uses a per-run
`uuid::Uuid::new_v4()` logical ID and compares the derived path before and after
derivation, instead of assuming that a fixed production server name has no
socket on the host. The protocol, effective-UID, SHA-256 digest, encoded-length,
case-sensitivity, and Unicode identity assertions are retained, as is the
no-file-creation acceptance.

## Validation

- `CARGO_BUILD_JOBS=2 cargo test --manifest-path core/Cargo.toml -p c2-config`:
  passed, 103 tests, 0 failed, including
  `local::tests::managed_v2_derivation_is_private_versioned_bounded_and_pure`.
- `FASTDB_PAYLOAD_LINK_MODE=system FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib DYLD_LIBRARY_PATH=/private/tmp/c2-memory-fastdb-sdk/lib CARGO_BUILD_JOBS=2 cargo check --locked --manifest-path sdk/python/native/Cargo.toml`:
  passed, exit 0, after the re-export repair below. This is the SDK native
  crate's separate manifest, not the Core workspace.
- `sdk/python/native/Cargo.lock`, `cli/Cargo.lock`, and `sdk/rust/Cargo.lock`
  each gained only the `libc` dependency edge of `c2-config` (one added line,
  no package version changed). `cargo metadata --locked` succeeds for all three
  manifests after the update.
- `cargo fmt --manifest-path core/Cargo.toml -p c2-config -- --check` and
  `rustfmt --edition 2024 --check --config skip_children=true
  core/foundation/c2-config/src/lib.rs` are clean. The native crate's only
  unformatted files are the pre-existing, untouched `mem_ffi.rs` and
  `shm_buffer.rs`.
- `git diff --check`: clean.
- `sdk/python/tests/unit/test_config.py` is absent at this baseline.
- The Unix endpoint tests ran on the current Unix host. The Windows-specific
  rejection and legacy-pipe tests are present but were not executed on Windows;
  this report claims compilation/tests on Unix only.

## Native compile validation and the E0412 repair

The first native compile attempt in the DSH `workspace-write` sandbox could not
reach the crate: the FastDB dependency build script configures CMake inside its
cargo git checkout and fails writing
`/Users/soku/.cargo/git/checkouts/fastdb-def8b40a5adb8189/4f99f86/fastcarto/lib/clipper/Clipper2Lib/include/clipper2/clipper.version.h.tmp`
with `Operation not permitted`; a direct `touch` probe in that checkout was
denied the same way. That sandbox-denied attempt is superseded.

The Host then ran the check through a command adapter using the official
CoreSDK system link mode, which reached `c2-python-native` and exposed a real
source error:

```text
error[E0422]: cannot find struct, variant or union type `BaseIpcConfigOverrides`
in crate `c2_config`
   --> src/config_ffi.rs:111:26
   --> src/config_ffi.rs:411:26
```

`core/foundation/c2-config/src/resolver.rs:78` already defines
`pub struct BaseIpcConfigOverrides` (the shared base override set of
`ServerIpcConfigOverrides` and `ClientIpcConfigOverrides`), but
`core/foundation/c2-config/src/lib.rs` did not re-export it, so the native crate
could not name the Rust authority type. The repair adds exactly that existing
type to the crate re-export list:

```rust
pub use resolver::{
    BaseIpcConfigOverrides, ClientIpcConfigOverrides, ConfigResolver, ...
};
```

No SDK-local copy of the type was introduced and no interface was removed; only
`core/foundation/c2-config/src/lib.rs` changed. The Host's native check command
was then re-run in this workspace and passed (`exit 0`, `Finished dev profile
[unoptimized + debuginfo] target(s) in 5.97s`), and the `c2-config` suite still
reports 103 passed / 0 failed. Re-running the same command on the final
delivered tree (`exit 0`, 2.24s, `c2-python-native` checked) reproduces the
result.

Evidence scope: system link mode compiles the native crate against the official
prebuilt `libfastdb.dylib` under
`/private/tmp/c2-memory-fastdb-sdk/lib`, so it validates the Rust
source/re-export boundary exercised by PyO3 but is not evidence for the FastDB
C++ source-mode build or for a source-mode native artifact.

## Integration notes

The worktree Git index lives outside the allocated checkout
(`/Users/soku/Desktop/codespace/WorldInProgress/c-two/.git/worktrees/checkout5`),
so no `git add` was attempted for this repair; as instructed, the worktree was
not touched through the index. This report stays untracked in the delivered
worktree and the Host integrates it via the sealed patch.
