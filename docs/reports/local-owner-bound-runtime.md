# Native OwnerBound runtime candidate

This report describes the Core candidate in the commit containing this report, based on `1583d6d0fd6951b0cd56e18b7fb77e6c4e7d7210`. The working checkout is `/Users/soku/.codex/worktrees/c2-ownerbound-final/c-two`. It does not accept the earlier partial implementation or the separate endpoint v2 work.

The receiver now establishes its single native watcher and probes the actual endpoint before readiness. Unix uses a nonblocking pipe read (`EAGAIN` versus EOF); Windows uses the validated overlapped named-pipe handle and `PeekNamedPipe` without another IOCP registration. A deterministic current-thread regression fails with the former first-Pending-as-Alive arm implementation: `/tmp/c2-ownerbound-deterministic-red.log`.

Owner EOF closes business admission before publishing OwnerMissing, then starts the existing native drain after bounded grace. The coordinator keeps the actual Server run future alive through callback drain and journal completion. Explicit shutdown cancels control observation and grace on initiation. Watcher I/O failures remain sticky diagnostics; actual work completion is a separate outcome.

Public shutdown waits on one remaining caller deadline. A shared native transaction continues owning the actual Server, route close journal and outgoing client barriers after a caller receives an incomplete result. Incomplete outcomes are not cached as terminal. Completed observations are available through Host.shutdown_outcome and lifecycle_snapshot. The watcher holds no strong Host reference; Drop neither joins a live thread nor force-frees callback or retained backing. Persistent Runtime reuse separates concrete Server transactions and retires dead metadata. Native lifecycle policy is configured and frozen in Runtime; the private receiver is attached and consumed once outside cloneable option data.

Relay cleanup reconciles every actual route close, including owner-triggered native journal records. This candidate also blocks new Host creation, server option changes and identity reset throughout pending teardown, including after the listener stops while relay withdrawal is still waiting. `Runtime.clear_server_identity` now returns a Result; SDK projections must propagate it.

## Fixed-source local validation

Final gates use the exclusive `/tmp/c2-endpoint-targets/owner-final` target directory with `CARGO_BUILD_JOBS=2`, empty C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS, and the official FastDB system SDK: FASTDB_PAYLOAD_LINK_MODE=system, FASTDB_PAYLOAD_SYSTEM_LIB_DIR and DYLD_LIBRARY_PATH `/private/tmp/c2-memory-fastdb-sdk/lib`.

- Core `--lib --tests`: 94 passed (42 library, 15 client modes, 9 error normalization, 5 Host routes, 6 lifetime, 17 OwnerBound). Log: `/tmp/c2-ownerbound-isolated-core.log`.
- Config and local `--lib`, local owner_control: 105 + 29 + 12 passed. Log: `/tmp/c2-ownerbound-isolated-local-config.log`.
- Windows `c2-local` x86_64-pc-windows-msvc target check passed. Log: `/tmp/c2-ownerbound-final-windows.log`; target directory `/tmp/c2-memory-owner-windows-check`.
- `git diff --check` passed. Every original OwnerBound integration test ID remains. New coverage includes callback-entered barriers, a 100 ms incomplete return followed by true completion, live watcher Drop, sticky native I/O failure, real controller kill with bounded marker reception and RAII direct-child kill/wait, actual relay idle eviction with Persistent survival, owner relay withdrawal, retained response/backing ownership, Persistent reuse, and stopped-listener/pending-withdraw restart refusal.

The earlier shared-target runs are diagnostic records only. A later shared-target compile reused another checkout's c2-config artifact, so those records are not final candidate evidence. No package release, SDK/CLI gate, whole workspace gate, 10,000-cycle run, Linux runtime or Windows runtime is claimed here. Windows runtime validation belongs to Actions.

## Required follow-up before final acceptance

The existing relay unregister path still carries only name/server_id (`core/transport/c2-http/src/client/control.rs`, RelayControlClient.unregister; RelayState.unregister_upstream). A delayed old teardown can therefore delete a newer registration with the same name/server_id in a different Runtime or process. The same-Runtime pending gate prevents the local overlap but cannot fence that distributed mutation. Final acceptance requires a narrow compare-remove request bound to the originally captured server_instance_id, route_uid and route_revision. The existing route_table.unregister_local_route_if_matches seam can implement it. This candidate is sealed for SDK integration while that follow-up is implemented as a separate commit.

Windows Peek behavior reference: [Microsoft PeekNamedPipe](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-peeknamedpipe). The receiver validates overlapped mode; no synchronous-handle blocking guarantee is assumed.
