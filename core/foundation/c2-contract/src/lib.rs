//! Language-neutral route contract validation and descriptor hashing.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MAX_WIRE_TEXT_BYTES: usize = u8::MAX as usize;
pub const CONTRACT_HASH_HEX_BYTES: usize = 64;
pub const PORTABLE_CONTRACT_SCHEMA: &str = "c-two.contract.v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExpectedRouteContract {
    pub route_name: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
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
    #[error("contract descriptor invalid at {path}: {message}")]
    InvalidDescriptor { path: String, message: String },
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
    if value.as_bytes().len() != CONTRACT_HASH_HEX_BYTES {
        return Err(ContractError::InvalidHash { field });
    }
    if !value
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

pub fn contract_descriptor_sha256_hex(json_bytes: &[u8]) -> Result<String, ContractError> {
    let value: Value = serde_json::from_slice(json_bytes)
        .map_err(|err| ContractError::InvalidJson(err.to_string()))?;
    let canonical = canonical_json(&value);
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(lower_hex(&digest))
}

pub fn validate_portable_contract_descriptor_json(json_bytes: &[u8]) -> Result<(), ContractError> {
    let value: Value = serde_json::from_slice(json_bytes)
        .map_err(|err| ContractError::InvalidJson(err.to_string()))?;
    validate_portable_contract_descriptor_value(&value)
}

pub fn validate_portable_contract_descriptor_value(value: &Value) -> Result<(), ContractError> {
    let root = object_at(value, "$")?;
    ensure_keys(root, "$", &["schema", "crm", "fingerprints", "methods"])?;
    let schema = string_at(required(root, "$", "schema")?, "$.schema")?;
    if schema != PORTABLE_CONTRACT_SCHEMA {
        return Err(invalid(
            "$.schema",
            format!("expected {PORTABLE_CONTRACT_SCHEMA:?}, got {schema:?}"),
        ));
    }

    let crm_path = "$.crm";
    let crm = object_at(required(root, "$", "crm")?, crm_path)?;
    ensure_keys(crm, crm_path, &["namespace", "name", "version"])?;
    let crm_ns = string_at(required(crm, crm_path, "namespace")?, "$.crm.namespace")?;
    let crm_name = string_at(required(crm, crm_path, "name")?, "$.crm.name")?;
    let crm_ver = string_at(required(crm, crm_path, "version")?, "$.crm.version")?;
    validate_crm_tag(crm_ns, crm_name, crm_ver)?;

    validate_fingerprints(required(root, "$", "fingerprints")?, "$.fingerprints")?;

    let methods = array_at(required(root, "$", "methods")?, "$.methods")?;
    let mut method_names = BTreeSet::new();
    for (index, method) in methods.iter().enumerate() {
        let method_path = format!("$.methods[{index}]");
        validate_method_descriptor(method, &method_path, &mut method_names)?;
    }
    Ok(())
}

fn validate_fingerprints(value: &Value, path: &str) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(object, path, &["abi_hash", "signature_hash"])?;
    let abi_hash_path = format!("{path}.abi_hash");
    let abi_hash = string_at(required(object, path, "abi_hash")?, &abi_hash_path)?;
    validate_hash_text(&abi_hash_path, abi_hash)?;
    let signature_hash_path = format!("{path}.signature_hash");
    let signature_hash = string_at(
        required(object, path, "signature_hash")?,
        &signature_hash_path,
    )?;
    validate_hash_text(&signature_hash_path, signature_hash)
}

fn validate_method_descriptor(
    value: &Value,
    path: &str,
    method_names: &mut BTreeSet<String>,
) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(
        object,
        path,
        &["access", "buffer", "name", "parameters", "return", "wire"],
    )?;
    let name_path = format!("{path}.name");
    let name = string_at(required(object, path, "name")?, &name_path)?;
    validate_contract_text_field("method name", name)?;
    if !method_names.insert(name.to_string()) {
        return Err(invalid(path, format!("duplicate method name {name:?}")));
    }

    let access_path = format!("{path}.access");
    let access = string_at(required(object, path, "access")?, &access_path)?;
    if !matches!(access, "read" | "write") {
        return Err(invalid(access_path, "access must be \"read\" or \"write\""));
    }

    let buffer_path = format!("{path}.buffer");
    validate_buffer(required(object, path, "buffer")?, &buffer_path)?;

    let params_path = format!("{path}.parameters");
    let parameters = array_at(required(object, path, "parameters")?, &params_path)?;
    let mut param_names = BTreeSet::new();
    for (index, param) in parameters.iter().enumerate() {
        let param_path = format!("{params_path}[{index}]");
        validate_parameter_descriptor(param, &param_path, &mut param_names)?;
    }

    let return_path = format!("{path}.return");
    validate_annotation(required(object, path, "return")?, &return_path)?;

    let wire_path = format!("{path}.wire");
    let wire = object_at(required(object, path, "wire")?, &wire_path)?;
    let input_path = format!("{wire_path}.input");
    validate_wire_ref(required(wire, &wire_path, "input")?, &input_path)?;
    let output_path = format!("{wire_path}.output");
    validate_wire_ref(required(wire, &wire_path, "output")?, &output_path)?;
    Ok(())
}

fn validate_parameter_descriptor(
    value: &Value,
    path: &str,
    param_names: &mut BTreeSet<String>,
) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    ensure_keys(object, path, &["default", "kind", "name", "type"])?;
    let name_path = format!("{path}.name");
    let name = string_at(required(object, path, "name")?, &name_path)?;
    validate_contract_text_field("parameter name", name)?;
    if !param_names.insert(name.to_string()) {
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

    let default_path = format!("{path}.default");
    validate_default(required(object, path, "default")?, &default_path)?;
    let type_path = format!("{path}.type");
    validate_annotation(required(object, path, "type")?, &type_path)
}

fn validate_default(value: &Value, path: &str) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    match kind {
        "missing" => {
            ensure_keys(object, path, &["kind"])?;
            Ok(())
        }
        "json_scalar" => {
            ensure_keys(object, path, &["kind", "value"])?;
            let value_path = format!("{path}.value");
            let default_value = required(object, path, "value")?;
            if matches!(
                default_value,
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
            ) {
                Ok(())
            } else {
                Err(invalid(
                    value_path,
                    "json_scalar default must be null, bool, number, or string",
                ))
            }
        }
        _ => Err(invalid(
            kind_path,
            "default kind must be missing or json_scalar",
        )),
    }
}

fn validate_annotation(value: &Value, path: &str) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    match kind {
        "none" => {
            ensure_keys(object, path, &["kind"])?;
            Ok(())
        }
        "primitive" => {
            ensure_keys(object, path, &["kind", "name"])?;
            let name_path = format!("{path}.name");
            let name = string_at(required(object, path, "name")?, &name_path)?;
            if matches!(
                name,
                "bool" | "int" | "float" | "str" | "bytes" | "memoryview" | "bytearray"
            ) {
                Ok(())
            } else {
                Err(invalid(
                    name_path,
                    format!("unsupported primitive {name:?}"),
                ))
            }
        }
        "list" => {
            ensure_keys(object, path, &["item", "kind"])?;
            let item_path = format!("{path}.item");
            validate_annotation(required(object, path, "item")?, &item_path)
        }
        "dict" => {
            ensure_keys(object, path, &["key", "kind", "value"])?;
            let key_path = format!("{path}.key");
            validate_annotation(required(object, path, "key")?, &key_path)?;
            let value_path = format!("{path}.value");
            validate_annotation(required(object, path, "value")?, &value_path)
        }
        "tuple_variadic" => {
            ensure_keys(object, path, &["item", "kind"])?;
            let item_path = format!("{path}.item");
            validate_annotation(required(object, path, "item")?, &item_path)
        }
        "tuple" => {
            ensure_keys(object, path, &["items", "kind"])?;
            let items_path = format!("{path}.items");
            let items = array_at(required(object, path, "items")?, &items_path)?;
            if items.is_empty() {
                return Err(invalid(items_path, "tuple items cannot be empty"));
            }
            for (index, item) in items.iter().enumerate() {
                validate_annotation(item, &format!("{items_path}[{index}]"))?;
            }
            Ok(())
        }
        "union" => {
            ensure_keys(object, path, &["items", "kind"])?;
            let items_path = format!("{path}.items");
            let items = array_at(required(object, path, "items")?, &items_path)?;
            if items.is_empty() {
                return Err(invalid(items_path, "union items cannot be empty"));
            }
            for (index, item) in items.iter().enumerate() {
                validate_annotation(item, &format!("{items_path}[{index}]"))?;
            }
            Ok(())
        }
        "codec" => {
            ensure_keys(object, path, &["codec", "kind"])?;
            let codec_path = format!("{path}.codec");
            validate_codec_ref(required(object, path, "codec")?, &codec_path)
        }
        "transferable" => {
            ensure_keys(object, path, &["abi_ref", "kind"])?;
            let abi_path = format!("{path}.abi_ref");
            validate_wire_ref(required(object, path, "abi_ref")?, &abi_path)
        }
        _ => Err(invalid(
            kind_path,
            format!("unsupported annotation kind {kind:?}"),
        )),
    }
}

fn validate_wire_ref(value: &Value, path: &str) -> Result<(), ContractError> {
    if value.is_null() {
        return Ok(());
    }
    if value
        .get("family")
        .and_then(Value::as_str)
        .is_some_and(|family| family == "python-pickle-default")
    {
        return Err(invalid(path, "python-pickle-default is not portable"));
    }
    let object = object_at(value, path)?;
    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    if kind != "codec_ref" {
        return Err(invalid(
            kind_path,
            "portable wire refs must use kind \"codec_ref\"",
        ));
    }
    validate_codec_ref(value, path)
}

fn validate_codec_ref(value: &Value, path: &str) -> Result<(), ContractError> {
    let object = object_at(value, path)?;
    let kind_path = format!("{path}.kind");
    let kind = string_at(required(object, path, "kind")?, &kind_path)?;
    if kind != "codec_ref" {
        return Err(invalid(kind_path, "codec ref kind must be \"codec_ref\""));
    }
    ensure_keys(
        object,
        path,
        &[
            "capabilities",
            "id",
            "kind",
            "media_type",
            "portable",
            "schema",
            "schema_sha256",
            "version",
        ],
    )?;

    validate_identity_value(required(object, path, "id")?, &format!("{path}.id"))?;
    validate_identity_value(
        required(object, path, "version")?,
        &format!("{path}.version"),
    )?;
    if let Some(schema) = object.get("schema") {
        validate_identity_value(schema, &format!("{path}.schema"))?;
    }
    if let Some(media_type) = object.get("media_type") {
        validate_identity_value(media_type, &format!("{path}.media_type"))?;
    }
    if let Some(schema_sha256) = object.get("schema_sha256") {
        let sha_path = format!("{path}.schema_sha256");
        let value = string_at(schema_sha256, &sha_path)?;
        validate_hash_text(&sha_path, value)?;
    }

    let portable_path = format!("{path}.portable");
    let portable = required(object, path, "portable")?
        .as_bool()
        .ok_or_else(|| invalid(&portable_path, "portable must be a boolean"))?;
    if !portable {
        return Err(invalid(
            portable_path,
            "portable codec refs must set portable=true",
        ));
    }

    if let Some(capabilities) = object.get("capabilities") {
        let capabilities_path = format!("{path}.capabilities");
        let items = array_at(capabilities, &capabilities_path)?;
        let mut seen = BTreeSet::new();
        for (index, capability) in items.iter().enumerate() {
            let capability_path = format!("{capabilities_path}[{index}]");
            let value = string_at(capability, &capability_path)?;
            validate_capability_text(&capability_path, value)?;
            if !seen.insert(value.to_string()) {
                return Err(invalid(
                    capability_path,
                    format!("duplicate capability {value:?}"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_buffer(value: &Value, path: &str) -> Result<(), ContractError> {
    if value.is_null() {
        return Ok(());
    }
    let value = string_at(value, path)?;
    if matches!(value, "view" | "hold") {
        Ok(())
    } else {
        Err(invalid(path, "buffer must be null, \"view\", or \"hold\""))
    }
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

fn validate_identity_value(value: &Value, path: &str) -> Result<(), ContractError> {
    let value = string_at(value, path)?;
    if value.is_empty() {
        return Err(invalid(path, "cannot be empty"));
    }
    if value.as_bytes().len() > MAX_WIRE_TEXT_BYTES {
        return Err(invalid(
            path,
            format!(
                "cannot exceed {MAX_WIRE_TEXT_BYTES} bytes: {}",
                value.as_bytes().len()
            ),
        ));
    }
    if value.trim() != value {
        return Err(invalid(
            path,
            "cannot contain leading or trailing whitespace",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(path, "cannot contain control characters"));
    }
    let mut chars = value.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric()) {
        return Err(invalid(path, "must start with an ASCII letter or digit"));
    }
    if !chars
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '/' | '+' | '-'))
    {
        return Err(invalid(path, "contains unsupported characters"));
    }
    Ok(())
}

fn validate_capability_text(path: &str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(invalid(path, "capability cannot be empty"));
    }
    if value.trim() != value {
        return Err(invalid(
            path,
            "capability cannot contain leading or trailing whitespace",
        ));
    }
    let mut chars = value.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric()) {
        return Err(invalid(
            path,
            "capability must start with an ASCII letter or digit",
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-')) {
        return Err(invalid(path, "capability contains unsupported characters"));
    }
    Ok(())
}

fn validate_hash_text(path: &str, value: &str) -> Result<(), ContractError> {
    if value.as_bytes().len() != CONTRACT_HASH_HEX_BYTES {
        return Err(invalid(path, "must be exactly 64 lowercase hex bytes"));
    }
    if !value
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

fn validate_wire_len(field: &'static str, value: &str) -> Result<(), ContractError> {
    let actual = value.as_bytes().len();
    if actual > MAX_WIRE_TEXT_BYTES {
        return Err(ContractError::TooLong {
            field,
            max: MAX_WIRE_TEXT_BYTES,
            actual,
        });
    }
    Ok(())
}

fn canonical_json(value: &Value) -> String {
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
            let body = map
                .iter()
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

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
    fn call_route_key_rejects_empty_like_named_route() {
        assert!(validate_call_route_key("route name", "").is_err());
        assert!(validate_named_route_name("route name", "").is_err());
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

    #[test]
    fn portable_descriptor_accepts_codec_refs_and_null_wire() {
        let descriptor = valid_portable_descriptor();
        validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn portable_descriptor_accepts_route_contract_fingerprints() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["fingerprints"] = serde_json::json!({
            "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        });

        validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn portable_descriptor_rejects_invalid_route_contract_fingerprints() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["fingerprints"] = serde_json::json!({
            "abi_hash": "bad",
            "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        });

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("abi_hash"));
    }

    #[test]
    fn portable_descriptor_requires_route_contract_fingerprints() {
        let mut descriptor = valid_portable_descriptor();
        descriptor.as_object_mut().unwrap().remove("fingerprints");

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("fingerprints"));
    }

    #[test]
    fn portable_descriptor_rejects_wrong_schema() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["schema"] = Value::String("c-two.python.crm.descriptor.v2".to_string());

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("schema"));
    }

    #[test]
    fn portable_descriptor_rejects_pickle_wire_refs() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["input"] = serde_json::json!({
            "family": "python-pickle-default",
            "kind": "builtin",
            "portable": false,
            "version": "pickle-protocol-5"
        });

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("python-pickle-default"));
    }

    #[test]
    fn portable_descriptor_rejects_custom_wire_refs() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["output"] = serde_json::json!({
            "kind": "custom_id",
            "value": "legacy"
        });

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("codec_ref"));
    }

    #[test]
    fn portable_descriptor_rejects_nonportable_codec_refs() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["output"]["portable"] = Value::Bool(false);

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("portable"));
    }

    #[test]
    fn portable_descriptor_rejects_non_string_codec_identity_fields() {
        for (field, value) in [
            ("version", serde_json::json!(1)),
            ("schema", serde_json::json!(["example.schema.v1"])),
            ("schema_sha256", serde_json::json!(null)),
            ("media_type", serde_json::json!(false)),
        ] {
            let mut descriptor = valid_portable_descriptor();
            descriptor["methods"][0]["wire"]["output"][field] = value;

            let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
                .unwrap_err();
            assert!(
                err.to_string().contains(&format!("wire.output.{field}")),
                "unexpected error for {field}: {err}"
            );
            assert!(
                err.to_string().contains("expected string"),
                "unexpected error for {field}: {err}"
            );
        }
    }

    #[test]
    fn portable_descriptor_rejects_malformed_codec_capabilities() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["output"]["capabilities"] =
            Value::String("bytes".to_string());

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("wire.output.capabilities"));
        assert!(err.to_string().contains("expected array"));

        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["output"]["capabilities"] =
            serde_json::json!(["bytes", 1]);

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("wire.output.capabilities[1]"));
        assert!(err.to_string().contains("expected string"));
    }

    #[test]
    fn portable_descriptor_rejects_bad_annotation_shape() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["parameters"][0]["type"] = serde_json::json!({
            "kind": "list"
        });

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("item"));
    }

    #[test]
    fn portable_descriptor_rejects_unknown_fields() {
        let mut descriptor = valid_portable_descriptor();
        descriptor["methods"][0]["wire"]["output"]["schem_sha256"] =
            Value::String("typo".to_string());

        let err = validate_portable_contract_descriptor_json(descriptor.to_string().as_bytes())
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    fn valid_portable_descriptor() -> Value {
        serde_json::json!({
            "schema": "c-two.contract.v1",
            "crm": {
                "namespace": "test.contract",
                "name": "Portable",
                "version": "0.1.0"
            },
            "fingerprints": {
                "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            },
            "methods": [
                {
                    "access": "write",
                    "buffer": "view",
                    "name": "echo",
                    "parameters": [
                        {
                            "name": "value",
                            "kind": "POSITIONAL_OR_KEYWORD",
                            "default": {"kind": "missing"},
                            "type": {
                                "kind": "codec",
                                "codec": codec_ref()
                            }
                        }
                    ],
                    "return": {
                        "kind": "codec",
                        "codec": codec_ref()
                    },
                    "wire": {
                        "input": codec_ref(),
                        "output": codec_ref()
                    }
                },
                {
                    "access": "read",
                    "buffer": "view",
                    "name": "ping",
                    "parameters": [],
                    "return": {"kind": "none"},
                    "wire": {
                        "input": null,
                        "output": null
                    }
                }
            ]
        })
    }

    fn codec_ref() -> Value {
        serde_json::json!({
            "kind": "codec_ref",
            "id": "org.example.codec",
            "version": "1",
            "schema": "example.schema.v1",
            "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "capabilities": ["bytes"],
            "media_type": "application/octet-stream",
            "portable": true
        })
    }
}
