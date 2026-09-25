use c_two::generated::{HeldResponse, open_borrowed, open_held, open_owned};
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};

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
