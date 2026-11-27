use assert_cmd::Command;
use predicates::prelude::*;

fn command() -> Command {
    let mut command = assert_cmd::cargo::cargo_bin_cmd!("c3");
    command.env_clear().env("C2_ENV_FILE", "");
    command
}

#[test]
fn bridge_dry_run_resolves_cli_env_and_file_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("bridge.env");
    let state = dir.path().join("bridge");
    std::fs::write(&env_file,
        "C2_BRIDGE_BIND=127.0.0.1:9420\nC2_BRIDGE_QUIC_BIND=127.0.0.1:7842\nC2_BRIDGE_TOKEN=file-secret\n"
    ).unwrap();
    command()
        .env("C2_ENV_FILE", &env_file)
        .env("C2_BRIDGE_BIND", "127.0.0.1:9421")
        .env("C2_BRIDGE_TOKEN", "process-secret")
        .args([
            "bridge",
            "--quic-bind",
            "OfF",
            "--token",
            "cli-secret",
            "--dry-run",
            "--state-dir",
        ])
        .arg(&state)
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=127.0.0.1:9421"))
        .stdout(predicate::str::contains("quic_bind=off"))
        .stdout(predicate::str::contains("token=explicit"))
        .stdout(predicate::str::contains("-secret").not());
    assert!(!state.exists());
}

#[test]
fn bridge_dry_run_rejects_invalid_configuration() {
    for (args, diagnostic) in [
        (
            &["--quic-bind", "127.0.0.1:65536"][..],
            "invalid bridge --quic-bind",
        ),
        (&["--tls-key", "key.pem"], "must be set together"),
        (&["--token", ""], "printable ASCII"),
    ] {
        command()
            .args(["bridge", "--dry-run"])
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(diagnostic));
    }
    command()
        .args(["bridge", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bind=0.0.0.0:8443"))
        .stdout(predicate::str::contains("quic_bind=0.0.0.0:7842"))
        .stdout(predicate::str::contains("token=auto"));
}

#[test]
fn relay_dry_run_redacts_mesh_credentials() {
    command()
        .env("C2_RELAY_BRIDGE_TOKEN", "bridge-password")
        .env("C2_RELAY_CLUSTER_SECRET", "cluster-password")
        .args([
            "relay",
            "--bridge",
            "https://bridge.example.org?quic=off",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("bridge_token=set"))
        .stdout(predicate::str::contains("cluster_secret=set"))
        .stdout(predicate::str::contains("-password").not());
}
