#[path = "fixtures/nested_error_phase.rs"]
mod fixture;

use c_two::Error;
use c_two::generated::ErrorCode;
use fixture::NestedErrorFixture;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../tests/fixtures/contracts/portable-no-payload.contract.json");

#[test]
fn nested_not_executed_phase_stops_at_the_resource_a_boundary() {
    let mut fixture = NestedErrorFixture::start(DESCRIPTOR, None);
    fixture.reject_b_before_callback();

    let Error::Semantic(inner) = fixture
        .call_b()
        .expect_err("B must refuse before its callback")
    else {
        panic!("B must return a semantic admission refusal");
    };
    assert_eq!(inner.code, ErrorCode::ResourceRemoved);
    assert_eq!(inner.details["route"], fixture.route_b);
    assert_eq!(inner.details["transport_phase"], "pre_dispatch");
    assert_eq!(inner.details["dispatch_phase"], "pre_dispatch");
    assert_eq!(fixture.counts(), (0, 0));

    let Error::Semantic(outer) = fixture.call_a().expect_err("A must propagate B's refusal") else {
        panic!("A's failure must remain semantic");
    };
    assert_eq!(
        fixture.counts(),
        (1, 0),
        "A ran while B never entered its callback"
    );
    assert_eq!(outer.code, inner.code);
    assert_eq!(outer.message, inner.message);
    assert_eq!(outer.details["route"], fixture.route_b);
    assert!(!outer.details.contains_key("route_uid"));
    assert!(!inner.details.contains_key("route_uid"));
    assert!(!outer.details.contains_key("transport_phase"));
    assert!(!outer.details.contains_key("dispatch_phase"));
}
