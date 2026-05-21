use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn c3_help_lists_runtime_commands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("relay"))
        .stdout(predicate::str::contains("contract"))
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

#[test]
fn c3_help_can_render_banner_with_legacy_color() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[38;2;102;237;173m"));
}

#[test]
fn c3_help_respects_no_color_for_banner() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .env("CLICOLOR_FORCE", "1")
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[38;2;102;237;173m").not());
}
