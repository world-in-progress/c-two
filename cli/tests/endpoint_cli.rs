//! Focused `c3 endpoint` tests: bounded arguments, honest status, and real
//! failure exit codes.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn c3() -> Command {
    Command::cargo_bin("c3").unwrap()
}

/// A credential file whose document is rejected by the strict codec: a v2
/// schema with no incarnation.
fn invalid_credential() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.json");
    std::fs::write(
        &path,
        r#"{"schemaVersion":2,"address":"ipc://c3-cli-bad","protocol":"managed-v2","platform":"unix","device":1,"inode":2,"changedSecs":3,"changedNanos":4}"#,
    )
    .unwrap();
    (dir, path)
}

#[test]
fn help_exposes_the_three_bounded_subcommands() {
    c3().args(["endpoint", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("inspect"))
        .stdout(predicate::str::contains("reap"))
        .stdout(predicate::str::contains("sweep"));
}

#[test]
fn inspect_reports_absent_without_inventing_an_endpoint() {
    c3().args(["endpoint", "inspect", "ipc://c3-cli-absent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status":"absent""#));
}

#[test]
fn inspect_rejects_a_non_local_or_path_like_address() {
    for address in ["tcp://host", "ipc://..", "ipc://x/y"] {
        c3().args(["endpoint", "inspect", address])
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid local endpoint"));
    }
}

#[test]
fn inspect_rejects_an_unknown_protocol() {
    c3().args(["endpoint", "inspect", "ipc://c3-cli-x", "--protocol", "managed-v3"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown IPC endpoint protocol"));
}

#[test]
fn sweep_requires_an_explicit_protocol() {
    // A sweep never guesses a namespace, so omitting --protocol is an error.
    c3().args(["endpoint", "sweep"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--protocol"));
}

#[test]
fn sweep_rejects_zero_and_unbounded_budgets() {
    for args in [
        vec!["--protocol", "legacy-v1", "--max-entries", "0"],
        vec!["--protocol", "legacy-v1", "--max-entries", "100000"],
        vec!["--protocol", "legacy-v1", "--max-ms", "0"],
        vec!["--protocol", "legacy-v1", "--max-ms", "60000"],
        vec!["--protocol", "legacy-v1", "--max-batches", "0"],
        vec!["--protocol", "legacy-v1", "--max-batches", "100000"],
    ] {
        let mut command = c3();
        command.arg("endpoint").arg("sweep").args(&args);
        command.assert().failure();
    }
}

#[test]
fn reap_requires_an_existing_credential_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.json");
    c3().args(["endpoint", "reap", "ipc://c3-cli-reap"])
        .arg("--credential")
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read credential"));
}

#[test]
fn reap_rejects_a_credential_that_fails_the_strict_codec() {
    let (_dir, path) = invalid_credential();
    c3().args(["endpoint", "reap", "ipc://c3-cli-bad"])
        .arg("--credential")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("incarnation-required"));
}

#[test]
fn reap_reports_a_credential_address_mismatch_as_stale() {
    // A well-formed managed credential for a different endpoint must not be
    // accepted as authority for the requested address.
    let unique = std::process::id();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"schemaVersion":2,"address":"ipc://c3-cli-other-{unique}","protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4}}"#
        ),
    )
    .unwrap();
    c3().args(["endpoint", "reap", &format!("ipc://c3-cli-target-{unique}")])
        .arg("--credential")
        .arg(&path)
        .assert()
        .failure()
        .stdout(predicate::str::contains(r#""status":"stale-target""#))
        .stdout(predicate::str::contains("credential-address-mismatch"));
}

#[test]
fn reap_rejects_an_oversized_credential_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.json");
    std::fs::write(&path, "x".repeat(8192)).unwrap();
    c3().args(["endpoint", "reap", "ipc://c3-cli-big"])
        .arg("--credential")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("exceeds the"));
}

#[test]
fn sweep_reports_an_honest_status_field() {
    // Whatever the namespace holds, the result must carry an explicit status
    // and must not claim reaped work it did not verify.
    let assert = c3()
        .args([
            "endpoint",
            "sweep",
            "--protocol",
            "legacy-v1",
            "--max-batches",
            "1",
        ])
        .assert();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""status":"#) && stdout.contains(r#""sweep":"#),
        "sweep output must carry status and counters: {stdout}"
    );
    // A one-batch legacy round cannot prove full coverage, so it must not
    // report `complete`.
    assert!(
        !stdout.contains(r#""status":"complete""#),
        "a bounded round must not claim completion: {stdout}"
    );
}

/// A credential path that is not a regular file must be rejected before any
/// read. A FIFO would otherwise block the open until a writer appeared.
#[cfg(unix)]
#[test]
fn reap_rejects_a_non_regular_credential_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("credential.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo must create the test FIFO");

    // `assert_cmd` captures output, so a blocked child would hang this test
    // rather than pass; the assertion is the fast, explicit rejection.
    c3().args(["endpoint", "reap", "ipc://c3-cli-fifo"])
        .arg("--credential")
        .arg(&fifo)
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a regular file"));

    // A directory is not a credential file either.
    c3().args(["endpoint", "reap", "ipc://c3-cli-dir"])
        .arg("--credential")
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a regular file"));
}

#[test]
fn reap_rejects_a_non_utf8_credential_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad-utf8.json");
    std::fs::write(&path, [0xff, 0xfe, 0x00, b'{', b'}']).unwrap();
    c3().args(["endpoint", "reap", "ipc://c3-cli-utf8"])
        .arg("--credential")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid UTF-8"));
}

/// The default protocol is the configured process policy, not a build-time
/// constant, and an explicit `--protocol` always wins over it.
#[test]
fn inspect_default_protocol_follows_the_configured_policy() {
    // An invalid configured value fails closed instead of silently falling
    // back to a hardcoded legacy default.
    c3().env("C2_IPC_ENDPOINT_PROTOCOL", "bogus")
        .args(["endpoint", "inspect", "ipc://c3-cli-protocol"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot resolve the configured endpoint protocol",
        ));

    // A valid configured value is accepted, so the policy is really consulted.
    c3().env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
        .args(["endpoint", "inspect", "ipc://c3-cli-protocol"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status":"absent""#));

    // An explicit protocol wins over a contradictory configured value.
    c3().env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
        .args([
            "endpoint",
            "inspect",
            "ipc://c3-cli-protocol",
            "--protocol",
            "legacy-v1",
        ])
        .assert()
        .success();
}

/// `reap` derives the address with the protocol the credential records, so a
/// managed credential is not misread as stale under a legacy process policy.
#[test]
fn reap_derives_the_endpoint_from_the_credential_protocol() {
    let unique = std::process::id();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"schemaVersion":2,"address":"ipc://c3-cli-derive-{unique}","protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4}}"#
        ),
    )
    .unwrap();
    c3().env("C2_IPC_ENDPOINT_PROTOCOL", "legacy-v1")
        .args(["endpoint", "reap", &format!("ipc://c3-cli-derive-{unique}")])
        .arg("--credential")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status":"already-absent""#));
}
