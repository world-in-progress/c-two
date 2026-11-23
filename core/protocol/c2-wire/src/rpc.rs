//! IPC 18 RPC record codec. Connection admission owns call state and cumulative offsets.

use c2_error::{C2Error, ErrorCode};

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

/// Byte budget for one inline CALL_END C2E1 error payload so the complete
/// frame never exceeds c2_config::MAX_RPC_FRAME_BYTES. Reserve room for the
/// frame envelope (12), kind byte, status byte and the server-stream item
/// count (8).
pub const MAX_INLINE_CALL_END_ERROR_BYTES: usize =
    (c2_config::MAX_RPC_FRAME_BYTES as usize).saturating_sub(12 + 1 + 1 + 8);

/// Compact byte budget for a QueryCallResult error C2E1 payload so control
/// replies stay tiny even when a route or admission error carries details.
pub const MAX_QUERY_CALL_ERROR_BYTES: usize = 4096;

/// Truncation marker appended to truncated `message` and `details` values of
/// a bounded CALL_END error. Short and ASCII so it never crosses a char
/// boundary or grows the payload measurably.
const CALL_END_TRUNCATION_MARKER: &str = "...[truncated]";

/// Wire encoding failed because a field cannot be represented by IPC 18.
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

/// Size of OPEN's kind, identity, mode and method index for unkeyed calls, without allocating.
pub fn encoded_open_metadata_len<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
) -> Result<usize, EncodeError> {
    Ok(4 + identity_encoded_len(&identity_fields(identity))?)
}

/// Size of OPEN's kind, identity, mode, method index and length-prefixed call ID
/// for a `KeyedUnary` call, without allocating.
pub fn encoded_keyed_open_metadata_len<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    call_id: &str,
) -> Result<usize, EncodeError> {
    validate_wire_call_id_encode(call_id)?;
    Ok(encoded_open_metadata_len(identity)? + 1 + call_id.len())
}

fn validate_wire_call_id_encode(call_id: &str) -> Result<(), EncodeError> {
    if call_id.len() > c2_config::MAX_CALL_ID_LEN {
        return Err(EncodeError::FieldTooLong {
            field: "call_id",
            max: c2_config::MAX_CALL_ID_LEN,
            actual: call_id.len(),
        });
    }
    c2_config::validate_call_id(call_id).map_err(|reason| EncodeError::InvalidText {
        field: "call_id",
        reason,
    })
}

fn read_call_id(input: &[u8], pos: &mut usize) -> Result<String, DecodeError> {
    let call_id = read_identity_text(input, pos, "call_id")?;
    c2_config::validate_call_id(&call_id).map_err(|reason| DecodeError::InvalidText {
        field: "call_id",
        reason,
    })?;
    Ok(call_id)
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

/// Decode and validate the OPEN kind, six identity fields, call mode, method
/// index and optional call ID for unary, keyed unary or server-stream calls.
/// Returns the byte offset where value storage begins.
pub fn decode_open_metadata(
    payload: &[u8],
) -> Result<(RouteCallIdentity, u16, CallMode, Option<String>, usize), DecodeError> {
    let (identity, mode, mut pos) = decode_open_identity_and_mode(payload)?;
    if matches!(mode, CallMode::Subscribe | CallMode::Recover) {
        return Err(invalid("call mode", mode as u64));
    }
    if payload.len() - pos < 2 {
        return Err(DecodeError::Truncated {
            field: "method_idx",
            need: 2,
            have: payload.len() - pos,
        });
    }
    let method_idx = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
    pos += 2;
    let call_id = if mode == CallMode::KeyedUnary {
        Some(read_call_id(payload, &mut pos)?)
    } else {
        None
    };
    Ok((identity, method_idx, mode, call_id, pos))
}

fn decode_route_call_identity(
    payload: &[u8],
    pos: &mut usize,
) -> Result<RouteCallIdentity, DecodeError> {
    let route_name = read_identity_text(payload, pos, "route_name")?;
    c2_contract::validate_call_route_key("route_name", &route_name).map_err(|error| {
        DecodeError::InvalidText {
            field: "route_name",
            reason: error.to_string(),
        }
    })?;
    let crm_ns = read_identity_text(payload, pos, "crm_ns")?;
    let crm_name = read_identity_text(payload, pos, "crm_name")?;
    let crm_ver = read_identity_text(payload, pos, "crm_ver")?;
    c2_contract::validate_crm_tag(&crm_ns, &crm_name, &crm_ver).map_err(|error| {
        DecodeError::InvalidText {
            field: "crm tag",
            reason: error.to_string(),
        }
    })?;
    let abi_hash = read_identity_text(payload, pos, "abi_hash")?;
    c2_contract::validate_contract_hash("abi_hash", &abi_hash).map_err(|error| {
        DecodeError::InvalidText {
            field: "abi_hash",
            reason: error.to_string(),
        }
    })?;
    let signature_hash = read_identity_text(payload, pos, "signature_hash")?;
    c2_contract::validate_contract_hash("signature_hash", &signature_hash).map_err(|error| {
        DecodeError::InvalidText {
            field: "signature_hash",
            reason: error.to_string(),
        }
    })?;
    Ok(RouteCallIdentity {
        route_name,
        crm_ns,
        crm_name,
        crm_ver,
        abi_hash,
        signature_hash,
    })
}

fn decode_open_identity_and_mode(
    payload: &[u8],
) -> Result<(RouteCallIdentity, CallMode, usize), DecodeError> {
    if payload.first().copied() != Some(RpcRecordKind::Open as u8) {
        return Err(invalid(
            "OPEN kind",
            payload.first().copied().unwrap_or(0).into(),
        ));
    }
    let mut pos = 1;
    let identity = decode_route_call_identity(payload, &mut pos)?;
    let mode = CallMode::try_from(*payload.get(pos).ok_or(DecodeError::Truncated {
        field: "call mode",
        need: 1,
        have: 0,
    })?)?;
    pos += 1;
    Ok((identity, mode, pos))
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
    Ready = 8,
    Sync = 9,
    SyncAck = 10,
    QueryCall = 11,
    QueryCallResult = 12,
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
            8 => Ok(Self::Ready),
            9 => Ok(Self::Sync),
            10 => Ok(Self::SyncAck),
            11 => Ok(Self::QueryCall),
            12 => Ok(Self::QueryCallResult),
            _ => Err(invalid("RPC record kind", value.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallMode {
    Unary = 0,
    ServerStream = 1,
    Subscribe = 2,
    KeyedUnary = 3,
    Recover = 4,
}

impl CallMode {
    /// True when the response terminates as a single unary `CALL_END` without
    /// a server-stream / subscription item counter.
    pub const fn is_unary_response(self) -> bool {
        matches!(self, Self::Unary | Self::KeyedUnary | Self::Recover)
    }

    /// Underlying contract method mode required by this wire call mode.
    pub const fn contract_method_mode(self) -> Self {
        match self {
            Self::KeyedUnary | Self::Recover => Self::Unary,
            other => other,
        }
    }
}

impl TryFrom<u8> for CallMode {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unary),
            1 => Ok(Self::ServerStream),
            2 => Ok(Self::Subscribe),
            3 => Ok(Self::KeyedUnary),
            4 => Ok(Self::Recover),
            _ => Err(invalid("call mode", value.into())),
        }
    }
}

/// Wire encoding of a keyed unary call's status snapshot returned by `QueryCallResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WireCallOutcomeStatus {
    InFlight = 0,
    Succeeded = 1,
    Failed = 2,
    Unknown = 3,
}

impl WireCallOutcomeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

impl TryFrom<u8> for WireCallOutcomeStatus {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::InFlight),
            1 => Ok(Self::Succeeded),
            2 => Ok(Self::Failed),
            3 => Ok(Self::Unknown),
            _ => Err(invalid("call outcome status", value.into())),
        }
    }
}

/// Wire encoding of why a succeeded keyed unary call's response bytes cannot be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WireResultUnavailableReason {
    NotRetained = 1,
    Evicted = 2,
}

impl WireResultUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRetained => "not_retained",
            Self::Evicted => "evicted",
        }
    }
}

impl TryFrom<u8> for WireResultUnavailableReason {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::NotRetained),
            2 => Ok(Self::Evicted),
            _ => Err(invalid("result unavailable reason", value.into())),
        }
    }
}

/// Compact bounded status snapshot returned by `QueryCallResult`.
///
/// Full response payloads and full `C2Error` bodies are intentionally excluded
/// and retrieved through `Recover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireCallOutcome {
    pub status: WireCallOutcomeStatus,
    pub method_name: Option<String>,
    pub result_available: bool,
    pub result_unavailable_reason: Option<WireResultUnavailableReason>,
    pub error_code: Option<ErrorCode>,
    pub error_details_available: bool,
}

impl WireCallOutcome {
    pub fn unknown() -> Self {
        Self {
            status: WireCallOutcomeStatus::Unknown,
            method_name: None,
            result_available: false,
            result_unavailable_reason: None,
            error_code: None,
            error_details_available: false,
        }
    }

    pub fn in_flight(method_name: String) -> Self {
        Self {
            status: WireCallOutcomeStatus::InFlight,
            method_name: Some(method_name),
            result_available: false,
            result_unavailable_reason: None,
            error_code: None,
            error_details_available: false,
        }
    }

    pub fn succeeded(
        method_name: String,
        result_available: bool,
        result_unavailable_reason: Option<WireResultUnavailableReason>,
    ) -> Self {
        Self {
            status: WireCallOutcomeStatus::Succeeded,
            method_name: Some(method_name),
            result_available,
            result_unavailable_reason,
            error_code: None,
            error_details_available: false,
        }
    }

    pub fn failed(
        method_name: String,
        error_code: ErrorCode,
        error_details_available: bool,
    ) -> Self {
        Self {
            status: WireCallOutcomeStatus::Failed,
            method_name: Some(method_name),
            result_available: false,
            result_unavailable_reason: None,
            error_code: Some(error_code),
            error_details_available,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(name) = &self.method_name {
            if name.is_empty() || name.len() > u8::MAX as usize {
                return Err("method_name length out of range");
            }
            if c2_contract::validate_contract_text_field("method_name", name).is_err() {
                return Err("method_name is invalid");
            }
        }
        match self.status {
            WireCallOutcomeStatus::Unknown => {
                if self.method_name.is_some()
                    || self.result_available
                    || self.result_unavailable_reason.is_some()
                    || self.error_code.is_some()
                    || self.error_details_available
                {
                    return Err("unknown outcome must not carry execution metadata");
                }
            }
            WireCallOutcomeStatus::InFlight => {
                if self.method_name.is_none()
                    || self.result_available
                    || self.result_unavailable_reason.is_some()
                    || self.error_code.is_some()
                    || self.error_details_available
                {
                    return Err("in_flight outcome must carry only method_name");
                }
            }
            WireCallOutcomeStatus::Succeeded => {
                if self.method_name.is_none()
                    || self.error_code.is_some()
                    || self.error_details_available
                {
                    return Err("succeeded outcome must not carry failure fields");
                }
                if self.result_available && self.result_unavailable_reason.is_some() {
                    return Err("available result must not carry unavailable reason");
                }
                if !self.result_available && self.result_unavailable_reason.is_none() {
                    return Err("unavailable result must carry unavailable reason");
                }
            }
            WireCallOutcomeStatus::Failed => {
                if self.method_name.is_none()
                    || self.result_available
                    || self.result_unavailable_reason.is_some()
                    || self.error_code.is_none()
                {
                    return Err("failed outcome must carry method_name and error_code");
                }
            }
        }
        Ok(())
    }
}

/// Payload carried by `RpcRecordKind::QueryCallResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireQueryCallResult {
    Outcome(WireCallOutcome),
    Error(C2Error),
}

/// Wire encoding of subscription starting position (`start`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WireTopicStart {
    #[default]
    Latest = 0,
    Next = 1,
}

/// Wire encoding of subscription lag policy (`on_lag`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WireLagPolicy {
    #[default]
    Skip = 0,
    Error = 1,
}

/// Packed one-byte subscription options carried in IPC 18 Subscribe OPEN.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WireSubscribeOptions {
    pub start: WireTopicStart,
    pub on_lag: WireLagPolicy,
}

impl WireSubscribeOptions {
    pub const fn to_byte(self) -> u8 {
        (self.start as u8) | ((self.on_lag as u8) << 1)
    }

    pub fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        if byte & !0x03 != 0 {
            return Err(invalid("subscribe options", byte.into()));
        }
        let start = if byte & 0x01 == 0 {
            WireTopicStart::Latest
        } else {
            WireTopicStart::Next
        };
        let on_lag = if byte & 0x02 == 0 {
            WireLagPolicy::Skip
        } else {
            WireLagPolicy::Error
        };
        Ok(Self { start, on_lag })
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
        call_id: Option<String>,
        value: ValuePart<'a>,
    },
    OpenSubscribe {
        identity: RouteCallIdentity,
        topic_name: String,
        options: WireSubscribeOptions,
    },
    OpenRecover {
        identity: RouteCallIdentity,
        method_idx: u16,
        call_id: String,
    },
    OpenContinuation {
        value: ValuePart<'a>,
    },
    Ready {
        start_seq: u64,
    },
    Sync,
    SyncAck {
        head_seq: u64,
    },
    QueryCall {
        identity: RouteCallIdentity,
        call_id: String,
    },
    QueryCallResult(WireQueryCallResult),
    ItemBegin {
        total_bytes: u64,
        seq: Option<u64>,
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
                let mode_byte = *payload.get(offset)?;
                offset = offset.checked_add(1)?;
                match CallMode::try_from(mode_byte).ok()? {
                    CallMode::Subscribe | CallMode::Recover => return None,
                    CallMode::Unary | CallMode::ServerStream => {
                        offset = offset.checked_add(2)?;
                    }
                    CallMode::KeyedUnary => {
                        offset = offset.checked_add(2)?;
                        let call_id_len = *payload.get(offset)? as usize;
                        offset = offset.checked_add(1 + call_id_len)?;
                        if offset > payload.len() {
                            return None;
                        }
                    }
                }
            }
            RpcRecordKind::CallEnd => {
                let status = CallStatus::try_from(*payload.get(offset)?).ok()?;
                offset = offset.checked_add(1)?;
                match status {
                    CallStatus::NotExecuted => {}
                    CallStatus::Success | CallStatus::Failed => {
                        let mode = mode?;
                        if !mode.is_unary_response() {
                            offset = offset.checked_add(8)?;
                        }
                    }
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
        RpcRecordKind::ItemBegin
            | RpcRecordKind::ItemData
            | RpcRecordKind::CallEnd
            | RpcRecordKind::Ready
            | RpcRecordKind::SyncAck
            | RpcRecordKind::QueryCallResult
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
            let (identity, mode, mut used) = decode_open_identity_and_mode(payload)?;
            if mode == CallMode::Subscribe {
                if !no_storage {
                    return Err(invalid("Subscribe OPEN flags", header.flags.into()));
                }
                let topic_name = read_identity_text(payload, &mut used, "topic_name")?;
                c2_contract::validate_contract_text_field("topic_name", &topic_name).map_err(
                    |error| DecodeError::InvalidText {
                        field: "topic_name",
                        reason: error.to_string(),
                    },
                )?;
                let options_byte = *payload.get(used).ok_or(DecodeError::Truncated {
                    field: "subscribe options",
                    need: 1,
                    have: 0,
                })?;
                used += 1;
                if used != payload.len() {
                    return Err(invalid(
                        "Subscribe OPEN trailing bytes",
                        (payload.len() - used) as u64,
                    ));
                }
                let options = WireSubscribeOptions::from_byte(options_byte)?;
                return Ok(RpcRecord::OpenSubscribe {
                    identity,
                    topic_name,
                    options,
                });
            }
            if payload.len() - used < 2 {
                return Err(DecodeError::Truncated {
                    field: "method_idx",
                    need: 2,
                    have: payload.len() - used,
                });
            }
            let method_idx = u16::from_le_bytes(payload[used..used + 2].try_into().unwrap());
            used += 2;
            if mode == CallMode::Recover {
                if !no_storage {
                    return Err(invalid("Recover OPEN flags", header.flags.into()));
                }
                let call_id = read_call_id(payload, &mut used)?;
                if used != payload.len() {
                    return Err(invalid(
                        "Recover OPEN trailing bytes",
                        (payload.len() - used) as u64,
                    ));
                }
                return Ok(RpcRecord::OpenRecover {
                    identity,
                    method_idx,
                    call_id,
                });
            }
            let call_id = if mode == CallMode::KeyedUnary {
                Some(read_call_id(payload, &mut used)?)
            } else {
                None
            };
            let value = decode_value_at(payload, used, shm, first)?;
            Ok(RpcRecord::Open {
                identity,
                method_idx,
                mode,
                call_id,
                value,
            })
        }
        RpcRecordKind::QueryCall => {
            if !no_storage {
                return Err(invalid("QUERY_CALL flags", header.flags.into()));
            }
            let mut pos = 1;
            let identity = decode_route_call_identity(payload, &mut pos)?;
            let call_id = read_call_id(payload, &mut pos)?;
            if pos != payload.len() {
                return Err(invalid(
                    "QUERY_CALL trailing bytes",
                    (payload.len() - pos) as u64,
                ));
            }
            Ok(RpcRecord::QueryCall { identity, call_id })
        }
        RpcRecordKind::QueryCallResult => {
            if !no_storage {
                return Err(invalid("QUERY_CALL_RESULT flags", header.flags.into()));
            }
            Ok(RpcRecord::QueryCallResult(decode_query_call_result(body)?))
        }
        RpcRecordKind::Ready => {
            require_exact(body, 8, "READY")?;
            if !no_storage {
                return Err(invalid("READY flags", header.flags.into()));
            }
            let start_seq = u64::from_le_bytes(body.try_into().unwrap());
            if start_seq == 0 {
                return Err(invalid("READY start_seq", 0));
            }
            Ok(RpcRecord::Ready { start_seq })
        }
        RpcRecordKind::Sync => {
            require_exact(body, 0, "SYNC")?;
            if !no_storage {
                return Err(invalid("SYNC flags", header.flags.into()));
            }
            Ok(RpcRecord::Sync)
        }
        RpcRecordKind::SyncAck => {
            require_exact(body, 8, "SYNC_ACK")?;
            if !no_storage {
                return Err(invalid("SYNC_ACK flags", header.flags.into()));
            }
            Ok(RpcRecord::SyncAck {
                head_seq: u64::from_le_bytes(body.try_into().unwrap()),
            })
        }
        RpcRecordKind::ItemBegin => {
            if !no_storage {
                return Err(invalid("ITEM_BEGIN flags", header.flags.into()));
            }
            match mode {
                Some(CallMode::ServerStream) => {
                    require_exact(body, 8, "ITEM_BEGIN")?;
                    Ok(RpcRecord::ItemBegin {
                        total_bytes: u64::from_le_bytes(body.try_into().unwrap()),
                        seq: None,
                    })
                }
                Some(CallMode::Subscribe) => {
                    require_exact(body, 16, "Subscribe ITEM_BEGIN")?;
                    let seq = u64::from_le_bytes(body[..8].try_into().unwrap());
                    if seq == 0 {
                        return Err(invalid("ITEM_BEGIN seq", 0));
                    }
                    let total_bytes = u64::from_le_bytes(body[8..16].try_into().unwrap());
                    Ok(RpcRecord::ItemBegin {
                        total_bytes,
                        seq: Some(seq),
                    })
                }
                None => match body.len() {
                    8 => Ok(RpcRecord::ItemBegin {
                        total_bytes: u64::from_le_bytes(body.try_into().unwrap()),
                        seq: None,
                    }),
                    16 => {
                        let seq = u64::from_le_bytes(body[..8].try_into().unwrap());
                        if seq == 0 {
                            return Err(invalid("ITEM_BEGIN seq", 0));
                        }
                        let total_bytes = u64::from_le_bytes(body[8..16].try_into().unwrap());
                        Ok(RpcRecord::ItemBegin {
                            total_bytes,
                            seq: Some(seq),
                        })
                    }
                    other => Err(invalid("ITEM_BEGIN", other as u64)),
                },
                Some(mode @ (CallMode::Unary | CallMode::KeyedUnary | CallMode::Recover)) => {
                    Err(invalid("ITEM_BEGIN mode", mode as u64))
                }
            }
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
                        mode if mode.is_unary_response() => (None, 1),
                        CallMode::ServerStream | CallMode::Subscribe => {
                            if body.len() < 9 {
                                return Err(DecodeError::BufferTooShort {
                                    need: 9,
                                    have: body.len(),
                                });
                            }
                            (Some(u64::from_le_bytes(body[1..9].try_into().unwrap())), 9)
                        }
                        _ => unreachable!(),
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
            mode if mode.is_unary_response() && complete_items != 0 => {
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
    } else if mode == Some(CallMode::Subscribe) {
        return Err("subscription CALL_END must carry a terminal error".into());
    } else if mode == Some(CallMode::ServerStream)
        && value != ValuePart::whole(ValueStorage::Inline(&[]))
    {
        return Err("successful server stream has no result value".into());
    }
    Ok(None)
}

fn decode_query_call_result(body: &[u8]) -> Result<WireQueryCallResult, DecodeError> {
    let tag = *body.first().ok_or(DecodeError::BufferTooShort {
        need: 1,
        have: 0,
    })?;
    match tag {
        0 => {
            if body.len() < 7 {
                return Err(DecodeError::BufferTooShort {
                    need: 7,
                    have: body.len(),
                });
            }
            let status = WireCallOutcomeStatus::try_from(body[1])?;
            let flags = body[2];
            if flags & !0x03 != 0 {
                return Err(invalid("call outcome flags", flags.into()));
            }
            let result_available = flags & 0x01 != 0;
            let error_details_available = flags & 0x02 != 0;
            let result_unavailable_reason = match body[3] {
                0 => None,
                other => Some(WireResultUnavailableReason::try_from(other)?),
            };
            let raw_error_code = u16::from_le_bytes(body[4..6].try_into().unwrap());
            let error_code = if status == WireCallOutcomeStatus::Failed {
                Some(
                    ErrorCode::try_from(raw_error_code)
                        .map_err(|_| invalid("call outcome error code", raw_error_code.into()))?,
                )
            } else {
                if raw_error_code != 0 {
                    return Err(invalid("call outcome error code", raw_error_code.into()));
                }
                None
            };
            let method_len = body[6] as usize;
            if body.len() != 7 + method_len {
                return Err(invalid("QUERY_CALL_RESULT length", body.len() as u64));
            }
            let method_name = if method_len == 0 {
                None
            } else {
                let s = std::str::from_utf8(&body[7..])
                    .map_err(|_| DecodeError::Utf8Error)?
                    .to_string();
                Some(s)
            };
            let outcome = WireCallOutcome {
                status,
                method_name,
                result_available,
                result_unavailable_reason,
                error_code,
                error_details_available,
            };
            outcome.validate().map_err(|reason| DecodeError::InvalidText {
                field: "QUERY_CALL_RESULT",
                reason: reason.into(),
            })?;
            Ok(WireQueryCallResult::Outcome(outcome))
        }
        1 => {
            let err_bytes = &body[1..];
            if err_bytes.is_empty() {
                return Err(DecodeError::InvalidText {
                    field: "QUERY_CALL_RESULT",
                    reason: "error is empty".into(),
                });
            }
            if err_bytes.len() > MAX_QUERY_CALL_ERROR_BYTES {
                return Err(invalid(
                    "QUERY_CALL_RESULT error length",
                    err_bytes.len() as u64,
                ));
            }
            let error = C2Error::from_wire_bytes(err_bytes)
                .map_err(|err| DecodeError::InvalidText {
                    field: "QUERY_CALL_RESULT",
                    reason: err.to_string(),
                })?
                .ok_or_else(|| DecodeError::InvalidText {
                    field: "QUERY_CALL_RESULT",
                    reason: "error is empty".into(),
                })?;
            Ok(WireQueryCallResult::Error(error))
        }
        other => Err(invalid("QUERY_CALL_RESULT tag", other.into())),
    }
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
    if !matches!(mode, CallMode::Unary | CallMode::ServerStream) {
        return Err(encode_invalid(
            "call mode",
            "encode_open_frame accepts only Unary or ServerStream",
        ));
    }
    let fields = identity_fields(identity);
    let metadata_len = 1 + identity_encoded_len(&fields)? + 1 + 2;
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
    out.push(mode as u8);
    out.extend_from_slice(&method_idx.to_le_bytes());
    if let ChunkPosition::First { total_bytes } = chunk {
        out.extend_from_slice(&total_bytes.to_le_bytes());
    }
    Ok(out)
}

/// Encode a `KeyedUnary` OPEN frame with an explicit nonzero request ID.
pub fn encode_open_keyed_frame<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("OPEN", "invalid request id or continuation"));
    }
    encode_open_keyed_record(request_id, identity, method_idx, call_id, value)
}

/// Encode a complete `KeyedUnary` OPEN draft. The connection's unique writer
/// assigns its RID before publication.
pub fn encode_open_keyed_draft_frame<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    encode_open_keyed_record(0, identity, method_idx, call_id, value)
}

/// Reserve one complete inline `KeyedUnary` OPEN draft before a charged
/// streaming source appends exactly `value_bytes` bytes.
pub fn encode_open_keyed_inline_draft_prefix<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
    chunk: ChunkPosition,
    value_bytes: usize,
) -> Result<Vec<u8>, EncodeError> {
    let (prefix_len, flags) = chunk_layout(chunk, value_bytes as u64)?;
    open_keyed_record_prefix(
        0,
        identity,
        method_idx,
        call_id,
        chunk,
        record_len(prefix_len, value_bytes)?,
        flags,
    )
}

fn encode_open_keyed_record<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
    value: ValuePart<'_>,
) -> Result<Vec<u8>, EncodeError> {
    let (value_len, flags) = value_layout(value)?;
    let mut out = open_keyed_record_prefix(
        request_id,
        identity,
        method_idx,
        call_id,
        value.chunk,
        value_len,
        flags,
    )?;
    write_storage(&mut out, value.storage);
    Ok(out)
}

fn open_keyed_record_prefix<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
    chunk: ChunkPosition,
    value_len: usize,
    flags: u32,
) -> Result<Vec<u8>, EncodeError> {
    if chunk == ChunkPosition::Continuation {
        return Err(encode_invalid("OPEN", "invalid request id or continuation"));
    }
    validate_wire_call_id_encode(call_id)?;
    let fields = identity_fields(identity);
    let metadata_len = 1 + identity_encoded_len(&fields)? + 1 + 2 + 1 + call_id.len();
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
    out.push(CallMode::KeyedUnary as u8);
    out.extend_from_slice(&method_idx.to_le_bytes());
    out.push(call_id.len() as u8);
    out.extend_from_slice(call_id.as_bytes());
    if let ChunkPosition::First { total_bytes } = chunk {
        out.extend_from_slice(&total_bytes.to_le_bytes());
    }
    Ok(out)
}

/// Encode a `Recover` OPEN frame with an explicit nonzero request ID.
pub fn encode_open_recover_frame<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("OPEN", "request id must be nonzero"));
    }
    encode_open_recover_record(request_id, identity, method_idx, call_id)
}

/// Encode a `Recover` OPEN draft. The connection's unique writer assigns its RID.
pub fn encode_open_recover_draft_frame<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    encode_open_recover_record(0, identity, method_idx, call_id)
}

fn encode_open_recover_record<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    method_idx: u16,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    validate_wire_call_id_encode(call_id)?;
    let fields = identity_fields(identity);
    let payload_len = 1 + identity_encoded_len(&fields)? + 1 + 2 + 1 + call_id.len();
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, payload_len)?;
    out.push(RpcRecordKind::Open as u8);
    for field in fields {
        out.push(field.len() as u8);
        out.extend_from_slice(field.as_bytes());
    }
    out.push(CallMode::Recover as u8);
    out.extend_from_slice(&method_idx.to_le_bytes());
    out.push(call_id.len() as u8);
    out.extend_from_slice(call_id.as_bytes());
    Ok(out)
}

/// Encode a `QueryCall` control record with an explicit nonzero request ID.
pub fn encode_query_call_frame<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("QUERY_CALL", "request id must be nonzero"));
    }
    encode_query_call_record(request_id, identity, call_id)
}

/// Encode a `QueryCall` draft; the connection writer assigns its nonzero RID.
pub fn encode_query_call_draft_frame<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    encode_query_call_record(0, identity, call_id)
}

fn encode_query_call_record<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    call_id: &str,
) -> Result<Vec<u8>, EncodeError> {
    validate_wire_call_id_encode(call_id)?;
    let fields = identity_fields(identity);
    let payload_len = 1 + identity_encoded_len(&fields)? + 1 + call_id.len();
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, payload_len)?;
    out.push(RpcRecordKind::QueryCall as u8);
    for field in fields {
        out.push(field.len() as u8);
        out.extend_from_slice(field.as_bytes());
    }
    out.push(call_id.len() as u8);
    out.extend_from_slice(call_id.as_bytes());
    Ok(out)
}

/// Encode a `QueryCallResult` control reply frame.
pub fn encode_query_call_result_frame(
    request_id: u64,
    result: &WireQueryCallResult,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid(
            "QUERY_CALL_RESULT",
            "request id must be nonzero",
        ));
    }
    match result {
        WireQueryCallResult::Outcome(outcome) => {
            outcome
                .validate()
                .map_err(|reason| encode_invalid("QUERY_CALL_RESULT", reason))?;
            let method_bytes = outcome.method_name.as_deref().unwrap_or("").as_bytes();
            let payload_len = 1 + 1 + 1 + 1 + 1 + 2 + 1 + method_bytes.len();
            let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, payload_len)?;
            out.push(RpcRecordKind::QueryCallResult as u8);
            out.push(0); // Outcome tag
            out.push(outcome.status as u8);
            let mut flags = 0u8;
            if outcome.result_available {
                flags |= 0x01;
            }
            if outcome.error_details_available {
                flags |= 0x02;
            }
            out.push(flags);
            out.push(outcome.result_unavailable_reason.map_or(0, |r| r as u8));
            let error_code: u16 = outcome.error_code.map_or(0, u16::from);
            out.extend_from_slice(&error_code.to_le_bytes());
            out.push(method_bytes.len() as u8);
            out.extend_from_slice(method_bytes);
            Ok(out)
        }
        WireQueryCallResult::Error(error) => {
            let err_bytes = bounded_query_call_error_bytes(error);
            let payload_len = record_len(2, err_bytes.len())?;
            let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, payload_len)?;
            out.push(RpcRecordKind::QueryCallResult as u8);
            out.push(1); // Error tag
            out.extend_from_slice(&err_bytes);
            Ok(out)
        }
    }
}

/// Encode a Subscribe OPEN frame with an explicit nonzero request ID.
pub fn encode_open_subscribe_frame<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    topic_name: &str,
    options: WireSubscribeOptions,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("OPEN", "request id must be nonzero"));
    }
    encode_open_subscribe_record(request_id, identity, topic_name, options)
}

/// Encode a complete Subscribe OPEN draft. The connection's unique writer
/// assigns its RID before publication.
pub fn encode_open_subscribe_draft_frame<S: AsRef<str>>(
    identity: &RouteCallIdentity<S>,
    topic_name: &str,
    options: WireSubscribeOptions,
) -> Result<Vec<u8>, EncodeError> {
    encode_open_subscribe_record(0, identity, topic_name, options)
}

fn encode_open_subscribe_record<S: AsRef<str>>(
    request_id: u64,
    identity: &RouteCallIdentity<S>,
    topic_name: &str,
    options: WireSubscribeOptions,
) -> Result<Vec<u8>, EncodeError> {
    c2_contract::validate_contract_text_field("topic_name", topic_name)
        .map_err(|error| encode_invalid("topic_name", &error.to_string()))?;
    if topic_name.len() > u8::MAX as usize {
        return Err(EncodeError::FieldTooLong {
            field: "topic_name",
            max: u8::MAX as usize,
            actual: topic_name.len(),
        });
    }
    let fields = identity_fields(identity);
    let payload_len = 1 + identity_encoded_len(&fields)? + 1 + 1 + topic_name.len() + 1;
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, payload_len)?;
    out.push(RpcRecordKind::Open as u8);
    for field in fields {
        out.push(field.len() as u8);
        out.extend_from_slice(field.as_bytes());
    }
    out.push(CallMode::Subscribe as u8);
    out.push(topic_name.len() as u8);
    out.extend_from_slice(topic_name.as_bytes());
    out.push(options.to_byte());
    Ok(out)
}

/// Largest byte index at or below `index` that is a valid char boundary of
/// `s`.
fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Halve `s` at a valid char boundary and append the truncation marker. The
/// result is always strictly shorter than an input longer than twice the
/// marker; shorter inputs collapse to the empty string, so iteration always
/// makes progress.
fn truncate_at_char_boundary(s: &str) -> String {
    if s.len() <= 2 * CALL_END_TRUNCATION_MARKER.len() {
        return String::new();
    }
    let cut = floor_char_boundary(s, s.len() / 2);
    let mut truncated = String::with_capacity(cut + CALL_END_TRUNCATION_MARKER.len());
    truncated.push_str(&s[..cut]);
    truncated.push_str(CALL_END_TRUNCATION_MARKER);
    truncated
}

fn marked_truncated(mut error: C2Error) -> C2Error {
    error
        .details
        .insert("truncated".to_string(), "true".to_string());
    error
}

/// One bounded-shrinking step: first the largest details value (never the
/// truncation marker itself), then the message. Returns `None` at the floor
/// (empty message, no nonempty details values).
fn truncate_error_once(error: &C2Error) -> Option<C2Error> {
    let mut details = error.details.clone();
    let victim = details
        .iter()
        .filter(|(key, value)| key.as_str() != "truncated" && !value.is_empty())
        .max_by_key(|(_, value)| value.len())
        .map(|(key, _)| key.clone());
    if let Some(key) = victim {
        let shrunk = truncate_at_char_boundary(&details[&key]);
        details.insert(key, shrunk);
        return Some(marked_truncated(C2Error {
            code: error.code,
            message: error.message.clone(),
            details,
        }));
    }
    if !error.message.is_empty() {
        return Some(marked_truncated(C2Error {
            code: error.code,
            message: truncate_at_char_boundary(&error.message),
            details,
        }));
    }
    None
}

/// Encode `error` as C2E1 bytes guaranteed to fit one inline CALL_END frame
/// (total_len <= c2_config::MAX_RPC_FRAME_BYTES). If the canonical encoding
/// already fits, it is returned unchanged. Otherwise the original ErrorCode
/// and name are kept, `message` and `details` values are truncated at valid
/// UTF-8 char boundaries with a truncation marker, and the result is
/// re-encoded as a valid C2E1 envelope (adding details key "truncated" =
/// "true"). Never panics; a pathological input degrades to a minimal valid
/// C2E1 payload.
pub fn bounded_call_end_error_bytes(error: &C2Error) -> Vec<u8> {
    bounded_error_bytes_with_limit(error, MAX_INLINE_CALL_END_ERROR_BYTES)
}

/// Encode `error` as compact C2E1 bytes guaranteed to fit within
/// [`MAX_QUERY_CALL_ERROR_BYTES`] for `QueryCallResult::Error`.
pub fn bounded_query_call_error_bytes(error: &C2Error) -> Vec<u8> {
    bounded_error_bytes_with_limit(error, MAX_QUERY_CALL_ERROR_BYTES)
}

fn bounded_error_bytes_with_limit(error: &C2Error, limit: usize) -> Vec<u8> {
    let mut current = error.clone();
    loop {
        let bytes = current.to_wire_bytes();
        if bytes.len() <= limit {
            return bytes;
        }
        match truncate_error_once(&current) {
            Some(truncated) => current = truncated,
            None => {
                // Floor: an empty message with only the truncation marker is
                // far below the budget and is always a valid C2E1 payload.
                return marked_truncated(C2Error::new(error.code, String::new())).to_wire_bytes();
            }
        }
    }
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
    let has_count = status != CallStatus::NotExecuted
        && matches!(mode, CallMode::ServerStream | CallMode::Subscribe);
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

/// Convenience: encode a complete inline CALL_END error frame that is always
/// within the frame budget (uses [`bounded_call_end_error_bytes`]).
pub fn encode_call_end_error_frame(
    request_id: u64,
    mode: CallMode,
    status: CallStatus,
    complete_items: u64,
    error: &C2Error,
) -> Result<Vec<u8>, EncodeError> {
    let payload = bounded_call_end_error_bytes(error);
    encode_call_end_frame(
        request_id,
        mode,
        status,
        complete_items,
        ValuePart::whole(ValueStorage::Inline(&payload)),
    )
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

pub fn encode_ready_frame(request_id: u64, start_seq: u64) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 || start_seq == 0 {
        return Err(encode_invalid(
            "READY",
            "request id and start_seq must be nonzero",
        ));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, 9)?;
    out.push(RpcRecordKind::Ready as u8);
    out.extend_from_slice(&start_seq.to_le_bytes());
    Ok(out)
}

/// Request the current head of an active subscription's source topic.
pub fn encode_sync_frame(request_id: u64) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("SYNC", "request id must be nonzero"));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD, 1)?;
    out.push(RpcRecordKind::Sync as u8);
    Ok(out)
}

/// Acknowledge SYNC. An empty topic has head sequence zero.
pub fn encode_sync_ack_frame(request_id: u64, head_seq: u64) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 {
        return Err(encode_invalid("SYNC_ACK", "request id must be nonzero"));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, 9)?;
    out.push(RpcRecordKind::SyncAck as u8);
    out.extend_from_slice(&head_seq.to_le_bytes());
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

pub fn encode_subscribe_item_begin_frame(
    request_id: u64,
    seq: u64,
    total_bytes: u64,
) -> Result<Vec<u8>, EncodeError> {
    if request_id == 0 || seq == 0 {
        return Err(encode_invalid(
            "ITEM_BEGIN",
            "request id and seq must be nonzero",
        ));
    }
    let mut out = record_frame(request_id, FLAG_RPC_RECORD | FLAG_RESPONSE, 17)?;
    out.push(RpcRecordKind::ItemBegin as u8);
    out.extend_from_slice(&seq.to_le_bytes());
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
