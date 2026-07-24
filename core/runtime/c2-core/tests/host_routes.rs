use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use c2_contract::{ContractRelease, ContractReleaseRef, ExpectedRouteContract, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, Error, HostOptions, MethodDefinition, Runtime,
    RuntimeOptions, ServiceConcurrencyMode, ServiceDefinition,
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
            name: "ping".to_string(),
            access: MethodAccess::Read,
        },
        MethodDefinition {
            index: 1,
            name: "echo".to_string(),
            access: MethodAccess::Write,
        },
    ]
}

fn nonportable_identity(route_name: &str) -> (ContractReleaseRef, ExpectedRouteContract) {
    let descriptor = br#"{
        "schema":"c-two.python.crm.descriptor.v2",
        "crm":{"namespace":"test.python-only","name":"PickleEcho","version":"0.1.0"},
        "methods":[{"name":"ping"},{"name":"echo"}]
    }"#;
    let descriptor_sha256 =
        c2_contract::contract_descriptor_sha256_hex(descriptor).expect("descriptor digest");
    let release_ref = ContractReleaseRef::from_json(
        format!(
            r#"{{
                "schema":"c-two.contract-release-ref.v1",
                "contract_schema":"c-two.python.crm.descriptor.v2",
                "crm":{{
                    "namespace":"test.python-only",
                    "name":"PickleEcho",
                    "version":"0.1.0"
                }},
                "descriptor_sha256":"{descriptor_sha256}"
            }}"#
        )
        .as_bytes(),
    )
    .expect("nonportable release reference");
    let expected = ExpectedRouteContract {
        route_name: route_name.to_string(),
        crm_ns: "test.python-only".to_string(),
        crm_name: "PickleEcho".to_string(),
        crm_ver: "0.1.0".to_string(),
        abi_hash: "a".repeat(64),
        signature_hash: "b".repeat(64),
    };
    (release_ref, expected)
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
                name: "ping".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 0,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        [
            MethodDefinition {
                index: 0,
                name: "renamed".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        [
            MethodDefinition {
                index: 0,
                name: "ping".to_string(),
                access: MethodAccess::Write,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
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

#[test]
fn explicitly_nonportable_service_keeps_its_schema_marker_and_uses_the_same_core_host() {
    let route_name = unique_name("python-only");
    let (release_ref, expected) = nonportable_identity(&route_name);
    let definition = ServiceDefinition::new_nonportable(
        release_ref.clone(),
        expected.clone(),
        methods(),
        Arc::new(Echo),
    )
    .expect("explicit nonportable definition");
    assert_eq!(definition.release_ref(), &release_ref);
    assert_eq!(definition.expected_route(), &expected);

    let portable = release();
    let error = ServiceDefinition::new_nonportable(
        portable.reference(),
        expected.clone(),
        methods(),
        Arc::new(Echo),
    )
    .expect_err("portable schema cannot enter the nonportable constructor");
    assert!(matches!(error, Error::Contract(_)));

    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("python-only-server")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let _registration = host.register(definition).expect("register route");
    let client = runtime
        .connect(
            expected,
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("client");
    assert_eq!(
        client.call_owned("echo", b"pickle-bytes").expect("call"),
        b"pickle-bytes"
    );
}

#[test]
fn registration_projects_the_core_owned_route_concurrency_state() {
    let release = release();
    let route_name = unique_name("concurrency");
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("concurrency-server")),
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
    .expect("definition")
    .with_concurrency(ServiceConcurrencyMode::Exclusive, Some(3), Some(1))
    .expect("concurrency options");
    let mut registration = host.register(definition).expect("register route");
    let concurrency = registration.route_concurrency();
    let snapshot = concurrency.snapshot();
    assert_eq!(snapshot.mode, ServiceConcurrencyMode::Exclusive);
    assert_eq!(snapshot.max_pending, Some(3));
    assert_eq!(snapshot.max_workers, Some(1));
    assert!(!snapshot.closed);

    let guard = concurrency
        .blocking_acquire(0)
        .expect("same-process execution guard");
    let active = concurrency.snapshot();
    assert_eq!(active.active_workers, 1);
    drop(guard);
    registration.close().expect("close route");
    assert!(concurrency.snapshot().closed);
}
