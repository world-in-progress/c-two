//! HTTP topic subscription records. All integer fields are little-endian.

use c2_error::C2Error;

pub const MEDIA_TYPE: &str = "application/vnd.c-two.topic-stream;version=1";
pub const RECORD_HEADER_LEN: usize = 9;
pub const MAX_TERMINAL_PAYLOAD_BYTES: u64 = c2_wire::rpc::MAX_CALL_END_PAYLOAD_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    Ready = 1,
    ItemBegin = 2,
    ItemData = 3,
    Terminal = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Failed = 1,
    NotExecuted = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub status: Status,
    pub complete_items: u64,
    pub error: C2Error,
}

pub fn header(kind: RecordKind, len: u64) -> [u8; RECORD_HEADER_LEN] {
    let mut out = [0; RECORD_HEADER_LEN];
    out[0] = kind as u8;
    out[1..].copy_from_slice(&len.to_le_bytes());
    out
}

pub fn decode_header(bytes: [u8; RECORD_HEADER_LEN]) -> Result<(RecordKind, u64), &'static str> {
    let len = u64::from_le_bytes(bytes[1..].try_into().unwrap());
    let kind = match bytes[0] {
        1 if len == 8 => RecordKind::Ready,
        2 if len == 16 => RecordKind::ItemBegin,
        3 if len > 0 => RecordKind::ItemData,
        4 if (14..=MAX_TERMINAL_PAYLOAD_BYTES).contains(&len) => RecordKind::Terminal,
        _ => return Err("invalid topic record kind or length"),
    };
    Ok((kind, len))
}

pub fn ready(start_seq: u64) -> Result<Vec<u8>, &'static str> {
    if start_seq == 0 {
        return Err("zero start sequence");
    }
    let mut out = header(RecordKind::Ready, 8).to_vec();
    out.extend_from_slice(&start_seq.to_le_bytes());
    Ok(out)
}

pub fn item_begin(seq: u64, len: u64) -> Result<Vec<u8>, &'static str> {
    if seq == 0 {
        return Err("zero item sequence");
    }
    let mut out = header(RecordKind::ItemBegin, 16).to_vec();
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    Ok(out)
}

pub fn terminal(value: &Terminal) -> Vec<u8> {
    let error = c2_wire::rpc::bounded_call_end_error_bytes(&value.error);
    let mut out = header(RecordKind::Terminal, (9 + error.len()) as u64).to_vec();
    out.push(value.status as u8);
    out.extend_from_slice(&value.complete_items.to_le_bytes());
    out.extend_from_slice(&error);
    out
}

pub fn decode_terminal(payload: &[u8]) -> Result<Terminal, &'static str> {
    if payload.len() < 14 || payload.len() as u64 > MAX_TERMINAL_PAYLOAD_BYTES {
        return Err("invalid terminal length");
    }
    let status = match payload[0] {
        1 => Status::Failed,
        2 => Status::NotExecuted,
        _ => return Err("invalid terminal status"),
    };
    let complete_items = u64::from_le_bytes(payload[1..9].try_into().unwrap());
    if status == Status::NotExecuted && complete_items != 0 {
        return Err("not-executed terminal has items");
    }
    let wire = &payload[9..];
    let error = C2Error::from_wire_bytes(wire)
        .map_err(|_| "invalid C2E1 error")?
        .ok_or("missing C2E1 error")?;
    if error.to_wire_bytes() != wire {
        return Err("noncanonical C2E1 error");
    }
    Ok(Terminal {
        status,
        complete_items,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_roundtrip_and_reject_invalid_shapes() {
        let r = ready(1).unwrap();
        assert_eq!(
            decode_header(r[..9].try_into().unwrap()),
            Ok((RecordKind::Ready, 8))
        );
        assert!(ready(0).is_err());
        assert!(item_begin(0, 1).is_err());
        assert!(decode_header(header(RecordKind::ItemData, 0)).is_err());
        let value = Terminal {
            status: Status::Failed,
            complete_items: 5,
            error: C2Error::new(c2_error::ErrorCode::CallCancelled, "cancelled"),
        };
        let encoded = terminal(&value);
        assert_eq!(decode_terminal(&encoded[9..]).unwrap(), value);
    }
}
