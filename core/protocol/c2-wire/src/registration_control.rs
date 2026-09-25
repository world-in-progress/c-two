use serde::{Deserialize, Serialize};

use crate::msg_type::MsgType;

pub const PENDING_ROUTE_REJECT_NOT_FOUND: &str = "not_found";
pub const PENDING_ROUTE_REJECT_TOKEN_MISMATCH: &str = "token_mismatch";
pub const PENDING_ROUTE_REJECT_INVALID: &str = "invalid";
pub const ROUTE_CONTRACT_REJECT_NOT_FOUND: &str = "not_found";
pub const ROUTE_CONTRACT_REJECT_INVALID: &str = "invalid";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRouteAttestationRequest {
    pub route_name: String,
    pub registration_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteContractRequest {
    pub route_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRouteAttestation {
    pub route_name: String,
    pub route_uid: String,
    pub route_revision: u64,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub method_names: Vec<String>,
    pub max_payload_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PendingRouteAttestationResponse {
    Attested { contract: PendingRouteAttestation },
    Rejected { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RouteContractResponse {
    Attested { contract: PendingRouteAttestation },
    Rejected { code: String, message: String },
}

pub fn encode_pending_route_attestation_request(
    route_name: &str,
    registration_token: &str,
) -> Result<Vec<u8>, String> {
    validate_route_name(route_name)?;
    validate_registration_token(registration_token)?;
    let request = PendingRouteAttestationRequest {
        route_name: route_name.to_string(),
        registration_token: registration_token.to_string(),
    };
    encode_json_payload(MsgType::PendingRouteAttest, &request)
}

pub fn decode_pending_route_attestation_request(
    payload: &[u8],
) -> Result<PendingRouteAttestationRequest, String> {
    let body = split_tag(payload, MsgType::PendingRouteAttest)?;
    let request: PendingRouteAttestationRequest =
        serde_json::from_slice(body).map_err(|err| err.to_string())?;
    validate_route_name(&request.route_name)?;
    validate_registration_token(&request.registration_token)?;
    Ok(request)
}

pub fn encode_pending_route_attestation_response(
    response: &PendingRouteAttestationResponse,
) -> Result<Vec<u8>, String> {
    if let PendingRouteAttestationResponse::Attested { contract } = response {
        validate_attested_contract(contract)?;
    }
    encode_json_payload(MsgType::PendingRouteAttestAck, response)
}

pub fn decode_pending_route_attestation_response(
    payload: &[u8],
) -> Result<PendingRouteAttestationResponse, String> {
    let body = split_tag(payload, MsgType::PendingRouteAttestAck)?;
    let response: PendingRouteAttestationResponse =
        serde_json::from_slice(body).map_err(|err| err.to_string())?;
    if let PendingRouteAttestationResponse::Attested { contract } = &response {
        validate_attested_contract(contract)?;
    }
    Ok(response)
}

pub fn encode_route_contract_request(route_name: &str) -> Result<Vec<u8>, String> {
    validate_route_name(route_name)?;
    let request = RouteContractRequest {
        route_name: route_name.to_string(),
    };
    encode_json_payload(MsgType::RouteContract, &request)
}

pub fn decode_route_contract_request(payload: &[u8]) -> Result<RouteContractRequest, String> {
    let body = split_tag(payload, MsgType::RouteContract)?;
    let request: RouteContractRequest =
        serde_json::from_slice(body).map_err(|err| err.to_string())?;
    validate_route_name(&request.route_name)?;
    Ok(request)
}

pub fn encode_route_contract_response(response: &RouteContractResponse) -> Result<Vec<u8>, String> {
    if let RouteContractResponse::Attested { contract } = response {
        validate_attested_contract(contract)?;
    }
    encode_json_payload(MsgType::RouteContractAck, response)
}

pub fn decode_route_contract_response(payload: &[u8]) -> Result<RouteContractResponse, String> {
    let body = split_tag(payload, MsgType::RouteContractAck)?;
    let response: RouteContractResponse =
        serde_json::from_slice(body).map_err(|err| err.to_string())?;
    if let RouteContractResponse::Attested { contract } = &response {
        validate_attested_contract(contract)?;
    }
    Ok(response)
}

fn encode_json_payload<T: Serialize>(tag: MsgType, value: &T) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    let mut payload = Vec::with_capacity(1 + json.len());
    payload.push(tag.as_byte());
    payload.extend_from_slice(&json);
    Ok(payload)
}

fn split_tag(payload: &[u8], expected: MsgType) -> Result<&[u8], String> {
    let (tag, body) = payload
        .split_first()
        .ok_or_else(|| "pending route attestation payload is empty".to_string())?;
    if *tag != expected.as_byte() {
        return Err("pending route attestation payload tag mismatch".to_string());
    }
    Ok(body)
}

fn validate_route_name(route_name: &str) -> Result<(), String> {
    c2_contract::validate_call_route_key("route_name", route_name).map_err(|err| err.to_string())
}

fn validate_registration_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("registration_token must not be empty".to_string());
    }
    if token.len() > c2_contract::MAX_WIRE_TEXT_BYTES {
        return Err(format!(
            "registration_token is too long: {} bytes > {}",
            token.len(),
            c2_contract::MAX_WIRE_TEXT_BYTES
        ));
    }
    if token.bytes().any(|b| b <= 0x20 || b == b'/' || b == b'\\') {
        return Err("registration_token contains an invalid character".to_string());
    }
    Ok(())
}

fn validate_attested_contract(contract: &PendingRouteAttestation) -> Result<(), String> {
    let expected = c2_contract::ExpectedRouteContract {
        route_name: contract.route_name.clone(),
        crm_ns: contract.crm_ns.clone(),
        crm_name: contract.crm_name.clone(),
        crm_ver: contract.crm_ver.clone(),
        abi_hash: contract.abi_hash.clone(),
        signature_hash: contract.signature_hash.clone(),
    };
    c2_contract::validate_expected_route_contract(&expected).map_err(|err| err.to_string())?;
    validate_route_uid(&contract.route_uid)?;
    if contract.max_payload_size == 0 {
        return Err("max_payload_size must be > 0".to_string());
    }
    if contract.method_names.len() > crate::handshake::MAX_METHODS {
        return Err(format!(
            "method count is too large: {} > {}",
            contract.method_names.len(),
            crate::handshake::MAX_METHODS
        ));
    }
    for method_name in &contract.method_names {
        let len = method_name.len();
        if len > u8::MAX as usize {
            return Err(format!("method name is too long: {len} bytes > 255"));
        }
    }
    Ok(())
}

fn validate_route_uid(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("route_uid must not be empty".to_string());
    }
    if value.len() > c2_contract::MAX_WIRE_TEXT_BYTES {
        return Err(format!(
            "route_uid is too long: {} bytes > {}",
            value.len(),
            c2_contract::MAX_WIRE_TEXT_BYTES
        ));
    }
    if value.bytes().any(|b| b <= 0x20 || b == b'/' || b == b'\\') {
        return Err("route_uid contains an invalid character".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> PendingRouteAttestation {
        PendingRouteAttestation {
            route_name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
            crm_ns: "test.echo".to_string(),
            crm_name: "Echo".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .to_string(),
            method_names: vec!["ping".to_string()],
            max_payload_size: 1024,
        }
    }

    #[test]
    fn pending_route_request_round_trips() {
        let encoded = encode_pending_route_attestation_request("grid", "abc123").unwrap();
        let decoded = decode_pending_route_attestation_request(&encoded).unwrap();

        assert_eq!(decoded.route_name, "grid");
        assert_eq!(decoded.registration_token, "abc123");
    }

    #[test]
    fn pending_route_request_matches_canonical_fixture() {
        let encoded = encode_pending_route_attestation_request("grid", "abc123").unwrap();
        let expected = b"\x0a{\"route_name\":\"grid\",\"registration_token\":\"abc123\"}".to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(
            decode_pending_route_attestation_request(&expected).unwrap(),
            PendingRouteAttestationRequest {
                route_name: "grid".to_string(),
                registration_token: "abc123".to_string(),
            }
        );
    }

    #[test]
    fn pending_route_response_round_trips() {
        let response = PendingRouteAttestationResponse::Attested {
            contract: contract(),
        };
        let encoded = encode_pending_route_attestation_response(&response).unwrap();

        assert_eq!(
            decode_pending_route_attestation_response(&encoded).unwrap(),
            response
        );
    }

    #[test]
    fn pending_route_response_matches_canonical_fixture() {
        let response = PendingRouteAttestationResponse::Attested {
            contract: contract(),
        };
        let encoded = encode_pending_route_attestation_response(&response).unwrap();
        let expected = concat!(
            "\x0b",
            "{\"status\":\"attested\",\"contract\":{",
            "\"route_name\":\"grid\",",
            "\"route_uid\":\"grid-route-uid-0001\",",
            "\"route_revision\":1,",
            "\"crm_ns\":\"test.echo\",",
            "\"crm_name\":\"Echo\",",
            "\"crm_ver\":\"0.1.0\",",
            "\"abi_hash\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",",
            "\"signature_hash\":\"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\",",
            "\"method_names\":[\"ping\"],",
            "\"max_payload_size\":1024",
            "}}",
        )
        .as_bytes()
        .to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(
            decode_pending_route_attestation_response(&expected).unwrap(),
            response
        );
    }

    #[test]
    fn pending_route_request_rejects_empty_token() {
        assert!(encode_pending_route_attestation_request("grid", "").is_err());
    }

    #[test]
    fn route_contract_request_matches_canonical_fixture() {
        let encoded = encode_route_contract_request("grid").unwrap();
        let expected = b"\x0c{\"route_name\":\"grid\"}".to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(
            decode_route_contract_request(&expected).unwrap(),
            RouteContractRequest {
                route_name: "grid".to_string(),
            }
        );
    }

    #[test]
    fn route_contract_response_matches_canonical_fixture() {
        let response = RouteContractResponse::Attested {
            contract: contract(),
        };
        let encoded = encode_route_contract_response(&response).unwrap();
        let expected = concat!(
            "\x0d",
            "{\"status\":\"attested\",\"contract\":{",
            "\"route_name\":\"grid\",",
            "\"route_uid\":\"grid-route-uid-0001\",",
            "\"route_revision\":1,",
            "\"crm_ns\":\"test.echo\",",
            "\"crm_name\":\"Echo\",",
            "\"crm_ver\":\"0.1.0\",",
            "\"abi_hash\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",",
            "\"signature_hash\":\"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\",",
            "\"method_names\":[\"ping\"],",
            "\"max_payload_size\":1024",
            "}}",
        )
        .as_bytes()
        .to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(decode_route_contract_response(&expected).unwrap(), response);
    }
}
