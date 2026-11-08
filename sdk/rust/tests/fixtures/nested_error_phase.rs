use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use c_two::generated::{
    AdapterFailurePhase, C2Error, EncodedClient, EncodedService, MethodAccess, MethodDefinition,
    ServiceDefinition, adapter_error,
};
use c_two::{
    Client, ConfigSources, Connect, ContractRelease, Error, Host, HostOptions, Registration,
    Runtime, RuntimeOptions,
};

struct B {
    calls: Arc<AtomicU64>,
}

impl EncodedService for B {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(method_index, 0);
        assert!(request.is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

struct A {
    b: Client,
    side_effects: Arc<AtomicU64>,
}

impl EncodedService for A {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(method_index, 0);
        assert!(request.is_empty());
        self.side_effects.fetch_add(1, Ordering::SeqCst);
        self.b.call_owned("ping", &[]).map_err(|error| {
            adapter_error(AdapterFailurePhase::ResourceFunctionExecuting, error)
        })?;
        Ok(Vec::new())
    }
}

fn definition(
    release: &ContractRelease,
    route_name: &str,
    service: Arc<dyn EncodedService>,
) -> ServiceDefinition {
    ServiceDefinition::new(
        release,
        release.reference(),
        route_name,
        [MethodDefinition {
            index: 0,
            name: "ping".into(),
            access: MethodAccess::Read,
        }],
        service,
    )
    .expect("one-method portable contract")
}

pub struct NestedErrorFixture {
    _runtime: Runtime,
    _host: Host,
    _registration_a: Registration,
    registration_b: Registration,
    client_a: Client,
    client_b: Client,
    side_effects: Arc<AtomicU64>,
    b_calls: Arc<AtomicU64>,
    pub address: String,
    pub route_a: String,
    pub route_b: String,
}

impl NestedErrorFixture {
    pub fn start(descriptor: &[u8], unix_root: Option<&Path>) -> Self {
        let release = ContractRelease::from_descriptor_json(descriptor).expect("valid descriptor");
        let suffix = std::process::id();
        let route_a = format!("nested-a-{suffix}");
        let route_b = format!("nested-b-{suffix}");
        let runtime = Runtime::new(RuntimeOptions {
            server_id: Some(format!("nested-errors-{suffix}")),
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .expect("fixture runtime");
        let endpoint = c_two::endpoint::LocalEndpointOptions {
            unix_root: if cfg!(unix) {
                unix_root.map(Path::to_path_buf)
            } else {
                None
            },
        };
        runtime
            .set_local_endpoint_with_sources(endpoint, ConfigSources::empty())
            .expect("fixture endpoint");
        let host = runtime
            .host(HostOptions::default().without_relay())
            .expect("fixture Host");
        let b_calls = Arc::new(AtomicU64::new(0));
        let registration_b = host
            .register(definition(
                &release,
                &route_b,
                Arc::new(B {
                    calls: b_calls.clone(),
                }),
            ))
            .expect("register B");
        let address = runtime.server_address().expect("Host IPC address");
        let client_b = runtime
            .connect(
                release.expected_route(&route_b).expect("B contract"),
                Connect::DirectIpc {
                    address: address.clone(),
                },
            )
            .expect("bound B client");
        let side_effects = Arc::new(AtomicU64::new(0));
        let registration_a = host
            .register(definition(
                &release,
                &route_a,
                Arc::new(A {
                    b: client_b.clone(),
                    side_effects: side_effects.clone(),
                }),
            ))
            .expect("register A");
        let client_a = runtime
            .connect(
                release.expected_route(&route_a).expect("A contract"),
                Connect::DirectIpc {
                    address: address.clone(),
                },
            )
            .expect("bound A client");
        Self {
            _runtime: runtime,
            _host: host,
            _registration_a: registration_a,
            registration_b,
            client_a,
            client_b,
            side_effects,
            b_calls,
            address,
            route_a,
            route_b,
        }
    }

    pub fn reject_b_before_callback(&mut self) {
        self.registration_b.close().expect("remove B route");
    }

    pub fn call_b(&self) -> Result<Vec<u8>, Error> {
        self.client_b.call_owned("ping", &[])
    }

    pub fn call_a(&self) -> Result<Vec<u8>, Error> {
        self.client_a.call_owned("ping", &[])
    }

    pub fn counts(&self) -> (u64, u64) {
        (
            self.side_effects.load(Ordering::SeqCst),
            self.b_calls.load(Ordering::SeqCst),
        )
    }
}
