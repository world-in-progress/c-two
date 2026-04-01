//! Handshake codec — capability negotiation and method table exchange.
//!
//! ## Client → Server
//!
//! ```text
//! [1B version=6]
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
//! [2B route_count LE]
//! [per-route:
//!     [1B name_len][route_name UTF-8]
//!     [2B method_count LE]
//!     [per-method: [1B name_len][method_name UTF-8][2B method_idx LE]]
//! ]
//! ```

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};

use crate::frame::DecodeError;

/// Handshake version number.
pub const HANDSHAKE_VERSION: u8 = 6;

// ── Capability flags (2 bytes) ───────────────────────────────────────────

/// Supports v2 call/reply frames (control-plane routing).
pub const CAP_CALL_V2: u16 = 1 << 0;

/// Supports method indexing (2-byte index vs UTF-8 name).
pub const CAP_METHOD_IDX: u16 = 1 << 1;

/// Supports chunked transfer for large payloads.
pub const CAP_CHUNKED: u16 = 1 << 2;

// ── Safety limits ────────────────────────────────────────────────────────

const MAX_SEGMENTS: usize = 16;
const MAX_ROUTES: usize = 64;
const MAX_METHODS: usize = 256;

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
    pub methods: Vec<MethodEntry>,
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
    /// Routes with method tables (only present in server→client ACK).
    pub routes: Vec<RouteInfo>,
}

// ── Encoding: Client → Server ────────────────────────────────────────────

/// Encode client→server handshake.
pub fn encode_client_handshake(
    segments: &[(String, u32)],
    capability_flags: u16,
    prefix: &str,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.push(HANDSHAKE_VERSION);
    // Prefix
    let prefix_b = prefix.as_bytes();
    buf.push(prefix_b.len() as u8);
    buf.extend_from_slice(prefix_b);
    // Segments
    buf.extend_from_slice(&(segments.len() as u16).to_le_bytes());
    for (name, size) in segments {
        buf.extend_from_slice(&size.to_le_bytes());
        let name_b = name.as_bytes();
        buf.push(name_b.len() as u8);
        buf.extend_from_slice(name_b);
    }
    buf.extend_from_slice(&capability_flags.to_le_bytes());
    buf
}

// ── Encoding: Server → Client (ACK) ─────────────────────────────────────

/// Encode server→client handshake ACK.
pub fn encode_server_handshake(
    segments: &[(String, u32)],
    capability_flags: u16,
    routes: &[RouteInfo],
    prefix: &str,
) -> Vec<u8> {
    let mut buf = encode_client_handshake(segments, capability_flags, prefix);
    buf.extend_from_slice(&(routes.len() as u16).to_le_bytes());
    for route in routes {
        let name_b = route.name.as_bytes();
        buf.push(name_b.len() as u8);
        buf.extend_from_slice(name_b);
        buf.extend_from_slice(&(route.methods.len() as u16).to_le_bytes());
        for m in &route.methods {
            let m_b = m.name.as_bytes();
            buf.push(m_b.len() as u8);
            buf.extend_from_slice(m_b);
            buf.extend_from_slice(&m.index.to_le_bytes());
        }
    }
    buf
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
    let prefix_len = buf[off] as usize; off += 1;
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
        let size = read_u32(buf, off); off += 4;
        let name_len = buf[off] as usize; off += 1;
        check_remaining(buf, off, name_len, "segment name")?;
        let name = read_str(buf, off, name_len)?;
        off += name_len;
        segments.push((name, size));
    }

    // Capability flags
    check_remaining(buf, off, 2, "capability flags")?;
    let capability_flags = read_u16(buf, off);
    off += 2;

    // Routes (optional — only in server→client ACK)
    let mut routes = Vec::new();
    if off + 2 <= len {
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
            let r_len = buf[off] as usize; off += 1;
            check_remaining(buf, off, r_len, "route name")?;
            let r_name = read_str(buf, off, r_len)?;
            off += r_len;

            check_remaining(buf, off, 2, "method count")?;
            let m_count = read_u16(buf, off) as usize; off += 2;
            if m_count > MAX_METHODS {
                return Err(DecodeError::InvalidValue {
                    field: "method count",
                    value: m_count as u64,
                });
            }
            let mut methods = Vec::with_capacity(m_count);
            for _ in 0..m_count {
                check_remaining(buf, off, 1, "method name_len")?;
                let m_len = buf[off] as usize; off += 1;
                check_remaining(buf, off, m_len, "method name")?;
                let m_name = read_str(buf, off, m_len)?;
                off += m_len;
                check_remaining(buf, off, 2, "method index")?;
                let m_idx = read_u16(buf, off); off += 2;
                methods.push(MethodEntry { name: m_name, index: m_idx });
            }
            routes.push(RouteInfo { name: r_name, methods });
        }
    }

    Ok(Handshake { prefix, segments, capability_flags, routes })
}

// ── Internal helpers ─────────────────────────────────────────────────────

#[inline]
fn check_remaining(buf: &[u8], off: usize, need: usize, field: &'static str) -> Result<(), DecodeError> {
    if off + need > buf.len() {
        Err(DecodeError::Truncated { field, need, have: buf.len() - off })
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
fn read_str(buf: &[u8], off: usize, len: usize) -> Result<String, DecodeError> {
    core::str::from_utf8(&buf[off..off + len])
        .map(|s| s.into())
        .map_err(|_| DecodeError::Utf8Error)
}
