# Windows 2025 stale-route tests — cause and validation 2026-09-25

Scope: the two windows-2025 `c2-http --lib` failures recorded in `docs/reports/windows-ci-current-failures.md` — `relay::router::tests::call_missing_upstream_route_removes_stale_local_route` (router.rs:4896) and `relay::router::tests::probe_missing_upstream_route_removes_stale_local_route` (router.rs:4781), both asserting 404 while receiving 410 with successful stale-route eviction. No production code was changed; the defect is in the two test fixtures.

## Root cause

The relay HTTP layer deliberately maps two distinct upstream route-withdrawal answers:

| Upstream catalog answer | `IpcError` | Withdrawal reason | HTTP | Canonical error |
| --- | --- | --- | --- | --- |
| Route absent, never served by this owner | `RouteNotFound` | `route-missing` | 404 | `ResourceNotFound` "route not found: {route}" |
| Route was served and explicitly unregistered (tombstone) | `RouteRemoved { route_uid }` | `route-removed` | 410 | `ResourceRemoved` "route removed: {route}" + `route_uid` detail |

The server route catalog (`core/transport/c2-server/src/catalog.rs`) keeps a `RemovedRouteTombstone` per explicitly unregistered route (introduced in `384857e`, 2026-06-05). Before that commit, `Server::unregister_route` left no server-side memory, so any later authoritative lookup answered `NotFound` and these tests passed with 404 deterministically. Since `384857e`, the fixture's `stale_server.unregister_route("grid")` guarantees a tombstone, so the probe/call acquisition path deterministically classifies the failure as known-removed → 410.

The tests kept passing on Linux/macOS only through a second, asynchronous actor: the relay's background upstream control watch (`core/transport/c2-http/src/relay/upstream_control.rs`), spawned by the HTTP register handler. It polls its own IPC connection every 50 ms and validates the route against its cached route directory; when its handshake snapshot no longer contains the route it withdraws the local route with `reason=route_catalog_update`, after which the probe/call answers 404 "no local route" before any upstream acquisition. The tests were therefore racing the watch against the data-plane acquisition:

- Watch's snapshot observes the removal first → local route already withdrawn → 404 (observed locally on macOS: `[relay] Upstream control watch removed route ... reason=route_catalog_update`).
- Probe/call acquisition wins → authoritative lookup hits the tombstone → 410 (observed on windows-2025 CI: `[relay] Failed to acquire upstream 'grid' ... (route-removed): IPC route removed: grid uid=...` then `Removed unreachable route ... reason=route-removed`).

Both are correct production outcomes of two different withdrawal paths; only the fixtures' single-404 assumption was wrong. Windows 2025 named-pipe/handshake timing consistently let the acquisition win, exposing the stale assumption. This is a test-fixture defect, not a production defect: the 404/410 split, canonical error codes (701/708), withdrawal reasons, and eviction behavior are all deliberate and covered by the mapping unit tests in `router.rs`.

## Fix

All changes are confined to the test region of `core/transport/c2-http/src/relay/router.rs`. The fixtures now use the module's established `test_commit_registration!` direct-commit pattern (as in `call_route_missing_on_expected_owner_removes_local_route` and `relay_probe_does_not_trust_cached_data_plane_after_control_watch_unavailable`), which commits the relay-local route without the HTTP register handler and therefore without the background control watch. The data-plane acquisition is the only actor that can observe the stale route, so the outcome no longer depends on any scheduling:

- `probe_missing_upstream_route_removes_stale_local_route` (rewritten) — never-found: a live owner that serves only `other`; the relay holds a stale local `grid` route. Probe asserts 404 plus the strict canonical envelope (code 701, `ResourceNotFound`, "route not found: grid", `route` detail), route eviction, and resolve 404.
- `probe_known_removed_upstream_route_removes_stale_local_route` (new) — known-removed: a live owner that served `grid`, then `unregister_route("grid")` leaves the tombstone; the relay-local route is committed afterwards with the real uid/revision read from the live route before removal. Probe asserts 410 plus the strict canonical envelope (code 708, `ResourceRemoved`, "route removed: grid", `route_uid` equal to the tombstoned incarnation), route eviction, and resolve 404.
- `call_missing_upstream_route_removes_stale_local_route` → `call_known_removed_upstream_route_removes_stale_local_route` (renamed/rewritten) — same known-removed fixture through the call verb. Asserts 410 with the same canonical envelope; the envelope is also the no-unintended-dispatch proof, because a dispatched call would have returned 200 with the echo body.

No assertion was loosened: every test still pins the exact status, the full canonical error identity, route eviction, and (for resolve) post-eviction behavior. The never-found call contract remains covered by the existing deterministic `call_route_missing_on_expected_owner_removes_local_route`; the watch-driven withdrawal (the old fixtures' accidental pass path) remains covered by `relay_upstream_watch_removes_local_route_without_data_plane_call`. The complete contract matrix is now:

| Contract | Verb | Test |
| --- | --- | --- |
| never-found → 404 | probe | `probe_missing_upstream_route_removes_stale_local_route` (rewritten) |
| never-found → 404 | call | `call_route_missing_on_expected_owner_removes_local_route` (existing) |
| known-removed → 410 | probe | `probe_known_removed_upstream_route_removes_stale_local_route` (new) |
| known-removed → 410 | call | `call_known_removed_upstream_route_removes_stale_local_route` (renamed) |
| watch withdrawal without data plane | — | `relay_upstream_watch_removes_local_route_without_data_plane_call` (existing) |

## Validation

Local host is macOS (darwin 25.6.0 arm64); the sibling `fastdb` dependency was provided as the permitted read-only symlink, and `c2-http` itself does not link it. Commands run from `core/`:

- `cargo test -p c2-http --features relay --lib -- --exact <the three tests above> --nocapture` — 3/3 pass; logs show the intended deterministic paths: `(route-missing): IPC route not found: grid` → `reason=route-missing` for never-found, and `(route-removed): IPC route removed: grid uid=<tombstone uid>` → `reason=route-removed` for known-removed.
- The same three-test invocation repeated 15 times — 15/15 pass, no flakes.
- `cargo test -p c2-http --features relay --lib` — 295 passed, 0 failed (was 292 passed + 2 failed on windows-2025).
- `cargo test -p c2-http --features relay` (all targets) — no other targets exist beyond the lib.
- `cargo test -p c2-http --features relay --lib relay::` — 252 relay-module tests pass.
- `cargo clippy --manifest-path core/Cargo.toml -p c2-http --features relay --all-targets` — clean.
- `cargo fmt --manifest-path core/Cargo.toml -p c2-http -- --check` — the only remaining diff is a pre-existing line-2305 wrapping difference that also exists on the base commit under the local rustfmt 1.8.0; no new formatting drift was introduced.

Windows-specific note: the fixtures no longer contain any cross-task race — no control watch is spawned, the server tombstone (or permanent absence) of `grid` is fully established before the relay-local route is committed, and the server state is immutable for the rest of the test. The two outcomes are closed systems, so platform scheduling cannot select a different status; no Windows runner was available locally, so final confirmation lands with the Host's windows-2022/2025 matrices.

## Files changed

- `core/transport/c2-http/src/relay/router.rs` — test-only changes in the `tests` module around the two failing tests.
- `docs/reports/windows-route-removal-tests.md` — this report.
