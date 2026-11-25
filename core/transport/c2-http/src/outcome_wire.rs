//! Bounded JSON projection of an IPC outcome query response.

use c2_wire::rpc::{WireCallOutcome, WireCallOutcomeStatus, WireResultUnavailableReason};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_OUTCOME_JSON_BYTES: usize = 1024;
pub(crate) const CALL_ID_HEADER: &str = "x-c2-call-id";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutcomeJson<'a> {
    status: &'a str,
    method_name: Option<String>,
    result_available: bool,
    result_unavailable_reason: Option<&'a str>,
    error_code: Option<u16>,
    error_details_available: bool,
}

#[cfg(feature = "relay")]
impl From<WireCallOutcome> for OutcomeJson<'static> {
    fn from(outcome: WireCallOutcome) -> Self {
        Self {
            status: outcome.status.as_str(),
            method_name: outcome.method_name,
            result_available: outcome.result_available,
            result_unavailable_reason: outcome
                .result_unavailable_reason
                .map(|reason| reason.as_str()),
            error_code: outcome.error_code.map(u16::from),
            error_details_available: outcome.error_details_available,
        }
    }
}

impl TryFrom<OutcomeJson<'_>> for WireCallOutcome {
    type Error = &'static str;

    fn try_from(json: OutcomeJson<'_>) -> Result<Self, Self::Error> {
        let outcome = Self {
            status: match json.status {
                "in_flight" => WireCallOutcomeStatus::InFlight,
                "succeeded" => WireCallOutcomeStatus::Succeeded,
                "failed" => WireCallOutcomeStatus::Failed,
                "unknown" => WireCallOutcomeStatus::Unknown,
                _ => return Err("invalid outcome status"),
            },
            method_name: json.method_name,
            result_available: json.result_available,
            result_unavailable_reason: match json.result_unavailable_reason {
                None => None,
                Some("not_retained") => Some(WireResultUnavailableReason::NotRetained),
                Some("evicted") => Some(WireResultUnavailableReason::Evicted),
                Some(_) => return Err("invalid unavailable reason"),
            },
            error_code: json
                .error_code
                .map(c2_error::ErrorCode::try_from)
                .transpose()
                .map_err(|_| "invalid outcome error code")?,
            error_details_available: json.error_details_available,
        };
        outcome.validate()?;
        Ok(outcome)
    }
}
