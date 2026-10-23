use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, ConnectionEndReason, ConnectionEvent, ConnectionEventWait, ConnectionSide,
    EncodedService, HostOptions, MethodDefinition, Runtime, RuntimeOptions, ServiceDefinition,
    recv_connection_event,
};
use c2_error::C2Error;
use std::sync::Arc;
use std::time::Duration;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");
struct Echo;
impl EncodedService for Echo {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(request)
    }
}

#[tokio::test]
async fn subscription_has_no_identity_configuration_or_runtime_ownership() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let events = runtime.connection_events();
    assert!(runtime.server_id().is_none());
    assert!(!runtime.local_endpoint_frozen());
    assert!(!runtime.client_config_frozen());
    drop(runtime);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
}

#[test]
fn idle_observer_survives_views_and_does_not_retain_host_or_runtime() {
    let name = format!("observe-{}", std::process::id());
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(name.clone()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    #[cfg(unix)]
    let directory = tempfile::Builder::new()
        .prefix("c2-observe-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(unix)]
    runtime
        .set_local_endpoint(c2_core::LocalEndpointOptions {
            unix_root: Some(directory.path().to_path_buf()),
        })
        .unwrap();
    let events = runtime.connection_events();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        &name,
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
        Arc::new(Echo),
    )
    .unwrap();
    let registration = host.register(definition).unwrap();
    // Host readiness may retry its ping. Drain every admitted probe with
    // its exact terminal record before creating the client under test.
    let mut probes = std::collections::HashMap::new();
    loop {
        let timeout = if probes.is_empty() {
            Duration::ZERO
        } else {
            Duration::from_secs(2)
        };
        match recv_connection_event(&events, Some(timeout)).expect("readiness probe events") {
            Some(ConnectionEventWait::Event(ConnectionEvent::Opened(probe))) => {
                assert!(probes.insert(probe.connection_id(), probe).is_none());
            }
            Some(ConnectionEventWait::Event(ConnectionEvent::Closed(closed))) => {
                let probe = probes
                    .remove(&closed.connection_id())
                    .expect("known readiness probe");
                assert_eq!(probe.snapshot(), Some(closed));
            }
            None => {
                assert!(probes.is_empty(), "readiness probe close deadline");
                break;
            }
            other => panic!("unexpected readiness probe event: {other:?}"),
        }
    }
    let next = || match recv_connection_event(&events, Some(Duration::from_secs(2)))
        .expect("connection event wait")
    {
        Some(ConnectionEventWait::Event(event)) => Some(event),
        Some(ConnectionEventWait::Ended) => None,
        Some(ConnectionEventWait::Lagged(missed)) => {
            panic!("connection events lagged by {missed}")
        }
        Some(ConnectionEventWait::ConcurrentReceive) => panic!("only one receiver"),
        None => panic!("connection event deadline"),
    };
    let client_runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    client_runtime
        .inherit_local_endpoint_selection(&runtime)
        .unwrap();
    let outgoing = client_runtime.connection_events();
    let client = client_runtime
        .connect(
            release.expected_route(&name).unwrap(),
            Connect::DirectIpc {
                address: endpoint.address().into(),
            },
        )
        .unwrap();
    let observation = client.connection_observation().unwrap();
    let Some(ConnectionEventWait::Event(ConnectionEvent::Opened(opened))) =
        recv_connection_event(&outgoing, Some(Duration::from_secs(2))).unwrap()
    else {
        panic!("outgoing physical connection opened");
    };
    assert_eq!(opened.side(), ConnectionSide::Client);
    assert_eq!(opened.connection_id(), observation.connection_id());
    let view = client.with_call_options(Default::default());
    assert_eq!(
        view.connection_observation().unwrap().connection_id(),
        observation.connection_id()
    );
    drop(client);
    assert!(observation.snapshot().is_none());
    let ConnectionEvent::Opened(incoming) = next().unwrap() else {
        panic!("expected Opened");
    };
    assert_eq!(incoming.side(), ConnectionSide::Server);
    assert!(
        recv_connection_event(&outgoing, Some(Duration::ZERO))
            .unwrap()
            .is_none()
    );
    let outcome = host.shutdown();
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    let Some(ConnectionEventWait::Event(ConnectionEvent::Closed(closed))) =
        recv_connection_event(&outgoing, Some(Duration::from_secs(2))).unwrap()
    else {
        panic!("idle receiver observed server close");
    };
    assert_eq!(closed.connection_id(), observation.connection_id());
    assert_eq!(closed.side(), ConnectionSide::Client);
    assert!(
        matches!(
            closed.reason(),
            ConnectionEndReason::PeerEof | ConnectionEndReason::ConnectionReset
        ),
        "{closed:?}"
    );
    let incoming_closed = incoming
        .snapshot()
        .expect("server side observed its own shutdown close");
    assert_eq!(incoming_closed.reason(), ConnectionEndReason::LocalClose);
    let ConnectionEvent::Closed(event) = next().unwrap() else {
        panic!("expected Closed");
    };
    assert_eq!(event, incoming_closed);
    drop(view);
    drop(client_runtime);
    drop(registration);
    drop(host);
    drop(runtime);
    assert!(
        next().is_none(),
        "observers must not retain the Runtime or feed sender"
    );
    assert_eq!(observation.snapshot(), Some(closed));
    assert!(matches!(
        recv_connection_event(&outgoing, Some(Duration::from_secs(2))).unwrap(),
        Some(ConnectionEventWait::Ended)
    ));
    assert!(matches!(
        c2_local::inspect_endpoint(&endpoint),
        c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
    ));
}
