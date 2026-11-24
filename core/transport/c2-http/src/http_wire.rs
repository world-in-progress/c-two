use c2_config::CallMetadata;
use c2_error::{C2Error, ErrorCode};
use reqwest::header::HeaderMap;

use crate::client::HttpError;

pub const CALL_PATH: &str = "/_c2/calls/call";
pub const KEYED_PATH: &str = "/_c2/calls/keyed";
pub const RECOVER_PATH: &str = "/_c2/calls/recover";
pub const OUTCOME_PATH: &str = "/_c2/calls/outcome";
pub const TOPIC_SUBSCRIBE_PATH: &str = "/_c2/topics/subscribe";
pub const TOPIC_CONFIRM_PATH: &str = "/_c2/topics/confirm";

const METADATA_PREFIX: &str = "x-c2-meta-";
pub(crate) const MAX_CONTROL_ERROR_BYTES: usize = c2_wire::rpc::MAX_QUERY_CALL_ERROR_BYTES;
pub(crate) const TOPIC_ADMISSION_REQUIRED_HEADER: &str = "x-c2-topic-admission-required";
pub(crate) const TOPIC_HEAD_SEQ_HEADER: &str = "x-c2-topic-head-seq";

pub(crate) fn add_metadata(
    mut request: reqwest::RequestBuilder,
    metadata: &CallMetadata,
) -> reqwest::RequestBuilder {
    for (key, value) in metadata.iter() {
        request = request.header(format!("{METADATA_PREFIX}{key}"), value);
    }
    request
}

#[cfg(feature = "relay")]
pub(crate) fn metadata_from_headers(headers: &HeaderMap) -> Result<CallMetadata, C2Error> {
    fn invalid(message: impl Into<String>) -> C2Error {
        C2Error::new(ErrorCode::ProtocolViolation, message).with_details(
            std::collections::BTreeMap::from([
                ("stage".into(), "call_metadata_validation".into()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
            ]),
        )
    }

    let names = headers.keys().filter_map(|name| {
        name.as_str()
            .strip_prefix(METADATA_PREFIX)
            .map(|key| (name, key))
    });
    let mut entries = Vec::new();
    for (name, key) in names {
        let mut values = headers.get_all(name).iter();
        let value = values
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| invalid("call metadata requires an ASCII header value"))?;
        if values.next().is_some() {
            return Err(invalid("duplicate call metadata header"));
        }
        entries.push((key, value));
    }
    CallMetadata::from_entries(entries).map_err(|error| invalid(error.to_string()))
}

pub(crate) fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

pub(crate) fn topic_open_headers(headers: &HeaderMap) -> Result<(bool, u64), &'static str> {
    let requires_admission = match single_header(headers, TOPIC_ADMISSION_REQUIRED_HEADER) {
        Some("0") => false,
        Some("1") => true,
        _ => return Err("invalid_topic_admission_required"),
    };
    let head_seq = single_header(headers, TOPIC_HEAD_SEQ_HEADER)
        .filter(|seq| !seq.is_empty() && seq.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|seq| seq.parse().ok())
        .ok_or("invalid_topic_head_seq")?;
    Ok((requires_admission, head_seq))
}

pub(crate) async fn read_control_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, HttpError> {
    let size_error = || HttpError::Transport("HTTP control response exceeds its size limit".into());
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(size_error());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(HttpError::Request)? {
        if chunk.len() > limit - body.len() {
            return Err(size_error());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn read_control_error(response: reqwest::Response) -> HttpError {
    let status = response.status().as_u16();
    match read_control_body(response, MAX_CONTROL_ERROR_BYTES).await {
        Ok(body) => HttpError::ResponseError(status, body),
        Err(error) => error,
    }
}

/// Decode a canonical control refusal that proves no resource was dispatched.
pub fn decode_control_error(status: u16, body: &[u8]) -> Option<C2Error> {
    let mut error = matched_control_error(status, body)?;
    if !error.details.contains_key("dispatch_phase") && !matches!(status, 403 | 404) {
        return None;
    }
    error
        .details
        .insert("dispatch_phase".into(), "pre_dispatch".into());
    Some(error)
}

/// Match status and non-conflicting phase fields. Probe replies omit dispatch phase.
pub(crate) fn matched_control_error(status: u16, body: &[u8]) -> Option<C2Error> {
    use ErrorCode::*;

    let error = C2Error::from_json_bytes(body).ok()?;
    if !matches!(
        (status, error.code),
        (400, ProtocolViolation)
            | (403, AdmissionDenied)
            | (404, ResourceNotFound)
            | (406, UnsupportedCallMode)
            | (
                409,
                ContractMismatch
                    | IdentityMismatch
                    | RouteConflict
                    | ResourceUnavailable
                    | CallIdConflict
                    | CallOutcomeUnknown
                    | FallbackDenied
            )
            | (410, ResourceRemoved)
            | (413 | 429, CallCapacityExceeded)
            | (502, ResourceUnavailable | ProtocolViolation | CallCancelled)
            | (
                503,
                ResourceUnavailable
                    | ResourceClosed
                    | RouteWatchUnavailable
                    | CallCapacityExceeded
                    | CallCancelled
                    | CallDeadlineExceeded
            )
    ) {
        return None;
    }
    ["dispatch_phase", "transport_phase"]
        .iter()
        .all(|key| {
            error
                .details
                .get(*key)
                .is_none_or(|phase| phase == "pre_dispatch")
        })
        .then_some(error)
}
