//! IPC 16 metadata follows the permanent fixed version preamble.
//!
//! Client metadata is `[prefix_len:u8][prefix][SHM direction bits:u16 LE]`.
//! Server metadata appends identity and routes; routes carry full contract
//! identity, payload limit and method table, without a revision.

use crate::frame::DecodeError;
use crate::rpc::EncodeError;

/// RPC protocol version; metadata from earlier versions is not supported.
pub const HANDSHAKE_VERSION: u32 = 16;

/// Fixed preamble body length, independent of configurable variable-frame limits.
pub const VERSION_PREAMBLE_BODY_LEN: u32 = 21;

/// Permanent preamble kinds. Future RPC versions retain these values and layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VersionPreambleKind {
    Hello = 1,
    Accepted = 2,
    VersionMismatch = 3,
}

/// Encode the permanent 25-byte preamble with this implementation's version.
///
/// Its length (21), request id (0), flags (4/6), magic, kind and u32 version
/// stay independent of future business-frame and metadata formats.
pub fn encode_version_preamble(kind: VersionPreambleKind) -> [u8; 25] {
    let mut bytes = [0; 25];
    bytes[..4].copy_from_slice(&VERSION_PREAMBLE_BODY_LEN.to_le_bytes());
    bytes[12] = if kind == VersionPreambleKind::Hello {
        4
    } else {
        6
    };
    bytes[16..20].copy_from_slice(b"C2IP");
    bytes[20] = kind as u8;
    bytes[21..].copy_from_slice(&HANDSHAKE_VERSION.to_le_bytes());
    bytes
}

/// Validate the fixed envelope before reading its nine-byte payload.
pub fn validate_version_preamble_header(
    header: &[u8; 16],
    response: bool,
) -> Result<(), DecodeError> {
    for (field, value, expected) in [
        (
            "version preamble length",
            u64::from(u32::from_le_bytes(header[..4].try_into().unwrap())),
            u64::from(VERSION_PREAMBLE_BODY_LEN),
        ),
        (
            "version preamble request id",
            u64::from_le_bytes(header[4..12].try_into().unwrap()),
            0,
        ),
        (
            "version preamble flags",
            u64::from(u32::from_le_bytes(header[12..].try_into().unwrap())),
            if response { 6 } else { 4 },
        ),
    ] {
        if value != expected {
            return Err(DecodeError::InvalidValue { field, value });
        }
    }
    Ok(())
}

/// Decode a complete fixed payload. The caller validates the expected phase.
pub fn decode_version_preamble_payload(
    payload: &[u8; 9],
) -> Result<(VersionPreambleKind, u32), DecodeError> {
    if &payload[..4] != b"C2IP" {
        return Err(DecodeError::InvalidText {
            field: "version preamble magic",
            reason: "expected C2IP".into(),
        });
    }
    let kind = match payload[4] {
        1 => VersionPreambleKind::Hello,
        2 => VersionPreambleKind::Accepted,
        3 => VersionPreambleKind::VersionMismatch,
        value => {
            return Err(DecodeError::InvalidValue {
                field: "version preamble kind",
                value: u64::from(value),
            });
        }
    };
    let version = u32::from_le_bytes(payload[5..].try_into().unwrap());
    if version == 0 {
        return Err(DecodeError::InvalidValue {
            field: "protocol version",
            value: 0,
        });
    }
    Ok((kind, version))
}

// ── Capability flags (2 bytes) ───────────────────────────────────────────

/// Client may carry request bodies in its own SHM blocks (client-relative
/// direction: client writes, server reads and sends release records).
///
/// The client offers the bit only when its native local stream can actually
/// reach shared memory with the peer (same TokenSessionId on Windows); the
/// server ACKs it only when the client offered it and the server's own
/// native reachability for this connection agrees. Without the ACK bit the
/// client never writes buddy request frames, and the server rejects any
/// inbound buddy request frame before touching a mapping.
pub const CAP_SHM_REQUEST: u16 = 1 << 3;

/// Server may carry response bodies in its own SHM blocks (client-relative
/// direction: server writes, client reads and releases).
///
/// Offered by the client only when its native local stream can reach shared
/// memory with the peer, and ACKed only through the same intersection. When
/// the bit is absent the server never publishes response coordinates, and
/// the client rejects any inbound buddy reply frame before admitting a
/// lease or lazy-opening a mapping.
pub const CAP_SHM_RESPONSE: u16 = 1 << 4;

// ── Safety limits ────────────────────────────────────────────────────────

pub const MAX_ROUTES: usize = 64;
pub const MAX_METHODS: usize = c2_contract::MAX_CONTRACT_METHODS;
// ── Data types ───────────────────────────────────────────────────────────

/// A single method in a route's method table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodEntry<S = String> {
    pub name: S,
    pub index: u16,
}

/// Route info exchanged during handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo<S = String> {
    pub name: S,
    pub crm_ns: S,
    pub crm_name: S,
    pub crm_ver: S,
    pub abi_hash: S,
    pub signature_hash: S,
    pub max_payload_size: u64,
    pub methods: Vec<MethodEntry<S>>,
}

/// Server identity exchanged in server→client handshake ACKs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity<S = String> {
    pub server_id: S,
    pub server_instance_id: S,
}

/// Decoded handshake payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// Pool prefix for deterministic segment naming.
    pub prefix: String,
    /// Capability flags.
    pub capability_flags: u16,
    /// Server identity (only present in server→client ACK).
    pub server_identity: Option<ServerIdentity>,
    /// Routes with method tables (only present in server→client ACK).
    pub routes: Vec<RouteInfo>,
}

/// Encode already validated client metadata after version acceptance.
/// Only wire-width failures remain; contract validation belongs to admission.
pub fn encode_client_handshake(
    capability_flags: u16,
    prefix: &str,
) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::with_capacity(3 + prefix.len());
    push_text(&mut buf, "prefix", prefix)?;
    buf.extend_from_slice(&capability_flags.to_le_bytes());
    Ok(buf)
}

/// Borrow route/method/identity text from the authoritative catalog instead of
/// cloning it into an intermediate directory for encoding.
pub fn encode_server_handshake<S: AsRef<str>, I: AsRef<str>>(
    capability_flags: u16,
    routes: &[RouteInfo<S>],
    prefix: &str,
    identity: &ServerIdentity<I>,
) -> Result<Vec<u8>, EncodeError> {
    let mut buf = encode_client_handshake(capability_flags, prefix)?;
    push_text(&mut buf, "server_id", identity.server_id.as_ref())?;
    push_text(
        &mut buf,
        "server_instance_id",
        identity.server_instance_id.as_ref(),
    )?;
    push_count(&mut buf, "route count", routes.len())?;
    for route in routes {
        for (field, text) in [
            ("route name", route.name.as_ref()),
            ("crm_ns", route.crm_ns.as_ref()),
            ("crm_name", route.crm_name.as_ref()),
            ("crm_ver", route.crm_ver.as_ref()),
            ("abi_hash", route.abi_hash.as_ref()),
            ("signature_hash", route.signature_hash.as_ref()),
        ] {
            push_text(&mut buf, field, text)?;
        }
        buf.extend_from_slice(&route.max_payload_size.to_le_bytes());
        push_count(&mut buf, "method count", route.methods.len())?;
        for method in &route.methods {
            push_text(&mut buf, "method name", method.name.as_ref())?;
            buf.extend_from_slice(&method.index.to_le_bytes());
        }
    }
    Ok(buf)
}

fn push_text(buf: &mut Vec<u8>, field: &'static str, value: &str) -> Result<(), EncodeError> {
    let len = u8::try_from(value.len()).map_err(|_| EncodeError::FieldTooLong {
        field,
        max: u8::MAX as usize,
        actual: value.len(),
    })?;
    buf.push(len);
    buf.extend_from_slice(value.as_bytes());
    Ok(())
}

fn push_count(buf: &mut Vec<u8>, field: &'static str, count: usize) -> Result<(), EncodeError> {
    let count = u16::try_from(count).map_err(|_| EncodeError::FieldTooLong {
        field,
        max: u16::MAX as usize,
        actual: count,
    })?;
    buf.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

fn validate_contract_hash_text(_field: &'static str, value: &str) -> Result<(), String> {
    c2_contract::validate_contract_hash(_field, value).map_err(|err| err.to_string())
}

fn validate_crm_tag(crm_ns: &str, crm_name: &str, crm_ver: &str) -> Result<(), String> {
    c2_contract::validate_crm_tag(crm_ns, crm_name, crm_ver).map_err(|err| err.to_string())
}

// ── Decoding (both directions) ───────────────────────────────────────────

/// Decode metadata from either direction after the fixed version exchange.
///
/// The expected direction is supplied by the receive boundary. Client metadata
/// cannot masquerade as a server ACK by appending superseded fields.
pub fn decode_handshake(buf: &[u8], response: bool) -> Result<Handshake, DecodeError> {
    let len = buf.len();
    if len < 3 {
        return Err(DecodeError::BufferTooShort { need: 3, have: len });
    }
    let mut off = 0usize;

    // Prefix
    check_remaining(buf, off, 1, "prefix length")?;
    let prefix_len = buf[off] as usize;
    off += 1;
    check_remaining(buf, off, prefix_len, "prefix")?;
    let prefix = read_str(buf, off, prefix_len)?;
    off += prefix_len;

    // Capability flags
    check_remaining(buf, off, 2, "capability flags")?;
    let capability_flags = read_u16(buf, off);
    off += 2;

    if capability_flags & !(CAP_SHM_REQUEST | CAP_SHM_RESPONSE) != 0 {
        return Err(DecodeError::InvalidValue {
            field: "capability flags",
            value: capability_flags.into(),
        });
    }
    if !response {
        if off != len {
            return Err(DecodeError::InvalidValue {
                field: "trailing bytes",
                value: (len - off) as u64,
            });
        }
        return Ok(Handshake {
            prefix,
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

    for (field, value) in [
        ("server_id", &server_id),
        ("server_instance_id", &server_instance_id),
    ] {
        c2_config::validate_server_id(value)
            .map_err(|reason| DecodeError::InvalidText { field, reason })?;
    }

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
        c2_contract::validate_call_route_key("route_name", &r_name).map_err(|err| {
            DecodeError::InvalidText {
                field: "route_name",
                reason: err.to_string(),
            }
        })?;

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
