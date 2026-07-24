use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, Error, HostOptions, MethodDefinition, Runtime,
    RuntimeOptions, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request.to_vec()),
            _ => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                "method index escaped validated definition",
            )),
        }
    }
}

fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).expect("valid release")
}

fn methods() -> [MethodDefinition; 2] {
    [
        MethodDefinition {
            index: 0,
            name: "ping",
            access: MethodAccess::Read,
        },
        MethodDefinition {
            index: 1,
            name: "echo",
            access: MethodAccess::Write,
        },
    ]
}

#[test]
fn service_definition_requires_exact_release_method_indices_names_and_access() {
    let release = release();
    let route_name = unique_name("validated");
    ServiceDefinition::new(
        &release,
        release.reference(),
        &route_name,
        methods(),
        Arc::new(Echo),
    )
    .expect("exact method definition");

    let bad_cases = [
        [
            MethodDefinition {
                index: 1,
                name: "ping",
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 0,
                name: "echo",
                access: MethodAccess::Write,
            },
        ],
        [
            MethodDefinition {
                index: 0,
                name: "renamed",
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo",
                access: MethodAccess::Write,
            },
        ],
        [
            MethodDefinition {
                index: 0,
                name: "ping",
                access: MethodAccess::Write,
            },
            MethodDefinition {
                index: 1,
                name: "echo",
                access: MethodAccess::Write,
            },
        ],
    ];

    for bad in bad_cases {
        let error = ServiceDefinition::new(
            &release,
            release.reference(),
            &route_name,
            bad,
            Arc::new(Echo),
        )
        .expect_err("method mismatch must fail before route construction");
        assert!(matches!(error, Error::Contract(_)));
    }
}

#[test]
fn host_registration_owns_the_server_route_and_idempotent_cleanup() {
    let release = release();
    let route_name = unique_name("host");
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("host-server")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        &route_name,
        methods(),
        Arc::new(Echo),
    )
    .expect("definition");
    let mut registration = host.register(definition).expect("register route");
    assert_eq!(registration.route_name(), route_name);
    assert!(!registration.outcome().relay_registered);

    let client = runtime
        .connect(
            release.expected_route(&route_name).expect("expected route"),
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("client");
    assert_eq!(
        client.call_owned("echo", b"payload").expect("call"),
        b"payload"
    );

    let first = registration.close().expect("first close");
    let second = registration.close().expect("second close");
    assert_eq!(first, second);
    assert!(first.local_removed);

    let Error::Semantic(error) = client
        .call_owned("echo", b"after-close")
        .expect_err("closed binding must not dispatch")
    else {
        panic!("route removal must be semantic");
    };
    assert_eq!(error.code, ErrorCode::ResourceRemoved);
}

#[test]
fn dropping_registration_performs_best_effort_route_cleanup() {
    let release = release();
    let route_name = unique_name("drop");
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("drop-server")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let client = {
        let definition = ServiceDefinition::new(
            &release,
            release.reference(),
            &route_name,
            methods(),
            Arc::new(Echo),
        )
        .expect("definition");
        let registration = host.register(definition).expect("register route");
        let client = runtime
            .connect(
                release.expected_route(&route_name).expect("expected route"),
                Connect::DirectIpc {
                    address: runtime.server_address().expect("server address"),
                },
            )
            .expect("client");
        drop(registration);
        client
    };

    let Error::Semantic(error) = client
        .call_owned("echo", b"after-drop")
        .expect_err("dropped registration must close route")
    else {
        panic!("route removal must be semantic");
    };
    assert_eq!(error.code, ErrorCode::ResourceRemoved);
}
