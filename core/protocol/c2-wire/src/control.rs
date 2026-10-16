//! V2 call/reply control-plane codec.
//!
//! In wire v2, routing metadata travels in the UDS inline frame (small),
//! while the SHM buddy block contains only pure serialized payload.
//!
//! ## V2 Call Control
//!
//! ```text
//! [1B route_name_len][route_name UTF-8]
//! [1B route_uid_len][route_uid UTF-8]
//! [8B observed_route_revision LE]
//! [1B crm_ns_len][crm_ns UTF-8]
//! [1B crm_name_len][crm_name UTF-8]
//! [1B crm_ver_len][crm_ver UTF-8]
//! [1B abi_hash_len][abi_hash UTF-8]
//! [1B signature_hash_len][signature_hash UTF-8]
//! [2B method_idx LE]
//! ```
//!
//! Empty route identity fields are invalid. CRM calls must carry a concrete
//! route token and expected contract; route name alone is not authoritative.
//!
//! ## V2 Reply Control
//!
//! ```text
//! [1B status]
//! if status == STATUS_ERROR (0x01):
//!     [4B error_len LE][error_bytes]
//! if status == STATUS_ROUTE_NOT_FOUND (0x02):
//!     [4B route_len LE][route_name UTF-8]
//! ```

use crate::frame::DecodeError;

/// Wire encoding failed because the requested control frame cannot be
/// represented by the protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    FieldTooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    InvalidText {
        field: &'static str,
        reason: String,
    },
    BufferTooShort {
        field: &'static str,
        need: usize,
        have: usize,
    },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FieldTooLong { field, max, actual } => {
                write!(f, "{field} is too long: {actual} bytes > {max}")
            }
            Self::InvalidText { field, reason } => {
                write!(f, "{field} is invalid: {reason}")
            }
            Self::BufferTooShort { field, need, have } => {
                write!(f, "{field} buffer is too short: need {need}, have {have}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Reply status: success — result data follows (inline or in buddy SHM).
pub const STATUS_SUCCESS: u8 = 0x00;

/// Reply status: error — error data follows inline.
pub const STATUS_ERROR: u8 = 0x01;

/// Reply status: requested route is no longer present on the IPC server.
pub const STATUS_ROUTE_NOT_FOUND: u8 = 0x02;

const MAX_CALL_TEXT_BYTES: usize = c2_contract::MAX_WIRE_TEXT_BYTES;

// ── V2 Call Control ──────────────────────────────────────────────────────

/// Route identity observed by a client when it acquired a route.
///
/// This token travels on every route-bound call so the server can reject stale
/// clients before invoking resource code. `route_name` remains a human chosen
/// routing key; `route_uid` is the authoritative identity of one committed
/// route registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCallIdentity {
    pub route_name: String,
    pub route_uid: String,
    pub observed_route_revision: u64,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
}

/// Decoded v2 call control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallControl {
    /// Route and contract identity observed by the caller.
    pub identity: RouteCallIdentity,
    /// Method index within the route's method table.
    pub method_idx: u16,
}

/// Return the encoded v2 call-control size for `identity`.
pub fn encoded_call_control_len(identity: &RouteCallIdentity) -> Result<usize, EncodeError> {
    validate_route_call_identity(identity)?;
    // Seven text-length bytes, one u64 revision and one u16 method index.
    Ok(7 + 8
        + 2
        + identity.route_name.len()
        + identity.route_uid.len()
        + identity.crm_ns.len()
        + identity.crm_name.len()
        + identity.crm_ver.len()
        + identity.abi_hash.len()
        + identity.signature_hash.len())
}

/// Encode v2 call control.
pub fn encode_call_control(
    identity: &RouteCallIdentity,
    method_idx: u16,
) -> Result<Vec<u8>, EncodeError> {
    let len = encoded_call_control_len(identity)?;
    let mut buf = Vec::with_capacity(len);
    push_text(&mut buf, &identity.route_name);
    push_text(&mut buf, &identity.route_uid);
    buf.extend_from_slice(&identity.observed_route_revision.to_le_bytes());
    push_text(&mut buf, &identity.crm_ns);
    push_text(&mut buf, &identity.crm_name);
    push_text(&mut buf, &identity.crm_ver);
    push_text(&mut buf, &identity.abi_hash);
    push_text(&mut buf, &identity.signature_hash);
    buf.extend_from_slice(&method_idx.to_le_bytes());
    Ok(buf)
}

/// Encode v2 call control directly into a buffer at `offset`.
///
/// Returns the number of bytes written.
pub fn encode_call_control_into(
    buf: &mut [u8],
    offset: usize,
    identity: &RouteCallIdentity,
    method_idx: u16,
) -> Result<usize, EncodeError> {
    let len = encoded_call_control_len(identity)?;
    check_write_capacity(buf, offset, len, "call_control")?;
    let mut cursor = offset;
    cursor = write_text(buf, cursor, &identity.route_name);
    cursor = write_text(buf, cursor, &identity.route_uid);
    buf[cursor..cursor + 8].copy_from_slice(&identity.observed_route_revision.to_le_bytes());
    cursor += 8;
    cursor = write_text(buf, cursor, &identity.crm_ns);
    cursor = write_text(buf, cursor, &identity.crm_name);
    cursor = write_text(buf, cursor, &identity.crm_ver);
    cursor = write_text(buf, cursor, &identity.abi_hash);
    cursor = write_text(buf, cursor, &identity.signature_hash);
    buf[cursor..cursor + 2].copy_from_slice(&method_idx.to_le_bytes());
    Ok(len)
}

/// Decode v2 call control from `buf[offset..]`.
///
/// Returns `(control, bytes_consumed)`.
pub fn decode_call_control(buf: &[u8], offset: usize) -> Result<(CallControl, usize), DecodeError> {
    let remaining = buf.len().saturating_sub(offset);
    if remaining < 1 + 1 + 8 + 1 + 1 + 1 + 1 + 1 + 2 {
        return Err(DecodeError::BufferTooShort {
            need: 17,
            have: remaining,
        });
    }
    let mut cursor = offset;
    let route_name = read_text(buf, &mut cursor, "route_name")?;
    c2_contract::validate_call_route_key("route_name", &route_name).map_err(|err| {
        DecodeError::InvalidText {
            field: "route_name",
            reason: err.to_string(),
        }
    })?;
    let route_uid = read_text(buf, &mut cursor, "route_uid")?;
    validate_route_uid(&route_uid).map_err(|reason| DecodeError::InvalidText {
        field: "route_uid",
        reason,
    })?;
    check_remaining(buf, cursor, 8, "observed_route_revision")?;
    let observed_route_revision = u64::from_le_bytes([
        buf[cursor],
        buf[cursor + 1],
        buf[cursor + 2],
        buf[cursor + 3],
        buf[cursor + 4],
        buf[cursor + 5],
        buf[cursor + 6],
        buf[cursor + 7],
    ]);
    cursor += 8;
    let crm_ns = read_text(buf, &mut cursor, "crm_ns")?;
    let crm_name = read_text(buf, &mut cursor, "crm_name")?;
    let crm_ver = read_text(buf, &mut cursor, "crm_ver")?;
    c2_contract::validate_crm_tag(&crm_ns, &crm_name, &crm_ver).map_err(|err| {
        DecodeError::InvalidText {
            field: "crm tag",
            reason: err.to_string(),
        }
    })?;
    let abi_hash = read_text(buf, &mut cursor, "abi_hash")?;
    c2_contract::validate_contract_hash("abi_hash", &abi_hash).map_err(|err| {
        DecodeError::InvalidText {
            field: "abi_hash",
            reason: err.to_string(),
        }
    })?;
    let signature_hash = read_text(buf, &mut cursor, "signature_hash")?;
    c2_contract::validate_contract_hash("signature_hash", &signature_hash).map_err(|err| {
        DecodeError::InvalidText {
            field: "signature_hash",
            reason: err.to_string(),
        }
    })?;
    check_remaining(buf, cursor, 2, "method_idx")?;
    let method_idx = u16::from_le_bytes([buf[cursor], buf[cursor + 1]]);
    cursor += 2;
    Ok((
        CallControl {
            identity: RouteCallIdentity {
                route_name,
                route_uid,
                observed_route_revision,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
            },
            method_idx,
        },
        cursor - offset,
    ))
}

fn validate_call_route_key(field: &'static str, value: &str) -> Result<(), EncodeError> {
    c2_contract::validate_call_route_key(field, value).map_err(|err| EncodeError::InvalidText {
        field,
        reason: err.to_string(),
    })
}

fn validate_route_uid(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("must not be empty".to_string());
    }
    if value.len() > MAX_CALL_TEXT_BYTES {
        return Err(format!(
            "is too long: {} bytes > {}",
            value.len(),
            MAX_CALL_TEXT_BYTES
        ));
    }
    if value.bytes().any(|b| b <= 0x20 || b == b'/' || b == b'\\') {
        return Err("contains an invalid character".to_string());
    }
    Ok(())
}

fn validate_route_call_identity(identity: &RouteCallIdentity) -> Result<(), EncodeError> {
    validate_call_route_key("route_name", &identity.route_name)?;
    validate_route_uid(&identity.route_uid).map_err(|reason| EncodeError::InvalidText {
        field: "route_uid",
        reason,
    })?;
    c2_contract::validate_crm_tag(&identity.crm_ns, &identity.crm_name, &identity.crm_ver)
        .map_err(|err| EncodeError::InvalidText {
            field: "crm tag",
            reason: err.to_string(),
        })?;
    c2_contract::validate_contract_hash("abi_hash", &identity.abi_hash).map_err(|err| {
        EncodeError::InvalidText {
            field: "abi_hash",
            reason: err.to_string(),
        }
    })?;
    c2_contract::validate_contract_hash("signature_hash", &identity.signature_hash).map_err(
        |err| EncodeError::InvalidText {
            field: "signature_hash",
            reason: err.to_string(),
        },
    )?;
    Ok(())
}

// Callers validate every text field before writing.
fn push_text(buf: &mut Vec<u8>, value: &str) {
    buf.push(value.len() as u8);
    buf.extend_from_slice(value.as_bytes());
}

// The caller validates every field and the complete output range first.
fn write_text(buf: &mut [u8], offset: usize, value: &str) -> usize {
    buf[offset] = value.len() as u8;
    let start = offset + 1;
    let end = start + value.len();
    buf[start..end].copy_from_slice(value.as_bytes());
    end
}

fn check_write_capacity(
    buf: &[u8],
    offset: usize,
    need: usize,
    field: &'static str,
) -> Result<(), EncodeError> {
    let have = buf.len().saturating_sub(offset);
    if have < need {
        return Err(EncodeError::BufferTooShort { field, need, have });
    }
    Ok(())
}

fn check_remaining(
    buf: &[u8],
    offset: usize,
    need: usize,
    field: &'static str,
) -> Result<(), DecodeError> {
    let remaining = buf.len().saturating_sub(offset);
    if remaining < need {
        Err(DecodeError::Truncated {
            field,
            need,
            have: remaining,
        })
    } else {
        Ok(())
    }
}

fn read_text(buf: &[u8], offset: &mut usize, field: &'static str) -> Result<String, DecodeError> {
    check_remaining(buf, *offset, 1, field)?;
    let len = buf[*offset] as usize;
    *offset += 1;
    check_remaining(buf, *offset, len, field)?;
    let value = core::str::from_utf8(&buf[*offset..*offset + len])
        .map_err(|_| DecodeError::Utf8Error)?
        .to_string();
    *offset += len;
    if value.is_empty() {
        return Err(DecodeError::InvalidText {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    Ok(value)
}

// ── V2 Reply Control ─────────────────────────────────────────────────────

/// Decoded v2 reply control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyControl {
    /// Success — result data follows (inline or buddy SHM).
    Success,
    /// System error — route no longer exists on the upstream IPC server.
    RouteNotFound(String),
    /// Error — error bytes follow inline.
    Error(Vec<u8>),
}

/// Encode v2 reply control.
pub fn try_encode_reply_control(ctrl: &ReplyControl) -> Result<Vec<u8>, EncodeError> {
    match ctrl {
        ReplyControl::Success => Ok(vec![STATUS_SUCCESS]),
        ReplyControl::RouteNotFound(route_name) => {
            validate_call_route_key("route_name", route_name)?;
            let route_bytes = route_name.as_bytes();
            let mut buf = Vec::with_capacity(1 + 4 + route_bytes.len());
            buf.push(STATUS_ROUTE_NOT_FOUND);
            buf.extend_from_slice(&(route_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(route_bytes);
            Ok(buf)
        }
        ReplyControl::Error(err_data) => {
            let mut buf = Vec::with_capacity(1 + 4 + err_data.len());
            buf.push(STATUS_ERROR);
            buf.extend_from_slice(&(err_data.len() as u32).to_le_bytes());
            buf.extend_from_slice(err_data);
            Ok(buf)
        }
    }
}

/// Decode v2 reply control from `buf[offset..]`.
///
/// Returns `(control, bytes_consumed)`.
pub fn decode_reply_control(
    buf: &[u8],
    offset: usize,
) -> Result<(ReplyControl, usize), DecodeError> {
    let remaining = buf.len().saturating_sub(offset);
    if remaining < 1 {
        return Err(DecodeError::BufferTooShort { need: 1, have: 0 });
    }
    let status = buf[offset];
    match status {
        STATUS_SUCCESS => Ok((ReplyControl::Success, 1)),
        STATUS_ERROR => {
            if remaining < 5 {
                return Err(DecodeError::Truncated {
                    field: "reply control error_len",
                    need: 5,
                    have: remaining,
                });
            }
            let err_len = u32::from_le_bytes([
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
                buf[offset + 4],
            ]) as usize;
            let total = 5 + err_len;
            if remaining < total {
                return Err(DecodeError::Truncated {
                    field: "reply control error_data",
                    need: total,
                    have: remaining,
                });
            }
            let err_data = buf[offset + 5..offset + 5 + err_len].to_vec();
            Ok((ReplyControl::Error(err_data), total))
        }
        STATUS_ROUTE_NOT_FOUND => {
            if remaining < 5 {
                return Err(DecodeError::Truncated {
                    field: "reply control route_len",
                    need: 5,
                    have: remaining,
                });
            }
            let route_len = u32::from_le_bytes([
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
                buf[offset + 4],
            ]) as usize;
            let total = 5 + route_len;
            if remaining < total {
                return Err(DecodeError::Truncated {
                    field: "reply control route_name",
                    need: total,
                    have: remaining,
                });
            }
            let route_name = core::str::from_utf8(&buf[offset + 5..offset + 5 + route_len])
                .map_err(|_| DecodeError::Utf8Error)?
                .to_string();
            c2_contract::validate_call_route_key("route_name", &route_name).map_err(|err| {
                DecodeError::InvalidText {
                    field: "route_name",
                    reason: err.to_string(),
                }
            })?;
            Ok((ReplyControl::RouteNotFound(route_name), total))
        }
        _ => Err(DecodeError::InvalidValue {
            field: "reply status",
            value: status as u64,
        }),
    }
}
