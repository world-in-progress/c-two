# Relay Mesh Discovery — Plan Review Report

**Plan:** `docs/superpowers/plans/2026-07-20-relay-mesh-discovery.md`
**Spec:** `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`
**Date:** 2026-07-20
**Scope:** Phase 1 (§1.1–§1.10) — Spec vs Plan cross-reference

## Summary

Systematic cross-reference of the 15-task implementation plan against the Phase 1 design spec,
validated by independent rubber-duck review. Found **20 real issues** (3 Critical, 8 High,
7 Medium, 2 Low) and **2 false alarms** (removed). Three additional issues were caught by
rubber-duck review that the initial pass missed.

### Well-Covered Areas (no issues)

- RouteTable + ConnectionPool + RelayState split (Tasks 2-4) — clean separation
- RouteEntry / RouteInfo / PeerInfo type definitions (Task 1) — match spec
- Deterministic resolution ordering: LOCAL first, then PEER by (registered_at, relay_id) (Task 2)
- PeerEnvelope / PeerMessage with protocol_version (Task 7) — all Phase 1 variants present
- Disseminator trait + FullBroadcast abstraction (Task 7)
- All 6 `/_peer/*` endpoints structurally correct (Task 8)
- Background tasks with CancellationToken lifecycle (Task 9)
- Duplicate name detection with ping (Task 10) — matches spec §1.3
- Error types: ResourceNotFound(701) / ResourceUnavailable(702) / RegistryUnavailable(705) (Task 11)
- Route cache with 30s TTL + threading.Lock (Task 12) — free-threading safe
- Upsert semantics for routes (Task 2) — correct primary key (name, relay_id)
- Dead-peer probe with recovery + immediate digest exchange (Task 9) — matches spec §1.6

---

## Removed False Alarms

| # | Original Finding | Why False |
|---|------------------|-----------|
| — | No SQLite storage | Spec Open Questions (line 742-745) explicitly says "in-memory for Phase 1 (simplicity)" |
| — | `Suspect` state not in spec | Spec line 95-98 includes `Alive \| Suspect \| Dead` — was present all along |

---

## 🔴 CRITICAL — Blocks Correct Operation

### C-1: Gossip broadcast never wired into /_register and /_unregister handlers

- **Spec:** §1.3 — register/unregister must broadcast route changes to all peers via gossip
- **Plan:** Task 10, lines 2308-2310 — comment says "Gossip announce to peers.
  (Disseminator broadcast will be wired in Task 12 integration.)"
- **Problem:** Task 12 is Python `cc.connect()`, not Rust gossip wiring. **No task** wires
  `disseminator.broadcast()` into the register/unregister axum handlers.
- **Impact:** CRM registrations update local RouteTable but **never propagate to peers**.
  Peers only learn about new routes via anti-entropy (60s default interval).
- **Fix:** Add an explicit wiring step (new task or extend Task 10) that calls
  `disseminator.broadcast(RelayAnnounce { ... })` inside `handle_register` and
  `disseminator.broadcast(RelayLeave { ... })` inside `handle_unregister`.

### C-2: RelayState doesn't hold Disseminator — handlers can't broadcast

- **Spec:** lines 118-122 — `RelayState` contains `disseminator: Box<dyn Disseminator>`
- **Plan:** Task 4, lines 924-928 — `RelayState` only has `route_table`, `conn_pool`, `config`
- **Problem:** Even if C-1 were fixed, axum handlers receive `State<Arc<RelayState>>` but
  have **no access** to the disseminator. It's only passed to `spawn_background_tasks` (Task 9).
- **Impact:** Structurally impossible for any handler to gossip.
- **Fix:** Add `disseminator: Arc<dyn Disseminator>` to `RelayState`. Update Task 4 struct
  definition and Task 9/10 initialization code.

### C-3: cc.unregister() doesn't notify relay — routes persist forever

- **Spec:** line 720 — "Extend register()/unregister() to notify relay"
- **Plan:** Task 13 — only adds relay notification for `cc.register()` (lines 2570-2633)
- **Problem:** Python `cc.unregister()` never calls `/_unregister` on the relay. The stale
  route stays in the relay's RouteTable indefinitely.
- **Impact:** **Worse than a peer timeout issue** — this is a local CRM route, so peer
  heartbeat timeout will NOT clean it up. The route is permanently stale until relay restart.
- **Fix:** Add `/_unregister` notification in Task 13's `unregister()` path, mirroring the
  register notification logic. Include the same background retry mechanism.

---

## 🟠 HIGH — Significant Spec Deviation

### H-1: cc.connect() has no fallback loop

- **Spec:** lines 460-470 — iterate all routes, try each address on connection failure
- **Plan:** Task 12, lines 2528-2537 — takes only `routes[0]`
- **Fix:** Implement `for route in routes: try connect; except: continue; raise` loop.

### H-2: Join protocol missing "announce to all learned peers"

- **Spec:** step 8 (lines 242-244) — after join, joiner must announce to ALL learned peers
- **Plan:** Task 9, lines 2148-2160 — `seed_retry_loop` contacts seed and merges snapshot,
  but never announces to the peers discovered in that snapshot
- **Fix:** After `merge_snapshot()`, iterate learned peers and POST `/_peer/announce` to each.

### H-3: Anti-entropy DigestDiff missing `extra` deletion set

- **Spec:** line 293 — `DigestDiff { missing, extra }` where `extra` identifies routes
  that should be deleted (convergent deletion)
- **Plan:** Task 7, lines 1430-1432 — `DigestDiff` only has `entries` field;
  Task 8, lines 1766-1772 — `handle_peer_digest` computes `missing_from_us` but drops it
- **Impact:** Deletions never converge across the mesh. A removed route persists on peers
  that missed the original deletion event.
- **Fix:** Add `extra: Vec<(String, String)>` to `DigestDiff`. In `handle_peer_digest`,
  process `extra` entries by removing them from local RouteTable.

### H-4: Background retry uses fixed 5s × 60 instead of exponential backoff

- **Spec:** line 434 — "exponential backoff, max 60s"
- **Plan:** Task 13, lines 2618-2629 — `sleep(5)` for 60 iterations
- **Fix:** Replace with exponential: 1s → 2s → 4s → ... → 60s cap.

### H-5: Background retry not cancellable by cc.unregister()

- **Spec:** line 434 — "until relay reachable OR cc.unregister() called"
- **Plan:** Task 13, lines 2608-2633 — daemon thread has no cancellation mechanism
- **Fix:** Add a `threading.Event` cancel flag. Check in retry loop. Set on `unregister()`.

### H-6: Forward-compatibility broken by strict enum deserialization *(rubber-duck)*

- **Spec:** lines 275-301 — unknown message types should be "warn and ignore"
- **Plan:** Task 7, lines 1383-1433 / Task 8, lines 1625+ — `Json<PeerEnvelope>` with
  tagged `PeerMessage` enum fails deserialization on unknown variants (400 error)
- **Impact:** Any future Phase 2/3 message type sent to a Phase 1 relay breaks the peer.
- **Fix:** Deserialize `PeerMessage` as `serde_json::Value` first, match known variants,
  log warning and return 200 for unknown. Or use `#[serde(other)]` catch-all variant.

### H-7: Heartbeat route_count computed incorrectly *(rubber-duck)*

- **Spec:** line 93 — `route_count: u32` = "Number of CRMs on **this** relay"
- **Plan:** Task 9, line 1950 — uses `state.route_names().len()` which counts unique names
  across the **entire** RouteTable (all relays), not just local CRMs
- **Fix:** Add `local_route_count()` method that filters by `relay_id == self_id`.

### H-8: Task 15 tests conflict with Task 10 behavior *(rubber-duck)*

- **Plan:** Task 10, lines 2301-2306 — `handle_register` opens real `IpcClient::connect()`
  and returns 502 on failure
- **Plan:** Task 15, lines 2816-2824, 2892-2899, 2932-2938 — tests POST fake `ipc://`
  addresses and expect 200
- **Impact:** Tests will fail as written because fake addresses can't be connected to.
- **Fix:** Either (a) make IPC validation optional/configurable for tests, or
  (b) use real IPC servers in integration tests, or (c) separate validation into
  a dedicated endpoint.

---

## 🟡 MEDIUM — Functionality Gap

### M-1: Join protocol TOCTOU protection absent

- **Spec:** lines 237-249 — write lock + gossip queueing + replay after snapshot merge
- **Plan:** Task 4, lines 442-475 — relies on RwLock implicit serialization only
- **Risk:** Concurrent register during join could be lost. Mitigated by write-lock
  serialization in practice, but spec's explicit queue/replay is safer.
- **Recommendation:** Document the deviation as accepted risk, or implement queue/replay.

### M-2: No passive failure detection from RPC forwarding

- **Spec:** lines 363-364 — forwarding failure marks peer as dead immediately
- **Plan:** Task 6 `call_handler` returns 502 but doesn't update peer status
- **Nuance:** Relay mesh is single-hop design; passive detection has limited applicability.
- **Recommendation:** Add `mark_dead()` call on forwarding failure as low-cost improvement.

### M-3: Missing `C2_RELAY_ANTI_ENTROPY_INTERVAL` Python setting

- **Spec:** lines 353, 721 — required configuration for anti-entropy interval
- **Plan:** Task 11, lines 2394-2411 — only adds `relay_seeds` to `C2Settings`
- **Fix:** Add `relay_anti_entropy_interval: float = 60.0` to `C2Settings` with
  `C2_RELAY_ANTI_ENTROPY_INTERVAL` env var.

### M-4: CLI `--upstream` still required — blocks mesh-only mode

- **Spec:** lines 494-500, 709 — show relay startup without `--upstream` (mesh-only mode)
- **Plan:** Task 14, lines 2692-2710 — `--upstream` has `required=True`, startup calls
  `register_upstream(upstream)` immediately
- **Fix:** Make `--upstream` optional. When absent, start in mesh-only mode using seeds.

### M-5: No `cc.set_relay()` API

- **Spec:** lines 384-390 — `cc.set_relay(address)` as programmatic alternative to env var
- **Plan:** Not mentioned in any task
- **Fix:** Add `set_relay()` to registry.py + export from `__init__.py`. It should set
  the relay address used by `cc.register()` and `cc.connect()` route lookups.

### M-6: Protocol version check only in handle_peer_announce

- **Spec:** line 301 — ALL `/_peer/*` handlers must validate protocol_version
- **Plan:** Task 8, lines 1630-1636 — only `handle_peer_announce` checks version
- **Fix:** Extract version check into a shared middleware or helper function; apply to
  all 6 peer handlers (announce, join, heartbeat, leave, digest, sync).

### M-7: No RelayLeave broadcast on graceful shutdown

- **Plan:** Task 7, lines 1416-1418 — defines `RelayLeave` message variant
- **Plan:** Task 8, lines 1712-1721 — defines `handle_peer_leave` receiver
- **Problem:** No code path sends `RelayLeave` when the relay shuts down gracefully.
  Task 9 cancels background tasks but doesn't announce departure.
- **Fix:** Add shutdown hook that broadcasts `RelayLeave` before cancelling tasks.

---

## 🟢 LOW — Minor Deviation

### L-1: `_ipc_address_override` naming mismatch

- **Plan:** Task 14, line 2638 — references `_ipc_address_override`
- **Code:** `registry.py` line 114 — actual field is `_explicit_address`
- **Fix:** Update plan to reference `_explicit_address`.

### L-2: Auto-address format missing PID

- **Spec:** line 400 — `cc_auto_{pid}_{uuid8}`
- **Plan:** Task 14, lines 2645-2649 — `cc_auto_{uuid16}`
- **Fix:** Include PID in format for easier debugging: `cc_auto_{pid}_{uuid8}`.

---

## Self-Review Checklist Corrections

The plan's self-review checklist (lines 2993-3004) marks **all items** as `[x]`.
Based on this review, several should be `[~]` (partial):

| Checklist Item | Status | Reason |
|----------------|--------|--------|
| §1.2 RouteTable | `[~]` partial | Anti-entropy missing deletion convergence (H-3) |
| §1.4 Peer Discovery | `[~]` partial | Missing announce-to-all (H-2), TOCTOU risk (M-1) |
| §1.5.1 Anti-Entropy | `[~]` partial | DigestDiff missing `extra` set (H-3) |
| §1.6 Failure Detection | `[~]` partial | No passive detection path (M-2) |
| §1.7 Config | `[~]` partial | Missing ANTI_ENTROPY_INTERVAL setting (M-3) |
| §1.9 Registry Integration | `[~]` partial | No fallback loop (H-1), no unregister notify (C-3), no backoff (H-4) |
| §1.10 CLI | `[~]` partial | `--upstream` still required (M-4) |

Items not listed above are correctly marked `[x]`.

---

## Priority Recommendations

**Must fix before implementation begins (3 items):**

1. C-1 + C-2 together: Add `disseminator` to `RelayState`, wire broadcast into
   `handle_register` / `handle_unregister`. Without this, gossip doesn't work.
2. C-3: Add relay notification for `cc.unregister()`. Without this, routes are immortal.

**Should fix during implementation (8 items):**

3. H-1: Fallback loop in `cc.connect()`
4. H-2: Announce to all learned peers after join
5. H-3: Anti-entropy deletion convergence
6. H-4 + H-5: Exponential backoff + cancellation for retry
7. H-6: Forward-compatible message deserialization
8. H-7: Correct `route_count` computation
9. H-8: Test/behavior alignment for register validation

**Address during implementation (7 items):**

10. M-1 through M-7: Functionality gaps (config, CLI, API, protocol checks, shutdown)

**Cosmetic fixes (2 items):**

11. L-1, L-2: Naming corrections

---

## Methodology

1. **First pass:** Section-by-section cross-reference of spec §1.1–§1.10 against all 15 plan tasks
2. **Code verification:** Grep/view of `registry.py`, `settings.py`, `__init__.py` to confirm
   current state matches plan assumptions
3. **Second pass:** Line-by-line re-read of full spec (748 lines) and full plan (3018 lines)
4. **Rubber-duck validation:** Independent agent reviewed all findings against source code,
   confirmed 17/19 original findings, identified 2 false alarms, and discovered 3 new issues
