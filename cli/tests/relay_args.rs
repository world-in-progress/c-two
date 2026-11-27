use assert_cmd::Command;
use predicates::prelude::*;

const RETIRED_FLAGS: &[&str] = &[
    "--idle-timeout",
    "--ipc-pool-enabled",
    "--ipc-shm-backing-budget-bytes",
    "--ipc-file-backing-budget-bytes",
    "--ipc-live-reassembly-budget-bytes",
    "--call-payload-buffer-budget-bytes",
    "--call-retained-input-budget-bytes",
];

#[test]
fn relay_help_exposes_current_options() {
    let mut help = isolated_relay_command();
    let mut assertion = help.args(["relay", "--help"]).assert().success();
    for flag in [
        "--bind",
        "--edge-bind",
        "--tls-cert",
        "--tls-key",
        "--tls-auto",
        "--tls-san",
        "--state-dir",
        "--insecure-plaintext",
        "--seeds",
        "--relay-id",
        "--advertise-url",
        "--upstream",
        "--ipc-root",
        "--memory-budget-bytes",
        "--file-spill-budget-bytes",
        "--call-max-outstanding",
    ] {
        assertion = assertion.stdout(predicate::str::contains(flag));
    }
    for flag in RETIRED_FLAGS {
        assertion = assertion.stdout(predicate::str::contains(*flag).not());
    }
}

#[test]
fn relay_rejects_retired_flags() {
    for flag in RETIRED_FLAGS {
        isolated_relay_command()
            .args(["relay", &format!("{flag}=1"), "--dry-run"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"))
            .stderr(predicate::str::contains(*flag));
    }
}

#[test]
fn relay_domain_flags_accept_zero_and_full_u64_range() {
    for value in ["0", "18446744073709551615"] {
        let automatic_calls = if value == "0" { "0" } else { "281474976710655" };
        isolated_relay_command()
            .env("C2_MEMORY_BUDGET_BYTES", "123")
            .env("C2_FILE_SPILL_BUDGET_BYTES", "456")
            .args([
                "relay",
                "--memory-budget-bytes",
                value,
                "--file-spill-budget-bytes",
                value,
                "--dry-run",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "memory_budget_bytes={value}\n"
            )))
            .stdout(predicate::str::contains(format!(
                "file_spill_budget_bytes={value}\n"
            )))
            .stdout(predicate::str::contains(format!(
                "call_max_outstanding={automatic_calls}\n"
            )));
        isolated_relay_command()
            .args(["relay", "--call-max-outstanding", value, "--dry-run"])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "call_max_outstanding={value}\n"
            )));
    }
}

#[test]
fn relay_rejects_positive_call_limit_with_zero_memory() {
    isolated_relay_command()
        .args([
            "relay",
            "--memory-budget-bytes",
            "0",
            "--call-max-outstanding",
            "1",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("max_outstanding_calls"));
}

#[test]
fn relay_domain_environment_ignores_retired_settings() {
    isolated_relay_command()
        .env("C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES", "invalid")
        .env("C2_IPC_SHM_BACKING_BUDGET_BYTES", "invalid")
        .env("C2_RELAY_IDLE_TIMEOUT", "invalid")
        .env("C2_REMOTE_PAYLOAD_CHUNK_SIZE", "invalid")
        .env("C2_MEMORY_BUDGET_BYTES", "123")
        .env("C2_FILE_SPILL_BUDGET_BYTES", "456")
        .env("C2_CALL_MAX_OUTSTANDING", "7")
        .args(["relay", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("memory_budget_bytes=123\n"))
        .stdout(predicate::str::contains("file_spill_budget_bytes=456\n"))
        .stdout(predicate::str::contains("call_max_outstanding=7\n"))
        .stdout(predicate::str::contains("ipc_pool_").not())
        .stdout(predicate::str::contains("ipc_shm_backing_budget_bytes").not())
        .stdout(predicate::str::contains("call_payload_buffer_budget_bytes").not())
        .stdout(predicate::str::contains("remote_payload_chunk_size").not())
        .stdout(predicate::str::contains("idle_timeout").not());
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
    .stdout(predicate::str::contains("relay_id=relay-a"))
    .stdout(predicate::str::contains(
        "seed=http://127.0.0.1:8301\nseed=http://127.0.0.1:8302\n",
    ))
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
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("C2_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("C2_ENV_FILE", "");
    cmd
}

#[test]
fn relay_rejects_malformed_domain_limits() {
    for (env, flag) in [
        ("C2_MEMORY_BUDGET_BYTES", "--memory-budget-bytes"),
        ("C2_FILE_SPILL_BUDGET_BYTES", "--file-spill-budget-bytes"),
        ("C2_CALL_MAX_OUTSTANDING", "--call-max-outstanding"),
    ] {
        isolated_relay_command()
            .env(env, "invalid")
            .args(["relay", "--dry-run"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(env));
        isolated_relay_command()
            .args([
                "relay",
                &format!("{flag}=18446744073709551616"),
                "--dry-run",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(flag));
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

#[test]
fn relay_dry_run_enforces_listener_and_tls_modes() {
    isolated_relay_command()
        .args(["relay", "--bind", "0.0.0.0:8080", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--edge-bind"));

    for (address, flags, mode, url) in [
        (
            "0.0.0.0:8443",
            &[][..],
            "AutoSelfSigned",
            "https://127.0.0.1:8443",
        ),
        (
            "[::]:8443",
            &["--insecure-plaintext"][..],
            "Plaintext",
            "http://[::1]:8443",
        ),
        (
            "localhost:8443",
            &[][..],
            "Plaintext",
            "http://127.0.0.1:8443",
        ),
        (
            "127.0.0.1:8443",
            &["--tls-auto", "--tls-san", "example.test,127.0.0.2"][..],
            "AutoSelfSigned",
            "https://127.0.0.1:8443",
        ),
    ] {
        isolated_relay_command()
            .args(["relay", "--edge-bind", address, "--dry-run"])
            .args(flags)
            .assert()
            .success()
            .stdout(predicate::str::contains(format!("edge_tls_mode={mode}")))
            .stdout(predicate::str::contains(format!("advertise_url={url}")));
    }
}
