# Resource Registration Error — Design Spec

**Date:** 2026-04-25
**Status:** Draft
**Scope:** Duplicate resource registration error behavior for relay-backed registration

## Problem Statement

When two processes on the same node register the same resource name through the
same relay, the relay correctly rejects the second registration. Before this
change, the Python SDK surfaced that rejection as a plain `RuntimeError`, which
made it hard for users to catch the expected conflict without also catching
unrelated runtime failures.

The goal is to give duplicate registration a stable C-Two error identity while
preserving the current relay-based ownership model:

- relay-backed duplicate registration fails synchronously;
- standalone registration without a relay remains permissive across processes;
- same-process duplicate registration remains a local SDK validation error.

## Error Contract

Add a dedicated Python error class:

| Error | Code | Raised When |
|-------|------|-------------|
| `ResourceAlreadyRegistered` | 703 | A relay rejects registration because the resource name is already owned by another live local route |

`ResourceAlreadyRegistered` is a `CCError` subclass, serializes through the
existing `CCError.serialize()` format, and deserializes back to the subclass via
`CCError.deserialize()`.

Recommended user handling:

```python
from c_two.error import ResourceAlreadyRegistered

try:
    cc.register(IMyResource, impl, name="grid")
except ResourceAlreadyRegistered:
    # Decide whether to reuse the existing service, exit cleanly, or pick a new name.
    ...
```

## Rust/Python Boundary

Rust relay core remains the authority for duplicate registration detection.

The HTTP control plane already represents the conflict as:

- HTTP status: `409 Conflict`
- JSON error: `"DuplicateRoute"`

The Python SDK maps that protocol-level conflict to
`ResourceAlreadyRegistered`. This keeps Python users on C-Two error types
without forcing Rust relay internals to depend on Python exception classes.

The embedded Rust relay API exposed through `_native.NativeRelay` should follow
the same Python-visible error contract. If `NativeRelay.register_upstream()`
receives the Rust relay conflict for an already-registered route, the FFI layer
should raise `c_two.error.ResourceAlreadyRegistered` instead of a generic
`RuntimeError`. Other relay lifecycle and connection errors remain
`RuntimeError`.

## Behavior Matrix

| Scenario | Expected Behavior |
|----------|-------------------|
| Same process calls `cc.register()` twice with the same name | Raise existing `ValueError` before contacting relay |
| Two processes call `cc.register()` with same name and no relay configured | Both may run; no duplicate prevention is attempted |
| Two processes call `cc.register()` with same name and the same relay configured | Second registration raises `ResourceAlreadyRegistered` synchronously |
| Relay rejects registration after local server startup | Local registration rolls back before the exception escapes |
| HTTP `POST /_register` duplicate live local route | Return `409 Conflict` with `DuplicateRoute` |
| `_native.NativeRelay.register_upstream()` duplicate route | Raise `ResourceAlreadyRegistered` at the Python boundary |

## Non-Goals

- Do not add file locks, process locks, or standalone single-node duplicate
  prevention outside the relay path.
- Do not change relay upsert behavior for the same route name and same IPC
  address, or for a dead previous IPC connection.
- Do not make all Rust relay errors into C-Two Python errors. This spec only
  covers the duplicate registration conflict.

## Test Plan

Python SDK:

- `c_two.error.ResourceAlreadyRegistered` has code `703`.
- `CCError.deserialize(CCError.serialize(ResourceAlreadyRegistered(...)))`
  returns `ResourceAlreadyRegistered`.
- Relay-backed duplicate `cc.register()` raises `ResourceAlreadyRegistered`.
- Failed relay registration rolls back the local process registry and server.

Rust relay / FFI:

- HTTP relay duplicate live local route continues returning `409 DuplicateRoute`.
- `_native.NativeRelay.register_upstream()` duplicate route raises
  `ResourceAlreadyRegistered` from Python.

