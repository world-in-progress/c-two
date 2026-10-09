use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn relay_help_exposes_mesh_and_idle_options() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--bind"))
        .stdout(predicate::str::contains("--idle-timeout"))
        .stdout(predicate::str::contains("--seeds"))
        .stdout(predicate::str::contains("--relay-id"))
        .stdout(predicate::str::contains("--advertise-url"))
        .stdout(predicate::str::contains("--upstream"))
        .stdout(predicate::str::contains("--ipc-root"))
        .stdout(predicate::str::contains("--ipc-pool-enabled"))
        .stdout(predicate::str::contains("--ipc-shm-backing-budget-bytes"))
        .stdout(predicate::str::contains("--ipc-file-backing-budget-bytes"))
        .stdout(predicate::str::contains(
            "--ipc-live-reassembly-budget-bytes",
        ));
}

#[test]
fn relay_rejects_invalid_upstream_format() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--upstream", "grid-ipc://server", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Expected NAME=SERVER_ID@ADDRESS"));
}

#[test]
fn relay_rejects_ambiguous_upstream_without_server_id() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--upstream", "grid=ipc://server", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Expected NAME=SERVER_ID@ADDRESS"));
}

#[test]
fn relay_rejects_invalid_upstream_server_id() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "relay",
        "--upstream",
        "grid= server @ipc://server",
        "--dry-run",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "server_id cannot contain leading or trailing whitespace",
    ));
}

#[test]
fn relay_dry_run_accepts_valid_configuration() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "relay",
        "--bind",
        "127.0.0.1:9999",
        "--idle-timeout",
        "10",
        "--seeds",
        "http://127.0.0.1:8301,http://127.0.0.1:8302",
        "--relay-id",
        "relay-a",
        "--advertise-url",
        "http://relay-a:9999",
        "--upstream",
        "grid=server-grid@ipc://server",
        "--dry-run",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("relay-a"))
    .stdout(predicate::str::contains(
        "upstream=grid server_id=server-grid address=ipc://server",
    ));
}

#[test]
fn relay_dry_run_loads_default_env_file() {
    let tempdir = tempfile::tempdir().unwrap();
    std::fs::write(
        tempdir.path().join(".env"),
        "C2_RELAY_BIND=127.0.0.1:9191\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.current_dir(tempdir.path())
        .env_remove("C2_RELAY_BIND")
        .env_remove("C2_ENV_FILE")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=127.0.0.1:9191"));
}

#[test]
fn relay_dry_run_uses_canonical_idle_timeout_default() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("C2_ENV_FILE", "")
        .env_remove("C2_RELAY_IDLE_TIMEOUT")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("idle_timeout=60"));
}

#[test]
fn relay_help_hides_skip_ipc_validation() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--skip-ipc-validation").not());
}

#[test]
fn relay_rejects_skip_ipc_validation_even_when_hidden() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("C2_ENV_FILE", "")
        .args(["relay", "--skip-ipc-validation", "--dry-run"])
        .assert()
        .failure();
}

#[test]
fn relay_dry_run_respects_custom_env_file() {
    let tempdir = tempfile::tempdir().unwrap();
    let env_file = tempdir.path().join("relay.env");
    std::fs::write(&env_file, "C2_RELAY_BIND=127.0.0.1:9292\n").unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.current_dir(tempdir.path())
        .env_remove("C2_RELAY_BIND")
        .env("C2_ENV_FILE", &env_file)
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=127.0.0.1:9292"));
}

#[test]
fn relay_dry_run_loads_proxy_policy_from_env_file() {
    let tempdir = tempfile::tempdir().unwrap();
    std::fs::write(tempdir.path().join(".env"), "C2_RELAY_USE_PROXY=1\n").unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.current_dir(tempdir.path())
        .env_remove("C2_RELAY_USE_PROXY")
        .env_remove("C2_ENV_FILE")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("relay_use_proxy=true"));
}

#[test]
fn relay_dry_run_loads_remote_payload_chunk_size_from_env_file() {
    let tempdir = tempfile::tempdir().unwrap();
    std::fs::write(
        tempdir.path().join(".env"),
        "C2_REMOTE_PAYLOAD_CHUNK_SIZE=2097152\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.current_dir(tempdir.path())
        .env_remove("C2_REMOTE_PAYLOAD_CHUNK_SIZE")
        .env_remove("C2_ENV_FILE")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "remote_payload_chunk_size=2097152",
        ));
}

#[test]
fn relay_process_env_overrides_env_file() {
    let tempdir = tempfile::tempdir().unwrap();
    std::fs::write(
        tempdir.path().join(".env"),
        "C2_RELAY_BIND=127.0.0.1:9191\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.current_dir(tempdir.path())
        .env("C2_RELAY_BIND", "127.0.0.1:9393")
        .env_remove("C2_ENV_FILE")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=127.0.0.1:9393"));
}

#[test]
fn relay_cli_flag_overrides_process_env() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("C2_RELAY_BIND", "127.0.0.1:9393")
        .env("C2_ENV_FILE", "")
        .args(["relay", "--bind", "127.0.0.1:9494", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=127.0.0.1:9494"));
}

// Each command owns its environment; no process-global mutation races parallel tests.
fn isolated_relay_command() -> Command {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    for (key, _) in std::env::vars() {
        if key.starts_with("C2_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("C2_ENV_FILE", "");
    cmd
}

#[test]
fn relay_ipc_dry_run_reports_canonical_defaults() {
    isolated_relay_command()
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ipc_pool_enabled=true"))
        .stdout(predicate::str::contains("ipc_pool_prewarm_segments=0"))
        .stdout(predicate::str::contains(
            "ipc_shm_backing_budget_bytes=8589934592",
        ))
        .stdout(predicate::str::contains(
            "ipc_file_backing_budget_bytes=17179869184",
        ))
        .stdout(predicate::str::contains(
            "ipc_live_reassembly_budget_bytes=8589934592",
        ));
}

#[test]
fn relay_ipc_flags_override_env_and_accept_zero_budgets() {
    isolated_relay_command()
        .env("C2_IPC_POOL_ENABLED", "true")
        .env("C2_IPC_SHM_BACKING_BUDGET_BYTES", "100")
        .env("C2_IPC_FILE_BACKING_BUDGET_BYTES", "200")
        .env("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "300")
        .args([
            "relay",
            "--ipc-pool-enabled",
            "false",
            "--ipc-shm-backing-budget-bytes",
            "0",
            "--ipc-file-backing-budget-bytes",
            "0",
            "--ipc-live-reassembly-budget-bytes",
            "0",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ipc_pool_enabled=false"))
        .stdout(predicate::str::contains("ipc_shm_backing_budget_bytes=0"))
        .stdout(predicate::str::contains("ipc_file_backing_budget_bytes=0"))
        .stdout(predicate::str::contains(
            "ipc_live_reassembly_budget_bytes=0",
        ));
}

#[test]
fn relay_ipc_env_overrides_dotenv_and_reports_prewarm() {
    let tempdir = tempfile::tempdir().unwrap();
    let path = tempdir.path().join("relay.env");
    std::fs::write(&path, "C2_IPC_POOL_ENABLED=false\nC2_IPC_POOL_PREWARM_SEGMENTS=0\nC2_IPC_SHM_BACKING_BUDGET_BYTES=100\nC2_IPC_FILE_BACKING_BUDGET_BYTES=200\nC2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES=300\n").unwrap();
    isolated_relay_command()
        .env("C2_ENV_FILE", path)
        .env("C2_IPC_POOL_ENABLED", "true")
        .env("C2_IPC_POOL_PREWARM_SEGMENTS", "1")
        .env("C2_IPC_SHM_BACKING_BUDGET_BYTES", "400")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ipc_pool_enabled=true"))
        .stdout(predicate::str::contains("ipc_pool_prewarm_segments=1"))
        .stdout(predicate::str::contains("ipc_shm_backing_budget_bytes=400"))
        .stdout(predicate::str::contains(
            "ipc_file_backing_budget_bytes=200",
        ))
        .stdout(predicate::str::contains(
            "ipc_live_reassembly_budget_bytes=300",
        ));
}

#[test]
fn relay_ipc_rejects_disabled_pool_with_prewarm_before_listening() {
    // Deliberately invalid bind distinguishes config rejection from listener errors.
    isolated_relay_command()
        .env("C2_IPC_POOL_PREWARM_SEGMENTS", "1")
        .args([
            "relay",
            "--bind",
            "invalid-bind",
            "--ipc-pool-enabled",
            "false",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pool_prewarm_segments"))
        .stderr(predicate::str::contains("failed to start relay").not());
}

#[test]
fn relay_ipc_rejects_malformed_env_before_listening() {
    for (key, value) in [
        ("C2_IPC_POOL_ENABLED", "invalid"),
        ("C2_IPC_SHM_BACKING_BUDGET_BYTES", "-1"),
        ("C2_IPC_FILE_BACKING_BUDGET_BYTES", "18446744073709551616"),
        ("C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES", "invalid"),
    ] {
        isolated_relay_command()
            .env(key, value)
            .args(["relay", "--bind", "invalid-bind"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(key))
            .stderr(predicate::str::contains("failed to start relay").not());
    }
}

#[test]
fn relay_ipc_rejects_malformed_cli_values() {
    for (flag, value) in [
        ("--ipc-pool-enabled", "invalid"),
        ("--ipc-shm-backing-budget-bytes", "-1"),
        ("--ipc-file-backing-budget-bytes", "18446744073709551616"),
        ("--ipc-live-reassembly-budget-bytes", "invalid"),
    ] {
        isolated_relay_command()
            .args(["relay", &format!("{flag}={value}"), "--dry-run"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument").not());
    }
}

#[test]
fn relay_default_context_matches_platform_authority() {
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    isolated_relay_command()
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "local_namespace={}",
            context.namespace_id()
        )));
    #[cfg(windows)]
    assert_eq!(
        context.platform_kind(),
        c2_config::LocalEndpointNamespace::WindowsNamedPipe
    );
}

#[cfg(unix)]
#[test]
fn relay_root_cli_overrides_env_and_env_overrides_dotenv() {
    let alpha = tempfile::Builder::new()
        .prefix("c3r-")
        .tempdir_in("/tmp")
        .unwrap();
    let beta = tempfile::Builder::new()
        .prefix("c3r-")
        .tempdir_in("/tmp")
        .unwrap();
    let env_file = alpha.path().join("relay.env");
    std::fs::write(
        &env_file,
        format!("C2_IPC_ROOT={}\n", alpha.path().display()),
    )
    .unwrap();
    let alpha_id = c2_config::LocalEndpointContext::with_unix_root(alpha.path()).unwrap();
    let beta_id = c2_config::LocalEndpointContext::with_unix_root(beta.path()).unwrap();
    isolated_relay_command()
        .env("C2_ENV_FILE", &env_file)
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "local_namespace={}",
            alpha_id.namespace_id()
        )));
    isolated_relay_command()
        .env("C2_ENV_FILE", &env_file)
        .env("C2_IPC_ROOT", beta.path())
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "local_namespace={}",
            beta_id.namespace_id()
        )));
    isolated_relay_command()
        .env("C2_ENV_FILE", &env_file)
        .env("C2_IPC_ROOT", beta.path())
        .args(["relay", "--ipc-root"])
        .arg(alpha.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "local_namespace={}",
            alpha_id.namespace_id()
        )));
}

#[cfg(unix)]
#[test]
fn relay_invalid_root_is_rejected_before_listening_and_dry_run_creates_nothing() {
    isolated_relay_command()
        .args(["relay", "--ipc-root", "relative", "--bind", "invalid-bind"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("local endpoint root option"))
        .stderr(predicate::str::contains("failed to start relay").not());
    let root = tempfile::Builder::new()
        .prefix("c3r-")
        .tempdir_in("/tmp")
        .unwrap();
    let missing = root.path().join("missing");
    isolated_relay_command()
        .args(["relay", "--ipc-root"])
        .arg(&missing)
        .arg("--dry-run")
        .assert()
        .success();
    assert!(!missing.exists());
}

#[cfg(windows)]
#[test]
fn relay_windows_root_override_is_not_applicable() {
    isolated_relay_command()
        .args(["relay", "--ipc-root", r"C:\c2-root", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not applicable"));
    isolated_relay_command()
        .env("C2_IPC_ROOT", r"C:\c2-root")
        .args(["relay", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not applicable"));
}
