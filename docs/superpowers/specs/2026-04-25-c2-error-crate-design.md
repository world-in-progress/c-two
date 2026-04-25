# C2 Error Crate — Design Spec

**Date:** 2026-04-25
**Status:** Draft
**Scope:** Canonical cross-language C-Two error model

## Problem Statement

C-Two currently defines its canonical SDK error codes in Python
(`sdk/python/src/c_two/error.py`). Rust relay/core code independently expresses
related failures through HTTP status codes, JSON strings, and local Rust error
types. The recent `ResourceAlreadyRegistered` work made this duplication more
visible: one logical error appears as Python `CCError`, HTTP `409 DuplicateRoute`,
and Rust relay control-plane errors.

The goal is to make Rust own the stable C-Two error protocol while preserving
idiomatic exception/result types in each language SDK.

## Decision

Create a standalone Rust crate:

```text
core/foundation/c2-error
```

Do not put canonical errors under `c2-wire`. C-Two errors are system-level API
semantics, not only wire-frame internals. Registry, relay, IPC, HTTP, server,
and future language SDKs all need to depend on the same error registry without
pulling in wire codec dependencies.

## Alternatives Considered

### Option A: `c2-wire::error`

This is convenient for CRM error serialization, but it couples registry/relay
semantics to the wire crate. Errors like `ResourceAlreadyRegistered` and
`RegistryUnavailable` are not frame encoding errors. They belong to shared
platform semantics.

### Option B: Keep Errors Per Language

This keeps implementation short today, but each SDK must copy code numbers,
messages, serialization behavior, and mapping rules. Divergence becomes likely
as soon as TypeScript, Go, or Rust-native SDKs need the same behavior.

### Option C: Standalone `c2-error`

This keeps the canonical error table and serialization logic in a small,
dependency-light foundation crate. Wire, transport, bridge, and language SDKs
can all depend on it. This is the recommended option.

## Industrial Analogs

- gRPC defines stable cross-language status codes separately from any one
  language's exception mechanism.
- Kubernetes shares API error semantics through `apimachinery`, rather than
  making each client invent its own error model.
- Rust's `http` crate is a useful local analogy: it provides shared semantic
  types such as `StatusCode` without implementing a client or server.

The pattern is: centralize protocol semantics, adapt at language boundaries.

## Architecture

```text
core/foundation/c2-error
  owns:
    ErrorCode
    C2Error
    legacy code:message serialization
    code registry and conversion helpers

core/protocol/c2-wire
  may depend on c2-error in a later RPC payload migration
  keeps the existing serialized failure payload bytes in the first migration

core/transport/*
  depends on c2-error when surfacing system-level failures
  maps local transport failures into C2Error where appropriate

core/bridge/c2-ffi
  depends on c2-error
  maps Rust C2Error or typed relay errors into Python exception classes

sdk/python/c_two/error.py
  remains the Python exception layer
  mirrors canonical codes from c2-error
  maps ErrorCode -> Python exception subclass
```

Dependency rule:

```text
foundation/c2-error
  must not depend on protocol, transport, bridge, or Python-specific crates.
```

## Rust API Shape

Initial crate API:

```rust
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Unknown = 0,
    ResourceInputDeserializing = 1,
    ResourceOutputSerializing = 2,
    ResourceFunctionExecuting = 3,
    ClientInputSerializing = 5,
    ClientOutputDeserializing = 6,
    ClientCallingResource = 7,
    ResourceNotFound = 701,
    ResourceUnavailable = 702,
    ResourceAlreadyRegistered = 703,
    StaleResource = 704,
    RegistryUnavailable = 705,
    WriteConflict = 706,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2Error {
    pub code: ErrorCode,
    pub message: String,
}
```

Required helpers:

```rust
impl C2Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self;
    pub fn unknown(message: impl Into<String>) -> Self;
    pub fn to_legacy_bytes(&self) -> Vec<u8>;
    pub fn from_legacy_bytes(data: &[u8]) -> Result<Option<Self>, C2ErrorDecodeError>;
}

impl TryFrom<u16> for ErrorCode;
impl From<ErrorCode> for u16;
```

`C2Error` should implement `Display` and `std::error::Error`.

## Serialization Compatibility

The first migration must preserve the current Python wire format:

```text
<decimal-code>:<message>
```

Examples:

```text
703:Route name already registered with relay: 'grid'
701:Resource 'grid' not found
```

Empty bytes continue to mean "no error".

Unknown numeric codes must degrade to `ErrorCode::Unknown`, preserving the
original numeric code and message in the returned error message. This is
intentional: Rust core failures and future SDKs may occasionally return errors
before a stable C-Two code exists. Degrading to `Unknown` keeps logs useful and
preserves reproduction context. Malformed bytes that cannot be parsed as the
legacy format still fail decode.

Example:

```text
9999:low-level relay failure
```

deserializes to:

```text
ErrorCode::Unknown
message = "Unknown error code 9999: low-level relay failure"
```

The Python layer currently raises if `ERROR_Code(code_value)` is unknown; this
must change during migration.

## Python SDK Role

Python keeps idiomatic exception classes:

```python
class ResourceAlreadyRegistered(CCError): ...
```

But the canonical code numbers should be generated from, validated against, or
loaded from the Rust `c2-error` registry. Python should not be the source of
truth for numeric codes after this migration.

Acceptable first step:

- keep Python classes manually written;
- add tests that compare Python `ERROR_Code` values against a Rust-exported
  registry exposed through `_native`;
- move serialization compatibility tests to cover Rust and Python round trips.

Preferred later step:

- generate Python `ERROR_Code` definitions from a single Rust-owned registry or
  machine-readable schema emitted by `c2-error`.

FFI is sufficient for Python and for any future SDK that already embeds or links
against the Rust core. For a TS browser SDK that ships C-Two as WebAssembly,
the WASM exports are the browser equivalent of this boundary: TypeScript can
call exported registry/decoder functions and wrap the returned canonical
`code + message` into TS-native error classes.

The canonical registry should still be represented inside `c2-error` as plain
static data so it can be exposed through native FFI, WASM exports, or, if a
future non-native SDK needs it, JSON/schema without changing the source of
truth.

Known escape hatches:

- Browser WASM clients may prefer wasm-bindgen exports initially, and a
  WIT/component-model interface later if C-Two adopts the WebAssembly component
  model.
- Pure HTTP clients that do not link to the Rust core may need generated
  constants or a schema artifact.

These are not first-migration requirements.

## Mapping Rules

| Source | Mapping |
|--------|---------|
| RPC handler exception | `C2Error` serialized into RPC failure payload |
| Python `CCError` | canonical code + message |
| HTTP relay duplicate registration | HTTP `409 DuplicateRoute` -> `ResourceAlreadyRegistered` |
| Rust `RelayControlError::DuplicateRoute` | `C2Error(ResourceAlreadyRegistered)` or direct Python subclass at FFI boundary |
| Missing relay registry | `RegistryUnavailable` |
| Route exists but cannot be reached | `ResourceUnavailable` |
| No route exists | `ResourceNotFound` |

Not every internal Rust error must become `C2Error`. Local lifecycle failures
such as "relay is not running" may remain local runtime errors unless they cross
an SDK or RPC boundary as stable API behavior.

## Error Chaining Model

Keep `C2Error` minimal:

```rust
pub struct C2Error {
    pub code: ErrorCode,
    pub message: String,
}
```

C-Two cannot know the internal error models of user resource implementations.
Nested failures should therefore be represented by composing messages as errors
cross resource, transport, and client boundaries. This is closer to Go-style
error wrapping than to a structured exception tree. Structured details can be
added later if a concrete cross-language use case appears, but they are outside
the first migration.

## Migration Plan

1. Add `core/foundation/c2-error` to the Rust workspace.
2. Move the canonical code table into `c2-error`.
3. Implement legacy `code:message` encode/decode in Rust.
4. Add Rust tests for code values, unknown/malformed decode behavior, and
   empty-byte handling.
5. Leave `c2-wire` dependency-free until an RPC payload migration actually
   consumes `C2Error`.
6. Make transport/relay code use `c2-error` where errors cross public
   boundaries.
7. Expose the canonical registry through `_native` so Python can validate its
   local classes against Rust.
8. Update Python `c_two.error` tests to assert parity with the Rust registry.
9. Keep the existing Python exception API stable.

## Non-Goals

- Do not remove Python exception subclasses.
- Do not force every local Rust error into `C2Error`.
- Do not change the wire error byte format in the first migration.
- Do not make `c2-error` depend on `pyo3`, `reqwest`, `axum`, or transport
  crates.

## Resolved Design Choices

1. Unknown numeric codes degrade to `ErrorCode::Unknown` while preserving the
   original code and message. Malformed bytes still fail decode.
2. The first migration exposes the canonical registry through FFI. The registry
   remains plain static data so JSON/schema export can be added later if a
   non-native SDK requires it.
3. `C2Error` stays minimal: `code + message`. Error chains are represented by
   message composition, not structured details.
