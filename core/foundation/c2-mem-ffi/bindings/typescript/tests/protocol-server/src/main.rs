//! Real c2-server peer for the Node IPC 16 / ABI 8 gate. No Python or FastDB.
use c2_config::{ConfigResolver, ConfigSources, LocalEndpointOptions};
use c2_contract::ContractRelease;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, CrmStream, CrmStreamCallback, CrmStreamItem,
    ResponseMeta, RouteBuildSpec, SchedulerLimits, Server, ServerIpcConfig,
};
use serde_json::json;
use std::{
    collections::HashMap,
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
        request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if method != 0 {
            return Err(CrmError::InternalError("unexpected unary method".into()));
        }
        self.unary_dispatches.fetch_add(1, Ordering::Relaxed);
        if request == [255] || request == [254] {
            emit(json!({"kind": "unary_blocked"}));
            self.unary_gate.wait();
            if request == [254] {
                emit(json!({"kind": "unary_completed"}));
            }
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
                return Err(CrmError::InternalError(
                    "node fixture stream failure".into(),
                ));
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
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: (args[0] != "-").then(|| args[0].clone().into()),
        },
        ConfigSources::empty(),
    )
    .unwrap();
    let endpoint = context.endpoint(&args[1]).unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let descriptor = release.descriptor();
    let gate = Arc::new(Gate::default());
    let unary_gate = Arc::new(Gate::default());
    let unary_dispatches = Arc::new(AtomicUsize::new(0));
    let stream_dispatches = Arc::new(AtomicUsize::new(0));
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
                gate: gate.clone(),
                unary_gate: unary_gate.clone(),
                unary_dispatches: unary_dispatches.clone(),
                stream_dispatches: stream_dispatches.clone(),
            }),
            descriptor,
        )
        .unwrap();
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
