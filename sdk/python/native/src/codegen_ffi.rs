use c2_codegen::{
    CodegenError as CoreCodegenError, ContractCodegenOptions, ContractCodegenTarget,
    compile_contract_artifacts,
};
use c2_contract::{ContractError, ContractRelease};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

pyo3::create_exception!(c_two._native, CodegenError, pyo3::exceptions::PyException);

#[pyfunction]
#[pyo3(signature = (descriptor_json, *, target))]
fn compile_contract_artifacts_projection(
    py: Python<'_>,
    descriptor_json: &[u8],
    target: &str,
) -> PyResult<Py<PyAny>> {
    let target = parse_target(py, target)?;
    let release = ContractRelease::from_descriptor_json(descriptor_json);
    let release = release.map_err(|error| codegen_error(py, CoreCodegenError::Contract(error)))?;
    let artifacts = py
        .detach(move || {
            compile_contract_artifacts(&release, target, &ContractCodegenOptions::default())
        })
        .map_err(|error| codegen_error(py, error))?;

    let projected = PyList::empty(py);
    for artifact in artifacts.artifacts() {
        let item = PyDict::new(py);
        item.set_item("relative_path", artifact.relative_path())?;
        item.set_item("kind", artifact.kind().as_str())?;
        item.set_item("bytes", PyBytes::new(py, artifact.bytes()))?;
        item.set_item("sha256", artifact.sha256_hex())?;
        item.set_item("owner", artifact.provenance().owner())?;
        item.set_item("source", artifact.provenance().source())?;
        projected.append(item)?;
    }

    let result = PyDict::new(py);
    result.set_item("artifacts", projected)?;
    result.set_item("total_bytes", artifacts.total_bytes())?;
    Ok(result.into_any().unbind())
}

fn parse_target(py: Python<'_>, target: &str) -> PyResult<ContractCodegenTarget> {
    match target {
        "rust" => Ok(ContractCodegenTarget::Rust),
        "python" => Ok(ContractCodegenTarget::Python),
        "typescript" => Ok(ContractCodegenTarget::TypeScript),
        _ => Err(public_error(
            py,
            format!(
                "unsupported contract codegen target {target:?}; expected rust, python, or typescript"
            ),
            ErrorFields::None,
        )),
    }
}

fn codegen_error(py: Python<'_>, error: CoreCodegenError) -> PyErr {
    let message = error.to_string();
    match error {
        CoreCodegenError::Contract(ContractError::LimitExceeded {
            metric,
            limit,
            observed,
            path,
        }) => public_error(
            py,
            message,
            ErrorFields::Admission {
                metric: metric.as_str(),
                limit,
                observed,
                path,
            },
        ),
        CoreCodegenError::FastDb {
            binding_path,
            code,
            symbol,
            path,
            message: fastdb_message,
            details_json,
        } => public_error(
            py,
            message,
            ErrorFields::FastDb(FastDbErrorFields {
                binding_path,
                code,
                symbol,
                path,
                message: fastdb_message,
                details_json,
            }),
        ),
        _ => public_error(py, message, ErrorFields::None),
    }
}

struct FastDbErrorFields {
    binding_path: String,
    code: u32,
    symbol: String,
    path: String,
    message: String,
    details_json: String,
}

enum ErrorFields {
    None,
    FastDb(FastDbErrorFields),
    Admission {
        metric: &'static str,
        limit: u64,
        observed: u64,
        path: String,
    },
}

fn public_error(py: Python<'_>, display_message: String, fields: ErrorFields) -> PyErr {
    let error = PyErr::new::<CodegenError, _>(display_message.clone());
    let value = error.value(py);
    let set_common_empty = || {
        value
            .setattr("binding_path", py.None())
            .and_then(|()| value.setattr("code", py.None()))
            .and_then(|()| value.setattr("symbol", py.None()))
            .and_then(|()| value.setattr("details_json", py.None()))
    };
    let set_admission_empty = || {
        value
            .setattr("metric", py.None())
            .and_then(|()| value.setattr("limit", py.None()))
            .and_then(|()| value.setattr("observed", py.None()))
    };
    let set_result = match fields {
        ErrorFields::FastDb(fields) => value
            .setattr("binding_path", fields.binding_path)
            .and_then(|()| value.setattr("code", fields.code))
            .and_then(|()| value.setattr("symbol", fields.symbol))
            .and_then(|()| value.setattr("path", fields.path))
            .and_then(|()| value.setattr("message", fields.message))
            .and_then(|()| value.setattr("details_json", fields.details_json))
            .and_then(|()| set_admission_empty()),
        ErrorFields::Admission {
            metric,
            limit,
            observed,
            path,
        } => set_common_empty()
            .and_then(|()| value.setattr("metric", metric))
            .and_then(|()| value.setattr("limit", limit))
            .and_then(|()| value.setattr("observed", observed))
            .and_then(|()| value.setattr("path", path))
            .and_then(|()| value.setattr("message", display_message)),
        ErrorFields::None => set_common_empty()
            .and_then(|()| set_admission_empty())
            .and_then(|()| value.setattr("path", py.None()))
            .and_then(|()| value.setattr("message", display_message))
            .and_then(|()| value.setattr("details_json", py.None())),
    };
    match set_result {
        Ok(()) => error,
        Err(attribute_error) => attribute_error,
    }
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("CodegenError", m.py().get_type::<CodegenError>())?;
    m.add_function(wrap_pyfunction!(compile_contract_artifacts_projection, m)?)?;
    Ok(())
}
