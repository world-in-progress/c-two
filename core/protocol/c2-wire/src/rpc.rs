//! IPC 16 RPC record codec. Connection admission owns call state and cumulative offsets.

use c2_error::C2Error;

use crate::buddy::{
    BUDDY_PAYLOAD_SIZE, BuddyPayload, decode_buddy_payload, encode_buddy_payload_into,
};
use crate::flags::{FLAG_BUDDY, FLAG_CHUNKED, FLAG_FIRST_CHUNK, FLAG_RESPONSE, FLAG_RPC_RECORD};
use crate::frame::{DecodeError, FrameHeader, HEADER_SIZE};
use crate::release::{ReleaseBatch, decode_release_frame};

const MAX_RPC_RECORD_BYTES: usize = u32::MAX as usize - (crate::frame::HEADER_SIZE - 4);

/// Maximum inline CALL_END payload after its kind byte, including status,
/// complete-item count when present, and error bytes. HTTP terminal records
/// use the same payload units. The u32 frame length includes RID and flags,
/// but excludes its own four-byte prefix.
pub const MAX_CALL_END_PAYLOAD_BYTES: u64 = (MAX_RPC_RECORD_BYTES - 1) as u64;

/// Wire encoding failed because a field cannot be represented by IPC 16.
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
            Self::InvalidText { field, reason } => write!(f, "{field} is invalid: {reason}"),
            Self::BufferTooShort { field, need, have } => {
                write!(f, "{field} buffer is too short: need {need}, have {have}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Six-field route and contract identity carried by OPEN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCallIdentity<S = String> {
    pub route_name: S,
    pub crm_ns: S,
    pub crm_name: S,
    pub crm_ver: S,
    pub abi_hash: S,
    pub signature_hash: S,
}

const IDENTITY_FIELDS: [&str; 6] = [
    "route_name",
    "crm_ns",
    "crm_name",
    "crm_ver",
    "abi_hash",
    "signature_hash",
];

fn identity_fields<S: AsRef<str>>(identity: &RouteCallIdentity<S>) -> [&str; 6] {
    [
        identity.route_name.as_ref(),
        identity.crm_ns.as_ref(),
        identity.crm_name.as_ref(),
        identity.crm_ver.as_ref(),
        identity.abi_hash.as_ref(),
        identity.signature_hash.as_ref(),
    ]
}

fn identity_encoded_len(fields: &[&str; 6]) -> Result<usize, EncodeError> {
    for (field, value) in IDENTITY_FIELDS.into_iter().zip(fields) {
        if value.len() > u8::MAX as usize {
            return Err(EncodeError::FieldTooLong {
                field,
                max: u8::MAX as usize,
                actual: value.len(),
            });
        }
    }
    Ok(6 + fields.iter().map(|s| s.len()).sum::<usize>())
}

/// Size of OPEN's kind, identity, method index and mode, without allocating.
pub fn encoded_open_metadata_len<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
) -> Result<usize, EncodeError> {
    Ok(4 + identity_encoded_len(&identity_fields(identity))?)
}

fn read_identity_text(
    input: &[u8],
    pos: &mut usize,
    field: &'static str,
) -> Result<String, DecodeError> {
    let len = *input.get(*pos).ok_or(DecodeError::Truncated {
        field,
        need: 1,
        have: 0,
    })? as usize;
    *pos += 1;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| invalid(field, len as u64))?;
    if end > input.len() {
        return Err(DecodeError::Truncated {
            field,
            need: len,
            have: input.len() - *pos,
        });
    }
    let value = std::str::from_utf8(&input[*pos..end])
        .map_err(|_| DecodeError::Utf8Error)?
        .to_string();
    *pos = end;
    if value.is_empty() {
        return Err(DecodeError::InvalidText {
            field,
            reason: "must not be empty".into(),
        });
    }
    Ok(value)
}

/// Decode and validate the OPEN kind, six identity fields, method index and call mode.
/// Returns the byte offset where the value storage begins.
pub fn decode_open_metadata(
    payload: &[u8],
) -> Result<(RouteCallIdentity, u16, CallMode, usize), DecodeError> {
    if payload.first().copied() != Some(RpcRecordKind::Open as u8) {
        return Err(invalid(
            "OPEN kind",
            payload.first().copied().unwrap_or(0).into(),
        ));
    }
    let mut pos = 1;
    let route_name = read_identity_text(payload, &mut pos, "route_name")?;
    c2_contract::validate_call_route_key("route_name", &route_name).map_err(|error| {
        DecodeError::InvalidText {
            field: "route_name",
            reason: error.to_string(),
        }
    })?;
    let crm_ns = read_identity_text(payload, &mut pos, "crm_ns")?;
    let crm_name = read_identity_text(payload, &mut pos, "crm_name")?;
    let crm_ver = read_identity_text(payload, &mut pos, "crm_ver")?;
    c2_contract::validate_crm_tag(&crm_ns, &crm_name, &crm_ver).map_err(|error| {
        DecodeError::InvalidText {
            field: "crm tag",
            reason: error.to_string(),
        }
    })?;
    let abi_hash = read_identity_text(payload, &mut pos, "abi_hash")?;
    c2_contract::validate_contract_hash("abi_hash", &abi_hash).map_err(|error| {
        DecodeError::InvalidText {
            field: "abi_hash",
            reason: error.to_string(),
        }
    })?;
    let signature_hash = read_identity_text(payload, &mut pos, "signature_hash")?;
    c2_contract::validate_contract_hash("signature_hash", &signature_hash).map_err(|error| {
        DecodeError::InvalidText {
            field: "signature_hash",
            reason: error.to_string(),
        }
    })?;
    if payload.len() - pos < 2 {
        return Err(DecodeError::Truncated {
            field: "method_idx",
            need: 2,
            have: payload.len() - pos,
        });
    }
    let method_idx = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
    pos += 2;
    let mode = CallMode::try_from(*payload.get(pos).ok_or(DecodeError::Truncated {
        field: "call mode",
        need: 1,
        have: 0,
    })?)?;
    pos += 1;
    Ok((
        RouteCallIdentity {
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
        },
        method_idx,
        mode,
        pos,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcRecordKind {
    Open = 1,
    ItemBegin = 2,
    ItemData = 3,
    Credit = 4,
    Cancel = 5,
    CallEnd = 6,
    Release = 7,
}

impl TryFrom<u8> for RpcRecordKind {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Open),
            2 => Ok(Self::ItemBegin),
            3 => Ok(Self::ItemData),
            4 => Ok(Self::Credit),
            5 => Ok(Self::Cancel),
            6 => Ok(Self::CallEnd),
            7 => Ok(Self::Release),
            _ => Err(invalid("RPC record kind", value.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallMode {
    Unary = 0,
    ServerStream = 1,
}

impl TryFrom<u8> for CallMode {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unary),
            1 => Ok(Self::ServerStream),
            _ => Err(invalid("call mode", value.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallStatus {
    Success = 0,
    Failed = 1,
    NotExecuted = 2,
}

impl TryFrom<u8> for CallStatus {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Success),
            1 => Ok(Self::Failed),
            2 => Ok(Self::NotExecuted),
            _ => Err(invalid("call status", value.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelCause {
    Consumer = 0,
    Lifecycle = 1,
}

impl TryFrom<u8> for CancelCause {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Consumer),
            1 => Ok(Self::Lifecycle),
            _ => Err(invalid("CANCEL cause", value.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueStorage<'a> {
    Inline(&'a [u8]),
    Shm(BuddyPayload),
}

impl ValueStorage<'_> {
    pub fn len(self) -> u64 {
        match self {
            Self::Inline(bytes) => bytes.len() as u64,
            Self::Shm(descriptor) => descriptor.data_size.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkPosition {
    Whole,
    First { total_bytes: u64 },
    Continuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValuePart<'a> {
    pub chunk: ChunkPosition,
    pub storage: ValueStorage<'a>,
}

impl<'a> ValuePart<'a> {
    pub fn whole(storage: ValueStorage<'a>) -> Self {
        Self {
            chunk: ChunkPosition::Whole,
            storage,
        }
    }

    pub fn first(total_bytes: u64, storage: ValueStorage<'a>) -> Self {
        Self {
            chunk: ChunkPosition::First { total_bytes },
            storage,
        }
    }

    pub fn continuation(storage: ValueStorage<'a>) -> Self {
        Self {
            chunk: ChunkPosition::Continuation,
            storage,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcRecord<'a> {
    Open {
        identity: RouteCallIdentity,
        method_idx: u16,
        mode: CallMode,
        value: ValuePart<'a>,
    },
    OpenContinuation {
        value: ValuePart<'a>,
    },
    ItemBegin {
        total_bytes: u64,
    },
    ItemData {
        value: ValuePart<'a>,
    },
    Credit {
        storage_bytes: u64,
        item_demand: u64,
    },
    Cancel {
        cause: CancelCause,
    },
    CallEnd {
        status: CallStatus,
        complete_items: Option<u64>,
        value: ValuePart<'a>,
        /// Canonical error decoded once at the wire boundary.
        error: Option<C2Error>,
    },
    CallEndContinuation {
        value: ValuePart<'a>,
    },
    Release(ReleaseBatch),
}

/// A failed record admission may still carry an already published SHM loan.
/// The connection must settle this descriptor on the original physical peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcDecodeFailure {
    pub error: DecodeError,
    pub carrier: Option<BuddyPayload>,
}

/// Decode a record while retaining a shape-valid SHM descriptor even if OPEN
/// identity or CALL_END status validation fails. The caller uses `carrier` on
/// failure to copy/discard and release the loan exactly once.
pub fn decode_rpc_record_with_carrier<'a>(
    header: &FrameHeader,
    payload: &'a [u8],
    mode: Option<CallMode>,
) -> Result<RpcRecord<'a>, RpcDecodeFailure> {
    decode_rpc_record(header, payload, mode).map_err(|error| RpcDecodeFailure {
        error,
        carrier: inspect_carrier(header, payload, mode),
    })
}

fn inspect_carrier(
    header: &FrameHeader,
    payload: &[u8],
    mode: Option<CallMode>,
) -> Option<BuddyPayload> {
    if header.flags & FLAG_BUDDY == 0 || header.flags & FLAG_RPC_RECORD == 0 || payload.is_empty() {
        return None;
    }
    let kind = RpcRecordKind::try_from(payload[0]).ok()?;
    if !matches!(
        kind,
        RpcRecordKind::Open | RpcRecordKind::ItemData | RpcRecordKind::CallEnd
    ) {
        return None;
    }
    let first = header.flags & FLAG_FIRST_CHUNK != 0;
    let continuation = header.flags & FLAG_CHUNKED != 0 && !first;
    let mut offset = 1usize;
    if !continuation {
        match kind {
            RpcRecordKind::Open => {
                for _ in 0..IDENTITY_FIELDS.len() {
                    let len = *payload.get(offset)? as usize;
                    offset = offset.checked_add(1 + len)?;
                    if offset > payload.len() {
                        return None;
                    }
                }
                offset = offset.checked_add(3)?;
            }
            RpcRecordKind::CallEnd => {
                let status = CallStatus::try_from(*payload.get(offset)?).ok()?;
                offset = offset.checked_add(1)?;
                match status {
                    CallStatus::NotExecuted => {}
                    CallStatus::Success | CallStatus::Failed => match mode? {
                        CallMode::Unary => {}
                        CallMode::ServerStream => offset = offset.checked_add(8)?,
                    },
                }
            }
            RpcRecordKind::ItemData => {}
            _ => unreachable!(),
        }
        if first {
            offset = offset.checked_add(8)?;
        }
    }
    if payload.len().checked_sub(offset)? != BUDDY_PAYLOAD_SIZE {
        return None;
    }
    decode_buddy_payload(&payload[offset..])
        .ok()
        .map(|(descriptor, _)| descriptor)
}

/// Validate a complete record frame. `mode` is needed for CALL_END metadata,
/// and comes from the active call rather than from the wire.
pub fn decode_rpc_record<'a>(
    header: &FrameHeader,
    payload: &'a [u8],
    mode: Option<CallMode>,
) -> Result<RpcRecord<'a>, DecodeError> {
    if header.total_len as usize != 12 + payload.len() {
        return Err(invalid("RPC record length", header.total_len.into()));
    }
    if payload.is_empty() {
        return Err(DecodeError::BufferTooShort { need: 1, have: 0 });
    }
    let kind = RpcRecordKind::try_from(payload[0])?;
    let allowed = FLAG_RPC_RECORD | FLAG_RESPONSE | FLAG_BUDDY | FLAG_CHUNKED | FLAG_FIRST_CHUNK;
    if header.flags & !allowed != 0 || header.flags & FLAG_RPC_RECORD == 0 {
        return Err(DecodeError::InvalidRpcEnvelope {
            field: "flags",
            value: header.flags.into(),
        });
    }
    let response = header.flags & FLAG_RESPONSE != 0;
    let expected_response = matches!(
        kind,
        RpcRecordKind::ItemBegin | RpcRecordKind::ItemData | RpcRecordKind::CallEnd
    );
    if kind != RpcRecordKind::Release && response != expected_response {
        return Err(DecodeError::InvalidRpcEnvelope {
            field: "direction",
            value: header.flags.into(),
        });
    }
    if (kind == RpcRecordKind::Release) != (header.request_id == 0) {
        return Err(DecodeError::InvalidRpcEnvelope {
            field: "request id",
            value: header.request_id,
        });
    }
    let chunked = header.flags & FLAG_CHUNKED != 0;
    let first = header.flags & FLAG_FIRST_CHUNK != 0;
    let shm = header.flags & FLAG_BUDDY != 0;
    if first && !chunked || chunked && !matches!(kind, RpcRecordKind::Open | RpcRecordKind::CallEnd)
    {
        return Err(invalid("RPC chunk flags", header.flags.into()));
    }
    if shm
        && !matches!(
            kind,
            RpcRecordKind::Open | RpcRecordKind::ItemData | RpcRecordKind::CallEnd
        )
    {
        return Err(invalid("RPC storage flags", header.flags.into()));
    }
    if kind == RpcRecordKind::Release {
        return decode_release_frame(header, payload, response, true).map(RpcRecord::Release);
    }
    let body = &payload[1..];
    let no_storage = !shm && !chunked && !first;
    match kind {
        RpcRecordKind::Open => {
            if chunked && !first {
                return Ok(RpcRecord::OpenContinuation {
                    value: decode_value(body, shm, ChunkPosition::Continuation)?,
                });
            }
            let (identity, method_idx, mode, used) = decode_open_metadata(payload)?;
            let value = decode_value_at(payload, used, shm, first)?;
            Ok(RpcRecord::Open {
                identity,
                method_idx,
                mode,
                value,
            })
        }
        RpcRecordKind::ItemBegin => {
            require_exact(body, 8, "ITEM_BEGIN")?;
            if !no_storage {
                return Err(invalid("ITEM_BEGIN flags", header.flags.into()));
            }
            Ok(RpcRecord::ItemBegin {
                total_bytes: u64::from_le_bytes(body.try_into().unwrap()),
            })
        }
        RpcRecordKind::ItemData => {
            let value = decode_value(body, shm, ChunkPosition::Whole)?;
            if value.storage.len() == 0 {
                return Err(invalid("ITEM_DATA length", 0));
            }
            Ok(RpcRecord::ItemData { value })
        }
        RpcRecordKind::Credit => {
            require_exact(body, 16, "CREDIT")?;
            if !no_storage {
                return Err(invalid("CREDIT flags", header.flags.into()));
            }
            let storage_bytes = u64::from_le_bytes(body[..8].try_into().unwrap());
            let item_demand = u64::from_le_bytes(body[8..].try_into().unwrap());
            if storage_bytes == 0 && item_demand == 0 {
                return Err(invalid("CREDIT zero grant", 0));
            }
            Ok(RpcRecord::Credit {
                storage_bytes,
                item_demand,
            })
        }
        RpcRecordKind::Cancel => {
            require_exact(body, 1, "CANCEL")?;
            if !no_storage {
                return Err(invalid("CANCEL flags", header.flags.into()));
            }
            Ok(RpcRecord::Cancel {
                cause: CancelCause::try_from(body[0])?,
            })
        }
        RpcRecordKind::CallEnd => {
            if chunked && !first {
                return Ok(RpcRecord::CallEndContinuation {
                    value: decode_value(body, shm, ChunkPosition::Continuation)?,
                });
            }
            if body.is_empty() {
                return Err(DecodeError::BufferTooShort { need: 1, have: 0 });
            }
            let status = CallStatus::try_from(body[0])?;
            let (complete_items, offset) = match status {
                CallStatus::NotExecuted => (None, 1),
                CallStatus::Success | CallStatus::Failed => {
                    match mode.ok_or_else(|| invalid("CALL_END mode", 0))? {
                        CallMode::Unary => (None, 1),
                        CallMode::ServerStream => {
                            if body.len() < 9 {
                                return Err(DecodeError::BufferTooShort {
                                    need: 9,
                                    have: body.len(),
                                });
                            }
                            (Some(u64::from_le_bytes(body[1..9].try_into().unwrap())), 9)
                        }
                    }
                }
            };
            let value = decode_value_at(body, offset, shm, first)?;
            let error = validate_call_end(status, mode, complete_items.unwrap_or(0), value)
                .map_err(|reason| DecodeError::InvalidText {
                    field: "CALL_END",
                    reason,
                })?;
            Ok(RpcRecord::CallEnd {
                status,
                complete_items,
                value,
                error,
            })
        }
        RpcRecordKind::Release => unreachable!(),
    }
}

fn decode_value_at<'a>(
    body: &'a [u8],
    offset: usize,
    shm: bool,
    first: bool,
) -> Result<ValuePart<'a>, DecodeError> {
    if offset > body.len() {
        return Err(DecodeError::BufferTooShort {
            need: offset,
            have: body.len(),
        });
    }
    if first {
        if body.len() - offset < 8 {
            return Err(DecodeError::BufferTooShort {
                need: 8,
                have: body.len() - offset,
            });
        }
        let total_bytes = u64::from_le_bytes(body[offset..offset + 8].try_into().unwrap());
        decode_value(
            &body[offset + 8..],
            shm,
            ChunkPosition::First { total_bytes },
        )
    } else {
        decode_value(&body[offset..], shm, ChunkPosition::Whole)
    }
}

fn decode_value<'a>(
    body: &'a [u8],
    shm: bool,
    chunk: ChunkPosition,
) -> Result<ValuePart<'a>, DecodeError> {
    let storage = if shm {
        require_exact(body, BUDDY_PAYLOAD_SIZE, "SHM descriptor")?;
        let (descriptor, _) = decode_buddy_payload(body)?;
        if descriptor.data_size == 0 {
            return Err(invalid("SHM data size", 0));
        }
        ValueStorage::Shm(descriptor)
    } else {
        ValueStorage::Inline(body)
    };
    let len = storage.len();
    match chunk {
        ChunkPosition::Whole => {}
        ChunkPosition::First { total_bytes } if len > 0 && len < total_bytes => {}
        ChunkPosition::Continuation if len > 0 => {}
        _ => return Err(invalid("RPC chunk length", len)),
    }
    Ok(ValuePart { chunk, storage })
}

fn validate_call_end(
    status: CallStatus,
    mode: Option<CallMode>,
    complete_items: u64,
    value: ValuePart<'_>,
) -> Result<Option<C2Error>, String> {
    if status == CallStatus::NotExecuted && complete_items != 0 {
        return Err("NotExecuted has completed items".into());
    }
    if status != CallStatus::NotExecuted {
        match mode.ok_or_else(|| "CALL_END mode is unknown".to_string())? {
            CallMode::Unary if complete_items != 0 => {
                return Err("unary call has completed items".into());
            }
            _ => {}
        }
    }
    if status != CallStatus::Success {
        let ValuePart {
            chunk: ChunkPosition::Whole,
            storage: ValueStorage::Inline(bytes),
        } = value
        else {
            return Err("error must be complete inline C2E1".into());
        };
        if bytes.is_empty() {
            return Err("error is empty".into());
        }
        return C2Error::from_wire_bytes(bytes)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "error is empty".into())
            .map(Some);
    } else if mode == Some(CallMode::ServerStream)
        && value != ValuePart::whole(ValueStorage::Inline(&[]))
    {
        return Err("successful server stream has no result value".into());
    }
    Ok(None)
}

fn require_exact(body: &[u8], len: usize, field: &'static str) -> Result<(), DecodeError> {
    if body.len() != len {
        Err(invalid(field, body.len() as u64))
    } else {
        Ok(())
    }
}

fn invalid(field: &'static str, value: u64) -> DecodeError {
    DecodeError::InvalidValue { field, value }
}

fn chunk_layout(chunk: ChunkPosition, bytes: u64) -> Result<(usize, u32), EncodeError> {
    let mut flags = 0;
    let prefix_len = match chunk {
        ChunkPosition::Whole => 0,
        ChunkPosition::First { total_bytes } if bytes > 0 && bytes < total_bytes => {
            flags |= FLAG_CHUNKED | FLAG_FIRST_CHUNK;
            8
        }
        ChunkPosition::Continuation if bytes > 0 => {
            flags |= FLAG_CHUNKED;
            0
        }
        _ => {
            return Err(encode_invalid(
                "RPC chunk length",
                "first/continuation requires a nonempty strict chunk",
            ));
        }
    };
    Ok((prefix_len, flags))
}

fn value_layout(value: ValuePart<'_>) -> Result<(usize, u32), EncodeError> {
    let (prefix_len, mut flags) = chunk_layout(value.chunk, value.storage.len())?;
    let storage_len = match value.storage {
        ValueStorage::Inline(bytes) => bytes.len(),
        ValueStorage::Shm(descriptor) => {
            flags |= FLAG_BUDDY;
            if descriptor.data_size == 0 {
                return Err(encode_invalid("SHM data size", "must be nonzero"));
            }
            BUDDY_PAYLOAD_SIZE
        }
    };
    Ok((record_len(prefix_len, storage_len)?, flags))
}

fn write_value(out: &mut Vec<u8>, value: ValuePart<'_>) {
    if let ChunkPosition::First { total_bytes } = value.chunk {
        out.extend_from_slice(&total_bytes.to_le_bytes());
    }
    write_storage(out, value.storage);
}

fn write_storage(out: &mut Vec<u8>, storage: ValueStorage<'_>) {
    match storage {
        ValueStorage::Inline(bytes) => out.extend_from_slice(bytes),
        ValueStorage::Shm(descriptor) => {
            let start = out.len();
            out.resize(start + BUDDY_PAYLOAD_SIZE, 0);
            encode_buddy_payload_into(
                &descriptor,
                (&mut out[start..])
                    .try_into()
                    .expect("fixed SHM descriptor"),
            );
        }
    }
}

/// Allocate the final frame only after all record lengths have been checked.
fn record_frame(request_id: u64, flags: u32, payload_len: usize) -> Result<Vec<u8>, EncodeError> {
    if payload_len > MAX_RPC_RECORD_BYTES {
        return Err(EncodeError::FieldTooLong {
            field: "RPC record",
            max: MAX_RPC_RECORD_BYTES,
            actual: payload_len,
        });
    }
    let mut out = Vec::with_capacity(HEADER_SIZE + payload_len);
    out.extend_from_slice(&((12 + payload_len) as u32).to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    Ok(out)
}

fn record_len(prefix_len: usize, value_len: usize) -> Result<usize, EncodeError> {
    prefix_len
        .checked_add(value_len)
        .ok_or(EncodeError::FieldTooLong {
            field: "RPC record",
            max: MAX_RPC_RECORD_BYTES,
            actual: usize::MAX,
        })
}

fn encode_invalid(field: &'static str, reason: &str) -> EncodeError {
    EncodeError::InvalidText {
        field,
        reason: reason.into(),
    }
}

/// Encode OPEN's complete or first value. Continuations use `encode_value_continuation_frame`.
pub fn encode_open_frame<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    mode: CallMode,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("OPEN", "invalid request id or continuation"));
    }
    encode_open_record(request_id, identity, method_idx, mode, value)
}

/// Encode a complete OPEN draft. The connection's unique writer assigns its RID.
pub fn encode_open_draft_frame<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    mode: CallMode,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    encode_open_record(0, identity, method_idx, mode, value)
}

fn encode_open_record<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    mode: CallMode,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    let (value_len, flags) = value_layout(value)?;
    let mut out = open_record_prefix(
        request_id,
        identity,
        method_idx,
        mode,
        value.chunk,
        value_len,
        flags,
    )?;
    write_storage(&mut out, value.storage);
    Ok(out)
}

/// Reserve one complete inline OPEN draft before a charged streaming source
/// appends exactly `value_bytes` bytes. Its final length is already in the header.
pub fn encode_open_inline_draft_prefix<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    mode: CallMode,
    chunk: ChunkPosition,
    value_bytes: usize,
) -> Result<Vec<u8>, EncodeError> {
    let (prefix_len, flags) = chunk_layout(chunk, value_bytes as u64)?;
    open_record_prefix(
        0,
        identity,
        method_idx,
        mode,
        chunk,
        record_len(prefix_len, value_bytes)?,
        flags,
    )
}

fn open_record_prefix<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    mode: CallMode,
    chunk: ChunkPosition,
    value_len: usize,
    flags: u32,
) -> Result<Vec<u8>, EncodeError> {
    if chunk == ChunkPosition::Continuation {
        return Err(encode_invalid("OPEN", "invalid request id or continuation"));
    }
    let fields = identity_fields(identity);
    let metadata_len = 1 + identity_encoded_len(&fields)? + 2 + 1;
    let mut out = record_frame(
        request_id,
        FLAG_RPC_RECORD | flags,
        record_len(metadata_len, value_len)?,
    )?;
    out.push(RpcRecordKind::Open as u8);
    for field in fields {
        out.push(field.len() as u8);
        out.extend_from_slice(field.as_bytes());
    }
    out.extend_from_slice(&method_idx.to_le_bytes());
    out.push(mode as u8);
    if let ChunkPosition::First { total_bytes } = chunk {
        out.extend_from_slice(&total_bytes.to_le_bytes());
    }
    Ok(out)
}

/// Encode CALL_END with mode-dependent metadata. Error bytes are checked once.
pub fn encode_call_end_frame(
    request_id: u64,
    mode: CallMode,
    status: CallStatus,
    complete_items: u64,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 || value.chunk == ChunkPosition::Continuation {
        return Err(encode_invalid(
            "CALL_END",
            "invalid request id or continuation",
        ));
    }
    validate_call_end(status, Some(mode), complete_items, value).map_err(|reason| {
        EncodeError::InvalidText {
            field: "CALL_END",
            reason,
        }
    })?;
    let has_count = status != CallStatus::NotExecuted && mode == CallMode::ServerStream;
    let (value_len, flags) = value_layout(value)?;
    let mut out = record_frame(
        request_id,
        FLAG_RPC_RECORD | FLAG_RESPONSE | flags,
        record_len(2 + if has_count { 8 } else { 0 }, value_len)?,
    )?;
    out.extend_from_slice(&[RpcRecordKind::CallEnd as u8, status as u8]);
    if has_count {
        out.extend_from_slice(&complete_items.to_le_bytes());
    }
    write_value(&mut out, value);
    Ok(out)
}

/// Encode a subsequent OPEN or CALL_END value chunk without repeating metadata.
pub fn encode_value_continuation_frame(
    request_id: u64,
    kind: RpcRecordKind,
    storage: ValueStorage<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid(
            "RPC continuation",
            "invalid request id or kind",
        ));
    }
    encode_continuation_record(request_id, kind, storage)
}

/// Encode an OPEN continuation draft for the same unique connection writer.
pub fn encode_open_continuation_draft_frame(
    storage: ValueStorage<'_>,
) -> Result<Vec<u8>, EncodeError> {
    encode_continuation_record(0, RpcRecordKind::Open, storage)
}

/// Reserve a complete inline OPEN continuation before appending its data.
pub fn encode_open_continuation_draft_prefix(value_bytes: usize) -> Result<Vec<u8>, EncodeError> {
    let (prefix_len, flags) = chunk_layout(ChunkPosition::Continuation, value_bytes as u64)?;
    continuation_record_prefix(
        0,
        RpcRecordKind::Open,
        record_len(prefix_len, value_bytes)?,
        flags,
    )
}

fn encode_continuation_record(
    request_id: u64,
    kind: RpcRecordKind,
    storage: ValueStorage<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if !matches!(kind, RpcRecordKind::Open | RpcRecordKind::CallEnd) {
        return Err(encode_invalid(
            "RPC continuation",
            "invalid request id or kind",
        ));
    }
    let value = ValuePart::continuation(storage);
    let (value_len, flags) = value_layout(value)?;
    let mut out = continuation_record_prefix(request_id, kind, value_len, flags)?;
    write_storage(&mut out, storage);
    Ok(out)
}

fn continuation_record_prefix(
    request_id: u64,
    kind: RpcRecordKind,
    value_len: usize,
    flags: u32,
) -> Result<Vec<u8>, EncodeError> {
    let mut out = record_frame(
        request_id,
        FLAG_RPC_RECORD
            | if kind == RpcRecordKind::CallEnd {
                FLAG_RESPONSE
            } else {
                0
            }
            | flags,
        record_len(1, value_len)?,
    )?;
    out.push(kind as u8);
    Ok(out)
}

pub fn encode_item_begin_frame(request_id: u64, total_bytes: u64) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("ITEM_BEGIN", "request id must be nonzero"));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, 9)?;
    out.push(RpcRecordKind::ItemBegin as u8);
    out.extend_from_slice(&total_bytes.to_le_bytes());
    Ok(out)
}

pub fn encode_item_data_frame(
    request_id: u64,
    storage: ValueStorage<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 || storage.len() == 0 {
        return Err(encode_invalid(
            "ITEM_DATA",
            "request id and storage length must be nonzero",
        ));
    }
    let value = ValuePart::whole(storage);
    let (value_len, flags) = value_layout(value)?;
    let mut out = record_frame(
        request_id,
        FLAG_RPC_RECORD | FLAG_RESPONSE | flags,
        record_len(1, value_len)?,
    )?;
    out.push(RpcRecordKind::ItemData as u8);
    write_value(&mut out, value);
    Ok(out)
}

pub fn encode_credit_frame(
    request_id: u64,
    storage_bytes: u64,
    item_demand: u64,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 || storage_bytes == 0 && item_demand == 0 {
        return Err(encode_invalid("CREDIT", "invalid request id or zero grant"));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, 17)?;
    out.push(RpcRecordKind::Credit as u8);
    out.extend_from_slice(&storage_bytes.to_le_bytes());
    out.extend_from_slice(&item_demand.to_le_bytes());
    Ok(out)
}

pub fn encode_cancel_frame(request_id: u64, cause: CancelCause) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("CANCEL", "request id must be nonzero"));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, 2)?;
    out.extend_from_slice(&[RpcRecordKind::Cancel as u8, cause as u8]);
    Ok(out)
}
