# Cross-Language Contract Codec Architecture

> **Status:** Superseded by `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md` for the portable CRM payload boundary.

This document used to describe an open-ended cross-language payload-codec architecture. That direction has been replaced for C-Two 0.x: FastDB call-db is the first-class portable CRM payload ABI. C-Two owns the FastDB-backed contract planner, bridge derivation, descriptor/artifact/codegen orchestration, TypeScript typed facade, and cross-transport verification. FastDB owns generic schema, storage, serialization, views, owned bytes, and runtime primitives consumed by C-Two.

Python pickle remains a Python-only local/prototype fallback. C-Two no longer ships alternate payload modules or a public codec registry. Strict portable export and cross-language codegen reject pickle and non-FastDB `PayloadAbiRef` values.

The key boundary is runtime ownership: C-Two contract layers depend on FastDB ABI, while C-Two runtime route, relay, IPC, scheduler, lease, and lifecycle layers do not parse FastDB storage internals.
