//! Retirement's observation budget never becomes a dispatched call's deadline.
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, HostOptions, MethodDefinition, Runtime, RuntimeOptions,
    RuntimeShutdownTarget, ServiceDefinition,
};
use c2_error::C2Error;

const WAIT: Duration = Duration::from_secs(5);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");

struct PausedEcho {
    entered: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
}
impl EncodedService for PausedEcho {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        self.entered.send(()).unwrap();
        self.resume.lock().unwrap().recv_timeout(WAIT).unwrap();
        Ok(request)
    }
}

#[test]
fn runtime_shutdown_preserves_dispatched_ipc_call_past_observation_budget() {
    for hosted in [false, true] {
        let root = tempfile::Builder::new()
            .prefix("c2-inflight-")
            .tempdir()
            .unwrap();
        let runtime = Runtime::new(RuntimeOptions {
            server_id: Some(format!("inflight-{}", uuid::Uuid::new_v4().simple())),
            use_process_relay_anchor: false,
            shm_threshold: Some(1),
            ..Default::default()
        })
        .unwrap();
        runtime
            .set_local_endpoint_with_sources(
                c2_config::LocalEndpointOptions {
                    #[cfg(unix)]
                    unix_root: Some(root.path().to_path_buf()),
                    ..Default::default()
                },
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
        let peer = Runtime::new(RuntimeOptions {
            server_id: Some(format!("inflight-peer-{}", uuid::Uuid::new_v4().simple())),
            use_process_relay_anchor: false,
            shm_threshold: Some(1),
            ..Default::default()
        })
        .unwrap();
        peer.inherit_local_endpoint_selection(&runtime).unwrap();
        let host = if hosted { &runtime } else { &peer }
            .host(HostOptions::default().without_relay())
            .unwrap();
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        let (entered, started) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let _registration = host
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    "echo",
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
                    Arc::new(PausedEcho {
                        entered,
                        resume: Mutex::new(paused),
                    }),
                )
                .unwrap(),
            )
            .unwrap();
        let client = runtime
            .connect(
                release.expected_route("echo").unwrap(),
                Connect::DirectIpc {
                    address: host.local_endpoint().address().into(),
                },
            )
            .unwrap();
        let observation = client.connection_observation().unwrap();
        let call = std::thread::spawn(move || client.call_owned("echo", &vec![9; 8193]));
        started.recv_timeout(WAIT).unwrap();
        let target = if hosted {
            RuntimeShutdownTarget::Host(&host)
        } else {
            RuntimeShutdownTarget::ClientsOnly
        };
        let receipt = runtime
            .begin_session_retirement(target, Instant::now() + WAIT)
            .unwrap();
        let (pending, completed) =
            receipt.observe_until(Instant::now() + Duration::from_millis(20));
        assert!(
            !completed,
            "active IPC callback must keep shutdown pending: {pending:?}"
        );
        assert!(
            !call.is_finished(),
            "retirement must preserve the dispatched call waiter"
        );
        assert!(observation.snapshot().is_none());
        resume.send(()).unwrap();
        assert_eq!(call.join().unwrap().unwrap(), vec![9; 8193]);
        let repeated = runtime
            .retirement_receipt(Instant::now() + WAIT)
            .unwrap()
            .unwrap();
        let (terminal, completed) = repeated.observe_until(Instant::now() + WAIT);
        assert!(
            completed,
            "original retirement must finish after the reply: {terminal:?}"
        );
        assert!(terminal.ipc_clients_drained, "{terminal:?}");
        assert_eq!(receipt.terminal_outcome(), Some(terminal));
        assert!(observation.snapshot().is_some());
        if !hosted {
            assert!(host.shutdown().runtime_barrier_error.is_none());
        }
    }
}
