//! PyO3 bindings for the `c2-wire` protocol codec.
//!
//! Exposes frame, control, buddy, chunk, ctrl, and handshake
//! encode/decode functions plus all flag and signal constants
//! to Python as part of `c_two._native`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

// ── Error conversion ────────────────────────────────────────────────────

fn decode_err(e: c2_wire::frame::DecodeError) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn encode_err(e: c2_wire::control::EncodeError) -> PyErr {
    PyValueError::new_err(e.to_string())
}

// ── PyO3 classes ────────────────────────────────────────────────────────

/// A single method in a route's method table.
#[pyclass(name = "MethodEntry", frozen)]
pub struct PyMethodEntry {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    index: u16,
}

#[pymethods]
impl PyMethodEntry {
    #[new]
    fn new(name: String, index: u16) -> Self {
        Self { name, index }
    }

    fn __repr__(&self) -> String {
        format!("MethodEntry(name='{}', index={})", self.name, self.index)
    }
}

/// Route info exchanged during handshake.
#[pyclass(name = "RouteInfo", frozen)]
pub struct PyRouteInfo {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    crm_ns: String,
    #[pyo3(get)]
    crm_name: String,
    #[pyo3(get)]
    crm_ver: String,
    #[pyo3(get)]
    abi_hash: String,
    #[pyo3(get)]
    signature_hash: String,
    #[pyo3(get)]
    max_payload_size: u64,
    #[pyo3(get)]
    methods: Vec<Py<PyMethodEntry>>,
}

#[pymethods]
impl PyRouteInfo {
    #[new]
    #[pyo3(signature = (name, methods, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, max_payload_size))]
    fn new(
        name: String,
        methods: Vec<Py<PyMethodEntry>>,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        max_payload_size: u64,
    ) -> PyResult<Self> {
        c2_contract::validate_crm_tag(crm_ns, crm_name, crm_ver)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        c2_contract::validate_contract_hash("abi_hash", abi_hash)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        c2_contract::validate_contract_hash("signature_hash", signature_hash)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        if max_payload_size == 0 {
            return Err(PyValueError::new_err("max_payload_size must be > 0"));
        }

        Ok(Self {
            name,
            crm_ns: crm_ns.to_string(),
            crm_name: crm_name.to_string(),
            crm_ver: crm_ver.to_string(),
            abi_hash: abi_hash.to_string(),
            signature_hash: signature_hash.to_string(),
            max_payload_size,
            methods,
        })
    }

    /// Look up method index by name, or `None` if not found.
    fn method_by_name(&self, name: &str) -> Option<u16> {
        self.methods.iter().find_map(|m| {
            let entry = m.get();
            if entry.name == name {
                Some(entry.index)
            } else {
                None
            }
        })
    }

    /// Look up method name by index, or `None` if not found.
    fn method_by_index(&self, idx: u16) -> Option<String> {
        self.methods.iter().find_map(|m| {
            let entry = m.get();
            if entry.index == idx {
                Some(entry.name.clone())
            } else {
                None
            }
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "RouteInfo(name='{}', crm_ns='{}', crm_name='{}', crm_ver='{}', abi_hash='{}', signature_hash='{}', max_payload_size={}, methods=[{}])",
            self.name,
            self.crm_ns,
            self.crm_name,
            self.crm_ver,
            self.abi_hash,
            self.signature_hash,
            self.max_payload_size,
            self.methods
                .iter()
                .map(|m| {
                    let e = m.get();
                    format!("{}={}", e.name, e.index)
                })
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Server identity exchanged in server→client handshake ACKs.
#[pyclass(name = "ServerIdentity", frozen)]
pub struct PyServerIdentity {
    #[pyo3(get)]
    server_id: String,
    #[pyo3(get)]
    server_instance_id: String,
}

#[pymethods]
impl PyServerIdentity {
    #[new]
    fn new(server_id: String, server_instance_id: String) -> Self {
        Self {
            server_id,
            server_instance_id,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "ServerIdentity(server_id='{}', server_instance_id='{}')",
            self.server_id, self.server_instance_id,
        )
    }
}

/// Decoded handshake payload.
#[pyclass(name = "Handshake", frozen)]
pub struct PyHandshake {
    #[pyo3(get)]
    segments: Vec<(String, u32)>,
    #[pyo3(get)]
    capability_flags: u16,
    #[pyo3(get)]
    routes: Vec<Py<PyRouteInfo>>,
    #[pyo3(get)]
    prefix: String,
    #[pyo3(get)]
    server_identity: Option<Py<PyServerIdentity>>,
}

#[pymethods]
impl PyHandshake {
    fn __repr__(&self) -> String {
        format!(
            "Handshake(prefix='{}', segments={}, caps=0x{:04x}, routes={})",
            self.prefix,
            self.segments.len(),
            self.capability_flags,
            self.routes.len(),
        )
    }
}

// ── Frame codec ─────────────────────────────────────────────────────────

/// Encode a complete frame: `[4B total_len][8B request_id][4B flags][payload]`.
#[pyfunction]
fn encode_frame(request_id: u64, flags: u32, payload: &[u8]) -> Vec<u8> {
    c2_wire::frame::encode_frame(request_id, flags, payload)
}

/// Decode a frame body (everything after the 4-byte `total_len` prefix).
///
/// Returns `(request_id, flags, payload)`.
#[pyfunction]
fn decode_frame(body: &[u8]) -> PyResult<(u64, u32, Vec<u8>)> {
    let total_len = body.len() as u32;
    let (header, payload) =
        c2_wire::frame::decode_frame_body(body, total_len).map_err(decode_err)?;
    Ok((header.request_id, header.flags, payload.to_vec()))
}

// ── Call / Reply control ────────────────────────────────────────────────

/// Encode V2 call control: `[1B name_len][route UTF-8][2B method_idx LE]`.
#[pyfunction]
fn encode_call_control(name: &str, method_idx: u16) -> PyResult<Vec<u8>> {
    c2_wire::control::encode_call_control(name, method_idx).map_err(encode_err)
}

/// Decode V2 call control from `data[offset..]`.
///
/// Returns `(route_name, method_idx, bytes_consumed)`.
#[pyfunction]
fn decode_call_control(data: &[u8], offset: usize) -> PyResult<(String, u16, usize)> {
    let (ctrl, consumed) =
        c2_wire::control::decode_call_control(data, offset).map_err(decode_err)?;
    Ok((ctrl.route_name, ctrl.method_idx, consumed))
}

/// Encode V2 reply control.
///
/// `status=0` → success (no body), `status=1` → error with optional data,
/// `status=2` → route-not-found with route name bytes.
#[pyfunction]
fn encode_reply_control(status: u8, error_data: Option<&[u8]>) -> PyResult<Vec<u8>> {
    let ctrl = match status {
        c2_wire::control::STATUS_SUCCESS => c2_wire::control::ReplyControl::Success,
        c2_wire::control::STATUS_ROUTE_NOT_FOUND => {
            let route = std::str::from_utf8(error_data.unwrap_or(&[]))
                .map_err(|e| PyValueError::new_err(e.to_string()))?
                .to_string();
            c2_wire::control::ReplyControl::RouteNotFound(route)
        }
        _ => c2_wire::control::ReplyControl::Error(error_data.unwrap_or(&[]).to_vec()),
    };
    c2_wire::control::try_encode_reply_control(&ctrl)
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Decode V2 reply control from `data[offset..]`.
///
/// Returns `(status, error_data_or_none, bytes_consumed)`.
#[pyfunction]
fn decode_reply_control(data: &[u8], offset: usize) -> PyResult<(u8, Option<Vec<u8>>, usize)> {
    let (ctrl, consumed) =
        c2_wire::control::decode_reply_control(data, offset).map_err(decode_err)?;
    match ctrl {
        c2_wire::control::ReplyControl::Success => {
            Ok((c2_wire::control::STATUS_SUCCESS, None, consumed))
        }
        c2_wire::control::ReplyControl::RouteNotFound(route) => Ok((
            c2_wire::control::STATUS_ROUTE_NOT_FOUND,
            Some(route.into_bytes()),
            consumed,
        )),
        c2_wire::control::ReplyControl::Error(err_data) => {
            Ok((c2_wire::control::STATUS_ERROR, Some(err_data), consumed))
        }
    }
}

// ── Buddy payload ───────────────────────────────────────────────────────

/// Encode buddy SHM pointer: `[2B seg_idx][4B offset][4B size][1B flags]`.
#[pyfunction]
fn encode_buddy_payload(seg_idx: u16, offset: u32, data_size: u32, is_dedicated: bool) -> Vec<u8> {
    let bp = c2_wire::buddy::BuddyPayload {
        seg_idx,
        offset,
        data_size,
        is_dedicated,
    };
    c2_wire::buddy::encode_buddy_payload(&bp).to_vec()
}

/// Decode buddy SHM pointer.
///
/// Returns `(seg_idx, offset, data_size, is_dedicated)`.
#[pyfunction]
fn decode_buddy_payload(payload: &[u8]) -> PyResult<(u16, u32, u32, bool)> {
    let (bp, _) = c2_wire::buddy::decode_buddy_payload(payload).map_err(decode_err)?;
    Ok((bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated))
}

// ── Chunk header ────────────────────────────────────────────────────────

/// Encode chunk header: `[2B chunk_idx LE][2B total_chunks LE]`.
#[pyfunction]
fn encode_chunk_header(chunk_idx: u16, total_chunks: u16) -> Vec<u8> {
    c2_wire::chunk::encode_chunk_header(chunk_idx, total_chunks).to_vec()
}

/// Decode chunk header from `data[offset..]`.
///
/// Returns `(chunk_idx, total_chunks, bytes_consumed)`.
#[pyfunction]
fn decode_chunk_header(data: &[u8], offset: usize) -> PyResult<(u16, u16, usize)> {
    c2_wire::chunk::decode_chunk_header(data, offset).map_err(decode_err)
}

// ── Control messages (IPC) ──────────────────────────────────────────────

/// Encode `CTRL_SEGMENT_ANNOUNCE`.
#[pyfunction]
fn encode_ctrl_segment_announce(direction: u8, index: u8, size: u32, name: &str) -> Vec<u8> {
    c2_wire::ctrl::encode_ctrl_segment_announce(direction, index, size, name)
}

/// Decode `CTRL_SEGMENT_ANNOUNCE`.
///
/// Returns `(direction, index, size, name)`.
#[pyfunction]
fn decode_ctrl_segment_announce(payload: &[u8]) -> PyResult<(u8, u8, u32, String)> {
    c2_wire::ctrl::decode_ctrl_segment_announce(payload).map_err(decode_err)
}

/// Encode `CTRL_CONSUMED`.
#[pyfunction]
fn encode_ctrl_consumed(direction: u8, index: u8) -> Vec<u8> {
    c2_wire::ctrl::encode_ctrl_consumed(direction, index).to_vec()
}

/// Decode `CTRL_CONSUMED`.
///
/// Returns `(direction, index)`.
#[pyfunction]
fn decode_ctrl_consumed(payload: &[u8]) -> PyResult<(u8, u8)> {
    c2_wire::ctrl::decode_ctrl_consumed(payload).map_err(decode_err)
}

/// Encode `CTRL_BUDDY_ANNOUNCE`.
#[pyfunction]
fn encode_ctrl_buddy_announce(seg_idx: u16, size: u32, name: &str) -> Vec<u8> {
    c2_wire::ctrl::encode_ctrl_buddy_announce(seg_idx, size, name)
}

/// Decode `CTRL_BUDDY_ANNOUNCE`.
///
/// Returns `(seg_idx, size, name)`.
#[pyfunction]
fn decode_ctrl_buddy_announce(payload: &[u8]) -> PyResult<(u16, u32, String)> {
    c2_wire::ctrl::decode_ctrl_buddy_announce(payload).map_err(decode_err)
}

// ── Handshake ───────────────────────────────────────────────────────────

/// Encode client→server handshake.
#[pyfunction]
fn encode_client_handshake(
    segments: Vec<(String, u32)>,
    capability_flags: u16,
    prefix: &str,
) -> PyResult<Vec<u8>> {
    c2_wire::handshake::encode_client_handshake(&segments, capability_flags, prefix)
        .map_err(encode_err)
}

/// Encode server→client handshake ACK (includes route tables).
#[pyfunction]
fn encode_server_handshake(
    segments: Vec<(String, u32)>,
    capability_flags: u16,
    routes: Vec<Py<PyRouteInfo>>,
    prefix: &str,
    server_id: &str,
    server_instance_id: &str,
) -> PyResult<Vec<u8>> {
    let rust_routes: Vec<c2_wire::handshake::RouteInfo> = routes
        .iter()
        .map(|r| {
            let r_ref = r.get();
            let methods = r_ref
                .methods
                .iter()
                .map(|m| {
                    let m_ref = m.get();
                    c2_wire::handshake::MethodEntry {
                        name: m_ref.name.clone(),
                        index: m_ref.index,
                    }
                })
                .collect();
            c2_wire::handshake::RouteInfo {
                name: r_ref.name.clone(),
                crm_ns: r_ref.crm_ns.clone(),
                crm_name: r_ref.crm_name.clone(),
                crm_ver: r_ref.crm_ver.clone(),
                abi_hash: r_ref.abi_hash.clone(),
                signature_hash: r_ref.signature_hash.clone(),
                max_payload_size: r_ref.max_payload_size,
                methods,
            }
        })
        .collect();
    let identity = c2_wire::handshake::ServerIdentity {
        server_id: server_id.to_string(),
        server_instance_id: server_instance_id.to_string(),
    };
    c2_wire::handshake::encode_server_handshake(
        &segments,
        capability_flags,
        &rust_routes,
        prefix,
        &identity,
    )
    .map_err(encode_err)
}

/// Decode handshake payload (both client and server directions).
#[pyfunction]
fn decode_handshake(py: Python<'_>, payload: &[u8]) -> PyResult<PyHandshake> {
    let hs = c2_wire::handshake::decode_handshake(payload).map_err(decode_err)?;

    let mut py_routes = Vec::with_capacity(hs.routes.len());
    for route in hs.routes {
        let py_methods: PyResult<Vec<Py<PyMethodEntry>>> = route
            .methods
            .into_iter()
            .map(|m| {
                Py::new(
                    py,
                    PyMethodEntry {
                        name: m.name,
                        index: m.index,
                    },
                )
            })
            .collect();
        py_routes.push(Py::new(
            py,
            PyRouteInfo {
                name: route.name,
                crm_ns: route.crm_ns,
                crm_name: route.crm_name,
                crm_ver: route.crm_ver,
                abi_hash: route.abi_hash,
                signature_hash: route.signature_hash,
                max_payload_size: route.max_payload_size,
                methods: py_methods?,
            },
        )?);
    }

    let server_identity = hs
        .server_identity
        .map(|identity| {
            Py::new(
                py,
                PyServerIdentity {
                    server_id: identity.server_id,
                    server_instance_id: identity.server_instance_id,
                },
            )
        })
        .transpose()?;

    Ok(PyHandshake {
        segments: hs.segments,
        capability_flags: hs.capability_flags,
        routes: py_routes,
        prefix: hs.prefix,
        server_identity,
    })
}

#[pyfunction]
fn contract_descriptor_sha256_hex(payload: &[u8]) -> PyResult<String> {
    c2_contract::contract_descriptor_sha256_hex(payload)
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn validate_portable_contract_descriptor(payload: &[u8]) -> PyResult<()> {
    c2_contract::validate_portable_contract_descriptor_json(payload)
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

// ── Module registration ─────────────────────────────────────────────────

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // ── Classes ──────────────────────────────────────────────────────
    m.add_class::<PyMethodEntry>()?;
    m.add_class::<PyRouteInfo>()?;
    m.add_class::<PyServerIdentity>()?;
    m.add_class::<PyHandshake>()?;

    // ── Frame codec ─────────────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_frame, m)?)?;
    m.add_function(wrap_pyfunction!(decode_frame, m)?)?;

    // ── Control plane codec ─────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_call_control, m)?)?;
    m.add_function(wrap_pyfunction!(decode_call_control, m)?)?;
    m.add_function(wrap_pyfunction!(encode_reply_control, m)?)?;
    m.add_function(wrap_pyfunction!(decode_reply_control, m)?)?;

    // ── Buddy payload ───────────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_buddy_payload, m)?)?;
    m.add_function(wrap_pyfunction!(decode_buddy_payload, m)?)?;

    // ── Chunk header ────────────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_chunk_header, m)?)?;
    m.add_function(wrap_pyfunction!(decode_chunk_header, m)?)?;

    // ── Control messages ────────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_ctrl_segment_announce, m)?)?;
    m.add_function(wrap_pyfunction!(decode_ctrl_segment_announce, m)?)?;
    m.add_function(wrap_pyfunction!(encode_ctrl_consumed, m)?)?;
    m.add_function(wrap_pyfunction!(decode_ctrl_consumed, m)?)?;
    m.add_function(wrap_pyfunction!(encode_ctrl_buddy_announce, m)?)?;
    m.add_function(wrap_pyfunction!(decode_ctrl_buddy_announce, m)?)?;

    // ── Handshake ───────────────────────────────────────────────────
    m.add_function(wrap_pyfunction!(encode_client_handshake, m)?)?;
    m.add_function(wrap_pyfunction!(encode_server_handshake, m)?)?;
    m.add_function(wrap_pyfunction!(decode_handshake, m)?)?;
    m.add_function(wrap_pyfunction!(contract_descriptor_sha256_hex, m)?)?;
    m.add_function(wrap_pyfunction!(validate_portable_contract_descriptor, m)?)?;

    // ── Flag constants (Python names — no _V2 suffix) ───────────────
    m.add("FLAG_SHM", c2_wire::flags::FLAG_SHM)?;
    m.add("FLAG_RESPONSE", c2_wire::flags::FLAG_RESPONSE)?;
    m.add("FLAG_HANDSHAKE", c2_wire::flags::FLAG_HANDSHAKE)?;
    m.add("FLAG_POOL", c2_wire::flags::FLAG_POOL)?;
    m.add("FLAG_CTRL", c2_wire::flags::FLAG_CTRL)?;
    m.add("FLAG_DISK_SPILL", c2_wire::flags::FLAG_DISK_SPILL)?;
    m.add("FLAG_BUDDY", c2_wire::flags::FLAG_BUDDY)?;
    m.add("FLAG_CALL", c2_wire::flags::FLAG_CALL_V2)?;
    m.add("FLAG_REPLY", c2_wire::flags::FLAG_REPLY_V2)?;
    m.add("FLAG_CHUNKED", c2_wire::flags::FLAG_CHUNKED)?;
    m.add("FLAG_CHUNK_LAST", c2_wire::flags::FLAG_CHUNK_LAST)?;
    m.add("FLAG_SIGNAL", c2_wire::flags::FLAG_SIGNAL)?;

    // ── Status codes ────────────────────────────────────────────────
    m.add("STATUS_SUCCESS", c2_wire::control::STATUS_SUCCESS)?;
    m.add("STATUS_ERROR", c2_wire::control::STATUS_ERROR)?;
    m.add(
        "STATUS_ROUTE_NOT_FOUND",
        c2_wire::control::STATUS_ROUTE_NOT_FOUND,
    )?;

    // ── Handshake constants ─────────────────────────────────────────
    m.add("HANDSHAKE_VERSION", c2_wire::handshake::HANDSHAKE_VERSION)?;
    m.add("CAP_CALL", c2_wire::handshake::CAP_CALL_V2)?;
    m.add("CAP_METHOD_IDX", c2_wire::handshake::CAP_METHOD_IDX)?;
    m.add("CAP_CHUNKED", c2_wire::handshake::CAP_CHUNKED)?;

    // ── Chunk header constant ───────────────────────────────────────
    m.add("CHUNK_HEADER_SIZE", c2_wire::chunk::CHUNK_HEADER_SIZE)?;

    // ── MsgType integer values (for Python IntEnum) ─────────────────
    m.add("MSG_PING", c2_wire::msg_type::MsgType::Ping as u8)?;
    m.add("MSG_PONG", c2_wire::msg_type::MsgType::Pong as u8)?;
    m.add("MSG_CRM_CALL", c2_wire::msg_type::MsgType::CrmCall as u8)?;
    m.add("MSG_CRM_REPLY", c2_wire::msg_type::MsgType::CrmReply as u8)?;
    m.add(
        "MSG_SHUTDOWN_CLIENT",
        c2_wire::msg_type::MsgType::ShutdownClient as u8,
    )?;
    m.add(
        "MSG_SHUTDOWN_ACK",
        c2_wire::msg_type::MsgType::ShutdownAck as u8,
    )?;
    m.add(
        "MSG_DISCONNECT",
        c2_wire::msg_type::MsgType::Disconnect as u8,
    )?;
    m.add(
        "MSG_DISCONNECT_ACK",
        c2_wire::msg_type::MsgType::DisconnectAck as u8,
    )?;

    // ── Signal bytes ────────────────────────────────────────────────
    let py = m.py();
    m.add(
        "PING_BYTES",
        PyBytes::new(py, &c2_wire::msg_type::PING_BYTES),
    )?;
    m.add(
        "PONG_BYTES",
        PyBytes::new(py, &c2_wire::msg_type::PONG_BYTES),
    )?;
    m.add(
        "DISCONNECT_BYTES",
        PyBytes::new(py, &c2_wire::msg_type::DISCONNECT_BYTES),
    )?;
    m.add(
        "DISCONNECT_ACK_BYTES",
        PyBytes::new(py, &c2_wire::msg_type::DISCONNECT_ACK_BYTES),
    )?;
    m.add("SIGNAL_SIZE", c2_wire::msg_type::SIGNAL_SIZE)?;

    // ── Control message constants ───────────────────────────────────
    m.add(
        "CTRL_SEGMENT_ANNOUNCE",
        c2_wire::ctrl::CTRL_SEGMENT_ANNOUNCE,
    )?;
    m.add("CTRL_CONSUMED", c2_wire::ctrl::CTRL_CONSUMED)?;
    m.add("CTRL_BUDDY_ANNOUNCE", c2_wire::ctrl::CTRL_BUDDY_ANNOUNCE)?;
    m.add("POOL_DIR_OUTBOUND", c2_wire::ctrl::POOL_DIR_OUTBOUND)?;
    m.add("POOL_DIR_RESPONSE", c2_wire::ctrl::POOL_DIR_RESPONSE)?;

    // ── Size constants ──────────────────────────────────────────────
    m.add("FRAME_HEADER_SIZE", c2_wire::frame::HEADER_SIZE)?;
    m.add("BUDDY_PAYLOAD_SIZE", c2_wire::buddy::BUDDY_PAYLOAD_SIZE)?;

    Ok(())
}
