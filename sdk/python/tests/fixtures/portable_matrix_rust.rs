use c_two::{Connect, Error as CTwoError, HostOptions, ObservedPath, Runtime, RuntimeOptions};
use fastdb::{
    BuildPolicy, Builder, CompiledSpec, GraphIdentity, Payload, PayloadError, View, ViewKind,
};
use std::error::Error;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[path = "../generated/no-payload/rust/rust/c_two_contract.rs"]
mod no_payload_contract;
#[path = "../generated/object-graph-v1/rust/rust/c_two_contract.rs"]
mod object_graph_contract;
#[path = "../generated/record-v1/rust/rust/c_two_contract.rs"]
mod record_contract;

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
    assert_eq!(root.field(9)?.acquire()?.str()?, "\u{feff}A\0B");
    assert_eq!(
        root.field(10)?.acquire()?.wstr()?,
        &[0xfeff, 0x0041, 0, 0xd83c, 0xdf0d, 0x03a9]
    );
    assert_eq!(root.field(11)?.acquire()?.bytes()?, &[0, 1, 0xff]);
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

fn inspect_owned_record(payload: &Payload) -> Result<(), PayloadError> {
    let (root, detached) = inspect_record_payload(payload)?;
    payload.invalidate()?;
    require_invalidated(&root)?;
    require_record_detached(&detached)
}

fn inspect_owned_graph(payload: &Payload) -> Result<(), PayloadError> {
    let (root, detached) = inspect_graph_payload(payload)?;
    payload.invalidate()?;
    require_invalidated(&root)?;
    require_graph_detached(&detached)
}

fn service_payload_error(error: PayloadError) -> CTwoError {
    CTwoError::Semantic(c_two::generated::fastdb_adapter_error(
        c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
        error,
    ))
}

struct NoPayloadHost {
    calls: Arc<AtomicUsize>,
}

impl no_payload_contract::Service for NoPayloadHost {
    fn method_0_ping(&self) -> Result<(), CTwoError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RecordHost {
    calls: Arc<AtomicUsize>,
}

impl record_contract::Service for RecordHost {
    fn method_0_roundtrip(&self, input: &Payload) -> Result<Payload, CTwoError> {
        let (_root, detached) = inspect_record_payload(input).map_err(service_payload_error)?;
        require_record_detached(&detached).map_err(service_payload_error)?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        build_record_payload().map_err(service_payload_error)
    }
}

struct ObjectGraphHost {
    calls: Arc<AtomicUsize>,
}

impl object_graph_contract::Service for ObjectGraphHost {
    fn method_0_roundtrip(&self, input: &Payload) -> Result<Payload, CTwoError> {
        let (_root, detached) = inspect_graph_payload(input).map_err(service_payload_error)?;
        require_graph_detached(&detached).map_err(service_payload_error)?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        build_graph_payload().map_err(service_payload_error)
    }
}

fn run_host(
    payload: &str,
    address: &str,
    route_name: &str,
    relay_url: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let server_id = address
        .strip_prefix("ipc://")
        .ok_or("Rust host address must use ipc://")?;
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id.to_string()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })?;
    let host_options = match relay_url {
        Some(relay_url) => HostOptions::default()
            .with_relay_anchor_address(relay_url)
            .with_relay_use_proxy(false),
        None => HostOptions::default().without_relay(),
    };
    let host = runtime.host(host_options)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let definition = match payload {
        "no-payload" => no_payload_contract::service_definition(
            route_name,
            NoPayloadHost {
                calls: Arc::clone(&calls),
            },
        )?,
        "record-v1" => record_contract::service_definition(
            route_name,
            RecordHost {
                calls: Arc::clone(&calls),
            },
        )?,
        "object-graph-v1" => object_graph_contract::service_definition(
            route_name,
            ObjectGraphHost {
                calls: Arc::clone(&calls),
            },
        )?,
        other => return Err(format!("unknown payload profile {other:?}").into()),
    };
    let mut registration = host.register(definition)?;
    let outcome = registration.outcome();
    let actual_address = runtime
        .server_address()
        .ok_or("missing Rust host address")?;
    if actual_address != address {
        return Err(format!("expected host address {address:?}, got {actual_address:?}").into());
    }
    println!(
        "READY address={} route_uid={} route_revision={}",
        actual_address, outcome.route_uid, outcome.route_revision
    );
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    registration.close()?;
    drop(host);

    println!("HOST_RECEIPT calls={}", calls.load(Ordering::SeqCst));
    Ok(())
}

fn run_client(
    payload: &str,
    transport: &str,
    endpoint: &str,
    route_name: &str,
) -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })?;
    let expected = match payload {
        "no-payload" => no_payload_contract::expected_route(route_name)?,
        "record-v1" => record_contract::expected_route(route_name)?,
        "object-graph-v1" => object_graph_contract::expected_route(route_name)?,
        other => return Err(format!("unknown payload profile {other:?}").into()),
    };
    let connect = match transport {
        "direct" => Connect::DirectIpc {
            address: endpoint.to_string(),
        },
        "relay" => Connect::ExplicitRelay {
            relay_url: endpoint.to_string(),
        },
        other => return Err(format!("unknown transport {other:?}").into()),
    };
    let core_client = runtime.connect(expected, connect)?;
    let observed_path = core_client.observed_path();
    let route_uid = core_client.observed_route().route_uid.clone();
    let route_revision = core_client.observed_route().route_revision;

    match payload {
        "no-payload" => {
            let client = no_payload_contract::ContractClient::new(core_client)?;
            client.method_0_ping()?;
        }
        "record-v1" => {
            let client = record_contract::ContractClient::new(core_client)?;
            let input = build_record_payload()?;
            let output = client.method_0_roundtrip(&input)?;
            inspect_owned_record(&output)?;
        }
        "object-graph-v1" => {
            let client = object_graph_contract::ContractClient::new(core_client)?;
            let input = build_graph_payload()?;
            let output = client.method_0_roundtrip(&input)?;
            inspect_owned_graph(&output)?;
        }
        _ => unreachable!("payload profile was validated above"),
    }

    let counters = runtime.path_counters();
    let selected = match observed_path {
        ObservedPath::DirectIpc => counters.direct_ipc(),
        ObservedPath::ExplicitRelay => counters.explicit_relay(),
        ObservedPath::RelayAwareLocalIpc => counters.relay_aware_local_ipc(),
        ObservedPath::RelayAwareRelay => counters.relay_aware_relay(),
    };
    if selected != 1 {
        return Err(format!("expected one selected path observation, got {selected}").into());
    }
    println!(
        "CLIENT_RECEIPT observed_path={observed_path:?} route_uid={route_uid} route_revision={route_revision} requests=1 responses=1"
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    let payload = args.next().ok_or("missing payload profile")?;
    let endpoint = args.next().ok_or("missing endpoint")?;
    let route_name = args.next().ok_or("missing route name")?;
    match mode.as_str() {
        "host" => {
            let relay = args.next();
            run_host(
                &payload,
                &endpoint,
                &route_name,
                relay.as_deref().filter(|value| *value != "-"),
            )
        }
        "client" => {
            let transport = args.next().ok_or("missing transport")?;
            run_client(&payload, &transport, &endpoint, &route_name)
        }
        other => Err(format!("unknown mode {other:?}").into()),
    }
}
