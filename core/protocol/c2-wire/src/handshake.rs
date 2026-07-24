//! Handshake codec — capability negotiation and method table exchange.
//!
//! ## Client → Server
//!
//! ```text
//! [1B version=10]
//! [1B prefix_len][prefix UTF-8]
//! [2B seg_count LE]
//! [per-segment: [4B size LE][1B name_len][name UTF-8]]
//! [2B capability_flags LE]
//! ```
//!
//! ## Server → Client (ACK)
//!
//! Same prefix, plus:
//!
//! ```text
//! [1B server_id_len][server_id UTF-8]
//! [1B server_instance_id_len][server_instance_id UTF-8]
//! [2B route_count LE]
//! [per-route:
//!     [1B name_len][route_name UTF-8]
//!     [1B route_uid_len][route_uid UTF-8]
//!     [8B route_revision LE]
//!     [1B crm_ns_len][crm_ns UTF-8]
//!     [1B crm_name_len][crm_name UTF-8]
//!     [1B crm_ver_len][crm_ver UTF-8]
//!     [1B abi_hash_len][abi_hash UTF-8]
//!     [1B signature_hash_len][signature_hash UTF-8]
//!     [8B max_payload_size LE]
//!     [2B method_count LE]
//!     [per-method: [1B name_len][method_name UTF-8][2B method_idx LE]]
//! ]
//! ```

use crate::control::EncodeError;
use crate::frame::DecodeError;

/// Handshake version number.
pub const HANDSHAKE_VERSION: u8 = 10;

// ── Capability flags (2 bytes) ───────────────────────────────────────────

/// Supports v2 call/reply frames (control-plane routing).
pub const CAP_CALL_V2: u16 = 1 << 0;

/// Supports method indexing (2-byte index vs UTF-8 name).
pub const CAP_METHOD_IDX: u16 = 1 << 1;

/// Supports chunked transfer for large payloads.
pub const CAP_CHUNKED: u16 = 1 << 2;

// ── Safety limits ────────────────────────────────────────────────────────

pub const MAX_SEGMENTS: usize = 16;
pub const MAX_ROUTES: usize = 64;
pub const MAX_METHODS: usize = c2_contract::MAX_CONTRACT_METHODS;
const MAX_HANDSHAKE_NAME_BYTES: usize = c2_contract::MAX_WIRE_TEXT_BYTES;

// ── Data types ───────────────────────────────────────────────────────────

/// A single method in a route's method table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodEntry {
    pub name: String,
    pub index: u16,
}

/// Route info exchanged during handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub name: String,
    pub route_uid: String,
    pub route_revision: u64,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
    pub methods: Vec<MethodEntry>,
}

/// Server identity exchanged in server→client handshake ACKs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub server_id: String,
    pub server_instance_id: String,
}

/// Decoded handshake payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// Pool prefix for deterministic segment naming.
    pub prefix: String,
    /// SHM segments: `(name, size)`.
    pub segments: Vec<(String, u32)>,
    /// Capability flags.
    pub capability_flags: u16,
    /// Server identity (only present in server→client ACK).
    pub server_identity: Option<ServerIdentity>,
    /// Routes with method tables (only present in server→client ACK).
    pub routes: Vec<RouteInfo>,
}

// ── Encoding: Client → Server ────────────────────────────────────────────

/// Encode client→server handshake.
pub fn encode_client_handshake(
    segments: &[(String, u32)],
    capability_flags: u16,
    prefix: &str,
) -> Result<Vec<u8>, EncodeError> {
    validate_name_len("prefix", prefix)?;
    if segments.len() > MAX_SEGMENTS {
        return Err(EncodeError::FieldTooLong {
            field: "segment count",
            max: MAX_SEGMENTS,
            actual: segments.len(),
        });
    }
    let mut buf = Vec::with_capacity(64);
    buf.push(HANDSHAKE_VERSION);
    // Prefix
    let prefix_b = prefix.as_bytes();
    buf.push(prefix_b.len() as u8);
    buf.extend_from_slice(prefix_b);
    // Segments
    buf.extend_from_slice(&(segments.len() as u16).to_le_bytes());
    for (name, size) in segments {
        validate_name_len("segment name", name)?;
        buf.extend_from_slice(&size.to_le_bytes());
        let name_b = name.as_bytes();
        buf.push(name_b.len() as u8);
        buf.extend_from_slice(name_b);
    }
    buf.extend_from_slice(&capability_flags.to_le_bytes());
    Ok(buf)
}

// ── Encoding: Server → Client (ACK) ─────────────────────────────────────

/// Encode server→client handshake ACK.
pub fn encode_server_handshake(
    segments: &[(String, u32)],
    capability_flags: u16,
    routes: &[RouteInfo],
    prefix: &str,
    identity: &ServerIdentity,
) -> Result<Vec<u8>, EncodeError> {
    if routes.len() > MAX_ROUTES {
        return Err(EncodeError::FieldTooLong {
            field: "route count",
            max: MAX_ROUTES,
            actual: routes.len(),
        });
    }
    let mut buf = encode_client_handshake(segments, capability_flags, prefix)?;
    validate_name_len("server_id", &identity.server_id)?;
    validate_name_len("server_instance_id", &identity.server_instance_id)?;
    let server_id_b = identity.server_id.as_bytes();
    buf.push(server_id_b.len() as u8);
    buf.extend_from_slice(server_id_b);
    let instance_b = identity.server_instance_id.as_bytes();
    buf.push(instance_b.len() as u8);
    buf.extend_from_slice(instance_b);
    buf.extend_from_slice(&(routes.len() as u16).to_le_bytes());
    for route in routes {
        validate_name_len("route name", &route.name)?;
        validate_route_uid(&route.route_uid).map_err(|reason| EncodeError::InvalidText {
            field: "route_uid",
            reason,
        })?;
        validate_crm_tag(&route.crm_ns, &route.crm_name, &route.crm_ver).map_err(|reason| {
            EncodeError::InvalidText {
                field: "crm tag",
                reason,
            }
        })?;
        validate_contract_hash("abi_hash", &route.abi_hash)?;
        validate_contract_hash("signature_hash", &route.signature_hash)?;
        if route.max_payload_size == 0 {
            return Err(EncodeError::InvalidText {
                field: "max_payload_size",
                reason: "must be > 0".to_string(),
            });
        }
        if route.methods.len() > MAX_METHODS {
            return Err(EncodeError::FieldTooLong {
                field: "method count",
                max: MAX_METHODS,
                actual: route.methods.len(),
            });
        }
        let name_b = route.name.as_bytes();
        buf.push(name_b.len() as u8);
        buf.extend_from_slice(name_b);
        let route_uid_b = route.route_uid.as_bytes();
        buf.push(route_uid_b.len() as u8);
        buf.extend_from_slice(route_uid_b);
        buf.extend_from_slice(&route.route_revision.to_le_bytes());
        let crm_ns_b = route.crm_ns.as_bytes();
        buf.push(crm_ns_b.len() as u8);
        buf.extend_from_slice(crm_ns_b);
        let crm_name_b = route.crm_name.as_bytes();
        buf.push(crm_name_b.len() as u8);
        buf.extend_from_slice(crm_name_b);
        let crm_ver_b = route.crm_ver.as_bytes();
        buf.push(crm_ver_b.len() as u8);
        buf.extend_from_slice(crm_ver_b);
        let abi_hash_b = route.abi_hash.as_bytes();
        buf.push(abi_hash_b.len() as u8);
        buf.extend_from_slice(abi_hash_b);
        let signature_hash_b = route.signature_hash.as_bytes();
        buf.push(signature_hash_b.len() as u8);
        buf.extend_from_slice(signature_hash_b);
        buf.extend_from_slice(&route.max_payload_size.to_le_bytes());
        buf.extend_from_slice(&(route.methods.len() as u16).to_le_bytes());
        for m in &route.methods {
            validate_name_len("method name", &m.name)?;
            let m_b = m.name.as_bytes();
            buf.push(m_b.len() as u8);
            buf.extend_from_slice(m_b);
            buf.extend_from_slice(&m.index.to_le_bytes());
        }
    }
    Ok(buf)
}

fn validate_name_len(field: &'static str, value: &str) -> Result<(), EncodeError> {
    let actual = value.len();
    if actual > MAX_HANDSHAKE_NAME_BYTES {
        return Err(EncodeError::FieldTooLong {
            field,
            max: MAX_HANDSHAKE_NAME_BYTES,
            actual,
        });
    }
    Ok(())
}

fn validate_contract_hash(field: &'static str, value: &str) -> Result<(), EncodeError> {
    validate_contract_hash_text(field, value)
        .map_err(|reason| EncodeError::InvalidText { field, reason })
}

fn validate_contract_hash_text(_field: &'static str, value: &str) -> Result<(), String> {
    c2_contract::validate_contract_hash(_field, value).map_err(|err| err.to_string())
}

fn validate_crm_tag(crm_ns: &str, crm_name: &str, crm_ver: &str) -> Result<(), String> {
    c2_contract::validate_crm_tag(crm_ns, crm_name, crm_ver).map_err(|err| err.to_string())
}

fn validate_route_uid(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("must not be empty".to_string());
    }
    if value.len() > MAX_HANDSHAKE_NAME_BYTES {
        return Err(format!(
            "is too long: {} bytes > {}",
            value.len(),
            MAX_HANDSHAKE_NAME_BYTES
        ));
    }
    if value.bytes().any(|b| b <= 0x20 || b == b'/' || b == b'\\') {
        return Err("contains an invalid character".to_string());
    }
    Ok(())
}

// ── Decoding (both directions) ───────────────────────────────────────────

/// Decode handshake from either direction.
///
/// Client payloads have no route section (detected by exhausting bytes
/// after `capability_flags`).
pub fn decode_handshake(buf: &[u8]) -> Result<Handshake, DecodeError> {
    let len = buf.len();
    if len < 3 {
        return Err(DecodeError::BufferTooShort { need: 3, have: len });
    }
    let version = buf[0];
    if version != HANDSHAKE_VERSION {
        return Err(DecodeError::InvalidValue {
            field: "handshake version",
            value: version as u64,
        });
    }

    let mut off = 1usize;

    // Prefix
    check_remaining(buf, off, 1, "prefix length")?;
    let prefix_len = buf[off] as usize;
    off += 1;
    check_remaining(buf, off, prefix_len, "prefix")?;
    let prefix = read_str(buf, off, prefix_len)?;
    off += prefix_len;

    // Segments
    check_remaining(buf, off, 2, "segment count")?;
    let seg_count = read_u16(buf, off) as usize;
    off += 2;
    if seg_count > MAX_SEGMENTS {
        return Err(DecodeError::InvalidValue {
            field: "segment count",
            value: seg_count as u64,
        });
    }
    let mut segments = Vec::with_capacity(seg_count);
    for _ in 0..seg_count {
        check_remaining(buf, off, 5, "segment entry")?;
        let size = read_u32(buf, off);
        off += 4;
        let name_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, name_len, "segment name")?;
        let name = read_str(buf, off, name_len)?;
        off += name_len;
        segments.push((name, size));
    }

    // Capability flags
    check_remaining(buf, off, 2, "capability flags")?;
    let capability_flags = read_u16(buf, off);
    off += 2;

    // Client handshakes end after capability flags. Server ACKs append
    // identity and route table data.
    if off == len {
        return Ok(Handshake {
            prefix,
            segments,
            capability_flags,
            server_identity: None,
            routes: Vec::new(),
        });
    }

    check_remaining(buf, off, 1, "server_id length")?;
    let server_id_len = buf[off] as usize;
    off += 1;
    check_remaining(buf, off, server_id_len, "server_id")?;
    let server_id = read_str(buf, off, server_id_len)?;
    off += server_id_len;

    check_remaining(buf, off, 1, "server_instance_id length")?;
    let instance_len = buf[off] as usize;
    off += 1;
    check_remaining(buf, off, instance_len, "server_instance_id")?;
    let server_instance_id = read_str(buf, off, instance_len)?;
    off += instance_len;

    // Routes (server→client ACK)
    let mut routes = Vec::new();
    check_remaining(buf, off, 2, "route count")?;
    let route_count = read_u16(buf, off) as usize;
    off += 2;
    if route_count > MAX_ROUTES {
        return Err(DecodeError::InvalidValue {
            field: "route count",
            value: route_count as u64,
        });
    }
    routes.reserve(route_count);
    for _ in 0..route_count {
        check_remaining(buf, off, 1, "route name_len")?;
        let r_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, r_len, "route name")?;
        let r_name = read_str(buf, off, r_len)?;
        off += r_len;

        check_remaining(buf, off, 1, "route_uid length")?;
        let route_uid_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, route_uid_len, "route_uid")?;
        let route_uid = read_str(buf, off, route_uid_len)?;
        off += route_uid_len;
        validate_route_uid(&route_uid).map_err(|reason| DecodeError::InvalidText {
            field: "route_uid",
            reason,
        })?;

        check_remaining(buf, off, 8, "route_revision")?;
        let route_revision = read_u64(buf, off);
        off += 8;

        check_remaining(buf, off, 1, "crm namespace length")?;
        let crm_ns_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, crm_ns_len, "crm namespace")?;
        let crm_ns = read_str(buf, off, crm_ns_len)?;
        off += crm_ns_len;

        check_remaining(buf, off, 1, "crm name length")?;
        let crm_name_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, crm_name_len, "crm name")?;
        let crm_name = read_str(buf, off, crm_name_len)?;
        off += crm_name_len;

        check_remaining(buf, off, 1, "crm version length")?;
        let crm_ver_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, crm_ver_len, "crm version")?;
        let crm_ver = read_str(buf, off, crm_ver_len)?;
        off += crm_ver_len;
        validate_crm_tag(&crm_ns, &crm_name, &crm_ver).map_err(|reason| {
            DecodeError::InvalidText {
                field: "crm tag",
                reason,
            }
        })?;

        check_remaining(buf, off, 1, "abi_hash length")?;
        let abi_hash_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, abi_hash_len, "abi_hash")?;
        let abi_hash = read_str(buf, off, abi_hash_len)?;
        off += abi_hash_len;
        validate_contract_hash_text("abi_hash", &abi_hash).map_err(|reason| {
            DecodeError::InvalidText {
                field: "abi_hash",
                reason,
            }
        })?;

        check_remaining(buf, off, 1, "signature_hash length")?;
        let signature_hash_len = buf[off] as usize;
        off += 1;
        check_remaining(buf, off, signature_hash_len, "signature_hash")?;
        let signature_hash = read_str(buf, off, signature_hash_len)?;
        off += signature_hash_len;
        validate_contract_hash_text("signature_hash", &signature_hash).map_err(|reason| {
            DecodeError::InvalidText {
                field: "signature_hash",
                reason,
            }
        })?;

        check_remaining(buf, off, 8, "max_payload_size")?;
        let max_payload_size = read_u64(buf, off);
        off += 8;
        if max_payload_size == 0 {
            return Err(DecodeError::InvalidValue {
                field: "max_payload_size",
                value: max_payload_size,
            });
        }

        check_remaining(buf, off, 2, "method count")?;
        let m_count = read_u16(buf, off) as usize;
        off += 2;
        if m_count > MAX_METHODS {
            return Err(DecodeError::InvalidValue {
                field: "method count",
                value: m_count as u64,
            });
        }
        let mut methods = Vec::with_capacity(m_count);
        for _ in 0..m_count {
            check_remaining(buf, off, 1, "method name_len")?;
            let m_len = buf[off] as usize;
            off += 1;
            check_remaining(buf, off, m_len, "method name")?;
            let m_name = read_str(buf, off, m_len)?;
            off += m_len;
            check_remaining(buf, off, 2, "method index")?;
            let m_idx = read_u16(buf, off);
            off += 2;
            methods.push(MethodEntry {
                name: m_name,
                index: m_idx,
            });
        }
        routes.push(RouteInfo {
            name: r_name,
            route_uid,
            route_revision,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
            max_payload_size,
            methods,
        });
    }
    if off != len {
        return Err(DecodeError::InvalidValue {
            field: "trailing bytes",
            value: (len - off) as u64,
        });
    }

    Ok(Handshake {
        prefix,
        segments,
        capability_flags,
        server_identity: Some(ServerIdentity {
            server_id,
            server_instance_id,
        }),
        routes,
    })
}

// ── Internal helpers ─────────────────────────────────────────────────────

#[inline]
fn check_remaining(
    buf: &[u8],
    off: usize,
    need: usize,
    field: &'static str,
) -> Result<(), DecodeError> {
    if off + need > buf.len() {
        Err(DecodeError::Truncated {
            field,
            need,
            have: buf.len() - off,
        })
    } else {
        Ok(())
    }
}

#[inline]
fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

#[inline]
fn read_str(buf: &[u8], off: usize, len: usize) -> Result<String, DecodeError> {
    core::str::from_utf8(&buf[off..off + len])
        .map(|s| s.into())
        .map_err(|_| DecodeError::Utf8Error)
}
