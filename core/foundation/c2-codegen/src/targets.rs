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
#[derive(Debug)]
pub enum ContractCallError {
    Contract(c2_contract::ContractError),
    Transport(c2_ipc::IpcError),
    Payload(fastdb::PayloadError),
    Response(String),
    UnexpectedPayload {
        method: &'static str,
        direction: &'static str,
        actual_bytes: usize,
    },
}

impl std::fmt::Display for ContractCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => write!(formatter, "contract error: {error}"),
            Self::Transport(error) => write!(formatter, "transport error: {error}"),
            Self::Payload(error) => write!(formatter, "FastDB payload error: {error}"),
            Self::Response(error) => write!(formatter, "response materialization error: {error}"),
            Self::UnexpectedPayload {
                method,
                direction,
                actual_bytes,
            } => write!(
                formatter,
                "method {method:?} has no {direction} payload but received {actual_bytes} byte(s)"
            ),
        }
    }
}

impl std::error::Error for ContractCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::Response(_) | Self::UnexpectedPayload { .. } => None,
        }
    }
}

impl From<c2_contract::ContractError> for ContractCallError {
    fn from(error: c2_contract::ContractError) -> Self {
        Self::Contract(error)
    }
}

impl From<c2_ipc::IpcError> for ContractCallError {
    fn from(error: c2_ipc::IpcError) -> Self {
        Self::Transport(error)
    }
}

impl From<fastdb::PayloadError> for ContractCallError {
    fn from(error: fastdb::PayloadError) -> Self {
        Self::Payload(error)
    }
}

pub fn expected_route(
    route_name: impl Into<String>,
) -> Result<c2_contract::ExpectedRouteContract, c2_contract::ContractError> {
    let expected = c2_contract::ExpectedRouteContract {
        route_name: route_name.into(),
        crm_ns: CRM_NAMESPACE.to_string(),
        crm_name: CRM_NAME.to_string(),
        crm_ver: CRM_VERSION.to_string(),
        abi_hash: ABI_HASH.to_string(),
        signature_hash: SIGNATURE_HASH.to_string(),
    };
    c2_contract::validate_expected_route_contract(&expected)?;
    Ok(expected)
}

pub struct ContractClient<'a> {
    client: &'a c2_ipc::SyncClient,
    binding: c2_ipc::RouteBinding,
}

impl<'a> ContractClient<'a> {
    pub fn acquire(
        client: &'a c2_ipc::SyncClient,
        route_name: impl Into<String>,
    ) -> Result<Self, ContractCallError> {
        let expected = expected_route(route_name)?;
        let binding = client.acquire_route(&expected)?;
        Ok(Self { client, binding })
    }

    pub fn binding(&self) -> &c2_ipc::RouteBinding {
        &self.binding
    }

    fn response_bytes(
        &self,
        response: c2_ipc::ResponseData,
    ) -> Result<Vec<u8>, ContractCallError> {
        response
            .into_bytes_with_pool(
                &self.client.server_pool_arc(),
                &self.client.reassembly_pool_arc(),
            )
            .map_err(ContractCallError::Response)
    }
"#,
    );
    for method in methods {
        render_rust_client_method(&mut output, method);
    }
    output.push_str("}\n\n");
    for method in methods {
        render_rust_payload_adapters(&mut output, method);
    }
    output.push_str(
        r#"fn require_empty_payload(
    method: &'static str,
    direction: &'static str,
    bytes: &[u8],
) -> Result<(), ContractCallError> {
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(ContractCallError::UnexpectedPayload {
            method,
            direction,
            actual_bytes: bytes.len(),
        })
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
        "pub const CONTRACT_SCHEMA: &str = {};\n\
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
         \x20   pub access: &'static str,\n\
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
            quoted(access_name(method.access)),
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
        "\n    pub fn {symbol}(&self{parameter}) -> Result<{return_type}, ContractCallError> {{\n\
         \x20       let request = {request};\n\
         \x20       let response = self.client.call_bound(&self.binding, {}, &request)?;\n\
         \x20       let response = self.response_bytes(response)?;\n\
         \x20       Ok(decode_{symbol}_output(&response)?)\n\
         \x20   }}\n",
        quoted(method.name),
    ));
}

fn render_rust_payload_adapters(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    render_rust_direction(output, &symbol, method.name, "input", method.input_sha256);
    render_rust_direction(output, &symbol, method.name, "output", method.output_sha256);
}

fn render_rust_direction(
    output: &mut String,
    symbol: &str,
    method_name: &str,
    direction: &'static str,
    digest: Option<&str>,
) {
    if let Some(digest) = digest {
        output.push_str(&format!(
            "\npub fn encode_{symbol}_{direction}(\n\
             \x20   payload: &fastdb::Payload,\n\
             ) -> Result<Vec<u8>, fastdb::PayloadError> {{\n\
             \x20   payload.require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)?;\n\
             \x20   payload.binary_bytes()\n\
             }}\n\n\
             pub fn decode_{symbol}_{direction}(\n\
             \x20   bytes: &[u8],\n\
             ) -> Result<fastdb::Payload, fastdb::PayloadError> {{\n\
             \x20   let spec = payload_{digest}::compile_spec()?;\n\
             \x20   let payload = fastdb::Payload::open_copy(\n\
             \x20       &spec,\n\
             \x20       bytes,\n\
             \x20       &fastdb::OpenOptions::default(),\n\
             \x20   )?;\n\
             \x20   payload.require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)?;\n\
             \x20   Ok(payload)\n\
             }}\n"
        ));
    } else {
        output.push_str(&format!(
            "\npub fn encode_{symbol}_{direction}() -> Vec<u8> {{\n\
             \x20   Vec::new()\n\
             }}\n\n\
             pub fn decode_{symbol}_{direction}(\n\
             \x20   bytes: &[u8],\n\
             ) -> Result<(), ContractCallError> {{\n\
             \x20   require_empty_payload({}, {direction:?}, bytes)\n\
             }}\n",
            quoted(method_name),
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
         import { Payload } from 'fastdb4ts/payload';\n",
    );
    for digest in payload_digests(methods) {
        output.push_str(&format!(
            "import * as payload_{digest} from './payloads/{digest}/fastdb_payload_{digest}.js';\n"
        ));
    }
    output.push('\n');
    output.push_str(TYPESCRIPT_TRANSPORT_SOURCE);
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
             \x20 payload.requireSpecSha256(payload_{digest}.payloadSha256());\n\
             \x20 return payload.binaryBytes();\n\
             }}\n\n\
             export function decode_{symbol}_{direction}(data: Uint8Array): Payload {{\n\
             \x20 const payload = Payload.openCopy(payload_{digest}.compileSpec(), data);\n\
             \x20 try {{\n\
             \x20   payload.requireSpecSha256(payload_{digest}.payloadSha256());\n\
             \x20   return payload;\n\
             \x20 }} catch (error) {{\n\
             \x20   try {{\n\
             \x20     payload.dispose();\n\
             \x20   }} catch {{\n\
             \x20   }}\n\
             \x20   throw error;\n\
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
