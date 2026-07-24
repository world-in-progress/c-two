use c_two::{Connect, Error as CTwoError, HostOptions, Runtime, RuntimeOptions};
use fastdb::{
    BuildPolicy, Builder, CompiledSpec, GraphIdentity, Payload, PayloadError, View, ViewKind,
};
use std::error::Error;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[path = "../generated/rust/c_two_contract.rs"]
mod contract;

const RECORD_SPEC: &[u8] = include_bytes!("../fixtures/record-all-types.source.json");
const GRAPH_SPEC: &[u8] = include_bytes!("../fixtures/graph-all-values.source.json");

fn build_record_payload() -> Result<Payload, PayloadError> {
    let spec = CompiledSpec::compile(RECORD_SPEC)?;
    let mut builder = Builder::create(&spec)?;
    builder
        .entry_begin(0, 1)?
        .value_component_begin()?
        .value_bool(true)?
        .value_u8(0xab)?
        .value_u16(0x1234)?
        .value_u32(0x89ab_cdef)?
        .value_i32(-42)?
        .value_u8n(0.0)?
        .value_u16n(1.0)?
        .value_f32_bits(0x3fc0_0000)?
        .value_f64_bits(0x4004_0000_0000_0000)?
        .value_str("\u{feff}A\0B")?
        .value_wstr(&[0xfeff, 0x0041, 0, 0xd83c, 0xdf0d, 0x03a9])?
        .value_bytes(&[0, 1, 0xff])?
        .value_component_begin()?
        .value_list_begin(3)?
        .value_list_begin(0)?
        .value_null()?
        .value_list_begin(3)?
        .value_str("")?
        .value_null()?
        .value_str("tail")?;
    builder
        .entry_begin(1, 4)?
        .value_null()?
        .value_list_begin(0)?
        .value_list_begin(3)?
        .value_u8(0)?
        .value_null()?
        .value_u8(0xff)?
        .value_list_begin(1)?
        .value_u8(7)?;
    Ok(builder
        .freeze()?
        .execute(BuildPolicy::AllowStaging)?
        .payload)
}

fn build_graph_payload() -> Result<Payload, PayloadError> {
    let spec = CompiledSpec::compile(GRAPH_SPEC)?;
    let node_index = spec.component_index("Node")?;
    let asset_index = spec.component_index("Asset")?;
    let mut builder = Builder::create(&spec)?;
    let node = builder.declare_object(node_index)?;
    let asset = builder.declare_object(asset_index)?;
    builder
        .object_fill_begin(node)?
        .value_bool(true)?
        .value_u8(0x12)?
        .value_u16(0x3456)?
        .value_u32(0x789a_bcde)?
        .value_i32(-1_234_567)?
        .value_u8n_bits(0x3fe0_0000_0000_0000)?
        .value_u16n_bits(0)?
        .value_f32_bits(0x7fa1_2345)?
        .value_f64_bits(0xfff8_0000_0000_1234)?
        .value_str("same")?
        .value_wstr(&[0x0041, 0xd83d, 0xde00])?
        .value_bytes(&[0x00, 0xff, 0x7e])?
        .value_component_begin()?
        .value_null()?
        .value_u16(0xbeef)?
        .value_list_begin(3)?
        .value_f32_bits(0x8000_0000)?
        .value_null()?
        .value_f32_bits(0xff80_0001)?
        .value_ref(node)?
        .value_ref(asset)?
        .object_fill_begin(asset)?
        .value_str("same")?
        .value_ref(node)?
        .entry_begin(0, 1)?
        .value_object(node)?
        .entry_begin(1, 1)?
        .value_object(asset)?
        .entry_begin(2, 2)?
        .value_ref(node)?
        .value_null()?
        .entry_begin(3, 3)?
        .value_u8n_bits(0)?
        .value_u8n_bits(0x3fe0_0000_0000_0000)?
        .value_u8n_bits(0x3ff0_0000_0000_0000)?
        .entry_begin(4, 3)?
        .value_u16n_bits(0xbff0_0000_0000_0000)?
        .value_u16n_bits(0)?
        .value_u16n_bits(0x3ff0_0000_0000_0000)?;
    Ok(builder
        .freeze()?
        .execute(BuildPolicy::AllowStaging)?
        .payload)
}

fn inspect_record_payload(payload: &Payload) -> Result<(View, View), PayloadError> {
    let sequence = payload.entry_view(0)?;
    assert_eq!(sequence.kind()?, ViewKind::Sequence);
    assert_eq!(sequence.length()?, 1);
    let root = sequence.at(0)?;
    assert_eq!(root.kind()?, ViewKind::Component);
    assert_eq!(root.field_count()?, 14);
    assert!(root.field(0)?.get_bool()?);
    assert_eq!(root.field(1)?.get_u8()?, 0xab);
    {
        let text = root.field(9)?;
        let access = text.acquire()?;
        assert_eq!(access.str()?, "\u{feff}A\0B");
    }
    {
        let wide = root.field(10)?;
        let access = wide.acquire()?;
        assert_eq!(access.wstr()?, &[0xfeff, 0x0041, 0, 0xd83c, 0xdf0d, 0x03a9]);
    }
    {
        let opaque = root.field(11)?;
        let access = opaque.acquire()?;
        assert_eq!(access.bytes()?, &[0, 1, 0xff]);
    }
    let nested = root.field(13)?;
    assert_eq!(nested.kind()?, ViewKind::List);
    assert_eq!(nested.length()?, 3);
    assert_eq!(nested.at(0)?.length()?, 0);
    assert!(nested.at(1)?.is_null()?);
    let present = nested.at(2)?;
    assert_eq!(present.length()?, 3);
    assert_eq!(present.at(0)?.acquire()?.str()?, "");
    assert!(present.at(1)?.is_null()?);
    assert_eq!(present.at(2)?.acquire()?.str()?, "tail");
    let series = payload.entry_view(1)?;
    assert_eq!(series.length()?, 4);
    assert!(series.at(0)?.is_null()?);
    assert_eq!(series.at(1)?.length()?, 0);
    assert_eq!(series.at(2)?.at(0)?.get_u8()?, 0);
    assert!(series.at(2)?.at(1)?.is_null()?);
    assert_eq!(series.at(2)?.at(2)?.get_u8()?, 0xff);
    assert_eq!(series.at(3)?.at(0)?.get_u8()?, 7);
    let detached = root.materialize()?;
    Ok((root, detached))
}

fn inspect_graph_payload(payload: &Payload) -> Result<(View, View), PayloadError> {
    let spec = CompiledSpec::compile(GRAPH_SPEC)?;
    let node_index = spec.component_index("Node")?;
    let asset_index = spec.component_index("Asset")?;
    let root_identity = GraphIdentity {
        component_index: node_index,
        object_id: 0,
    };
    let asset_identity = GraphIdentity {
        component_index: asset_index,
        object_id: 0,
    };
    let root = payload.entry_view(0)?.at(0)?;
    assert_eq!(root.graph_identity()?, root_identity);
    assert_eq!(root.field(9)?.acquire()?.str()?, "same");
    assert_eq!(
        root.field(10)?.acquire()?.wstr()?,
        &[0x0041, 0xd83d, 0xde00]
    );
    assert_eq!(root.field(11)?.acquire()?.bytes()?, &[0x00, 0xff, 0x7e]);
    let values = root.field(13)?;
    assert_eq!(values.length()?, 3);
    assert!(values.at(1)?.is_null()?);
    let self_ref = root.field(14)?;
    assert_eq!(self_ref.kind()?, ViewKind::Ref);
    assert_eq!(self_ref.graph_identity()?, root_identity);
    assert_eq!(self_ref.ref_target()?.graph_identity()?, root_identity);
    let asset_ref = root.field(15)?;
    assert_eq!(asset_ref.graph_identity()?, asset_identity);
    let asset = asset_ref.ref_target()?;
    let owner_ref = asset.field(1)?;
    assert_eq!(owner_ref.graph_identity()?, root_identity);
    assert_eq!(owner_ref.ref_target()?.graph_identity()?, root_identity);
    let refs = payload.entry_view(2)?;
    assert_eq!(refs.at(0)?.graph_identity()?, root_identity);
    assert_eq!(refs.at(0)?.ref_target()?.graph_identity()?, root_identity);
    assert!(refs.at(1)?.is_null()?);
    let detached = root.materialize()?;
    Ok((root, detached))
}

fn require_invalidated(view: &View) -> Result<(), PayloadError> {
    let error = view
        .kind()
        .expect_err("FastDB view remained usable after owner invalidation");
    assert_eq!(error.symbol(), "VIEW_INVALIDATED");
    assert_eq!(error.path(), "/view");
    Ok(())
}

fn require_record_detached(detached: &View) -> Result<(), PayloadError> {
    assert_eq!(detached.field(1)?.get_u8()?, 0xab);
    assert_eq!(detached.field(9)?.acquire()?.str()?, "\u{feff}A\0B");
    Ok(())
}

fn require_graph_detached(detached: &View) -> Result<(), PayloadError> {
    let root_identity = detached.graph_identity()?;
    let self_ref = detached.field(14)?;
    assert_eq!(self_ref.graph_identity()?, root_identity);
    assert_eq!(self_ref.ref_target()?.graph_identity()?, root_identity);
    let asset = detached.field(15)?.ref_target()?;
    assert_eq!(
        asset.field(1)?.ref_target()?.graph_identity()?,
        root_identity
    );
    Ok(())
}

fn inspect_materialize_invalidate_record(payload: &Payload) -> Result<(), PayloadError> {
    let (root, detached) = inspect_record_payload(payload)?;
    payload.invalidate()?;
    require_invalidated(&root)?;
    require_record_detached(&detached)
}

fn inspect_materialize_invalidate_graph(payload: &Payload) -> Result<(), PayloadError> {
    let (root, detached) = inspect_graph_payload(payload)?;
    payload.invalidate()?;
    require_invalidated(&root)?;
    require_graph_detached(&detached)
}

fn inspect_borrowed_record(payload: &Payload) -> Result<(), PayloadError> {
    let (_root, detached) = inspect_record_payload(payload)?;
    require_record_detached(&detached)
}

fn inspect_borrowed_graph(payload: &Payload) -> Result<(), PayloadError> {
    let (_root, detached) = inspect_graph_payload(payload)?;
    require_graph_detached(&detached)
}

fn service_payload_error(error: PayloadError) -> CTwoError {
    CTwoError::Semantic(c_two::generated::fastdb_adapter_error(
        c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
        error,
    ))
}

#[derive(Default)]
struct CallCounts {
    graph: usize,
    ping: usize,
    record: usize,
}

struct PortableHost {
    counts: Arc<Mutex<CallCounts>>,
}

impl contract::Service for PortableHost {
    fn method_0_graph_roundtrip(&self, input: &Payload) -> Result<Payload, CTwoError> {
        inspect_borrowed_graph(input).map_err(service_payload_error)?;
        let output = build_graph_payload().map_err(service_payload_error)?;
        self.counts.lock().expect("call counts").graph += 1;
        Ok(output)
    }

    fn method_1_ping(&self) -> Result<(), CTwoError> {
        self.counts.lock().expect("call counts").ping += 1;
        Ok(())
    }

    fn method_2_record_roundtrip(&self, input: &Payload) -> Result<Payload, CTwoError> {
        inspect_borrowed_record(input).map_err(service_payload_error)?;
        let output = build_record_payload().map_err(service_payload_error)?;
        self.counts.lock().expect("call counts").record += 1;
        Ok(output)
    }
}

fn run_host(address: &str, route_name: &str) -> Result<(), Box<dyn Error>> {
    let server_id = address
        .strip_prefix("ipc://")
        .ok_or("Rust host address must use ipc://")?;
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id.to_string()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })?;
    let host = runtime.host(HostOptions::default().without_relay())?;
    let counts = Arc::new(Mutex::new(CallCounts::default()));
    let mut registration = host.register(contract::service_definition(
        route_name,
        PortableHost {
            counts: Arc::clone(&counts),
        },
    )?)?;
    let actual_address = runtime
        .server_address()
        .ok_or("missing Rust host address")?;
    if actual_address != address {
        return Err(format!("expected host address {address:?}, got {actual_address:?}").into());
    }
    println!("READY {actual_address}");
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    registration.close()?;
    drop(host);

    let counts = counts.lock().expect("call counts");
    println!(
        "RUST_HOST_RECEIPT {{\"graph\":{},\"ping\":{},\"record\":{}}}",
        counts.graph, counts.ping, counts.record
    );
    println!("OK rust-host");
    Ok(())
}

fn run_client(address: &str, route_name: &str, label: &str) -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })?;
    let mut wrong = contract::expected_route(route_name)?;
    wrong.abi_hash = "0".repeat(64);
    match runtime.connect(
        wrong,
        Connect::DirectIpc {
            address: address.to_string(),
        },
    ) {
        Err(CTwoError::Semantic(error))
            if error.code == c_two::generated::ErrorCode::ContractMismatch => {}
        Err(error) => return Err(format!("wrong route failed through wrong owner: {error}").into()),
        Ok(_) => return Err("wrong route contract unexpectedly acquired".into()),
    }

    let core_client = runtime.connect(
        contract::expected_route(route_name)?,
        Connect::DirectIpc {
            address: address.to_string(),
        },
    )?;
    let client = contract::ContractClient::new(core_client)?;
    let graph = build_graph_payload()?;
    let graph_response = client.method_0_graph_roundtrip(&graph)?;
    inspect_materialize_invalidate_graph(&graph_response)?;

    client.method_1_ping()?;

    let record = build_record_payload()?;
    let record_response = client.method_2_record_roundtrip(&record)?;
    inspect_materialize_invalidate_record(&record_response)?;

    match client.method_2_record_roundtrip(&graph) {
        Err(CTwoError::Semantic(error)) => {
            assert_eq!(
                error.code,
                c_two::generated::ErrorCode::ClientInputSerializing
            );
            assert_eq!(
                error.details.get("fastdb_symbol").map(String::as_str),
                Some("DIGEST_MISMATCH")
            );
            assert_eq!(
                error.details.get("fastdb_path").map(String::as_str),
                Some("/payload/spec_sha256")
            );
        }
        Err(error) => {
            return Err(format!("digest mismatch failed through wrong owner: {error}").into());
        }
        Ok(_) => return Err("wrong FastDB payload digest unexpectedly succeeded".into()),
    }
    println!(
        "RUST_CLIENT_RECEIPT {{\"label\":{label:?},\"graph\":1,\"ping\":1,\"record\":1,\"route_mismatch\":1,\"digest_mismatch\":1}}"
    );
    println!("OK rust-client {label}");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    let address = args.next().ok_or("missing IPC address")?;
    let route_name = args.next().ok_or("missing route name")?;
    match mode.as_str() {
        "host" => run_host(&address, &route_name),
        "client" => {
            let label = args.next().ok_or("missing client label")?;
            run_client(&address, &route_name, &label)
        }
        other => Err(format!("unknown mode {other:?}").into()),
    }
}
