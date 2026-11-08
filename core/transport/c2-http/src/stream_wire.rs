//! The version 1 HTTP response records for one server-stream call.

use c2_error::C2Error;
use std::io::{self, Write};

pub use c2_wire::rpc::MAX_CALL_END_PAYLOAD_BYTES as MAX_TERMINAL_PAYLOAD_BYTES;

pub const MEDIA_TYPE: &str = "application/vnd.c-two.server-stream;version=1";
pub const RECORD_HEADER_LEN: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HttpStreamRecordKind {
    ItemBegin = 1,
    CallEnd = 2,
    Failure = 3,
    ItemData = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HttpStreamStatus {
    Success = 0,
    Failed = 1,
    NotExecuted = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStreamTerminal {
    pub status: HttpStreamStatus,
    pub complete_items: Option<u64>,
    pub error: Option<C2Error>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HttpStreamWireError {
    #[error("unknown HTTP stream record kind {0}")]
    UnknownRecordKind(u8),
    #[error("invalid HTTP stream terminal: {0}")]
    InvalidTerminal(&'static str),
    #[error("invalid HTTP stream error: {0}")]
    InvalidError(String),
    #[error("HTTP stream FAILURE requires dispatch_phase=dispatch_uncertain")]
    InvalidFailurePhase,
    #[error("HTTP stream record length does not fit u64")]
    LengthOverflow,
    #[error(
        "HTTP terminal payload exceeds IPC CALL_END limit: {length} > {MAX_TERMINAL_PAYLOAD_BYTES}"
    )]
    TerminalTooLarge { length: u64 },
    #[error("HTTP ITEM_BEGIN payload must be exactly eight bytes")]
    InvalidItemBeginLength,
    #[error("HTTP ITEM_DATA payload must be nonempty")]
    EmptyItemData,
}

/// The length describes only the record payload. The reader checks its available
/// bytes and storage budget before accepting an ITEM_DATA payload.
pub fn encode_header(kind: HttpStreamRecordKind, payload_len: u64) -> [u8; RECORD_HEADER_LEN] {
    let mut header = [0; RECORD_HEADER_LEN];
    header[0] = kind as u8;
    header[1..].copy_from_slice(&payload_len.to_le_bytes());
    header
}

pub fn decode_header(
    header: [u8; RECORD_HEADER_LEN],
) -> Result<(HttpStreamRecordKind, u64), HttpStreamWireError> {
    let kind = match header[0] {
        1 => HttpStreamRecordKind::ItemBegin,
        2 => HttpStreamRecordKind::CallEnd,
        3 => HttpStreamRecordKind::Failure,
        4 => HttpStreamRecordKind::ItemData,
        other => return Err(HttpStreamWireError::UnknownRecordKind(other)),
    };
    let length = u64::from_le_bytes(header[1..].try_into().expect("fixed HTTP stream header"));
    match kind {
        HttpStreamRecordKind::ItemBegin if length != 8 => {
            return Err(HttpStreamWireError::InvalidItemBeginLength);
        }
        HttpStreamRecordKind::ItemData if length == 0 => {
            return Err(HttpStreamWireError::EmptyItemData);
        }
        HttpStreamRecordKind::CallEnd | HttpStreamRecordKind::Failure => {
            check_terminal_length(length)?;
        }
        _ => {}
    }
    Ok((kind, length))
}

/// Start one item; zero bytes completes it without any ITEM_DATA record.
pub fn encode_item_begin(total_bytes: u64) -> [u8; RECORD_HEADER_LEN + 8] {
    let mut bytes = [0; RECORD_HEADER_LEN + 8];
    bytes[..RECORD_HEADER_LEN].copy_from_slice(&encode_header(HttpStreamRecordKind::ItemBegin, 8));
    bytes[RECORD_HEADER_LEN..].copy_from_slice(&total_bytes.to_le_bytes());
    bytes
}

/// Decode the fixed payload of an ITEM_BEGIN record.
pub fn decode_item_begin(payload: [u8; 8]) -> u64 {
    u64::from_le_bytes(payload)
}

fn check_terminal_length(length: u64) -> Result<(), HttpStreamWireError> {
    if length > MAX_TERMINAL_PAYLOAD_BYTES {
        return Err(HttpStreamWireError::TerminalTooLarge { length });
    }
    Ok(())
}

const ERROR_MAGIC: &[u8; 4] = b"C2E1";

#[derive(Default)]
struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("HTTP stream record length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ComparingWriter<'a> {
    expected: &'a [u8],
    position: usize,
    matches: bool,
}

impl Write for ComparingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self.position.saturating_add(bytes.len());
        self.matches &= self.expected.get(self.position..end) == Some(bytes);
        self.position = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn error_wire_len(error: &C2Error) -> Result<usize, HttpStreamWireError> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, &error.envelope())
        .map_err(|_| HttpStreamWireError::LengthOverflow)?;
    ERROR_MAGIC
        .len()
        .checked_add(writer.0)
        .ok_or(HttpStreamWireError::LengthOverflow)
}

fn record_len(payload_len: usize) -> Result<usize, HttpStreamWireError> {
    check_terminal_length(
        u64::try_from(payload_len).map_err(|_| HttpStreamWireError::LengthOverflow)?,
    )?;
    RECORD_HEADER_LEN
        .checked_add(payload_len)
        .ok_or(HttpStreamWireError::LengthOverflow)
}

fn write_error(record: &mut Vec<u8>, error: &C2Error) {
    record.extend_from_slice(ERROR_MAGIC);
    serde_json::to_writer(record, &error.envelope())
        .expect("C2 error envelope serialization failed");
}

enum PreparedTerminal<'a> {
    Success(u64),
    Failed(u64, C2Error),
    NotExecuted(&'a C2Error),
}

impl PreparedTerminal<'_> {
    fn payload_len(&self) -> Result<usize, HttpStreamWireError> {
        match self {
            Self::Success(_) => Ok(1 + 8),
            Self::Failed(_, error) => (1_usize + 8)
                .checked_add(error_wire_len(error)?)
                .ok_or(HttpStreamWireError::LengthOverflow),
            Self::NotExecuted(error) => 1_usize
                .checked_add(error_wire_len(error)?)
                .ok_or(HttpStreamWireError::LengthOverflow),
        }
    }
}

fn prepare_terminal(
    terminal: &HttpStreamTerminal,
) -> Result<PreparedTerminal<'_>, HttpStreamWireError> {
    match terminal.status {
        HttpStreamStatus::Success => {
            let count = terminal
                .complete_items
                .ok_or(HttpStreamWireError::InvalidTerminal(
                    "Success requires a complete-item count",
                ))?;
            if terminal.error.is_some() {
                return Err(HttpStreamWireError::InvalidTerminal(
                    "Success cannot carry an error",
                ));
            }
            Ok(PreparedTerminal::Success(count))
        }
        HttpStreamStatus::Failed => {
            let count = terminal
                .complete_items
                .ok_or(HttpStreamWireError::InvalidTerminal(
                    "Failed requires a complete-item count",
                ))?;
            let mut error = terminal
                .error
                .clone()
                .ok_or(HttpStreamWireError::InvalidTerminal(
                    "Failed requires a C2E1 error",
                ))?;
            // A resource outcome does not inherit a nested call's transport phase.
            error.details.remove("transport_phase");
            error.details.remove("dispatch_phase");
            Ok(PreparedTerminal::Failed(count, error))
        }
        HttpStreamStatus::NotExecuted => {
            if terminal.complete_items.is_some() {
                return Err(HttpStreamWireError::InvalidTerminal(
                    "NotExecuted cannot carry a complete-item count",
                ));
            }
            let error = terminal
                .error
                .as_ref()
                .ok_or(HttpStreamWireError::InvalidTerminal(
                    "NotExecuted requires a C2E1 error",
                ))?;
            Ok(PreparedTerminal::NotExecuted(error))
        }
    }
}

fn decode_error(bytes: &[u8]) -> Result<C2Error, HttpStreamWireError> {
    if bytes.is_empty() {
        return Err(HttpStreamWireError::InvalidError(
            "empty C2E1 payload".into(),
        ));
    }
    let error = C2Error::from_wire_bytes(bytes)
        .map_err(|error| HttpStreamWireError::InvalidError(error.to_string()))?
        .ok_or_else(|| HttpStreamWireError::InvalidError("empty C2E1 payload".into()))?;
    let mut writer = ComparingWriter {
        expected: &bytes[ERROR_MAGIC.len()..],
        position: 0,
        matches: true,
    };
    serde_json::to_writer(&mut writer, &error.envelope())
        .expect("C2 error envelope serialization failed");
    if !writer.matches || writer.position != writer.expected.len() {
        return Err(HttpStreamWireError::InvalidError(
            "noncanonical C2E1 payload".into(),
        ));
    }
    Ok(error)
}

/// Encodes a terminal CALL_END, including the record header.
pub fn encode_terminal(terminal: &HttpStreamTerminal) -> Result<Vec<u8>, HttpStreamWireError> {
    let prepared = prepare_terminal(terminal)?;
    let payload_len = prepared.payload_len()?;
    let capacity = record_len(payload_len)?;
    let mut record = Vec::with_capacity(capacity);
    record.extend_from_slice(&encode_header(
        HttpStreamRecordKind::CallEnd,
        payload_len as u64,
    ));
    record.push(terminal.status as u8);
    match prepared {
        PreparedTerminal::Success(count) => record.extend_from_slice(&count.to_le_bytes()),
        PreparedTerminal::Failed(count, error) => {
            record.extend_from_slice(&count.to_le_bytes());
            write_error(&mut record, &error);
        }
        PreparedTerminal::NotExecuted(error) => write_error(&mut record, error),
    }
    debug_assert_eq!(record.len(), capacity);
    Ok(record)
}

/// Counts the complete CALL_END record without allocating its output bytes.
pub fn terminal_record_len(terminal: &HttpStreamTerminal) -> Result<usize, HttpStreamWireError> {
    record_len(prepare_terminal(terminal)?.payload_len()?)
}

/// Decodes one CALL_END payload after the reader has checked its record length.
pub fn decode_terminal(payload: &[u8]) -> Result<HttpStreamTerminal, HttpStreamWireError> {
    let (&status, rest) = payload
        .split_first()
        .ok_or(HttpStreamWireError::InvalidTerminal("missing status"))?;
    match status {
        0 => {
            let count: [u8; 8] = rest.try_into().map_err(|_| {
                HttpStreamWireError::InvalidTerminal("Success requires exactly eight count bytes")
            })?;
            Ok(HttpStreamTerminal {
                status: HttpStreamStatus::Success,
                complete_items: Some(u64::from_le_bytes(count)),
                error: None,
            })
        }
        1 => {
            let (count, error) =
                rest.split_first_chunk::<8>()
                    .ok_or(HttpStreamWireError::InvalidTerminal(
                        "Failed requires eight count bytes",
                    ))?;
            let error = decode_error(error)?;
            if error.details.contains_key("transport_phase")
                || error.details.contains_key("dispatch_phase")
            {
                return Err(HttpStreamWireError::InvalidTerminal(
                    "Failed resource error carries a transport phase",
                ));
            }
            Ok(HttpStreamTerminal {
                status: HttpStreamStatus::Failed,
                complete_items: Some(u64::from_le_bytes(*count)),
                error: Some(error),
            })
        }
        2 => Ok(HttpStreamTerminal {
            status: HttpStreamStatus::NotExecuted,
            complete_items: None,
            error: Some(decode_error(rest)?),
        }),
        _ => Err(HttpStreamWireError::InvalidTerminal("unknown status")),
    }
}

/// Encodes a transport failure with unknown dispatch outcome. No item count is
/// represented in this record.
pub fn encode_failure(error: &C2Error) -> Result<Vec<u8>, HttpStreamWireError> {
    let capacity = failure_record_len(error)?;
    let payload_len = capacity - RECORD_HEADER_LEN;
    let mut record = Vec::with_capacity(capacity);
    record.extend_from_slice(&encode_header(
        HttpStreamRecordKind::Failure,
        payload_len as u64,
    ));
    write_error(&mut record, error);
    debug_assert_eq!(record.len(), capacity);
    Ok(record)
}

/// Counts the complete FAILURE record without allocating its output bytes.
pub fn failure_record_len(error: &C2Error) -> Result<usize, HttpStreamWireError> {
    if error.details.get("dispatch_phase").map(String::as_str) != Some("dispatch_uncertain") {
        return Err(HttpStreamWireError::InvalidFailurePhase);
    }
    record_len(error_wire_len(error)?)
}

pub fn decode_failure(payload: &[u8]) -> Result<C2Error, HttpStreamWireError> {
    let error = decode_error(payload)?;
    if error.details.get("dispatch_phase").map(String::as_str) != Some("dispatch_uncertain") {
        return Err(HttpStreamWireError::InvalidFailurePhase);
    }
    Ok(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_error::ErrorCode;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    fn error(code: ErrorCode, phase: Option<&str>) -> C2Error {
        let mut details = BTreeMap::new();
        if let Some(phase) = phase {
            details.insert("dispatch_phase".into(), phase.into());
        }
        C2Error::new(code, "stream ended").with_details(details)
    }

    #[test]
    fn headers_have_fixed_little_endian_layout_and_reject_unknown_kind() {
        let header = encode_header(HttpStreamRecordKind::ItemData, 0x0102_0304_0506_0708);
        assert_eq!(header, [4, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(
            decode_header(header).unwrap(),
            (HttpStreamRecordKind::ItemData, 0x0102_0304_0506_0708)
        );
        assert_eq!(
            decode_header([0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(HttpStreamWireError::UnknownRecordKind(0))
        );
    }

    #[test]
    fn terminal_payload_limit_matches_ipc_units_and_checks_header_before_allocation() {
        use c2_wire::rpc::{CallMode, CallStatus, ValuePart, ValueStorage};

        let error = error(ErrorCode::CallCancelled, None);
        for (status, ipc_status, count) in [
            (HttpStreamStatus::Success, CallStatus::Success, Some(3)),
            (HttpStreamStatus::Failed, CallStatus::Failed, Some(3)),
            (HttpStreamStatus::NotExecuted, CallStatus::NotExecuted, None),
        ] {
            let terminal = HttpStreamTerminal {
                status,
                complete_items: count,
                error: (status != HttpStreamStatus::Success).then(|| error.clone()),
            };
            let http = encode_terminal(&terminal).unwrap();
            let error_wire = terminal
                .error
                .as_ref()
                .map(C2Error::to_wire_bytes)
                .unwrap_or_default();
            let ipc = c2_wire::rpc::encode_call_end_frame(
                1,
                CallMode::ServerStream,
                ipc_status,
                count.unwrap_or(0),
                ValuePart::whole(ValueStorage::Inline(&error_wire)),
            )
            .unwrap();
            let (header, _) = c2_wire::frame::decode_frame(&ipc).unwrap();
            assert_eq!(
                MAX_TERMINAL_PAYLOAD_BYTES - (http.len() - RECORD_HEADER_LEN) as u64,
                u64::from(u32::MAX - header.total_len)
            );
        }
        for kind in [HttpStreamRecordKind::CallEnd, HttpStreamRecordKind::Failure] {
            assert_eq!(
                decode_header(encode_header(kind, MAX_TERMINAL_PAYLOAD_BYTES)).unwrap(),
                (kind, MAX_TERMINAL_PAYLOAD_BYTES)
            );
            assert_eq!(
                decode_header(encode_header(kind, MAX_TERMINAL_PAYLOAD_BYTES + 1)),
                Err(HttpStreamWireError::TerminalTooLarge {
                    length: MAX_TERMINAL_PAYLOAD_BYTES + 1,
                })
            );
        }
    }

    #[test]
    fn terminal_layouts_and_resource_phase_scrubbing() {
        for terminal in [
            HttpStreamTerminal {
                status: HttpStreamStatus::Success,
                complete_items: Some(3),
                error: None,
            },
            HttpStreamTerminal {
                status: HttpStreamStatus::Failed,
                complete_items: Some(2),
                error: Some(error(ErrorCode::CallCancelled, None)),
            },
            HttpStreamTerminal {
                status: HttpStreamStatus::NotExecuted,
                complete_items: None,
                error: Some(error(ErrorCode::ResourceRemoved, Some("pre_dispatch"))),
            },
        ] {
            let record = encode_terminal(&terminal).unwrap();
            assert_eq!(terminal_record_len(&terminal).unwrap(), record.len());
            let header: [u8; RECORD_HEADER_LEN] = record[..RECORD_HEADER_LEN].try_into().unwrap();
            assert_eq!(
                decode_header(header).unwrap().0,
                HttpStreamRecordKind::CallEnd
            );
            assert_eq!(
                decode_header(header).unwrap().1 as usize,
                record.len() - RECORD_HEADER_LEN
            );
            assert_eq!(
                decode_terminal(&record[RECORD_HEADER_LEN..]).unwrap(),
                terminal
            );
        }

        let mut inner = error(ErrorCode::ResourceFunctionExecuting, Some("pre_dispatch"));
        inner
            .details
            .insert("transport_phase".into(), "pre_dispatch".into());
        let terminal = HttpStreamTerminal {
            status: HttpStreamStatus::Failed,
            complete_items: Some(1),
            error: Some(inner),
        };
        let record = encode_terminal(&terminal).unwrap();
        assert_eq!(terminal_record_len(&terminal).unwrap(), record.len());
        let decoded = decode_terminal(&record[RECORD_HEADER_LEN..]).unwrap();
        assert!(
            !decoded
                .error
                .unwrap()
                .details
                .contains_key("dispatch_phase")
        );
    }

    #[test]
    fn terminal_rejects_missing_extra_and_malformed_fields() {
        for bytes in [
            vec![],
            vec![0],
            vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 9],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![2],
            vec![2, 0],
            vec![3],
        ] {
            assert!(decode_terminal(&bytes).is_err(), "accepted {bytes:?}");
        }
        assert!(
            encode_terminal(&HttpStreamTerminal {
                status: HttpStreamStatus::Success,
                complete_items: None,
                error: None,
            })
            .is_err()
        );
        let invalid = HttpStreamTerminal {
            status: HttpStreamStatus::Failed,
            complete_items: None,
            error: Some(error(ErrorCode::CallCancelled, None)),
        };
        assert_eq!(
            terminal_record_len(&invalid),
            encode_terminal(&invalid).map(|record| record.len())
        );
        assert!(
            encode_terminal(&HttpStreamTerminal {
                status: HttpStreamStatus::NotExecuted,
                complete_items: Some(0),
                error: Some(error(ErrorCode::CallCancelled, None)),
            })
            .is_err()
        );
    }

    #[test]
    fn failure_has_uncertain_phase_and_no_count() {
        let uncertain = error(ErrorCode::ClientCallingResource, Some("dispatch_uncertain"));
        let record = encode_failure(&uncertain).unwrap();
        assert_eq!(failure_record_len(&uncertain).unwrap(), record.len());
        let header: [u8; RECORD_HEADER_LEN] = record[..RECORD_HEADER_LEN].try_into().unwrap();
        assert_eq!(
            decode_header(header).unwrap().0,
            HttpStreamRecordKind::Failure
        );
        assert_eq!(
            decode_failure(&record[RECORD_HEADER_LEN..]).unwrap(),
            uncertain
        );
        assert_eq!(
            encode_failure(&error(
                ErrorCode::ClientCallingResource,
                Some("pre_dispatch")
            )),
            Err(HttpStreamWireError::InvalidFailurePhase)
        );
        assert!(decode_failure(b"C2E1{}").is_err());
        let mut noncanonical = record[RECORD_HEADER_LEN..].to_vec();
        noncanonical.push(b' ');
        assert_eq!(
            decode_failure(&noncanonical),
            Err(HttpStreamWireError::InvalidError(
                "noncanonical C2E1 payload".into()
            ))
        );
        assert_eq!(
            failure_record_len(&error(
                ErrorCode::ClientCallingResource,
                Some("pre_dispatch")
            )),
            Err(HttpStreamWireError::InvalidFailurePhase)
        );
    }

    #[derive(Deserialize)]
    struct Fixture {
        positive: Vec<Positive>,
        negative: Vec<Negative>,
    }
    #[derive(Deserialize)]
    struct Positive {
        name: String,
        record_hex: String,
        kind: u8,
        payload_hex: String,
    }
    #[derive(Deserialize)]
    struct Negative {
        name: String,
        record_hex: String,
    }

    fn unhex(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0);
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn shared_fixture_matches_wire_codec() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../tests/fixtures/stream-format-v1.json")).unwrap();
        for row in fixture.positive {
            let record = unhex(&row.record_hex);
            let header: [u8; RECORD_HEADER_LEN] = record[..RECORD_HEADER_LEN].try_into().unwrap();
            let (kind, length) = decode_header(header).unwrap();
            assert_eq!(kind as u8, row.kind, "{}", row.name);
            assert_eq!(
                length as usize,
                record.len() - RECORD_HEADER_LEN,
                "{}",
                row.name
            );
            assert_eq!(
                &record[RECORD_HEADER_LEN..],
                unhex(&row.payload_hex),
                "{}",
                row.name
            );
            match kind {
                HttpStreamRecordKind::ItemBegin | HttpStreamRecordKind::ItemData => {}
                HttpStreamRecordKind::CallEnd => {
                    let value = decode_terminal(&record[RECORD_HEADER_LEN..]).unwrap();
                    assert_eq!(terminal_record_len(&value).unwrap(), record.len());
                    assert_eq!(encode_terminal(&value).unwrap(), record, "{}", row.name);
                }
                HttpStreamRecordKind::Failure => {
                    let value = decode_failure(&record[RECORD_HEADER_LEN..]).unwrap();
                    assert_eq!(failure_record_len(&value).unwrap(), record.len());
                    assert_eq!(encode_failure(&value).unwrap(), record, "{}", row.name);
                }
            }
        }
        for row in fixture.negative {
            let record = unhex(&row.record_hex);
            let header: [u8; RECORD_HEADER_LEN] = record[..RECORD_HEADER_LEN].try_into().unwrap();
            let bad = match decode_header(header) {
                Err(_) => true,
                Ok((_, length)) if length != (record.len() - RECORD_HEADER_LEN) as u64 => true,
                Ok((HttpStreamRecordKind::ItemBegin | HttpStreamRecordKind::ItemData, _)) => false,
                Ok((HttpStreamRecordKind::CallEnd, _)) => {
                    decode_terminal(&record[RECORD_HEADER_LEN..]).is_err()
                }
                Ok((HttpStreamRecordKind::Failure, _)) => {
                    decode_failure(&record[RECORD_HEADER_LEN..]).is_err()
                }
            };
            assert!(bad, "accepted {}", row.name);
        }
    }
}
