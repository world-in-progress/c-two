# Windows user-scoped IPC and authenticated local connections

Status: C-Two 0.9.0 and c3 0.5.0 were published on 2026-10-25 after canonical release qualification. Implementation, targeted repair, real Windows evidence review, and developer-host cleanup are complete. The separately approved R06 repair retains its own baseline and performance evidence. See the [acceptance record](2026-10-25-090-windows-release-acceptance.md) for exact source commits, reused validation and remaining release gates.

## Scope and boundary

Windows endpoint identity comes from the current process primary token's `TokenUser`, shared across that user's SSH, console, RDP, and scheduled-task logons. `TokenSessionId` determines whether session-local memory mappings are mutually accessible; it no longer selects the endpoint. There is one automatic policy, with no old logon-derived name or ticket-context fallback.

The native local-connection boundary verifies the connected pipe object owner and the server process's `TokenUser` before returning any stream to IPC, ping, shutdown, owner recovery, or Node. It holds the server process handle while the stream lives, checks liveness and rechecks the pipe's recorded server PID. Identification SQOS is set when opening the pipe, before trusting it. Identity policy refusals use permission-denied without an invented OS error code; actual OS query failures retain their original cause. Neither result authorizes a retry or means that the target is absent.

The existing I/O error carries a typed peer-identity refusal. Owner inspection projects it as `identity_mismatch`; a commit that has not sent retains `not_sent` with that cause. The C/Node bridge exposes `IDENTITY_MISMATCH` (7), separately from ordinary `PERMISSION_DENIED` (6) and `IO_ERROR` (5). Classification uses the error type, not message matching or an assumption that every access denial proves an identity mismatch. Existing signatures, ABI 6 layouts and control/wire formats remain unchanged.

Concurrent first listeners serialize this process-lifetime setup and record success once; an unsuccessful setup propagates its original error and is not cached as success. Before publishing its first listener, a Windows server grants its own user SID `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` on its process and `TOKEN_QUERY` on its primary token. This permits a Medium client in another login to authenticate a High server whose original process ACL only admitted its logon or administrators. Existing ACEs remain; no terminate, memory-write, token-duplication or impersonation right is added. These read permissions last for the process lifetime, avoiding a second listener-lifetime authority. The actual Windows cross-session probe verifies both successful identity queries and continued denial of process control and token duplication.

Pipe and mapping security descriptors explicitly name the current user as owner and grant that user access. The listener mutex uses the global kernel namespace and the same user ACL, matching the machine-wide pipe namespace. Existing connected stream handles do not own this listener lease, so closing the old listener still permits restart while those handles survive. Foreign-owned pipe objects cannot be adopted by a new listener instance.

The private owner bootstrap remains a trusted launcher capability. Its creation and adoption use the same user helper and actual pipe-owner check. Adoption must still work when the launcher has already exited; it cannot require a live creator or reinterpret inherited raw handles as proof of launcher provenance. The owner context uses `c2.owner.windows-user-scope.v1`; old logon-bound ticket digests fail without a fallback. Ticket encoding and control protocol remain unchanged.

## Existing transport reuse

Mappings remain `Local\` objects and require no global-mapping privilege. Each connected native stream obtains the actual peer session from its pipe handle. A query error fails the connection; a known different session disables shared references while leaving normal owned, inline, and chunked transfer available.

IPC handshake 13 adds two directional capabilities: `CAP_SHM_REQUEST` and `CAP_SHM_RESPONSE`, expressed relative to the client. The server acknowledges only offered capabilities that the actual connection can support. Rust and generated TypeScript consume the same negotiation. This is a clean protocol cut: handshake 12 is rejected; business-frame layouts, CRM hashes and SHM header version 3 do not change.

Transport selection gates every SHM publication, including sized streams, preallocated requests and prepared responses. A preallocated request uses its existing owner and transaction lock to copy its requested bytes and release the unused SHM allocation before dispatching owned bytes. A prepared response retains pool-identity and payload-limit validation, then copies and releases through its original owner when SHM cannot be used. Neither path adds another release state machine. Receiver-side reassembly may still use its own SHM or file storage.

An unnegotiated incoming SHM reference is rejected before mapping, lease construction, or late/discard cleanup can open it. Combined flags, cancelled requests, unknown requests and malformed descriptors are part of this boundary. Successful connections remain reusable; a transmitted business request is never automatically replayed.

## Node and public impact

The existing native endpoint context also opens Windows pipes. The C bridge transfers an unregistered, verified overlapped pipe handle and its process pin. Node adopts the pipe through its own libuv CRT descriptor conversion and its existing `Socket` and connection reader; a Tokio-registered handle is never transferred to a second IOCP. Failure and late-close paths retain exact ownership.

C/FFI ABI 6 makes this verified Windows entry mandatory. The official Windows connector does not invoke raw socket-factory or endpoint-path overrides that bypass native verification. Real native context injection remains subject to the existing native handle type check. The generic generated transport interface remains available for caller-supplied transports; it does not become tied to Node or gain a second authentication registry. HTTP-only imports retain lazy native loading.

Windows endpoint naming is intentionally incompatible with the old default. Stop old services, upgrade all participants, then restart; do not search old names. Windows EndpointCredential JSON schema 1 remains a logical endpoint record interpreted in the current user namespace; it does not acquire an explicit persisted SID guarantee. Python's normal connection and server configuration surfaces require no new switch.

Same user SID is not a statement that all processes have equal privilege or integrity. A real Windows 11 probe confirmed that a non-elevated Low integrity fake server received connection bytes from 0.8.2. The user approved requiring a server integrity level of at least Medium, checked on the same pinned process before publication of the connected stream. Low and Untrusted servers are rejected; ordinary Medium and High processes may still connect across logons. AppContainer-specific behavior requires separate evidence rather than an assumption from SID equality. Named-pipe occupation can cause denial of service; this work adds no broker or name-distribution secret.

## Acceptance

- Baseline and candidate: distinct SSH logons with the same user, both while the first login is alive and after it exits, with token facts and a server proven to remain alive.
- A temporary ordinary account: fake-first, genuine-first and close/restart orders; record bytes received by the fake pipe and distinguish identity rejection from occupation preventing bind. Clean the exact account, profile, processes and task resources afterward.
- Two actual session IDs of the same user: one listener lease, retained-handle restart, small and large requests/responses, and evidence of the selected data path. Exercise preallocated requests and prepared responses as well as normal calls.
- Cross-logon owner recovery, rejection of old context digests and missing capabilities, existing grace/deadline/admission invariants, and private bootstrap early-parent-exit behavior.
- Rust/Python/Node regression, complete relay, Linux/macOS/Windows x64 and Windows ARM64 GitHub Actions for the exact candidate, task endpoint zero residue, and independent security/regression review before release.

Validation records will identify actual source commits, commands, systems and artifact hashes. Historical IPC directories are not test or cleanup targets. Rebuildable outputs are removed once no active test or build needs them.
