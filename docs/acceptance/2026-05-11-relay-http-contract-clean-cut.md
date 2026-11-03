# 2026-05-11 Relay HTTP Contract Clean Cut Review

## Context

The closed-loop review found that the route fingerprint work removed the normal
Python and PyO3 route-key call bypasses, but the relay HTTP data-plane still
kept compatibility-shaped entry points. This document records the remaining
issues before the clean cut, and defines the concrete code, test, and guardrail
changes required to close them.

## Frozen Invariants

1. **Named CRM data-plane invariant**
   Every HTTP CRM call or probe for a named route must be route-bound at client
   construction or must carry one complete `ExpectedRouteContract` on the same
   request. A route name alone is not authority to call or probe a route.

2. **No optional contract fallback invariant**
   Data-plane code must not represent expected route identity as
   `Option<ExpectedRouteContract>`. Optional expected identity is only allowed
   for explicitly non-dispatch catalog/listing surfaces, never for CRM
   dispatch, runtime resolve, or route health probes used by dispatch clients.

3. **No independent route-key plus contract invariant**
   A helper that already has an `ExpectedRouteContract` must derive the route
   name from `expected.route_name`. It must not accept a second independent
   `route_name` argument that can drift from the validated contract.

4. **Guardrail closure invariant**
   Tests and static guards must fail on all old shapes:
   - missing expected headers on relay `POST /{route}/{method}`;
   - missing expected headers on relay `GET /_probe/{route}`;
   - missing expected query fields on relay `GET /_resolve/{route}`;
   - `RelayControlClient::resolve(name)` / `resolve_async(name)`;
   - Python `RustRelayControlClient.resolve(name)`;
   - crate-internal HTTP helpers taking `expected: Option<_>`;
   - crate-internal HTTP helpers taking both `route_name` and an expected
     contract.
   - relay route-table production code retaining a private
     `resolve_filtered(name, None)` or `Option<&ExpectedRouteContract>` branch.

## Findings

Status as of the 2026-05-11 clean-cut pass: the findings below describe the
pre-fix compatibility shapes that were removed. The closure evidence is recorded
after the matrix so this review remains useful as both the defect record and the
regression checklist.

### Critical: relay data-plane still accepts name-only HTTP dispatch

Evidence:

- `core/transport/c2-http/src/relay/router.rs::expected_crm_from_headers(...)`
  returns `Ok(None)` when all five expected-contract headers are absent.
- `validate_expected_crm_for_route(...)` and
  `validate_expected_crm_for_acquired_route(...)` treat `None` as success.
- `handle_probe(...)` and `call_handler(...)` both use that optional expected
  value before acquiring or dispatching the upstream IPC client.

Impact:

Any hand-written HTTP caller, old raw client, or future internal caller can use
`/_probe/{route}` or `/{route}/{method}` without CRM namespace/name/version,
`abi_hash`, or `signature_hash`. That preserves the route-name-only behavior
that the contract-fingerprint remediation was supposed to remove.

Required clean cut:

- Data-plane and probe handlers must require all expected-contract headers.
- A completely missing expected header set must return `400 InvalidCrmTag`
  before route lookup, upstream acquire, or IPC dispatch.
- Partial expected headers must continue to return `400`.
- Mismatched complete expected headers must continue to return `409`.

### High: raw HTTP helper still accepts independent route name and optional expected contract

Evidence:

- `core/transport/c2-http/src/client/client.rs::call_with_expected_crm_async(...)`
  accepts `route_name: &str` and `expected: Option<&ExpectedRouteContract>`.
- `probe_route_with_expected_crm_async(...)` has the same shape.

Impact:

The public raw `HttpClient::call(route_name, ...)` API was removed, but the
crate-internal helper still allows future code to reintroduce `None` expected
identity or pass a route name that differs from the expected contract. This is a
half-closed API: current callers behave, but the choke point does not enforce
the invariant.

Required clean cut:

- Change both helpers to accept `expected: &ExpectedRouteContract`.
- Remove the `route_name` parameter from both helpers.
- Derive the URL route segment from `expected.route_name`.
- Update relay-aware callers and source guards accordingly.

### Medium: name-only runtime resolve remained and would become a discovery bypass

2026-05-11 follow-up: this finding was remediated by removing the runtime
name-only resolve surfaces and requiring complete expected-contract query fields
for callable relay resolution.

Evidence:

- `RelayControlClient::resolve(name)` and `resolve_async(name)` called
  `resolve_with_query_async(name, None)`.
- Relay `/_resolve/{route}` allowed missing query parameters and returned
  `state.resolve(&name)`.
- The Python FFI exposed `RustRelayControlClient.resolve(name)`, keeping the
  same name-only surface available to SDK tests and future callers.

Impact:

This is not a direct dispatch call, but it was the runtime route-selection
surface used before dispatch. Keeping name-only resolve there leaves a
half-closed model: the call path requires CRM fingerprints, but route discovery
could be performed by name and later stitched back into dispatch code.

Required clean cut:

- Remove the Rust `RelayControlClient::resolve(name)` and
  `resolve_async(name)` APIs.
- Remove the Python `RustRelayControlClient.resolve(name)` FFI method and its
  route projection helper.
- Require `/_resolve/{route}` to carry all five expected-contract query fields:
  `crm_ns`, `crm_name`, `crm_ver`, `abi_hash`, and `signature_hash`.
- Treat a complete expected-contract mismatch as normal non-resolution for the
  runtime endpoint: return `404`, not a name-only `409` diagnostic.
- If name-based mesh lookup is needed later, add a separate catalog/discovery
  endpoint, for example `/_discover/{name}` or `/_routes?name=...`, that returns
  route plus CRM tag/hash metadata and is explicitly not consumed by dispatch
  clients.

## Source-To-Sink Matrix

| Invariant | Producer | Consumer / Storage | Network / Wire | FFI / SDK | Test / Support | Legacy / Bypass Closure |
| --- | --- | --- | --- | --- | --- | --- |
| Named CRM data-plane | `cc.connect(...)` and `RelayAwareHttpClient` produce `ExpectedRouteContract` | Relay handler validates local route and acquired route against required expected contract | `x-c2-expected-*` headers required on `/_probe` and `/{route}/{method}` | PyO3 route-bound clients expose `call(method, data)` only | Router unit tests cover missing, partial, mismatched, and valid headers | No no-header call/probe dispatch |
| No optional contract fallback | HTTP client helpers take `&ExpectedRouteContract` | Helper cannot be called with `None` | Request headers always include all expected fields | SDK caller cannot pass route separately | Source guard rejects `expected: Option<&ExpectedRouteContract>` in data-plane helpers | No `route_name + None` helper |
| No independent route-key plus contract | Route name is `expected.route_name` | Cache/probe/call state cannot drift from expected route | URL route segment derived from expected contract | Python does not expose route-key call | Source guard rejects helper signatures with both `route_name` and expected | No dual-key half-fix |
| Runtime resolve is contract-bound | `RelayAwareHttpClient` produces one `ExpectedRouteContract` | `RelayControlClient` cache key stores the complete expected contract; `RouteTable::resolve_matching(...)` filters directly by the required contract | `/_resolve/{route}` query includes CRM tag plus both hashes | Python FFI exposes no name-only resolve method | Router tests cover missing query and wrong-contract 404; source guards reject public name-only APIs and private optional filters | No `resolve(name)`, no `ResolveCacheKey::Name`, no `state.resolve(&name)` fallback in `handle_resolve`, no production `resolve_filtered(name, None)` |

## Clean-Cut Closure Evidence

Code changes made in this pass:

- `core/transport/c2-http/src/client/client.rs`
  - `call_with_expected_crm_async(...)` now accepts
    `expected: &ExpectedRouteContract` and derives the URL route segment from
    `expected.route_name`.
  - `probe_route_with_expected_crm_async(...)` now accepts
    `expected: &ExpectedRouteContract`.
  - `add_expected_contract_headers(...)` no longer accepts `Option`; it always
    writes CRM tag plus `abi_hash` and `signature_hash`.
- `core/transport/c2-http/src/client/relay_aware.rs`
  - Relay-aware probe and call paths pass `&self.expected` directly into the
    route-bound helpers.
- `core/transport/c2-http/src/relay/router.rs`
  - `expected_crm_from_headers(...)` now returns a required
    `ExpectedRouteContract`.
  - Missing expected headers return `400 InvalidCrmTag` before route lookup or
    upstream acquire.
  - `validate_expected_crm_for_route(...)` and
    `validate_expected_crm_for_acquired_route(...)` no longer accept
    `Option<ExpectedRouteContract>`.
  - `handle_resolve(...)` now requires the complete expected-contract query and
    uses only `resolve_matching(...)`; wrong-contract requests return `404`
    rather than a name-only mismatch diagnostic.
  - Test support calls/probes now include complete expected-contract headers
    unless the test is explicitly asserting that missing headers are rejected.
- `core/transport/c2-http/src/client/control.rs`
  - Removed public `resolve(name)` / `resolve_async(name)`.
  - `ResolveCacheKey` stores the complete `ExpectedRouteContract`, including
    both hashes, and `resolve_matching_async(...)` is the only runtime resolve
    API.
- `sdk/python/native/src/http_ffi.rs`
  - Removed Python-visible `RustRelayControlClient.resolve(name)` and the
    now-unused route-list projection helper.
- `sdk/python/tests/integration/test_relay_mesh.py`
  - Raw relay-mesh resolve calls now include the full CRM tag and hash query.
- `core/transport/c2-http/src/relay/{route_table,state}.rs`
  - Name-only `resolve(name)` helpers are now `#[cfg(test)]` only.
  - `RouteTable::resolve_matching(...)` no longer calls a production
    `resolve_filtered(...)` helper with `Option<&ExpectedRouteContract>`.
  - `name_only_resolve_helpers_are_test_only` now rejects production
    `resolve(name)`, `resolve_filtered(...)`, `resolve_filtered(name, None)`, and
    `expected_crm: Option<&ExpectedRouteContract>` shapes before the test module.

Fresh verification through the final route-table guard pass:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay relay_data_plane_requires_expected_contract_headers -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay relay_probe_requires_expected_contract_headers -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay http_client_expected_contract_helpers_do_not_accept_optional_or_independent_route -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay data_plane_rejects_partial_expected_hash_headers_before_acquire -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay expected_crm_headers_are_sent_on_probe_and_call -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay call_unreachable_upstream_removes_stale_local_route -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay concurrent_calls_after_idle_eviction_do_not_report_unreachable -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay resolve_requires_expected_contract_query -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay resolve_with_expected_contract_hides_name_match -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay name_only_resolve_helpers_are_test_only -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http relay_control_client_does_not_expose_name_only_resolve -- --nocapture
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_sdk_boundary.py::test_relay_control_client_does_not_expose_name_only_resolve_to_python -q --timeout=30 -rs
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay -- --nocapture
cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo test --manifest-path core/Cargo.toml --workspace
git diff --check
```

Observed result for the full `c2-http` relay-feature run: `233 passed; 0
failed`. The final route-table pass verified the strengthened guard with a RED
failure on `fn resolve_filtered(` before the code fix, then a GREEN pass after
the production helper was removed. The client-only `c2-http` check, Rust
formatting check, whitespace diff check, and full Rust workspace test suite also
passed. The earlier Python SDK full-suite clean-cut verification passed with
`808 passed; 2 skipped`; it was not rerun in the final route-table-only pass
because no Python or PyO3 code changed in that pass.

## Required Verification

Run these before claiming closure:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay relay_data_plane_requires_expected_contract_headers -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay relay_probe_requires_expected_contract_headers -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay http_client_expected_contract_helpers_do_not_accept_optional_or_independent_route -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay resolve_requires_expected_contract_query -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay resolve_with_expected_contract_hides_name_match -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay name_only_resolve_helpers_are_test_only -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http relay_control_client_does_not_expose_name_only_resolve -- --nocapture
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_sdk_boundary.py::test_relay_control_client_does_not_expose_name_only_resolve_to_python -q --timeout=30 -rs
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay -- --nocapture
cargo test --manifest-path core/Cargo.toml --workspace
```

If code changes touch Python/PyO3 surfaces, run the full Python SDK suite or
record why only a smaller targeted subset is sufficient.
