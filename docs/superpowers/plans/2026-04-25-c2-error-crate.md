# C2 Error Crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move C-Two's canonical error code registry and legacy error-byte codec into a standalone Rust `c2-error` crate while preserving Python exception behavior.

**Architecture:** Add `core/foundation/c2-error` as the source of truth for `ErrorCode`, `C2Error`, and `<decimal-code>:<message>` encode/decode. Expose the Rust registry and decoder through `_native`, then update Python tests and `c_two.error` so Python remains the idiomatic exception layer while validating against Rust-owned codes.

**Tech Stack:** Rust 2024 workspace crate, PyO3 `_native` bridge, Python `IntEnum`/exception classes, pytest, cargo test/check.

---

## File Structure

- Create: `core/foundation/c2-error/Cargo.toml`
  - New dependency-light foundation crate.
- Create: `core/foundation/c2-error/src/lib.rs`
  - Defines `ErrorCode`, `C2Error`, `C2ErrorDecodeError`, legacy codec, and registry helpers.
- Modify: `core/Cargo.toml`
  - Add `foundation/c2-error` workspace member.
- Modify: `core/bridge/c2-ffi/Cargo.toml`
  - Add dependency on `c2-error`.
- Create: `core/bridge/c2-ffi/src/error_ffi.rs`
  - Exposes `error_registry()`, `decode_error_legacy()`, and `encode_error_legacy()` to Python.
- Modify: `core/bridge/c2-ffi/src/lib.rs`
  - Register `error_ffi`.
- Modify: `sdk/python/src/c_two/error.py`
  - Degrade unknown numeric codes to `ERROR_UNKNOWN`, keep original code/message in `message`.
  - Optionally use `_native.error_registry()` for parity validation in tests, not at import time.
- Modify: `sdk/python/tests/unit/test_error.py`
  - Add tests for unknown-code fallback and Rust registry parity.
- Create: `sdk/python/tests/unit/test_native_error_registry.py`
  - Tests `_native` error registry and legacy codec exports directly.

## Task 1: Add Rust `c2-error` crate and canonical code table

**Files:**
- Create: `core/foundation/c2-error/Cargo.toml`
- Create: `core/foundation/c2-error/src/lib.rs`
- Modify: `core/Cargo.toml`

- [x] **Step 1: Write failing Rust crate test in new crate**

Create `core/foundation/c2-error/Cargo.toml`:

```toml
[package]
name = "c2-error"
edition.workspace = true
version.workspace = true
description = "Canonical C-Two error codes and legacy error serialization"

[dependencies]
```

Create `core/foundation/c2-error/src/lib.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::{C2Error, ErrorCode};

    #[test]
    fn canonical_error_codes_match_python_compatibility_values() {
        assert_eq!(u16::from(ErrorCode::Unknown), 0);
        assert_eq!(u16::from(ErrorCode::ResourceInputDeserializing), 1);
        assert_eq!(u16::from(ErrorCode::ResourceOutputSerializing), 2);
        assert_eq!(u16::from(ErrorCode::ResourceFunctionExecuting), 3);
        assert_eq!(u16::from(ErrorCode::ClientInputSerializing), 5);
        assert_eq!(u16::from(ErrorCode::ClientOutputDeserializing), 6);
        assert_eq!(u16::from(ErrorCode::ClientCallingResource), 7);
        assert_eq!(u16::from(ErrorCode::ResourceNotFound), 701);
        assert_eq!(u16::from(ErrorCode::ResourceUnavailable), 702);
        assert_eq!(u16::from(ErrorCode::ResourceAlreadyRegistered), 703);
        assert_eq!(u16::from(ErrorCode::StaleResource), 704);
        assert_eq!(u16::from(ErrorCode::RegistryUnavailable), 705);
        assert_eq!(u16::from(ErrorCode::WriteConflict), 706);
    }

    #[test]
    fn c2_error_display_uses_code_name_and_message() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(err.to_string(), "ResourceAlreadyRegistered: grid exists");
    }
}
```

Modify `core/Cargo.toml` workspace members:

```toml
members = [
    "foundation/c2-config",
    "foundation/c2-error",
    "foundation/c2-mem",
    "protocol/c2-wire",
    "transport/c2-ipc",
    "transport/c2-http",
    "transport/c2-server",
    "bridge/c2-ffi",
]
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p c2-error
```

Expected: FAIL because `C2Error` and `ErrorCode` do not exist.

- [x] **Step 3: Implement minimal Rust types**

Replace `core/foundation/c2-error/src/lib.rs` with:

```rust
use std::fmt;

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

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> Self {
        code as u16
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2Error {
    pub code: ErrorCode,
    pub message: String,
}

impl C2Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unknown, message)
    }
}

impl fmt::Display for C2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for C2Error {}

#[cfg(test)]
mod tests {
    use super::{C2Error, ErrorCode};

    #[test]
    fn canonical_error_codes_match_python_compatibility_values() {
        assert_eq!(u16::from(ErrorCode::Unknown), 0);
        assert_eq!(u16::from(ErrorCode::ResourceInputDeserializing), 1);
        assert_eq!(u16::from(ErrorCode::ResourceOutputSerializing), 2);
        assert_eq!(u16::from(ErrorCode::ResourceFunctionExecuting), 3);
        assert_eq!(u16::from(ErrorCode::ClientInputSerializing), 5);
        assert_eq!(u16::from(ErrorCode::ClientOutputDeserializing), 6);
        assert_eq!(u16::from(ErrorCode::ClientCallingResource), 7);
        assert_eq!(u16::from(ErrorCode::ResourceNotFound), 701);
        assert_eq!(u16::from(ErrorCode::ResourceUnavailable), 702);
        assert_eq!(u16::from(ErrorCode::ResourceAlreadyRegistered), 703);
        assert_eq!(u16::from(ErrorCode::StaleResource), 704);
        assert_eq!(u16::from(ErrorCode::RegistryUnavailable), 705);
        assert_eq!(u16::from(ErrorCode::WriteConflict), 706);
    }

    #[test]
    fn c2_error_display_uses_code_name_and_message() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(err.to_string(), "ResourceAlreadyRegistered: grid exists");
    }
}
```

- [x] **Step 4: Run test to verify it passes**

Run:

```bash
cargo test -p c2-error
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/Cargo.toml core/foundation/c2-error
git commit -m "feat: add canonical c2-error crate"
```

## Task 2: Implement legacy `code:message` codec in Rust

**Files:**
- Modify: `core/foundation/c2-error/src/lib.rs`

- [x] **Step 1: Write failing codec tests**

Add these tests to the existing test module:

```rust
#[test]
fn legacy_encode_matches_existing_python_format() {
    let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
    assert_eq!(err.to_legacy_bytes(), b"703:grid exists");
}

#[test]
fn legacy_decode_empty_bytes_means_no_error() {
    assert_eq!(C2Error::from_legacy_bytes(b"").unwrap(), None);
}

#[test]
fn legacy_decode_known_code_returns_canonical_error() {
    let err = C2Error::from_legacy_bytes(b"701:missing grid").unwrap().unwrap();
    assert_eq!(err.code, ErrorCode::ResourceNotFound);
    assert_eq!(err.message, "missing grid");
}

#[test]
fn legacy_decode_preserves_colons_in_message() {
    let err = C2Error::from_legacy_bytes(b"0:host:port:extra").unwrap().unwrap();
    assert_eq!(err.code, ErrorCode::Unknown);
    assert_eq!(err.message, "host:port:extra");
}

#[test]
fn legacy_decode_unknown_code_degrades_to_unknown_with_context() {
    let err = C2Error::from_legacy_bytes(b"9999:low-level relay failure").unwrap().unwrap();
    assert_eq!(err.code, ErrorCode::Unknown);
    assert_eq!(err.message, "Unknown error code 9999: low-level relay failure");
}

#[test]
fn legacy_decode_malformed_code_fails() {
    let err = C2Error::from_legacy_bytes(b"abc:not a number").unwrap_err();
    assert_eq!(err.to_string(), "invalid legacy C2 error code: abc");
}
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p c2-error
```

Expected: FAIL because `to_legacy_bytes`, `from_legacy_bytes`, and `C2ErrorDecodeError` are missing.

- [x] **Step 3: Implement codec and decode error**

Add below `impl From<ErrorCode> for u16`:

```rust
impl TryFrom<u16> for ErrorCode {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ErrorCode::Unknown),
            1 => Ok(ErrorCode::ResourceInputDeserializing),
            2 => Ok(ErrorCode::ResourceOutputSerializing),
            3 => Ok(ErrorCode::ResourceFunctionExecuting),
            5 => Ok(ErrorCode::ClientInputSerializing),
            6 => Ok(ErrorCode::ClientOutputDeserializing),
            7 => Ok(ErrorCode::ClientCallingResource),
            701 => Ok(ErrorCode::ResourceNotFound),
            702 => Ok(ErrorCode::ResourceUnavailable),
            703 => Ok(ErrorCode::ResourceAlreadyRegistered),
            704 => Ok(ErrorCode::StaleResource),
            705 => Ok(ErrorCode::RegistryUnavailable),
            706 => Ok(ErrorCode::WriteConflict),
            _ => Err(()),
        }
    }
}
```

Add below `C2Error`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C2ErrorDecodeError {
    InvalidUtf8,
    MissingSeparator,
    InvalidCode(String),
}

impl fmt::Display for C2ErrorDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            C2ErrorDecodeError::InvalidUtf8 => f.write_str("invalid legacy C2 error UTF-8"),
            C2ErrorDecodeError::MissingSeparator => {
                f.write_str("invalid legacy C2 error: missing ':' separator")
            }
            C2ErrorDecodeError::InvalidCode(code) => {
                write!(f, "invalid legacy C2 error code: {code}")
            }
        }
    }
}

impl std::error::Error for C2ErrorDecodeError {}
```

Add these methods to `impl C2Error`:

```rust
pub fn to_legacy_bytes(&self) -> Vec<u8> {
    format!("{}:{}", u16::from(self.code), self.message).into_bytes()
}

pub fn from_legacy_bytes(data: &[u8]) -> Result<Option<Self>, C2ErrorDecodeError> {
    if data.is_empty() {
        return Ok(None);
    }

    let raw = std::str::from_utf8(data).map_err(|_| C2ErrorDecodeError::InvalidUtf8)?;
    let (code_raw, message) = raw
        .split_once(':')
        .ok_or(C2ErrorDecodeError::MissingSeparator)?;
    let code_value = code_raw
        .parse::<u16>()
        .map_err(|_| C2ErrorDecodeError::InvalidCode(code_raw.to_string()))?;

    match ErrorCode::try_from(code_value) {
        Ok(code) => Ok(Some(C2Error::new(code, message))),
        Err(()) => Ok(Some(C2Error::unknown(format!(
            "Unknown error code {code_value}: {message}"
        )))),
    }
}
```

- [x] **Step 4: Run test to verify it passes**

Run:

```bash
cargo test -p c2-error
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/foundation/c2-error/src/lib.rs
git commit -m "feat: add legacy c2 error codec"
```

## Task 3: Expose Rust error registry and codec through `_native`

**Files:**
- Modify: `core/bridge/c2-ffi/Cargo.toml`
- Create: `core/bridge/c2-ffi/src/error_ffi.rs`
- Modify: `core/bridge/c2-ffi/src/lib.rs`
- Create: `sdk/python/tests/unit/test_native_error_registry.py`

- [x] **Step 1: Write failing Python tests**

Create `sdk/python/tests/unit/test_native_error_registry.py`:

```python
from c_two import _native


def test_native_error_registry_exposes_canonical_codes():
    registry = _native.error_registry()
    assert registry["Unknown"] == 0
    assert registry["ResourceNotFound"] == 701
    assert registry["ResourceUnavailable"] == 702
    assert registry["ResourceAlreadyRegistered"] == 703
    assert registry["StaleResource"] == 704
    assert registry["RegistryUnavailable"] == 705
    assert registry["WriteConflict"] == 706


def test_native_decode_legacy_error_known_code():
    decoded = _native.decode_error_legacy(b"703:grid exists")
    assert decoded == {
        "code": 703,
        "name": "ResourceAlreadyRegistered",
        "message": "grid exists",
    }


def test_native_decode_legacy_error_unknown_code_degrades():
    decoded = _native.decode_error_legacy(b"9999:relay exploded")
    assert decoded == {
        "code": 0,
        "name": "Unknown",
        "message": "Unknown error code 9999: relay exploded",
    }


def test_native_decode_legacy_error_empty_bytes_returns_none():
    assert _native.decode_error_legacy(b"") is None


def test_native_encode_legacy_error_matches_python_wire_format():
    assert _native.encode_error_legacy(703, "grid exists") == b"703:grid exists"
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_native_error_registry.py -q --timeout=60
```

Expected: FAIL because `_native.error_registry`, `_native.decode_error_legacy`, and `_native.encode_error_legacy` do not exist.

- [x] **Step 3: Add `c2-error` dependency to FFI crate**

Modify `core/bridge/c2-ffi/Cargo.toml`:

```toml
c2-error = { path = "../../foundation/c2-error" }
```

- [x] **Step 4: Implement `error_ffi.rs`**

Create `core/bridge/c2-ffi/src/error_ffi.rs`:

```rust
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use c2_error::{C2Error, ErrorCode};

fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unknown => "Unknown",
        ErrorCode::ResourceInputDeserializing => "ResourceInputDeserializing",
        ErrorCode::ResourceOutputSerializing => "ResourceOutputSerializing",
        ErrorCode::ResourceFunctionExecuting => "ResourceFunctionExecuting",
        ErrorCode::ClientInputSerializing => "ClientInputSerializing",
        ErrorCode::ClientOutputDeserializing => "ClientOutputDeserializing",
        ErrorCode::ClientCallingResource => "ClientCallingResource",
        ErrorCode::ResourceNotFound => "ResourceNotFound",
        ErrorCode::ResourceUnavailable => "ResourceUnavailable",
        ErrorCode::ResourceAlreadyRegistered => "ResourceAlreadyRegistered",
        ErrorCode::StaleResource => "StaleResource",
        ErrorCode::RegistryUnavailable => "RegistryUnavailable",
        ErrorCode::WriteConflict => "WriteConflict",
    }
}

fn all_codes() -> &'static [ErrorCode] {
    &[
        ErrorCode::Unknown,
        ErrorCode::ResourceInputDeserializing,
        ErrorCode::ResourceOutputSerializing,
        ErrorCode::ResourceFunctionExecuting,
        ErrorCode::ClientInputSerializing,
        ErrorCode::ClientOutputDeserializing,
        ErrorCode::ClientCallingResource,
        ErrorCode::ResourceNotFound,
        ErrorCode::ResourceUnavailable,
        ErrorCode::ResourceAlreadyRegistered,
        ErrorCode::StaleResource,
        ErrorCode::RegistryUnavailable,
        ErrorCode::WriteConflict,
    ]
}

#[pyfunction]
fn error_registry(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for code in all_codes() {
        dict.set_item(code_name(*code), u16::from(*code))?;
    }
    Ok(dict.into_any().unbind())
}

#[pyfunction]
fn decode_error_legacy(py: Python<'_>, data: &[u8]) -> PyResult<Option<Py<PyAny>>> {
    let Some(err) = C2Error::from_legacy_bytes(data)
        .map_err(|e| PyValueError::new_err(e.to_string()))?
    else {
        return Ok(None);
    };

    let dict = PyDict::new(py);
    dict.set_item("code", u16::from(err.code))?;
    dict.set_item("name", code_name(err.code))?;
    dict.set_item("message", err.message)?;
    Ok(Some(dict.into_any().unbind()))
}

#[pyfunction]
fn encode_error_legacy<'py>(
    py: Python<'py>,
    code: u16,
    message: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    let code = ErrorCode::try_from(code).unwrap_or(ErrorCode::Unknown);
    let err = C2Error::new(code, message);
    Ok(PyBytes::new(py, &err.to_legacy_bytes()))
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(error_registry, m)?)?;
    m.add_function(wrap_pyfunction!(decode_error_legacy, m)?)?;
    m.add_function(wrap_pyfunction!(encode_error_legacy, m)?)?;
    Ok(())
}
```

- [x] **Step 5: Register module functions**

Modify `core/bridge/c2-ffi/src/lib.rs`:

```rust
#[cfg(feature = "python")]
mod error_ffi;
```

and in `c2_native`:

```rust
error_ffi::register_module(m)?;
```

Place it before client/http registration so error helpers are available early.

- [x] **Step 6: Run test to verify it passes**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_native_error_registry.py -q --timeout=60
```

Expected: PASS.

- [x] **Step 7: Run Rust bridge check**

Run:

```bash
cargo check -p c2-ffi --features python
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add core/bridge/c2-ffi/Cargo.toml core/bridge/c2-ffi/src/error_ffi.rs core/bridge/c2-ffi/src/lib.rs sdk/python/tests/unit/test_native_error_registry.py
git commit -m "feat: expose canonical c2 errors through native bridge"
```

## Task 4: Make Python unknown-code decode resilient and validate parity

**Files:**
- Modify: `sdk/python/src/c_two/error.py`
- Modify: `sdk/python/tests/unit/test_error.py`

- [x] **Step 1: Write failing Python tests**

Add to `TestCCErrorSerialization` in `sdk/python/tests/unit/test_error.py`:

```python
    def test_unknown_numeric_code_deserializes_to_unknown_with_context(self):
        restored = CCError.deserialize(memoryview(b"9999:low-level relay failure"))
        assert restored is not None
        assert type(restored) is CCError
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == "Unknown error code 9999: low-level relay failure"
```

Add a new test class:

```python
class TestNativeErrorRegistryParity:
    def test_python_error_codes_match_native_registry(self):
        from c_two import _native

        native = _native.error_registry()
        assert ERROR_Code.ERROR_UNKNOWN.value == native["Unknown"]
        assert ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING.value == native["ResourceInputDeserializing"]
        assert ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING.value == native["ResourceOutputSerializing"]
        assert ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING.value == native["ResourceFunctionExecuting"]
        assert ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING.value == native["ClientInputSerializing"]
        assert ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING.value == native["ClientOutputDeserializing"]
        assert ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE.value == native["ClientCallingResource"]
        assert ERROR_Code.ERROR_RESOURCE_NOT_FOUND.value == native["ResourceNotFound"]
        assert ERROR_Code.ERROR_RESOURCE_UNAVAILABLE.value == native["ResourceUnavailable"]
        assert ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED.value == native["ResourceAlreadyRegistered"]
        assert ERROR_Code.ERROR_REGISTRY_UNAVAILABLE.value == native["RegistryUnavailable"]
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_error.py -q --timeout=60
```

Expected: FAIL on unknown numeric code because `ERROR_Code(code_value)` raises `ValueError`.

- [x] **Step 3: Update `CCError.deserialize` fallback**

In `sdk/python/src/c_two/error.py`, replace:

```python
code = ERROR_Code(code_value)
```

with:

```python
try:
    code = ERROR_Code(code_value)
except ValueError:
    code = ERROR_Code.ERROR_UNKNOWN
    message = f'Unknown error code {code_value}: {message or ""}'
```

Keep the remaining subclass lookup unchanged.

- [x] **Step 4: Run test to verify it passes**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_error.py -q --timeout=60
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add sdk/python/src/c_two/error.py sdk/python/tests/unit/test_error.py
git commit -m "fix: align python error decoding with canonical registry"
```

## Task 5: Wire `c2-error` into Rust consumers without changing payload format

**Files:**
- Modify: `core/transport/c2-http/Cargo.toml`
- Modify: `core/transport/c2-http/src/relay/server.rs`

- [x] **Step 1: Write failing Rust test for relay duplicate conversion**

In `core/transport/c2-http/src/relay/server.rs` test module, add:

```rust
#[test]
fn duplicate_route_error_maps_to_c2_error() {
    let err = RelayControlError::DuplicateRoute {
        name: "grid".to_string(),
    };
    let c2 = err.to_c2_error();

    assert_eq!(c2.code, c2_error::ErrorCode::ResourceAlreadyRegistered);
    assert_eq!(c2.message, "Route name already registered: 'grid'");
}
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p c2-http --features relay relay::server::tests::duplicate_route_error_maps_to_c2_error
```

Expected: FAIL because `c2-error` dependency and `to_c2_error` are missing.

- [x] **Step 3: Add relay-scoped dependency**

Do not add a `c2-wire` dependency until a later RPC payload migration actually
uses `C2Error`.

Modify `core/transport/c2-http/Cargo.toml`:

```toml
c2-error = { path = "../../foundation/c2-error", optional = true }
```

and include `"dep:c2-error"` in the `relay` feature.

- [x] **Step 4: Implement conversion**

In `core/transport/c2-http/src/relay/server.rs`, add:

```rust
impl RelayControlError {
    pub fn to_c2_error(&self) -> c2_error::C2Error {
        match self {
            RelayControlError::DuplicateRoute { .. } => {
                c2_error::C2Error::new(
                    c2_error::ErrorCode::ResourceAlreadyRegistered,
                    self.to_string(),
                )
            }
            RelayControlError::Other(message) => c2_error::C2Error::unknown(message.clone()),
        }
    }
}
```

- [x] **Step 5: Run targeted Rust test**

Run:

```bash
cargo test -p c2-http --features relay relay::server::tests::duplicate_route_error_maps_to_c2_error
```

Expected: PASS.

- [x] **Step 6: Run broader Rust checks**

Run:

```bash
cargo test -p c2-error
cargo test -p c2-http --features relay
cargo check -p c2-ffi --features python
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add core/protocol/c2-wire/Cargo.toml core/transport/c2-http/Cargo.toml core/transport/c2-http/src/relay/server.rs
git commit -m "feat: map relay control errors to canonical c2 errors"
```

## Task 6: Final regression and documentation consistency

**Files:**
- Verify: `docs/superpowers/specs/2026-04-25-c2-error-crate-design.md`
- Verify: `docs/superpowers/specs/2026-04-25-resource-registration-error-design.md`
- Verify: `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`

- [x] **Step 1: Run Python error and relay regression**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/unit/test_error.py sdk/python/tests/unit/test_native_error_registry.py sdk/python/tests/unit/test_mesh_errors.py sdk/python/tests/unit/test_native_relay.py sdk/python/tests/integration/test_registry.py sdk/python/tests/integration/test_http_relay.py -q --timeout=60
```

Expected: PASS.

- [x] **Step 2: Run Rust regression**

Run:

```bash
cargo test -p c2-error
cargo test -p c2-http --features relay
cargo check -p c2-ffi --features python
```

Expected: PASS.

- [ ] **Step 3: Check docs for conflicting codes**

Run:

```bash
rg -n "ResourceAlreadyRegistered|WriteConflict|703|706" docs/superpowers/specs sdk/python/src/c_two/error.py core/foundation/c2-error core/transport/c2-http/src/relay/server.rs
```

Expected:

- `ResourceAlreadyRegistered` uses code `703`.
- `WriteConflict` uses code `706`.
- No document assigns code `703` to `WriteConflict`.

- [ ] **Step 4: Check whitespace**

Run:

```bash
git diff --check
```

Expected: no output and exit 0.

- [ ] **Step 5: Commit docs/cleanup**

```bash
git add docs/superpowers/specs/2026-04-25-c2-error-crate-design.md docs/superpowers/plans/2026-04-25-c2-error-crate.md
git commit -m "docs: plan canonical c2 error crate"
```

## Self-Review Notes

- Spec coverage:
  - Standalone `c2-error` crate: Tasks 1-2.
  - Legacy wire compatibility: Task 2.
  - Unknown-code fallback: Tasks 2 and 4.
  - FFI registry first, exportable static data later: Task 3.
  - Minimal `code + message`: Tasks 1-2.
  - Python remains exception layer: Task 4.
  - Rust relay mapping: Task 5.
- Placeholder scan:
  - No `TBD`, `TODO`, or unspecified "add tests" steps remain.
- Risk:
  - Existing uncommitted duplicate-registration work should be committed or reviewed before executing this plan, because Task 5 touches `core/transport/c2-http/src/relay/server.rs`, which already has pending edits.
