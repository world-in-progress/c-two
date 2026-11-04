# IPC RPC handshake and fixed version preamble

This page specifies RPC IPC handshake version 15 and the boundary for future protocol changes. The fixed preamble is unchanged since version 14 (C-Two 0.10.0).

Handshake 14 (C-Two 0.10.x) and the preamble-free handshake 13 (0.9.x) are incompatible. [ADR 0001](../decisions/0001-single-owner-release.md) records what version 15 changed; see the [design plan](../acceptance/2026-10-25-ipc-version-mismatch.md), [0.10.0 release notes](../releases/0.10.0.md), and [acceptance record](../acceptance/2026-10-26-ipc14-version-diagnostics-acceptance.md) for the preamble's implementation background, three-platform gates, mixed-version boundary, and latency evidence.

## Scope

This specification governs only `HANDSHAKE` version negotiation on ordinary RPC connections: business handshakes initiated by Rust/Python clients and generated TypeScript clients over Unix UDS or Windows Named Pipes. SIGNAL ping/shutdown control does not use this preamble and does not change with the RPC version. Diagnosability of different IPC versions applies only to RPC connections using this fixed preamble; it does not imply that any local channel or unresponsive process can report a version.

## Permanent fixed preamble (25 bytes)

Before selecting a version, both ends exchange one fixed 25-byte record. Its identification format is version independent: even if future business frames or metadata layouts change, this initial envelope must still be recognized by these bytes and must not change with the business-frame codec.

| Bytes | Fixed meaning |
| --- | --- |
| 0–3 | `u32 LE total_len = 21`, excluding these four bytes |
| 4–11 | `u64 LE request_id = 0` |
| 12–15 | Request flags `0x04`; response flags `0x06`; no additional flags accepted |
| 16–19 | Fixed magic: ASCII `C2IP` |
| 20 | `1 = Hello`, `2 = Accepted`, `3 = VersionMismatch` |
| 21–24 | Sender's protocol version as `u32 LE`, in the range 1–4294967295 |

Each side first reads the fixed 16-byte envelope and checks `total_len=21`, `RID=0`, and exact direction flags, then reads the fixed 9-byte content and checks magic, kind, and version. Do not use length-driven payload allocation or TypeScript `readIpcFrame` for an unknown preamble. Reject version zero as malformed. The fixed preamble is independent of `max_frame_size`; metadata, business, and other control frames still obey their original limits, and only a preamble satisfying both fixed length and flags is exempt. Versions are compared only for equality, with no downgrade negotiation. The preamble has no variable-length diagnostic text, supported-version list, alternate version, reserved field, or configuration switch.

## Negotiation sequence and RPC phase fence

After the existing OS connection and native identity checks, the client sends only the Hello preamble and waits for a fixed response; it sends no SHM names, route requests, or business data yet. On a version mismatch, the server writes VersionMismatch carrying the server version; on equality, it writes Accepted. Only after receiving Accepted with an equal version does the client send its metadata: only its pool prefix and real directional SHM capability, with no segment list. The server then performs the full validation and produces the identity/route/capability ACK with its own prefix; identity and route fields belong to this server ACK. The one-byte version field in the old metadata is removed; the fixed preamble alone owns the version boundary.

Initial negotiation exclusively reads Hello and metadata in one handshake call stack. Each stage checks RID, direction, and exact flags. It does not return to the ordinary dispatch loop while waiting for metadata and does not add a second persistent state machine. Only after the final metadata ACK is written does the server publish `handshake_done` and `ordinary_protocol`, opening business calls and release frames. Before handshake completion, CALL and ordinary CTRL frames other than independent SIGNAL control terminate the connection with `frame_before_handshake`; a second HANDSHAKE on an established connection terminates it with `duplicate_handshake`. No RPC channel bypasses version negotiation.

## Malformed records versus VersionMismatch

A malformed record is a protocol error, not a version fact. Incorrect envelope length, RID or flags; incorrect magic; unknown kind; version zero; truncation; contradictory records (Accepted with a different version or VersionMismatch with the local version); peer crash, EOF, and timeout retain the existing decode/protocol/I/O classifications and do not fabricate `peer_protocol_version`. Only a completely decoded VersionMismatch with a peer version different from the local version yields a typed version-mismatch error carrying both `u32` values. Lower layers must not extract versions from error text.

## Deadlines, terminal outcome, and close

Rust/Python clients place both handshake stages under the caller's existing single absolute `ConnectAttempt` deadline and handshake phase guard; neither stage receives a fresh budget. Generated TypeScript retains its existing connection interface. This change adds no handshake timer or setting and cannot promise an overall deadline for an unresponsive custom connection. A version mismatch is terminal: it does not enter stale-pool reconnection, relay HTTP fallback, or background retry, and it does not revoke other valid shared connections. After reading the fixed refusal, the client constructs the structured error and closes locally without waiting for server EOF. Server preamble reads, response writes, and metadata reads retain shutdown cancellation.

The server creates one absolute `DEFAULT_CONNECT_TIMEOUT` (five-second) upper bound when it starts sending a rejection. Writer acquisition, `write_all`, asynchronous flush, and bounded peer-close wait share its remaining time and can be canceled by shutdown; no step gets a new budget. Version-rejection peer-close wait continues until client close, shutdown cancellation, or this budget's expiry, without the earlier control-ACK 50 ms window. Do not call synchronous `FlushFileBuffers` and occupy a Tokio worker: Microsoft documents that it waits for the client to consume buffered data, while `DisconnectNamedPipe` discards unread data. Asynchronous flush only drains existing adapter writes and does not prove client consumption.

## Identity before version

Unix directory/lease identity, Windows pipe-object ownership, process TokenUser and integrity, and the process pin all precede sending the preamble; do not bypass those checks to obtain a version. Missing listeners and permission or identity refusals retain their original classifications. A version fact reports only successfully read content and does not prove a same-user process is a trusted application. Application-instance identity remains validated in the same-version metadata ACK in its original order; a version mismatch does not claim that this instance was verified.

## Public error surface

Python continues to catch only `c_two.error.CoreError`. The class declares `local_protocol_version: int | None` and `peer_protocol_version: int | None`, initialized to `None` on every instance, and adds `category='protocol_version_mismatch'`. Only that category has two values in 1–4294967295; every other error has `None` for both (for example, local 15 and peer 14). The mismatch has `transport_kind='ipc'`, `transport_phase='pre_dispatch'`, and `fallback_eligible=False`; `code`, `name`, `error_bytes`, and all three OS-cause fields are `None`. It adds no CRM semantic error code or business error wire.

Rust wire decoding produces only a checked preamble. `c2_ipc::IpcError::ProtocolVersionMismatch { local: u32, peer: u32 }` and Core `Error::ProtocolVersionMismatch { local: u32, peer: u32 }` are the only typed version-mismatch branches; Core normalization maps directly, and the Rust SDK reuses Core Error without a wrapper. Exhaustive matches must include them. Malformed records retain `DecodeError` / `Protocol` classification and the pre-dispatch phase. Generated TypeScript retains `C2IpcTransportError`, adding read-only `category: "protocol_version_mismatch" | null` and nullable `localProtocolVersion` / `peerProtocolVersion`. If local close fails, retain the primary error and version fields and attach the close exception to standard `Error.cause`. `C2IpcConnection` write/readExactly/close and HTTP-only loading are unchanged. See the [error reference](errors.md) for the surrounding error families.

## Relay boundary

For relay-selected local routes, Core treats a typed version mismatch as terminal: after local cleanup it rethrows the original error without trying later routes, converting it to FallbackDenied, using generic connection-failure fallback, or retrying automatically. A relay connecting to its upstream resource over IPC has a distinct version pair. An upstream mismatch is projected by the relay as the existing `ProtocolViolation` (HTTP 502), with `route`, `transport=ipc`, `local_protocol_version`, and `peer_protocol_version` details. These identify the relay/upstream pair; the caller's own local version does not change because a remote route mismatches.

## Compatibility

RPC handshakes before version 14 lack this format. A new endpoint does not parse an old payload, try an old pipe name, or infer a version from EOF or error code 2. Mixed connections with an old endpoint, including 0.9.x and earlier wheels, remain connection/protocol failures; the old wheel can observe only a generic connection failure, without typed projection. Low-level manual handshake callers must use the new sequence, and the old single-frame RPC handshake entry point is not retained. Version 15 keeps the preamble, the frame envelope and CRM hashes. It removes the metadata segment lists and route revisions, gives SHM descriptors a `loan_id`, and adds the release frame (`0x17`); it moves together with SHM 4 and C/FFI ABI 7. A version 14 peer receives a typed VersionMismatch from a version 15 endpoint and the reverse.

## Fixed Windows naming

The Windows pipe name is `\\.\pipe\c_two-{scope}-{identity}`. `scope` is the full 64-character lowercase SHA-256 hex digest of the UTF-8 bytes of the process TokenUser SID text (such as `S-1-5-…`). `identity` is the 32-character lowercase hex digest of the first 16 bytes of SHA-256 of the server_id's original UTF-8 after removing `ipc://`, preserving case. Listener uniqueness is maintained by a user-restricted mutex named with `Global\c_two_listener-` followed by the lowercase SHA-256 hex digest of the pipe name encoded as UTF-16LE, with each UTF-16 code unit written in little-endian byte order. Names contain neither package nor IPC protocol versions. Protocol or package upgrades must not change the TokenUser source, SID text, server_id encoding or case, SHA-256 truncation, name prefixes or separators, or introduce a version-query bypass, broadcast, registry file, or secret broker.

## Cost boundary

The preamble adds one local round trip and a pair of 25-byte records to each new physical connection; calls on an established connection add no round trip. A mismatch needs only the Hello/VersionMismatch round trip and sends or waits for no variable metadata. The current generated TypeScript relay-aware local branch creates a new transport for every call and closes it afterward, so every call through that entry bears the added handshake round trip and must be measured separately. This specification does not change its connection reuse policy. Measured latency for successful first connection, sustained reuse, and version rejection, and each platform's acceptance results, belong to the measurement record; this page states no numbers in advance.
