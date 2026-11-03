# Contracts and routing

This reference owns CRM contract identity, portable bindings, and direct IPC and relay route selection.

## CRM contracts and releases

Path: `sdk/python/src/c_two/crm/`

- CRM contracts are interface classes decorated with `@cc.crm(namespace='...', version='...')`.
- Only methods in the contract are exposed remotely.
- CRM route contracts are identified by route name plus the CRM namespace, CRM name, CRM version, ABI hash, and signature hash. Python may compute the descriptor/fingerprints from the CRM class, but Rust `c2-contract` validates the complete expected route contract at IPC and relay boundaries.
- A persistent CRM contract release is a Rust-validated `ContractReleaseRef` derived from canonical `c-two.contract.v2` content. It never contains a route name. `ExpectedRouteContract` is derived later from a validated release plus a runtime route name.
- Resource implementations are plain Python classes and are not decorated.
- Portable methods declare zero or one input and zero or one output with `@cc.transfer(input=FASTDB_SPEC, output=FASTDB_SPEC)`. Their Python parameter and return type is `fastdb4py.payload.Payload`; C-Two embeds each nested specification as an opaque JSON value and delegates it to FastDB Core.
- Python-only fallback values use pickle and must not be treated as portable descriptor or codegen inputs.
- C-Two 0.10.1 source uses FastDB 0.2.4; published releases keep the FastDB version recorded in their release notes. Python FastDB payloads require the optional `c-two[fastdb]` extra, pinned to `fastdb4py==0.2.4`. Ordinary imports, CRM definitions, calls and error wrapping must not import FastDB or NumPy. The dev group selects the extra for full tests; native Core/Rust pins are independent.
- FastDB retained response owners are selected by call-site `cc.hold(...)`; server-side borrowed inputs are selected only by `cc.register(..., input_lifetime={...: 'borrowed'})`.
- CRM methods can use `@cc.read` or `@cc.write`; writes are the default.
- `@cc.transfer(input=..., output=...)` is the only portable FastDB binding authoring surface. `@cc.transferable` and `@cc.transfer(buffer='hold')` are obsolete and must not be reintroduced as codec or lifetime selection mechanisms.
- `@on_shutdown` marks one public CRM contract method as a shutdown callback. The resource implements it normally; registration rejects implementation-only shutdown markers. It is not exposed through RPC.

## Portable contract v2 and artifact composition

The `c-two.contract.v2` descriptor has exactly `schema`, `crm`, `fingerprints`, and `methods` at its root. Rust validates CRM identity, unique ordered method names, read/write access, explicit parameter and return shapes, exact binding keys, and the derived lowercase SHA-256 fingerprints. An absent input binding requires no parameters; a FastDB input binding requires one payload parameter with a missing default. An absent output binding requires a `none` return; a FastDB output binding requires a payload return. Variadic parameters are outside the portable shape. Method order fixes transport method indexes. Rust treats each `fastdb` binding's `spec` as opaque JSON; FastDB Core validates its contents when code generation consumes it.

The ABI fingerprint covers CRM identity, ordered method names, and opaque input/output bindings. The signature fingerprint covers CRM identity, ordered method names, access, parameter metadata, and return metadata. JSON object key order is canonicalized for these outer hashes. FastDB's canonical specification SHA-256 remains a separate identity. Runtime choices such as hold, borrowed input, SHM coordinates, route token, and allocator policy do not enter the descriptor.

`c2-contract` bounds each externally supplied descriptor before and during parsing: 16 MiB source bytes, 1,000,000 JSON values, depth 128 with the root at depth one, 256 methods, and 16 MiB of cumulative canonical nested FastDB specification bytes. Every binding occurrence contributes to the nested total, including repeated equal specifications. `ContractError::LimitExceeded` identifies the metric, limit, observed value, and path. These control-plane limits do not cap call payloads.

`c2-codegen` delegates each nested specification to FastDB Core, retains the Core digest and structured error, verifies returned artifact hashes, and composes one deterministic tree for the selected Rust, Python, or TypeScript target. Equal FastDB specifications share a digest-named subtree. Artifact paths are relative, slash-separated ASCII with `[A-Za-z0-9._-]` segments; the composer rejects empty or traversal segments, Windows device names, case-folded duplicates, file/directory prefix collisions, and mismatched hashes. A path is limited to 1,024 bytes, a segment to 255 bytes, and each provenance owner/source field to 1,024 UTF-8 bytes. The default complete set is limited to 4,096 regular files and 256 MiB, with lexical path ordering. Publication writes and verifies a sibling staging tree and renames it to an absent destination. The destination parent must remain stable; publication promises atomic visibility, not power-loss durability or protection against hostile parent replacement.

## Core ownership and SDK parity

C-Two has a language-neutral Rust core and user-facing Rust and Python SDKs. Neither SDK is the canonical home for generic runtime mechanisms. The Rust SDK at `sdk/rust` is package `c-two`, imported as `c_two`, and is a thin facade over Core plus generated typed clients/services. The Python SDK owns Python domain logic, CRM authoring, Python resource invocation, serialization orchestration, and same-process direct-call glue. Rust `c2-core` is the single shared owner of route selection, client/host calls, retry classification, error normalization, transport lease ordering, and runtime lifecycle; the lower Core crates own memory, wire, contract, relay, and configuration mechanisms. PyO3/maturin projects that same Core into Python as `c_two._native`.

The Rust SDK may depend on the official `fastdb` Rust binding to adapt portable payloads, but `c2-core` must not depend on FastDB. Rust users continue to name `fastdb::Payload`; do not re-export it under a C-Two semantic namespace. Rust and Python must expose the same portable route, payload, error, and lifetime capabilities even when their language-level syntax differs.

## Direct IPC

Direct IPC is a complete standalone mode. `cc.connect(..., address='ipc://...')` must bypass relay discovery and remain usable when no relay is configured or a relay environment variable points at an unavailable server. Relay is only a discovery/forwarding projection above IPC; do not make relay the owner of IPC registration, scheduling, or connection establishment.

Python resource servers create an auto-generated `ipc://` address. Use `cc.server_address()` after registration only when a same-host process needs to connect directly.

## Relay discovery and forwarding

`C2_RELAY_ANCHOR_ADDRESS` is the SDK process's relay anchor for control-plane registration and name resolution. It is not necessarily the relay that will carry every data-plane call: after resolution, remote HTTP calls use the selected route's `relay_url` directly. Direct IPC selection from relay resolution is allowed only when the anchor endpoint is loopback/local; nonlocal relay responses must be treated as HTTP relay targets even if they include an `ipc_address`.

Relay resolution and data-plane calls are contract-scoped, not name-only. `cc.connect(CRMClass, name='...')` derives an `ExpectedRouteContract` from the CRM class and the route name. Runtime relay resolve, probe, and call paths must carry and validate the full route name, CRM tag, ABI hash, and signature hash. Do not add production APIs, fallback behavior, cache keys, or wire paths that resolve, probe, or call a CRM route by name alone. A future "find by name" or diagnostic relay-mesh lookup must be a separate discovery/admin surface that returns candidate route metadata and CRM tags; it must not be reused as the normal CRM call path.

Relay-discovered IPC fast paths must validate endpoint identity in Rust before accepting the IPC client. The relay response identity (`server_id` and `server_instance_id`) must match the identity returned by the IPC handshake, and then the requested route name must exist. Do not reintroduce Python-side or route-name-only trust decisions for relay IPC.

### Registration, mesh resolution, and peer state

The relay rejects a different process claiming a live local route name with HTTP 409 and semantic `ResourceAlreadyRegistered` (703). It probes an existing owner before replacement; a dead or route-missing owner may be replaced after the candidate passes IPC identity and route-contract attestation and a fresh final owner probe confirms the old state. Final commit fences the captured connection slot, owner generation, lease epoch, replaceability, and active-request count. Lease expiry alone does not surrender ownership, and owner freshness stays inside the owning relay rather than gossip. A rejected relay registration aborts or rolls back its local registration transaction. Without a relay, separate processes may register the same name; within one process a duplicate is rejected locally.

Different relays may advertise the same name. Contract-scoped resolution returns matching local routes first, followed by live peer routes ordered by `(registered_at, relay_id)`. The timestamp uses wall-clock time, so clock skew can affect which peer comes first. A peer message's `sender_relay_id` is a claim, not authentication. The relay checks known membership, claimed owner consistency, and protection of its local routes; peer snapshots omit local-only server identity and IPC address. Deployments must protect the internal `/_peer/*` control endpoints with their own network and node identity controls. An IP allowlist alone does not identify a relay, and this protocol has no HMAC peer authentication.

### Route bindings, lookup, and fallback

An endpoint connection is a carrier for calls to multiple routes on one server instance. A route-bound client retains its observed `route_uid`, complete expected contract, and server identity. Acquisition uses server `RouteLookup` authority rather than handshake data alone; calls carry `RouteCallIdentity`, which the server checks before invoking resource code. A new route registration gets a new route UID, while existing clients keep their binding until closed.

The server's dispatcher is the single route authority, projected by `RouteList` and exact `RouteLookup`. List improves visibility; lookup and call-time identity checks decide correctness. Route removal does not change an immutable `ContractReleaseRef` retained by a caller.

| Runtime route condition | Semantic result |
| --- | --- |
| No matching resource has been found | `ResourceNotFound` |
| A known resource is temporarily unreachable | `ResourceUnavailable` |
| An observed route was removed | `ResourceRemoved` |
| A route exists but rejects new calls | `ResourceClosed` |
| A bound route UID is stale | `RouteStale` |

Only a failure proven to occur before dispatch can trigger transport fallback or replay. A failed verified local IPC acquisition may select an independent relay path; returning to the same failed local endpoint is denied. Contract, identity, and protocol mismatches are terminal. A stale cached route can cause one fresh resolution before dispatch. A service error or a connection loss after possible dispatch is terminal for automatic replay because C-Two has no call deduplication identity. Transport I/O failures affect the endpoint connection and do not, by themselves, withdraw route authority.

### Relay payload forwarding

The HTTP relay validates route, expected CRM contract, route token, `Content-Length`, and the upstream route's `max_payload_size` before reading the request body. Known-length requests stream through the route-bound IPC call with checked size admission; unknown-length requests are materialized under the lesser of 64 KiB and the route payload limit. `c2-http` delegates IPC SHM allocation and release to `c2-ipc`. Replay requires a replayable body and a proven pre-dispatch failure. Responses are currently materialized into HTTP bytes; the request streaming path does not imply response streaming. `C2_REMOTE_PAYLOAD_CHUNK_SIZE` controls remote body batching independently of IPC wire chunking.

The Python SDK does not embed or start a relay server. Start `c3 relay` outside the SDK, through the CLI, Docker Compose, Kubernetes, or other orchestration. Do not add SDK APIs that embed, start, or manage relay-server lifecycle.
