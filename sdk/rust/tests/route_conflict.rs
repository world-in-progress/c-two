use std::collections::BTreeMap;

use c_two::{
    Error,
    generated::{C2Error, ErrorCode},
};

#[test]
fn semantic_route_conflict_keeps_canonical_identity_and_details() {
    let registrations =
        r#"[{"relay_id":"a","relay_url":"http://relay","registered_at":1,"relay_status":"Alive"}]"#;
    let error = Error::Semantic(
        C2Error::new(ErrorCode::RouteConflict, "duplicate online route").with_details(
            BTreeMap::from([
                ("route".into(), "grid".into()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
                ("registrations".into(), registrations.into()),
            ]),
        ),
    );
    let Error::Semantic(semantic) = error else {
        panic!("route conflict must remain semantic");
    };
    assert_eq!(semantic.code, ErrorCode::RouteConflict);
    assert_eq!(semantic.code.name(), "RouteConflict");
    assert_eq!(semantic.details["registrations"], registrations);
}
