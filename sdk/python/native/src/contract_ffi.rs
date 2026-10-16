//! PyO3 projection of the canonical contract authority.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[pyfunction]
fn contract_descriptor_sha256_hex(payload: &[u8]) -> PyResult<String> {
    c2_contract::contract_descriptor_sha256_hex(payload)
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn validate_portable_contract_descriptor(payload: &[u8]) -> PyResult<()> {
    c2_contract::validate_portable_contract_descriptor_json(payload)
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn derive_portable_contract_fingerprints(payload: &[u8]) -> PyResult<(String, String)> {
    c2_contract::derive_contract_fingerprints_json(payload)
        .map(|fingerprints| {
            (
                fingerprints.abi_hash().to_string(),
                fingerprints.signature_hash().to_string(),
            )
        })
        .map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn canonicalize_portable_contract_descriptor(payload: &[u8]) -> PyResult<String> {
    c2_contract::ContractRelease::from_descriptor_json(payload)
        .map(|release| release.canonical_descriptor_json().to_string())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
fn contract_release_ref_json(payload: &[u8]) -> PyResult<String> {
    c2_contract::ContractRelease::from_descriptor_json(payload)
        .and_then(|release| release.reference().to_canonical_json())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(contract_descriptor_sha256_hex, m)?)?;
    m.add_function(wrap_pyfunction!(validate_portable_contract_descriptor, m)?)?;
    m.add_function(wrap_pyfunction!(derive_portable_contract_fingerprints, m)?)?;
    m.add_function(wrap_pyfunction!(
        canonicalize_portable_contract_descriptor,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(contract_release_ref_json, m)?)?;
    Ok(())
}
