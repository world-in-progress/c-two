use std::error::Error;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use c_two::generated::{
    C2Error, EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, HeldResponse,
    MethodAccess, MethodDefinition,
};
use c_two::{
    Connect, ContractRelease, Error as CTwoError, Host, HostOptions, ResponseStream, Runtime,
    RuntimeOptions, ServiceDefinition,
};
use fastdb::{BuildPolicy, Builder, CompiledSpec, OpenOptions, Payload, PayloadError};
use serde_json::{Value, json};

const DESCRIPTOR: &[u8] = include_bytes!("../fixtures/stream.contract.json");
const SPEC: &[u8] = include_bytes!("../fixtures/stream-spec.json");
const LARGE_LEN: usize = 256 * 1024;

#[derive(Clone)]
struct StreamOutcome {
    count: usize,
    ending: String,
}

impl StreamOutcome {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let mut outcome = Self {
            count: 2,
            ending: "success".into(),
        };
        let mut args = args;
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--items" => outcome.count = args.next().ok_or("missing --items value")?.parse()?,
                "--ending" => {
                    outcome.ending = args.next().ok_or("missing --ending value")?;
                    assert!(matches!(
                        outcome.ending.as_str(),
                        "success" | "step" | "cleanup" | "cleanup-step"
                    ));
                }
                _ => return Err(format!("unknown stream option: {flag}").into()),
            }
        }
        Ok(outcome)
    }
}

fn stream_failure(message: &str) -> C2Error {
    C2Error::new(
        c_two::generated::ErrorCode::ResourceFunctionExecuting,
        message,
    )
}

fn cell(value: c_two::observe::MemoryCellStats) -> Value {
    json!({
        "limit_bytes": value.limit_bytes, "used_bytes": value.used_bytes,
        "peak_bytes": value.peak_bytes, "rejected_allocations": value.rejected_allocations,
        "rejected_bytes": value.rejected_bytes,
    })
}

fn scope(value: c_two::observe::MemoryScopeStats) -> Value {
    json!({"cells": {"shm": cell(value.shm), "file": cell(value.file),
                     "reassembly": cell(value.reassembly)}})
}

fn memory_stats(stats: c_two::observe::RuntimeMemoryStats) -> Value {
    json!({
        "runtime_outgoing": stats.runtime_outgoing.map(scope),
        "server": stats.server.map(scope),
        "payload_buffer": stats.payload_buffer.map(cell),
    })
}

fn value(large: bool) -> String {
    if large {
        "x".repeat(LARGE_LEN)
    } else {
        "stream-matrix".into()
    }
}

fn build_payload(large: bool) -> Result<Payload, PayloadError> {
    let spec = CompiledSpec::compile(SPEC)?;
    let mut builder = Builder::create(&spec)?;
    builder.entry_begin(0, 1)?.value_str(&value(large))?;
    Ok(builder
        .freeze()?
        .execute(BuildPolicy::AllowStaging)?
        .payload)
}

fn inspect(payload: &Payload, large: bool) -> Result<(), PayloadError> {
    let actual = payload.entry_view(0)?.at(0)?.acquire()?.str()?.to_owned();
    assert_eq!(actual, value(large));
    Ok(())
}

fn decode_owned(response: HeldResponse) -> Result<Payload, CTwoError> {
    let spec = CompiledSpec::compile(SPEC).map_err(payload_error)?;
    Payload::open_copy(&spec, response.bytes(), &OpenOptions::default()).map_err(payload_error)
}

fn decode_held(response: HeldResponse) -> Result<c_two::Held<Payload>, CTwoError> {
    let spec = CompiledSpec::compile(SPEC).map_err(payload_error)?;
    c_two::generated::open_held(&spec, response).map_err(payload_error)
}

fn payload_error(error: PayloadError) -> CTwoError {
    CTwoError::Semantic(c_two::generated::fastdb_adapter_error(
        c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,
        error,
    ))
}

struct Resource {
    calls: Arc<AtomicUsize>,
    large: bool,
    host: Host,
    input_stats: Arc<Mutex<Option<Value>>>,
    gated: bool,
    gate: Arc<(Mutex<bool>, Condvar)>,
    outcome: StreamOutcome,
}

impl EncodedService for Resource {
    fn invoke(&self, method: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(method, 0);
        assert!(request.is_empty());
        Ok(Vec::new())
    }
}

impl EncodedStreamService for Resource {
    fn open_stream(
        &self,
        method: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn EncodedStream>, C2Error> {
        assert_eq!(method, 1);
        let spec = CompiledSpec::compile(SPEC).unwrap();
        let input = Payload::open_copy(&spec, &request, &OpenOptions::default()).unwrap();
        inspect(&input, self.large).unwrap();
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        *self.input_stats.lock().unwrap() = Some(scope(self.host.memory_stats().server.unwrap()));
        if self.gated {
            println!("CALL calls={calls}");
            std::io::stdout().flush().unwrap();
        }
        let bytes = build_payload(self.large).unwrap().binary_bytes().unwrap();
        Ok(Box::new(MatrixItems {
            bytes,
            position: 0,
            gated: self.gated,
            gate: self.gate.clone(),
            outcome: self.outcome.clone(),
        }))
    }
}

struct Item(Vec<u8>);
impl EncodedStreamItem for Item {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        destination.copy_from_slice(&self.0[offset..offset + destination.len()]);
        Ok(())
    }
}

struct MatrixItems {
    bytes: Vec<u8>,
    position: usize,
    gated: bool,
    gate: Arc<(Mutex<bool>, Condvar)>,
    outcome: StreamOutcome,
}
impl EncodedStream for MatrixItems {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        if self.position == self.outcome.count {
            return if matches!(self.outcome.ending.as_str(), "step" | "cleanup-step") {
                Err(stream_failure("stream matrix step failed"))
            } else {
                Ok(None)
            };
        }
        if self.gated && self.position == 1 {
            println!("STEP_BLOCKED");
            std::io::stdout().flush().unwrap();
            let (lock, condition) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
        }
        self.position += 1;
        Ok(Some(Box::new(Item(self.bytes.clone()))))
    }
    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        if self.outcome.ending.starts_with("cleanup") {
            Err(stream_failure("stream matrix cleanup failed"))
        } else {
            Ok(())
        }
    }
}

fn runtime(server_id: Option<String>, shm: bool) -> Result<Runtime, c_two::LifecycleError> {
    Runtime::new(RuntimeOptions {
        server_id,
        shm_threshold: Some(if shm { 64 * 1024 } else { 1 << 30 }),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
}

fn host(
    address: &str,
    route: &str,
    shm: bool,
    large: bool,
    gated: bool,
    outcome: StreamOutcome,
) -> Result<(), Box<dyn Error>> {
    let server_id = address
        .strip_prefix("ipc://")
        .ok_or("expected IPC address")?;
    let runtime = runtime(Some(server_id.into()), shm)?;
    let host = runtime.host(HostOptions::default().without_relay())?;
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let input_stats = Arc::new(Mutex::new(None));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let definition = ServiceDefinition::new_stream(
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
        Arc::new(Resource {
            calls: Arc::clone(&calls),
            large,
            host: host.clone(),
            input_stats: input_stats.clone(),
            gated,
            gate: gate.clone(),
            outcome,
        }),
    )?;
    let mut registration = host.register(definition)?;
    assert_eq!(runtime.server_address().as_deref(), Some(address));
    println!("READY address={address}");
    std::io::stdout().flush()?;
    loop {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "stats" {
            break;
        }
        println!(
            "{}",
            json!({"memory": memory_stats(host.memory_stats()), "input": *input_stats.lock().unwrap()})
        );
        std::io::stdout().flush()?;
    }
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    registration.close()?;
    println!(
        "{}",
        json!({"memory": memory_stats(host.memory_stats()), "input": *input_stats.lock().unwrap()})
    );
    println!("HOST_RECEIPT calls={}", calls.load(Ordering::SeqCst));
    Ok(())
}

fn client(
    address: &str,
    route: &str,
    shm: bool,
    large: bool,
    held: bool,
) -> Result<(), Box<dyn Error>> {
    let runtime = runtime(None, shm)?;
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR)?;
    let connection = runtime.connect(
        release.expected_route(route)?,
        Connect::DirectIpc {
            address: address.into(),
        },
    )?;
    assert_eq!(
        connection.observed_path(),
        c_two::observe::ObservedPath::DirectIpc
    );
    let input = build_payload(large)?;
    let encoded_len = input.binary_bytes()?.len();
    let mut call = connection.begin_stream("echo")?;
    c_two::generated::charge_payload_input(&mut call, &input)?;
    let core = call.encode_vec(input.binary_bytes()?)?.stream()?;
    if held {
        let mut stream = ResponseStream::from_core(core, decode_held);
        for _ in 0..2 {
            let mut item = stream.next().ok_or("missing held stream item")??;
            assert_eq!(item.value().unwrap().binary_bytes()?.len(), encoded_len);
            inspect(item.value().ok_or("released before inspection")?, large)?;
            item.release()?;
            assert!(item.is_released());
        }
        assert!(stream.next().is_none());
        let receipt = stream.close();
        assert!(receipt.completed && receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(2));
    } else {
        let mut stream = ResponseStream::from_core(core, decode_owned);
        for _ in 0..2 {
            let item = stream.next().ok_or("missing materialized stream item")??;
            assert_eq!(item.binary_bytes()?.len(), encoded_len);
            inspect(&item, large)?;
        }
        assert!(stream.next().is_none());
        let receipt = stream.close();
        assert!(receipt.completed && receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(2));
    }
    drop(
        connection
            .begin_call("ping")?
            .encode_vec(Vec::new())?
            .call_owned()?,
    );
    assert_eq!(runtime.call_execution_snapshot()?.used_operations, 0);
    let stats = memory_stats(runtime.memory_stats());
    assert_eq!(stats["payload_buffer"]["used_bytes"], 0);
    println!(
        "{}",
        json!({"items": 2, "path": "direct_ipc", "memory": stats,
                         "encoded_len": encoded_len})
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    if mode == "relay-host" {
        return relay::host(args.collect());
    }
    if mode == "relay-client" {
        return relay::client(args.collect());
    }
    let address = args.next().ok_or("missing address")?;
    let route = args.next().ok_or("missing route")?;
    let shm = args.next().ok_or("missing shm")? == "shm";
    let large = args.next().ok_or("missing size")? == "large";
    let mut args = args.peekable();
    let held = if matches!(
        args.peek().map(String::as_str),
        Some("hold" | "materialized")
    ) {
        args.next().unwrap() == "hold"
    } else {
        false
    };
    let outcome = StreamOutcome::parse(args)?;
    match mode.as_str() {
        "host" => host(&address, &route, shm, large, false, outcome),
        "gated-host" => host(&address, &route, shm, large, true, outcome),
        "client" => client(&address, &route, shm, large, held),
        _ => Err("unknown mode".into()),
    }
}

// Additional one-hop relay modes share the portable contract with the direct
// matrix above. They do not change its input, output or process protocol.
mod relay {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const ITEM_BYTES: usize = 128 * 1024;

    fn payload(text: &str) -> Result<Payload, PayloadError> {
        let spec = CompiledSpec::compile(SPEC)?;
        let mut builder = Builder::create(&spec)?;
        builder.entry_begin(0, 1)?.value_str(text)?;
        Ok(builder
            .freeze()?
            .execute(BuildPolicy::AllowStaging)?
            .payload)
    }

    fn item(index: usize) -> Vec<u8> {
        let overhead = payload("").unwrap().binary_bytes().unwrap().len();
        let text = format!("{}{:012}", "x".repeat(ITEM_BYTES - overhead - 12), index);
        let bytes = payload(&text).unwrap().binary_bytes().unwrap();
        assert_eq!(bytes.len(), ITEM_BYTES);
        bytes
    }

    fn inspect_item(value: &Payload, index: usize) {
        assert_eq!(value.binary_bytes().unwrap().len(), ITEM_BYTES);
        let actual = value
            .entry_view(0)
            .unwrap()
            .at(0)
            .unwrap()
            .acquire()
            .unwrap();
        let text = actual.str().unwrap();
        assert!(text.ends_with(&format!("{index:012}")));
        assert!(text[..text.len() - 12].bytes().all(|byte| byte == b'x'));
    }

    #[derive(Default)]
    struct Counts {
        steps: usize,
        produced: usize,
        cleanups: usize,
    }

    impl Counts {
        fn json(&self) -> Value {
            json!({"steps": self.steps, "produced": self.produced, "cleanups": self.cleanups})
        }
    }

    struct RelayResource {
        counts: Arc<(Mutex<Counts>, Condvar)>,
        count: usize,
        ending: String,
    }

    impl EncodedService for RelayResource {
        fn invoke(&self, method: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
            assert_eq!(method, 0);
            assert!(request.is_empty());
            Ok(Vec::new())
        }
    }

    impl EncodedStreamService for RelayResource {
        fn open_stream(
            &self,
            method: u16,
            request: Vec<u8>,
        ) -> Result<Box<dyn EncodedStream>, C2Error> {
            assert_eq!(method, 1);
            let spec = CompiledSpec::compile(SPEC).unwrap();
            inspect(
                &Payload::open_copy(&spec, &request, &OpenOptions::default()).unwrap(),
                false,
            )
            .unwrap();
            Ok(Box::new(RelayItems {
                counts: self.counts.clone(),
                count: self.count,
                position: 0,
                ending: self.ending.clone(),
            }))
        }
    }

    struct RelayItems {
        counts: Arc<(Mutex<Counts>, Condvar)>,
        count: usize,
        position: usize,
        ending: String,
    }

    impl EncodedStream for RelayItems {
        fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
            {
                let mut counts = self.counts.0.lock().unwrap();
                counts.steps += 1;
                self.counts.1.notify_all();
            }
            if self.position == self.count {
                return if self.ending == "step" || self.ending == "cleanup-step" {
                    Err(stream_failure("relay cross step failed"))
                } else {
                    Ok(None)
                };
            }
            let bytes = item(self.position);
            self.position += 1;
            let mut counts = self.counts.0.lock().unwrap();
            counts.produced += 1;
            self.counts.1.notify_all();
            Ok(Some(Box::new(Item(bytes))))
        }

        fn finish(self: Box<Self>) -> Result<(), C2Error> {
            let mut counts = self.counts.0.lock().unwrap();
            counts.cleanups += 1;
            self.counts.1.notify_all();
            if self.ending.starts_with("cleanup") {
                Err(stream_failure("relay cross cleanup failed"))
            } else {
                Ok(())
            }
        }
    }

    fn emit(value: Value) {
        println!("{value}");
        std::io::stdout().flush().unwrap();
    }

    fn command(expected: &str) {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line.trim(), expected);
    }

    pub fn host(args: Vec<String>) -> Result<(), Box<dyn Error>> {
        let address = &args[0];
        let route = &args[1];
        let runtime = runtime(Some(address.strip_prefix("ipc://").unwrap().into()), true)?;
        let host = runtime.host(HostOptions::default().with_relay_anchor_address(&args[2]))?;
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR)?;
        let outcome = StreamOutcome::parse(args[3..].iter().cloned())?;
        let counts = Arc::new((Mutex::new(Counts::default()), Condvar::new()));
        let definition = ServiceDefinition::new_stream(
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
            Arc::new(RelayResource {
                counts: counts.clone(),
                count: outcome.count,
                ending: outcome.ending,
            }),
        )?;
        let mut registration = host.register(definition)?;
        emit(json!({"event": "ready", "address": address}));
        loop {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            match line.trim() {
                "stats" => emit(counts.0.lock().unwrap().json()),
                "quiet" => {
                    // Observe an unchanged resource counter while the consumer
                    // is parked at its stdin barrier. No producer barrier can
                    // artificially cap the transport's measured run-ahead.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let mut current = counts.0.lock().unwrap();
                    loop {
                        let before = current.steps;
                        let (next, timeout) = counts
                            .1
                            .wait_timeout(current, Duration::from_secs(1))
                            .unwrap();
                        current = next;
                        assert!(
                            current.steps <= 65,
                            "relay did not stop within 64 run-ahead items"
                        );
                        if timeout.timed_out() && current.steps == before {
                            emit(current.json());
                            break;
                        }
                        assert!(Instant::now() < deadline, "resource did not become quiet");
                    }
                }
                "produced-three" => {
                    let counts = counts
                        .1
                        .wait_timeout_while(
                            counts.0.lock().unwrap(),
                            Duration::from_secs(10),
                            |value| value.produced < 3,
                        )
                        .unwrap()
                        .0;
                    assert!(counts.produced >= 3);
                    emit(counts.json());
                }
                "stop" => break,
                other => panic!("unknown relay host command: {other}"),
            }
        }
        registration.close()?;
        emit(counts.0.lock().unwrap().json());
        Ok(())
    }

    fn error_json(error: &CTwoError) -> Value {
        match error {
            CTwoError::Semantic(error) => {
                json!({"code": u16::from(error.code), "message": error.message, "details": error.details})
            }
            other => panic!("expected semantic resource error, got {other:?}"),
        }
    }

    pub fn client(args: Vec<String>) -> Result<(), Box<dyn Error>> {
        let runtime = runtime(None, true)?;
        let behavior = args[3].as_str();
        let budget = if behavior == "hold" {
            2 * ITEM_BYTES
        } else {
            3 * ITEM_BYTES
        };
        runtime.set_call_execution_limits(c_two::CallExecutionLimitsOverrides {
            payload_buffer_budget_bytes: Some(budget as u64),
            ..Default::default()
        })?;
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR)?;
        let connection = runtime.connect(
            release.expected_route(&args[1])?,
            Connect::ExplicitRelay {
                relay_url: args[0].clone(),
            },
        )?;
        assert_eq!(
            connection.observed_path(),
            c_two::observe::ObservedPath::ExplicitRelay
        );
        let input = build_payload(false)?;
        let mut call = connection.begin_stream("echo")?;
        c_two::generated::charge_payload_input(&mut call, &input)?;
        let core = call.encode_vec(input.binary_bytes()?)?.stream()?;
        let mut delivered = 0;
        let mut failure = Value::Null;
        let mut holds = Value::Null;
        let receipt;
        if behavior == "hold" {
            let mut stream = ResponseStream::from_core(core, decode_held);
            let mut first = stream.next().unwrap()?;
            let mut second = stream.next().unwrap()?;
            inspect_item(first.value().unwrap(), 0);
            inspect_item(second.value().unwrap(), 1);
            let full = memory_stats(runtime.memory_stats());
            assert_eq!(full["payload_buffer"]["used_bytes"], budget);
            let (started, ready) = mpsc::sync_channel(0);
            let (done, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                started.send(()).unwrap();
                let third = stream.next().unwrap();
                done.send((stream, third)).unwrap();
            });
            ready.recv()?;
            emit(json!({"event": "holds-full", "memory": full}));
            command("release");
            assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
            first.release()?;
            let (mut stream, third) = result.recv_timeout(Duration::from_secs(10))?;
            let mut third = third?;
            worker.join().unwrap();
            inspect_item(third.value().unwrap(), 2);
            inspect_item(second.value().unwrap(), 1);
            let after_one_release = memory_stats(runtime.memory_stats());
            assert_eq!(after_one_release["payload_buffer"]["used_bytes"], budget);
            second.release()?;
            third.release()?;
            assert!(stream.next().is_none());
            receipt = stream.close();
            assert_eq!(stream.close().complete_items, receipt.complete_items);
            delivered = 3;
            holds = json!({"full": full, "after_one_release": after_one_release});
        } else {
            let mut stream = ResponseStream::from_core(core, decode_owned);
            while let Some(item) = stream.next() {
                match item {
                    Ok(item) => {
                        inspect_item(&item, delivered);
                        delivered += 1;
                        if behavior == "slow" && delivered == 1 {
                            emit(json!({"event": "paused", "delivered": delivered}));
                            command("resume");
                        }
                    }
                    Err(error) => {
                        failure = error_json(&error);
                        break;
                    }
                }
            }
            receipt = stream.close();
            assert!(stream.next().is_none());
            assert_eq!(stream.close().complete_items, receipt.complete_items);
        }
        assert!(receipt.completed);
        let terminal = match receipt.terminal.as_ref().unwrap() {
            Ok(()) => Value::Null,
            Err(error) => error_json(error),
        };
        assert_eq!(failure, terminal);
        assert_eq!(delivered, args[2].parse::<usize>()?);
        assert_eq!(receipt.complete_items, Some(delivered as u64));
        let stats = memory_stats(runtime.memory_stats());
        assert_eq!(stats["payload_buffer"]["used_bytes"], 0);
        assert_eq!(runtime.call_execution_snapshot()?.used_operations, 0);
        emit(
            json!({"event": "done", "delivered": delivered, "complete_items": receipt.complete_items,
                    "error": terminal, "memory": stats, "holds": holds, "path": "explicit_relay"}),
        );
        Ok(())
    }
}
