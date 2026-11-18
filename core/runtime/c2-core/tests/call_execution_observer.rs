//! Public snapshots observe role domains without activating them.
use c2_contract::ContractRelease;
use c2_core::{
    ClientConfigPatch, ConfigField, Connect, DomainLimitsPatch, RoleCallExecutionStats, Runtime,
    RuntimeOptions,
};

fn attempt_missing_connection(runtime: &Runtime) {
    let release = ContractRelease::from_descriptor_json(include_bytes!(
        "../../../../tests/fixtures/contracts/portable-release.contract.json"
    ))
    .unwrap();
    assert!(
        runtime
            .connect(
                release.expected_route("missing").unwrap(),
                Connect::DirectIpc {
                    address: format!("ipc://missing-{}", uuid::Uuid::new_v4().simple()),
                },
            )
            .is_err()
    );
}

#[test]
fn observing_a_dormant_domain_does_not_resolve_or_freeze_its_configuration() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    for slots in [1, 2] {
        runtime
            .update_client_config(ClientConfigPatch {
                domain_limits: DomainLimitsPatch {
                    max_outstanding_calls: ConfigField::Set(slots),
                    ..Default::default()
                },
            })
            .unwrap();
        assert_eq!(
            runtime.client_overrides().max_outstanding_calls,
            Some(slots)
        );
        assert_eq!(
            runtime.call_execution_snapshot().client,
            RoleCallExecutionStats::default()
        );
        assert!(runtime.client_domain_observer().is_none());
        assert!(!runtime.memory_stats().client.frozen);
    }
    attempt_missing_connection(&runtime);
    let snapshot = runtime.call_execution_snapshot();
    assert_eq!(snapshot.client.max_operations, Some(2));
    assert_eq!(snapshot.client.used_operations, Some(0));
    assert_eq!(snapshot.server, RoleCallExecutionStats::default());
}

#[test]
fn cloned_runtime_observes_the_same_domain_and_distinct_runtime_observes_its_own() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let clone = runtime.clone();
    let other = Runtime::new(RuntimeOptions::default()).unwrap();
    attempt_missing_connection(&runtime);
    attempt_missing_connection(&other);

    let domain_id = runtime
        .memory_stats()
        .client
        .domain_id
        .expect("activated client");
    assert_eq!(clone.memory_stats().client.domain_id, Some(domain_id));
    assert_ne!(other.memory_stats().client.domain_id, Some(domain_id));

    let observer = runtime.client_domain_observer().unwrap().downgrade();
    drop(runtime);
    assert_eq!(observer.snapshot().unwrap().domain_id, domain_id);
    drop(clone);
    assert!(!observer.is_alive());
    assert!(other.client_domain_observer().is_some());
}
