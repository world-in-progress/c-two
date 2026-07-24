# C-Two Rust SDK

`sdk/rust` is the user-facing Rust projection of C-Two:

```text
Cargo package: c-two
Rust import:   c_two
version:       0.1.0
publish:       false
```

This local-candidate package is not a registry release. It reuses
`c2-contract` for release identity and `c2-core` for route, transport, retry,
host, and lifetime behavior. It does not expose a second runtime state machine.

Portable payloads keep their owner-qualified name:

```rust
use c_two::{Connect, ContractLimits, ContractRelease, Runtime};
use fastdb::Payload;
```

C-Two-generated Rust modules provide typed clients and service definitions.
They consume the hidden `c_two::generated` seam; ordinary applications should
not construct encoded calls or service dispatch tables by hand.

All portable receive paths are currently copy-backed. `Held<Payload>` adds a
checked lifetime: explicit `release()` and `Drop` invalidate the FastDB owner
before releasing the C-Two response lease. Generated service inputs use the
same ordering at callback exit. This is not a zero-copy claim, and unsafe raw
pointers extracted outside FastDB's checked owner/view model cannot be
revoked.

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC
call. They use the generated seam directly only because Task 6 supplies the
actual generated client and typed service modules.
