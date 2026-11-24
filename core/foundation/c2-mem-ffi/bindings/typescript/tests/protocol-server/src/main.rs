//! Real c2-server peer for the Node IPC 18 / ABI 10 gate. No Python or FastDB.
use c2_config::{ConfigResolver, ConfigSources, EnvCatalog, LocalEndpointOptions};
use c2_contract::{ContractRelease, derive_contract_fingerprints_json};
use c2_server::{
    AdmissionContext, AdmissionDecision, ConcurrencyMode, CrmCallback, CrmError, CrmStream,
    CrmStreamCallback, CrmStreamItem, ResponseMeta, RouteBuildSpec, SchedulerLimits, Server,
    ServerIpcConfig,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../../../c2-contract/tests/fixtures/server-stream.contract.json");

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct Echo {
    generation: u8,
    gate: Arc<Gate>,
    unary_gate: Arc<Gate>,
    unary_dispatches: Arc<AtomicUsize>,
    stream_dispatches: Arc<AtomicUsize>,
}
impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        method: u16,
        mut request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if method != 0 {
            return Err(CrmError::InternalError("unexpected unary method".into()));
        }
        self.unary_dispatches.fetch_add(1, Ordering::Relaxed);
        if request == [253] {
            return Err(CrmError::UserError(
                c2_error::C2Error::new(
                    c2_error::ErrorCode::ResourceFunctionExecuting,
                    "keyed failure",
                )
                .to_wire_bytes(),
            ));
        }
        if request == [255] || request == [254] {
            emit(json!({"kind": "unary_blocked"}));
            self.unary_gate.wait();
            if request == [254] {
                emit(json!({"kind": "unary_completed"}));
            }
        }
        if self.generation != 0 {
            request.push(self.generation);
        }
        Ok(ResponseMeta::Inline(request))
    }
}

struct BytesStream {
    mode: u8,
    count: u8,
    length: usize,
    cursor: u8,
    gate: Arc<Gate>,
}
impl CrmStream for BytesStream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        if self.mode == 3 && self.cursor == 0 {
            emit(json!({"kind": "second_stream_step"}));
        }
        if self.mode == 6 && self.cursor < self.count {
            emit(json!({"kind": "item_step", "index": self.cursor}));
        }
        if (self.mode == 2 || self.mode == 4 || self.mode == 5 || self.mode == 7)
            && self.cursor == 1
        {
            emit(json!({"kind": "step_blocked"}));
            self.gate.wait();
        }
        if self.mode != 7 && self.cursor >= self.count {
            if self.mode == 1 {
                let error = c2_error::C2Error::new(
                    c2_error::ErrorCode::ResourceFunctionExecuting,
                    "node fixture stream failure",
                )
                .with_details(std::collections::BTreeMap::from([(
                    "transport_phase".into(),
                    "pre_dispatch".into(),
                )]));
                return Err(CrmError::UserError(error.to_wire_bytes()));
            }
            return Ok(None);
        }
        let index = self.cursor;
        self.cursor = self.cursor.wrapping_add(1);
        Ok(Some(Box::new(vec![index; self.length])))
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        if self.mode == 4 {
            // The driver decides the natural terminal before entering cleanup.
            emit(json!({"kind": "continued_after_deadline"}));
        }
        if self.mode == 5 {
            emit(json!({"kind": "stream_cleanup"}));
        }
        Ok(())
    }
}

impl CrmStreamCallback for Echo {
    fn open_stream(
        &self,
        _: &str,
        method: u16,
        request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        if method != 1 || request.len() != 6 {
            return Err(CrmError::InternalError("invalid stream request".into()));
        }
        self.stream_dispatches.fetch_add(1, Ordering::Relaxed);
        let length = u32::from_le_bytes(request[2..6].try_into().unwrap()) as usize;
        Ok(Box::new(BytesStream {
            mode: request[0],
            count: request[1],
            length,
            cursor: 0,
            gate: self.gate.clone(),
        }))
    }
}
fn emit(value: serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let catalog = EnvCatalog::load(ConfigSources::empty()).unwrap();
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: (args[0] != "-").then(|| args[0].clone().into()),
        },
        &catalog,
    )
    .unwrap();
    let endpoint = context.endpoint(&args[1]).unwrap();
    let mut config = ServerIpcConfig::default();
    if args.get(2).is_some_and(|mode| mode == "metadata_only") {
        config.outcome_max_retained_bytes = Some(0);
    }
    let mut server = Server::new_with_endpoint(endpoint, config).unwrap();
    let admission_enabled = args.get(2).is_some_and(|mode| mode == "admission");
    if admission_enabled {
        server = server.with_admission_hook(Arc::new(|context: &AdmissionContext<'_>| {
            if context.metadata().get("token") == Some("allowed") {
                AdmissionDecision::allow()
            } else {
                AdmissionDecision::deny("missing token")
            }
        }));
    }
    let server = Arc::new(server);
    let topics_enabled = admission_enabled || args.get(2).is_some_and(|mode| mode == "topics");
    let release = if topics_enabled {
        let mut descriptor: serde_json::Value = serde_json::from_slice(DESCRIPTOR).unwrap();
        descriptor["topics"] = json!([{
            "name": "state", "output": {"kind": "payload"},
            "binding": descriptor["methods"][1]["bindings"]["output"],
        }]);
        let fingerprints =
            derive_contract_fingerprints_json(descriptor.to_string().as_bytes()).unwrap();
        descriptor["fingerprints"] = json!({
            "abi_hash": fingerprints.abi_hash(),
            "signature_hash": fingerprints.signature_hash(),
        });
        ContractRelease::from_descriptor_json(descriptor.to_string().as_bytes()).unwrap()
    } else {
        ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap()
    };
    let descriptor = release.descriptor();
    let gate = Arc::new(Gate::default());
    let unary_gate = Arc::new(Gate::default());
    let unary_dispatches = Arc::new(AtomicUsize::new(0));
    let stream_dispatches = Arc::new(AtomicUsize::new(0));
    let route_for_generation = |generation| {
        let route = server
            .build_stream_route(
                RouteBuildSpec {
                    name: "echo".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: vec!["ping".into(), "echo".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Echo {
                    generation,
                    gate: gate.clone(),
                    unary_gate: unary_gate.clone(),
                    unary_dispatches: unary_dispatches.clone(),
                    stream_dispatches: stream_dispatches.clone(),
                }),
                descriptor,
            )
            .unwrap();
        if !topics_enabled {
            return (route, None);
        }
        let topics = server.create_route_topics(
            release.expected_route("echo").unwrap(),
            &BTreeMap::from([("state".into(), Default::default())]),
        );
        let publisher = topics.publisher("state").unwrap();
        publisher.publish(vec![generation]).unwrap();
        (route.with_topics(topics), Some(publisher))
    };
    let (route, mut publisher) = route_for_generation(0);
    server
        .commit_reserved_route(server.reserve_route(route).await.unwrap())
        .await
        .unwrap();
    let running = server.clone();
    let task = tokio::spawn(async move { running.run().await });
    server
        .wait_until_ready(Duration::from_secs(10))
        .await
        .unwrap();
    emit(json!({"kind": "ready", "contract": {
        "schema": descriptor.contract_schema(),
        "namespace": descriptor.crm_namespace(),
        "name": descriptor.crm_name(),
        "version": descriptor.crm_version(),
        "descriptorSha256": release.descriptor_sha256().to_string(),
        "abiHash": descriptor.abi_hash(),
        "signatureHash": descriptor.signature_hash()
    }}));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        if line == "quit" {
            break;
        }
        if let Some(bytes) = line.strip_prefix("publish ") {
            let value: u8 = bytes.parse().unwrap();
            let seq = publisher.as_ref().unwrap().publish(vec![value]).unwrap();
            emit(json!({"kind": "published", "seq": seq}));
        }
        if let Some(range) = line.strip_prefix("publish_burst ") {
            let (first, last) = range.split_once(' ').unwrap();
            let first: u8 = first.parse().unwrap();
            let last: u8 = last.parse().unwrap();
            let publisher = publisher.as_ref().unwrap();
            let seqs: Vec<_> = (first..=last)
                .map(|value| publisher.publish(vec![value]).unwrap())
                .collect();
            emit(json!({"kind": "published_burst", "seqs": seqs}));
        }
        if line == "stats" {
            emit(
                json!({"kind": "stats", "allocations": server.response_pool_arc().read().stats().alloc_count,
                    "unary_dispatches": unary_dispatches.load(Ordering::Relaxed),
                    "stream_dispatches": stream_dispatches.load(Ordering::Relaxed)}),
            );
        }
        if line == "release" {
            gate.release();
            emit(json!({"kind": "released"}));
        }
        if line == "release_unary" {
            unary_gate.release();
            emit(json!({"kind": "unary_released"}));
        }
        if line == "replace" {
            assert!(server.unregister_route("echo").await);
            emit(json!({"kind": "route_removed"}));
        }
        if line == "register_replacement" {
            let (route, next_publisher) = route_for_generation(1);
            publisher = next_publisher;
            server
                .commit_reserved_route(server.reserve_route(route).await.unwrap())
                .await
                .unwrap();
            emit(json!({"kind": "route_replaced"}));
        }
        if line == "shutdown" || line == "unregister" {
            let cause = line.clone();
            let observing = server.clone();
            tokio::spawn(async move {
                loop {
                    if observing
                        .route_scheduler_snapshot("echo")
                        .await
                        .is_some_and(|snapshot| snapshot.closed)
                    {
                        emit(json!({"kind": "route_closed", "cause": cause}));
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            });
            if line == "shutdown" {
                server.request_shutdown_signal_with_reason("shutdown");
            } else {
                let unregistering = server.clone();
                tokio::spawn(async move {
                    assert!(unregistering.unregister_route("echo").await);
                });
            }
        }
    }
    gate.release();
    unary_gate.release();
    server
        .shutdown_and_wait(Duration::from_secs(10))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    emit(json!({"kind": "stopped"}));
}
