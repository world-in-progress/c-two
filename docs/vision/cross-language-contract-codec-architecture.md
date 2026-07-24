# Cross-Language Contract and Payload Architecture

> **Status:** Current boundary summary. The authoritative executable design is [`2026-07-24 portable-payload contract composition`](../superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md).

C-Two owns `c-two.contract.v2`, CRM method/binding relationships, route-independent release identity, route/relay/transport/lifecycle semantics, generated CRM adapters, and final artifact composition. A portable method explicitly binds zero or one input and zero or one output through `@cc.transfer(...)`; each binding embeds one opaque nested `fastdb.payload.v1` JSON value.

FastDB C++ Core is the sole authority for nested parsing, canonical identity, digest, type/profile meaning, binary layout, build/open/view/materialize/invalidate behavior, structured payload errors, and payload-only C++/Rust/Python/TypeScript codegen. C-Two delegates nested values through the official FastDB projection and never implements those semantics itself.

Python pickle remains a Python-only prototype facility and portable export/codegen rejects it. The proven Rust and Python portable receive paths are copy-backed; C-Two lifetime policies invalidate FastDB owners before releasing transport leases, but do not imply direct final-backing construction.

The active implementation plan is [`2026-07-24 portable-payload contract composition`](../superpowers/plans/2026-07-24-portable-payload-contract-composition.md). Remaining distribution, SDK, backing, benchmark, publication, and hosted/release limits are tracked in [`contract-release deferred capabilities`](../issues/contract-release-deferred-capabilities.md).
