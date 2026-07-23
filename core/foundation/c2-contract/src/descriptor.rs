use crate::{
    CONTRACT_HASH_HEX_BYTES, ContractDescriptorDigest, ContractError, ContractFingerprintField,
    PORTABLE_CONTRACT_SCHEMA, validate_contract_text_field,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const CONTRACT_ABI_SCHEMA: &str = "c-two.contract-abi.v2";
const CONTRACT_SIGNATURE_SCHEMA: &str = "c-two.contract-signature.v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodAccess {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedFastDbSpec {
    direction: BindingDirection,
    outer_path: String,
    canonical_json: String,
}

impl NestedFastDbSpec {
    pub fn direction(&self) -> BindingDirection {
        self.direction
    }

    pub fn outer_path(&self) -> &str {
        &self.outer_path
    }

    /// Returns deterministic C-Two extraction bytes for the opaque nested JSON
    /// value. FastDB Core remains authoritative for FastDB canonicalization.
    pub fn canonical_json(&self) -> &str {
        &self.canonical_json
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMethodDescriptor {
    name: String,
    access: MethodAccess,
    input: Option<NestedFastDbSpec>,
    output: Option<NestedFastDbSpec>,
}

impl ValidatedMethodDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn access(&self) -> MethodAccess {
        self.access
    }

    pub fn input(&self) -> Option<&NestedFastDbSpec> {
        self.input.as_ref()
    }

    pub fn output(&self) -> Option<&NestedFastDbSpec> {
        self.output.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractFingerprints {
    abi_hash: String,
    signature_hash: String,
}

impl ContractFingerprints {
    pub fn abi_hash(&self) -> &str {
        &self.abi_hash
    }

    pub fn signature_hash(&self) -> &str {
        &self.signature_hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedContractDescriptor {
    canonical_json: String,
    contract_schema: String,
    crm_namespace: String,
    crm_name: String,
    crm_version: String,
    abi_hash: String,
    signature_hash: String,
    methods: Vec<ValidatedMethodDescriptor>,
    descriptor_sha256: ContractDescriptorDigest,
}

impl ValidatedContractDescriptor {
    pub fn from_json(json_bytes: &[u8]) -> Result<Self, ContractError> {
        let value = parse_json(json_bytes)?;
        let parsed = parse_outer_descriptor(&value)?;
        let derived = derive_contract_fingerprints_value(&value, &parsed)?;
        verify_fingerprint(
            ContractFingerprintField::AbiHash,
            &parsed.abi_hash,
            derived.abi_hash(),
        )?;
        verify_fingerprint(
            ContractFingerprintField::SignatureHash,
            &parsed.signature_hash,
            derived.signature_hash(),
        )?;

        let canonical_json = canonical_json(&value);
        let descriptor_sha256 =
            ContractDescriptorDigest::parse(sha256_hex(canonical_json.as_bytes()))?;

        Ok(Self {
            canonical_json,
            contract_schema: PORTABLE_CONTRACT_SCHEMA.to_string(),
            crm_namespace: parsed.crm_namespace,
            crm_name: parsed.crm_name,
            crm_version: parsed.crm_version,
            abi_hash: derived.abi_hash,
            signature_hash: derived.signature_hash,
            methods: parsed.methods,
            descriptor_sha256,
        })
    }

    pub fn canonical_json(&self) -> &str {
        &self.canonical_json
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

    pub fn abi_hash(&self) -> &str {
        &self.abi_hash
    }

    pub fn signature_hash(&self) -> &str {
        &self.signature_hash
    }

    pub fn methods(&self) -> &[ValidatedMethodDescriptor] {
        &self.methods
    }

    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest {
        &self.descriptor_sha256
    }
}

struct ParsedOuterDescriptor {
    crm_namespace: String,
    crm_name: String,
    crm_version: String,
    abi_hash: String,
    signature_hash: String,
    methods: Vec<ValidatedMethodDescriptor>,
}

pub fn derive_contract_fingerprints_json(
    json_bytes: &[u8],
) -> Result<ContractFingerprints, ContractError> {
    let value = parse_json(json_bytes)?;
    let parsed = parse_outer_descriptor(&value)?;
    derive_contract_fingerprints_value(&value, &parsed)
}

pub fn contract_descriptor_sha256_hex(json_bytes: &[u8]) -> Result<String, ContractError> {
    let value = parse_json(json_bytes)?;
    Ok(sha256_hex(canonical_json(&value).as_bytes()))
}

pub fn validate_portable_contract_descriptor_json(json_bytes: &[u8]) -> Result<(), ContractError> {
    ValidatedContractDescriptor::from_json(json_bytes).map(|_| ())
}

pub fn validate_portable_contract_descriptor_value(value: &Value) -> Result<(), ContractError> {
    let canonical = canonical_json(value);
    ValidatedContractDescriptor::from_json(canonical.as_bytes()).map(|_| ())
}

fn parse_json(json_bytes: &[u8]) -> Result<Value, ContractError> {
    serde_json::from_slice(json_bytes)
        .map_err(|error| ContractError::InvalidJson(error.to_string()))
}

fn parse_outer_descriptor(value: &Value) -> Result<ParsedOuterDescriptor, ContractError> {
    let root = object_at(value, "$")?;
    ensure_keys(root, "$", &["schema", "crm", "fingerprints", "methods"])?;
    let schema = string_at(required(root, "$", "schema")?, "$.schema")?;
    if schema != PORTABLE_CONTRACT_SCHEMA {
        return Err(invalid(
            "$.schema",
            format!("expected {PORTABLE_CONTRACT_SCHEMA:?}, got {schema:?}"),
        ));
    }

    let crm = object_at(required(root, "$", "crm")?, "$.crm")?;
    ensure_keys(crm, "$.crm", &["namespace", "name", "version"])?;
    let crm_namespace = validated_text(
        crm,
        "$.crm",
        "namespace",
        "$.crm.namespace",
        "crm namespace",
    )?;
    let crm_name = validated_text(crm, "$.crm", "name", "$.crm.name", "crm name")?;
    let crm_version = validated_text(crm, "$.crm", "version", "$.crm.version", "crm version")?;

    let fingerprints = object_at(required(root, "$", "fingerprints")?, "$.fingerprints")?;
    ensure_keys(
        fingerprints,
        "$.fingerprints",
        &["abi_hash", "signature_hash"],
    )?;
    let abi_hash = fingerprint_text(fingerprints, "abi_hash", "$.fingerprints.abi_hash")?;
    let signature_hash = fingerprint_text(
        fingerprints,
        "signature_hash",
        "$.fingerprints.signature_hash",
    )?;

    let method_values = array_at(required(root, "$", "methods")?, "$.methods")?;
    let mut method_names = BTreeSet::new();
    let mut methods = Vec::with_capacity(method_values.len());
    for (index, method) in method_values.iter().enumerate() {
        let path = format!("$.methods[{index}]");
        methods.push(parse_method_descriptor(method, &path, &mut method_names)?);
    }

    Ok(ParsedOuterDescriptor {
        crm_namespace,
        crm_name,
        crm_version,
        abi_hash,
        signature_hash,
        methods,
    })
}

fn parse_method_descriptor(
    value: &Value,
    path: &str,
    method_names: &mut BTreeSet<String>,
) -> Result<ValidatedMethodDescriptor, ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(
        object,
        path,
        &["access", "name", "parameters", "return", "bindings"],
    )?;

    let name_path = format!("{path}.name");
    let name = validated_text(object, path, "name", &name_path, "method name")?;
    if !method_names.insert(name.clone()) {
        return Err(invalid(path, format!("duplicate method name {name:?}")));
    }

    let access_path = format!("{path}.access");
    let access = match string_at(required(object, path, "access")?, &access_path)? {
        "read" => MethodAccess::Read,
        "write" => MethodAccess::Write,
        _ => {
            return Err(invalid(access_path, "access must be \"read\" or \"write\""));
        }
    };

    let parameters_path = format!("{path}.parameters");
    let parameters = array_at(required(object, path, "parameters")?, &parameters_path)?;
    let mut parameter_names = BTreeSet::new();
    for (index, parameter) in parameters.iter().enumerate() {
        validate_payload_parameter(
            parameter,
            &format!("{parameters_path}[{index}]"),
            &mut parameter_names,
        )?;
    }

    let return_path = format!("{path}.return");
    let return_kind = validate_return(required(object, path, "return")?, &return_path)?;

    let bindings_path = format!("{path}.bindings");
    let bindings = object_at(required(object, path, "bindings")?, &bindings_path)?;
    ensure_keys(bindings, &bindings_path, &["input", "output"])?;
    let input = parse_binding(
        required(bindings, &bindings_path, "input")?,
        &format!("{bindings_path}.input"),
        BindingDirection::Input,
    )?;
    let output = parse_binding(
        required(bindings, &bindings_path, "output")?,
        &format!("{bindings_path}.output"),
        BindingDirection::Output,
    )?;

    match input {
        None if !parameters.is_empty() => {
            return Err(invalid(
                parameters_path,
                "null input binding requires zero parameters",
            ));
        }
        Some(_) if parameters.len() != 1 => {
            return Err(invalid(
                parameters_path,
                "FastDB input binding requires exactly one payload parameter",
            ));
        }
        _ => {}
    }

    match (output.is_some(), return_kind) {
        (false, ReturnKind::Payload) => {
            return Err(invalid(
                return_path,
                "null output binding requires return kind \"none\"",
            ));
        }
        (true, ReturnKind::None) => {
            return Err(invalid(
                return_path,
                "FastDB output binding requires return kind \"payload\"",
            ));
        }
        _ => {}
    }

    Ok(ValidatedMethodDescriptor {
        name,
        access,
        input,
        output,
    })
}

fn validate_payload_parameter(
    value: &Value,
    path: &str,
    parameter_names: &mut BTreeSet<String>,
) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(object, path, &["name", "kind", "default", "type"])?;

    let name_path = format!("{path}.name");
    let name = validated_text(object, path, "name", &name_path, "parameter name")?;
    if !parameter_names.insert(name.clone()) {
        return Err(invalid(path, format!("duplicate parameter name {name:?}")));
    }

    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    if !matches!(
        kind,
        "POSITIONAL_ONLY" | "POSITIONAL_OR_KEYWORD" | "KEYWORD_ONLY"
    ) {
        return Err(invalid(
            kind_path,
            "parameter kind must be POSITIONAL_ONLY, POSITIONAL_OR_KEYWORD, or KEYWORD_ONLY",
        ));
    }

    validate_single_kind_object(
        required(object, path, "default")?,
        &format!("{path}.default"),
        "missing",
    )?;
    validate_single_kind_object(
        required(object, path, "type")?,
        &format!("{path}.type"),
        "payload",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    None,
    Payload,
}

fn validate_return(value: &Value, path: &str) -> Result<ReturnKind, ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(object, path, &["kind"])?;
    let kind_path = format!("{path}.kind");
    match string_at(required(object, path, "kind")?, &kind_path)? {
        "none" => Ok(ReturnKind::None),
        "payload" => Ok(ReturnKind::Payload),
        _ => Err(invalid(
            kind_path,
            "return kind must be \"none\" or \"payload\"",
        )),
    }
}

fn validate_single_kind_object(
    value: &Value,
    path: &str,
    expected: &'static str,
) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(object, path, &["kind"])?;
    let kind_path = format!("{path}.kind");
    let actual = string_at(required(object, path, "kind")?, &kind_path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid(
            kind_path,
            format!("expected kind {expected:?}, got {actual:?}"),
        ))
    }
}

fn parse_binding(
    value: &Value,
    path: &str,
    direction: BindingDirection,
) -> Result<Option<NestedFastDbSpec>, ContractError> {
    if value.is_null() {
        return Ok(None);
    }

    let object = object_at(value, path)?;
    ensure_keys(object, path, &["kind", "spec"])?;
    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    if kind != "fastdb" {
        return Err(invalid(kind_path, "binding kind must be \"fastdb\""));
    }

    let spec_path = format!("{path}.spec");
    let spec = required(object, path, "spec")?;
    Ok(Some(NestedFastDbSpec {
        direction,
        outer_path: spec_path,
        canonical_json: canonical_json(spec),
    }))
}

fn derive_contract_fingerprints_value(
    value: &Value,
    parsed: &ParsedOuterDescriptor,
) -> Result<ContractFingerprints, ContractError> {
    let root = object_at(value, "$")?;
    let crm = required(root, "$", "crm")?.clone();
    let methods = array_at(required(root, "$", "methods")?, "$.methods")?;

    let abi_methods = methods
        .iter()
        .enumerate()
        .map(|(index, method)| {
            let path = format!("$.methods[{index}]");
            let object = object_at(method, &path)?;
            Ok(serde_json::json!({
                "name": required(object, &path, "name")?.clone(),
                "bindings": required(object, &path, "bindings")?.clone(),
            }))
        })
        .collect::<Result<Vec<_>, ContractError>>()?;
    let signature_methods = methods
        .iter()
        .enumerate()
        .map(|(index, method)| {
            let path = format!("$.methods[{index}]");
            let object = object_at(method, &path)?;
            Ok(serde_json::json!({
                "name": required(object, &path, "name")?.clone(),
                "access": required(object, &path, "access")?.clone(),
                "parameters": required(object, &path, "parameters")?.clone(),
                "return": required(object, &path, "return")?.clone(),
            }))
        })
        .collect::<Result<Vec<_>, ContractError>>()?;

    let abi_projection = serde_json::json!({
        "schema": CONTRACT_ABI_SCHEMA,
        "crm": crm.clone(),
        "methods": abi_methods,
    });
    let signature_projection = serde_json::json!({
        "schema": CONTRACT_SIGNATURE_SCHEMA,
        "crm": crm,
        "methods": signature_methods,
    });

    // Parsing already validated the complete outer shape. Reading a field from
    // `parsed` here makes that ordering explicit and prevents this function
    // from becoming a second, weaker descriptor parser.
    debug_assert_eq!(parsed.methods.len(), methods.len());

    Ok(ContractFingerprints {
        abi_hash: sha256_hex(canonical_json(&abi_projection).as_bytes()),
        signature_hash: sha256_hex(canonical_json(&signature_projection).as_bytes()),
    })
}

fn verify_fingerprint(
    field: ContractFingerprintField,
    supplied: &str,
    derived: &str,
) -> Result<(), ContractError> {
    if supplied == derived {
        Ok(())
    } else {
        Err(ContractError::FingerprintMismatch {
            field,
            expected: derived.to_string(),
            actual: supplied.to_string(),
        })
    }
}

fn fingerprint_text(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
    path: &str,
) -> Result<String, ContractError> {
    let value = string_at(required(object, "$.fingerprints", key)?, path)?;
    validate_hash_text(path, value)?;
    Ok(value.to_string())
}

fn validated_text(
    object: &serde_json::Map<String, Value>,
    object_path: &str,
    key: &'static str,
    path: &str,
    field: &'static str,
) -> Result<String, ContractError> {
    let value = string_at(required(object, object_path, key)?, path)?;
    validate_contract_text_field(field, value).map_err(|error| invalid(path, error.to_string()))?;
    Ok(value.to_string())
}

fn required<'a>(
    object: &'a serde_json::Map<String, Value>,
    path: &str,
    key: &'static str,
) -> Result<&'a Value, ContractError> {
    object
        .get(key)
        .ok_or_else(|| invalid(format!("{path}.{key}"), "required field is missing"))
}

fn ensure_keys(
    object: &serde_json::Map<String, Value>,
    path: &str,
    allowed: &[&'static str],
) -> Result<(), ContractError> {
    for key in object.keys() {
        if !allowed.iter().any(|allowed_key| *allowed_key == key) {
            return Err(invalid(
                format!("{path}.{key}"),
                format!("unknown field {key:?}"),
            ));
        }
    }
    Ok(())
}

fn object_at<'a>(
    value: &'a Value,
    path: &str,
) -> Result<&'a serde_json::Map<String, Value>, ContractError> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, "expected object"))
}

fn array_at<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, ContractError> {
    value
        .as_array()
        .ok_or_else(|| invalid(path, "expected array"))
}

fn string_at<'a>(value: &'a Value, path: &str) -> Result<&'a str, ContractError> {
    value
        .as_str()
        .ok_or_else(|| invalid(path, "expected string"))
}

fn validate_hash_text(path: &str, value: &str) -> Result<(), ContractError> {
    if value.len() != CONTRACT_HASH_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(path, "must be exactly 64 lowercase hex bytes"));
    }
    Ok(())
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> ContractError {
    ContractError::InvalidDescriptor {
        path: path.into(),
        message: message.into(),
    }
}

pub(crate) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
        Value::Array(values) => {
            let body = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        Value::Object(map) => {
            // Canonical identity cannot depend on serde_json's preserve_order
            // feature being unified into the downstream dependency graph.
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            let body = entries
                .into_iter()
                .map(|(key, value)| {
                    let encoded_key = serde_json::to_string(key).expect("JSON object key encodes");
                    format!("{encoded_key}:{}", canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    lower_hex(&digest)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
