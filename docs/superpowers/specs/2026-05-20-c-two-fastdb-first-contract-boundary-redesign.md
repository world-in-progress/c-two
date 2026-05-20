# C-Two FastDB-First Contract Boundary Redesign

**Date:** 2026-05-20
**Status:** Draft for review
**Scope:** Redesign the C-Two/FastDB boundary so FastDB is the first-class portable payload ABI for C-Two contracts, while C-Two owns the contract integration and FastDB remains the storage/serialization/runtime substrate.

> **0.x constraint:** C-Two is pre-1.0. Do not preserve compatibility shims for stale provider-neutral public APIs, optional integration install flows, or docs that describe Arrow/custom codecs as a peer portable path. Prefer clean cuts that make the FastDB-first contract boundary explicit.

## Problem

The current fastdb-first work proved a large amount of functionality, but the ownership model is muddled. C-Two documents and APIs still use provider-neutral or FastDB-neutral language in places where the intended product decision is stronger: FastDB is the portable payload ABI for C-Two CRM contracts.

The current split also puts too much C-Two-specific glue in the FastDB repository. Files such as `fastdb4py.c_two_provider`, `fastdb4py.c_two_call`, `fastdb4py.c_two_bridge`, `fastdb4py.codegen.c_two_ts`, `fastdb4ts/src/c-two.ts`, and `fdb codegen --c-two-ts` make FastDB look like it owns C-Two contract integration. That reverses the desired responsibility: C-Two owns CRM contracts, route identity, transport semantics, bridge application, contract export, and generated clients; FastDB owns schema, storage, serialization, views, and memory/runtime substrate.

The incorrect boundary came from overcorrecting for a real risk. C-Two runtime layers must not parse arbitrary FastDB storage internals or become a copy of FastDB's table/object-graph engine. But that does not imply C-Two should avoid depending on FastDB ABI. If C-Two is FastDB-first, C-Two should introduce FastDB as a direct ABI dependency in the contract layer, just as an RPC framework may directly depend on protobuf/gRPC while still keeping scheduling, routing, and lifecycle semantics separate from application message definitions.

## Goals

1. Make FastDB the only first-class portable CRM payload ABI for C-Two.
2. Move C-Two-specific FastDB contract integration into the C-Two repository.
3. Keep Python pickle fallback as Python-only local/prototype behavior, never as a portable export/codegen path.
4. Remove or demote provider-neutral product language from public C-Two docs and APIs.
5. Keep C-Two runtime core responsible for route, transport, relay, scheduler, lease, and lifecycle semantics.
6. Keep FastDB responsible for generic schema, storage, serialization, owned-buffer, view, and runtime behavior.
7. Prevent FastDB repository docs and public APIs from becoming C-Two-specific except for generic capabilities that C-Two happens to consume.

## Non-Goals

- Do not continue Implementation Order item 91 during this boundary redesign.
- Do not create an independent `fastdb-c-two` integration package. C-Two contract integration belongs in the C-Two repository because FastDB is C-Two's portable ABI, not an optional plugin.
- Do not keep Arrow/custom providers as peer portable ABI routes. They may remain examples, tests, or Python-only experiments only if they cannot be confused with the official portable path.
- Do not move FastDB storage engine, column layout, object-graph engine, or WASM internals into C-Two runtime core.
- Do not make FastDB own C-Two route validation, relay resolution, IPC framing, scheduler/backpressure, or lease authority.

## Core Decision

C-Two portable CRM contract payloads are FastDB call-db payloads or no-payload. Python pickle exists only as a Python SDK fallback for local/prototype flows and must be rejected by portable export, strict codegen, and cross-language contract validation.

C-Two may and should depend on FastDB ABI in its contract layer. The boundary is not "C-Two must be FastDB-neutral." The correct boundary is: C-Two runtime layers do not parse FastDB storage internals, while C-Two contract layers directly model FastDB as the portable ABI.

FastDB must not carry C-Two contract glue as a primary product surface. FastDB should expose generic schema, serialization, view, owned-buffer, and runtime APIs that C-Two consumes. C-Two should own the FastDB-backed CRM planner, bridge derivation, contract artifacts, codegen orchestration, TypeScript typed facade, and C-Two-specific tests.

## Ownership Model

### C-Two Owns

- CRM contract identity, route name, CRM tag, ABI hash, signature hash, and route validation.
- FastDB-backed portable contract planning for CRM method input and output payloads.
- FastDB call-db envelope binding as the C-Two portable method payload shape.
- Python SDK annotation interpretation for C-Two CRM contracts.
- Python resource bridge application across thread-local, direct IPC, and relay-backed HTTP execution.
- Contract export, diagnostics, artifacts, and strict codegen rules.
- Generated TypeScript C-Two client transport and FastDB-backed typed facade generation.
- Held-call semantics, raw payload retention, and C-Two `HeldResult` lifecycle.
- IPC, relay, route cache, scheduler/backpressure, shutdown, and native lease authority.
- Tests proving C-Two + FastDB ABI behavior across C-Two transports.

### FastDB Owns

- FastDB scalar aliases, feature schemas, call-db schemas, and schema hashing.
- Columnar and object-graph storage/runtime behavior.
- WSTR, BYTES, list, nullable, ref/list-ref, object-graph, and table layout semantics.
- Python and TypeScript serialization/deserialization primitives.
- FastDB view/runtime APIs, including retained views where FastDB can safely expose them.
- Owned bytes and WASM-owned payload ingress.
- Generic validation for FastDB descriptors and storage/runtime capabilities.
- Tests proving FastDB runtime semantics independent of C-Two route/transport behavior.

### Forbidden Boundaries

- C-Two runtime core must not import FastDB storage engine modules or inspect column/table/object-graph internals to make route, relay, IPC, scheduler, or lifecycle decisions.
- FastDB must not own C-Two route/CRM identity, relay cache behavior, direct IPC framing, C-Two generated client transport, or bridge application across C-Two transports.
- C-Two docs must not describe arbitrary codec providers as a peer portable strategy.
- FastDB README/docs must not present C-Two-specific glue as part of FastDB's core architecture.

## Proposed Architecture

### 1) Add a first-class C-Two FastDB ABI layer

Introduce a C-Two-owned FastDB ABI module, tentatively `sdk/python/src/c_two/fastdb/` or `sdk/python/src/c_two/abi/fastdb/`. This module is the public Python home for FastDB-first CRM authoring and the internal home for FastDB contract planning.

This module may directly depend on `fastdb4py`. It should expose or re-export the annotations and aliases C-Two users need for portable contracts, such as scalar aliases, `Feature`, `Array`, `Batch`, and FastDB-backed bridge helpers. It must not require users to call `install_c_two_provider(...)` or manually register a provider to make the official portable path work.

The top-level `c_two` package may expose the most common FastDB ABI names if that keeps CRM authoring ergonomic, but the source of truth should remain the C-Two FastDB ABI layer.

### 2) Replace provider-neutral public flow with FastDB ABI planning

The current provider registry can be retained as an internal migration mechanism only if it simplifies implementation. It should not be presented as a public product surface for portable C-Two. Names and diagnostics should move from "provider" language toward "FastDB ABI", "payload ABI", or "contract ABI".

Portable export should accept only FastDB call-db refs and no-payload refs. Non-FastDB `PayloadAbiRef` values should either be rejected in strict portable mode or classified as Python/example/internal experimental refs. The descriptor representation can still use `PayloadAbiRef` as an internal data shape, but `PayloadAbiRef` must not imply arbitrary portable codec pluggability.

### 3) Move C-Two-specific FastDB glue from FastDB into C-Two

Migrate C-Two-specific FastDB code from the FastDB repository into the C-Two repository. The likely migration targets are:

- `fastdb4py.c_two_provider` -> C-Two FastDB ABI planner/transferable binding.
- `fastdb4py.c_two_call` -> C-Two FastDB call-db envelope planner, backed by generic FastDB schema/runtime APIs.
- `fastdb4py.c_two_bridge` -> C-Two FastDB resource bridge derivation helpers.
- `fastdb4py.codegen.c_two_ts` -> C-Two TypeScript FastDB ABI helper/facade generation.
- `fastdb4ts/src/c-two.ts` -> generated or C-Two-owned TypeScript FastDB ABI runtime facade, using generic `fastdb4ts` primitives.
- `fdb codegen --c-two-ts` -> `c3 contract codegen typescript` or a C-Two-owned subcommand that emits the FastDB-backed typed facade.

FastDB may need to expose small generic APIs to support this migration. Those APIs should be named and documented as FastDB capabilities, not C-Two integration hooks.

### 4) Keep FastDB generic and reusable

After migration, FastDB should still provide the substrate C-Two needs:

- schema export and validation;
- call-db schema construction primitives;
- encode/decode primitives;
- retained view APIs;
- owned bytes / WASM ingress APIs;
- storage/runtime guardrails for WSTR, BYTES, list, nullable, and object-graph behavior.

FastDB tests should focus on these generic semantics. C-Two-specific end-to-end tests should move to C-Two.

### 5) Make C-Two codegen own the FastDB-backed client path

`c3 contract codegen typescript` should become the authoritative TypeScript generation path for C-Two contracts. For FastDB-backed contracts it should emit or coordinate:

- C-Two route fingerprints and method wire specs;
- C-Two HTTP/relay/direct-IPC transports;
- FastDB ABI codec bindings and typed method facades;
- held-call wrappers that expose FastDB views only when the FastDB ABI declares the result viewable;
- payload allocator integration for FastDB-owned bytes where the runtime supports it.

FastDB may still provide low-level TypeScript runtime primitives, but users should not need to run a separate FastDB C-Two codegen command as the main path.

### 6) Document the Python-only fallback explicitly

Python-native annotations and pickle fallback remain useful for local experiments, tests, and Python-only resource prototyping. They must not be framed as portable. Diagnostics should say that the method is Python-only and explain the FastDB annotation needed to become portable.

This lets the Python SDK stay ergonomic without weakening the cross-language contract rule.

## Repository Migration Plan

### Phase 0: Boundary inventory and freeze

- Inventory C-Two docs/code that uses provider-neutral, FastDB-neutral, Arrow/custom portable, or optional install language.
- Inventory FastDB docs/code/tests that are C-Two-specific.
- Mark current fastdb-first implementation as a source branch for migration, not a merge-ready endpoint.
- Do not add item 91 features during this phase.

### Phase 1: C-Two FastDB ABI module skeleton

- Add the C-Two-owned FastDB ABI module.
- Move or re-home Python FastDB annotations, call-db planning, and portable diagnostics behind this module.
- Make official FastDB ABI behavior available without manual provider installation.
- Keep old provider entrypoints only as temporary internal compatibility if needed for incremental migration, with tests proving the official path does not depend on them.

### Phase 2: Bridge ownership migration

- Move C-Two-specific bridge derivation and bridge application tests into C-Two.
- Keep generic FastDB conversion primitives in FastDB only if they are useful without C-Two.
- Ensure thread-local, direct IPC, explicit relay, and held-call bridge behavior remains tested in C-Two.

### Phase 3: TypeScript codegen ownership migration

- Move C-Two-specific TypeScript FastDB facade/codegen into C-Two.
- Make `c3 contract codegen typescript` the primary generated-client entrypoint for FastDB-backed contracts.
- Keep FastDB TypeScript runtime primitives generic and dependency-free from C-Two route/transport semantics.

### Phase 4: FastDB repository cleanup

- Remove or rename C-Two-specific FastDB modules after C-Two has adopted their functionality.
- Remove C-Two-specific README narrative from FastDB main docs or reduce it to a short "consumed by C-Two" note.
- Keep FastDB tests for generic schema/runtime/storage behavior.
- Move C-Two end-to-end and transport tests to C-Two.

### Phase 5: C-Two docs and API cleanup

- Rewrite C-Two README, roadmap, provider docs, and plan docs around the FastDB-first contract model.
- Demote Arrow/custom/pickle to examples, tests, internal experiments, or Python-only fallback language.
- Rename public APIs where "provider" incorrectly implies optional portable codec ecology.
- Add migration notes for any removed provider-neutral public APIs.

## Acceptance Criteria

- C-Two public docs state that FastDB call-db is the portable CRM payload ABI.
- Portable export and strict TypeScript codegen reject pickle and non-FastDB `PayloadAbiRef` values, except no-payload refs.
- C-Two can generate and run a FastDB-backed TypeScript client path without requiring `fdb codegen --c-two-ts` as a separate primary workflow.
- C-Two Python users can author FastDB-backed CRM contracts without calling a provider installation function.
- C-Two runtime layers still do not parse FastDB storage internals for route, relay, IPC, scheduler, lease, or shutdown decisions.
- FastDB repository no longer exposes C-Two-specific modules as core package surfaces.
- FastDB generic runtime tests and C-Two end-to-end contract/transport tests are separated by responsibility.
- Existing verified behavior for scalar, `Array`, `Batch`, WSTR, BYTES, held views, bridge derivation, direct IPC, relay, and TypeScript allocator-backed paths remains covered after migration.

## Risks and Mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Moving too much FastDB logic into C-Two runtime core | High | Keep FastDB storage/runtime operations behind public FastDB APIs and prohibit FastDB parsing in route/relay/IPC/scheduler/lease code. |
| Leaving provider-neutral API names in place | High | Treat naming and docs as part of the migration acceptance criteria, not cleanup afterthoughts. |
| Splitting code before generic FastDB APIs exist | Medium | First identify which FastDB APIs C-Two needs, then extract generic FastDB surfaces before moving C-Two glue. |
| Breaking current verified behavior during migration | High | Preserve the existing focused test matrix and move tests with their owning responsibility. |
| Creating a hidden optional integration package by another name | Medium | Keep C-Two FastDB integration inside the C-Two repository and make it part of official C-Two contract/codegen behavior. |
| Overloading top-level `c_two` namespace | Medium | Use a dedicated C-Two FastDB ABI module as source of truth and re-export only ergonomic stable names. |

## Open Questions

1. Should the C-Two FastDB ABI module path be `c_two.fastdb` or `c_two.abi.fastdb`?
2. Should common FastDB CRM aliases be re-exported from top-level `c_two`, or should users import them from the FastDB ABI module explicitly?
3. Which FastDB APIs must be extracted or stabilized before C-Two can own call-db planning and TypeScript facade generation?
4. How aggressively should old provider-neutral APIs be removed in the first migration pass versus kept as private transition helpers?
5. Should the existing Arrow provider remain in-tree as an example/test fixture, or move out of the C-Two main package entirely?

## Initial Recommendation

Use `c_two.fastdb` as the public C-Two FastDB ABI module. It is shorter, matches the product decision, and avoids making FastDB look like one ABI among many public peers. Keep implementation internals under that module separated by concern, for example `annotations.py`, `call_db.py`, `bridge.py`, `descriptor.py`, and `typescript.py`.

Re-export a minimal stable subset from top-level `c_two` only if it materially improves CRM readability. Prefer explicit `from c_two import fastdb as fdb` or `from c_two.fastdb import I32, Feature, Batch` in examples so the FastDB-first dependency is visible.

Move C-Two glue from FastDB to C-Two before preparing PRs. The current large fastdb-first diff should be treated as raw material for this redesign, not as the final integration shape.

## Verification Plan

- Run C-Two Python focused FastDB ABI tests after each Python migration slice.
- Run C-Two grid FastDB smoke across thread-local, direct IPC, explicit relay, and held calls after bridge migration.
- Run C-Two Rust workspace tests after contract/codegen/runtime changes.
- Run C-Two generated TypeScript tests and c2-mem-ffi tests after TypeScript migration.
- Run FastDB Python and TypeScript suites after removing C-Two-specific modules to prove generic runtime behavior remains intact.
- Run production import scans in both directions: C-Two runtime layers must not import FastDB storage internals, and FastDB core modules must not import C-Two.
- Run `git diff --check` in both repositories after every phase.
