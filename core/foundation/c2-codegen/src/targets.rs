use crate::{
    ArtifactKind, ArtifactProvenance, CodegenError, ContractArtifact, ContractCodegenTarget,
};
use c2_contract::{MethodAccess, MethodMode, ValidatedContractDescriptor};
use serde_json::Value;
use std::collections::BTreeSet;

const TYPESCRIPT_TRANSPORT_SOURCE: &str = include_str!("../assets/typescript_transport.ts");

pub(crate) struct TargetMethod<'a> {
    pub(crate) index: usize,
    pub(crate) name: &'a str,
    pub(crate) access: MethodAccess,
    pub(crate) mode: MethodMode,
    pub(crate) input_sha256: Option<&'a str>,
    pub(crate) output_sha256: Option<&'a str>,
}

pub(crate) struct TargetTopic<'a> {
    pub(crate) index: usize,
    pub(crate) name: &'a str,
    pub(crate) output_sha256: &'a str,
}

pub(crate) fn render_target_artifacts(
    target: ContractCodegenTarget,
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) -> Result<Vec<ContractArtifact>, CodegenError> {
    let (path, source) = match target {
        ContractCodegenTarget::Rust => (
            "rust/c_two_contract.rs",
            render_rust(descriptor, methods, topics),
        ),
        ContractCodegenTarget::Python => (
            "python/c_two_contract.py",
            render_python(descriptor, methods, topics),
        ),
        ContractCodegenTarget::TypeScript => (
            "typescript/c_two_contract.ts",
            render_typescript(descriptor, methods, topics),
        ),
    };
    Ok(vec![ContractArtifact::new(
        path,
        ArtifactKind::Source,
        source.into_bytes(),
        ArtifactProvenance::new("c-two", format!("{}-contract-module", target.as_str()))?,
    )?])
}

fn render_rust(
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) -> String {
    let has_stream = methods
        .iter()
        .any(|method| method.mode == MethodMode::ServerStream);
    // A topics-only contract keeps the standard unary dispatch scaffolding,
    // which is inert there; keep its compilation warning-free.
    let header = if methods.is_empty() {
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\
         #![allow(non_snake_case)]\n\
         #![allow(dead_code, unused_variables)]\n\n"
    } else {
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\
         #![allow(non_snake_case)]\n\n"
    };
    let mut output = String::from(header);
    for digest in payload_digests(methods, topics) {
        output.push_str(&format!(
            "#[path = \"payloads/{digest}/fastdb_payload_{digest}.rs\"]\n\
             pub mod payload_{digest};\n\n"
        ));
    }
    render_rust_contract_facts(&mut output, descriptor, methods, topics);
    output.push_str(
        r#"
pub fn contract_release() -> Result<c_two::ContractRelease, c_two::Error> {
    Ok(c_two::ContractRelease::from_descriptor_json(
        CONTRACT_DESCRIPTOR_JSON.as_bytes(),
    )?)
}

pub fn contract_release_ref() -> Result<c_two::contract::ContractReleaseRef, c_two::Error> {
    Ok(contract_release()?.reference())
}

pub fn expected_route(
    route_name: impl Into<String>,
) -> Result<c_two::contract::ExpectedRouteContract, c_two::Error> {
    Ok(contract_release()?.expected_route(route_name)?)
}

/// Typed calls consume the sealed c_two::generated::EncodedClient authority
/// through Core preparation, before caller-side serialization.
#[derive(Clone)]
pub struct ContractClient {
    client: c_two::Client,
}

impl ContractClient {
    pub fn new(client: c_two::Client) -> Result<Self, c_two::Error> {
        let expected = expected_route(client.expected_route().route_name.clone())?;
        if client.expected_route() != &expected {
            return Err(c_two::contract::ContractError::InvalidDescriptor {
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

    /// Immutable policy view over the same route and connection acquisition.
    pub fn with_call_options(&self, options: c_two::CallOptions) -> Self {
        Self { client: self.client.with_call_options(options) }
    }

    pub fn call_options(&self) -> c_two::CallOptions {
        self.client.call_options()
    }

    pub fn expected_route(&self) -> &c_two::contract::ExpectedRouteContract {
        self.client.expected_route()
    }

    /// Read the already-acquired physical IPC connection without reconnecting.
    pub fn connection_observation(&self) -> Result<c_two::observe::ConnectionObservation, c_two::Error> {
        self.client.connection_observation()
    }
"#,
    );
    for method in methods {
        render_rust_client_method(&mut output, method);
    }
    for topic in topics {
        render_rust_topic_client_method(&mut output, topic);
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
        request: Vec<u8>,
    ) -> Result<Vec<u8>, c_two::generated::C2Error> {
        match method_index {
"#,
    );
    for method in methods {
        if method.mode == MethodMode::ServerStream {
            continue;
        }
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
        request: Vec<u8>,
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
"#,
    );
    if has_stream {
        render_rust_stream_service(&mut output, methods);
    }
    output.push_str(
        r#"
pub fn service_definition<S: Service>(
    route_name: impl Into<String>,
    service: S,
) -> Result<c_two::ServiceDefinition, c_two::Error> {
    let release = contract_release()?;
    let release_ref = release.reference();
    let methods = METHODS
        .iter()
        .map(|method| -> Result<c_two::generated::MethodDefinition, c_two::Error> {
            let index = u16::try_from(method.index).map_err(|_| {
                c_two::contract::ContractError::InvalidDescriptor {
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
"#,
    );
    output.push_str(if has_stream {
        "    c_two::ServiceDefinition::new_stream(\n"
    } else {
        "    c_two::ServiceDefinition::new(\n"
    });
    output.push_str(
        r#"        &release,
        release_ref,
        route_name,
        methods,
        std::sync::Arc::new(ServiceAdapter { service }),
    )
}

"#,
    );
    if !topics.is_empty() {
        render_rust_topic_service(&mut output, topics);
    }
    for method in methods {
        render_rust_payload_adapters(&mut output, method);
    }
    for topic in topics {
        render_rust_topic_payload_adapters(&mut output, topic);
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
    topics: &[TargetTopic<'_>],
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
    if topics.is_empty() {
        return;
    }
    output.push_str(
        "\n\
         #[derive(Clone, Copy, Debug, Eq, PartialEq)]\n\
         pub struct TopicBinding {\n\
         \x20   pub index: usize,\n\
         \x20   pub name: &'static str,\n\
         \x20   pub output_sha256: &'static str,\n\
         }\n\n\
         pub const TOPICS: &[TopicBinding] = &[\n",
    );
    for topic in topics {
        output.push_str(&format!(
            "    TopicBinding {{ index: {}, name: {}, output_sha256: {} }},\n",
            topic.index,
            quoted(topic.name),
            quoted(topic.output_sha256),
        ));
    }
    output.push_str("];\n");
}

fn render_rust_client_method(output: &mut String, method: &TargetMethod<'_>) {
    if method.mode == MethodMode::ServerStream {
        render_rust_stream_client_method(output, method);
        return;
    }
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
    let preparation = if method.input_sha256.is_some() {
        "        c_two::generated::charge_payload_input(&mut call, payload)?;\n"
    } else {
        ""
    };
    let binding = if method.input_sha256.is_some() {
        "mut call"
    } else {
        "call"
    };
    output.push_str(&format!(
        "\n    pub fn {symbol}(&self{parameter}) -> Result<{return_type}, c_two::Error> {{\n\
         \x20       let {binding} = self.client.begin_call({})?;\n\
         {preparation}\
         \x20       let request = {request};\n\
         \x20       let response = call.encode_vec(request)?.call_owned()?;\n\
         \x20       decode_{symbol}_output(response)\n\
         \x20   }}\n",
        quoted(method.name),
    ));
    if method.output_sha256.is_some() {
        output.push_str(&format!(
            "\n    pub fn hold_{symbol}(\n\
             \x20       &self{parameter},\n\
             \x20   ) -> Result<c_two::Held<fastdb::Payload>, c_two::Error> {{\n\
             \x20       let {binding} = self.client.begin_call({})?;\n\
             {preparation}\
             \x20       let request = {request};\n\
             \x20       let response = call.encode_vec(request)?.call_held()?;\n\
             \x20       decode_{symbol}_output_held(response)\n\
             \x20   }}\n",
            quoted(method.name),
        ));
    }
}

fn render_rust_stream_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        ", payload: &fastdb::Payload"
    } else {
        ""
    };
    let request = if method.input_sha256.is_some() {
        format!("encode_{symbol}_input(payload)?")
    } else {
        format!("encode_{symbol}_input()")
    };
    let preparation = if method.input_sha256.is_some() {
        "        c_two::generated::charge_payload_input(&mut call, payload)?;\n"
    } else {
        ""
    };
    let binding = if method.input_sha256.is_some() {
        "mut call"
    } else {
        "call"
    };
    output.push_str(&format!(
        "\n    pub fn {symbol}(&self{parameter}) -> Result<c_two::ResponseStream<fastdb::Payload>, c_two::Error> {{\n\
         \x20       let {binding} = self.client.begin_stream({})?;\n\
         {preparation}\
         \x20       let request = {request};\n\
         \x20       let stream = call.encode_vec(request)?.stream()?;\n\
         \x20       Ok(c_two::ResponseStream::from_core(stream, |response| {{\n\
         \x20           decode_{symbol}_output(response.bytes())\n\
         \x20       }}))\n\
         \x20   }}\n",
        quoted(method.name),
    ));
    output.push_str(&format!(
        "\n    pub fn hold_{symbol}(\n\
         \x20       &self{parameter},\n\
         \x20   ) -> Result<c_two::ResponseStream<c_two::Held<fastdb::Payload>>, c_two::Error> {{\n\
         \x20       let {binding} = self.client.begin_stream({})?;\n\
         {preparation}\
         \x20       let request = {request};\n\
         \x20       let stream = call.encode_vec(request)?.stream()?;\n\
         \x20       Ok(c_two::ResponseStream::from_core(stream, decode_{symbol}_output_held))\n\
         \x20   }}\n",
        quoted(method.name),
    ));
}

fn render_rust_stream_service(output: &mut String, methods: &[TargetMethod<'_>]) {
    output.push_str(
        r#"
impl<S: Service> ServiceAdapter<S> {
    fn open_stream_checked(
        &self,
        method_index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn c_two::generated::EncodedStream>, c_two::generated::C2Error> {
        match method_index {
"#,
    );
    for method in methods
        .iter()
        .filter(|method| method.mode == MethodMode::ServerStream)
    {
        let symbol = method_symbol(method.index, method.name);
        output.push_str(&format!("            {} => {{\n", method.index));
        if method.input_sha256.is_some() {
            output.push_str(&format!(
                "                let input = decode_{symbol}_input_borrowed(request)?;\n\
                 \x20               let iterator = self.service.{symbol}(input.payload()).map_err(|error| {{\n\
                 \x20                   c_two::generated::adapter_error(\n\
                 \x20                       c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,\n\
                 \x20                       error,\n\
                 \x20                   )\n\
                 \x20               }})?;\n\
                 \x20               let input = Some(input);\n"
            ));
        } else {
            output.push_str(&format!(
                "                decode_{symbol}_input(&request)?;\n\
                 \x20               let iterator = self.service.{symbol}().map_err(|error| {{\n\
                 \x20                   c_two::generated::adapter_error(\n\
                 \x20                       c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,\n\
                 \x20                       error,\n\
                 \x20                   )\n\
                 \x20               }})?;\n\
                 \x20               let input = None;\n"
            ));
        }
        let digest = method
            .output_sha256
            .expect("stream payload output validated by contract");
        output.push_str(&format!(
            "                Ok(Box::new(c_two::generated::PayloadStream::new(\n\
             \x20                   iterator, input, payload_{digest}::PAYLOAD_SHA256_BYTES, {}, {},\n\
             \x20               )))\n\
             \x20           }}\n",
            quoted(method.name),
            method.index,
        ));
    }
    output.push_str(
        r#"            other => Err(c_two::generated::C2Error::new(
                c_two::generated::ErrorCode::ProtocolViolation,
                format!("unknown generated stream method index {other}"),
            )),
        }
    }
}

impl<S: Service> c_two::generated::EncodedStreamService for ServiceAdapter<S> {
    fn open_stream(
        &self,
        method_index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn c_two::generated::EncodedStream>, c_two::generated::C2Error> {
        let method_name = METHODS
            .get(usize::from(method_index))
            .map(|method| method.name)
            .unwrap_or("<unknown>");
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.open_stream_checked(method_index, request)
        })) {
            Ok(result) => result,
            Err(_) => Err(c_two::generated::normalize_adapter_failure(
                c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
                c_two::generated::AdapterFailure::Local {
                    message: format!("service method {method_name:?} stream factory panicked"),
                    details: std::collections::BTreeMap::from([
                        ("method".to_string(), method_name.to_string()),
                        ("method_index".to_string(), method_index.to_string()),
                    ]),
                },
            )),
        }
    }
}
"#,
    );
}

fn render_rust_service_trait_method(output: &mut String, method: &TargetMethod<'_>) {
    if method.mode == MethodMode::ServerStream {
        let symbol = method_symbol(method.index, method.name);
        let parameter = if method.input_sha256.is_some() {
            ", input: &fastdb::Payload"
        } else {
            ""
        };
        output.push_str(&format!(
            "    fn {symbol}(&self{parameter}) -> Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>;\n"
        ));
        return;
    }
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
            "                decode_{symbol}_input(&request)?;\n\
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
             \x20   bytes: impl Into<Vec<u8>>,\n\
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
        let rendered = format!(
            "pub fn decode_{symbol}_output(\n\
             \x20   bytes: impl Into<Vec<u8>>,\n\
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
             \x20       return Err(c_two::contract::ContractError::InvalidDescriptor {{\n\
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
             \x20       c_two::generated::release_held_after_error(&mut held);\n\
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
        );
        if method.mode == MethodMode::ServerStream {
            let encoder = format!("fn encode_{symbol}_output(");
            output.push_str(
                rendered
                    .split_once(&encoder)
                    .expect("payload renderer has a unary output encoder")
                    .0,
            );
        } else {
            output.push_str(&rendered);
        }
    } else {
        output.push_str(&format!(
            "pub fn decode_{symbol}_output(bytes: impl Into<Vec<u8>>) -> Result<(), c_two::Error> {{\n\
             \x20   require_empty_payload(\n\
             \x20       c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,\n\
             \x20       {},\n\
             \x20       \"output\",\n\
             \x20       &bytes.into(),\n\
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

fn render_rust_topic_client_method(output: &mut String, topic: &TargetTopic<'_>) {
    let symbol = topic_symbol(topic.index, topic.name);
    output.push_str(&format!(
        "\n    /// Subscribe to the {} topic declared on this contract.\n\
         \x20   pub fn subscribe_{symbol}(\n\
         \x20       &self,\n\
         \x20       options: c_two::SubscribeOptions,\n\
         \x20   ) -> Result<c_two::Subscription<fastdb::Payload>, c_two::Error> {{\n\
         \x20       let subscription = self.client.subscribe({}, options)?;\n\
         \x20       Ok(c_two::Subscription::from_core(\n\
         \x20           subscription,\n\
         \x20           |bytes| decode_{symbol}_value(bytes),\n\
         \x20           decode_{symbol}_value_held,\n\
         \x20       ))\n\
         \x20   }}\n",
        quoted(topic.name),
        quoted(topic.name),
    ));
}

fn render_rust_topic_service(output: &mut String, topics: &[TargetTopic<'_>]) {
    output.push_str(
        r#"/// Registration configuration for this contract's topics: bounded history
/// capacity and initial frames committed atomically before the route opens.
pub struct TopicService {
    definition: c_two::ServiceDefinition,
}

/// Start topic-aware registration of `service` on `route_name`. Topics left
/// unconfigured keep their default ring capacity of 1.
pub fn topic_service<S: Service>(
    route_name: impl Into<String>,
    service: S,
) -> Result<TopicService, c_two::Error> {
    Ok(TopicService {
        definition: service_definition(route_name, service)?,
    })
}

impl TopicService {
"#,
    );
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        output.push_str(&format!(
            "    /// Set the bounded history ring depth of topic {}.\n\
             \x20   pub fn {symbol}_capacity(mut self, capacity: usize) -> Result<Self, c_two::Error> {{\n\
             \x20       self.definition = self.definition.with_topic_capacity({}, capacity)?;\n\
             \x20       Ok(self)\n\
             \x20   }}\n\n\
             \x20   /// Stage the initial frame of topic {}, committed atomically before\n\
             \x20   /// the route opens and rolled back if registration fails.\n\
             \x20   pub fn initial_{symbol}(mut self, value: &fastdb::Payload) -> Result<Self, c_two::Error> {{\n\
             \x20       let payload = encode_{symbol}_value(value)?;\n\
             \x20       self.definition = self.definition.with_initial_topic_bytes({}, payload)?;\n\
             \x20       Ok(self)\n\
             \x20   }}\n\n",
            quoted(topic.name),
            quoted(topic.name),
            quoted(topic.name),
            quoted(topic.name),
        ));
    }
    output.push_str(
        r#"    /// Finish configuration and return the release-verified definition.
    pub fn build(self) -> c_two::ServiceDefinition {
        self.definition
    }
}

/// Typed publisher handles bound to one committed registration. A handle
/// fails with `ResourceClosed` (707) once its registration ends and never
/// writes to a later registration of the same route name.
pub struct RegistrationTopics<'a> {
    registration: &'a c_two::Registration,
}

/// Borrow typed publisher handles from a committed registration.
pub fn registration_topics(registration: &c_two::Registration) -> RegistrationTopics<'_> {
    RegistrationTopics { registration }
}

impl RegistrationTopics<'_> {
"#,
    );
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        output.push_str(&format!(
            "    /// Publisher for the {} topic of this registration.\n\
             \x20   pub fn {symbol}(&self) -> Result<c_two::Publisher<fastdb::Payload>, c_two::Error> {{\n\
             \x20       Ok(c_two::Publisher::from_core(\n\
             \x20           self.registration.publisher({})?,\n\
             \x20           encode_{symbol}_value,\n\
             \x20       ))\n\
             \x20   }}\n\n",
            quoted(topic.name),
            quoted(topic.name),
        ));
    }
    output.push_str("}\n\n");
}

fn render_rust_topic_payload_adapters(output: &mut String, topic: &TargetTopic<'_>) {
    let symbol = topic_symbol(topic.index, topic.name);
    let digest = topic.output_sha256;
    output.push_str(&format!(
        "pub fn encode_{symbol}_value(\n\
         \x20   payload: &fastdb::Payload,\n\
         ) -> Result<Vec<u8>, c_two::Error> {{\n\
         \x20   payload\n\
         \x20       .require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
         \x20       .map_err(|error| c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
         \x20           c_two::generated::AdapterFailurePhase::ResourceOutputSerializing,\n\
         \x20           error,\n\
         \x20       )))?;\n\
         \x20   payload.binary_bytes().map_err(|error| {{\n\
         \x20       c_two::Error::Semantic(c_two::generated::fastdb_adapter_error(\n\
         \x20           c_two::generated::AdapterFailurePhase::ResourceOutputSerializing,\n\
         \x20           error,\n\
         \x20       ))\n\
         \x20   }})\n\
         }}\n\n\
         pub fn decode_{symbol}_value(\n\
         \x20   bytes: impl Into<Vec<u8>>,\n\
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
         pub fn decode_{symbol}_value_held(\n\
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
         \x20       return Err(c_two::contract::ContractError::InvalidDescriptor {{\n\
         \x20           path: \"$.held\".to_string(),\n\
         \x20           message: \"new held topic payload was already released\".to_string(),\n\
         \x20       }}.into());\n\
         \x20   }};\n\
         \x20   if let Err(error) =\n\
         \x20       payload.require_spec_sha256(&payload_{digest}::PAYLOAD_SHA256_BYTES)\n\
         \x20   {{\n\
         \x20       let semantic = c_two::generated::fastdb_adapter_error(\n\
         \x20           c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,\n\
         \x20           error,\n\
         \x20       );\n\
         \x20       c_two::generated::release_held_after_error(&mut held);\n\
         \x20       return Err(c_two::Error::Semantic(semantic));\n\
         \x20   }}\n\
         \x20   Ok(held)\n\
         }}\n\n"
    ));
}

fn render_python(
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) -> String {
    let has_stream = methods
        .iter()
        .any(|method| method.mode == MethodMode::ServerStream);
    let mut output = String::from(
        "# generated-by: c-two.contract.codegen.v2\n\
         # payload-semantics-owner: fastdb-core\n\
         from __future__ import annotations\n\n\
         import importlib.util\n\
         import sys\n\
         from dataclasses import dataclass\n\
         from pathlib import Path\n\
         from types import ModuleType\n\
         from typing import Any, Protocol\n\n",
    );
    output.push_str(
        "from fastdb4py.payload import Payload\n\
         from c_two.error import CCBaseError, ErrorCode, SerializationError, _post_dispatch_cleanup_error, _project_native_error\n\n",
    );
    match (has_stream, !topics.is_empty()) {
        (true, false) => output.push_str("from c_two import HeldResult, Stream\n\n"),
        (false, true) => output.push_str("from c_two import HeldResult, Subscription\n\n"),
        (true, true) => output.push_str("from c_two import HeldResult, Stream, Subscription\n\n"),
        (false, false) => {}
    }
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
         \x20   output_sha256: str | None\n",
    );
    if has_stream {
        output.push_str("    mode: str\n");
    }
    output.push_str("\nMETHODS = (\n");
    for method in methods {
        let mode = if has_stream {
            format!(
                ", {}",
                quoted(if method.mode == MethodMode::ServerStream {
                    "server_stream"
                } else {
                    "unary"
                })
            )
        } else {
            String::new()
        };
        output.push_str(&format!(
            "    MethodBinding({}, {}, {}, {}, {}{mode}),\n",
            method.index,
            quoted(method.name),
            quoted(access_name(method.access)),
            python_option(method.input_sha256),
            python_option(method.output_sha256),
        ));
    }
    output.push_str(")\n");
    if !topics.is_empty() {
        output.push_str(
            "\n\
             @dataclass(frozen=True)\n\
             class TopicBinding:\n\
             \x20   index: int\n\
             \x20   name: str\n\
             \x20   output_sha256: str\n\n\
             TOPICS = (\n",
        );
        for topic in topics {
            output.push_str(&format!(
                "    TopicBinding({}, {}, {}),\n",
                topic.index,
                quoted(topic.name),
                quoted(topic.output_sha256),
            ));
        }
        output.push_str(")\n");
    }
    output.push_str(
        "\n\
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
    for digest in payload_digests(methods, topics) {
        output.push_str(&format!(
            "_PAYLOAD_{digest} = _load_payload_module({})\n",
            quoted(digest),
        ));
    }
    if methods
        .iter()
        .any(|method| method.input_sha256.is_some() || method.output_sha256.is_some())
        || !topics.is_empty()
    {
        output.push('\n');
    }
    output.push_str(PYTHON_ERROR_HELPERS);
    output.push_str("class BoundClient(Protocol):\n    def call(self, method_name: str, data: bytes) -> Any: ...\n");
    if has_stream {
        output.push_str(
            "    @property\n    def local_copy_tracker(self) -> Any: ...\n\
             \x20   @property\n    def route_name(self) -> str: ...\n\
             \x20   def begin_stream(self, method_name: str) -> Any: ...\n",
        );
    }
    if !topics.is_empty() {
        output.push_str(
            "    def subscribe(self, topic: str, *, start: str = \"latest\", on_lag: str = \"skip\", hold: bool = False) -> Any: ...\n",
        );
    }
    output.push_str(
        "\n\
         def _copy_and_release_response(response: Any) -> bytes:\n\
         \x20   primary = None\n\
         \x20   try:\n\
         \x20       try:\n\
         \x20           return bytes(response)\n\
         \x20       except BaseException as exc:\n\
         \x20           raise _serialization_error(ErrorCode.ClientOutputFromBuffer, exc) from exc\n\
         \x20   except BaseException as exc:\n\
         \x20       primary = exc\n\
         \x20       raise\n\
         \x20   finally:\n\
         \x20       try:\n\
         \x20           _release_response(response, primary)\n\
         \x20       finally:\n\
         \x20           primary = None\n\n\
         class ContractClient:\n\
         \x20   def __init__(self, bound_client: BoundClient) -> None:\n\
         \x20       self._bound_client = bound_client\n",
    );
    for method in methods {
        render_python_client_method(&mut output, method);
    }
    for topic in topics {
        render_python_topic_client_method(&mut output, topic);
    }
    output.push('\n');
    for method in methods {
        render_python_direction(&mut output, method, "input", method.input_sha256);
        render_python_direction(&mut output, method, "output", method.output_sha256);
    }
    for topic in topics {
        render_python_topic_value(&mut output, topic);
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

const PYTHON_ERROR_HELPERS: &str = "\
def _safe_error_text(value: Any) -> str:
    try:
        return str(value)
    except BaseException:
        return f\"<{type(value).__qualname__}: unprintable value>\"

def _fastdb_cause_details(exc: BaseException) -> dict[str, str]:
    payload_error = getattr(sys.modules.get(\"fastdb4py.payload\"), \"PayloadError\", None)
    if not isinstance(payload_error, type) or not issubclass(type(exc), payload_error):
        return {}
    fields: dict[str, str] = {}
    for source, target in (
        (\"code\", \"fastdb_code\"),
        (\"symbol\", \"fastdb_symbol\"),
        (\"path\", \"fastdb_path\"),
        (\"message\", \"fastdb_message\"),
        (\"details_json\", \"fastdb_details_json\"),
    ):
        value = getattr(exc, source, None)
        if value is not None:
            fields[target] = _safe_error_text(value)
    if fields:
        fields[\"cause_owner\"] = \"fastdb\"
    return fields

def _serialization_error(code: ErrorCode, exc: BaseException) -> SerializationError:
    return SerializationError(code, _safe_error_text(exc), details=_fastdb_cause_details(exc))

def _client_input_error(exc: BaseException) -> SerializationError:
    return SerializationError(
        ErrorCode.ClientInputSerializing,
        _safe_error_text(exc),
        details=_fastdb_cause_details(exc),
        transport_phase=\"pre_dispatch\",
    )

def _release_failed_payload(payload: Any) -> None:
    try:
        close = getattr(payload, \"close\", None)
        if close is not None:
            close()
    except BaseException as cleanup_error:
        try:
            print(f\"Warning: payload decode cleanup failed: {_safe_error_text(cleanup_error)}\", file=sys.stderr)
        except BaseException:
            pass

def _note_cleanup_failure(primary: BaseException, cleanup_error: BaseException) -> None:
    try:
        add_note = getattr(primary, \"add_note\", None)
        if add_note is not None:
            add_note(f\"Cleanup failed: {_safe_error_text(cleanup_error)}\")
    except BaseException:
        pass

def _close_prepared(prepared: Any, primary: BaseException | None, dispatched: bool = False) -> None:
    try:
        try:
            prepared.close()
        except BaseException as cleanup_error:
            if primary is None:
                if dispatched:
                    cleaned = _post_dispatch_cleanup_error(cleanup_error)
                    if cleaned is not cleanup_error:
                        try:
                            raise cleaned from cleanup_error
                        finally:
                            cleaned = None
                            cleanup_error = None
                raise
            try:
                _note_cleanup_failure(primary, cleanup_error)
            finally:
                cleanup_error = None
    finally:
        primary = None

def _cancel_unclaimed_stream(native: Any, primary: BaseException) -> None:
    try:
        cancel = getattr(native, \"cancel\", None)
        if callable(cancel):
            _project_call(cancel)
    except BaseException as cancel_error:
        try:
            _note_cleanup_failure(primary, cancel_error)
        finally:
            cancel_error = None
    finally:
        primary = None

def _release_response(response: Any, primary: BaseException | None) -> None:
    try:
        try:
            release = getattr(response, \"release\", None)
            if release is not None:
                release()
        except BaseException as exc:
            if primary is not None:
                try:
                    _note_cleanup_failure(primary, exc)
                finally:
                    exc = None
            else:
                raise _serialization_error(ErrorCode.ClientOutputFromBuffer, exc) from exc
    finally:
        primary = None

def _project_call(operation, *args):
    \"\"\"Project a native call failure's canonical semantic error, if any.

    A CRMProxy bound client already projects internally; this thin guard
    keeps custom native bound clients on the same typed-exception surface.
    \"\"\"
    try:
        return operation(*args)
    except BaseException as exc:
        projected = _project_native_error(exc)
        if projected is not None:
            try:
                raise projected from exc
            finally:
                projected = None
                exc = None
        raise

";

fn render_python_client_method(output: &mut String, method: &TargetMethod<'_>) {
    if method.mode == MethodMode::ServerStream {
        render_python_stream_client_method(output, method);
        return;
    }
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
         \x20       response = _project_call(self._bound_client.call, {}, request)\n\
         \x20       return decode_{symbol}_output(_copy_and_release_response(response))\n",
        quoted(method.name),
    ));
}

fn render_python_topic_client_method(output: &mut String, topic: &TargetTopic<'_>) {
    let symbol = topic_symbol(topic.index, topic.name);
    for held in [false, true] {
        let name = if held {
            format!("hold_subscribe_{symbol}")
        } else {
            format!("subscribe_{symbol}")
        };
        let return_type = if held {
            "Subscription[HeldResult[Payload]]"
        } else {
            "Subscription[Payload]"
        };
        let hold_literal = if held { "True" } else { "False" };
        output.push_str(&format!(
            "\n    def {name}(self, *, start: str = \"latest\", on_lag: str = \"skip\") -> {return_type}:\n\
             \x20       return _project_call(lambda: self._bound_client.subscribe({}, start=start, on_lag=on_lag, hold={hold_literal}))\n",
            quoted(topic.name),
        ));
    }
}

fn render_python_stream_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let parameter = if method.input_sha256.is_some() {
        ", payload: Payload"
    } else {
        ""
    };
    let request = if method.input_sha256.is_some() {
        format!("encode_{symbol}_input(payload)")
    } else {
        format!("encode_{symbol}_input()")
    };
    for held in [false, true] {
        let name = if held {
            format!("hold_{symbol}")
        } else {
            symbol.clone()
        };
        let return_type = if held {
            "Stream[HeldResult[Payload]]"
        } else {
            "Stream[Payload]"
        };
        let invalidator = if held {
            ", invalidator=Payload.invalidate"
        } else {
            ""
        };
        let held_literal = if held { "True" } else { "False" };
        let method_name = quoted(method.name);
        let charge_block = if method.input_sha256.is_some() {
            "\
             \x20               try:\n\
             \x20                   used_bytes = payload.execution_report().used_bytes\n\
             \x20               except BaseException as exc:\n\
             \x20                   raise _client_input_error(exc) from exc\n\
             \x20               _project_call(prepared.charge_input, used_bytes)\n"
        } else {
            ""
        };
        output.push_str(&format!(
            "\n    def {name}(self{parameter}) -> {return_type}:\n\
             \x20       prepared = _project_call(self._bound_client.begin_stream, {method_name})\n\
             \x20       primary = None\n\
             \x20       dispatched = False\n\
             \x20       unclaimed = None\n\
             \x20       try:\n\
             \x20           try:\n\
             {charge_block}\
             \x20               request = {request}\n\
             \x20               dispatched = True\n\
             \x20               unclaimed = _project_call(prepared.stream, request)\n\
             \x20           except BaseException as exc:\n\
             \x20               primary = exc\n\
             \x20               raise\n\
             \x20           finally:\n\
             \x20               try:\n\
             \x20                   _close_prepared(prepared, primary, dispatched)\n\
             \x20               except BaseException as close_exc:\n\
             \x20                   if primary is None:\n\
             \x20                       primary = close_exc\n\
             \x20                   close_exc = None\n\
             \x20                   raise\n\
             \x20           try:\n\
             \x20               stream_obj = Stream._from_encoded(unclaimed, decode_{symbol}_output, held={held_literal}{invalidator}, tracker=self._bound_client.local_copy_tracker, route_name=self._bound_client.route_name, method_name={method_name}, decoder_classifies=True)\n\
             \x20           except BaseException as wrap_exc:\n\
             \x20               primary = wrap_exc\n\
             \x20               clean = _post_dispatch_cleanup_error(wrap_exc)\n\
             \x20               primary = clean\n\
             \x20               try:\n\
             \x20                   if clean is not wrap_exc:\n\
             \x20                       raise clean from wrap_exc\n\
             \x20                   raise\n\
             \x20               finally:\n\
             \x20                   clean = None\n\
             \x20                   wrap_exc = None\n\
             \x20           unclaimed = None\n\
             \x20           return stream_obj\n\
             \x20       finally:\n\
             \x20           try:\n\
             \x20               if unclaimed is not None:\n\
             \x20                   if primary is not None:\n\
             \x20                       _cancel_unclaimed_stream(unclaimed, primary)\n\
             \x20                   else:\n\
             \x20                       cancel = getattr(unclaimed, 'cancel', None)\n\
             \x20                       if callable(cancel):\n\
             \x20                           cancel()\n\
             \x20           finally:\n\
             \x20               unclaimed = None\n\
             \x20               primary = None\n",
        ));
    }
}

fn render_python_direction(
    output: &mut String,
    method: &TargetMethod<'_>,
    direction: &'static str,
    digest: Option<&str>,
) {
    let symbol = method_symbol(method.index, method.name);
    if let Some(digest) = digest {
        // Client-side input encoding is retry-safe pre-dispatch; server-side
        // output encoding carries code 2.
        let encode_code = if direction == "input" {
            "_client_input_error(exc)"
        } else {
            "_serialization_error(ErrorCode.ResourceOutputSerializing, exc)"
        };
        // The input decoder runs on the resource side (codes 4/1), the output
        // decoder on the client side (codes 8/6); opening the buffer view is
        // from_buffer, verifying the payload spec is deserialize.
        let (from_buffer_code, deserialize_code) = if direction == "input" {
            (
                "ErrorCode.ResourceInputFromBuffer",
                "ErrorCode.ResourceInputDeserializing",
            )
        } else {
            (
                "ErrorCode.ClientOutputFromBuffer",
                "ErrorCode.ClientOutputDeserializing",
            )
        };
        output.push_str(&format!(
            "def encode_{symbol}_{direction}(payload: Payload) -> bytes:\n\
             \x20   try:\n\
             \x20       payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20       return payload.binary_bytes()\n\
             \x20   except BaseException as exc:\n\
             \x20       raise {encode_code} from exc\n\n\
             def decode_{symbol}_{direction}(data: bytes) -> Payload:\n\
             \x20   try:\n\
             \x20       spec = _PAYLOAD_{digest}.compile_spec()\n\
             \x20   except BaseException as exc:\n\
             \x20       raise _serialization_error({from_buffer_code}, exc) from exc\n\
             \x20   try:\n\
             \x20       payload = Payload.open_copy(spec, data)\n\
             \x20   except BaseException as exc:\n\
             \x20       raise _serialization_error({from_buffer_code}, exc) from exc\n\
             \x20   try:\n\
             \x20       payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20   except BaseException as exc:\n\
             \x20       _release_failed_payload(payload)\n\
             \x20       raise _serialization_error({deserialize_code}, exc) from exc\n\
             \x20   return payload\n\n"
        ));
    } else {
        let decode_code = if direction == "input" {
            "ErrorCode.ResourceInputDeserializing"
        } else {
            "ErrorCode.ClientOutputDeserializing"
        };
        output.push_str(&format!(
            "def encode_{symbol}_{direction}() -> bytes:\n\
             \x20   return b\"\"\n\n\
             def decode_{symbol}_{direction}(data: bytes) -> None:\n\
             \x20   try:\n\
             \x20       _require_empty_payload({}, {direction:?}, data)\n\
             \x20   except BaseException as exc:\n\
             \x20       raise _serialization_error({decode_code}, exc) from exc\n\n",
            quoted(method.name),
        ));
    }
}

/// Topic value codecs mirroring `render_python_direction`'s output direction:
/// publishing serializes a resource output, subscribing deserializes a client
/// output, and every decoded payload is checked against the contract's
/// FastDB specification digest.
fn render_python_topic_value(output: &mut String, topic: &TargetTopic<'_>) {
    let symbol = topic_symbol(topic.index, topic.name);
    let digest = topic.output_sha256;
    output.push_str(&format!(
        "def encode_{symbol}_value(payload: Payload) -> bytes:\n\
         \x20   try:\n\
         \x20       payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
         \x20       return payload.binary_bytes()\n\
         \x20   except BaseException as exc:\n\
         \x20       raise _serialization_error(ErrorCode.ResourceOutputSerializing, exc) from exc\n\n\
         def decode_{symbol}_value(data: bytes) -> Payload:\n\
         \x20   try:\n\
         \x20       spec = _PAYLOAD_{digest}.compile_spec()\n\
         \x20   except BaseException as exc:\n\
         \x20       raise _serialization_error(ErrorCode.ClientOutputFromBuffer, exc) from exc\n\
         \x20   try:\n\
         \x20       payload = Payload.open_copy(spec, data)\n\
         \x20   except BaseException as exc:\n\
         \x20       raise _serialization_error(ErrorCode.ClientOutputFromBuffer, exc) from exc\n\
         \x20   try:\n\
         \x20       payload.require_spec_sha256(_PAYLOAD_{digest}.PAYLOAD_SHA256_BYTES)\n\
         \x20   except BaseException as exc:\n\
         \x20       _release_failed_payload(payload)\n\
         \x20       raise _serialization_error(ErrorCode.ClientOutputDeserializing, exc) from exc\n\
         \x20   return payload\n\n"
    ));
}

fn render_typescript(
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) -> String {
    let mut output = String::from(
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\
         import { Payload, PayloadError } from 'fastdb4ts/payload';\n",
    );
    for digest in payload_digests(methods, topics) {
        output.push_str(&format!(
            "import * as payload_{digest} from './payloads/{digest}/fastdb_payload_{digest}.js';\n"
        ));
    }
    output.push('\n');
    output.push_str(&render_typescript_transport_source());
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
         \x20 readonly access: 'read' | 'write';\n",
    );
    if methods
        .iter()
        .any(|method| method.mode == MethodMode::ServerStream)
    {
        output.push_str("  readonly mode?: 'server_stream';\n");
    }
    output.push_str(
        "  readonly inputSha256?: string;\n\
         \x20 readonly outputSha256?: string;\n\
         }\n\n\
         export const METHODS: readonly MethodBinding[] = Object.freeze([\n",
    );
    for method in methods {
        output.push_str(&format!(
            "  {{ index: {}, name: {}, access: {}{}{}{} }},\n",
            method.index,
            quoted(method.name),
            quoted(access_name(method.access)),
            if method.mode == MethodMode::ServerStream {
                ", mode: 'server_stream'"
            } else {
                ""
            },
            typescript_optional_field("inputSha256", method.input_sha256),
            typescript_optional_field("outputSha256", method.output_sha256),
        ));
    }
    output.push_str("]);\n");
    if !topics.is_empty() {
        output.push_str(
            "\n\
             export interface TopicBinding {\n\
             \x20 readonly index: number;\n\
             \x20 readonly name: string;\n\
             \x20 readonly outputSha256: string;\n\
             }\n\n\
             export const TOPICS: readonly TopicBinding[] = Object.freeze([\n",
        );
        for topic in topics {
            output.push_str(&format!(
                "  {{ index: {}, name: {}, outputSha256: {} }},\n",
                topic.index,
                quoted(topic.name),
                quoted(topic.output_sha256),
            ));
        }
        output.push_str("]);\n");
    }
    output.push_str("\nexport class ContractClient {\n  constructor(\n");
    let stream = methods
        .iter()
        .any(|method| method.mode == MethodMode::ServerStream);
    let transport = match (stream, topics.is_empty()) {
        (false, true) => "C2EncodedClientTransport",
        (true, true) => "C2StreamEncodedClientTransport",
        (false, false) => "C2ClientTransport",
        (true, false) => "C2StreamEncodedClientTransport & C2ClientTransport",
    };
    output.push_str(&format!("    private readonly transport: {transport},\n"));
    output.push_str("    private readonly routeName: string,\n  ) {}\n");
    for method in methods {
        render_typescript_client_method(&mut output, method);
    }
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        output.push_str(&format!(
            "\n  subscribe_{symbol}(options?: TopicSubscribeOptions): Promise<C2TopicSubscription<Payload>> {{\n\
             \x20   return this.transport.subscribe(this.routeName, CONTRACT, {}, decode_{symbol}_output, options);\n\
             \x20 }}\n\n\
             \x20 hold_subscribe_{symbol}(options?: TopicSubscribeOptions): Promise<C2TopicSubscription<C2HeldResult<Payload>>> {{\n\
             \x20   return this.transport.subscribe(this.routeName, CONTRACT, {}, decode_{symbol}_output, options, true);\n\
             \x20 }}\n",
            quoted(topic.name), quoted(topic.name),
        ));
    }
    output.push_str("}\n\n");
    for method in methods {
        let symbol = method_symbol(method.index, method.name);
        render_typescript_direction(
            &mut output,
            &symbol,
            method.name,
            "input",
            method.input_sha256,
        );
        render_typescript_direction(
            &mut output,
            &symbol,
            method.name,
            "output",
            method.output_sha256,
        );
    }
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        render_typescript_direction(
            &mut output,
            &symbol,
            topic.name,
            "output",
            Some(topic.output_sha256),
        );
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
         \x20 } catch (cleanupError) {\n\
         \x20   console.warn(\"Warning: held payload decode cleanup failed:\", cleanupError);\n\
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
    if method.mode == MethodMode::ServerStream {
        output.push_str(&format!(
            "\n  {symbol}({parameter}): C2ResponseStream<Payload> {{\n\
             \x20   return this.transport.stream(this.routeName, CONTRACT, {}, () => {request}, decode_{symbol}_output);\n\
             \x20 }}\n\n\
             \x20 hold_{symbol}({parameter}): C2ResponseStream<C2HeldResult<Payload>> {{\n\
             \x20   return this.transport.stream(this.routeName, CONTRACT, {}, () => {request}, decode_{symbol}_output, true);\n\
             \x20 }}\n",
            quoted(method.name),
            quoted(method.name),
        ));
        return;
    }
    output.push_str(&format!(
        "\n  async {symbol}({parameter}): Promise<{return_type}> {{\n\
         \x20   const response = requireResponsePayload(\n\
         \x20     await this.transport.call(this.routeName, CONTRACT, {}, () => {request}),\n\
         \x20     \"encoded transport response\",\n\
         \x20   );\n\
         \x20   return decodeAndReleaseResponse(response, decode_{symbol}_output);\n\
         \x20 }}\n",
        quoted(method.name),
    ));
    if method.output_sha256.is_some() {
        output.push_str(&format!(
            "\n  async hold_{symbol}({parameter}): Promise<C2HeldResult<{return_type}>> {{\n\
             \x20   const response = requireResponsePayload(\n\
             \x20     await this.transport.call(this.routeName, CONTRACT, {}, () => {request}),\n\
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
    symbol: &str,
    name: &str,
    direction: &'static str,
    digest: Option<&str>,
) {
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
             \x20   }} catch (cleanupError) {{\n\
             \x20     console.warn(\"Warning: payload decode cleanup failed:\", cleanupError);\n\
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
            quoted(name),
        ));
    }
}

fn payload_digests<'a>(
    methods: &'a [TargetMethod<'a>],
    topics: &'a [TargetTopic<'a>],
) -> BTreeSet<&'a str> {
    methods
        .iter()
        .flat_map(|method| [method.input_sha256, method.output_sha256])
        .chain(topics.iter().map(|topic| Some(topic.output_sha256)))
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

fn topic_symbol(index: usize, name: &str) -> String {
    let mut symbol = format!("topic_{index}_");
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            symbol.push(character.to_ascii_lowercase());
        } else {
            symbol.push('_');
        }
    }
    symbol
}

const TS_ERROR_CODE_PLACEHOLDER: &str = "/* __C2_GENERATED_ERROR_CODE_NAMES__ */ Object.freeze({})";

fn render_typescript_transport_source() -> String {
    let generated_table = render_typescript_error_code_table();
    let rendered = TYPESCRIPT_TRANSPORT_SOURCE.replace(TS_ERROR_CODE_PLACEHOLDER, &generated_table);
    assert_ne!(
        rendered, TYPESCRIPT_TRANSPORT_SOURCE,
        "assets/typescript_transport.ts is missing TS_ERROR_CODE_PLACEHOLDER"
    );
    rendered
}

fn render_typescript_error_code_table() -> String {
    let mut table = String::from("Object.freeze({\n");
    for entry in c2_error::ErrorCode::registry() {
        table.push_str(&format!(
            "  {}: {},\n",
            u16::from(entry.code),
            quoted(entry.name)
        ));
    }
    table.push_str("})");
    table
}

#[cfg(test)]
mod registry_sync_tests {
    use super::{
        TS_ERROR_CODE_PLACEHOLDER, TYPESCRIPT_TRANSPORT_SOURCE, render_typescript_error_code_table,
        render_typescript_transport_source,
    };

    #[test]
    fn typescript_transport_injects_generated_error_code_table() {
        assert!(TYPESCRIPT_TRANSPORT_SOURCE.contains(TS_ERROR_CODE_PLACEHOLDER));
        let rendered = render_typescript_transport_source();
        assert!(!rendered.contains("__C2_GENERATED_ERROR_CODE_NAMES__"));
        assert!(rendered.contains(&render_typescript_error_code_table()));
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
