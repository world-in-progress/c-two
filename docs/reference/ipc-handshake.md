# IPC RPC handshake, version preamble and call records

This page specifies RPC IPC version 16: the fixed version preamble, the handshake and the call records that carry calls. The fixed preamble is unchanged since version 14 (C-Two 0.10.0).

Handshake 15 (the single-owner release), 14 (C-Two 0.10.x) and the preamble-free handshake 13 (0.9.x) are incompatible. [ADR 0001](../decisions/0001-single-owner-release.md) records what version 15 changed, and the [streaming design](../design/2026-11-03-streaming-rpc-design.md) (section 7 and [ADR 0003](../decisions/0003-server-streams.md)) what version 16 changed; see the [design plan](../acceptance/2026-10-25-ipc-version-mismatch.md), [0.10.0 release notes](../releases/0.10.0.md), and [acceptance record](../acceptance/2026-10-26-ipc14-version-diagnostics-acceptance.md) for the preamble's implementation background, three-platform gates, mixed-version boundary, and latency evidence.

## Scope

This specification governs only `HANDSHAKE` version negotiation on ordinary RPC connections: business handshakes initiated by the Rust client, which the Python SDK and generated TypeScript clients also use through their native bindings, over Unix UDS or Windows Named Pipes. SIGNAL ping/shutdown control does not use this preamble and does not change with the RPC version. Diagnosability of different IPC versions applies only to RPC connections using this fixed preamble; it does not imply that any local channel or unresponsive process can report a version.

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

Each side first reads the fixed 16-byte envelope and checks `total_len=21`, `RID=0`, and exact direction flags, then reads the fixed 9-byte content and checks magic, kind, and version. Do not use length-driven payload allocation for an unknown preamble. Reject version zero as malformed. The fixed preamble is independent of `max_frame_size`; metadata, business, and other control frames still obey their original limits, and only a preamble satisfying both fixed length and flags is exempt. Versions are compared only for equality, with no downgrade negotiation. The preamble has no variable-length diagnostic text, supported-version list, alternate version, reserved field, or configuration switch.

## Negotiation sequence and RPC phase fence

After the existing OS connection and native identity checks, the client sends only the Hello preamble and waits for a fixed response; it sends no SHM names, route requests, or business data yet. On a version mismatch, the server writes VersionMismatch carrying the server version; on equality, it writes Accepted. Only after receiving Accepted with an equal version does the client send its metadata: only its pool prefix and real directional SHM capability, with no segment list. The server then performs the full validation and produces the identity/route/capability ACK with its own prefix; identity and route fields belong to this server ACK. The one-byte version field in the old metadata is removed; the fixed preamble alone owns the version boundary.

Initial negotiation exclusively reads Hello and metadata in one handshake call stack. Each stage checks RID, direction, and exact flags. It does not return to the ordinary dispatch loop while waiting for metadata and does not add a second persistent state machine. Only after the final metadata ACK is written does the server publish `handshake_done` and `ordinary_protocol`, opening call records. Before handshake completion, call records and ordinary CTRL frames other than independent SIGNAL control terminate the connection with `frame_before_handshake`; a second HANDSHAKE on an established connection terminates it with `duplicate_handshake`. No RPC channel bypasses version negotiation.

## Call records (version 16)

After the handshake, every call travels as call records. A record frame is the existing 16-byte envelope, all little-endian: `total_len` (u32, excluding itself), `request_id` (u64) and `flags` (u32), followed by one record-kind byte and the record body.

| Flag | Value | Meaning |
| --- | --- | --- |
| Call record | `0x1000` | Set on every call record; never combined with HANDSHAKE, CTRL or SIGNAL |
| Response | `0x0002` | Server to client |
| SHM | `0x0040` | The value in this frame is one 23-byte SHM 4 descriptor |
| Chunked | `0x0200` | This frame carries one chunk of a value |
| First chunk | `0x2000` | Only with Chunked: the first chunk, which carries the record metadata and the total length |

Any other bit is rejected; IPC 15 used `0x0400` for its last chunk, and version 16 does not reuse it.

| Kind | Record | Body after the kind byte | Direction and `request_id` |
| --- | --- | --- | --- |
| 1 | OPEN | identity, method index (u16), input value | Client to server, non-zero |
| 2 | ITEM_BEGIN | item length (u64) | Server to client, non-zero; server streams only |
| 3 | ITEM_DATA | one part of an item, inline or one SHM descriptor | Server to client, non-zero; server streams only |
| 4 | CREDIT | storage bytes (u64), item demand (u64) | Client to server, non-zero; server streams only |
| 5 | CANCEL | cause (u8): Consumer 0, Lifecycle 1 | Client to server, non-zero; server streams only |
| 6 | CALL_END | status (u8), result or error | Server to client, non-zero; Success and Failed of a server-stream call also carry its count of complete items (u64) after the status |
| 7 | RELEASE | count (u8, 1 to 64), loan IDs (u64 each) | Both directions, `request_id` 0 |

The identity in OPEN is the seven route and contract fields `route_name`, `route_uid`, `crm_ns`, `crm_name`, `crm_ver`, `abi_hash` and `signature_hash`, each a u8 byte length followed by UTF-8 text, validated as before. OPEN carries no mode and no initial demand: the mode comes from the validated contract, and a server stream starts with demand for one item.

CALL_END has one status: Success (0), Failed (1) or NotExecuted (2). Failed and NotExecuted carry a non-empty inline C2E1 error. NotExecuted means the call never entered the resource: the server sends it only while the call is still being prepared or queued, so a client can report it as pre-dispatch even though OPEN was written. Failed means the resource ran. NotExecuted never carries an item count, so a receiver reads the count only when the status is Success or Failed and the call's validated mode is a server stream. A server stream's CALL_END is one inline frame, so its body after the kind byte is at most 4294967282 bytes; a relay keeps the same bound for the terminal records of the HTTP streams it forwards.

A value is sent whole when it fits: inline values run to the end of the frame, and an SHM value is exactly one descriptor. A value too large for one frame is chunked: the first chunk carries the record metadata, the total length (u64) and the first part, which must be shorter than the total; every later chunk carries only the kind byte and its part, and is exactly as long as the first part or the remainder. Chunks carry no index or count. The receive loop assigns each chunk's offset from one running count when it admits the chunk, so copies may finish in any order; the value is complete when every byte has been copied. Small unary calls use one OPEN and one CALL_END frame.

Request IDs are assigned by the connection's single writer when it first commits a call or a control that expects a reply, so OPEN records reach the peer in strictly increasing order; IDs never repeat or wrap, and gaps are allowed. ID 0 is for RELEASE and controls without a reply. The server keeps the largest OPEN ID it has seen, the request-ID high-water mark, and a table of active calls. A new OPEN must exceed the mark. A chunk for an ID at or below the mark that is no longer active is accepted at the frame boundary, its SHM descriptor is released to the original connection, and it is discarded without a new call or another reply; a chunk for an ID above the mark is a protocol error. The client dispatches records by its own table of active calls, so a late reply can never be taken for a later call's response. Loans are settled by RELEASE independently of call outcomes, and a late RELEASE still settles on its original connection.

Protocol errors in well-framed records, such as a repeated OPEN, deactivate the connection: it takes no new calls, and calls already sent finish and reply. If the server cannot send the rest of a chunked CALL_END after its first chunk has been written, it deactivates the connection without sending a second CALL_END; the other calls on the connection still finish, and the client reports the incomplete call as dispatch-uncertain when the connection ends.

A frame write that exceeds `write_timeout` deactivates the connection, but a call record the writer has already accepted keeps writing, and the call's next chunk, item and CALL_END follow that frame's real result.

## Server streams

A server stream opens with one OPEN, and the validated contract gives its mode. Once the server admits it, the stream has demand for one item. The server runs a resource step only while the stream has demand, and each step yields one item, finishes or raises. A call has at most one unfinished item.

Each item starts with ITEM_BEGIN, which carries the item's length; an item of length zero is complete at ITEM_BEGIN and has no ITEM_DATA. On ITEM_BEGIN the client reserves the whole length in its payload buffer budget and only then sends CREDIT for that many storage bytes; while the budget is short only this call waits, and the receive loop goes on with other frames. An item longer than the whole budget fails at once: the consumer receives `CallCapacityExceeded` and the client sends CANCEL, keeping the call's record until CALL_END. Once storage has been granted, the server sends the item either as one SHM ITEM_DATA that carries the whole item and uses up all storage granted so far, or as inline ITEM_DATA parts, each no longer than the storage still granted and at most `chunk_size` long, so other calls' frames pass between them. A client that consumes the item grants the whole item at once. A relay that forwards the stream grants one copy window at a time, so it can receive a whole SHM item under a smaller grant; it copies that item in slices and ends its read range after the last one. An SHM ITEM_DATA that does not carry the whole item, or an item that mixes SHM and inline parts, is a protocol error and deactivates the connection; its SHM descriptor is still released to the original connection. Inline ITEM_DATA carries no chunk flags and no offset: each part continues where the item's received bytes end. The client copies a whole SHM item outside its receive loop, so it keeps reading other calls' frames during the copy, and it delivers the call's CALL_END only after that item.

When the client places a complete item in its local consumer interface, which holds one item, it grants demand for the next item; a relay grants it when its HTTP response body asks for the next record. A client therefore holds at most one delivered item that has not been taken and one more it has granted. Storage and demand waiting to be sent travel together in one CREDIT. CREDIT may not grant more storage than the rest of the current item or raise demand above one; such a CREDIT, an ITEM_BEGIN without demand and ITEM_DATA beyond the granted storage are protocol errors.

A stream ends with one CALL_END after its cleanup has run. A stream that runs out sends Success with its count of complete items. An error from a step comes after the items already sent, as Failed with the count, and a cleanup error replaces the outcome it follows. CANCEL is a request to stop: a step that has not started does not start, a running step runs to its end, and then the cleanup runs without waiting for demand or storage. CANCEL carries its cause. A Consumer CANCEL also gives up the item being sent: the server ignores CREDIT after it, and an item abandoned partway is not delivered; its late ITEM_DATA records still settle on the original connection, which releases their SHM descriptors and drops their data. A Lifecycle CANCEL, which a relay sends when it shuts down, stops the stream the way Runtime shutdown does: CREDIT still counts and the item a step has already produced is still sent. Runtime shutdown and route unregistration stop a stream the same way without a CANCEL record. A cancelled stream ends with `CallCancelled` (718) after its cleanup, as NotExecuted when it never entered the resource and otherwise as Failed with the count; when the server or a relay ended it, the message names shutdown, unregistration or relay shutdown. A cancellation recorded while the last step ran ends the stream with `CallCancelled` even if that step ended the stream; one recorded after the natural end was decided does not change it. A second CANCEL keeps the first cause, except that a Consumer CANCEL after a Lifecycle one still gives up the item being sent.

A stream that has entered the resource, and an item waiting for demand or storage, is never collected for being silent. Deactivation stops new calls; admitted streams continue, and the connection closes physically only after its call records, admitted writes, remote read ranges and loans have ended. If the peer ends, the server runs the cleanup after the current step and sends no CALL_END; the client reports a stream without CALL_END as dispatch-uncertain, and the items it already delivered remain valid.

The connection's single writer sends RELEASE, CREDIT and CANCEL between complete frames, takes turns among writable calls one chunk or item part at a time, so one large item cannot hold the connection, and never sends a call's CALL_END ahead of that call's data.

## Malformed records versus VersionMismatch

A malformed record is a protocol error, not a version fact. Incorrect envelope length, RID or flags; incorrect magic; unknown kind; version zero; truncation; contradictory records (Accepted with a different version or VersionMismatch with the local version); peer crash, EOF, and timeout retain the existing decode/protocol/I/O classifications and do not fabricate `peer_protocol_version`. Only a completely decoded VersionMismatch with a peer version different from the local version yields a typed version-mismatch error carrying both `u32` values. Lower layers must not extract versions from error text.

## Deadlines, terminal outcome, and close

Clients place both handshake stages under the caller's single absolute `ConnectAttempt` deadline and handshake phase guard; neither stage receives a fresh budget. Generated TypeScript clients set that deadline through the native connect timeout. There is no separate handshake timer or setting. A version mismatch is terminal: it does not enter stale-pool reconnection, relay HTTP fallback, or background retry, and it does not revoke other valid shared connections. After reading the fixed refusal, the client constructs the structured error and closes locally without waiting for server EOF. Server preamble reads, response writes, and metadata reads retain shutdown cancellation.

The server creates one absolute `DEFAULT_CONNECT_TIMEOUT` (five-second) upper bound when it starts sending a rejection. Writer acquisition, `write_all`, asynchronous flush, and bounded peer-close wait share its remaining time and can be canceled by shutdown; no step gets a new budget. Version-rejection peer-close wait continues until client close, shutdown cancellation, or this budget's expiry, without the earlier control-ACK 50 ms window. Do not call synchronous `FlushFileBuffers` and occupy a Tokio worker: Microsoft documents that it waits for the client to consume buffered data, while `DisconnectNamedPipe` discards unread data. Asynchronous flush only drains existing adapter writes and does not prove client consumption.

## Identity before version

Unix directory/lease identity, Windows pipe-object ownership, process TokenUser and integrity, and the process pin all precede sending the preamble; do not bypass those checks to obtain a version. Missing listeners and permission or identity refusals retain their original classifications. A version fact reports only successfully read content and does not prove a same-user process is a trusted application. Application-instance identity remains validated in the same-version metadata ACK in its original order; a version mismatch does not claim that this instance was verified.

## Public error surface

Python continues to catch only `c_two.error.CoreError`. The class declares `local_protocol_version: int | None` and `peer_protocol_version: int | None`, initialized to `None` on every instance, and adds `category='protocol_version_mismatch'`. Only that category has two values in 1–4294967295; every other error has `None` for both (for example, local 16 and peer 15). The mismatch has `transport_kind='ipc'`, `transport_phase='pre_dispatch'`, and `fallback_eligible=False`; `code`, `name`, `error_bytes`, and all three OS-cause fields are `None`. It adds no CRM semantic error code or business error wire.

Rust wire decoding produces only a checked preamble. `c2_ipc::IpcError::ProtocolVersionMismatch { local: u32, peer: u32 }` and Core `Error::ProtocolVersionMismatch { local: u32, peer: u32 }` are the only typed version-mismatch branches; Core normalization maps directly, and the Rust SDK reuses Core Error without a wrapper. Exhaustive matches must include them. Malformed records retain `DecodeError` / `Protocol` classification and the pre-dispatch phase. Generated TypeScript retains `C2IpcTransportError`, adding read-only `category: "protocol_version_mismatch" | null` and nullable `localProtocolVersion` / `peerProtocolVersion`. If local close fails, retain the primary error and version fields and attach the close exception to standard `Error.cause`. `C2IpcConnection` write/readExactly/close and HTTP-only loading are unchanged. See the [error reference](errors.md) for the surrounding error families.

## Relay boundary

For relay-selected local routes, Core treats a typed version mismatch as terminal: after local cleanup it rethrows the original error without trying later routes, converting it to FallbackDenied, using generic connection-failure fallback, or retrying automatically. A relay connecting to its upstream resource over IPC has a distinct version pair. An upstream mismatch is projected by the relay as the existing `ProtocolViolation` (HTTP 502), with `route`, `transport=ipc`, `local_protocol_version`, and `peer_protocol_version` details. These identify the relay/upstream pair; the caller's own local version does not change because a remote route mismatches.

## Compatibility

RPC handshakes before version 14 lack this format. A new endpoint does not parse an old payload, try an old pipe name, or infer a version from EOF or error code 2. Mixed connections with an old endpoint, including 0.9.x and earlier wheels, remain connection/protocol failures; the old wheel can observe only a generic connection failure, without typed projection. Low-level manual handshake callers must use the new sequence, and the old single-frame RPC handshake entry point is not retained. Version 15 keeps the preamble, the frame envelope and CRM hashes. It removes the metadata segment lists and route revisions, gives SHM descriptors a `loan_id`, and adds the release frame (`0x17`); it moves together with SHM 4 and C/FFI ABI 7. A version 14 peer receives a typed VersionMismatch from a version 15 endpoint and the reverse. Version 16 keeps the preamble, the 16-byte frame envelope, SHM 4 descriptors and C/FFI ABI 7, and replaces the business frames, their call and reply controls, the chunk headers and the release control frame with the call records below; a version 15 peer and a version 16 endpoint receive a typed VersionMismatch from each other. Generated TypeScript clients embed the transport and must be regenerated.

## Fixed Windows naming

The Windows pipe name is `\\.\pipe\c_two-{scope}-{identity}`. `scope` is the full 64-character lowercase SHA-256 hex digest of the UTF-8 bytes of the process TokenUser SID text (such as `S-1-5-…`). `identity` is the 32-character lowercase hex digest of the first 16 bytes of SHA-256 of the server_id's original UTF-8 after removing `ipc://`, preserving case. Listener uniqueness is maintained by a user-restricted mutex named with `Global\c_two_listener-` followed by the lowercase SHA-256 hex digest of the pipe name encoded as UTF-16LE, with each UTF-16 code unit written in little-endian byte order. Names contain neither package nor IPC protocol versions. Protocol or package upgrades must not change the TokenUser source, SID text, server_id encoding or case, SHA-256 truncation, name prefixes or separators, or introduce a version-query bypass, broadcast, registry file, or secret broker.

## Cost boundary

The preamble adds one local round trip and a pair of 25-byte records to each new physical connection; calls on an established connection add no round trip. A mismatch needs only the Hello/VersionMismatch round trip and sends or waits for no variable metadata. The current generated TypeScript relay-aware local branch creates a new transport for every call and closes it afterward, so every call through that entry bears the added handshake round trip and must be measured separately. This specification does not change its connection reuse policy. Measured latency for successful first connection, sustained reuse, and version rejection, and each platform's acceptance results, belong to the measurement record; this page states no numbers in advance.
