# Local Endpoint Credential Codec and CLI Facade
Scope: the native endpoint credential JSON codec plus a thin Rust/CLI facade. No Python FFI, owner bound, or transport algorithm is included.

## Ownership

`c2-local/src/credential.rs` is the only Rust parser for the credential document; `to_json()`/`from_json()` are the sole encode/decode surface, and SDKs and the CLI call it and never re-implement fields. `c2-core` re-exports native `EndpointCredential`, `EndpointInspection`, `EndpointReapResult`, `EndpointSweep`, `inspect_endpoint`, and `reap_endpoint`; its `c2-local` dep moved from dev to a normal minimal edge with no version upgrade. `sdk/rust` re-exports that surface unchanged and adds no paths or parser.

## Codec Rules

- Strict JSON: `deny_unknown_fields`; required `schemaVersion`, `address`, `protocol`, `platform`, and (Unix) `device`/`inode`/`changedSecs`/`changedNanos`; capped at 4096 bytes on both encode and decode.
- Schema follows the credential: managed-v2 with an incarnation encodes as v2, legacy as v1. v2 requires an incarnation and a v1 record is never upgraded into a UUID incarnation; an incarnation paired with the legacy protocol is rejected.
- The `LocalEndpoint` is always rebuilt from the recorded address and protocol, so the OS path is never authority and is never recorded. Integers are checked, never coerced.
- Unix credentials carry the socket identity (device/inode/ctime) and, for managed-v2, the listener incarnation. Windows credentials are kernel-managed metadata only: always v1, never claiming a pipe exists and never deleting a file. `EndpointCredential` is imported unconditionally; only the socket identity is Unix-only.
- A credential is not a secret and not an authorization capability. Every actual reap still runs the native gate, lease, and identity check. Encoding/decoding never read the filesystem and never create a lock.

## CLI (`c3 endpoint`)

- `inspect ADDRESS [--protocol legacy-v1|managed-v2]` — an explicit protocol wins; otherwise the default is resolved from the current process environment and `.env` through the Rust config resolver (`c2-config`), not a build-time legacy constant. An unparsable configured value fails closed.
- `reap ADDRESS --credential FILE` — the file is rejected unless it is a regular file; it is read once with a `take(MAX + 1)` bound, so a FIFO or device cannot block and a growing file stays bounded. The address is derived with the credential's own protocol, and the native identity check still decides the outcome.
- `sweep --protocol REQUIRED [--max-entries 64] [--max-ms 10] [--max-batches finite]` — never guesses a namespace, never scans a legacy directory by default, and validates budgets to a finite positive range.
- Every result is one JSON line with explicit `status` and `reason`. `KernelManaged` is not-applicable, never alive. An ignored partial-cleanup flag is unknown, and `socketRemoved` is never invented.
- Each process advances a stable native iterator; entries are never fully collected. Failures exit non-zero via `ExitCode`; `busy`, `stale-target`, `unverified`, and an unfinished round are never success.

## Verification (native, this host)

- `c2-local` lib tests: 75 passed on `aarch64-apple-darwin`. The original 11 credential IDs are unchanged; 6 platform-neutral strict-parse cases now run on Unix and Windows alike, and 3 Windows-only cases (kernel-managed v1 round-trip, `badplatform`, `managedunsupported`) are compiled by the Windows target check.
- `c3 endpoint` focused tests: 15 passed on `aarch64-apple-darwin`, including non-regular-file/FIFO rejection, non-UTF-8 rejection, config-resolved default protocol, and credential-protocol-derived reap.
- Env: `FASTDB_PAYLOAD_LINK_MODE=system`, `FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib`, `DYLD_FALLBACK_LIBRARY_PATH` to the same dir; target `/tmp/c2-endpoint-targets/codec-cli`.
- Windows boundary: `cargo check -p c2-local --target x86_64-pc-windows-msvc --all-targets` passes with no warnings, so the codec and its Windows tests compile. Windows test execution, and any Windows `c2-core`/SDK/CLI check, are unknown here: this macOS host has no MSVC C toolchain, so `ring`'s C build fails before linking. No Windows runtime result is claimed.
- Python FFI projection of the same codec and the whole-suite gates remain for the Host.
