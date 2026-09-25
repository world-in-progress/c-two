use std::collections::BTreeMap;

use c2_core::{
    AdapterFailure, AdapterFailurePhase, ExternalCause, external_cause_details,
    normalize_adapter_failure,
};
use c2_error::C2Error;
use fastdb::PayloadError;

pub use c2_core::Error;

/// Project one official FastDB error through Core's frozen external-cause
/// convention.
///
/// The SDK does not parse FastDB messages or maintain a second error registry.
/// Every value comes from the public fields of [`PayloadError`].
pub fn fastdb_cause_details(error: &PayloadError) -> BTreeMap<String, String> {
    let code = error.code().to_string();
    external_cause_details(ExternalCause {
        owner: "fastdb",
        code: &code,
        symbol: error.symbol(),
        path: error.path(),
        message: error.message(),
        details_json: error.details_json(),
    })
}

/// Convert an official FastDB error at one generated-adapter phase into the
/// canonical C-Two semantic error registry.
pub fn fastdb_adapter_error(phase: AdapterFailurePhase, error: PayloadError) -> C2Error {
    normalize_adapter_failure(
        phase,
        AdapterFailure::Local {
            message: error.message().to_string(),
            details: fastdb_cause_details(&error),
        },
    )
}

/// Preserve a semantic C-Two error exactly, or classify a local SDK error at
/// the generated-adapter phase where it occurred.
pub fn adapter_error(phase: AdapterFailurePhase, error: Error) -> C2Error {
    match error {
        Error::Semantic(error) => normalize_adapter_failure(phase, AdapterFailure::Semantic(error)),
        local => normalize_adapter_failure(
            phase,
            AdapterFailure::Local {
                message: local.to_string(),
                details: BTreeMap::new(),
            },
        ),
    }
}
