use std::sync::Arc;

use c_two::generated::{
    C2Error, EncodedClient, EncodedService, ErrorCode, MethodAccess, MethodDefinition,
    ServiceDefinition,
};
use c_two::{Connect, ContractRelease, HostOptions, Runtime, RuntimeOptions};

const DESCRIPTOR: &str = r#"{"schema":"c-two.contract.v2","crm":{"namespace":"test.contract-release","name":"Portable","version":"0.1.0"},"fingerprints":{"abi_hash":"bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe","signature_hash":"c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940"},"methods":[{"access":"read","name":"ping","parameters":[],"return":{"kind":"none"},"bindings":{"input":null,"output":null}},{"access":"write","name":"echo","parameters":[{"name":"payload","kind":"POSITIONAL_OR_KEYWORD","default":{"kind":"missing"},"type":{"kind":"payload"}}],"return":{"kind":"payload"},"bindings":{"input":{"kind":"fastdb","spec":{"schema":"fastdb.payload.v1","profile":"record.v1","entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],"components":[]}},"output":{"kind":"fastdb","spec":{"schema":"fastdb.payload.v1","profile":"record.v1","entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],"components":[]}}}}]}"#;

struct Resource;

impl EncodedService for Resource {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request.to_vec()),
            _ => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                "unexpected generated method index",
            )),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes())?;
    let route_name = format!("rust-sdk-host-example-{}", std::process::id());
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(format!("rust-sdk-host-{}", std::process::id())),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })?;
    let host = runtime.host(HostOptions::default().without_relay())?;
    let mut registration = host.register(ServiceDefinition::new(
        &release,
        release.reference(),
        &route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        Arc::new(Resource),
    )?)?;

    let address = runtime
        .server_address()
        .ok_or("host did not publish an IPC address")?;
    let probe = runtime.connect(
        release.expected_route(&route_name)?,
        Connect::DirectIpc {
            address: address.clone(),
        },
    )?;
    assert!(probe.call_owned("ping", &[])?.is_empty());
    println!("hosted {route_name} at {address}");

    registration.close()?;
    Ok(())
}
