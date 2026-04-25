use std::fmt;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Unknown = 0,
    ResourceInputDeserializing = 1,
    ResourceOutputSerializing = 2,
    ResourceFunctionExecuting = 3,
    ClientInputSerializing = 5,
    ClientOutputDeserializing = 6,
    ClientCallingResource = 7,
    ResourceNotFound = 701,
    ResourceUnavailable = 702,
    ResourceAlreadyRegistered = 703,
    StaleResource = 704,
    RegistryUnavailable = 705,
    WriteConflict = 706,
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
        code: ErrorCode::StaleResource,
        name: "StaleResource",
    },
    ErrorCodeEntry {
        code: ErrorCode::RegistryUnavailable,
        name: "RegistryUnavailable",
    },
    ErrorCodeEntry {
        code: ErrorCode::WriteConflict,
        name: "WriteConflict",
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
            5 => Ok(ErrorCode::ClientInputSerializing),
            6 => Ok(ErrorCode::ClientOutputDeserializing),
            7 => Ok(ErrorCode::ClientCallingResource),
            701 => Ok(ErrorCode::ResourceNotFound),
            702 => Ok(ErrorCode::ResourceUnavailable),
            703 => Ok(ErrorCode::ResourceAlreadyRegistered),
            704 => Ok(ErrorCode::StaleResource),
            705 => Ok(ErrorCode::RegistryUnavailable),
            706 => Ok(ErrorCode::WriteConflict),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2Error {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C2ErrorDecodeError {
    InvalidUtf8,
    MissingSeparator,
    InvalidCode(String),
}

impl fmt::Display for C2ErrorDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            C2ErrorDecodeError::InvalidUtf8 => f.write_str("invalid legacy C2 error UTF-8"),
            C2ErrorDecodeError::MissingSeparator => {
                f.write_str("invalid legacy C2 error: missing ':' separator")
            }
            C2ErrorDecodeError::InvalidCode(code) => {
                write!(f, "invalid legacy C2 error code: {code}")
            }
        }
    }
}

impl std::error::Error for C2ErrorDecodeError {}

impl C2Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unknown, message)
    }

    pub fn to_legacy_bytes(&self) -> Vec<u8> {
        format!("{}:{}", u16::from(self.code), self.message).into_bytes()
    }

    pub fn from_legacy_bytes(data: &[u8]) -> Result<Option<Self>, C2ErrorDecodeError> {
        if data.is_empty() {
            return Ok(None);
        }

        let raw = std::str::from_utf8(data).map_err(|_| C2ErrorDecodeError::InvalidUtf8)?;
        let (code_raw, message) = raw
            .split_once(':')
            .ok_or(C2ErrorDecodeError::MissingSeparator)?;
        let code_value = code_raw
            .parse::<u16>()
            .map_err(|_| C2ErrorDecodeError::InvalidCode(code_raw.to_string()))?;

        match ErrorCode::try_from(code_value) {
            Ok(code) => Ok(Some(C2Error::new(code, message))),
            Err(()) => Ok(Some(C2Error::unknown(format!(
                "Unknown error code {code_value}: {message}"
            )))),
        }
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
    use super::{C2Error, ErrorCode};

    #[test]
    fn canonical_error_codes_match_python_compatibility_values() {
        assert_eq!(u16::from(ErrorCode::Unknown), 0);
        assert_eq!(u16::from(ErrorCode::ResourceInputDeserializing), 1);
        assert_eq!(u16::from(ErrorCode::ResourceOutputSerializing), 2);
        assert_eq!(u16::from(ErrorCode::ResourceFunctionExecuting), 3);
        assert_eq!(u16::from(ErrorCode::ClientInputSerializing), 5);
        assert_eq!(u16::from(ErrorCode::ClientOutputDeserializing), 6);
        assert_eq!(u16::from(ErrorCode::ClientCallingResource), 7);
        assert_eq!(u16::from(ErrorCode::ResourceNotFound), 701);
        assert_eq!(u16::from(ErrorCode::ResourceUnavailable), 702);
        assert_eq!(u16::from(ErrorCode::ResourceAlreadyRegistered), 703);
        assert_eq!(u16::from(ErrorCode::StaleResource), 704);
        assert_eq!(u16::from(ErrorCode::RegistryUnavailable), 705);
        assert_eq!(u16::from(ErrorCode::WriteConflict), 706);
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
                ("ClientInputSerializing", 5),
                ("ClientOutputDeserializing", 6),
                ("ClientCallingResource", 7),
                ("ResourceNotFound", 701),
                ("ResourceUnavailable", 702),
                ("ResourceAlreadyRegistered", 703),
                ("StaleResource", 704),
                ("RegistryUnavailable", 705),
                ("WriteConflict", 706),
            ],
        );
        assert_eq!(ErrorCode::WriteConflict.name(), "WriteConflict");
    }

    #[test]
    fn legacy_encode_matches_existing_python_format() {
        let err = C2Error::new(ErrorCode::ResourceAlreadyRegistered, "grid exists");
        assert_eq!(err.to_legacy_bytes(), b"703:grid exists");
    }

    #[test]
    fn legacy_decode_empty_bytes_means_no_error() {
        assert_eq!(C2Error::from_legacy_bytes(b"").unwrap(), None);
    }

    #[test]
    fn legacy_decode_known_code_returns_canonical_error() {
        let err = C2Error::from_legacy_bytes(b"701:missing grid").unwrap().unwrap();
        assert_eq!(err.code, ErrorCode::ResourceNotFound);
        assert_eq!(err.message, "missing grid");
    }

    #[test]
    fn legacy_decode_preserves_colons_in_message() {
        let err = C2Error::from_legacy_bytes(b"0:host:port:extra").unwrap().unwrap();
        assert_eq!(err.code, ErrorCode::Unknown);
        assert_eq!(err.message, "host:port:extra");
    }

    #[test]
    fn legacy_decode_unknown_code_degrades_to_unknown_with_context() {
        let err = C2Error::from_legacy_bytes(b"9999:low-level relay failure").unwrap().unwrap();
        assert_eq!(err.code, ErrorCode::Unknown);
        assert_eq!(err.message, "Unknown error code 9999: low-level relay failure");
    }

    #[test]
    fn legacy_decode_malformed_code_fails() {
        let err = C2Error::from_legacy_bytes(b"abc:not a number").unwrap_err();
        assert_eq!(err.to_string(), "invalid legacy C2 error code: abc");
    }
}
