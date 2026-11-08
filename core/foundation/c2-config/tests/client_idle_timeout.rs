use std::time::Duration;

use c2_config::{
    ClientIpcConfig, ClientIpcConfigOverrides, ConfigResolver, ConfigSources,
    RuntimeConfigOverrides,
};

fn resolve(value: Option<&str>, override_value: Option<f64>) -> Result<ClientIpcConfig, String> {
    let mut sources = ConfigSources::empty();
    if let Some(value) = value {
        sources
            .process_env
            .insert("C2_IPC_CLIENT_IDLE_TIMEOUT".into(), value.into());
    }
    ConfigResolver::resolve_client_ipc(
        ClientIpcConfigOverrides {
            client_idle_timeout_secs: override_value,
            ..Default::default()
        },
        RuntimeConfigOverrides::default(),
        sources,
    )
    .map_err(|error| error.to_string())
}

#[test]
fn client_idle_timeout_defaults_to_sixty_seconds() {
    let cfg = resolve(None, None).unwrap();
    assert_eq!(cfg.client_idle_timeout_secs, 60.0);
    assert_eq!(cfg.client_idle_timeout(), Some(Duration::from_secs(60)));
}

#[test]
fn client_idle_timeout_zero_disables_timed_eviction() {
    for cfg in [resolve(Some("0"), None), resolve(Some("0.125"), Some(0.0))] {
        let cfg = cfg.unwrap();
        assert_eq!(cfg.client_idle_timeout_secs, 0.0);
        assert_eq!(cfg.client_idle_timeout(), None);
    }
}

#[test]
fn client_idle_timeout_fractional_seconds_and_override_precedence() {
    let env = resolve(Some("0.125"), None).unwrap();
    assert_eq!(env.client_idle_timeout(), Some(Duration::from_millis(125)));
    let explicit = resolve(Some("0.125"), Some(0.25)).unwrap();
    assert_eq!(
        explicit.client_idle_timeout(),
        Some(Duration::from_millis(250))
    );
}

#[test]
fn client_idle_timeout_rejects_invalid_environment_values() {
    for value in ["invalid", "-1", "NaN", "inf", "-inf", "1e100"] {
        let error = resolve(Some(value), None).unwrap_err();
        assert!(
            error.contains("CLIENT_IDLE_TIMEOUT") || error.contains("client_idle_timeout"),
            "{value}: {error}"
        );
    }
    // Follow the existing IPC parser: malformed environment text is an error
    // even when code overrides that field.
    assert!(
        resolve(Some("invalid"), Some(1.0))
            .unwrap_err()
            .contains("C2_IPC_CLIENT_IDLE_TIMEOUT")
    );
}

#[test]
fn client_idle_timeout_rejects_invalid_typed_values() {
    for value in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e100] {
        assert!(
            resolve(None, Some(value))
                .unwrap_err()
                .contains("client_idle_timeout")
        );
    }
}

#[test]
fn client_idle_timeout_must_fit_milliseconds() {
    let rounded_limit = u64::MAX as f64 / 1000.0;
    let largest_valid = f64::from_bits(rounded_limit.to_bits() - 1);
    let cfg = resolve(None, Some(largest_valid)).unwrap();
    assert!(cfg.client_idle_timeout().unwrap().as_millis() <= u128::from(u64::MAX));
    for cfg in [
        resolve(None, Some(rounded_limit)),
        resolve(Some(&rounded_limit.to_string()), None),
    ] {
        assert!(cfg.unwrap_err().contains("milliseconds"));
    }
}
