use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn c3_help_lists_runtime_commands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("relay"))
        .stdout(predicate::str::contains("registry"));
}

#[test]
fn c3_version_prints_package_version() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn c3_help_includes_embedded_banner() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("C-Two command-line interface"));
}
