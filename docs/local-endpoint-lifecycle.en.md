# Local endpoint lifecycle and controller integration

English · [简体中文](local-endpoint-lifecycle.md)

Rust `c2-config` derives endpoints from logical `ipc://` addresses. SDKs, relay and c3 consume the same context. Unix defaults to `/tmp/c2-<uidhex>/<32-character id>`, with a nonce coordinator and removable listener leases. The original coordinator inode remains pinned until listener lease ownership ends. Windows uses the current-logon SID's Named Pipe and a non-inheritable kernel listener lease. The OS selects the backend; credential `managed-v2` / `named-pipe` fields describe formats.

## Directory selection and credentials

Select the final Unix directory through `cc.set_local_endpoint(root=...)`, c3 `--ipc-root`, or `C2_IPC_ROOT`. Code/CLI overrides take precedence over process environment, `.env` and the platform default. A custom directory is application-provisioned and owned by the current user. Mode `0755` is accepted when that user has read, write and traversal access and group/other users cannot write. C-Two creates its default directory with mode `0700` and verifies socket mode `0600` before listening. Endpoint names have 32 lowercase hexadecimal characters, with no version subdirectory or socket suffix. Linux/macOS bind, connect and probe operations use the directory descriptor and short name; spaces, Unicode and long directories are supported within filesystem limits. Cleanup retains the application directory and unrelated files. Windows rejects Unix root overrides.

macOS also checks the extended ACL on the open directory descriptor. Read, traversal and deny entries are accepted; allow entries granting entry creation, deletion, attribute changes or permission changes are rejected, including inherited entries. A failed startup releases its listener lease even while duplicate file descriptors survive.

Runtime freezes the endpoint context before its first local bind/connect attempt, including failure. Changing the frozen root reports a configuration error; later environment changes do not affect existing listeners, clients, credentials or sweeps. Queries perform no filesystem I/O. Different directories form distinct native contexts, while connections still validate server identity, instance identity and route contracts. Root configuration does not move SHM, file spill or configuration files. See the [configuration guide](configuration.en.md).

Unix credentials uniformly use existing schema 3, recording final root, namespace id, incarnation and socket identity. Encoding, decoding and CLI reading share a 32 KiB bound. Windows credentials use kernel-managed schema 1. Maintenance uses the recorded context; a mismatched explicit root is rejected before target access. Native configuration derives the full socket path; the document contains no caller-selected socket path. A credential describes an object and does not itself grant deletion authority.

## Persistent and OwnerBound resources

The default `Persistent` lifecycle keeps resources available after ordinary client disconnects and relay eviction. `OwnerBound` binds server lifetime to one private inherited controller capability, prepared before readiness. Create the relationship with `cc.owner_control_pair()`, transfer the receiver only to the target child with `cc.spawn_owned_child(receiver, program, args)`, and retain the keepalive in the controller. The child calls `cc.adopt_owner_stdin()` and selects `cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0)` through `cc.set_server(..., owner_control=...)` before registration. The receiver cannot be reconnected or taken over. Losing that capability starts native draining after the configured bounded grace period; the grace period has no recovery entry point.

`OwnedChild.poll()`, `wait(timeout=...)` and `kill()` observe or control the process. `close()` releases the observer; the native shared reaper continues OS process reclamation. There is no always-running system monitor required to manage every endpoint.

`cc.shutdown(timeout=...)` reports native structured completion. A direct IPC admin shutdown ACK proves initiation, without proving that callbacks have drained or shutdown hooks can run. With `completed=False`, the session, bridge, routes and hooks remain available for observing the same transaction again. Listener closure, active work and retained payload leases are separate facts. Endpoint cleanup does not release held or borrowed payloads; their own lease release invalidates FastDB checked views.

## Exact cleanup and bounded sweeps

After readiness, call `cc.inspect_endpoint(address)` and save the `EndpointCredential` from a `present` result. `present` describes the observed object without proving process liveness. `WouldBlock` indicates coordinator contention and cannot establish death. After normal exit, or after `kill()` plus an OS `wait()` confirms exit, call `cc.reap_endpoint(address, credential)`. An old credential cannot remove a new incarnation.

```python
observed = cc.inspect_endpoint(address)
credential = observed['credential']
# The controller retains the credential while managing its child.
child.kill()
child.wait()
result = cc.reap_endpoint(address, credential)
```

For bounded maintenance, use the `cc.sweep_endpoints(addresses=[...], max_entries=64, max_ms=10)` context manager and `next_batch()`. Select only this run's logical addresses. Mark a round complete only when native `round_complete` is true; handle interrupted rounds and namespace changes separately. `c3 endpoint inspect/reap/sweep` calls the same native mechanisms. Sweep accepts repeated `--address`; use `--ipc-root` or `C2_IPC_ROOT` for a custom directory. Controllers do not derive OS paths or directly unlink endpoint files.

## Relay and platform boundaries

Relay freezes its local upstream context at startup. Upstream resources share that directory; separate local domains can use separate relays. Discovery carries the native namespace identifier, without exposing a raw root or granting authority. A local anchor may offer an IPC candidate only when namespaces, handshake identities and the full route contract match. A namespace mismatch selects HTTP and preserves the route. Windows namespace identity derives from backend and current logon scope; the OS DACL, identity handshake and route contract enforce access.

Authoritative route acquisition queries current server state and binds the route UID/revision and server instance. Idle eviction removes the data connection while retaining registration and owner state. An old proxy cannot acquire a replacement resource with the same name. Generated TypeScript persistent IPC clients use the same control lookup and identity rules.

Replaced objects, unknown endpoints, corrupt records and incomplete initialization report `unverified`. Age, PID and connection failure cannot authorize removal. Processes in one communication group use the same build and final-directory configuration. The new path directly replaces the old layout; existing processes, old credentials and historical directories are not migrated or swept automatically. Windows maintenance reports kernel ownership or `not-applicable`. Recovery from a crash at every possible point in initial registration is outside the guarantee. Validation reports identify the environments actually exercised.

See the [owned-child Rust example](../sdk/rust/examples/owned_child.rs) and [0.7.4 guide](releases/0.7.4.md). Earlier [canonical endpoint validation](reports/canonical-local-endpoint-validation.md) and [endpoint validation](reports/local-endpoint-final-validation.md) retain their historical sources and scopes.
