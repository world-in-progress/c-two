//! Rust SDK projects the Core connection deadline without changing call options.
use std::time::Duration;

use c_two::generated::ErrorCode;
use c_two::{Connect, ConnectOptions, Error, ExpectedRouteContract, Runtime, RuntimeOptions};

fn expected() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "connect-timeout".into(),
        crm_ns: "test.connect-timeout".into(),
        crm_name: "Echo".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "a".repeat(64),
        signature_hash: "b".repeat(64),
    }
}

#[test]
fn connect_options_default_and_finite_budget() {
    assert_eq!(ConnectOptions::new().timeout(), None);
    assert_eq!(
        ConnectOptions::from_timeout_secs(None).unwrap().timeout(),
        None
    );
    assert_eq!(
        ConnectOptions::new()
            .with_timeout(Duration::from_millis(125))
            .timeout(),
        Some(Duration::from_millis(125)),
    );
    assert_eq!(
        ConnectOptions::from_timeout_secs(Some(0.0))
            .unwrap()
            .timeout(),
        Some(Duration::ZERO),
    );
    for invalid in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300] {
        assert!(ConnectOptions::from_timeout_secs(Some(invalid)).is_err());
    }
}

#[test]
fn zero_budget_expires_before_any_transport_or_relay_config() {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    for mode in [
        Connect::DirectIpc {
            address: "ipc://connect-timeout-unused".into(),
        },
        Connect::ExplicitRelay {
            relay_url: "http://127.0.0.1:9".into(),
        },
        Connect::RelayAware,
    ] {
        let failure = runtime
            .connect_with_options(
                expected(),
                mode,
                ConnectOptions::new().with_timeout(Duration::ZERO),
            )
            .err()
            .expect("zero connect budget must expire");
        let Error::Semantic(error) = failure else {
            panic!("wrong failure: {failure}");
        };
        assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
        assert_eq!(
            error.details.get("operation").map(String::as_str),
            Some("connect")
        );
        assert_eq!(
            error.details.get("transport_phase").map(String::as_str),
            Some("pre_dispatch")
        );
        assert!(
            error
                .details
                .get("stage")
                .is_some_and(|stage| !stage.is_empty())
        );
    }
}
