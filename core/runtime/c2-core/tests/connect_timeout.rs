use std::time::Duration;

use c2_config::ConnectDeadline;
use c2_config::ConnectOptions;
use c2_contract::ExpectedRouteContract;
use c2_core::{Connect, ConnectAttempt, Error, Runtime, RuntimeOptions};
use c2_error::ErrorCode;

#[test]
fn connect_timeout_seconds_are_checked() {
    for seconds in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(ConnectOptions::from_timeout_secs(Some(seconds)).is_err());
    }
    assert_eq!(ConnectOptions::new().timeout(), None);
    assert_eq!(
        ConnectOptions::from_timeout_secs(None).unwrap().timeout(),
        None
    );
    assert_eq!(
        ConnectOptions::from_timeout_secs(Some(0.0))
            .unwrap()
            .timeout(),
        Some(Duration::ZERO)
    );
}

fn expected() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "connect-timeout".into(),
        crm_ns: "test.connect".into(),
        crm_name: "Echo".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "a".repeat(64),
        signature_hash: "b".repeat(64),
    }
}

#[test]
fn zero_budget_is_canonical_before_any_transport_or_configuration() {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    for mode in [
        Connect::DirectIpc {
            address: "malformed".into(),
        },
        Connect::ExplicitRelay {
            relay_url: "malformed".into(),
        },
        Connect::RelayAware,
    ] {
        let error = runtime
            .connect_with_options(
                expected(),
                mode,
                ConnectOptions::new().with_timeout(Duration::ZERO),
            )
            .unwrap_err();
        let Error::Semantic(error) = error else {
            panic!("expected canonical connect deadline")
        };
        assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
        assert_eq!(error.details["operation"], "connect");
        assert_eq!(error.details["transport_phase"], "pre_dispatch");
        assert_eq!(error.details["stage"], "connect_start");
        assert_eq!(error.details["fallback_eligible"], "false");
        assert_eq!(error.details["route_withdrawal"], "false");
    }
    assert_eq!(runtime.path_counters().direct_ipc(), 0);
    assert!(!runtime.client_config_frozen());
}

#[test]
fn absolute_budget_is_not_restarted_when_copied() {
    let deadline =
        ConnectDeadline::start(ConnectOptions::new().with_timeout(Duration::from_millis(80)))
            .unwrap();
    let original = deadline.instant();
    std::thread::sleep(Duration::from_millis(35));
    let copied = deadline;
    assert_eq!(copied.instant(), original);
    assert!(copied.remaining("next_stage").unwrap().unwrap() < Duration::from_millis(65));
    std::thread::sleep(Duration::from_millis(65));
    assert_eq!(copied.check("next_stage").unwrap_err().stage, "next_stage");
}

#[test]
fn unrepresentable_deadline_is_rejected_before_transport() {
    assert!(ConnectOptions::from_timeout_secs(Some(1e300)).is_err());
    assert!(ConnectDeadline::start(ConnectOptions::new().with_timeout(Duration::MAX)).is_err());
}

#[test]
fn sdk_glue_and_connect_share_the_original_attempt_budget() {
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let attempt =
        ConnectAttempt::start(ConnectOptions::new().with_timeout(Duration::from_millis(50)))
            .unwrap();
    std::thread::sleep(Duration::from_millis(70));
    let error = runtime
        .connect_with_attempt(
            expected(),
            Connect::DirectIpc {
                address: "malformed".into(),
            },
            &attempt,
        )
        .unwrap_err();
    let Error::Semantic(error) = error else {
        panic!("attempt must not restart")
    };
    assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
    assert_eq!(error.details["stage"], "connect_start");
    assert!(!runtime.client_config_frozen());
}
