//! Public metadata observation requires no endpoint, executor or payload.
//! Limits belong to each Runtime/domain, not one cumulative process budget.
use c2_core::{
    CallExecutionLimitsOverrides, CallExecutionObservation, CallExecutionObserver, ConfigSources,
    Runtime, RuntimeOptions,
};

fn assert_read_only_handle(_: &CallExecutionObserver) {}

#[test]
fn capture_open_uninitialized_domain_without_freezing_policy() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let observer = runtime.call_execution_observer();
    assert_read_only_handle(&observer);
    assert_eq!(
        observer.snapshot(),
        CallExecutionObservation {
            initialized: false,
            closed: false,
            counters: None,
        }
    );
    assert!(observer.is_live());
    runtime
        .set_call_execution_limits_with_sources(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(1),
                retained_input_budget_bytes: Some(32),
            },
            ConfigSources::empty(),
        )
        .unwrap();
    assert_eq!(
        runtime
            .call_execution_limits_overrides()
            .max_outstanding_calls,
        Some(1)
    );
    // Existing resolved snapshot remains a policy preview, not initialization.
    assert_eq!(runtime.call_execution_snapshot().unwrap().max_operations, 1);
    assert!(!observer.snapshot().initialized);
    runtime
        .set_call_execution_limits_with_sources(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(2),
                retained_input_budget_bytes: Some(64),
            },
            ConfigSources::empty(),
        )
        .unwrap();
    assert_eq!(runtime.call_execution_snapshot().unwrap().max_operations, 2);
}

#[test]
fn cloned_runtime_shares_domain_distinct_runtime_has_independent_domain() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let clone = runtime.clone();
    let other = Runtime::new(RuntimeOptions::default()).unwrap();
    let observer = runtime.call_execution_observer();
    assert!(observer.same_domain(&observer.clone()));
    assert!(observer.same_domain(&clone.call_execution_observer()));
    assert!(!observer.same_domain(&other.call_execution_observer()));
    drop(runtime);
    assert!(observer.is_live());
    drop(clone);
    assert_eq!(
        observer.snapshot(),
        CallExecutionObservation {
            initialized: false,
            closed: true,
            counters: None,
        }
    );
    assert!(!observer.is_live());
    assert!(other.call_execution_observer().is_live());
}
