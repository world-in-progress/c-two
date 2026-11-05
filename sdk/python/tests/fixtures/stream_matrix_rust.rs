use std::error::Error;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use c_two::generated::{
    C2Error, EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, HeldResponse,
    MethodAccess, MethodDefinition,
};
use c_two::{
    Connect, ContractRelease, Error as CTwoError, HostOptions, ResponseStream, Runtime,
    RuntimeOptions, ServiceDefinition,
};
use fastdb::{BuildPolicy, Builder, CompiledSpec, OpenOptions, Payload, PayloadError};

const DESCRIPTOR: &[u8] = include_bytes!("../fixtures/stream.contract.json");
const SPEC: &[u8] = include_bytes!("../fixtures/stream-spec.json");
const LARGE_LEN: usize = 256 * 1024;

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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = build_payload(self.large).unwrap().binary_bytes().unwrap();
        Ok(Box::new(TwoItems { bytes, position: 0 }))
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

struct TwoItems {
    bytes: Vec<u8>,
    position: usize,
}
impl EncodedStream for TwoItems {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        if self.position == 2 {
            return Ok(None);
        }
        self.position += 1;
        Ok(Some(Box::new(Item(self.bytes.clone()))))
    }
    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Ok(())
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

fn host(address: &str, route: &str, shm: bool, large: bool) -> Result<(), Box<dyn Error>> {
    let server_id = address
        .strip_prefix("ipc://")
        .ok_or("expected IPC address")?;
    let runtime = runtime(Some(server_id.into()), shm)?;
    let host = runtime.host(HostOptions::default().without_relay())?;
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR)?;
    let calls = Arc::new(AtomicUsize::new(0));
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
        }),
    )?;
    let mut registration = host.register(definition)?;
    assert_eq!(runtime.server_address().as_deref(), Some(address));
    println!(
        "READY address={address} route_uid={}",
        registration.outcome().route_uid
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    registration.close()?;
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
    let mut call = connection.begin_stream("echo")?;
    c_two::generated::charge_payload_input(&mut call, &input)?;
    let core = call.encode_vec(input.binary_bytes()?)?.stream()?;
    if held {
        let mut stream = ResponseStream::from_core(core, decode_held);
        for _ in 0..2 {
            let mut item = stream.next().ok_or("missing held stream item")??;
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
            inspect(&item, large)?;
        }
        assert!(stream.next().is_none());
        let receipt = stream.close();
        assert!(receipt.completed && receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(2));
    }
    let outgoing = runtime
        .memory_stats()
        .runtime_outgoing
        .ok_or("missing outgoing memory scope")?;
    assert_eq!(outgoing.reassembly.used_bytes, 0);
    if large && shm {
        assert!(outgoing.shm.peak_bytes > 0);
    }
    println!("CLIENT_RECEIPT items=2 path=direct_ipc");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    let address = args.next().ok_or("missing address")?;
    let route = args.next().ok_or("missing route")?;
    let shm = args.next().ok_or("missing shm")? == "shm";
    let large = args.next().ok_or("missing size")? == "large";
    let held = args.next().unwrap_or_default() == "hold";
    match mode.as_str() {
        "host" => host(&address, &route, shm, large),
        "client" => client(&address, &route, shm, large, held),
        _ => Err("unknown mode".into()),
    }
}
