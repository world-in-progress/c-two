use crate::{
    ArtifactKind, ArtifactProvenance, CodegenError, ContractArtifact, ContractCodegenTarget,
};
use c2_contract::{MethodAccess, ValidatedContractDescriptor};
use serde_json::Value;
use std::collections::BTreeSet;

const TYPESCRIPT_TRANSPORT_SOURCE: &str = include_str!("../assets/typescript_transport.ts");

pub(crate) struct TargetMethod<'a> {
    pub(crate) index: usize,
    pub(crate) name: &'a str,
    pub(crate) access: MethodAccess,
    pub(crate) input_sha256: Option<&'a str>,
    pub(crate) output_sha256: Option<&'a str>,
}

pub(crate) fn render_target_artifacts(
    target: ContractCodegenTarget,
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
) -> Result<Vec<ContractArtifact>, CodegenError> {
    let (path, source) = match target {
        ContractCodegenTarget::Rust => ("rust/c_two_contract.rs", render_rust(descriptor, methods)),
        ContractCodegenTarget::Python => (
            "python/c_two_contract.py",
            render_python(descriptor, methods),
        ),
        ContractCodegenTarget::TypeScript => (
            "typescript/c_two_contract.ts",
            render_typescript(descriptor, methods),
        ),
    };
    Ok(vec![ContractArtifact::new(
        path,
        ArtifactKind::Source,
        source.into_bytes(),
        ArtifactProvenance::new("c-two", format!("{}-contract-module", target.as_str()))?,
    )?])
}

fn render_rust(descriptor: &ValidatedContractDescriptor, methods: &[TargetMethod<'_>]) -> String {
    let mut output = String::from(
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\
         #![allow(non_snake_case)]\n\n",
    );
    for digest in payload_digests(methods) {
        output.push_str(&format!(
            "#[path = \"payloads/{digest}/fastdb_payload_{digest}.rs\"]\n\
             pub mod payload_{digest};\n\n"
        ));
    }
    render_rust_contract_facts(&mut output, descriptor, methods);
    output.push_str(
        r#"
pub fn contract_release() -> Result<c_two::ContractRelease, c_two::Error> {
    Ok(c_two::ContractRelease::from_descriptor_json(
        CONTRACT_DESCRIPTOR_JSON.as_bytes(),
    )?)
}

pub fn contract_release_ref() -> Result<c_two::ContractReleaseRef, c_two::Error> {
    Ok(contract_release()?.reference())
}

pub fn expected_route(
    route_name: impl Into<String>,
) -> Result<c_two::ExpectedRouteContract, c_two::Error> {
    Ok(contract_release()?.expected_route(route_name)?)
}

pub struct ContractClient {
    client: c_two::Client,
}

impl ContractClient {
    pub fn new(client: c_two::Client) -> Result<Self, c_two::Error> {
        let expected = expected_route(client.expected_route().route_name.clone())?;
        if client.expected_route() != &expected {
            return Err(c_two::ContractError::InvalidDescriptor {
                path: "$.client.expected_route".to_string(),
                message: format!(
                    "client route facts do not match generated release {}: expected {:?}, got {:?}",
                    DESCRIPTOR_SHA256,
                    expected,
                    client.expected_route(),
                ),
            }
            .into());
        }
        Ok(Self { client })
    }

    pub fn expected_route(&self) -> &c_two::ExpectedRouteContract {
        self.client.expected_route()
    }
"#,
    );
    for method in methods {
        render_rust_client_method(&mut output, method);
    }
    output.push_str("}\n\npub trait Service: Send + Sync + 'static {\n");
    for method in methods {
        render_rust_service_trait_method(&mut output, method);
    }
    output.push_str(
        r#"}

struct ServiceAdapter<S> {
    service: S,
}

impl<S: Service> ServiceAdapter<S> {
    fn invoke_checked(
        &self,
        method_index: u16,
        request: &[u8],
    ) -> Result<Vec<u8>, c_two::generated::C2Error> {
        match method_index {
"#,
    );
    for method in methods {
        render_rust_service_dispatch_arm(&mut output, method);
    }
    output.push_str(
        r#"            other => Err(c_two::generated::C2Error::new(
                c_two::generated::ErrorCode::ProtocolViolation,
                format!("unknown generated method index {other}"),
            )),
        }
    }
}

impl<S: Service> c_two::generated::EncodedService for ServiceAdapter<S> {
    fn invoke(
        &self,
        method_index: u16,
        request: &[u8],
    ) -> Result<Vec<u8>, c_two::generated::C2Error> {
        let method_name = METHODS
            .get(usize::from(method_index))
            .map(|method| method.name)
            .unwrap_or("<unknown>");
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.invoke_checked(method_index, request)
        })) {
            Ok(result) => result,
            Err(_) => Err(c_two::generated::normalize_adapter_failure(
                c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
                c_two::generated::AdapterFailure::Local {
                    message: format!("service method {method_name:?} panicked"),
                    details: std::collections::BTreeMap::from([
                        ("method".to_string(), method_name.to_string()),
                        ("method_index".to_string(), method_index.to_string()),
                    ]),
                },
            )),
        }
    }
}

pub fn service_definition<S: Service>(
    route_name: impl Into<String>,
    service: S,
) -> Result<c_two::ServiceDefinition, c_two::Error> {
    let release = contract_release()?;
    let release_ref = contract_release_ref()?;
    let methods = METHODS
        .iter()
        .map(|method| -> Result<c_two::generated::MethodDefinition, c_two::Error> {
            let index = u16::try_from(method.index).map_err(|_| {
                c_two::ContractError::InvalidDescriptor {
                    path: "$.methods".to_string(),
                    message: format!(
                        "generated method index {} exceeds the C-Two wire capacity",
                        method.index,
                    ),
                }
            })?;
            Ok(c_two::generated::MethodDefinition {
                index,
                name: method.name.to_string(),
                access: method.access,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    c_two::ServiceDefinition::new(
        &release,
        release_ref,
        route_name,
        methods,
        std::sync::Arc::new(ServiceAdapter { service }),
    )
}

"#,
    );
    for method in methods {
        render_rust_payload_adapters(&mut output, method);
    }
    output.push_str(
        r#"fn require_empty_payload(
    phase: c_two::generated::AdapterFailurePhase,
    method: &'static str,
    direction: &'static str,
    bytes: &[u8],
) -> Result<(), c_two::generated::C2Error> {
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(c_two::generated::normalize_adapter_failure(
            phase,
            c_two::generated::AdapterFailure::Local {
                message: format!(
                    "method {method:?} has no {direction} payload but received {} byte(s)",
                    bytes.len(),
                ),
                details: std::collections::BTreeMap::from([
                    ("actual_bytes".to_string(), bytes.len().to_string()),
                    ("direction".to_string(), direction.to_string()),
                    ("method".to_string(), method.to_string()),
                ]),
            },
        ))
    }
}
"#,
    );
    output
}

fn render_rust_contract_facts(
    output: &mut String,
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
) {
    output.push_str(&format!(
        "pub const CONTRACT_DESCRIPTOR_JSON: &str = include_str!(\"../metadata/contract.json\");\n\
         pub const CONTRACT_SCHEMA: &str = {};\n\
         pub const CRM_NAMESPACE: &str = {};\n\
         pub const CRM_NAME: &str = {};\n\
         pub const CRM_VERSION: &str = {};\n\
         pub const DESCRIPTOR_SHA256: &str = {};\n\
         pub const ABI_HASH: &str = {};\n\
         pub const SIGNATURE_HASH: &str = {};\n\n",
        quoted(descriptor.contract_schema()),
        quoted(descriptor.crm_namespace()),
        quoted(descriptor.crm_name()),
        quoted(descriptor.crm_version()),
        quoted(descriptor.descriptor_sha256().as_str()),
        quoted(descriptor.abi_hash()),
        quoted(descriptor.signature_hash()),
    ));
    output.push_str(
        "#[derive(Clone, Copy, Debug, Eq, PartialEq)]\n\
         pub struct MethodBinding {\n\
         \x20   pub index: usize,\n\
         \x20   pub name: &'static str,\n\
         \x20   pub access: c_two::generated::MethodAccess,\n\
         \x20   pub input_sha256: Option<&'static str>,\n\
         \x20   pub output_sha256: Option<&'static str>,\n\
         }\n\n\
         pub const METHODS: &[MethodBinding] = &[\n",
    );
    for method in methods {
        output.push_str(&format!(
            "    MethodBinding {{ index: {}, name: {}, access: {}, input_sha256: {}, output_sha256: {} }},\n",
            method.index,
            quoted(method.name),
            rust_method_access(method.access),
            rust_option(method.input_sha256),
            rust_option(method.output_sha256),
        ));
    }
    output.push_str("];\n");
}

fn render_rust_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        ", payload: &fastdb::Payload"
    } else {
        ""
    };
    let return_type = if method.output_sha256.is_some() {
        "fastdb::Payload"
    } else {
        "()"
    };
    let request = if method.input_sha256.is_some() {
        format!("encode_{symbol}_input(payload)?")
    } else {
        format!("encode_{symbol}_input()")
    };
    output.push_str(&format!(
        "\n    pub fn {symbol}(&self{parameter}) -> Result<{return_type}, c_two::Error> {{\n\
         \x20       let request = {request};\n\
         \x20       let response = c_two::generated::EncodedClient::call_owned(\n\
         \x20           &self.client,\n\
         \x20           {},\n\
         \x20           &request,\n\
         \x20       )?;\n\
         \x20       decode_{symbol}_output(&response)\n\
         \x20   }}\n",
        quoted(method.name),
    ));
    if method.output_sha256.is_some() {
        output.push_str(&format!(
            "\n    pub fn hold_{symbol}(\n\
             \x20       &self{parameter},\n\
             \x20   ) -> Result<c_two::Held<fastdb::Payload>, c_two::Error> {{\n\
             \x20       let request = {request};\n\
             \x20       let response = c_two::generated::EncodedClient::call_held(\n\
             \x20           &self.client,\n\
             \x20           {},\n\
             \x20           &request,\n\
             \x20       )?;\n\
             \x20       decode_{symbol}_output_held(response)\n\
             \x20   }}\n",
            quoted(method.name),
        ));
    }
}

fn render_rust_service_trait_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        ", input: &fastdb::Payload"
    } else {
        ""
    };
    let return_type = if method.output_sha256.is_some() {
        "fastdb::Payload"
    } else {
        "()"
    };
    output.push_str(&format!(
        "    fn {symbol}(&self{parameter}) -> Result<{return_type}, c_two::Error>;\n"
    ));
}

fn render_rust_service_dispatch_arm(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    output.push_str(&format!("            {} => {{\n", method.index));
    if method.input_sha256.is_some() {
        output.push_str(&format!(
            "                let input = decode_{symbol}_input_borrowed(request)?;\n\
             \x20               let output = self\n\
             \x20                   .service\n\
             \x20                   .{symbol}(input.payload())\n\
             \x20                   .map_err(|error| {{\n\
             \x20                       c_two::generated::adapter_error(\n\
             \x20                           c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,\n\
             \x20                           error,\n\
             \x20                       )\n\
             \x20                   }})?;\n"
        ));
    } else {
        output.push_str(&format!(
            "                decode_{symbol}_input(request)?;\n\
             \x20               let output = self\n\
             \x20                   .service\n\
             \x20                   .{symbol}()\n\
             \x20                   .map_err(|error| {{\n\
             \x20                       c_two::generated::adapter_error(\n\
             \x20                           c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,\n\
             \x20                           error,\n\
             \x20                       )\n\
             \x20                   }})?;\n"
        ));
    }
    if method.output_sha256.is_some() {
        output.push_str(&format!(
            "                encode_{symbol}_output(&output)\n"
        ));
    } else {
        output.push_str(&format!(
            "                let () = output;\n\
             \x20               Ok(encode_{symbol}_output())\n"
        ));
    }
    output.push_str("            }\n");
}

fn render_rust_payload_adapters(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    if let Some(digest) = method.input_sha256 {
        output.push_str(&format!(
            "pub fn encode_{symbol}_input(\n\
             \x20   payload: &fastdb::Payload,\n\
             ) -> Result<Vec<u8>, c_two::Error> {{\n\
             \x20   payload\n\
             \x20       .require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
             \x20       .map_err(|error| c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientInputSerializing,\n\
             \x20           error,\n\
             \x20       )))?;\n\
             \x20   payload.binary_bytes().map_err(|error| {{\n\
             \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientInputSerializing,\n\
             \x20           error,\n\
             \x20       ))\n\
             \x20   }})\n\
             }}\n\n\
             fn decode_{symbol}_input_borrowed(\n\
             \x20   bytes: &[u8],\n\
             ) -> Result<c_two::generated::BorrowedPayload, c_two::generated::C2Error> {{\n\
             \x20   let spec = payload_{digest}::compile_spec().map_err(|error| {{\n\
             \x20       c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ResourceInputFromBuffer,\n\
             \x20           error,\n\
             \x20       )\n\
             \x20   }})?;\n\
             \x20   let input = c_two::generated::open_borrowed(&spec, bytes).map_err(|error| {{\n\
             \x20       c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ResourceInputFromBuffer,\n\
             \x20           error,\n\
             \x20       )\n\
             \x20   }})?;\n\
             \x20   input\n\
             \x20       .payload()\n\
             \x20       .require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
             \x20       .map_err(|error| {{\n\
             \x20           c_two::generated::fastdb_adapter_error(\n\
             \x20               c_two::generated::AdapterFailurePhase::ResourceInputDeserializing,\n\
             \x20               error,\n\
             \x20           )\n\
             \x20       }})?;\n\
             \x20   Ok(input)\n\
             }}\n\n"
        ));
    } else {
        output.push_str(&format!(
            "pub fn encode_{symbol}_input() -> Vec<u8> {{\n\
             \x20   Vec::new()\n\
             }}\n\n\
             fn decode_{symbol}_input(\n\
             \x20   bytes: &[u8],\n\
             ) -> Result<(), c_two::generated::C2Error> {{\n\
             \x20   require_empty_payload(\n\
             \x20       c_two::generated::AdapterFailurePhase::ResourceInputDeserializing,\n\
             \x20       {},\n\
             \x20       \"input\",\n\
             \x20       bytes,\n\
             \x20   )\n\
             }}\n\n",
            quoted(method.name),
        ));
    }

    if let Some(digest) = method.output_sha256 {
        output.push_str(&format!(
            "pub fn decode_{symbol}_output(\n\
             \x20   bytes: &[u8],\n\
             ) -> Result<fastdb::Payload, c_two::Error> {{\n\
             \x20   let spec = payload_{digest}::compile_spec().map_err(|error| {{\n\
             \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientOutputFromBuffer,\n\
             \x20           error,\n\
             \x20       ))\n\
             \x20   }})?;\n\
             \x20   let payload = c_two::generated::open_owned(&spec, bytes).map_err(|error| {{\n\
             \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientOutputFromBuffer,\n\
             \x20           error,\n\
             \x20       ))\n\
             \x20   }})?;\n\
             \x20   payload\n\
             \x20       .require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
             \x20       .map_err(|error| {{\n\
             \x20           c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20               c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,\n\
             \x20               error,\n\
             \x20           ))\n\
             \x20       }})?;\n\
             \x20   Ok(payload)\n\
             }}\n\n\
             pub fn decode_{symbol}_output_held(\n\
             \x20   response: c_two::generated::HeldResponse,\n\
             ) -> Result<c_two::Held<fastdb::Payload>, c_two::Error> {{\n\
             \x20   let spec = payload_{digest}::compile_spec().map_err(|error| {{\n\
             \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientOutputFromBuffer,\n\
             \x20           error,\n\
             \x20       ))\n\
             \x20   }})?;\n\
             \x20   let mut held = c_two::generated::open_held(&spec, response).map_err(|error| {{\n\
             \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientOutputFromBuffer,\n\
             \x20           error,\n\
             \x20       ))\n\
             \x20   }})?;\n\
             \x20   let Some(payload) = held.value() else {{\n\
             \x20       return Err(c_two::ContractError::InvalidDescriptor {{\n\
             \x20           path: \"$.held\".to_string(),\n\
             \x20           message: \"new held payload was already released\".to_string(),\n\
             \x20       }}.into());\n\
             \x20   }};\n\
             \x20   if let Err(error) =\n\
             \x20       payload.require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
             \x20   {{\n\
             \x20       let semantic = c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,\n\
             \x20           error,\n\
             \x20       );\n\
             \x20       let _ = held.release();\n\
             \x20       return Err(c_two::Error::Semantic(semantic));\n\
             \x20   }}\n\
             \x20   Ok(held)\n\
             }}\n\n\
             fn encode_{symbol}_output(\n\
             \x20   payload: &fastdb::Payload,\n\
             ) -> Result<Vec<u8>, c_two::generated::C2Error> {{\n\
             \x20   payload\n\
             \x20       .require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
             \x20       .map_err(|error| {{\n\
             \x20           c_two::generated::fastdb_adapter_error(\n\
             \x20               c_two::generated::AdapterFailurePhase::ResourceOutputSerializing,\n\
             \x20               error,\n\
             \x20           )\n\
             \x20       }})?;\n\
             \x20   payload.binary_bytes().map_err(|error| {{\n\
             \x20       c_two::generated::fastdb_adapter_error(\n\
             \x20           c_two::generated::AdapterFailurePhase::ResourceOutputSerializing,\n\
             \x20           error,\n\
             \x20       )\n\
             \x20   }})\n\
             }}\n\n"
        ));
    } else {
        output.push_str(&format!(
            "pub fn decode_{symbol}_output(bytes: &[u8]) -> Result<(), c_two::Error> {{\n\
             \x20   require_empty_payload(\n\
             \x20       c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,\n\
             \x20       {},\n\
             \x20       \"output\",\n\
             \x20       bytes,\n\
             \x20   )\n\
             \x20   .map_err(c_two::Error::Semantic)\n\
             }}\n\n\
             fn encode_{symbol}_output() -> Vec<u8> {{\n\
             \x20   Vec::new()\n\
             }}\n\n",
            quoted(method.name),
        ));
    }
}

fn render_python(descriptor: &ValidatedContractDescriptor, methods: &[TargetMethod<'_>]) -> String {
    let mut output = String::from(
        "# generated-by: c-two.contract.codegen.v2\n\
         # payload-semantics-owner: fastdb-core\n\
         from __future__ import annotations\n\n\
         import importlib.util\n\
         import sys\n\
         from dataclasses import dataclass\n\
         from pathlib import Path\n\
         from types import ModuleType\n\
         from typing import Any, Protocol\n\n\
         from fastdb4py.payload import Payload\n\n",
    );
    output.push_str(&format!(
        "CONTRACT_SCHEMA = {}\n\
         CRM_NAMESPACE = {}\n\
         CRM_NAME = {}\n\
         CRM_VERSION = {}\n\
         DESCRIPTOR_SHA256 = {}\n\
         ABI_HASH = {}\n\
         SIGNATURE_HASH = {}\n\n",
        quoted(descriptor.contract_schema()),
        quoted(descriptor.crm_namespace()),
        quoted(descriptor.crm_name()),
        quoted(descriptor.crm_version()),
        quoted(descriptor.descriptor_sha256().as_str()),
        quoted(descriptor.abi_hash()),
        quoted(descriptor.signature_hash()),
    ));
    output.push_str(
        "@dataclass(frozen=True)\n\
         class MethodBinding:\n\
         \x20   index: int\n\
         \x20   name: str\n\
         \x20   access: str\n\
         \x20   input_sha256: str | None\n\
         \x20   output_sha256: str | None\n\n\
         METHODS = (\n",
    );
    for method in methods {
        output.push_str(&format!(
            "    MethodBinding({}, {}, {}, {}, {}),\n",
            method.index,
            quoted(method.name),
            quoted(access_name(method.access)),
            python_option(method.input_sha256),
            python_option(method.output_sha256),
        ));
    }
    output.push_str(
        ")\n\n\
         def expected_route(route_name: str) -> dict[str, str]:\n\
         \x20   return {\n\
         \x20       \"route_name\": route_name,\n\
         \x20       \"crm_ns\": CRM_NAMESPACE,\n\
         \x20       \"crm_name\": CRM_NAME,\n\
         \x20       \"crm_ver\": CRM_VERSION,\n\
         \x20       \"abi_hash\": ABI_HASH,\n\
         \x20       \"signature_hash\": SIGNATURE_HASH,\n\
         \x20   }\n\n\
         def _load_payload_module(digest: str) -> ModuleType:\n\
         \x20   path = Path(__file__).parent / \"payloads\" / digest / f\"fastdb_payload_{digest}.py\"\n\
         \x20   module_name = f\"_c_two_fastdb_payload_{digest}\"\n\
         \x20   spec = importlib.util.spec_from_file_location(module_name, path)\n\
         \x20   if spec is None or spec.loader is None:\n\
         \x20       raise ImportError(f\"cannot load generated FastDB payload module at {path}\")\n\
         \x20   module = importlib.util.module_from_spec(spec)\n\
         \x20   sys.modules[module_name] = module\n\
         \x20   try:\n\
         \x20       spec.loader.exec_module(module)\n\
         \x20   except BaseException:\n\
         \x20       sys.modules.pop(module_name, None)\n\
         \x20       raise\n\
         \x20   return module\n\n",
    );
    for digest in payload_digests(methods) {
        output.push_str(&format!(
            "_PAYLOAD_{digest} = _load_payload_module({})\n",
            quoted(digest),
        ));
    }
    if methods
        .iter()
        .any(|method| method.input_sha256.is_some() || method.output_sha256.is_some())
    {
        output.push('\n');
    }
    output.push_str(
        "class BoundClient(Protocol):\n\
         \x20   def call(self, method_name: str, data: bytes) -> Any: ...\n\n\
         def _copy_and_release_response(response: Any) -> bytes:\n\
         \x20   try:\n\
         \x20       data = bytes(response)\n\
         \x20   except BaseException:\n\
         \x20       release = getattr(response, \"release\", None)\n\
         \x20       if release is not None:\n\
         \x20           try:\n\
         \x20               release()\n\
         \x20           except BaseException:\n\
         \x20               pass\n\
         \x20       raise\n\
         \x20   release = getattr(response, \"release\", None)\n\
         \x20   if release is not None:\n\
         \x20       release()\n\
         \x20   return data\n\n\
         class ContractClient:\n\
         \x20   def __init__(self, bound_client: BoundClient) -> None:\n\
         \x20       self._bound_client = bound_client\n",
    );
    for method in methods {
        render_python_client_method(&mut output, method);
    }
    output.push('\n');
    for method in methods {
        render_python_direction(&mut output, method, "input", method.input_sha256);
        render_python_direction(&mut output, method, "output", method.output_sha256);
    }
    output.push_str(
        "def _require_empty_payload(method: str, direction: str, data: bytes) -> None:\n\
         \x20   if data:\n\
         \x20       raise ValueError(\n\
         \x20           f\"method {method!r} has no {direction} payload but received {len(data)} byte(s)\"\n\
         \x20       )\n",
    );
    output
}

fn render_python_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        ", payload: Payload"
    } else {
        ""
    };
    let return_type = if method.output_sha256.is_some() {
        "Payload"
    } else {
        "None"
    };
    let request = if method.input_sha256.is_some() {
        format!("encode_{symbol}_input(payload)")
    } else {
        format!("encode_{symbol}_input()")
    };
    output.push_str(&format!(
        "\n    def {symbol}(self{parameter}) -> {return_type}:\n\
         \x20       request = {request}\n\
         \x20       response = self._bound_client.call({}, request)\n\
         \x20       return decode_{symbol}_output(_copy_and_release_response(response))\n",
        quoted(method.name),
    ));
}

fn render_python_direction(
    output: &mut String,
    method: &TargetMethod<'_>,
    direction: &'static str,
    digest: Option<&str>,
) {
    let symbol = method_symbol(method.index, method.name);
    if let Some(digest) = digest {
        output.push_str(&format!(
            "def encode_{symbol}_{direction}(payload: Payload) -> bytes:\n\
             \x20   payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20   return payload.binary_bytes()\n\n\
             def decode_{symbol}_{direction}(data: bytes) -> Payload:\n\
             \x20   payload = Payload.open_copy(_PAYLOAD_{digest}.compile_spec(), data)\n\
             \x20   try:\n\
             \x20       payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20   except BaseException:\n\
             \x20       try:\n\
             \x20           payload.close()\n\
             \x20       except BaseException:\n\
             \x20           pass\n\
             \x20       raise\n\
             \x20   return payload\n\n"
        ));
    } else {
        output.push_str(&format!(
            "def encode_{symbol}_{direction}() -> bytes:\n\
             \x20   return b\"\"\n\n\
             def decode_{symbol}_{direction}(data: bytes) -> None:\n\
             \x20   _require_empty_payload({}, {direction:?}, data)\n\n",
            quoted(method.name),
        ));
    }
}

fn render_typescript(
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
) -> String {
    let mut output = String::from(
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\
         import { Payload, PayloadError } from 'fastdb4ts/payload';\n",
    );
    for digest in payload_digests(methods) {
        output.push_str(&format!(
            "import * as payload_{digest} from './payloads/{digest}/fastdb_payload_{digest}.js';\n"
        ));
    }
    output.push('\n');
    output.push_str(TYPESCRIPT_TRANSPORT_SOURCE);
    output.push_str(
        r#"

export interface C2FastDbCauseFields {
  readonly cause_owner: "fastdb";
  readonly fastdb_code: string;
  readonly fastdb_details_json: string;
  readonly fastdb_message: string;
  readonly fastdb_path: string;
  readonly fastdb_symbol: string;
}

export class C2PayloadAdapterError extends Error {
  constructor(
    readonly details: C2FastDbCauseFields,
    readonly cause: PayloadError,
  ) {
    super(`C-Two FastDB payload adapter failed: ${cause.message}`);
    this.name = "C2PayloadAdapterError";
    Object.setPrototypeOf(this, C2PayloadAdapterError.prototype);
  }
}

export function projectFastDbCause(error: PayloadError): C2FastDbCauseFields {
  if (!(error instanceof PayloadError)) {
    throw new TypeError("error must be a fastdb4ts PayloadError");
  }
  return Object.freeze({
    cause_owner: "fastdb",
    fastdb_code: error.code.toString(),
    fastdb_details_json: error.detailsJson,
    fastdb_message: error.message,
    fastdb_path: error.path,
    fastdb_symbol: error.symbol,
  });
}

function normalizeFastDbPayloadError(error: unknown): unknown {
  if (!(error instanceof PayloadError)) {
    return error;
  }
  return new C2PayloadAdapterError(
    projectFastDbCause(error),
    error,
  );
}
"#,
    );
    output.push_str(&format!(
        "\n\nexport const CONTRACT: C2ContractIdentity = {{\n\
         \x20 schema: 'c-two.contract.v2',\n\
         \x20 namespace: {},\n\
         \x20 name: {},\n\
         \x20 version: {},\n\
         \x20 descriptorSha256: {},\n\
         \x20 abiHash: {},\n\
         \x20 signatureHash: {},\n\
         }};\n\n",
        quoted(descriptor.crm_namespace()),
        quoted(descriptor.crm_name()),
        quoted(descriptor.crm_version()),
        quoted(descriptor.descriptor_sha256().as_str()),
        quoted(descriptor.abi_hash()),
        quoted(descriptor.signature_hash()),
    ));
    output.push_str(
        "export interface MethodBinding {\n\
         \x20 readonly index: number;\n\
         \x20 readonly name: string;\n\
         \x20 readonly access: 'read' | 'write';\n\
         \x20 readonly inputSha256?: string;\n\
         \x20 readonly outputSha256?: string;\n\
         }\n\n\
         export const METHODS: readonly MethodBinding[] = Object.freeze([\n",
    );
    for method in methods {
        output.push_str(&format!(
            "  {{ index: {}, name: {}, access: {}{}{} }},\n",
            method.index,
            quoted(method.name),
            quoted(access_name(method.access)),
            typescript_optional_field("inputSha256", method.input_sha256),
            typescript_optional_field("outputSha256", method.output_sha256),
        ));
    }
    output.push_str(
        "]);\n\n\
         export class ContractClient {\n\
         \x20 constructor(\n\
         \x20   private readonly transport: C2EncodedClientTransport,\n\
         \x20   private readonly routeName: string,\n\
         \x20 ) {}\n",
    );
    for method in methods {
        render_typescript_client_method(&mut output, method);
    }
    output.push_str("}\n\n");
    for method in methods {
        render_typescript_direction(&mut output, method, "input", method.input_sha256);
        render_typescript_direction(&mut output, method, "output", method.output_sha256);
    }
    output.push_str(
        "function decodeAndReleaseResponse<T>(\n\
         \x20 response: C2ResponsePayload,\n\
         \x20 decode: (bytes: Uint8Array) => T,\n\
         ): T {\n\
         \x20 let value: T;\n\
         \x20 try {\n\
         \x20   value = decode(contractResponseBytes(response));\n\
         \x20 } catch (error) {\n\
         \x20   tryReleaseResponsePayload(response);\n\
         \x20   throw error;\n\
         \x20 }\n\
         \x20 try {\n\
         \x20   releaseHeldValue(response);\n\
         \x20 } catch (error) {\n\
         \x20   tryReleaseHeldValue(value);\n\
         \x20   throw error;\n\
         \x20 }\n\
         \x20 return value;\n\
         }\n\n\
         function tryReleaseHeldValue(value: unknown): void {\n\
         \x20 try {\n\
         \x20   releaseHeldValue(value);\n\
         \x20 } catch {\n\
         \x20 }\n\
         }\n\n\
         function contractResponseBytes(response: C2ResponsePayload): Uint8Array {\n\
         \x20 if (response instanceof Uint8Array) {\n\
         \x20   return response;\n\
         \x20 }\n\
         \x20 if (isArrayBufferLike(response)) {\n\
         \x20   return new Uint8Array(response);\n\
         \x20 }\n\
         \x20 throw new Error(\n\
         \x20   \"FastDB copy-backed decoding requires a Uint8Array or ArrayBuffer response; opaque response allocators are not supported by this adapter\",\n\
         \x20 );\n\
         }\n\n\
         function requireEmptyPayload(method: string, direction: string, data: Uint8Array): void {\n\
         \x20 if (data.byteLength !== 0) {\n\
         \x20   throw new Error(\n\
         \x20     `method ${JSON.stringify(method)} has no ${direction} payload but received ${data.byteLength} byte(s)`,\n\
         \x20   );\n\
         \x20 }\n\
         }\n",
    );
    output
}

fn render_typescript_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        "payload: Payload"
    } else {
        ""
    };
    let return_type = if method.output_sha256.is_some() {
        "Payload"
    } else {
        "void"
    };
    let request = if method.input_sha256.is_some() {
        format!("encode_{symbol}_input(payload)")
    } else {
        format!("encode_{symbol}_input()")
    };
    output.push_str(&format!(
        "\n  async {symbol}({parameter}): Promise<{return_type}> {{\n\
         \x20   const request = {request};\n\
         \x20   const response = requireResponsePayload(\n\
         \x20     await this.transport.call(this.routeName, CONTRACT, {}, request),\n\
         \x20     \"encoded transport response\",\n\
         \x20   );\n\
         \x20   return decodeAndReleaseResponse(response, decode_{symbol}_output);\n\
         \x20 }}\n",
        quoted(method.name),
    ));
    if method.output_sha256.is_some() {
        output.push_str(&format!(
            "\n  async hold_{symbol}({parameter}): Promise<C2HeldResult<{return_type}>> {{\n\
             \x20   const request = {request};\n\
             \x20   const response = requireResponsePayload(\n\
             \x20     await this.transport.call(this.routeName, CONTRACT, {}, request),\n\
             \x20     \"encoded transport response\",\n\
             \x20   );\n\
             \x20   try {{\n\
             \x20     const value = decode_{symbol}_output(contractResponseBytes(response));\n\
             \x20     return createHeldResult(value, true, retainableResponsePayload(response), true);\n\
             \x20   }} catch (error) {{\n\
             \x20     tryReleaseResponsePayload(response);\n\
             \x20     throw error;\n\
             \x20   }}\n\
             \x20 }}\n",
            quoted(method.name),
        ));
    } else {
        output.push_str(&format!(
            "\n  async hold_{symbol}({parameter}): Promise<C2HeldResult<{return_type}>> {{\n\
             \x20   await this.{symbol}({});\n\
             \x20   return createHeldResult(undefined, false);\n\
             \x20 }}\n",
            if method.input_sha256.is_some() {
                "payload"
            } else {
                ""
            },
        ));
    }
}

fn render_typescript_direction(
    output: &mut String,
    method: &TargetMethod<'_>,
    direction: &'static str,
    digest: Option<&str>,
) {
    let symbol = method_symbol(method.index, method.name);
    if let Some(digest) = digest {
        output.push_str(&format!(
            "export function encode_{symbol}_{direction}(payload: Payload): Uint8Array {{\n\
             \x20 try {{\n\
             \x20   payload.requireSpecSha256(payload_{digest}.payloadSha256());\n\
             \x20   return payload.binaryBytes();\n\
             \x20 }} catch (error) {{\n\
             \x20   throw normalizeFastDbPayloadError(error);\n\
             \x20 }}\n\
             }}\n\n\
             export function decode_{symbol}_{direction}(data: Uint8Array): Payload {{\n\
             \x20 let payload: Payload | undefined;\n\
             \x20 try {{\n\
             \x20   payload = Payload.openCopy(payload_{digest}.compileSpec(), data);\n\
             \x20   payload.requireSpecSha256(payload_{digest}.payloadSha256());\n\
             \x20   return payload;\n\
             \x20 }} catch (error) {{\n\
             \x20   try {{\n\
             \x20     payload?.dispose();\n\
             \x20   }} catch {{\n\
             \x20   }}\n\
             \x20   throw normalizeFastDbPayloadError(error);\n\
             \x20 }}\n\
             }}\n\n"
        ));
    } else {
        output.push_str(&format!(
            "export function encode_{symbol}_{direction}(): Uint8Array {{\n\
             \x20 return new Uint8Array();\n\
             }}\n\n\
             export function decode_{symbol}_{direction}(data: Uint8Array): void {{\n\
             \x20 requireEmptyPayload({}, {direction:?}, data);\n\
             }}\n\n",
            quoted(method.name),
        ));
    }
}

fn payload_digests<'a>(methods: &'a [TargetMethod<'a>]) -> BTreeSet<&'a str> {
    methods
        .iter()
        .flat_map(|method| [method.input_sha256, method.output_sha256])
        .flatten()
        .collect()
}

fn access_name(access: MethodAccess) -> &'static str {
    match access {
        MethodAccess::Read => "read",
        MethodAccess::Write => "write",
    }
}

fn rust_method_access(access: MethodAccess) -> &'static str {
    match access {
        MethodAccess::Read => "c_two::generated::MethodAccess::Read",
        MethodAccess::Write => "c_two::generated::MethodAccess::Write",
    }
}

fn quoted(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

fn rust_option(value: Option<&str>) -> String {
    value
        .map(|value| format!("Some({})", quoted(value)))
        .unwrap_or_else(|| "None".to_string())
}

fn python_option(value: Option<&str>) -> String {
    value.map(quoted).unwrap_or_else(|| "None".to_string())
}

fn typescript_optional_field(name: &str, value: Option<&str>) -> String {
    value
        .map(|value| format!(", {name}: {}", quoted(value)))
        .unwrap_or_default()
}

fn method_symbol(index: usize, name: &str) -> String {
    let mut symbol = format!("method_{index}_");
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            symbol.push(character.to_ascii_lowercase());
        } else {
            symbol.push('_');
        }
    }
    symbol
}
