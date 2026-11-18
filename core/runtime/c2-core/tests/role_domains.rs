//! Real direct IPC through independent runtime role domains.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallOptions, CallTimeout, Client, ClientConfigPatch, ConfigField, Connect,
    DomainLimitsOverrides, DomainLimitsPatch, EncodedClient, EncodedService, Error, Host,
    HostOptions, LifecycleError, MethodDefinition, Registration, Runtime, RuntimeOptions,
    ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};

const WAIT: Duration = Duration::from_secs(5);
const PAYLOAD: usize = 4096;

struct Resource<F>(F);

impl<F> EncodedService for Resource<F>
where
    F: Fn(Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
{
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        (self.0)(request)
    }
}

fn runtime(client_limits: DomainLimitsOverrides) -> Runtime {
    Runtime::new(RuntimeOptions {
        client_limits,
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap()
}

fn contract() -> ContractRelease {
    ContractRelease::from_descriptor_json(include_bytes!(
        "../../../../tests/fixtures/contracts/portable-release.contract.json"
    ))
    .unwrap()
}

fn register<F>(host: &Host, route: &str, service: F) -> Registration
where
    F: Fn(Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
{
    let release = contract();
    host.register(
        ServiceDefinition::new(
            &release,
            release.reference(),
            route,
            [
                MethodDefinition {
                    index: 0,
                    name: "ping".into(),
                    access: MethodAccess::Read,
                },
                MethodDefinition {
                    index: 1,
                    name: "echo".into(),
                    access: MethodAccess::Write,
                },
            ],
            Arc::new(Resource(service)),
        )
        .unwrap(),
    )
    .unwrap()
}

fn connect(client: &Runtime, host: &Host, route: &str) -> Client {
    client
        .connect(
            contract().expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().into(),
            },
        )
        .unwrap()
}

fn assert_capacity_refusal(error: Error) {
    let Error::Semantic(error) = error else {
        panic!("expected canonical capacity refusal, got {error:?}");
    };
    assert_eq!(error.code, ErrorCode::CallCapacityExceeded);
    assert_eq!(error.details["stage"], "call_execution_admission");
    assert_eq!(error.details["transport_phase"], "pre_dispatch");
}

#[test]
fn saturated_inbound_runtime_can_call_downstream_from_its_resource_worker() {
    let c = runtime(DomainLimitsOverrides::default());
    let c_host = c.host(HostOptions::default().without_relay()).unwrap();
    let _c_route = register(&c_host, "downstream", Ok);

    let b = runtime(DomainLimitsOverrides {
        memory_budget_bytes: Some(PAYLOAD as u64),
        max_outstanding_calls: Some(1),
        ..Default::default()
    });
    let b_host = b
        .host(
            HostOptions::default()
                .without_relay()
                .with_max_execution_workers(1)
                .with_memory_budget_bytes(PAYLOAD as u64)
                .with_max_outstanding_calls(1),
        )
        .unwrap();
    assert_eq!(b.server_config().unwrap().ipc.max_execution_workers, 1);

    let c_address = c_host.local_endpoint().address().to_owned();
    let mid = b.clone();
    let _b_route = register(&b_host, "middle", move |input| {
        let server = mid.memory_stats().server;
        assert_eq!(server.memory.unwrap().used_bytes, PAYLOAD as u64);
        assert_eq!(
            mid.call_execution_snapshot().server.used_operations,
            Some(1)
        );
        assert!(!mid.client_config_frozen());

        let downstream = mid
            .connect(
                contract().expected_route("downstream").unwrap(),
                Connect::DirectIpc {
                    address: c_address.clone(),
                },
            )
            .unwrap()
            .with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)));
        let mut prepared = downstream.begin_call("echo").unwrap();
        prepared.charge_input(input.len() as u64).unwrap();
        let during = mid.memory_stats();
        assert_eq!(during.client.memory.unwrap().used_bytes, PAYLOAD as u64);
        assert_eq!(during.server.memory.unwrap().used_bytes, PAYLOAD as u64);
        assert_ne!(during.client.domain_id, during.server.domain_id);
        assert_eq!(
            mid.call_execution_snapshot().client.used_operations,
            Some(1)
        );
        Ok(prepared.encode_vec(input).unwrap().call_owned().unwrap())
    });

    let a = runtime(DomainLimitsOverrides::default());
    let upstream = connect(&a, &b_host, "middle")
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)));
    assert!(!a.server_config_frozen());
    assert!(!b.client_config_frozen());
    assert!(!c.client_config_frozen());
    let request = vec![7; PAYLOAD];
    assert_eq!(upstream.call_owned("echo", &request).unwrap(), request);
    assert_eq!(b.call_execution_snapshot().client.used_operations, Some(0));
    assert_eq!(b.call_execution_snapshot().server.used_operations, Some(0));
    assert!(!c.client_config_frozen());
}

#[test]
fn self_connection_uses_one_slot_in_each_role_and_charges_empty_unary_calls() {
    let b = runtime(DomainLimitsOverrides {
        max_outstanding_calls: Some(1),
        ..Default::default()
    });
    // A patch naming just memory must preserve the explicit one-call limit.
    b.update_client_config(ClientConfigPatch {
        domain_limits: DomainLimitsPatch {
            memory_budget_bytes: ConfigField::Set(PAYLOAD as u64),
            ..Default::default()
        },
    })
    .unwrap();
    let host = b
        .host(
            HostOptions::default()
                .without_relay()
                .with_max_outstanding_calls(1),
        )
        .unwrap();
    let middle = b.clone();
    let started = Arc::new(AtomicUsize::new(0));
    let calls = started.clone();
    let _route = register(&host, "echo", move |input| {
        calls.fetch_add(1, Ordering::SeqCst);
        let slots = middle.call_execution_snapshot();
        assert_eq!(slots.client.used_operations, Some(1));
        assert_eq!(slots.server.used_operations, Some(1));
        Ok(input)
    });
    let client = connect(&b, &host, "echo");
    assert_eq!(b.call_execution_snapshot().client.max_operations, Some(1));
    b.update_client_config(ClientConfigPatch::default())
        .unwrap();
    assert_eq!(
        b.update_client_config(ClientConfigPatch {
            domain_limits: DomainLimitsPatch {
                max_outstanding_calls: ConfigField::Set(2),
                ..Default::default()
            },
        }),
        Err(LifecycleError::ConfigFrozen)
    );

    let pending = client.begin_call("echo").unwrap();
    assert!(pending.scope().deadline().is_none());
    assert_capacity_refusal(client.call_owned("echo", &[]).unwrap_err());
    assert_eq!(started.load(Ordering::SeqCst), 0);
    assert!(
        pending
            .encode_slice(&[])
            .unwrap()
            .call_owned()
            .unwrap()
            .is_empty()
    );
    assert_eq!(started.load(Ordering::SeqCst), 1);
}

#[test]
fn zero_client_memory_rejects_empty_unary_before_execution() {
    let server = runtime(DomainLimitsOverrides::default());
    let host = server.host(HostOptions::default().without_relay()).unwrap();
    let _route = register(&host, "disabled", |_| {
        panic!("client admission is disabled")
    });
    let client_runtime = runtime(DomainLimitsOverrides {
        memory_budget_bytes: Some(0),
        ..Default::default()
    });
    let client = connect(&client_runtime, &host, "disabled");
    assert_eq!(
        client_runtime
            .call_execution_snapshot()
            .client
            .max_operations,
        Some(0)
    );
    assert_capacity_refusal(client.call_owned("echo", &[]).unwrap_err());
    assert!(!client_runtime.server_config_frozen());
    assert!(!server.client_config_frozen());
}

#[test]
fn runtime_constructor_rejects_conflicting_limits_without_creating_roles() {
    assert!(matches!(
        Runtime::new(RuntimeOptions {
            client_limits: DomainLimitsOverrides {
                memory_budget_bytes: Some(0),
                max_outstanding_calls: Some(1),
                ..Default::default()
            },
            ..Default::default()
        }),
        Err(LifecycleError::Configuration(_))
    ));
    let runtime = runtime(DomainLimitsOverrides::default());
    assert!(!runtime.memory_stats().client.frozen);
    assert!(!runtime.memory_stats().server.frozen);
}

#[test]
fn relay_registration_and_withdrawal_do_not_activate_the_host_client_role() {
    use c2_http::relay::{RelayConfig, RelayServer};

    let runtime = runtime(DomainLimitsOverrides::default());
    let mut relay = RelayServer::start_resolved(
        RelayConfig {
            bind: "127.0.0.1:0".into(),
            ..Default::default()
        },
        runtime.local_endpoint_context().unwrap(),
    )
    .unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(format!("http://{}", relay.local_addr()))
                .with_relay_use_proxy(false),
        )
        .unwrap();
    assert!(runtime.server_config_frozen());
    assert!(!runtime.client_config_frozen());
    let mut registration = register(&host, "relay-echo", Ok);
    assert!(registration.outcome().relay_registered);
    assert!(!runtime.client_config_frozen());
    let closed = registration.close().unwrap();
    assert!(closed.local_removed);
    assert!(closed.relay_error.is_none(), "{closed:?}");
    assert!(runtime.memory_stats().client.domain_id.is_none());
    assert!(host.shutdown().runtime_barrier_error.is_none());
    relay.stop().unwrap();
}
