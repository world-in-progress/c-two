# OwnerBound SDK facade slice — 2026-10-06

This is an independently rebuilt SDK slice for Host integration review. It is not complete-project acceptance, Windows runtime evidence, source-mode FastDB packaging evidence, or a published release.

Base: `b4b081b3cb11e52cb46e6b6fb798b845b79e8d71`. Implementation and test input Git tree before adding this report: `58b0adcd0ff87c6eb9734d2c88c92a57ae6bc2c4`. The optional partial `56f1ef7172057e5d4daeb9e82deb1f532bf427a5` was reviewed and selectively imported; its prior test results were not accepted as evidence.

## Ownership and behavior

Core remains the owner of server policy, freeze, owner monitoring, admission, drain and shutdown transactions. The only Core changes in this slice are canonical SDK reexports and a read-only receiver-consumed getter. No Host transaction, relay algorithm, protocol algorithm or Unix endpoint cleanup implementation was changed.

Python `LifecycleConfig` is data only. Rust validates policy and grace before attaching a receiver. A duplicate or late attachment, invalid policy, invalid IPC configuration and a failed-start late attachment preserve a still-valid receiver wrapper. Host creation is serialized with capability attachment, including free-threaded Python. Explicit `adopt_owner_stdin()` duplicates the inherited stdin object; it never runs automatically and never transfers ownership of the inherited source descriptor/handle. The opaque receiver has one consumption; a stdio transfer consumes it even if process spawn subsequently fails.

`RuntimeSession.ensure_host()` inherits the Core policy through default HostOptions. `host_started` observes `Host::is_running()`. Snapshot and terminal outcome methods delegate to Core. Native shutdown forwards every supplied timeout to `Host::shutdown_with_timeout()` and retains its Host and registrations while Core has no terminal journal. The added outcome `completed` field projects Core completion; SDK code does not infer relay completion from the presence or absence of diagnostic relay errors. This distinction preserves cleanup when a late malformed relay environment produces an error report for a host Core has actually completed.

The bridge retains Python slots and skips hooks on incomplete shutdown. Once Core reports completion, it atomically removes hook-safe slots and invokes each hook once. The registry retains the same Session, identity and bridge while incomplete; it publishes a retirement-observation-preserving replacement only after completion. Singleton reset follows the same boundary. `cc.shutdown(timeout=...)` returns the native structured outcome. Blocking `cc.serve()` observes native owner termination and repeats bounded native consumption until completion; OwnerMissing, Draining, and a stopped listener alone cannot end serve or trigger hooks. Nonblocking serve plus manual shutdown uses the same path.

The Rust SDK reexports the exact Core lifecycle/capability types and provides an owned_child controller example. The controller transfers the receiver only to one child's stdin and retains the keepalive privately. Its output contains public markers and PID; the Python resource fixture outputs its public IPC address and completion marker. Capability values are never carried through argv, environment, serialized configuration or logs.

## Actual local verification

Exclusive environment: `/tmp/c2-endpoint-python-owner-venv` (CPython 3.12.10). Exclusive Cargo target: `/tmp/c2-endpoint-targets/python-owner-final`, `CARGO_BUILD_JOBS=2`. Native extension was actually rebuilt and installed editable with maturin, using official FastDB 0.2.1 system linkage:

```sh
FASTDB_PAYLOAD_LINK_MODE=system
FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib
DYLD_LIBRARY_PATH=/private/tmp/c2-memory-fastdb-sdk/lib
```

Final native build exit was zero. Log: `/tmp/c2-python-owner-build-final.log`. `otool -L` confirms `@rpath/libfastdb.dylib`. Imported module: `/Users/soku/.codex/worktrees/c2-python-owner-final/c-two/sdk/python/src/c_two/_native.cpython-312-darwin.so`.

- Native SHA-256: `c75d05117b6b8cf2f39ec55eef3312d74148b5bc6ca7f449ec19cdf5e80ffbab`.
- Rust owned_child SHA-256: `3a1ea339732e20a7363bfeac9a1ece7efd12bac5c813fb61e35ef98b4f0b0883`.
- FastDB dylib SHA-256: `669e6a9bcc2a658dd4eafb30445bc047102016dd13323eed37fe1a8096cdb6dd`.

`cargo test --manifest-path sdk/rust/Cargo.toml --all-features`: **12 passed**, zero failures (4 payload tests, 8 public API tests). The new test verifies exact Core type identity and uses Core policy/receiver attachment. Log: `/tmp/c2-python-owner-rust-sdk-tests.log`. Launcher rebuilt successfully from the final source. Log: `/tmp/c2-python-owner-rust-launcher-build-final.log`.

Broad Python run selected all `sdk/python/tests/unit/` plus owner_bound_lifecycle, portable_payload_runtime, serve, registry, multi_crm_server and transfer_hold integration files, with normal `-n 2` fixture parallelism and `--timeout=90`: **723 passed, 3 skipped, 1 failed**, in 11.37 seconds. Log: `/tmp/c2-python-owner-final-verification.log`. The one failure is the pre-existing protocol source assertion `test_route_authority_reports_invalid_ipc_address_as_validation_error`, which still requires `local_endpoint_from_ipc_address(address)` although the base authority implementation is protocol-aware. The authority implementation and that assertion were left for Host protocol integration review. The broad run is therefore not claimed green. Skip reasons were not captured by this run; minimum Python 3.10 and all platform gates remain Host validation work.

After strengthening the final business-disconnect check, removing the unused Python controller fixture, and adding a failed-spawn consumption test, the final focused lifecycle run was **31 passed**, in 9.19 seconds. Log: `/tmp/c2-python-owner-final-lifecycle.log`.

The actual lifecycle evidence includes Rust parent keepalive shutdown, Drop and parent kill driving native stop, one Python shutdown hook and child exit; repeated real business-client disconnects preserving the armed host; refusal to adopt ordinary business stdin; missing/preclosed owner refusing readiness; duplicate/configuration/late/failed-start attachment preserving a valid unused receiver; and opaque receiver single consumption.

The in-flight test uses a real direct IPC connection from an independent native client Session. Its resource blocks behind an entered/finish barrier with a borrowed FastDB Payload. A 100 ms shutdown observation returns incomplete within 800 ms, preserves Host/registrations/identity/slots, runs no hook, and leaves the borrowed input and a separate held response readable. After releasing the callback barrier, borrowed views invalidate at callback completion; repeat shutdown reaches the same Core terminal outcome and runs one hook. The held response remains readable through server shutdown and invalidates only on explicit hold release. The OwnerBound variant also closes the keepalive while `cc.serve()` blocks: serve stays alive during the blocked callback and exits only after the terminal transaction. Persistent is exercised with the same drain test.

Existing test IDs were retained. Test doubles were updated to the new native `completed`/timeout contract rather than adding production compatibility behavior. The barrier-warning test now asserts that incomplete Sessions stay owned and only completed cleanup retires once; its warning assertions remain intact.

## Remaining Host gates

Host must review the immutable SDK diff, integrate the protocol source assertion, run its complete fixed-source suite and portable matrices, rebuild source-mode package artifacts as required, and obtain Windows Actions runtime evidence. The installed local module here is explicitly system-linked; no source-mode or Windows success is inferred from it. The SDK does not introduce reconnect/takeover, Python pipe/path monitoring, publication, version changes, or endpoint deletion.
