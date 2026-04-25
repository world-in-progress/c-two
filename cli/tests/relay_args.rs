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
        .stdout(predicate::str::contains("--upstream"));
}

#[test]
fn relay_rejects_invalid_upstream_format() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--upstream", "grid-ipc://server", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Expected NAME=ADDRESS"));
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
        "grid=ipc://server",
        "--dry-run",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("relay-a"))
    .stdout(predicate::str::contains("grid=ipc://server"));
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
