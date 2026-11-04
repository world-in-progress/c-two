# Local endpoint lifecycle

This page defines local endpoint identity, listener ownership, credentials and bounded maintenance.

Rust `c2-config::LocalEndpoint::from_address()` derives the one OS-specific endpoint from a logical `ipc://` address; SDKs, relay and c3 consume the same context, with no `endpoint_protocol` setting or protocol-selecting SDK/CLI API. Unix defaults to `/tmp/c2-v3-<uidhex>/<32-character id>`, with a nonce coordinator and removable listener leases. The original coordinator inode remains pinned until listener lease ownership ends. Windows uses the current TokenUser SID's Named Pipe and a non-inheritable Global kernel listener lease. The OS selects the backend; credential `managed-v2` / `named-pipe` fields are backend metadata.

Windows pipes and mappings use the process TokenUser SID with explicit user ownership and protected user DACLs. Connections authenticate the actual connected pipe owner and its pinned server process before ordinary or control bytes. TokenUser must match, and the server must be at least Medium integrity. Preserve native OS causes and identification SQOS; unknown reachability is an error, not a capability downgrade. Listener startup adds only same-user process-query/synchronize and token-query rights, allowing Medium and High peers across logons without granting process control or token duplication or weakening checks after permission failure; there is no logon-name fallback. A Global user-restricted mutex owns listener uniqueness; mappings stay Local and require no global-mapping privilege. Native pipe session facts and handshake 15 directional request/response capabilities decide when shared references are reachable, including prepared requests and replies. Cross-session calls use owned/inline/chunked transfer and release copied SHM payloads through their original owner. See the [0.9.0 migration notes](../releases/0.9.0.md) for the required offline upgrade.

## Directory selection and credentials

Python `runtime = cc.Runtime()` owns a separate endpoint context, routes and native session. Use `runtime.set_local_endpoint(root=...)` before `runtime.start()` or `runtime.register()`, and pass `runtime.local_endpoint_context()` to the existing inspect/reap/sweep helpers when maintaining that instance's custom Unix root. An explicit Runtime does not remove a caller-created directory. Windows uses its normal per-user pipe namespace and rejects Unix roots. The [owned preflight example](../../examples/python/independent_runtime.py) creates a fresh private Unix directory and deletes it only after confirming shutdown completion and error-free drain.

Select the final Unix directory through `cc.endpoint.set_local_endpoint(root=...)`, c3 `--ipc-root`, or `C2_IPC_ROOT`. Resolution order is code/CLI override, process environment, `.env`, then platform default. A custom directory is application-provisioned and owned by the current user. Mode `0755` is accepted when that user has read, write and traversal access and group/other users cannot write. C-Two creates its default directory with mode `0700` and verifies socket mode `0600` before listening. Endpoint names have 32 lowercase hexadecimal characters, with no version subdirectory or socket suffix; the default root's `v3` suffix names namespace disk format 3, while a custom root is used exactly as configured. Linux/macOS bind, connect and probe operations use the directory descriptor and short name; spaces, Unicode and long directories are supported within filesystem limits. Cleanup retains the application directory and unrelated files. Windows rejects Unix root overrides.

macOS also checks the extended ACL on the open directory descriptor. Read, traversal and deny entries are accepted; allow entries granting entry creation, deletion, attribute changes or permission changes are rejected, including inherited entries. A failed startup explicitly unlocks its listener lease even while duplicate file descriptors survive.

For the macOS default namespace, sockets stay in `/tmp/c2-v3-<uidhex>`; only `.gate`, `.gate.marker` and `<id>.lease` move to `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`, the system-provided non-temporary user directory resolved during native I/O. Aliases of an existing default socket directory share the coordinator by native directory identity. An explicit alias whose directory is absent remains application-provisioned. Linux and non-default custom roots keep colocated records.

Both directories are verified under the same gate, and the listener pins the original directory handles until lease ownership ends. Normal close/Drop retires its socket and lease; abnormal exit uses the existing exact reap, bounded sweep or subsequent bind. When reboot or system cleanup removes the socket directory, a complete unlocked lease can still be retired without recreating that directory. A live lease still blocks a second listener. Replaced directories and invalid records remain unverifiable; never recreate a missing recorded coordinator.

Unix namespace disk format 3 is shared by the gate, marker and lease record. The fixed `c2mgatv2` / `c2mgrec2` magic still identifies the managed-v2 backend. The gate is 25 bytes, the marker 33 bytes and the lease record's fixed header 51 bytes. Persistent identity omits device and the record no longer stores `RECORD_PROTOCOL`. Live directory/file checks still use dev+ino. A recognizable different gate/marker version reports `unsupported-namespace-format` before old content can be interpreted or adopted.

Endpoint leases use those same native retirement paths; failed initialization removes only newly created entries. Invalid historical leases remain for separate handling. Gate/marker identities are permanent during online operation, with no new background sweeper. Versioned default directories keep different namespace disk formats apart: the new build neither reads nor deletes old default directories, and no same-path cold switch is needed. Upgrade the Python and Rust SDKs, c3 and Node consumers on one host together. For historical manual cleanup, first ensure every old process has exited and cannot restart. Explicitly sharing one custom physical root across disk formats is unsupported. If that root reports `unsupported-namespace-format`, rebuild only after all users have exited, cannot restart and the entire root has been safely archived offline. Identity checks still prevent deleting the other format's endpoints if roots are misconfigured, but counts and reasons can be inaccurate.

On Unix bind initialization failure, rollback removes only a lease created by that attempt whose original directory, gate and file identities still match. If the attempt bound a socket, that socket must first have been safely withdrawn. Pre-existing leases, replacements and unverifiable objects remain. This rollback uses creation proof still held by the current call; it does not expand historical invalid-record reap/sweep authority or guarantee cleanup after a process crashes during initialization.

Runtime freezes the endpoint context before its first local bind/connect attempt, including failure. Changing the frozen root reports a configuration error; later environment changes do not affect existing listeners, clients, credentials or sweeps. Pure context queries perform no filesystem I/O. Different directories form distinct native contexts, while connections still validate server identity, instance identity and route contracts. Root configuration does not move SHM, file spill or configuration files. See the [configuration guide](configuration.md).

Unix credentials use schema 4, recording final root, namespace id, incarnation and socket inode/ctime without device; all schema 3 documents are rejected. Encoding, decoding and CLI reading share a 32 KiB bound. Windows credentials use kernel-managed schema 1; inspection is `not-applicable` and reap is `NotApplicable`. Maintenance uses the recorded context; a mismatched explicit root is rejected before target access. Native configuration derives the full socket path; the document contains no caller-selected socket path. A credential describes an object and does not itself grant deletion authority.

On Windows, the Global listener lease is non-inheritable and connected streams do not retain it. Closing the listener releases that lease, so a new listener can restart at the same endpoint while old client pipe handles remain alive.

## Bind and connect failures

Since 0.8.1, startup deadline expiry has `CoreError.lifecycle_kind='startup_timeout'`, `startup_stage` and `timeout_seconds`. Local bind failure messages no longer carry the incorrect `config error` prefix; their existing OS fields are preserved. Configure the wait with `cc.set_server(startup_timeout=...)` before registration. See [startup deadlines](configuration.md#server-startup-timeout) for the finite default, freezing and cleanup boundary.

Public `c_two.error.CoreError` exposes `io_operation` (`bind` or `connect`), `io_kind` (`permission_denied`, `not_found`, `connection_refused`, `address_in_use` or `other`) and `raw_os_error` (the native errno/Win32 code, or `None` when no native code exists). An unverifiable endpoint at bind reports native `InvalidData`, `io_kind=other` and `endpoint_unverified_reason` with the same finite strings as inspection; policy refusals do not fabricate an errno. Actual OS errors retain their kind and errno. Configuration, handshake, contract and deadline failures have no local cause. The existing Windows PIPE_BUSY wait retains its original connection budget; gate-wait expiry is `WouldBlock`, while a live address or listener lease conflict is `AddrInUse`.

Rust Core and SDK errors expose `local_io_cause() -> Option<LocalIoCause>`; its `operation()` and `error()` project the operation and unchanged `EndpointIoError`. `LocalIoCause::Bind(EndpointBindCause)` carries `error` and optional `unverified_reason`; Connect still carries `EndpointIoError`. The shared readiness watch carries `ServerFailure { message, bind_cause, startup_timeout }` with that same `EndpointBindCause`. Exhaustive low-level matches must include `IpcError::Connect` and `ServerError::Bind` / `Failed` / `StartupTimeout`; Core exposes the latter through `LifecycleError::StartupTimeout`. Recompile Rust callers; no old payload or compatibility entry point remains. ABI and protocol versions do not change.

Context preserves the original OS kind/code while outer directory-policy rejection stays unchanged. Readable messages, error ordering, retry classification and deadlines keep their existing meaning. These fields neither prove process death nor authorize restart, fallback or cleanup; controllers still use the existing listener ownership and identity-checked maintenance APIs. Endpoint maintenance retains its existing ErrorKind text format.

## Public structured Python errors

Catch `c_two.error.CoreError` directly. It is a `CCBaseError` and a `RuntimeError`; `CCError` remains the separate canonical semantic-error family. Catch `CCBaseError` to handle both families, or `CCError` when only semantic errors should bypass a call-stage wrapper. `_native.CoreError` is removed. There is no top-level alias or replacement error wrapper.

```python
from c_two.error import CoreError

try:
    resource = cc.connect(Grid, name="grid", address="ipc://worker")
except CoreError as exc:
    if exc.io_operation == "connect" and exc.io_kind == "permission_denied":
        raise PermissionError(exc.message) from exc
    raise
```

Every instance has the annotated fields below. Optional fields default to `None`, `details` to a fresh dictionary. `str(exc)` and `args[0]` retain the original outer diagnostic. Python copying and pickle preserve the structured fields; they do not define a portable error protocol.

| Field | Type and values |
| --- | --- |
| `category` | `semantic`, `contract`, `admission`, `transport`, `lifecycle`, `protocol_version_mismatch` |
| `message` | `str`, outer Core diagnostic |
| `code`, `name` | `int` / `str` or `None`; a matched pair from the canonical `c2-error` registry (the codes in `error.ERROR_Code`, with Rust variant names) |
| `details` | `dict[str, str]`, canonical semantic context; keys depend on the error |
| `error_bytes` | `bytes` or `None`, canonical semantic error bytes; its inner message can differ from `message` |
| `transport_kind` | `ipc`, `http` or `None` |
| `transport_phase` | `str` or `None`; Core transport failures use `pre_dispatch` (before business dispatch) or `dispatch_uncertain` (peer execution is unknown). Semantic errors preserve an arbitrary peer-provided detail; it is not a trusted transport classification |
| `fallback_eligible`, `route_withdrawal` | `bool` or `None`, existing Core classifications; not independent permission to replay a request |
| `io_operation` | `bind`, `connect` or `None` |
| `io_kind` | `permission_denied`, `not_found`, `connection_refused`, `address_in_use`, `other` or `None` |
| `raw_os_error` | `int` or `None`; original errno/Win32 code, absent for refusals without a native code and for non-I/O failures |
| `endpoint_unverified_reason` | `unsafe-directory`, `symlink`, `unexpected-object`, `foreign-owner`, `missing-ownership`, `invalid-ownership`, `invalid-record`, `record-mismatch`, `coordinator-missing`, `coordinator-replaced`, `initialization-incomplete`, `unsupported-namespace-format` or `None`; only unverifiable bind failures, shared with inspection reason strings |
| `local_protocol_version`, `peer_protocol_version` | `int` or `None`; the two RPC handshake versions, nonzero only for `category='protocol_version_mismatch'` (for example local 14, peer 15), `None` on every other error |
| `lifecycle_kind` | `invalid_server_id`, `config_frozen`, `client_config_frozen`, `duplicate_route`, `missing_route`, `relay_duplicate_route`, `missing_relay_address`, `relay_http`, `register_failure`, `held_response_copy`, `held_response_release`, `response_copy`, `configuration`, `server`, `startup_timeout`, `runtime_retired`, `shutdown_wait_timeout`, `relay` or `None` |
| `startup_stage` | `readiness`, `responsiveness` or `None` |
| `timeout_seconds` | `float` or `None`, configured startup or shutdown budget in seconds |
| `route_name`, `body` | `str` or `None`, affected route or HTTP response diagnostic |
| `status_code` | `int` or `None`, HTTP status (including 409 for duplicate registration) |
| `relay_duplicate` | `bool` or `None`, relay duplicate-registration classification |
| `failure_source` | `relay_projection`, `relay_prepare`, `commit`, `relay_register`, `route_open` or `None` |
| `registration_failure`, `rollback`, `relay_cleanup_error` | `dict[str, object]` or `None`, existing registration transaction outcomes |

`registration_failure` contains `route_name`, `failure_source`, `error_message`, nullable `status_code`, `rollback` and `relay_cleanup_error`. `rollback` contains `route_name`, `local_removed`, `active_drained`, `closed_reason` and `close_error`; `relay_cleanup_error` contains `route_name`, nullable `status_code` and `message`. These outcomes report the original transaction; observing them does not repeat cleanup. Ordinary imports do not load FastDB or NumPy.

A peer that negotiates a different RPC handshake version raises `CoreError` with `category='protocol_version_mismatch'`, `transport_kind='ipc'`, `transport_phase='pre_dispatch'` and `fallback_eligible=False`; `code`, `name`, `error_bytes` and the three OS-cause fields stay `None`. The mismatch is a terminal fact with no route or HTTP fallback and no retry, and malformed/truncated preambles keep the ordinary transport/protocol classifications instead of fabricating a peer version. See the [handshake spec](ipc-handshake.md).

## Admin shutdown

An accepted admin shutdown ACK (`'initiated'`) proves initiation, without proving that callbacks have drained or shutdown hooks can run; `'absent'` means the operating system reported no listener, and `'unconfirmed'` that initiation was not confirmed. Listener closure, active work and the local copies of held or borrowed payloads are separate facts. Endpoint cleanup does not touch those local copies; releasing a hold, or the end of a method with a borrowed input, invalidates FastDB checked views. See [Runtime lifecycle](runtime-lifecycle.md) for SDK retirement, failed-start receipts and shutdown handoff.

Direct IPC admin shutdown retries transient connection failures only before attempting to write the control request. After writing starts, a write failure, lost ACK or timeout returns the existing unconfirmed result without reconnecting and resending; an unconfirmed result does not prove that the server did not execute the request. A replacement instance at the same address therefore cannot receive a replay from this call. Ping retains its probe retries; initial endpoint absence handling and the original total budget are unchanged.

## Exact cleanup and bounded sweeps

Tests and runners select logical addresses owned by their current run. Historical endpoints of unknown provenance are never test cleanup targets.

Endpoint credentials and bounded sweep state belong to `c2-local` and are projected through Core, SDK and CLI. After listener readiness, the host can call `cc.endpoint.server_credential() -> EndpointCredential | None` and pass that credential to its controller. It reads the listener value already published through the lifecycle watch; it does not inspect the endpoint, perform filesystem I/O, take the namespace gate or wait for readiness. Ready and Stopping with a retained listener return a value; before start, after bind failure and in terminal Stopped/Failed states it returns `None`. Between run cancellation and existing lifecycle finalization, it may return the previous snapshot. A controller can also use `cc.endpoint.inspect_endpoint(address)` when the coordinator gate is available. Neither result proves liveness or grants cleanup or owner authority; `WouldBlock` indicates contention. After normal exit, or after `kill()` plus an OS `wait()` confirms exit, call `cc.endpoint.reap_endpoint(address, credential)`. An old credential cannot remove a new incarnation. On Windows the getter returns kernel-managed schema 1; reap remains `NotApplicable`.

`cc.endpoint.EndpointCredential` is the immutable native value returned directly by the getter or inspection and accepted by reap. Share its reference within a process; for storage or transfer, explicitly use `credential.to_json()` and `cc.endpoint.EndpointCredential.from_json(document=...)`. Both retain the sole Rust codec's strict validation and 32 KiB bound; inspection does not guarantee successful JSON export. Manual construction, subclassing, mutation and shallow copying are unsupported. The Python wrapper, its `_native` slot and the old `_native.PyEndpointCredential` export were removed in 0.9.0; see its [migration notes](../releases/0.9.0.md).

```python
import c_two as cc

cc.register(Grid, GridResource(), name="grid")
credential = cc.endpoint.server_credential()  # The listener is ready; pass this to the controller.
```

```python
observed = cc.endpoint.inspect_endpoint(address)
credential = observed['credential']
# The controller retains the credential while managing its child.
child.kill()
child.wait()
result = cc.endpoint.reap_endpoint(address, credential)
```

For bounded maintenance, use the `cc.endpoint.sweep_endpoints(addresses=[...], max_entries=64, max_ms=10)` context manager and `next_batch()`. Select only this run's logical addresses. Mark a round complete only when native `round_complete` is true; handle interrupted rounds and namespace changes separately. `c3 endpoint inspect/reap/sweep` calls the same native mechanisms. Sweep accepts repeated `--address`; use `--ipc-root` or `C2_IPC_ROOT` for a custom directory. Controllers do not derive OS paths or directly unlink endpoint files.

## Relay and platform boundaries

Relay freezes its local upstream context at startup: one relay local domain uses one root, and upstream resources share that context; separate local domains can use separate relays. The client sends a Core-derived local namespace identifier in discovery without exposing the raw root or granting authority. The `x-c2-local-namespace` header on `/_resolve` is the sole namespace source for the entire route batch; individual JSON routes no longer carry that field. Rust/Core accepts an IPC candidate only for a local anchor when this response header matches its local context and handshake identity, full route contract and generation checks all pass. A missing or mismatched header selects HTTP without assuming a default root or falling back to an old JSON field; duplicate or malformed headers are rejected. HTTP routes are not withdrawn for a namespace mismatch, and HTTP-only calls need no local endpoint capability. Windows namespace identity derives from backend and current user scope; the OS DACL, identity handshake and route contract enforce access.

A typed RPC handshake version mismatch on a relay-selected local IPC candidate is terminal: the client cleans up locally and rethrows the same error without trying further routes, HTTP fallback or automatic retry. Over HTTP, a relay whose own upstream IPC connection fails version negotiation projects the existing `ProtocolViolation` (502) with `local_protocol_version` / `peer_protocol_version` details naming the relay/upstream pair — the caller's local end is not that pair. See the [handshake spec](ipc-handshake.md).

Authoritative route acquisition queries current server state and binds the route UID and server instance. Idle eviction removes the data connection while retaining registration and owner state. An old proxy cannot acquire a replacement resource with the same name. Generated TypeScript persistent IPC clients use the same control lookup and identity rules.

## Observing physical IPC connections

Subscribe with `cc.observe.connection_events()` or Rust `Runtime::connection_events()` before making outgoing connections or starting a listener. The same bounded feed reports this Runtime's managed outgoing IPC connections and accepted connections, with `event.side` equal to `client` or `server`. Subscription starts no host, creates no identity and freezes no configuration; subscriptions survive legal pre-use configuration changes.

Python `events.recv(timeout=None)` returns a `ConnectionEvent`: `kind='opened'` carries a `ConnectionObservation` in `value`, and `kind='closed'` carries a `ConnectionClosed`. Both expose `connection_id` and `side`. The outgoing ID matches `cc.observe.connection_observation(proxy).connection_id`. Open means that the OS connection was established or accepted, not that handshake, contract validation or a business call succeeded. Each physical reconnect has a new ID. Several logical proxies may share one connection; closing a proxy is not a physical close and emits no event by itself.

```python
events = cc.observe.connection_events()  # subscribe before connecting
try:
    peers = [cc.connect(Grid, name='grid', address=address) for address in addresses]
    for event in events:
        if event.side == 'client' and event.kind == 'closed':
            print(event.connection_id, event.value.reason, event.value.operation)
finally:
    events.close()
```

`recv()` releases the GIL, returns `None` on timeout and raises `StopIteration` when the source or subscription ends. `events.close()` can be called from another thread to interrupt a blocked receive; it is idempotent, discards that subscription's queued metadata and does not close a transport or another subscription. A receive racing close may already have received an event; later receives are ended. Each subscription permits one receiver at a time; concurrent receivers raise `RuntimeError`. Applications use one consumer for many connections, without one blocking thread per connection. HTTP-only and same-process calls produce no outgoing IPC events; independent raw admin controls are not inserted into the Runtime's client cache.

There is no historical replay: a late subscriber can observe a later close without a synthetic open. The queue is allocated on first subscription and retains at most 1024 events. Overflow raises `ConnectionEventsLagged` with a `missed` count and permanently ends that subscription. Rust uses async `recv(&self)`, returning `Ok(None)` at end, `ConnectionEventsError::Lagged` on overflow and `ConcurrentReceive` for a second receiver. Subscriptions and event values retain metadata only, not Runtime, transport, resource or payload leases.

`cc.observe.connection_observation(proxy)` remains a read-only view of the current physical IPC incarnation; `snapshot()` immediately returns its terminal record or `None`. Thread-local and HTTP proxies reject this query with `UnsupportedCallMode`. Python `ConnectionObservation.wait_closed()` and the Core blocking `wait_connection_closed()` bridge are removed: use the event feed for waiting and the snapshot for late inspection. Rust's existing async `ConnectionObservation::wait_closed(Option<Duration>)` remains the cancellable single-connection primitive used by native close barriers. Its cancellation affects only that waiter, and its terminal record is the same one published in the feed.

Reasons are `local_close`, `peer_close_message`, `peer_eof`, `connection_reset`, `protocol_error` and `io_error`. Only an actual DISCONNECT message yields `peer_close_message`. EOF and reset do not prove process death. The first terminal fact is retained and the two ends can observe different reasons. Linux/macOS UDS and Windows Named Pipe receive events drive notifications even with no pending call. The notification is published when the physical stream retires: after its remote reads end and its loans are settled, or after its read side ends with the remaining loans left in the original pool. It still proves neither cleanup nor process exit: use the original native shutdown/close outcomes for cleanup completion, OS wait for process exit and captured credentials for exact reap. Observations do not release HeldResult owners.

Replaced objects, unknown endpoints, corrupt records and incomplete initialization report `unverified`. Age, PID and connection failure cannot authorize removal. Processes in one communication group use the same build and final-directory configuration. The new path directly replaces the old layout; existing processes, old credentials and historical directories are not migrated or swept automatically. Windows maintenance reports kernel ownership or `not-applicable`. Recovery from a crash at every possible point in initial registration is outside the guarantee. Validation reports identify the environments actually exercised.

See the historical [0.7.4 guide](../releases/0.7.4.md). Earlier [canonical endpoint validation](../acceptance/canonical-local-endpoint-validation.md) and [endpoint validation](../acceptance/local-endpoint-final-validation.md) retain their historical sources and scopes.

Slot stat failures retain the existing I/O outcome. If a missing-lease sweep slot's socket stat fails, the batch increments `io_errors` and retains `last_io_error`, rather than counting it as `already_absent`. Inspection reports a final lease-stat error as `io-error`, while an already-known socket identity mismatch still takes precedence; missing/replaced entries and gate fences retain their behavior. Python uses the existing kind/errno and `retryable=True` projection; that flag starts no automatic retry. CLI sweep `complete` still denotes directory EOF, so callers must inspect the error counts separately. Windows KernelManaged/NotApplicable and credential JSON are unchanged.

Lease verification and record reads also preserve native I/O causes, including split-directory slot checks, fstat/stat, open and read. Maintenance no longer classifies those failures as InvalidOwnership. Known file-type, owner, mode, identity and malformed-record refusals keep their order. Bind retains InvalidData and its original message for policy refusals, while passing through actual I/O errors. Failed explicit close still releases the listener/lease and leaves the record for identity-checked reap; Drop and bind rollback retain their cleanup conditions without added retries.
