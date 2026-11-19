use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

const ERROR_WIRE_MAGIC: &[u8; 4] = b"C2E1";
pub const ERROR_WIRE_VERSION: u16 = 1;

/// Which side of a call produced an adapter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorSide {
    Client,
    Resource,
}

impl ErrorSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Resource => "resource",
        }
    }
}

/// The operation that produced an adapter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorStage {
    Serialize,
    Deserialize,
    FromBuffer,
    Execute,
    Call,
}

impl ErrorStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serialize => "serialize",
            Self::Deserialize => "deserialize",
            Self::FromBuffer => "from_buffer",
            Self::Execute => "execute",
            Self::Call => "call",
        }
    }
}

macro_rules! define_error_codes {
    ($($variant:ident = $value:literal $(=> ($side:ident, $stage:ident))?),+ $(,)?) => {
        #[repr(u16)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $($variant = $value,)+
        }

        const ERROR_CODE_REGISTRY: &[ErrorCodeEntry] = &[
            $(ErrorCodeEntry {
                code: ErrorCode::$variant,
                name: stringify!($variant),
                side: ErrorCode::$variant.side(),
                stage: ErrorCode::$variant.stage(),
            },)+
        ];

        impl ErrorCode {
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)+
                }
            }

            pub const fn side(self) -> Option<ErrorSide> {
                match self {
                    $($(Self::$variant => Some(ErrorSide::$side),)?)+
                    _ => None,
                }
            }

            pub const fn stage(self) -> Option<ErrorStage> {
                match self {
                    $($(Self::$variant => Some(ErrorStage::$stage),)?)+
                    _ => None,
                }
            }

            pub fn registry() -> &'static [ErrorCodeEntry] {
                ERROR_CODE_REGISTRY
            }
        }

        impl TryFrom<u16> for ErrorCode {
            type Error = ();

            fn try_from(value: u16) -> Result<Self, Self::Error> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(()),
                }
            }
        }
    };
}

// Permanently retired: 704 RouteStale, 706 WriteConflict, 711 RouteCatalogCompacted. Never reassign.
define_error_codes! {
    Unknown = 0,
    ResourceInputDeserializing = 1 => (Resource, Deserialize),
    ResourceOutputSerializing = 2 => (Resource, Serialize),
    ResourceFunctionExecuting = 3 => (Resource, Execute),
    ResourceInputFromBuffer = 4 => (Resource, FromBuffer),
    ClientInputSerializing = 5 => (Client, Serialize),
    ClientOutputDeserializing = 6 => (Client, Deserialize),
    ClientCallingResource = 7 => (Client, Call),
    ClientOutputFromBuffer = 8 => (Client, FromBuffer),
    ResourceNotFound = 701,
    ResourceUnavailable = 702,
    ResourceAlreadyRegistered = 703,
    RegistryUnavailable = 705,
    ResourceClosed = 707,
    ResourceRemoved = 708,
    ContractMismatch = 709,
    IdentityMismatch = 710,
    RouteWatchUnavailable = 712,
    ProtocolViolation = 713,
    FallbackDenied = 714,
    CallDeadlineExceeded = 715,
    UnsupportedCallMode = 716,
    CallCapacityExceeded = 717,
    CallCancelled = 718,
    RouteConflict = 719,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorCodeEntry {
    pub code: ErrorCode,
    pub name: &'static str,
    pub side: Option<ErrorSide>,
    pub stage: Option<ErrorStage>,
}

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> Self {
        code as u16
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct C2ErrorEnvelope {
    pub version: u16,
    pub code: u16,
    pub name: String,
    pub message: String,
    pub details: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for C2ErrorEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EnvelopeVisitor;

        impl<'de> serde::de::Visitor<'de> for EnvelopeVisitor {
            type Value = C2ErrorEnvelope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a C2 error JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut version: Option<u16> = None;
                let mut code: Option<u16> = None;
                let mut name: Option<String> = None;
                let mut message: Option<String> = None;
                let mut details: Option<BTreeMap<String, String>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => {
                            if version.is_some() {
                                return Err(serde::de::Error::duplicate_field("version"));
                            }
                            version = Some(map.next_value()?);
                        }
                        "code" => {
                            if code.is_some() {
                                return Err(serde::de::Error::duplicate_field("code"));
                            }
                            code = Some(map.next_value()?);
                        }
                        "name" => {
                            if name.is_some() {
                                return Err(serde::de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "message" => {
                            if message.is_some() {
                                return Err(serde::de::Error::duplicate_field("message"));
                            }
                            message = Some(map.next_value()?);
                        }
                        "details" => {
                            if details.is_some() {
                                return Err(serde::de::Error::duplicate_field("details"));
                            }
                            details = Some(map.next_value()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }

                Ok(C2ErrorEnvelope {
                    version: version.ok_or_else(|| serde::de::Error::missing_field("version"))?,
                    code: code.ok_or_else(|| serde::de::Error::missing_field("code"))?,
                    name: name.ok_or_else(|| serde::de::Error::missing_field("name"))?,
                    message: message.ok_or_else(|| serde::de::Error::missing_field("message"))?,
                    details: details.ok_or_else(|| serde::de::Error::missing_field("details"))?,
                })
            }
        }

        deserializer.deserialize_map(EnvelopeVisitor)
    }
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

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, C2ErrorDecodeError> {
        let body = std::str::from_utf8(bytes)
            .map_err(|error| C2ErrorDecodeError::InvalidJson(error.to_string()))?;
        let envelope: C2ErrorEnvelope = serde_json::from_str(body)
            .map_err(|error| C2ErrorDecodeError::InvalidJson(error.to_string()))?;
        Self::from_envelope(envelope)
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
        Ok(Some(Self::from_json_bytes(payload)?))
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
    use super::{C2Error, C2ErrorEnvelope, ErrorCode, ErrorSide, ErrorStage};
    use std::collections::BTreeMap;

    #[test]
    fn retired_error_codes_are_never_reassigned() {
        for code in [704, 706, 711] {
            assert!(ErrorCode::try_from(code).is_err());
            assert!(
                ErrorCode::registry()
                    .iter()
                    .all(|entry| u16::from(entry.code) != code)
            );
        }
        assert_eq!(
            ErrorCode::try_from(712),
            Ok(ErrorCode::RouteWatchUnavailable)
        );
    }

    #[test]
    fn error_code_registry_matches_canonical_wire_values() {
        use ErrorCode::*;

        let expected: [(ErrorCode, u16, &str); 25] = [
            (Unknown, 0, "Unknown"),
            (ResourceInputDeserializing, 1, "ResourceInputDeserializing"),
            (ResourceOutputSerializing, 2, "ResourceOutputSerializing"),
            (ResourceFunctionExecuting, 3, "ResourceFunctionExecuting"),
            (ResourceInputFromBuffer, 4, "ResourceInputFromBuffer"),
            (ClientInputSerializing, 5, "ClientInputSerializing"),
            (ClientOutputDeserializing, 6, "ClientOutputDeserializing"),
            (ClientCallingResource, 7, "ClientCallingResource"),
            (ClientOutputFromBuffer, 8, "ClientOutputFromBuffer"),
            (ResourceNotFound, 701, "ResourceNotFound"),
            (ResourceUnavailable, 702, "ResourceUnavailable"),
            (ResourceAlreadyRegistered, 703, "ResourceAlreadyRegistered"),
            (RegistryUnavailable, 705, "RegistryUnavailable"),
            (ResourceClosed, 707, "ResourceClosed"),
            (ResourceRemoved, 708, "ResourceRemoved"),
            (ContractMismatch, 709, "ContractMismatch"),
            (IdentityMismatch, 710, "IdentityMismatch"),
            (RouteWatchUnavailable, 712, "RouteWatchUnavailable"),
            (ProtocolViolation, 713, "ProtocolViolation"),
            (FallbackDenied, 714, "FallbackDenied"),
            (CallDeadlineExceeded, 715, "CallDeadlineExceeded"),
            (UnsupportedCallMode, 716, "UnsupportedCallMode"),
            (CallCapacityExceeded, 717, "CallCapacityExceeded"),
            (CallCancelled, 718, "CallCancelled"),
            (RouteConflict, 719, "RouteConflict"),
        ];
        let registry = ErrorCode::registry();
        assert_eq!(registry.len(), expected.len());
        for (entry, (variant, value, name)) in registry.iter().zip(expected) {
            assert_eq!(entry.code, variant);
            assert_eq!(u16::from(variant), value);
            assert_eq!(entry.name, name);
            assert_eq!(variant.name(), name);
            assert_eq!(ErrorCode::try_from(value), Ok(variant));
        }
        for value in [9, 700, 9999, u16::MAX] {
            assert_eq!(ErrorCode::try_from(value), Err(()));
        }
    }

    #[test]
    fn error_registry_classifies_only_adapter_codes() {
        use ErrorSide::{Client, Resource};
        use ErrorStage::{Call, Deserialize, Execute, FromBuffer, Serialize};

        let expected = [
            (Resource, Deserialize, "resource", "deserialize"),
            (Resource, Serialize, "resource", "serialize"),
            (Resource, Execute, "resource", "execute"),
            (Resource, FromBuffer, "resource", "from_buffer"),
            (Client, Serialize, "client", "serialize"),
            (Client, Deserialize, "client", "deserialize"),
            (Client, Call, "client", "call"),
            (Client, FromBuffer, "client", "from_buffer"),
        ];
        for entry in ErrorCode::registry() {
            let value = u16::from(entry.code);
            if (1..=8).contains(&value) {
                let (side, stage, side_name, stage_name) = expected[usize::from(value - 1)];
                assert_eq!((entry.side, entry.stage), (Some(side), Some(stage)));
                assert_eq!(side.as_str(), side_name);
                assert_eq!(stage.as_str(), stage_name);
            } else {
                assert_eq!((entry.side, entry.stage), (None, None));
            }
            assert_eq!(entry.code.side(), entry.side);
            assert_eq!(entry.code.stage(), entry.stage);
        }
    }

    #[test]
    fn c2_error_display_uses_code_name_and_message() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(err.to_string(), "ResourceAlreadyRegistered: grid exists");
    }

    #[test]
    fn wire_encode_matches_canonical_error_wire_format() {
        let err = C2Error::new(ErrorCode::ResourceInputDeserializing, "invalid input");
        assert_eq!(
            err.to_wire_bytes(),
            br#"C2E1{"version":1,"code":1,"name":"ResourceInputDeserializing","message":"invalid input","details":{}}"#
        );
    }

    #[test]
    fn call_cancelled_round_trips_with_canonical_code_and_details() {
        let error = C2Error::new(ErrorCode::CallCancelled, "stream closed")
            .with_details(BTreeMap::from([("reason".into(), "client_close".into())]));
        let decoded = C2Error::from_wire_bytes(&error.to_wire_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(decoded, error);
        assert_eq!(decoded.code as u16, 718);
    }

    #[test]
    fn route_conflict_round_trips_with_canonical_code_and_registrations() {
        let registrations = r#"[{"relay_id":"a","relay_url":"http://relay","registered_at":1,"relay_status":"Alive"}]"#;
        let error = C2Error::new(ErrorCode::RouteConflict, "duplicate online route").with_details(
            BTreeMap::from([
                ("route".into(), "grid".into()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
                ("registrations".into(), registrations.into()),
            ]),
        );
        let decoded = C2Error::from_wire_bytes(&error.to_wire_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(decoded, error);
        assert_eq!(decoded.code as u16, 719);
        assert_eq!(decoded.code.name(), "RouteConflict");
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

    #[test]
    fn envelope_json_rejects_arrays_floats_and_duplicate_fields() {
        for invalid in [
            r#"[1,717,"CallCapacityExceeded","msg",{"dispatch_phase":"pre_dispatch"}]"#,
            r#"{"version":1,"code":717.0,"name":"CallCapacityExceeded","message":"m","details":{}}"#,
            r#"{"version":1.0,"code":717,"name":"CallCapacityExceeded","message":"m","details":{}}"#,
            r#"{"version":1,"code":717,"code":717,"name":"CallCapacityExceeded","message":"m","details":{}}"#,
        ] {
            assert!(
                serde_json::from_str::<C2ErrorEnvelope>(invalid).is_err(),
                "expected C2ErrorEnvelope JSON rejection for {invalid}"
            );
            let mut wire = b"C2E1".to_vec();
            wire.extend_from_slice(invalid.as_bytes());
            assert!(
                C2Error::from_wire_bytes(&wire).is_err(),
                "expected C2E1 wire rejection for {invalid}"
            );
            assert!(
                C2Error::from_json_bytes(invalid.as_bytes()).is_err(),
                "expected JSON bytes rejection for {invalid}"
            );
        }

        for raw_invalid in [
            &b"{\"version\":1,\"code\":701,\"name\":\"ResourceNotFound\",\"message\":\"m\",\"details\":{},\"extra\":\"bad\xff\"}"[..],
            &b"\xef\xbb\xbf{\"version\":1,\"code\":717,\"name\":\"CallCapacityExceeded\",\"message\":\"m\",\"details\":{\"dispatch_phase\":\"pre_dispatch\"}}"[..],
            &b"{\"version\":1,\"code\":717,\"name\":\"CallCapacityExceeded\",\"message\":\"bad \\ud800\",\"details\":{\"dispatch_phase\":\"pre_dispatch\"}}"[..],
            &b"{\"version\":1,\"code\":717,\"name\":\"CallCapacityExceeded\",\"message\":\"m\",\"details\":{\"k\":1,\"k\":\"ok\",\"dispatch_phase\":\"pre_dispatch\"}}"[..],
        ] {
            assert!(
                C2Error::from_json_bytes(raw_invalid).is_err(),
                "expected C2Error::from_json_bytes rejection for {raw_invalid:?}"
            );
            assert!(
                serde_json::from_slice::<C2ErrorEnvelope>(raw_invalid).is_err(),
                "expected serde_json::from_slice::<C2ErrorEnvelope> rejection for {raw_invalid:?}"
            );
        }
    }
}
