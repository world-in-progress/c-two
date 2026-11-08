use c_two::{Runtime, RuntimeOptions};

#[test]
fn runtime_projects_client_idle_timeout_override() {
    let mut options = RuntimeOptions::default();
    options
        .client_ipc_overrides
        .get_or_insert_default()
        .client_idle_timeout_secs = Some(0.125);
    let runtime = Runtime::new(options).unwrap();
    let mut projected = runtime.client_ipc_overrides().unwrap();
    assert_eq!(projected.client_idle_timeout_secs, Some(0.125));
    assert!(!runtime.client_config_frozen());
    projected.client_idle_timeout_secs = Some(0.0);
    runtime.set_client_ipc_overrides(Some(projected)).unwrap();
    assert_eq!(
        runtime
            .client_ipc_overrides()
            .unwrap()
            .client_idle_timeout_secs,
        Some(0.0)
    );
}
