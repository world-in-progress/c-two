use super::{TargetMethod, TargetTopic, method_symbol, payload_digests, topic_symbol};
use crate::artifact::is_windows_reserved_name;
use base64::{Engine, engine::general_purpose::STANDARD};
use c2_contract::{MethodAccess, MethodMode, ValidatedContractDescriptor, lower_hex};
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub(super) fn render_java(
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) -> (String, String) {
    let namespace = descriptor
        .crm_namespace()
        .split('.')
        .map(java_identifier)
        .collect::<Vec<_>>()
        .join(".");
    // Many empty dot-separated segments can expand past the artifact path limit.
    let namespace = if namespace.len() > 255 {
        java_identifier(descriptor.crm_namespace())
    } else {
        namespace
    };
    let mut class = java_identifier(descriptor.crm_name());
    class[..1].make_ascii_uppercase();
    // Reserve space for javac's longest nested class filename as well as the source file.
    let class_stem_limit = 255 - "Contract$RegistrationTopics.class".len();
    bound_identifier(&mut class, descriptor.crm_name(), class_stem_limit);
    class.push_str("Contract");
    let path = format!(
        "java/src/main/java/io/ctwo/generated/{}/{class}.java",
        namespace.replace('.', "/")
    );
    let mut output = format!(
        "// generated-by: c-two.contract.codegen.v2\n\
         // payload-semantics-owner: fastdb-core\n\n\
         package io.ctwo.generated.{namespace};\n\n"
    );
    output.push_str(
        r#"import io.ctwo.CallOptions;
import io.ctwo.CallOutcome;
import io.ctwo.Client;
import io.ctwo.ContractRelease;
import io.ctwo.Held;
import io.ctwo.MethodAccess;
import io.ctwo.MethodMode;
import io.ctwo.ResponseStream;
import io.ctwo.ServerStream;
import io.ctwo.ServiceDefinition;
import io.ctwo.error.ContractException;
import io.ctwo.generated.GeneratedContractSupport;
import io.ctwo.generated.GeneratedMethod;
import io.ctwo.generated.StreamServiceHandler;
import io.ctwo.generated.UnaryCodec;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.OpenOptions;
import io.fastdb.payload.Payload;
import java.nio.charset.StandardCharsets;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Objects;
"#,
    );
    if !topics.is_empty() {
        output.push_str(
            r#"import io.ctwo.Publisher;
import io.ctwo.Registration;
import io.ctwo.SubscribeOptions;
import io.ctwo.Subscription;
import io.ctwo.generated.GeneratedTopic;
import io.fastdb.payload.BuildPolicy;
"#,
        );
    }
    writeln!(
        output,
        "\npublic final class {class} {{\n    private {class}() {{}}\n"
    )
    .unwrap();
    render_contract_facts(&mut output, descriptor, methods, topics);
    render_bindings(&mut output, methods, topics);
    render_service(&mut output, methods);
    render_client(&mut output, methods, topics);

    for method in methods {
        let symbol = method_symbol(method.index, method.name);
        render_codec(&mut output, &symbol, "input", method.input_sha256);
        render_codec(&mut output, &symbol, "output", method.output_sha256);
    }
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        render_codec(&mut output, &symbol, "value", Some(topic.output_sha256));
    }
    if !topics.is_empty() {
        render_topic_service(&mut output, topics);
    }
    output.push_str("}\n");
    (path, output)
}

fn render_contract_facts(
    output: &mut String,
    descriptor: &ValidatedContractDescriptor,
    methods: &[TargetMethod<'_>],
    topics: &[TargetTopic<'_>],
) {
    for (name, value) in [
        ("CONTRACT_SCHEMA", descriptor.contract_schema()),
        ("CRM_NAMESPACE", descriptor.crm_namespace()),
        ("CRM_NAME", descriptor.crm_name()),
        ("CRM_VERSION", descriptor.crm_version()),
        ("DESCRIPTOR_SHA256", descriptor.descriptor_sha256().as_str()),
        ("ABI_HASH", descriptor.abi_hash()),
        ("SIGNATURE_HASH", descriptor.signature_hash()),
    ] {
        writeln!(
            output,
            "    public static final String {name} = {};",
            java_string(value)
        )
        .unwrap();
    }

    // Keep nested specs opaque. Runtime joining also prevents javac from
    // folding a large descriptor into one constant-pool UTF-8 entry.
    let encoded = STANDARD.encode(descriptor.canonical_json());
    output.push_str(
        "\n    public static final String CONTRACT_DESCRIPTOR_JSON = new String(\n\
         \x20           Base64.getDecoder().decode(String.join(\"\",\n",
    );
    let mut chunks = encoded.as_bytes().chunks(16 * 1024).peekable();
    while let Some(chunk) = chunks.next() {
        write!(
            output,
            "                \"{}\"",
            std::str::from_utf8(chunk).unwrap()
        )
        .unwrap();
        if chunks.peek().is_some() {
            output.push_str(",\n");
        }
    }
    output.push_str(
        r#")), StandardCharsets.UTF_8);

    private static final class Release {
        private static final ContractRelease VALUE =
                ContractRelease.fromDescriptorJson(CONTRACT_DESCRIPTOR_JSON);
    }

    public static ContractRelease contractRelease() {
        return Release.VALUE;
    }

    public static io.ctwo.ExpectedRouteContract expectedRoute(String routeName) {
        return contractRelease().expectedRoute(routeName);
    }

    public record MethodBinding(int index, String name, MethodAccess access, MethodMode mode,
            String inputSha256, String outputSha256) {}

    public static final List<MethodBinding> METHODS = List.of("#,
    );
    let method_facts = methods
        .iter()
        .map(|method| {
            let access = match method.access {
                MethodAccess::Read => "READ",
                MethodAccess::Write => "WRITE",
            };
            let mode = match method.mode {
                MethodMode::Unary => "UNARY",
                MethodMode::ServerStream => "SERVER_STREAM",
            };
            format!(
                "\n            new MethodBinding({}, {}, MethodAccess.{access}, MethodMode.{mode}, {}, {})",
                method.index,
                java_string(method.name),
                method.input_sha256.map_or("null".into(), java_string),
                method.output_sha256.map_or("null".into(), java_string),
            )
        })
        .collect::<Vec<_>>();
    output.push_str(&method_facts.join(","));
    output.push_str(");\n");

    if !topics.is_empty() {
        output.push_str(
            "\n    public record TopicBinding(int index, String name, String outputSha256) {}\n\n\
             \x20   public static final List<TopicBinding> TOPICS = List.of(",
        );
        let topic_facts = topics
            .iter()
            .map(|topic| {
                format!(
                    "\n            new TopicBinding({}, {}, \"{}\")",
                    topic.index,
                    java_string(topic.name),
                    topic.output_sha256,
                )
            })
            .collect::<Vec<_>>();
        output.push_str(&topic_facts.join(","));
        output.push_str(");\n");
    }
}

fn render_bindings(output: &mut String, methods: &[TargetMethod<'_>], topics: &[TargetTopic<'_>]) {
    output.push_str("\n    private static final class Bindings {\n");
    for digest in payload_digests(methods, topics) {
        writeln!(
            output,
            "        private static final CompiledSpec SPEC_{digest} =\n\
             \x20               io.fastdb.payload.generated.FastdbPayload_{digest}.compileSpec();"
        )
        .unwrap();
    }
    for method in methods {
        let spec = |digest: Option<&str>| {
            digest.map_or("null".to_string(), |digest| format!("SPEC_{digest}"))
        };
        writeln!(
            output,
            "        private static final GeneratedMethod METHOD_{} = new GeneratedMethod(\n\
             \x20               {}, {}, {});",
            method.index,
            java_string(method.name),
            spec(method.input_sha256),
            spec(method.output_sha256),
        )
        .unwrap();
    }
    for topic in topics {
        writeln!(
            output,
            "        private static final GeneratedTopic TOPIC_{} = new GeneratedTopic({}, SPEC_{});",
            topic.index,
            java_string(topic.name),
            topic.output_sha256,
        )
        .unwrap();
    }
    output.push_str("    }\n");
}

fn render_service(output: &mut String, methods: &[TargetMethod<'_>]) {
    output.push_str("\n    public interface Service {\n");
    for method in methods {
        let symbol = method_symbol(method.index, method.name);
        let parameter = if method.input_sha256.is_some() {
            "Payload input"
        } else {
            ""
        };
        let result = match (method.mode, method.output_sha256) {
            (MethodMode::ServerStream, _) => "ServerStream<Payload>",
            (MethodMode::Unary, Some(_)) => "Payload",
            (MethodMode::Unary, None) => "void",
        };
        writeln!(output, "        {result} {symbol}({parameter});").unwrap();
        if method.mode == MethodMode::Unary && method.output_sha256.is_some() {
            writeln!(
                output,
                "        /** A non-null plan supplies the result in place of the payload method. */\n\
                 \x20       default BuildPlan {symbol}_plan({parameter}) {{ return null; }}"
            )
            .unwrap();
        }
    }
    output.push_str(
        r#"    }

    public static ServiceDefinition serviceDefinition(String routeName, Service service) {
        Objects.requireNonNull(service);
        var unary = new LinkedHashMap<GeneratedMethod, UnaryCodec>();
        var streams = new LinkedHashMap<GeneratedMethod, StreamServiceHandler>();
"#,
    );
    for method in methods {
        let symbol = method_symbol(method.index, method.name);
        let index = method.index;
        let input = if method.input_sha256.is_some() {
            "input"
        } else {
            ""
        };
        if method.mode == MethodMode::ServerStream {
            writeln!(
                output,
                "        streams.put(Bindings.METHOD_{index}, input -> service.{symbol}({input}));"
            )
            .unwrap();
        } else {
            writeln!(
                output,
                "        unary.put(Bindings.METHOD_{index}, input -> {{"
            )
            .unwrap();
            if method.output_sha256.is_some() {
                writeln!(
                    output,
                    "            BuildPlan plan = service.{symbol}_plan({input});\n\
                     \x20           return plan != null ? plan : service.{symbol}({input});"
                )
                .unwrap();
            } else {
                writeln!(
                    output,
                    "            service.{symbol}({input});\n            return null;"
                )
                .unwrap();
            }
            output.push_str("        });\n");
        }
    }
    output.push_str(
        "        return GeneratedContractSupport.service(contractRelease(), routeName, unary, streams);\n    }\n",
    );
}

fn render_client(output: &mut String, methods: &[TargetMethod<'_>], topics: &[TargetTopic<'_>]) {
    output.push_str(
        r#"
    public static final class ContractClient implements AutoCloseable {
        private final Client client;

        public ContractClient(Client client) {
            this.client = Objects.requireNonNull(client);
            var expected = contractRelease().expectedRoute(client.expectedRoute().routeName());
            if (!expected.equals(client.expectedRoute())) {
                throw new ContractException("Client route facts do not match generated release " + DESCRIPTOR_SHA256);
            }
        }

        public ContractClient withCallOptions(CallOptions options) {
            return new ContractClient(client.withCallOptions(options));
        }

        public CallOptions callOptions() {
            return client.callOptions();
        }

        public io.ctwo.ExpectedRouteContract expectedRoute() {
            return client.expectedRoute();
        }

        public CallOutcome queryCall(String callId) {
            return client.queryCall(callId);
        }

        @Override
        public void close() {
            client.close();
        }
"#,
    );
    for method in methods {
        render_client_method(output, method);
    }
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        let index = topic.index;
        writeln!(
            output,
            "\n        public Subscription<Payload> subscribe_{symbol}(SubscribeOptions options) {{\n\
             \x20           return GeneratedContractSupport.subscribe(client, Bindings.TOPIC_{index}, options);\n\
             \x20       }}"
        )
        .unwrap();
    }
    output.push_str("    }\n");
}

fn render_client_method(output: &mut String, method: &TargetMethod<'_>) {
    let symbol = method_symbol(method.index, method.name);
    let index = method.index;
    let result = if method.output_sha256.is_some() {
        "Payload"
    } else {
        "void"
    };
    let mut emit = |name: &str, result: &str, operation: &str, args: &[(&str, &str)]| {
        let params = args
            .iter()
            .map(|(kind, name)| format!("{kind} {name}"))
            .collect::<Vec<_>>();
        let mut arguments = format!("client, Bindings.METHOD_{index}");
        for (_, name) in args {
            write!(arguments, ", {name}").unwrap();
        }
        let return_prefix = if result == "void" { "" } else { "return " };
        writeln!(
            output,
            "\n        public {result} {name}({}) {{\n\
             \x20           {return_prefix}GeneratedContractSupport.{operation}({arguments});\n\
             \x20       }}",
            params.join(", "),
        )
        .unwrap();
    };
    let call_id = ("String", "callId");
    let input_types: &[&str] = if method.input_sha256.is_some() {
        &["Payload", "BuildPlan"]
    } else {
        &[""]
    };
    for input_type in input_types {
        let mut args = Vec::new();
        if !input_type.is_empty() {
            args.push((*input_type, "input"));
        }
        if method.mode == MethodMode::ServerStream {
            emit(&symbol, "ResponseStream<Payload>", "stream", &args);
            continue;
        }
        emit(&symbol, result, "call", &args);
        emit(&format!("hold_{symbol}"), "Held<Payload>", "hold", &args);
        args.insert(0, call_id);
        emit(
            &format!("{symbol}_with_call_id"),
            result,
            "callKeyed",
            &args,
        );
        emit(
            &format!("hold_{symbol}_with_call_id"),
            "Held<Payload>",
            "holdKeyed",
            &args,
        );
    }
    if method.mode == MethodMode::Unary {
        emit(&format!("recover_{symbol}"), result, "recover", &[call_id]);
        emit(
            &format!("recover_hold_{symbol}"),
            "Held<Payload>",
            "holdRecover",
            &[call_id],
        );
    }
}

fn render_codec(output: &mut String, symbol: &str, direction: &str, digest: Option<&str>) {
    if let Some(digest) = digest {
        writeln!(
            output,
            r#"
    public static byte[] encode_{symbol}_{direction}(Payload payload) {{
        payload.requireSpecSha256(Bindings.SPEC_{digest}.sha256());
        return payload.binaryBytes();
    }}

    public static Payload decode_{symbol}_{direction}(byte[] data) {{
        OpenOptions options = OpenOptions.defaults();
        if (data.length > options.maxTotalBytes()) {{
            options = options.withMaxTotalBytes(data.length);
        }}
        Payload payload = Payload.openCopy(Bindings.SPEC_{digest}, data, options);
        try {{
            payload.requireSpecSha256(Bindings.SPEC_{digest}.sha256());
            return payload;
        }} catch (RuntimeException | Error failure) {{
            payload.close();
            throw failure;
        }}
    }}"#
        )
        .unwrap();
    } else {
        writeln!(
            output,
            r#"
    public static byte[] encode_{symbol}_{direction}() {{
        return new byte[0];
    }}

    public static void decode_{symbol}_{direction}(byte[] data) {{
        if (data.length != 0) {{
            throw new IllegalArgumentException("Expected an empty {direction} payload for {symbol}");
        }}
    }}"#
        )
        .unwrap();
    }
}

fn render_topic_service(output: &mut String, topics: &[TargetTopic<'_>]) {
    output.push_str(
        r#"
    public static TopicService topicService(String routeName, Service service) {
        return new TopicService(serviceDefinition(routeName, service));
    }

    public static final class TopicService {
        private ServiceDefinition definition;

        private TopicService(ServiceDefinition definition) {
            this.definition = definition;
        }

        public ServiceDefinition build() {
            return definition;
        }
"#,
    );
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        let name = java_string(topic.name);
        writeln!(
            output,
            r#"
        public TopicService {symbol}_capacity(int capacity) {{
            definition = definition.withTopicCapacity({name}, capacity);
            return this;
        }}

        public TopicService initial_{symbol}(Payload payload) {{
            definition = definition.withInitialTopicBytes({name}, encode_{symbol}_value(payload));
            return this;
        }}

        public TopicService initial_{symbol}(BuildPlan plan) {{
            try (var result = plan.execute(BuildPolicy.ALLOW_STAGING)) {{
                return initial_{symbol}(result.payload());
            }}
        }}"#
        )
        .unwrap();
    }
    output.push_str(
        r#"    }

    public static RegistrationTopics registrationTopics(Registration registration) {
        return new RegistrationTopics(registration);
    }

    public static final class RegistrationTopics {
        private final Registration registration;

        private RegistrationTopics(Registration registration) {
            this.registration = Objects.requireNonNull(registration);
        }
"#,
    );
    for topic in topics {
        let symbol = topic_symbol(topic.index, topic.name);
        let index = topic.index;
        writeln!(
            output,
            "\n        public Publisher<Payload> {symbol}() {{\n\
             \x20           var topic = Bindings.TOPIC_{index};\n\
             \x20           return registration.publisher(topic.name(), topic.spec());\n\
             \x20       }}"
        )
        .unwrap();
    }
    output.push_str("    }\n");
}

fn java_identifier(raw: &str) -> String {
    let mut ident: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if ident.is_empty() || ident.as_bytes()[0].is_ascii_digit() {
        ident.insert(0, '_');
    }
    if matches!(
        ident.as_str(),
        "_" | "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "false"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "null"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "true"
            | "try"
            | "void"
            | "volatile"
            | "while"
    ) || is_windows_reserved_name(&ident)
    {
        ident.push('_');
    }
    // A wire name can be 255 bytes before its leading underscore is added.
    bound_identifier(&mut ident, raw, 255);
    ident
}

fn bound_identifier(ident: &mut String, raw: &str, limit: usize) {
    if ident.len() > limit {
        let digest = lower_hex(&<[u8; 32]>::from(Sha256::digest(raw)));
        ident.truncate(limit - 17);
        write!(ident, "_{}", &digest[..16]).unwrap();
    }
}

fn java_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for unit in value.encode_utf16() {
        match unit {
            0x22 => quoted.push_str("\\\""),
            0x5c => quoted.push_str("\\\\"),
            0..=0x1f => write!(quoted, "\\{unit:03o}").unwrap(),
            0x20..=0x7e => quoted.push(char::from(unit as u8)),
            _ => write!(quoted, "\\u{unit:04x}").unwrap(),
        }
    }
    quoted.push('"');
    quoted
}
