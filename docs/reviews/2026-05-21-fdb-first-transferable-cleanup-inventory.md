# FDB-First Transferable Cleanup Inventory

> Date: 2026-05-21
> Scope: Review the current Python CRM transfer path before introducing a neutral authoring schema and schema-to-CRM code generation.
> Status: Evidence inventory and cleanup recommendation only. This file does not change runtime behavior.

## Position

C-Two can keep Python pickle fallback as a Python-only prototype/runtime path, but the portable authoring schema must not depend on `@cc.transferable`, custom Python byte hooks, arbitrary non-FastDB `PayloadAbiRef` values, or the current implicit `auto_transfer` fallback search. The portable path should be FastDB call-db payload ABI or no payload, with schema export warning or failing on Python-only fallback depending on mode.

The current implementation already has the right strict-export choke point: portable descriptor export rejects `python-pickle-default` and non-FastDB payload refs. The remaining problem is that the runtime planning path still routes both FastDB ABI and legacy Python transferables through the same `Transferable` abstraction, so later authoring schema work would be forced to reason about legacy concepts that are not part of the FDB-first contract model.

## Inventory Counts

Counts below are line hits collected with narrow `rg` patterns from the current worktree. They are not unique symbol counts. Documentation counts exclude `docs/superpowers/**` and this inventory file itself so the report does not count its own text; Python test counts are listed separately from Rust CLI tests.

| Area | `@cc.transferable` / `@transferable` refs | `@cc.transfer(...)` refs | `cc.hold(...)` refs | `python-pickle-default` refs |
| --- | ---: | ---: | ---: | ---: |
| `sdk/python/src` | 4 | 0 | 5 | 5 |
| `sdk/python/tests` | 68 | 54 | 69 | 35 |
| `cli/tests` | 0 | 0 | 0 | 3 |
| `examples` | 0 | 0 | 2 | 0 |
| `sdk/python/benchmarks` | 7 | 1 | 4 | 0 |
| `README.md` | 6 | 3 | 6 | 1 |
| `docs` | 60 | 19 | 62 | 12 |

The count patterns were `@cc\.transferable\b|@transferable\b|cc\.transferable\(`, `@cc\.transfer\b|@transfer\b|cc\.transfer\(`, `cc\.hold\(`, and `python-pickle-default`. Source-tree hits for `@cc.transferable` are public text or helper definitions rather than active production decorators; active decorated transferable classes are currently in tests and benchmarks, while FastDB production planning generates a private adapter class internally.

Source files with current transfer surface references are concentrated in `sdk/python/src/c_two/crm/transferable.py`, `sdk/python/src/c_two/crm/descriptor.py`, `sdk/python/src/c_two/crm/meta.py`, `sdk/python/src/c_two/crm/_payload_abi.py`, `sdk/python/src/c_two/fastdb/call_db.py`, `sdk/python/src/c_two/fastdb/typescript.py`, `sdk/python/src/c_two/transport/client/proxy.py`, and the top-level package exports in `sdk/python/src/c_two/__init__.py`.

## Current Source Evidence

`sdk/python/src/c_two/__init__.py` now exports `hold` and `HeldResult`, but no longer exports `transferable` or `transfer`. This leaves retained-response lifecycle as the public SDK surface while removing the old custom-codec authoring tools from the top-level namespace.

`sdk/python/src/c_two/crm/meta.py:149` through `sdk/python/src/c_two/crm/meta.py:166` decorate every public CRM method through `auto_transfer(...)`, passing CRM namespace/name/version/method context into the planner. This is the correct place to bind contract context, but today it always enters the same transfer wrapper pipeline whether the method resolves to FastDB call-db, a custom registered transferable, or pickle fallback.

`sdk/python/src/c_two/crm/transferable.py` now contains retained result lifetime (`HeldResult` and `hold`), a narrow internal `transfer(buffer=...)` metadata helper, and `auto_transfer(...)` wrapper construction over payload bindings. The legacy `Transferable` metaclass, global registry, and dynamic `Default*Transferable` factory have been removed from the source path.

`auto_transfer(...)` now resolves input/output in this order: try FastDB method payload ABI planning, else use an internal `PYTHON_PICKLE` binding for non-empty Python payloads, else use `NO_PAYLOAD`. There is no registered `@transferable` direct match branch and no dynamic `Default*Transferable` class creation branch.

`sdk/python/src/c_two/fastdb/call_db.py` now resolves method payload shapes directly into `PayloadBinding(kind=FDB)`. The binding carries the FastDB `PayloadAbiRef`, payload ABI artifacts, encode/decode hooks, and optional retained output view hook; it no longer generates a private `_payload_abi_ref_transferable(...)` class.

`sdk/python/src/c_two/crm/descriptor.py` already enforces the desired portable boundary. `_wire_ref_for_input(...)` and `_wire_ref_for_output(...)` return `python-pickle-default` when no nonempty payload binding exists (`sdk/python/src/c_two/crm/descriptor.py:619` through `sdk/python/src/c_two/crm/descriptor.py:637`). `_ensure_portable_wire_ref(...)` rejects `python-pickle-default`, rejects refs without `PayloadAbiRef` wire identity, and rejects payload ABI IDs outside `org.fastdb.call-db` (`sdk/python/src/c_two/crm/descriptor.py:734` through `sdk/python/src/c_two/crm/descriptor.py:756`). `contract_descriptor_diagnostics(...)` emits `python_only_pickle` and `non_fastdb_portable_payload_abi` warnings before strict workflows fail (`sdk/python/src/c_two/crm/descriptor.py:759` through `sdk/python/src/c_two/crm/descriptor.py:797`).

`README.md:229` through `README.md:286` has contradictory public narrative. The older custom serialization section presents `@cc.transferable`, `@cc.transfer`, automatic matching, and `from_buffer` as ordinary public authoring tools, while the later contract/codegen section states that FastDB call-db is the portable CRM payload ABI and that pickle/non-FastDB refs are rejected by strict export/codegen.

There is no current `sdk/python/src/c_two/providers` package or public provider module in the source tree, which is good. The remaining provider-neutral residue is mostly terminology and generic `codec`/`PayloadAbiRef` language, not a live `c_two.providers` module.

## Findings

### 1. `Transferable` is doing two incompatible jobs

Severity: High.

For legacy Python it means "a user-authored Python class with custom `serialize` / `deserialize` / optional `from_buffer` hooks." For FDB-first it now also means "an internal adapter class that lets a FastDB call-db ABI plan pass through the old wrapper." Those two meanings should not survive into authoring schema design. The schema should describe FastDB payload ABI refs and method shapes, not generated Python classes pretending to be user transferables.

Implemented direction: `sdk/python/src/c_two/crm/payload_plan.py` defines `PayloadPlanKind` with `NO_PAYLOAD`, `FDB`, and `PYTHON_PICKLE`, plus a non-`Transferable` `PayloadBinding`. The FastDB call-db planner returns that binding directly, and old `Transferable` mechanics are not represented as a plan kind.

### 2. `auto_transfer` mixes portable planning with Python fallback routing

Severity: High.

`auto_transfer` is currently both the CRM method wrapper factory and the payload planner. This makes the official FDB-first path share control flow with global registered transferables and dynamic pickle fallback. It is acceptable for existing runtime compatibility, but it is a poor foundation for neutral schema export/codegen because the export layer would have to reverse-engineer which branch happened.

Implemented direction: method planning now produces only `FDB`, `PYTHON_PICKLE`, or `NO_PAYLOAD`. Portable export and authoring schema should consume only `FDB` and `NO_PAYLOAD`; runtime can still dispatch Python fallback through the explicit Python-only pickle binding. Old `Transferable` classes do not enter the payload plan as `legacy_transferable` or `UNSUPPORTED_TRANSFERABLE`.

### 3. `@cc.transfer` conflates codec override with resource-side buffer policy

Severity: High.

The current decorator accepts arbitrary `input` and `output` transferables and an input `buffer` mode. In FDB-first portable CRM, method wire identity should come from FastDB annotations and call-db planning, not from arbitrary method-level Python codec overrides. At the same time, buffer/retention policy remains a real concern for zero-copy behavior.

Recommended direction: stop treating `@cc.transfer(input=..., output=...)` as a portable authoring tool and remove its codec override role in the 0.x cleanup. If input buffer policy is still needed for Python fallback, isolate it from wire codec selection. For FDB-first, retained-view capability should be derived from the FastDB payload ABI/capabilities and call site `cc.hold(...)`, not from arbitrary method-level codec classes.

### 4. `from_buffer` has overloaded input and output semantics

Severity: High.

The same hook name previously covered resource-side input construction and client-side held output view construction. Server-side borrowed input is now controlled by `cc.register(..., input_lifetime=...)`, while client-side held output is controlled by `cc.hold(...)`; no method payload binding should carry an auto-input-hold escape hatch.

Recommended direction: split capabilities by direction. For portable payloads, represent retained-output viewability as a payload ABI capability such as `buffer-view` on output refs. Resource-side input handling should be explicit about whether it materializes, borrows transiently, or returns a retained resource-facing view. Do not let a generic Python method named `from_buffer` drive portable schema semantics.

### 5. `cc.hold` should survive, but not as a `Transferable.from_buffer` feature

Severity: Medium.

`cc.hold(...)` is still an important public API for high-performance FastDB output views. FastDB grid and FastDB CRM smoke tests exercise retained call-db views through this API. The incompatible part is not `cc.hold` itself; it is the implementation rule that a held output asks a transferable for `from_buffer(...)` if present.

Recommended direction: keep `cc.hold` and `HeldResult` as C-Two retained-response lifecycle APIs. Rewire held decode through the method payload plan: FastDB call-db outputs with retained-view capability produce FastDB views, decode-only payloads produce materialized values with no retained FastDB-owned view, and Python fallback remains explicitly nonportable.

### 6. The global transferable registry is legacy hidden state

Severity: Medium.

`TransferableMeta` registers decorated classes in `_TRANSFERABLE_MAP`, and `auto_transfer` can silently match by module/name for single-parameter and return-type signatures. This implicit state is difficult to express in a neutral schema and makes codegen behavior depend on Python import side effects.

Implemented direction: global registry matching has been removed from `auto_transfer` entirely. Generated schema and runtime planning no longer depend on "which Python modules were imported before `@cc.crm` ran."

### 7. Descriptor diagnostics are mostly aligned but should become the schema gate

Severity: Medium.

The existing descriptor path already rejects pickle and non-FastDB refs in portable mode and provides warnings through `contract_descriptor_diagnostics(...)`. Authoring schema export should reuse this style but change the non-strict behavior from "export everything with pickle descriptors" to "warn and skip nonportable methods" only if the requested schema mode allows partial export. Strict mode should fail.

Recommended direction: define two authoring-schema export modes up front: `strict` fails on any Python-only method; `diagnostic/partial` emits warnings and omits nonportable methods. Do not put pickle or legacy transferable descriptors into a neutral authoring schema as if they were first-class IDL.

### 8. Tests currently preserve legacy behavior by design

Severity: Medium.

The test suite has extensive coverage of `create_default_transferable`, `@cc.transferable`, registry matching, `@cc.transfer`, input hold, `from_buffer`, and `cc.hold`. This is useful evidence while refactoring, but the product direction is now a clean 0.x removal of old Transferable semantics rather than a compatibility window.

Recommended direction: split tests into `portable_fastdb_contract` and `python_fallback_runtime` buckets. Portable tests should never assert `@cc.transferable` semantics. Python fallback tests should assert that pickle works and is diagnosed as nonportable. Tests whose only purpose is preserving old custom hook behavior should be deleted or rewritten around FDB/pickle behavior.

## Mechanism Classification

| Mechanism | Current role | FDB-first assessment | Suggested action before authoring schema |
| --- | --- | --- | --- |
| `PayloadAbiRef` | Opaque wire identity for payload ABI | Keep as internal descriptor shape, but do not imply arbitrary portable codec ecosystem | Keep, document as internal/general ref with FastDB-only portable whitelist |
| FastDB call-db planner | Plans CRM method payloads from FastDB annotations | Official portable path | Keep and move behind a non-Transferable payload binding |
| `_payload_abi_ref_transferable` | Former bridge from payload ABI refs into old transfer wrapper | Removed from source path | No action remains except deleting stale tests/docs that mention it |
| `Transferable` / `@cc.transferable` | Former user custom Python byte hook path | Removed from public/new planning path | Delete or rewrite stale tests/docs that still preserve old behavior |
| `create_default_transferable` | Former dynamic pickle fallback class factory | Removed from source path | Use ordinary pickle encode/decode helpers behind `PYTHON_PICKLE`; never export to neutral schema |
| `_TRANSFERABLE_MAP` / `get_transferable` | Former import-order-dependent implicit matching | Removed from source path | Delete or rewrite stale tests/docs that still preserve registry behavior |
| `@cc.transfer(input/output=...)` | Former method-level custom wire override | Codec override removed; narrow internal buffer metadata may remain temporarily | Delete or rewrite stale tests/docs that still use codec overrides |
| `from_buffer` on transferables | Input and output buffer hook | Directionally ambiguous | Replace portable meaning with explicit payload-plan capabilities |
| `cc.hold` / `HeldResult` | Retained response lifetime API | Still valuable for FastDB views | Keep, rewire through payload ABI capabilities |
| `contract_descriptor_diagnostics` | Warns on pickle and non-FastDB refs | Aligned with FDB-first | Reuse as authoring-schema export gate |

## Recommended Cleanup Order

1. Freeze the public contract rule in docs and tests: portable authoring schema accepts only FastDB call-db payload refs or no payload. Python pickle fallback is warning-and-skip in partial schema export and error in strict schema export.
2. Split `auto_transfer` internally into "method payload planning" and "runtime wrapper construction." The planning result should explicitly classify only `FDB`, `PYTHON_PICKLE`, and `NO_PAYLOAD`.
3. Introduce a non-`Transferable` internal binding for FastDB call-db with `PayloadAbiRef`, artifacts, encode/decode, and retained-output-view capability. Remove `_payload_abi_ref_transferable` once the wrapper can consume the new binding directly.
4. Reclassify tests. Move or rename tests so portable FastDB contract tests do not depend on `@cc.transferable`; Python fallback tests prove pickle runtime behavior plus diagnostics; old custom-hook preservation tests are removed or rewritten.
5. Remove `@cc.transferable` and `@cc.transfer(input/output=...)` from public docs and top-level exports as part of the 0.x cleanup. The docs should stop presenting them as the normal way to author cross-language CRM payloads.
6. Rework `cc.hold` implementation to use payload-plan retained-view capability instead of `Transferable.from_buffer` presence. Preserve the public API and lifecycle semantics.
7. Only after the above, design `c-two.authoring.schema.v1`. At that point schema export/codegen can ignore legacy transferable mechanics rather than carrying compatibility vocabulary into the new IDL.

## Authoring Schema Implications

The neutral authoring schema should not contain Python class module paths, `@cc.transferable` class names, custom `serialize`/`deserialize` hook names, pickle protocol descriptors, or global registry matches. It should contain CRM identity, method shape, access mode, FastDB payload ABI refs/artifacts, payload capabilities, and nonsemantic documentation/target hints.

For `CRM -> schema` export, a Python CRM with pickle fallback should produce diagnostics. In strict mode export should fail. In partial mode export may skip those methods and record warnings, but it should not translate them into fake neutral schema entries. Old custom transferables should be gone before neutral schema work begins; if encountered during migration, they should be reported as removed/unsupported API rather than modeled inside the schema.

For `schema -> CRM` codegen, generated Python CRM should use `fastdb4py` ABI annotations and `@cc.crm`. It should not generate `@cc.transferable` classes for portable payloads.

## Open Questions

1. Should `@cc.transfer(buffer=...)` be replaced with a narrower input-retention decorator for Python fallback, or should all buffer behavior become an internal payload-plan decision?
2. Should authoring schema partial export skip an entire method when either input or output is nonportable, or preserve a method stub with explicit nonportable diagnostics outside the schema artifact?
3. Should non-FastDB `PayloadAbiRef` support remain only as a negative diagnostic fixture, or as an internal experimental hook behind a private module?

## Verification Performed

- Read `sdk/python/src/c_two/crm/transferable.py`, `meta.py`, `descriptor.py`, `_payload_abi.py`, `infer.py`, `template.py`, `transport/client/proxy.py`, `transport/server/native.py`, and `fastdb/call_db.py`.
- Read relevant FastDB-first boundary docs and README sections.
- Ran narrow `rg` inventory searches for `@cc.transferable` / `@transferable`, `@cc.transfer`, `cc.hold`, `python-pickle-default`, `auto_transfer`, `from_buffer`, `PayloadAbiRef`, provider/codec residue, and related tests.
- Checked git status after the audit; the only reported change is this untracked Markdown review file.
