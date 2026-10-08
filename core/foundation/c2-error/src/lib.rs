use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

const ERROR_WIRE_MAGIC: &[u8; 4] = b"C2E1";
pub const ERROR_WIRE_VERSION: u16 = 1;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Unknown = 0,
    ResourceInputDeserializing = 1,
    ResourceOutputSerializing = 2,
    ResourceFunctionExecuting = 3,
    ResourceInputFromBuffer = 4,
    ClientInputSerializing = 5,
    ClientOutputDeserializing = 6,
    ClientCallingResource = 7,
    ClientOutputFromBuffer = 8,
    ResourceNotFound = 701,
    ResourceUnavailable = 702,
    ResourceAlreadyRegistered = 703,
    RouteStale = 704,
    RegistryUnavailable = 705,
    WriteConflict = 706,
    ResourceClosed = 707,
    ResourceRemoved = 708,
    ContractMismatch = 709,
    IdentityMismatch = 710,
    RouteCatalogCompacted = 711,
    RouteWatchUnavailable = 712,
    ProtocolViolation = 713,
    FallbackDenied = 714,
    CallDeadlineExceeded = 715,
    UnsupportedCallMode = 716,
    CallCapacityExceeded = 717,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorCodeEntry {
    pub code: ErrorCode,
    pub name: &'static str,
}

const ERROR_CODE_REGISTRY: &[ErrorCodeEntry] = &[
    ErrorCodeEntry {
        code: ErrorCode::Unknown,
        name: "Unknown",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceInputDeserializing,
        name: "ResourceInputDeserializing",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceOutputSerializing,
        name: "ResourceOutputSerializing",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceFunctionExecuting,
        name: "ResourceFunctionExecuting",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceInputFromBuffer,
        name: "ResourceInputFromBuffer",
    },
    ErrorCodeEntry {
        code: ErrorCode::ClientInputSerializing,
        name: "ClientInputSerializing",
    },
    ErrorCodeEntry {
        code: ErrorCode::ClientOutputDeserializing,
        name: "ClientOutputDeserializing",
    },
    ErrorCodeEntry {
        code: ErrorCode::ClientCallingResource,
        name: "ClientCallingResource",
    },
    ErrorCodeEntry {
        code: ErrorCode::ClientOutputFromBuffer,
        name: "ClientOutputFromBuffer",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceNotFound,
        name: "ResourceNotFound",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceUnavailable,
        name: "ResourceUnavailable",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceAlreadyRegistered,
        name: "ResourceAlreadyRegistered",
    },
    ErrorCodeEntry {
        code: ErrorCode::RouteStale,
        name: "RouteStale",
    },
    ErrorCodeEntry {
        code: ErrorCode::RegistryUnavailable,
        name: "RegistryUnavailable",
    },
    ErrorCodeEntry {
        code: ErrorCode::WriteConflict,
        name: "WriteConflict",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceClosed,
        name: "ResourceClosed",
    },
    ErrorCodeEntry {
        code: ErrorCode::ResourceRemoved,
        name: "ResourceRemoved",
    },
    ErrorCodeEntry {
        code: ErrorCode::ContractMismatch,
        name: "ContractMismatch",
    },
    ErrorCodeEntry {
        code: ErrorCode::IdentityMismatch,
        name: "IdentityMismatch",
    },
    ErrorCodeEntry {
        code: ErrorCode::RouteCatalogCompacted,
        name: "RouteCatalogCompacted",
    },
    ErrorCodeEntry {
        code: ErrorCode::RouteWatchUnavailable,
        name: "RouteWatchUnavailable",
    },
    ErrorCodeEntry {
        code: ErrorCode::ProtocolViolation,
        name: "ProtocolViolation",
    },
    ErrorCodeEntry {
        code: ErrorCode::FallbackDenied,
        name: "FallbackDenied",
    },
    ErrorCodeEntry {
        code: ErrorCode::CallDeadlineExceeded,
        name: "CallDeadlineExceeded",
    },
    ErrorCodeEntry {
        code: ErrorCode::UnsupportedCallMode,
        name: "UnsupportedCallMode",
    },
    ErrorCodeEntry {
        code: ErrorCode::CallCapacityExceeded,
        name: "CallCapacityExceeded",
    },
];

impl ErrorCode {
    pub fn name(self) -> &'static str {
        ERROR_CODE_REGISTRY
            .iter()
            .find(|entry| entry.code == self)
            .map(|entry| entry.name)
            .expect("c2-error registry must include every ErrorCode variant")
    }

    pub fn registry() -> &'static [ErrorCodeEntry] {
        ERROR_CODE_REGISTRY
    }
}

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> Self {
        code as u16
    }
}

impl TryFrom<u16> for ErrorCode {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ErrorCode::Unknown),
            1 => Ok(ErrorCode::ResourceInputDeserializing),
            2 => Ok(ErrorCode::ResourceOutputSerializing),
            3 => Ok(ErrorCode::ResourceFunctionExecuting),
            4 => Ok(ErrorCode::ResourceInputFromBuffer),
            5 => Ok(ErrorCode::ClientInputSerializing),
            6 => Ok(ErrorCode::ClientOutputDeserializing),
            7 => Ok(ErrorCode::ClientCallingResource),
            8 => Ok(ErrorCode::ClientOutputFromBuffer),
            701 => Ok(ErrorCode::ResourceNotFound),
            702 => Ok(ErrorCode::ResourceUnavailable),
            703 => Ok(ErrorCode::ResourceAlreadyRegistered),
            704 => Ok(ErrorCode::RouteStale),
            705 => Ok(ErrorCode::RegistryUnavailable),
            706 => Ok(ErrorCode::WriteConflict),
            707 => Ok(ErrorCode::ResourceClosed),
            708 => Ok(ErrorCode::ResourceRemoved),
            709 => Ok(ErrorCode::ContractMismatch),
            710 => Ok(ErrorCode::IdentityMismatch),
            711 => Ok(ErrorCode::RouteCatalogCompacted),
            712 => Ok(ErrorCode::RouteWatchUnavailable),
            713 => Ok(ErrorCode::ProtocolViolation),
            714 => Ok(ErrorCode::FallbackDenied),
            715 => Ok(ErrorCode::CallDeadlineExceeded),
            716 => Ok(ErrorCode::UnsupportedCallMode),
            717 => Ok(ErrorCode::CallCapacityExceeded),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2Error {
    pub code: ErrorCode,
    pub message: String,
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C2ErrorDecodeError {
    MissingMagic,
    InvalidJson(String),
    UnsupportedVersion(u16),
    CodeNameMismatch {
        code: u16,
        expected: &'static str,
        actual: String,
    },
}

impl fmt::Display for C2ErrorDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            C2ErrorDecodeError::MissingMagic => {
                f.write_str("invalid C2 error wire payload: missing C2E1 envelope magic")
            }
            C2ErrorDecodeError::InvalidJson(err) => {
                write!(f, "invalid C2 error wire JSON envelope: {err}")
            }
            C2ErrorDecodeError::UnsupportedVersion(version) => {
                write!(f, "unsupported C2 error wire version: {version}")
            }
            C2ErrorDecodeError::CodeNameMismatch {
                code,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "invalid C2 error wire name for code {code}: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for C2ErrorDecodeError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct C2ErrorEnvelope {
    pub version: u16,
    pub code: u16,
    pub name: String,
    pub message: String,
    pub details: BTreeMap<String, String>,
}

impl C2Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: BTreeMap::new(),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unknown, message)
    }

    pub fn with_details(mut self, details: BTreeMap<String, String>) -> Self {
        self.details = details;
        self
    }

    pub fn envelope(&self) -> C2ErrorEnvelope {
        C2ErrorEnvelope {
            version: ERROR_WIRE_VERSION,
            code: u16::from(self.code),
            name: self.code.name().to_string(),
            message: self.message.clone(),
            details: self.details.clone(),
        }
    }

    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut out = Vec::from(ERROR_WIRE_MAGIC.as_slice());
        let envelope =
            serde_json::to_vec(&self.envelope()).expect("C2 error envelope serialization failed");
        out.extend_from_slice(&envelope);
        out
    }

    pub fn from_envelope(envelope: C2ErrorEnvelope) -> Result<Self, C2ErrorDecodeError> {
        if envelope.version != ERROR_WIRE_VERSION {
            return Err(C2ErrorDecodeError::UnsupportedVersion(envelope.version));
        }

        match ErrorCode::try_from(envelope.code) {
            Ok(code) => {
                if envelope.name != code.name() {
                    return Err(C2ErrorDecodeError::CodeNameMismatch {
                        code: envelope.code,
                        expected: code.name(),
                        actual: envelope.name,
                    });
                }
                Ok(C2Error {
                    code,
                    message: envelope.message,
                    details: envelope.details,
                })
            }
            Err(()) => {
                let mut details = envelope.details;
                details
                    .entry("unknown_code".to_string())
                    .or_insert_with(|| envelope.code.to_string());
                details
                    .entry("unknown_name".to_string())
                    .or_insert_with(|| envelope.name.clone());
                Ok(C2Error {
                    code: ErrorCode::Unknown,
                    message: format!(
                        "Unknown error code {} ({}): {}",
                        envelope.code, envelope.name, envelope.message
                    ),
                    details,
                })
            }
        }
    }

    pub fn from_wire_bytes(data: &[u8]) -> Result<Option<Self>, C2ErrorDecodeError> {
        if data.is_empty() {
            return Ok(None);
        }

        let Some(payload) = data.strip_prefix(ERROR_WIRE_MAGIC) else {
            return Err(C2ErrorDecodeError::MissingMagic);
        };
        let envelope: C2ErrorEnvelope = serde_json::from_slice(payload)
            .map_err(|err| C2ErrorDecodeError::InvalidJson(err.to_string()))?;
        Ok(Some(Self::from_envelope(envelope)?))
    }
}

impl fmt::Display for C2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.name(), self.message)
    }
}

impl std::error::Error for C2Error {}

#[cfg(test)]
mod tests {
    use super::{C2Error, C2ErrorEnvelope, ErrorCode};
    use std::collections::BTreeMap;

    #[test]
    fn canonical_error_codes_match_wire_values() {
        assert_eq!(u16::from(ErrorCode::Unknown), 0);
        assert_eq!(u16::from(ErrorCode::ResourceInputDeserializing), 1);
        assert_eq!(u16::from(ErrorCode::ResourceOutputSerializing), 2);
        assert_eq!(u16::from(ErrorCode::ResourceFunctionExecuting), 3);
        assert_eq!(u16::from(ErrorCode::ResourceInputFromBuffer), 4);
        assert_eq!(u16::from(ErrorCode::ClientInputSerializing), 5);
        assert_eq!(u16::from(ErrorCode::ClientOutputDeserializing), 6);
        assert_eq!(u16::from(ErrorCode::ClientCallingResource), 7);
        assert_eq!(u16::from(ErrorCode::ClientOutputFromBuffer), 8);
        assert_eq!(u16::from(ErrorCode::ResourceNotFound), 701);
        assert_eq!(u16::from(ErrorCode::ResourceUnavailable), 702);
        assert_eq!(u16::from(ErrorCode::ResourceAlreadyRegistered), 703);
        assert_eq!(u16::from(ErrorCode::RouteStale), 704);
        assert_eq!(u16::from(ErrorCode::RegistryUnavailable), 705);
        assert_eq!(u16::from(ErrorCode::WriteConflict), 706);
        assert_eq!(u16::from(ErrorCode::ResourceClosed), 707);
        assert_eq!(u16::from(ErrorCode::ResourceRemoved), 708);
        assert_eq!(u16::from(ErrorCode::ContractMismatch), 709);
        assert_eq!(u16::from(ErrorCode::IdentityMismatch), 710);
        assert_eq!(u16::from(ErrorCode::RouteCatalogCompacted), 711);
        assert_eq!(u16::from(ErrorCode::RouteWatchUnavailable), 712);
        assert_eq!(u16::from(ErrorCode::ProtocolViolation), 713);
        assert_eq!(u16::from(ErrorCode::FallbackDenied), 714);
        assert_eq!(u16::from(ErrorCode::CallDeadlineExceeded), 715);
        assert_eq!(u16::from(ErrorCode::UnsupportedCallMode), 716);
        assert_eq!(u16::from(ErrorCode::CallCapacityExceeded), 717);
    }

    #[test]
    fn c2_error_display_uses_code_name_and_message() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(err.to_string(), "ResourceAlreadyRegistered: grid exists");
    }

    #[test]
    fn error_code_registry_exposes_names_and_codes_from_crate() {
        let registry: Vec<_> = ErrorCode::registry()
            .iter()
            .map(|entry| (entry.name, u16::from(entry.code)))
            .collect();

        assert_eq!(
            registry,
            vec![
                ("Unknown", 0),
                ("ResourceInputDeserializing", 1),
                ("ResourceOutputSerializing", 2),
                ("ResourceFunctionExecuting", 3),
                ("ResourceInputFromBuffer", 4),
                ("ClientInputSerializing", 5),
                ("ClientOutputDeserializing", 6),
                ("ClientCallingResource", 7),
                ("ClientOutputFromBuffer", 8),
                ("ResourceNotFound", 701),
                ("ResourceUnavailable", 702),
                ("ResourceAlreadyRegistered", 703),
                ("RouteStale", 704),
                ("RegistryUnavailable", 705),
                ("WriteConflict", 706),
                ("ResourceClosed", 707),
                ("ResourceRemoved", 708),
                ("ContractMismatch", 709),
                ("IdentityMismatch", 710),
                ("RouteCatalogCompacted", 711),
                ("RouteWatchUnavailable", 712),
                ("ProtocolViolation", 713),
                ("FallbackDenied", 714),
                ("CallDeadlineExceeded", 715),
                ("UnsupportedCallMode", 716),
                ("CallCapacityExceeded", 717),
            ],
        );
        assert_eq!(ErrorCode::WriteConflict.name(), "WriteConflict");
    }

    #[test]
    fn error_code_registry_has_unique_codes_names_and_full_variant_coverage() {
        let registry = ErrorCode::registry();
        assert_eq!(registry.len(), 26);

        let mut codes: Vec<u16> = registry.iter().map(|entry| u16::from(entry.code)).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), registry.len());

        let mut names: Vec<&str> = registry.iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), registry.len());
    }

    #[test]
    fn from_buffer_error_codes_round_trip_from_wire_values() {
        assert_eq!(
            ErrorCode::try_from(4),
            Ok(ErrorCode::ResourceInputFromBuffer),
        );
        assert_eq!(
            ErrorCode::try_from(8),
            Ok(ErrorCode::ClientOutputFromBuffer),
        );
    }

    #[test]
    fn wire_encode_matches_canonical_error_wire_format() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(
            err.to_wire_bytes(),
            br#"C2E1{"version":1,"code":703,"name":"ResourceAlreadyRegistered","message":"grid exists","details":{}}"#
        );
    }

    #[test]
    fn wire_encode_sorts_details_for_stable_fixtures() {
        let mut details = BTreeMap::new();
        details.insert("route".to_string(), "grid".to_string());
        details.insert("cause_kind".to_string(), "TransportIo".to_string());
        let err = C2Error::new(ErrorCode::ResourceUnavailable, "upstream unavailable")
            .with_details(details);
        assert_eq!(
            err.to_wire_bytes(),
            br#"C2E1{"version":1,"code":702,"name":"ResourceUnavailable","message":"upstream unavailable","details":{"cause_kind":"TransportIo","route":"grid"}}"#
        );
    }

    #[test]
    fn wire_decode_empty_bytes_means_no_error() {
        assert_eq!(C2Error::from_wire_bytes(b"").unwrap(), None);
    }

    #[test]
    fn wire_decode_known_code_returns_canonical_error() {
        let err = C2Error::from_wire_bytes(
            br#"C2E1{"version":1,"code":701,"name":"ResourceNotFound","message":"missing grid","details":{"route":"grid"}}"#,
        )
            .unwrap()
            .unwrap();
        assert_eq!(err.code, ErrorCode::ResourceNotFound);
        assert_eq!(err.message, "missing grid");
        assert_eq!(err.details.get("route").map(String::as_str), Some("grid"));
    }

    #[test]
    fn envelope_decode_known_code_returns_canonical_error() {
        let mut details = BTreeMap::new();
        details.insert("route".to_string(), "grid".to_string());
        let err = C2Error::from_envelope(C2ErrorEnvelope {
            version: 1,
            code: 701,
            name: "ResourceNotFound".to_string(),
            message: "missing grid".to_string(),
            details,
        })
        .unwrap();

        assert_eq!(err.code, ErrorCode::ResourceNotFound);
        assert_eq!(err.message, "missing grid");
        assert_eq!(err.details.get("route").map(String::as_str), Some("grid"));
    }

    #[test]
    fn wire_decode_preserves_colons_in_message() {
        let err = C2Error::from_wire_bytes(
            br#"C2E1{"version":1,"code":0,"name":"Unknown","message":"host:port:extra","details":{}}"#,
        )
            .unwrap()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Unknown);
        assert_eq!(err.message, "host:port:extra");
    }

    #[test]
    fn wire_decode_unknown_code_degrades_to_unknown_with_context_and_details() {
        let err = C2Error::from_wire_bytes(
            br#"C2E1{"version":1,"code":9999,"name":"FutureRouteError","message":"low-level relay failure","details":{"route":"grid"}}"#,
        )
            .unwrap()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Unknown);
        assert_eq!(
            err.message,
            "Unknown error code 9999 (FutureRouteError): low-level relay failure"
        );
        assert_eq!(err.details.get("route").map(String::as_str), Some("grid"));
        assert_eq!(
            err.details.get("unknown_code").map(String::as_str),
            Some("9999")
        );
        assert_eq!(
            err.details.get("unknown_name").map(String::as_str),
            Some("FutureRouteError")
        );
    }

    #[test]
    fn wire_decode_rejects_legacy_code_message_payloads() {
        let err = C2Error::from_wire_bytes(b"703:grid exists").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid C2 error wire payload: missing C2E1 envelope magic"
        );
    }

    #[test]
    fn wire_decode_rejects_name_mismatch() {
        let err = C2Error::from_wire_bytes(
            br#"C2E1{"version":1,"code":703,"name":"ResourceNotFound","message":"grid exists","details":{}}"#,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid C2 error wire name for code 703: expected ResourceAlreadyRegistered, got ResourceNotFound"
        );
    }

    #[test]
    fn wire_round_trips_new_call_error_codes() {
        let mut details = BTreeMap::new();
        details.insert(
            "transport_phase".to_string(),
            "dispatch_uncertain".to_string(),
        );
        let err = C2Error::new(
            ErrorCode::CallDeadlineExceeded,
            "deadline elapsed before a definitive outcome",
        )
        .with_details(details);
        let decoded = C2Error::from_wire_bytes(&err.to_wire_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(decoded.code, ErrorCode::CallDeadlineExceeded);
        assert_eq!(
            decoded.message,
            "deadline elapsed before a definitive outcome"
        );
        assert_eq!(
            decoded.details.get("transport_phase").map(String::as_str),
            Some("dispatch_uncertain")
        );

        for (code, expected) in [
            (ErrorCode::UnsupportedCallMode, 716_u16),
            (ErrorCode::CallCapacityExceeded, 717),
        ] {
            let plain = C2Error::new(code, "bounded call budget exhausted");
            let decoded = C2Error::from_wire_bytes(&plain.to_wire_bytes())
                .unwrap()
                .unwrap();
            assert_eq!(decoded.code, code);
            assert_eq!(u16::from(decoded.code), expected);
            assert_eq!(decoded.message, "bounded call budget exhausted");
            assert!(decoded.details.is_empty());
        }
    }
}
