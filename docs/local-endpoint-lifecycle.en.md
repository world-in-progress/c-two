# Local endpoint lifecycle and controller integration

English · [简体中文](local-endpoint-lifecycle.md)

Rust `c2-config` derives endpoints from logical `ipc://` addresses. SDKs, relay and c3 consume the same context. Unix defaults to `/tmp/c2-v3-<uidhex>/<32-character id>`, with a nonce coordinator and removable listener leases. The original coordinator inode remains pinned until listener lease ownership ends. Windows uses the current TokenUser SID's Named Pipe and a non-inheritable kernel listener lease. The OS selects the backend; credential `managed-v2` / `named-pipe` fields identify it.

Windows connections authenticate the connected pipe owner and its pinned server process before any data/control bytes. TokenUser must match, and the server must be at least Medium integrity. Listener startup adds only same-user process-query/synchronize and token-query rights, allowing Medium and High peers across logons without granting process control or token duplication. A Global user-restricted mutex owns listener uniqueness; Local mappings are used only when the native peer session and negotiated direction permit them. Cross-session calls use owned/inline/chunked transfer, including prepared payloads. See the [0.9.0 migration notes](releases/0.9.0.md) for the required offline upgrade and ticket-context cut.

## Directory selection and credentials

Select the final Unix directory through `cc.set_local_endpoint(root=...)`, c3 `--ipc-root`, or `C2_IPC_ROOT`. Code/CLI overrides take precedence over process environment, `.env` and the platform default. A custom directory is application-provisioned and owned by the current user. Mode `0755` is accepted when that user has read, write and traversal access and group/other users cannot write. C-Two creates its default directory with mode `0700` and verifies socket mode `0600` before listening. Endpoint names have 32 lowercase hexadecimal characters, with no version subdirectory or socket suffix. Linux/macOS bind, connect and probe operations use the directory descriptor and short name; spaces, Unicode and long directories are supported within filesystem limits. Cleanup retains the application directory and unrelated files. Windows rejects Unix root overrides.

macOS also checks the extended ACL on the open directory descriptor. Read, traversal and deny entries are accepted; allow entries granting entry creation, deletion, attribute changes or permission changes are rejected, including inherited entries. A failed startup explicitly unlocks its listener lease even while duplicate file descriptors survive.

For the macOS default namespace, sockets stay in `/tmp/c2-v3-<uidhex>`; only `.gate`, `.gate.marker` and `<id>.lease` move to `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`, the system-provided non-temporary user directory resolved during native I/O. Aliases of an existing default socket directory share the coordinator by native directory identity. An explicit alias whose directory is absent remains application-provisioned. Linux and non-default custom roots keep colocated records.

Both directories are verified under the same gate, and the listener pins the original directory handles until lease ownership ends. Normal close/Drop retires its socket and lease; abnormal exit uses the existing exact reap, bounded sweep or subsequent bind. When reboot or system cleanup removes the socket directory, a complete unlocked lease can still be retired without recreating that directory. A live lease still blocks a second listener. Replaced directories and invalid records remain unverifiable.

Unix namespace disk format 3 is shared by the gate, marker and lease record. The fixed `c2mgatv2` / `c2mgrec2` magic still identifies the managed-v2 backend. The gate is 25 bytes, the marker 33 bytes and the lease record's fixed header 51 bytes. Persistent identity omits device and the record no longer stores `RECORD_PROTOCOL`. Live directory/file checks still use dev+ino; owner `context_digest` and its domain remain unchanged. A recognizable different gate/marker version reports `unsupported-namespace-format` before old content can be interpreted or adopted.

Endpoint leases use those same native retirement paths; failed initialization removes only newly created entries. Invalid historical leases remain for separate handling. Gate/marker identities are permanent during online operation, with no new background sweeper. Versioned default directories keep different namespace disk formats apart: the new build neither reads nor deletes old default directories, and no same-path cold switch is needed. Upgrade the Python and Rust SDKs, c3 and Node consumers on one host together. For historical manual cleanup, first ensure every old process has exited and cannot restart. Explicitly sharing one custom physical root across disk formats is unsupported. If that root reports `unsupported-namespace-format`, rebuild only after all users have exited, cannot restart and the entire root has been safely archived offline. Identity checks still prevent deleting the other format's endpoints if roots are misconfigured, but counts and reasons can be inaccurate.

On Unix bind initialization failure, rollback removes only a lease created by that attempt whose original directory, gate and file identities still match. If the attempt bound a socket, that socket must first have been safely withdrawn. Pre-existing leases, replacements and unverifiable objects remain. This rollback uses creation proof still held by the current call; it does not expand historical invalid-record reap/sweep authority or guarantee cleanup after a process crashes during initialization.

Runtime freezes the endpoint context before its first local bind/connect attempt or owner takeover preparation, including failure. Changing the frozen root reports a configuration error; later environment changes do not affect existing listeners, clients, credentials or sweeps. Pure context queries perform no filesystem I/O. Different directories form distinct native contexts, while connections still validate server identity, instance identity and route contracts. Root configuration does not move SHM, file spill or configuration files. See the [configuration guide](configuration.en.md).

Unix credentials use schema 4, recording final root, namespace id, incarnation and socket inode/ctime without device; all schema 3 documents are rejected. Encoding, decoding and CLI reading share a 32 KiB bound. Windows credentials use kernel-managed schema 1; inspection is `not-applicable` and reap is `NotApplicable`. Maintenance uses the recorded context; a mismatched explicit root is rejected before target access. Native configuration derives the full socket path; the document contains no caller-selected socket path. A credential describes an object and does not itself grant deletion authority.

## Bind and connect failures

Since 0.8.1, startup deadline expiry has `CoreError.lifecycle_kind='startup_timeout'`, `startup_stage` and `timeout_seconds`. Local bind failure messages no longer carry the incorrect `config error` prefix; their existing OS fields are preserved. Configure the wait with `cc.set_server(startup_timeout=...)` before registration. See [startup deadlines](configuration.en.md#server-startup-deadlines) for the finite default, freezing and cleanup boundary.

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
| `lifecycle_kind` | `invalid_server_id`, `config_frozen`, `client_config_frozen`, `duplicate_route`, `missing_route`, `relay_duplicate_route`, `missing_relay_address`, `relay_http`, `register_failure`, `held_response_copy`, `held_response_release`, `response_copy`, `configuration`, `server`, `startup_timeout`, `relay` or `None` |
| `startup_stage` | `readiness`, `responsiveness`, `owner_control` or `None` |
| `timeout_seconds` | `float` or `None`, configured startup budget in seconds |
| `route_name`, `body` | `str` or `None`, affected route or HTTP response diagnostic |
| `status_code` | `int` or `None`, HTTP status (including 409 for duplicate registration) |
| `relay_duplicate` | `bool` or `None`, relay duplicate-registration classification |
| `failure_source` | `relay_projection`, `relay_prepare`, `commit`, `relay_register`, `route_open` or `None` |
| `registration_failure`, `rollback`, `relay_cleanup_error` | `dict[str, object]` or `None`, existing registration transaction outcomes |

`registration_failure` contains `route_name`, `failure_source`, `error_message`, nullable `status_code`, `rollback` and `relay_cleanup_error`. `rollback` contains `route_name`, `local_removed`, `active_drained`, `closed_reason` and `close_error`; `relay_cleanup_error` contains `route_name`, nullable `status_code` and `message`. These outcomes report the original transaction; observing them does not repeat cleanup. Ordinary imports do not load FastDB or NumPy.

A peer that negotiates a different RPC handshake version raises `CoreError` with `category='protocol_version_mismatch'`, `transport_kind='ipc'`, `transport_phase='pre_dispatch'` and `fallback_eligible=False`; `code`, `name`, `error_bytes` and the three OS-cause fields stay `None`. The mismatch is a terminal fact with no route or HTTP fallback and no retry, and malformed/truncated preambles keep the ordinary transport/protocol classifications instead of fabricating a peer version. See the [handshake spec](ipc-handshake.md).

## Persistent and OwnerBound resources

The owner API and grace semantics below apply to C-Two 0.8.0; see the [release and migration notes](releases/0.8.0.md). The default `Persistent` lifecycle keeps resources available after ordinary client disconnects and relay eviction. Use `OwnerBound` only for a dedicated child whose entire Runtime belongs to one controller. `control, receiver = cc.owner_control_pair()` returns `OwnerControl` and a one-use `NativeOwnerBootstrap`; Rust returns `OwnerControl` and `OwnerBootstrap`. Retain control and transfer only the bootstrap to the intended child with `cc.spawn_owned_child(receiver, program, args)`; `receiver` remains the parameter name. The child explicitly calls `bootstrap = cc.adopt_owner_stdin()` and selects `cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0), owner_control=bootstrap)` before registration.

Core arms the private duplex relationship before readiness. Unix uses a nonblocking CLOEXEC socketpair; Windows uses a local current TokenUser SID duplex byte-mode Named Pipe, registering the bootstrap with its reactor/IOCP only after transfer to the child. A trusted launcher supplies the inherited capability; ordinary business stdin or arbitrary handles cannot be promoted to owner authority. Late bootstrap attachment cannot bypass ticket recovery. Restarting a terminal OwnerBound host requires a fresh Runtime and bootstrap; an old ticket cannot authorize a new server instance. Ticket requests made before readiness wait within their original request budget until the endpoint is ready to issue a bound capability.

`control.close()`, destruction of control or controller exit ends the relationship. Native observation of its termination enters `OwnerMissing`, pauses host admission and starts the configured grace period. The current valid owner's `control.request_shutdown(timeout=...)` instead requests native drain immediately; success acknowledges shutdown initiation without waiting for business work or process exit. Ordinary direct IPC admin shutdown receives a negative ACK on OwnerBound hosts, including pre-handshake and duplicate initiate requests, and cannot bypass owner revocation. Persistent admin behavior and trusted in-process `Runtime.shutdown()` / `cc.shutdown()` retain their existing semantics.

During `OwnerMissing`, published routes remain `Ready` with the same route UID/revision and server instance; catalog and relay registration remain present. `connect()` may succeed while calls report existing `ResourceUnavailable`, including Python thread-local calls through the same native gate. Already admitted work and held/borrowed leases retain their lifetime rules. Old pending requests cannot gain admission by sleeping through pause and resume. Takeover restores only the host gate: it neither re-registers routes nor reopens explicitly closed/unregistered routes. Late relay final responses still require their original registration token's identity and admission generation checks. A sent business frame retains its original transport phase/retry rules; `ResourceUnavailable` alone neither proves pre-dispatch failure nor authorizes business replay.

Takeover requires `OwnerMissing` and monotonic `now < original grace deadline`. At or after the deadline it is refused and native draining begins even if the timer has not run yet. Wrong tickets, partial requests, competing attempts and inspect cannot extend that deadline. Successful commit atomically consumes the old grant, installs the successor grant, advances the owner epoch and transfers supervision. At most one contender using a ticket wins; old tickets and old-epoch commands, EOF and timers lose authority over the new owner. `WatcherIoError`, `Draining` and `Finished` cannot recover through takeover.

Only the current session/epoch can issue a ticket with `control.recovery_ticket(timeout=...)` or idempotently revoke the current grant with `control.revoke_recovery(timeout=...)`. Repeated issuance returns the same current grant; issuance after revocation creates a new secret. `OwnerRecoveryTicket` has a redacted repr and no implicit pickle support. Explicit `ticket.export()` includes its secret; import with Rust `OwnerRecoveryTicket::import(&bytes)` or Python `cc.OwnerRecoveryTicket.from_bytes(data)`. The application must protect and durably save these bytes outside the controller. C-Two creates no secret file or background recovery worker.

Configure the recovering controller's endpoint through the existing configuration API before `cc.prepare_owner_takeover(ticket)`; Rust uses `Runtime::prepare_owner_takeover(ticket)`. Preparation freezes that Runtime's existing endpoint configuration and captures native directory identity, including on failure; it neither starts a host nor adopts a directory from the ticket. Tickets bind the logical address, server instance, owner epoch and native endpoint context identity. Aliases of the same directory are checked by native identity rather than path spelling. Wrong roots/instances and directory replacement are rejected, with identity revalidated during inspect/commit and no automatic default-root selection. Recovery tickets and endpoint credentials are not instructions to adopt historical or replacement directories.

Prepare, optionally call `prepared.inspect(timeout=...)`, save `prepared.successor_ticket.export()`, then call `prepared.commit(timeout=...)` once. Inspect observes current identity/epoch/phase; it is neither authorization nor a receipt proving this transaction succeeded. Commit revalidates authority and returns the new `OwnerControl` on success. Python exposes `successor_ticket` and `recovery_ticket` as properties; Rust uses methods of those names. The successor becomes valid only when the server atomically installs it, and both tickets remain exportable from the prepared object after every outcome. In the short example below, `save_ticket(name, data)` and `load_ticket(name)` are application-provided durable storage functions; the keys illustrate one recovery attempt. Keep the original ticket and successor separately, and finish saving before commit so they remain readable after controller exit. Use separate records for later attempts without overwriting saved tickets that might still be valid.

```python
# Original controller: the child is Ready; control came from cc.owner_control_pair().
save_ticket("current", control.recovery_ticket(timeout=1.0).export())
```

```python
import c_two as cc

# Recovering controller: the original host's endpoint context is configured.
ticket = cc.OwnerRecoveryTicket.from_bytes(load_ticket("current"))
prepared = cc.prepare_owner_takeover(ticket)
status = prepared.inspect(timeout=1.0)  # Observation only, never proof of takeover.
save_ticket("successor", prepared.successor_ticket.export())
control = prepared.commit(timeout=1.0)  # One commit per object; retain control.
```

Control failures raise `cc.OwnerControlError`. Native peer-identity refusal during inspection has `kind="identity_mismatch"`. If commit has not sent its request, it has `kind="not_sent"` and `cause_kind="identity_mismatch"`; ordinary OS access failures are not classified as an identity mismatch. Python `error.kind` / `error.cause_kind` correspond to Rust `kind()` / `cause_kind()`. `NotSent` (`not_sent`) means the mutating request did not begin sending. `Indeterminate` (`indeterminate`) means sending began without a valid confirmation or explicit rejection; the candidate session closes, and neither success nor non-execution can be inferred. Complete refusals retain definite kinds such as `owner_present`, `invalid_grant`, `grace_expired`, `identity_mismatch` and `shutting_down`. The example propagates errors. Each prepared object permits only one commit regardless of outcome; retrying after NotSent also requires new preparation. The application must retain both tickets and their epoch information. After an uncertain result, inspect may observe the current epoch/phase, and the application can explicitly choose a saved original or successor ticket for a new prepared attempt. The original remains valid only if unconsumed; after consumption only the authorized successor is valid, and a competing winner or revocation can invalidate the application's tickets. Never automatically try old/new keys in sequence or treat an epoch change as proof that this transaction won.

Each control operation has one native absolute budget and each session has one in-flight request. A waiter timing out before acquiring the serial slot cannot close another operation's session; expiry after acquiring the slot but before the first write also preserves a healthy session. Losing confirmation after writing begins terminates that dedicated session so a late reply cannot satisfy a later request. Candidate setup is bounded by the earlier of the native 5-second limit and the original grace deadline; partial frames and retries never reset it, and no configuration option is added. Owner control uses independent version 1 messages, bounded to 4 KiB within the existing frame envelope. The current transport uses IPC handshake 14 with the permanent fixed version preamble (see the [handshake spec](ipc-handshake.md)); SHM 3 and C/FFI ABI 6 are unchanged. Owner control remains version 1; business frame/call/reply layouts are unchanged. Rust exhaustive matches must add `MsgType::OwnerControl`, `SchedulerAcquireError::Unavailable` and `RouteConcurrencyError::Unavailable` branches. Superseded single-direction owner types, formats and aliases are removed without compatibility paths or downgrade to ordinary admin, HTTP or relay.

`OwnedChild.poll()`, `wait(timeout=...)` and `kill()` observe or control the process. `OwnedChild.close()` releases the observer; the native shared reaper continues OS process reclamation. Its lifecycle effect differs from `OwnerControl.close()`. There is no always-running system monitor required to manage every endpoint.

`cc.shutdown(timeout=...)` reports native structured completion. An accepted owner shutdown or Persistent admin shutdown ACK proves initiation, without proving that callbacks have drained or shutdown hooks can run. With `completed=False`, the session, bridge, routes and hooks remain available for observing the same transaction again. Listener closure, active work and retained payload leases are separate facts. Endpoint cleanup does not release held or borrowed payloads; their own lease release invalidates FastDB checked views.

Persistent direct IPC admin shutdown retries transient connection failures only before attempting to write the control request. After writing starts, a write failure, lost ACK or timeout returns the existing unconfirmed result without reconnecting and resending; an unconfirmed result does not prove that the server did not execute the request. A replacement instance at the same address therefore cannot receive a replay from this call. Ping retains its probe retries; initial endpoint absence handling and the original total budget are unchanged.

## Exact cleanup and bounded sweeps

After listener readiness, the host can call `cc.server_credential() -> EndpointCredential | None` and pass that credential to its controller. It reads the listener value already published through the lifecycle watch; it does not inspect the endpoint, perform filesystem I/O, take the namespace gate or wait for readiness. Ready and Stopping with a retained listener return a value; before start, after bind failure and in terminal Stopped/Failed states it returns `None`. Between run cancellation and existing lifecycle finalization, it may return the previous snapshot. A controller can also use `cc.inspect_endpoint(address)` when the coordinator gate is available. Neither result proves liveness or grants cleanup or owner authority; `WouldBlock` indicates contention. After normal exit, or after `kill()` plus an OS `wait()` confirms exit, call `cc.reap_endpoint(address, credential)`. An old credential cannot remove a new incarnation. On Windows the getter returns kernel-managed schema 1; reap remains `NotApplicable`.

`cc.EndpointCredential` is the immutable native value returned directly by the getter or inspection and accepted by reap. Share its reference within a process; for storage or transfer, explicitly use `credential.to_json()` and `cc.EndpointCredential.from_json(document=...)`. Both retain the sole Rust codec's strict validation and 32 KiB bound; inspection does not guarantee successful JSON export. Manual construction, subclassing, mutation and shallow copying are unsupported. The Python wrapper, its `_native` slot and the old `_native.PyEndpointCredential` export are removed; see the [unreleased migration notes](releases/unreleased.md).

```python
import c_two as cc

cc.register(Grid, GridResource(), name="grid")
credential = cc.server_credential()  # The listener is ready; pass this to the controller.
```

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

Relay freezes its local upstream context at startup. Upstream resources share that directory; separate local domains can use separate relays. Discovery carries the native namespace identifier, without exposing a raw root or granting authority. A local anchor may offer an IPC candidate only when namespaces, handshake identities and the full route contract match. A namespace mismatch selects HTTP and preserves the route. Windows namespace identity derives from backend and current user scope; the OS DACL, identity handshake and route contract enforce access.

A typed RPC handshake version mismatch on a relay-selected local IPC candidate is terminal: the client cleans up locally and rethrows the same error without trying further routes, HTTP fallback or automatic retry. Over HTTP, a relay whose own upstream IPC connection fails version negotiation projects the existing `ProtocolViolation` (502) with `local_protocol_version` / `peer_protocol_version` details naming the relay/upstream pair — the caller's local end is not that pair. See the [handshake spec](ipc-handshake.md).

Authoritative route acquisition queries current server state and binds the route UID/revision and server instance. Idle eviction removes the data connection while retaining registration and owner state. An old proxy cannot acquire a replacement resource with the same name. Generated TypeScript persistent IPC clients use the same control lookup and identity rules.

## Observing physical IPC connections

Subscribe with `cc.connection_events()` or Rust `Runtime::connection_events()` before making outgoing connections or starting a listener. The same bounded feed reports this Runtime's managed outgoing IPC connections and accepted connections, with `event.side` equal to `client` or `server`. Subscription starts no host, creates no identity and freezes no configuration; subscriptions survive legal pre-use configuration changes.

Python `events.recv(timeout=None)` returns a `ConnectionEvent`: `kind='opened'` carries a `ConnectionObservation` in `value`, and `kind='closed'` carries a `ConnectionClosed`. Both expose `connection_id` and `side`. The outgoing ID matches `cc.connection_observation(proxy).connection_id`. Open means that the OS connection was established or accepted, not that handshake, contract validation or a business call succeeded. Each physical reconnect has a new ID. Several logical proxies may share one connection; closing a proxy is not a physical close and emits no event by itself.

```python
events = cc.connection_events()  # subscribe before connecting
try:
    peers = [cc.connect(Grid, name='grid', address=address) for address in addresses]
    for event in events:
        if event.side == 'client' and event.kind == 'closed':
            print(event.connection_id, event.value.reason, event.value.operation)
finally:
    events.close()
```

`recv()` releases the GIL, returns `None` on timeout and raises `StopIteration` when the source or subscription ends. `events.close()` can be called from another thread to interrupt a blocked receive; it is idempotent, discards that subscription's queued metadata and does not close a transport or another subscription. A receive racing close may already have received an event; later receives are ended. Each subscription permits one receiver at a time; concurrent receivers raise `RuntimeError`. Applications use one consumer for many connections, without one blocking thread per connection. HTTP-only and same-process calls produce no outgoing IPC events; independent raw admin/owner controls are not inserted into the Runtime's client cache.

There is no historical replay: a late subscriber can observe a later close without a synthetic open. The queue is allocated on first subscription and retains at most 1024 events. Overflow raises `ConnectionEventsLagged` with a `missed` count and permanently ends that subscription. Rust uses async `recv(&self)`, returning `Ok(None)` at end, `ConnectionEventsError::Lagged` on overflow and `ConcurrentReceive` for a second receiver. Subscriptions and event values retain metadata only, not Runtime, transport, resource or payload leases.

`cc.connection_observation(proxy)` remains a read-only view of the current physical IPC incarnation; `snapshot()` immediately returns its terminal record or `None`. Thread-local and HTTP proxies reject this query with `UnsupportedCallMode`. Python `ConnectionObservation.wait_closed()` and the Core blocking `wait_connection_closed()` bridge are removed: use the event feed for waiting and the snapshot for late inspection. Rust's existing async `ConnectionObservation::wait_closed(Option<Duration>)` remains the cancellable single-connection primitive used by native close barriers. Its cancellation affects only that waiter, and its terminal record is the same one published in the feed.

Reasons are `local_close`, `peer_close_message`, `peer_eof`, `connection_reset`,
`heartbeat_timeout`, `protocol_error` and `io_error`. Only an actual DISCONNECT
message yields `peer_close_message`. EOF, reset and heartbeat timeout do not
prove process death. The first terminal fact is retained and the two ends can
observe different reasons. Existing Linux/macOS UDS and Windows Named Pipe
receive events and heartbeat drive notifications even with no pending call.
Notification precedes work drain and memory release: use the original native
shutdown/close outcomes for cleanup completion, OS wait for process exit and
captured credentials for exact reap. Observations neither release Held results
nor turn an ordinary disconnect into OwnerBound owner loss.

Replaced objects, unknown endpoints, corrupt records and incomplete initialization report `unverified`. Age, PID and connection failure cannot authorize removal. Processes in one communication group use the same build and final-directory configuration. The new path directly replaces the old layout; existing processes, old credentials and historical directories are not migrated or swept automatically. Windows maintenance reports kernel ownership or `not-applicable`. Recovery from a crash at every possible point in initial registration is outside the guarantee. Validation reports identify the environments actually exercised.

See the [owned-child Rust example](../sdk/rust/examples/owned_child.rs) and historical [0.7.4 guide](releases/0.7.4.md). Earlier [canonical endpoint validation](reports/canonical-local-endpoint-validation.md) and [endpoint validation](reports/local-endpoint-final-validation.md) retain their historical sources and scopes.

Slot stat failures retain the existing I/O outcome. If a missing-lease sweep slot's socket stat fails, the batch increments `io_errors` and retains `last_io_error`, rather than counting it as `already_absent`. Inspection reports a final lease-stat error as `io-error`, while an already-known socket identity mismatch still takes precedence; missing/replaced entries and gate fences retain their behavior. Python uses the existing kind/errno and `retryable=True` projection; that flag starts no automatic retry. CLI sweep `complete` still denotes directory EOF, so callers must inspect the error counts separately. Windows KernelManaged/NotApplicable and credential JSON are unchanged.

Lease verification and record reads also preserve native I/O causes, including split-directory slot checks, fstat/stat, open and read. Maintenance no longer classifies those failures as InvalidOwnership. Known file-type, owner, mode, identity and malformed-record refusals keep their order. Bind retains InvalidData and its original message for policy refusals, while passing through actual I/O errors. Failed explicit close still releases the listener/lease and leaves the record for identity-checked reap; Drop and bind rollback retain their cleanup conditions without added retries.
