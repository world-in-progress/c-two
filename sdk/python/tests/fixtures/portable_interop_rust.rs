use c2_config::{BaseIpcConfig, ClientIpcConfig, ServerIpcConfig};
use c2_ipc::{IpcError, SyncClient};
use c2_mem::{MemPool, PoolConfig};
use c2_server::{
    AccessLevel, ConcurrencyMode, CrmCallback, CrmError, RequestData, ResponseMeta,
    RouteBuildSpec, SchedulerLimits, Server,
};
use fastdb::{
    BuildPolicy, Builder, CompiledSpec, GraphIdentity, Payload, PayloadError, View, ViewKind,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

#[path = "../generated/rust/c_two_contract.rs"]
mod contract;

const RECORD_SPEC: &[u8] = include_bytes!("../fixtures/record-all-types.source.json");
const GRAPH_SPEC: &[u8] = include_bytes!("../fixtures/graph-all-values.source.json");

fn small_base_config() -> BaseIpcConfig {
    BaseIpcConfig {
        pool_segment_size: 1024 * 1024,
        max_pool_segments: 1,
        max_pool_memory: 1024 * 1024,
        reassembly_segment_size: 1024 * 1024,
        reassembly_max_segments: 1,
        max_total_chunks: 32,
        chunk_gc_interval_secs: 1.0,
        chunk_threshold_ratio: 0.9,
        chunk_assembler_timeout_secs: 10.0,
        max_reassembly_bytes: 16 * 1024 * 1024,
        chunk_size: 64 * 1024,
        ..BaseIpcConfig::default()
    }
}

fn client_config() -> ClientIpcConfig {
    ClientIpcConfig {
        base: small_base_config(),
        shm_threshold: 1,
    }
}

fn server_config() -> ServerIpcConfig {
    ServerIpcConfig {
        base: small_base_config(),
        shm_threshold: 1,
        max_frame_size: 16 * 1024 * 1024,
        max_payload_size: 16 * 1024 * 1024,
        max_pending_requests: 32,
        max_execution_workers: 4,
        pool_decay_seconds: 1.0,
        heartbeat_interval_secs: 0.0,
        heartbeat_timeout_secs: 5.0,
    }
}

fn client_pool() -> Arc<Mutex<MemPool>> {
    Arc::new(Mutex::new(MemPool::new(PoolConfig {
        segment_size: 1024 * 1024,
        min_block_size: 4096,
        max_segments: 1,
        max_dedicated_segments: 2,
        dedicated_crash_timeout_secs: 5.0,
        buddy_idle_decay_secs: 1.0,
        spill_threshold: 0.8,
        spill_dir: std::env::temp_dir().join("c_two_portable_interop_spill"),
    })))
}

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
    Ok(builder.freeze()?.execute(BuildPolicy::AllowStaging)?.payload)
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
    Ok(builder.freeze()?.execute(BuildPolicy::AllowStaging)?.payload)
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
        assert_eq!(
            access.wstr()?,
            &[0xfeff, 0x0041, 0, 0xd83c, 0xdf0d, 0x03a9]
        );
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
    assert_eq!(
        detached.field(9)?.acquire()?.str()?,
        "\u{feff}A\0B"
    );
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

fn crm_internal(context: &str, error: impl std::fmt::Display) -> CrmError {
    CrmError::InternalError(format!("{context}: {error}"))
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

impl CrmCallback for PortableHost {
    fn invoke(
        &self,
        _route_name: &str,
        method_idx: u16,
        request: RequestData,
        _response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        let bytes = c2_server::RequestLease::new(request)
            .into_owned_bytes()
            .map_err(|error| crm_internal("materialize request", error))?;
        let method = contract::METHODS
            .get(usize::from(method_idx))
            .ok_or_else(|| crm_internal("dispatch", format!("unknown method index {method_idx}")))?;
        match method.name {
            "graph_roundtrip" => {
                let input = contract::decode_method_0_graph_roundtrip_input(&bytes)
                    .map_err(|error| crm_internal("decode graph input", error))?;
                inspect_materialize_invalidate_graph(&input)
                    .map_err(|error| crm_internal("inspect graph input", error))?;
                let output = build_graph_payload()
                    .map_err(|error| crm_internal("build graph output", error))?;
                let encoded = contract::encode_method_0_graph_roundtrip_output(&output)
                    .map_err(|error| crm_internal("encode graph output", error))?;
                self.counts.lock().graph += 1;
                Ok(ResponseMeta::Inline(encoded))
            }
            "ping" => {
                contract::decode_method_1_ping_input(&bytes)
                    .map_err(|error| crm_internal("decode ping input", error))?;
                assert!(contract::encode_method_1_ping_output().is_empty());
                self.counts.lock().ping += 1;
                Ok(ResponseMeta::Empty)
            }
            "record_roundtrip" => {
                let input = contract::decode_method_2_record_roundtrip_input(&bytes)
                    .map_err(|error| crm_internal("decode record input", error))?;
                inspect_materialize_invalidate_record(&input)
                    .map_err(|error| crm_internal("inspect record input", error))?;
                let output = build_record_payload()
                    .map_err(|error| crm_internal("build record output", error))?;
                let encoded = contract::encode_method_2_record_roundtrip_output(&output)
                    .map_err(|error| crm_internal("encode record output", error))?;
                self.counts.lock().record += 1;
                Ok(ResponseMeta::Inline(encoded))
            }
            name => Err(crm_internal("dispatch", format!("unknown method {name:?}"))),
        }
    }
}

async fn register_route(
    server: &Server,
    route_name: &str,
    callback: Arc<dyn CrmCallback>,
) -> Result<(), Box<dyn Error>> {
    let mut access_map = HashMap::new();
    for method in contract::METHODS {
        access_map.insert(
            u16::try_from(method.index)?,
            match method.access {
                "read" => AccessLevel::Read,
                "write" => AccessLevel::Write,
                other => return Err(format!("unsupported generated access {other:?}").into()),
            },
        );
    }
    let built = server.build_route(
        RouteBuildSpec {
            name: route_name.to_string(),
            crm_ns: contract::CRM_NAMESPACE.to_string(),
            crm_name: contract::CRM_NAME.to_string(),
            crm_ver: contract::CRM_VERSION.to_string(),
            abi_hash: contract::ABI_HASH.to_string(),
            signature_hash: contract::SIGNATURE_HASH.to_string(),
            method_names: contract::METHODS
                .iter()
                .map(|method| method.name.to_string())
                .collect(),
            access_map,
            concurrency_mode: ConcurrencyMode::ReadParallel,
            limits: SchedulerLimits::default(),
        },
        callback,
    )?;
    let reservation = server.reserve_route(built).await?;
    server.commit_reserved_route(reservation).await?;
    Ok(())
}

async fn run_host(address: &str, route_name: &str) -> Result<(), Box<dyn Error>> {
    let counts = Arc::new(Mutex::new(CallCounts::default()));
    let server = Arc::new(Server::new(address, server_config())?);
    register_route(
        &server,
        route_name,
        Arc::new(PortableHost {
            counts: Arc::clone(&counts),
        }),
    )
    .await?;
    server.begin_start_attempt()?;
    let running = Arc::clone(&server);
    let run_task = tokio::spawn(async move { running.run().await });
    server.wait_until_ready(Duration::from_secs(10)).await?;
    println!("READY {address}");
    std::io::stdout().flush()?;

    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)
    })
    .await??;
    server.shutdown_and_wait(Duration::from_secs(10)).await?;
    run_task.await??;

    let counts = counts.lock();
    println!(
        "RUST_HOST_RECEIPT {{\"graph\":{},\"ping\":{},\"record\":{}}}",
        counts.graph, counts.ping, counts.record
    );
    println!("OK rust-host");
    Ok(())
}

fn run_client(address: &str, route_name: &str, label: &str) -> Result<(), Box<dyn Error>> {
    let mut transport = SyncClient::connect(address, Some(client_pool()), client_config())?;
    let mut wrong = contract::expected_route(route_name)?;
    wrong.abi_hash = "0".repeat(64);
    match transport.acquire_route(&wrong) {
        Err(IpcError::ContractMismatch(_)) => {}
        Err(error) => return Err(format!("wrong route failed through wrong owner: {error}").into()),
        Ok(_) => return Err("wrong route contract unexpectedly acquired".into()),
    }

    {
        let client = contract::ContractClient::acquire(&transport, route_name)?;
        let graph = build_graph_payload()?;
        let graph_response = client.method_0_graph_roundtrip(&graph)?;
        inspect_materialize_invalidate_graph(&graph_response)?;

        client.method_1_ping()?;

        let record = build_record_payload()?;
        let record_response = client.method_2_record_roundtrip(&record)?;
        inspect_materialize_invalidate_record(&record_response)?;

        match client.method_2_record_roundtrip(&graph) {
            Err(contract::ContractCallError::Payload(error)) => {
                assert_eq!(error.symbol(), "DIGEST_MISMATCH");
                assert_eq!(error.path(), "/payload/spec_sha256");
            }
            Err(error) => {
                return Err(format!("digest mismatch failed through wrong owner: {error}").into());
            }
            Ok(_) => return Err("wrong FastDB payload digest unexpectedly succeeded".into()),
        }
    }
    transport.close();
    println!("RUST_CLIENT_RECEIPT {{\"label\":{label:?},\"graph\":1,\"ping\":1,\"record\":1,\"route_mismatch\":1,\"digest_mismatch\":1}}");
    println!("OK rust-client {label}");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("missing mode")?;
    let address = args.next().ok_or("missing IPC address")?;
    let route_name = args.next().ok_or("missing route name")?;
    match mode.as_str() {
        "host" => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?
            .block_on(run_host(&address, &route_name)),
        "client" => {
            let label = args.next().ok_or("missing client label")?;
            run_client(&address, &route_name, &label)
        }
        other => Err(format!("unknown mode {other:?}").into()),
    }
}
