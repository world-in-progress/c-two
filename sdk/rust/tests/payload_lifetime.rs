use c_two::generated::{
    HeldResponse, open_borrowed, open_held, open_owned, release_held_after_error,
};
use c2_ipc::ResponseData;
use c2_mem::{MemPool, PoolConfig};
use c2_wire::chunk::backing::ReassemblyBacking;
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};
use parking_lot::RwLock;
use std::sync::Arc;

const VALUE_SPEC: &[u8] = br#"{
  "schema": "fastdb.payload.v1",
  "profile": "record.v1",
  "entries": [
    {
      "id": "value",
      "cardinality": "one",
      "type": {"kind": "u8", "nullable": false}
    }
  ],
  "components": []
}"#;

fn value_payload(value: u8) -> (CompiledSpec, Payload) {
    let spec = CompiledSpec::compile(VALUE_SPEC).expect("compile value spec");
    let mut builder = Builder::create(&spec).expect("create value builder");
    builder
        .entry_begin(0, 1)
        .expect("begin value entry")
        .value_u8(value)
        .expect("write value");
    let plan = builder.freeze().expect("freeze value plan");
    let payload = plan
        .execute(BuildPolicy::AllowStaging)
        .expect("execute value plan")
        .payload;
    (spec, payload)
}

fn payload_value(payload: &Payload) -> u8 {
    payload
        .entry_view(0)
        .expect("value entry")
        .at(0)
        .expect("one value")
        .get_u8()
        .expect("u8 value")
}

#[test]
fn owned_receive_opens_an_independent_copy() {
    let (spec, source) = value_payload(7);
    let bytes = source.binary_bytes().expect("portable bytes");
    let opened = open_owned(&spec, &bytes).expect("owned receive");
    drop(source);
    assert_eq!(payload_value(&opened), 7);
}

#[test]
fn held_release_invalidates_payload_views_then_clears_both_owners_idempotently() {
    let (spec, source) = value_payload(11);
    let response =
        HeldResponse::from_owned_bytes(source.binary_bytes().expect("portable response bytes"));
    let mut held = open_held(&spec, response).expect("held payload");
    let _: &c_two::Held<Payload> = &held;
    let payload_clone = held.value().expect("live payload").clone();
    let view = payload_clone
        .entry_view(0)
        .expect("entry")
        .at(0)
        .expect("value");
    let detached = view.materialize().expect("detached value");

    held.release().expect("first release");
    held.release().expect("idempotent release");

    assert!(held.is_released());
    assert!(held.value().is_none());
    assert_eq!(
        view.kind()
            .expect_err("checked view must be invalidated")
            .symbol(),
        "VIEW_INVALIDATED"
    );
    assert_eq!(
        payload_clone
            .entry_view(0)
            .expect_err("shared payload owner must be invalidated")
            .symbol(),
        "VIEW_INVALIDATED"
    );
    assert_eq!(detached.get_u8().expect("detached value remains valid"), 11);
}

#[test]
fn held_drop_runs_the_same_invalidation_path() {
    let (spec, source) = value_payload(13);
    let view = {
        let response =
            HeldResponse::from_owned_bytes(source.binary_bytes().expect("portable response bytes"));
        let held = open_held(&spec, response).expect("held payload");
        held.value()
            .expect("live payload")
            .entry_view(0)
            .expect("entry")
            .at(0)
            .expect("value")
    };

    assert_eq!(
        view.kind()
            .expect_err("drop must invalidate views")
            .symbol(),
        "VIEW_INVALIDATED"
    );
}

#[test]
fn held_open_failure_preserves_original_payload_error_after_transport_release() {
    let spec = CompiledSpec::compile(VALUE_SPEC).expect("compile value spec");
    let invalid_bytes = [0xa5; 512];
    let expected = open_owned(&spec, &invalid_bytes).expect_err("invalid payload");
    let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default()).unwrap()));
    let budget = pool.read().budget().unwrap().clone();
    let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, invalid_bytes.len()).unwrap();
    backing.write_at(0, &invalid_bytes).unwrap();
    let response = HeldResponse::from_response_data(ResponseData::Handle(backing)).unwrap();
    assert_eq!(pool.read().stats().alloc_count, 0);
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);

    let error = open_held(&spec, response).err().expect("invalid payload");
    assert_eq!(error.symbol(), expected.symbol());
    assert_eq!(error.message(), expected.message());
    drop(pool);
}

#[test]
fn held_local_value_survives_reassembly_storage_reuse_and_pool_destruction() {
    check_held_local_copy_after_reassembly_release(true);
}

#[test]
fn held_local_value_survives_file_spill_release_and_pool_drop() {
    check_held_local_copy_after_reassembly_release(false);
}

fn check_held_local_copy_after_reassembly_release(buddy: bool) {
    let (spec, source) = value_payload(19);
    let bytes = source.binary_bytes().unwrap();
    let mut pool = MemPool::new(PoolConfig {
        segment_size: 64 * 1024,
        spill_threshold: if buddy { 1.0 } else { 0.0 },
        ..PoolConfig::default()
    })
    .unwrap();
    if buddy {
        // Warm a real SHM slot without fallback, so an unavailable mapping is
        // reported with its original cause rather than passing as file spill.
        let seed = pool
            .alloc_shm_handle(bytes.len())
            .expect("buddy SHM required");
        pool.release_handle(seed);
    }
    let pool = Arc::new(RwLock::new(pool));
    let weak_pool = Arc::downgrade(&pool);
    let budget = pool.read().budget().unwrap().clone();
    let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, bytes.len()).unwrap();
    assert_eq!(backing.is_buddy(), buddy);
    assert_eq!(backing.is_file_spill(), !buddy);
    let spill_path = backing.file_spill_path();
    backing.write_at(0, &bytes).unwrap();
    assert_eq!(pool.read().stats().alloc_count, u32::from(buddy));
    let response = HeldResponse::from_response_data(ResponseData::Handle(backing)).unwrap();
    assert_eq!(pool.read().stats().alloc_count, 0);
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    if let Some(path) = spill_path {
        assert!(
            !path.exists(),
            "copied file backing must already be removed"
        );
    }
    assert_eq!(response.bytes(), bytes);
    let mut held = open_held(&spec, response).unwrap();
    let clone = held.value().unwrap().clone();
    let view = clone.entry_view(0).unwrap().at(0).unwrap();

    let mut replacement = ReassemblyBacking::admit(Arc::clone(&pool), 2, bytes.len()).unwrap();
    replacement.write_at(0, &vec![0xa5; bytes.len()]).unwrap();
    assert_eq!(view.get_u8().unwrap(), 19);
    drop(replacement);
    drop(pool);
    assert!(
        weak_pool.upgrade().is_none(),
        "hold must not retain the pool"
    );
    assert_eq!(payload_value(held.value().unwrap()), 19);
    assert_eq!(view.get_u8().unwrap(), 19);

    release_held_after_error(&mut held);

    assert!(held.is_released());
    assert!(held.value().is_none());
    assert_eq!(view.kind().unwrap_err().symbol(), "VIEW_INVALIDATED");
    assert_eq!(
        clone.entry_view(0).unwrap_err().symbol(),
        "VIEW_INVALIDATED"
    );
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
}

#[test]
fn borrowed_guard_invalidates_clones_and_views_when_callback_scope_ends() {
    let (spec, source) = value_payload(17);
    let bytes = source.binary_bytes().expect("portable request bytes");
    let guard = open_borrowed(&spec, &bytes).expect("borrowed callback payload");
    let shared = guard.payload().clone();
    let view = shared.entry_view(0).expect("entry").at(0).expect("value");
    assert_eq!(view.get_u8().expect("live callback value"), 17);

    drop(guard);

    assert_eq!(
        view.kind()
            .expect_err("callback exit must invalidate checked views")
            .symbol(),
        "VIEW_INVALIDATED"
    );
    assert_eq!(
        shared
            .entry_view(0)
            .expect_err("callback clone must share invalidation")
            .symbol(),
        "VIEW_INVALIDATED"
    );
}
