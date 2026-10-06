//! Focused `c3 endpoint` tests: bounded arguments, honest status, and real
//! failure exit codes.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
fn prepare_legacy_namespace() {
    static PREPARED: std::sync::Once = std::sync::Once::new();
    PREPARED.call_once(|| {
        let address = unique_address("namespace-setup");
        let mut overrides = c2_config::ServerIpcConfigOverrides::default();
        overrides.base.endpoint_protocol = Some(c2_config::LocalEndpointProtocol::LegacyV1);
        let runtime = c2_core::Runtime::new(c2_core::RuntimeOptions {
            server_id: Some(address.strip_prefix("ipc://").unwrap().to_owned()),
            server_ipc_overrides: Some(overrides),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let host = runtime
            .host(c2_core::HostOptions::default().without_relay())
            .unwrap();
        let outcome = host.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    });
}

#[cfg(unix)]
fn inspect_when_gate_available(mut command: Command) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let output = command.output().unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if report["status"] != "io-error" {
            assert!(output.status.success(), "{report:?}");
            return report;
        }
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(report["reason"], std::io::ErrorKind::WouldBlock.to_string());
        assert!(
            std::time::Instant::now() < deadline,
            "namespace gate stayed busy: {report:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn unique_address(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "ipc://c3-cli-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
struct TestSocket {
    endpoint: c2_core::LocalEndpoint,
    _listener: std::os::unix::net::UnixListener,
}

#[cfg(unix)]
impl TestSocket {
    fn bind(address: &str) -> Self {
        prepare_legacy_namespace();
        let endpoint = c2_core::LocalEndpoint::from_address_with_protocol(
            address,
            c2_config::LocalEndpointProtocol::LegacyV1,
        )
        .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(endpoint.os_name()).unwrap();
        Self {
            endpoint,
            _listener: listener,
        }
    }

    fn assert_reachable(&self) {
        std::os::unix::net::UnixStream::connect(self.endpoint.os_name()).unwrap();
    }
}

#[cfg(unix)]
impl Drop for TestSocket {
    fn drop(&mut self) {
        // Remove only the test's own canonical socket, including on assertion failure.
        let _ = std::fs::remove_file(self.endpoint.os_name());
    }
}

fn c3() -> Command {
    let mut command = Command::cargo_bin("c3").unwrap();
    command
        .env("C2_ENV_FILE", "")
        .env("C2_IPC_ENDPOINT_PROTOCOL", "legacy-v1");
    command
}

/// A credential file whose document is rejected by the strict codec: a v2
/// schema with no incarnation.
fn invalid_credential() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.json");
    #[cfg(unix)]
    let document = r#"{"schemaVersion":2,"address":"ipc://c3-cli-bad","protocol":"managed-v2","platform":"unix","device":1,"inode":2,"changedSecs":3,"changedNanos":4}"#;
    #[cfg(windows)]
    let document = r#"{"schemaVersion":2,"address":"ipc://c3-cli-bad","protocol":"legacy-v1","platform":"windows"}"#;
    std::fs::write(&path, document).unwrap();
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
    let address = unique_address("absent");
    let assert = c3()
        .args(["endpoint", "inspect", &address])
        .assert()
        .success();
    #[cfg(unix)]
    assert.stdout(predicate::str::contains(r#""status":"absent""#));
    #[cfg(windows)]
    assert
        .stdout(predicate::str::contains(r#""status":"not-applicable""#))
        .stdout(predicate::str::contains("kernel-managed"));
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
    c3().args([
        "endpoint",
        "inspect",
        "ipc://c3-cli-x",
        "--protocol",
        "managed-v3",
    ])
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
    let address = unique_address("other");
    let target = unique_address("target");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.json");
    #[cfg(unix)]
    let document = serde_json::json!({"schemaVersion":2,"address":address,"protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4});
    #[cfg(windows)]
    let document = serde_json::json!({"schemaVersion":1,"address":address,"protocol":"legacy-v1","platform":"windows"});
    std::fs::write(&path, document.to_string()).unwrap();
    c3().args(["endpoint", "reap", &target])
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
    #[cfg(unix)]
    prepare_legacy_namespace();
    let address = unique_address("sweep");
    let assert = c3()
        .args([
            "endpoint",
            "sweep",
            "--protocol",
            "legacy-v1",
            "--address",
            &address,
            "--max-entries",
            "4096",
            "--max-ms",
            "1000",
            "--max-batches",
            "4096",
        ])
        .assert()
        .success();
    let report: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["sweep"]["roundComplete"], true);
    assert_eq!(report["sweep"]["roundInterrupted"], false);
    assert_eq!(report["sweep"]["endpointsExamined"], 0);
    assert_eq!(report["sweep"]["reaped"], 0);
    #[cfg(windows)]
    assert_eq!(report["sweep"]["notApplicable"], 1);
    #[cfg(unix)]
    {
        let selected = unique_address("sweep-incomplete");
        let other = unique_address("sweep-extra-entry");
        let selected_socket = TestSocket::bind(&selected);
        let other_socket = TestSocket::bind(&other);
        let limited = c3()
            .args([
                "endpoint",
                "sweep",
                "--protocol",
                "legacy-v1",
                "--address",
                &selected,
                "--max-entries",
                "1",
                "--max-batches",
                "1",
            ])
            .assert()
            .code(1);
        let report: serde_json::Value =
            serde_json::from_slice(&limited.get_output().stdout).unwrap();
        assert_eq!(report["status"], "batch-limit");
        assert_eq!(report["reason"], "max-batches-reached");
        assert_eq!(report["sweep"]["roundComplete"], false);
        assert_eq!(report["sweep"]["roundInterrupted"], false);
        selected_socket.assert_reachable();
        other_socket.assert_reachable();
    }
}

/// A credential path that is not a regular file must be rejected before any
/// read. A FIFO would otherwise block the open until a writer appeared.
#[test]
fn reap_rejects_a_non_regular_credential_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
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
    }

    // A directory is not a credential file on Windows or Unix.
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

    let address = unique_address("policy");
    #[cfg(unix)]
    {
        let mut managed = c3();
        managed
            .env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
            .args(["endpoint", "inspect", &address]);
        assert_eq!(inspect_when_gate_available(managed)["status"], "absent");
    }
    #[cfg(windows)]
    c3().env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
        .args(["endpoint", "inspect", &address])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not supported on Windows"));

    let explicit = c3()
        .env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
        .args(["endpoint", "inspect", &address, "--protocol", "legacy-v1"])
        .assert()
        .success();
    #[cfg(unix)]
    explicit.stdout(predicate::str::contains(r#""status":"absent""#));
    #[cfg(windows)]
    explicit.stdout(predicate::str::contains("kernel-managed"));
}

/// `reap` derives the address with the protocol the credential records, so a
/// managed credential is not misread as stale under a legacy process policy.
#[test]
fn reap_derives_the_endpoint_from_the_credential_protocol() {
    #[cfg(unix)]
    {
        use c2_config::{LocalEndpointProtocol, ServerIpcConfigOverrides};
        use c2_core::{
            EndpointInspection, HostOptions, Runtime, RuntimeOptions, inspect_endpoint,
            ping_direct_ipc_with_protocol,
        };
        let address = unique_address("derive");
        let mut overrides = ServerIpcConfigOverrides::default();
        overrides.base.endpoint_protocol = Some(LocalEndpointProtocol::ManagedV2);
        let runtime = Runtime::new(RuntimeOptions {
            server_id: Some(address.strip_prefix("ipc://").unwrap().to_string()),
            server_ipc_overrides: Some(overrides),
            use_process_relay_anchor: false,
            ..RuntimeOptions::default()
        })
        .unwrap();
        // host() waits for native readiness; no global gate is held by this test.
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let endpoint = c2_core::LocalEndpoint::from_address_with_protocol(
            &address,
            LocalEndpointProtocol::ManagedV2,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let credential = loop {
            match inspect_endpoint(&endpoint) {
                EndpointInspection::Present(credential) => break credential,
                EndpointInspection::IoError(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "namespace gate stayed busy"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                other => panic!("ready listener credential observation failed: {other:?}"),
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed.json");
        std::fs::write(&path, credential.to_json().unwrap()).unwrap();
        c3().env("C2_IPC_ENDPOINT_PROTOCOL", "legacy-v1")
            .args(["endpoint", "reap", &address])
            .arg("--credential")
            .arg(&path)
            .assert()
            .code(1)
            .stdout(predicate::str::contains(r#""status":"busy""#))
            .stdout(predicate::str::contains(r#""reason":"coordinator-held""#));
        assert!(
            ping_direct_ipc_with_protocol(
                &address,
                LocalEndpointProtocol::ManagedV2,
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
        let shutdown = host.shutdown();
        assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    }
    #[cfg(windows)]
    {
        let address = unique_address("derive");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.json");
        // Only legacy-v1 kernel credentials are representable on Windows.
        let document = serde_json::json!({"schemaVersion":1,"address":address,"protocol":"legacy-v1","platform":"windows"});
        let credential = c2_core::EndpointCredential::from_json(&document.to_string()).unwrap();
        std::fs::write(&path, credential.to_json().unwrap()).unwrap();
        c3().env("C2_IPC_ENDPOINT_PROTOCOL", "managed-v2")
            .args(["endpoint", "reap", &address])
            .arg("--credential")
            .arg(&path)
            .assert()
            .code(1)
            .stdout(predicate::str::contains(r#""status":"not-applicable""#))
            .stdout(predicate::str::contains("no-filesystem-entry"));
        let unsupported = serde_json::json!({"schemaVersion":2,"address":address,"protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4});
        std::fs::write(&path, unsupported.to_string()).unwrap();
        c3().args(["endpoint", "reap", &address])
            .arg("--credential")
            .arg(&path)
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid-value"));
    }
}

#[test]
fn sweep_rejects_invalid_and_oversized_address_scopes() {
    for address in ["tcp://host", "ipc://..", "ipc://x/y", "/tmp/c_two_ipc"] {
        c3().args([
            "endpoint",
            "sweep",
            "--protocol",
            "legacy-v1",
            "--address",
            address,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid endpoint sweep scope"));
    }
    let address = unique_address("scope-limit");
    #[cfg(unix)]
    {
        let mut command = c3();
        command.args(["endpoint", "sweep", "--protocol", "legacy-v1"]);
        for _ in 0..4097 {
            command.args(["--address", &address]);
        }
        command.assert().failure().stderr(predicate::str::contains(
            "too many endpoint sweep addresses",
        ));
    }
    #[cfg(windows)]
    {
        // 4097 repeated flags exceed Windows' process command-line limit.
        // Exercise the same native pre-lease validator directly on that branch;
        // the invalid-address cases above still invoke the actual CLI.
        let endpoint = c2_core::LocalEndpoint::from_address_with_protocol(
            &address,
            c2_config::LocalEndpointProtocol::LegacyV1,
        )
        .unwrap();
        let error =
            match c2_core::EndpointSweep::scope_for_addresses(&endpoint, &vec![address; 4097]) {
                Ok(_) => panic!("oversized Windows scope must be rejected"),
                Err(error) => error,
            };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "too many endpoint sweep addresses");
    }
}

#[test]
fn sweep_accepts_repeated_logical_targets_without_touching_unselected_entries() {
    let selected = unique_address("selected");
    let other = unique_address("unselected");
    #[cfg(unix)]
    let selected_socket = TestSocket::bind(&selected);
    #[cfg(unix)]
    let unselected_socket = TestSocket::bind(&other);
    let second = unique_address("second");
    let assert = c3()
        .args([
            "endpoint",
            "sweep",
            "--protocol",
            "legacy-v1",
            "--address",
            &selected,
            "--address",
            &second,
            "--max-entries",
            "4096",
            "--max-ms",
            "1000",
            "--max-batches",
            "4096",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""reaped":0"#));
    #[cfg(unix)]
    {
        // Exactly the selected raw socket is examined and honestly refused:
        // it has no native ownership record. A global scan or ignored scope
        // would examine the unselected socket too, or examine neither.
        assert
            .stdout(predicate::str::contains(r#""endpointsExamined":1"#))
            .stdout(predicate::str::contains(r#""unverified":1"#));
        selected_socket.assert_reachable();
        unselected_socket.assert_reachable();
    }
    #[cfg(windows)]
    {
        assert.stdout(predicate::str::contains(r#""notApplicable":1"#));
        // A Windows scope still validates addresses; it has no filesystem work.
        c3().args([
            "endpoint",
            "sweep",
            "--protocol",
            "managed-v2",
            "--address",
            &other,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not supported on Windows"));
    }
}
