use crate::descriptor::canonical_json;
use crate::{
    ContractError, ContractLimits, ExpectedRouteContract, ValidatedContractDescriptor,
    admission::{JsonDocumentKind, parse_bounded_json},
    validate_contract_hash, validate_contract_text_field, validate_expected_route_contract,
};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

pub const CONTRACT_RELEASE_REF_SCHEMA: &str = "c-two.contract-release-ref.v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContractDescriptorDigest(String);

impl ContractDescriptorDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        validate_contract_hash("descriptor_sha256", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContractDescriptorDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ContractDescriptorDigest {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractReleaseRefField {
    ContractSchema,
    CrmNamespace,
    CrmName,
    CrmVersion,
    DescriptorSha256,
}

impl fmt::Display for ContractReleaseRefField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ContractSchema => "contract_schema",
            Self::CrmNamespace => "crm.namespace",
            Self::CrmName => "crm.name",
            Self::CrmVersion => "crm.version",
            Self::DescriptorSha256 => "descriptor_sha256",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRelease {
    descriptor: ValidatedContractDescriptor,
}

impl ContractRelease {
    pub fn from_descriptor_json(bytes: &[u8]) -> Result<Self, ContractError> {
        Self::from_descriptor_json_with_limits(bytes, ContractLimits::default())
    }

    pub fn from_descriptor_json_with_limits(
        bytes: &[u8],
        limits: ContractLimits,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            descriptor: ValidatedContractDescriptor::from_json_with_limits(bytes, limits)?,
        })
    }

    pub fn descriptor(&self) -> &ValidatedContractDescriptor {
        &self.descriptor
    }

    pub fn canonical_descriptor_json(&self) -> &str {
        self.descriptor.canonical_json()
    }

    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest {
        self.descriptor.descriptor_sha256()
    }

    pub fn reference(&self) -> ContractReleaseRef {
        ContractReleaseRef {
            contract_schema: self.descriptor.contract_schema().to_string(),
            crm_namespace: self.descriptor.crm_namespace().to_string(),
            crm_name: self.descriptor.crm_name().to_string(),
            crm_version: self.descriptor.crm_version().to_string(),
            descriptor_sha256: self.descriptor.descriptor_sha256().clone(),
        }
    }

    pub fn expected_route(
        &self,
        route_name: impl Into<String>,
    ) -> Result<ExpectedRouteContract, ContractError> {
        let expected = ExpectedRouteContract {
            route_name: route_name.into(),
            crm_ns: self.descriptor.crm_namespace().to_string(),
            crm_name: self.descriptor.crm_name().to_string(),
            crm_ver: self.descriptor.crm_version().to_string(),
            abi_hash: self.descriptor.abi_hash().to_string(),
            signature_hash: self.descriptor.signature_hash().to_string(),
        };
        validate_expected_route_contract(&expected)?;
        Ok(expected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContractReleaseRef {
    contract_schema: String,
    crm_namespace: String,
    crm_name: String,
    crm_version: String,
    descriptor_sha256: ContractDescriptorDigest,
}

impl ContractReleaseRef {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ContractError> {
        Self::from_json_with_limits(bytes, ContractLimits::default())
    }

    pub fn from_json_with_limits(
        bytes: &[u8],
        limits: ContractLimits,
    ) -> Result<Self, ContractError> {
        let value = parse_bounded_json(bytes, limits, JsonDocumentKind::ContractReleaseRef)?;
        let root = ref_object_at(&value, "$")?;
        ensure_ref_keys(
            root,
            "$",
            &["schema", "contract_schema", "crm", "descriptor_sha256"],
        )?;

        let schema = ref_string_at(ref_required(root, "$", "schema")?, "$.schema")?;
        if schema != CONTRACT_RELEASE_REF_SCHEMA {
            return Err(invalid_ref(
                "$.schema",
                format!("expected {CONTRACT_RELEASE_REF_SCHEMA:?}, got {schema:?}"),
            ));
        }

        let contract_schema = ref_string_at(
            ref_required(root, "$", "contract_schema")?,
            "$.contract_schema",
        )?;
        validate_ref_text("$.contract_schema", "contract schema", contract_schema)?;

        let crm = ref_object_at(ref_required(root, "$", "crm")?, "$.crm")?;
        ensure_ref_keys(crm, "$.crm", &["namespace", "name", "version"])?;
        let crm_namespace =
            ref_string_at(ref_required(crm, "$.crm", "namespace")?, "$.crm.namespace")?;
        validate_ref_text("$.crm.namespace", "crm namespace", crm_namespace)?;
        let crm_name = ref_string_at(ref_required(crm, "$.crm", "name")?, "$.crm.name")?;
        validate_ref_text("$.crm.name", "crm name", crm_name)?;
        let crm_version = ref_string_at(ref_required(crm, "$.crm", "version")?, "$.crm.version")?;
        validate_ref_text("$.crm.version", "crm version", crm_version)?;

        let descriptor_sha256 = ref_string_at(
            ref_required(root, "$", "descriptor_sha256")?,
            "$.descriptor_sha256",
        )?;
        let descriptor_sha256 = ContractDescriptorDigest::parse(descriptor_sha256)
            .map_err(|error| invalid_ref("$.descriptor_sha256", error.to_string()))?;

        Ok(Self {
            contract_schema: contract_schema.to_string(),
            crm_namespace: crm_namespace.to_string(),
            crm_name: crm_name.to_string(),
            crm_version: crm_version.to_string(),
            descriptor_sha256,
        })
    }

    pub fn schema(&self) -> &str {
        CONTRACT_RELEASE_REF_SCHEMA
    }

    pub fn contract_schema(&self) -> &str {
        &self.contract_schema
    }

    pub fn crm_namespace(&self) -> &str {
        &self.crm_namespace
    }

    pub fn crm_name(&self) -> &str {
        &self.crm_name
    }

    pub fn crm_version(&self) -> &str {
        &self.crm_version
    }

    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest {
        &self.descriptor_sha256
    }

    pub fn to_canonical_json(&self) -> Result<String, ContractError> {
        Ok(canonical_json(&serde_json::json!({
            "schema": CONTRACT_RELEASE_REF_SCHEMA,
            "contract_schema": self.contract_schema,
            "crm": {
                "namespace": self.crm_namespace,
                "name": self.crm_name,
                "version": self.crm_version,
            },
            "descriptor_sha256": self.descriptor_sha256.as_str(),
        })))
    }

    pub fn verify_release(&self, release: &ContractRelease) -> Result<(), ContractError> {
        verify_ref_field(
            ContractReleaseRefField::ContractSchema,
            self.contract_schema(),
            release.descriptor.contract_schema(),
        )?;
        verify_ref_field(
            ContractReleaseRefField::CrmNamespace,
            self.crm_namespace(),
            release.descriptor.crm_namespace(),
        )?;
        verify_ref_field(
            ContractReleaseRefField::CrmName,
            self.crm_name(),
            release.descriptor.crm_name(),
        )?;
        verify_ref_field(
            ContractReleaseRefField::CrmVersion,
            self.crm_version(),
            release.descriptor.crm_version(),
        )?;
        verify_ref_field(
            ContractReleaseRefField::DescriptorSha256,
            self.descriptor_sha256().as_str(),
            release.descriptor_sha256().as_str(),
        )
    }
}

fn verify_ref_field(
    field: ContractReleaseRefField,
    expected: &str,
    actual: &str,
) -> Result<(), ContractError> {
    if expected == actual {
        return Ok(());
    }
    Err(ContractError::ReleaseRefMismatch {
        field,
        expected: expected.to_string(),
        actual: actual.to_string(),
    })
}

fn ref_required<'a>(
    object: &'a serde_json::Map<String, Value>,
    path: &str,
    key: &'static str,
) -> Result<&'a Value, ContractError> {
    object
        .get(key)
        .ok_or_else(|| invalid_ref(format!("{path}.{key}"), "required field is missing"))
}

fn ensure_ref_keys(
    object: &serde_json::Map<String, Value>,
    path: &str,
    allowed: &[&'static str],
) -> Result<(), ContractError> {
    for key in object.keys() {
        if !allowed.iter().any(|allowed_key| *allowed_key == key) {
            return Err(invalid_ref(
                format!("{path}.{key}"),
                format!("unknown field {key:?}"),
            ));
        }
    }
    Ok(())
}

fn ref_object_at<'a>(
    value: &'a Value,
    path: &str,
) -> Result<&'a serde_json::Map<String, Value>, ContractError> {
    value
        .as_object()
        .ok_or_else(|| invalid_ref(path, "expected object"))
}

fn ref_string_at<'a>(value: &'a Value, path: &str) -> Result<&'a str, ContractError> {
    value
        .as_str()
        .ok_or_else(|| invalid_ref(path, "expected string"))
}

fn validate_ref_text(path: &str, field: &'static str, value: &str) -> Result<(), ContractError> {
    validate_contract_text_field(field, value).map_err(|error| invalid_ref(path, error.to_string()))
}

fn invalid_ref(path: impl Into<String>, message: impl Into<String>) -> ContractError {
    ContractError::InvalidReleaseRef {
        path: path.into(),
        message: message.into(),
    }
}
