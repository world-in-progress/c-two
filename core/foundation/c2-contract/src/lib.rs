//! Language-neutral route contract validation and descriptor hashing.

use std::fmt;
use thiserror::Error;

mod admission;
mod descriptor;
mod release;

pub use admission::{
    ContractLimitMetric, ContractLimits, ContractLimitsProfile, MAX_CONTRACT_METHODS,
    descriptor_admission_count_for_current_thread,
};
pub use descriptor::{
    BindingDirection, ContractFingerprints, MethodAccess, NestedFastDbSpec,
    ValidatedContractDescriptor, ValidatedMethodDescriptor, contract_descriptor_sha256_hex,
    contract_descriptor_sha256_hex_with_limits, derive_contract_fingerprints_json,
    derive_contract_fingerprints_json_with_limits, validate_portable_contract_descriptor_json,
};
pub use release::{
    CONTRACT_RELEASE_REF_SCHEMA, ContractDescriptorDigest, ContractRelease, ContractReleaseRef,
    ContractReleaseRefField,
};

pub const MAX_WIRE_TEXT_BYTES: usize = u8::MAX as usize;
pub const CONTRACT_HASH_HEX_BYTES: usize = 64;
pub const PORTABLE_CONTRACT_SCHEMA: &str = "c-two.contract.v2";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpectedRouteContract {
    pub route_name: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractFingerprintField {
    AbiHash,
    SignatureHash,
}

impl fmt::Display for ContractFingerprintField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AbiHash => "abi_hash",
            Self::SignatureHash => "signature_hash",
        })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    #[error("{field} cannot be empty")]
    Empty { field: &'static str },
    #[error("{field} cannot exceed {max} bytes: {actual}")]
    TooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("{field} cannot contain leading or trailing whitespace")]
    SurroundingWhitespace { field: &'static str },
    #[error("{field} cannot contain control characters")]
    ControlCharacter { field: &'static str },
    #[error("{field} cannot contain path or tag separators")]
    Separator { field: &'static str },
    #[error("{field} must be exactly 64 lowercase hex bytes")]
    InvalidHash { field: &'static str },
    #[error("contract descriptor must be valid JSON: {0}")]
    InvalidJson(String),
    #[error(
        "contract admission limit exceeded for {metric:?} at {path}: profile={profile:?}, limit={limit}, observed={observed}"
    )]
    LimitExceeded {
        profile: ContractLimitsProfile,
        metric: ContractLimitMetric,
        limit: u64,
        observed: u64,
        path: String,
    },
    #[error("contract descriptor invalid at {path}: {message}")]
    InvalidDescriptor { path: String, message: String },
    #[error("contract fingerprint mismatch at {field}: expected {expected:?}, got {actual:?}")]
    FingerprintMismatch {
        field: ContractFingerprintField,
        expected: String,
        actual: String,
    },
    #[error("contract release reference must be valid JSON: {0}")]
    InvalidReleaseRefJson(String),
    #[error("contract release reference invalid at {path}: {message}")]
    InvalidReleaseRef { path: String, message: String },
    #[error(
        "contract release reference mismatch at {field}: expected {expected:?}, got {actual:?}"
    )]
    ReleaseRefMismatch {
        field: ContractReleaseRefField,
        expected: String,
        actual: String,
    },
}

pub fn validate_named_route_name(field: &'static str, value: &str) -> Result<(), ContractError> {
    validate_route_text_field(field, value)
}

pub fn validate_call_route_key(field: &'static str, value: &str) -> Result<(), ContractError> {
    validate_route_text_field(field, value)
}

pub fn validate_contract_text_field(field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(ContractError::Empty { field });
    }
    validate_wire_len(field, value)?;
    if value.trim() != value {
        return Err(ContractError::SurroundingWhitespace { field });
    }
    if value.chars().any(char::is_control) {
        return Err(ContractError::ControlCharacter { field });
    }
    if value.contains('/') || value.contains('\\') {
        return Err(ContractError::Separator { field });
    }
    Ok(())
}

fn validate_route_text_field(field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(ContractError::Empty { field });
    }
    validate_wire_len(field, value)?;
    if value.trim() != value {
        return Err(ContractError::SurroundingWhitespace { field });
    }
    if value.chars().any(char::is_control) {
        return Err(ContractError::ControlCharacter { field });
    }
    if value.contains('\\') {
        return Err(ContractError::Separator { field });
    }
    Ok(())
}

pub fn validate_crm_tag(crm_ns: &str, crm_name: &str, crm_ver: &str) -> Result<(), ContractError> {
    validate_contract_text_field("crm namespace", crm_ns)?;
    validate_contract_text_field("crm name", crm_name)?;
    validate_contract_text_field("crm version", crm_ver)?;
    Ok(())
}

pub fn validate_contract_hash(field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.len() != CONTRACT_HASH_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ContractError::InvalidHash { field });
    }
    Ok(())
}

pub fn validate_expected_route_contract(
    expected: &ExpectedRouteContract,
) -> Result<(), ContractError> {
    validate_named_route_name("route name", &expected.route_name)?;
    validate_crm_tag(&expected.crm_ns, &expected.crm_name, &expected.crm_ver)?;
    validate_contract_hash("abi_hash", &expected.abi_hash)?;
    validate_contract_hash("signature_hash", &expected.signature_hash)?;
    Ok(())
}

fn validate_wire_len(field: &'static str, value: &str) -> Result<(), ContractError> {
    let actual = value.len();
    if actual > MAX_WIRE_TEXT_BYTES {
        return Err(ContractError::TooLong {
            field,
            max: MAX_WIRE_TEXT_BYTES,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_hash_shape() {
        validate_contract_hash(
            "abi_hash",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert!(validate_contract_hash("abi_hash", "").is_err());
        assert!(
            validate_contract_hash(
                "abi_hash",
                "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
            )
            .is_err()
        );
    }

    #[test]
    fn route_name_allows_forward_slash_but_rejects_backslash() {
        validate_named_route_name("route name", "toodle/grid/0").unwrap();
        validate_call_route_key("route name", "toodle/grid/0").unwrap();
        assert!(validate_named_route_name("route name", "bad\\route").is_err());
    }

    #[test]
    fn descriptor_hash_canonicalizes_object_order() {
        let left =
            contract_descriptor_sha256_hex(br#"{"b":2,"a":{"y":1,"x":[true,null]}}"#).unwrap();
        let right =
            contract_descriptor_sha256_hex(br#"{"a":{"x":[true,null],"y":1},"b":2}"#).unwrap();
        assert_eq!(left, right);
        assert_eq!(left.len(), 64);
    }
}
