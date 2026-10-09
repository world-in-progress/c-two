//! Focused `c3 endpoint` tests: bounded arguments, honest status, and real
//! failure exit codes.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
fn inspect_report(mut command: Command) -> serde_json::Value {
    let output = command.output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report:?}");
    report
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
    fn bind(namespace: &TestNamespace, address: &str) -> Self {
        let endpoint = namespace.context.endpoint(address).unwrap();
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
    command.env("C2_ENV_FILE", "");
    command.env_remove("C2_IPC_ROOT");
    command
}

#[cfg(unix)]
fn short_root() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new()
        .prefix("c3r-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

#[cfg(unix)]
fn missing_short_root() -> PathBuf {
    let root = short_root();
    let missing = root.path().to_owned();
    root.close().unwrap();
    missing
}

#[cfg(unix)]
#[test]
fn short_fixture_roots_fit_native_endpoint_capacity() {
    let ready = short_root();
    let missing = missing_short_root();
    for root in [ready.path(), missing.as_path()] {
        let context = c2_config::LocalEndpointContext::with_unix_root(root).unwrap();
        context.endpoint(&unique_address("capacity")).unwrap();
        context.endpoint("ipc://c3-endpoint-sweep").unwrap();
    }
    assert!(!missing.exists(), "pure derivation must not create a root");
}

#[cfg(unix)]
#[test]
fn long_final_directory_supports_cli_credentials_and_exact_maintenance() {
    use std::os::unix::fs::DirBuilderExt;
    let outer = short_root();
    let segment = format!("目录 with spaces {}", "x".repeat(110));
    let root = (0..4).fold(outer.path().to_owned(), |path, _| path.join(&segment));
    assert!(root.as_os_str().len() >= 512);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .unwrap();
    let sentinel = root.join("application-data");
    std::fs::write(&sentinel, b"preserve").unwrap();
    let address = unique_address("long-root");
    let host = host_at(&address, &root);
    let document = credential_at(&address, &root);
    assert_eq!(document["unixRoot"], root.to_str().unwrap());
    assert_eq!(document["schemaVersion"], 3);
    let file = root.join("credential.json");
    std::fs::write(&file, document.to_string()).unwrap();
    c3().args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("busy"));
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(&root)
        .unwrap()
        .endpoint(&address)
        .unwrap();
    assert_eq!(
        std::path::Path::new(endpoint.os_name()).parent(),
        Some(root.as_path())
    );
    assert!(host.shutdown().runtime_barrier_error.is_none());
    c3().args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("already-absent"));
    c3().args(["endpoint", "sweep", "--ipc-root"])
        .arg(&root)
        .arg("--address")
        .arg(&address)
        .assert()
        .success();
    assert!(!std::path::Path::new(endpoint.os_name()).exists());
    assert!(root.is_dir());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
}

/// One verified namespace per real Unix fixture: sibling targets share this
/// domain, while parallel tests never contend on the default coordinator.
#[cfg(unix)]
struct TestNamespace {
    root: tempfile::TempDir,
    context: c2_config::LocalEndpointContext,
}

#[cfg(unix)]
impl TestNamespace {
    fn new() -> Self {
        let root = short_root();
        let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
        let host = host_at(&unique_address("namespace-setup"), root.path());
        let outcome = host.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        Self { root, context }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = c3();
        command.args(args).arg("--ipc-root").arg(self.root.path());
        command
    }
}

#[cfg(unix)]
fn host_at(address: &str, root: &std::path::Path) -> c2_core::Host {
    let runtime = c2_core::Runtime::new(c2_core::RuntimeOptions {
        server_id: Some(address.strip_prefix("ipc://").unwrap().to_owned()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_local_endpoint(c2_config::LocalEndpointOptions {
            unix_root: Some(root.to_owned()),
        })
        .unwrap();
    runtime
        .host(c2_core::HostOptions::default().without_relay())
        .unwrap()
}

#[cfg(unix)]
fn credential_at(address: &str, root: &std::path::Path) -> serde_json::Value {
    let mut command = c3();
    command
        .args(["endpoint", "inspect", address])
        .arg("--ipc-root")
        .arg(root);
    inspect_report(command)["credential"].clone()
}

#[test]
fn endpoint_help_exposes_root_on_each_command() {
    for command in ["inspect", "reap", "sweep"] {
        c3().args(["endpoint", command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--ipc-root"));
    }
}

#[cfg(unix)]
#[test]
fn inspect_root_precedence_and_sweep_use_the_selected_domain() {
    let alpha = short_root();
    let beta = short_root();
    let address = unique_address("domains");
    let host = host_at(&address, alpha.path());
    let beta_host = host_at(&unique_address("beta-setup"), beta.path());
    assert!(beta_host.shutdown().runtime_barrier_error.is_none());
    let env_file = beta.path().join("endpoint.env");
    std::fs::write(
        &env_file,
        format!("C2_IPC_ROOT={}\n", beta.path().display()),
    )
    .unwrap();
    let mut from_file = c3();
    from_file
        .env("C2_ENV_FILE", &env_file)
        .args(["endpoint", "inspect", &address]);
    assert_eq!(inspect_report(from_file)["status"], "absent");
    let mut from_env = c3();
    from_env
        .env("C2_ENV_FILE", &env_file)
        .env("C2_IPC_ROOT", alpha.path())
        .args(["endpoint", "inspect", &address]);
    assert_eq!(
        inspect_report(from_env)["credential"]["unixRoot"],
        alpha.path().to_str().unwrap()
    );
    let mut from_cli = c3();
    from_cli
        .env("C2_ENV_FILE", &env_file)
        .env("C2_IPC_ROOT", beta.path())
        .args(["endpoint", "inspect", &address, "--ipc-root"])
        .arg(alpha.path());
    assert_eq!(inspect_report(from_cli)["status"], "present");
    for (root, examined, busy) in [(alpha.path(), 1, 1), (beta.path(), 0, 0)] {
        let output = c3()
            .env("C2_IPC_ROOT", beta.path())
            .args(["endpoint", "sweep", "--ipc-root"])
            .arg(root)
            .args([
                "--address",
                &address,
                "--max-entries",
                "4096",
                "--max-ms",
                "1000",
            ])
            .assert()
            .success();
        let report: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(report["sweep"]["endpointsExamined"], examined);
        assert_eq!(report["sweep"]["busy"], busy);
    }
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[cfg(unix)]
#[test]
fn reap_captured_context_ignores_env_and_rejects_explicit_other_root() {
    let root = short_root();
    let address = unique_address("captured");
    let host = host_at(&address, root.path());
    let credential = credential_at(&address, root.path());
    assert_eq!(credential["schemaVersion"], 3);
    let file = root.path().join("credential.json");
    std::fs::write(&file, credential.to_string()).unwrap();
    let missing_root = root.path().join("missing");
    c3().env("C2_IPC_ROOT", "relative-invalid-env")
        .args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("coordinator-held"));
    c3().env("C2_IPC_ROOT", root.path())
        .args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .arg("--ipc-root")
        .arg(&missing_root)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("credential-context-mismatch"));
    assert!(
        !missing_root.exists(),
        "reject must not create the mismatched root"
    );
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[cfg(unix)]
#[test]
fn recorded_directory_reap_is_not_reinterpreted_by_env() {
    let address = unique_address("recorded");
    let dir = short_root();
    let file = dir.path().join("credential.json");
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let document = described_unix_credential(&address, &context);
    let credential = c2_core::EndpointCredential::from_json(&document.to_string()).unwrap();
    assert_eq!(credential.endpoint().context(), &context);
    std::fs::write(&file, document.to_string()).unwrap();
    c3().env("C2_IPC_ROOT", "relative-invalid-env")
        .args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("already-absent"));
}

#[cfg(unix)]
fn described_unix_credential(
    address: &str,
    context: &c2_config::LocalEndpointContext,
) -> serde_json::Value {
    serde_json::json!({"schemaVersion":3,"address":address,"protocol":"managed-v2","platform":"unix","unixRoot":context.unix_root().unwrap(),"namespaceId":context.namespace_id(),"incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4})
}

#[cfg(unix)]
#[test]
fn missing_custom_root_is_never_created_by_inspection_or_sweep() {
    let missing = missing_short_root();
    c3().args([
        "endpoint",
        "inspect",
        &unique_address("missing"),
        "--ipc-root",
    ])
    .arg(&missing)
    .assert()
    .success()
    .stdout(predicate::str::contains("absent"));
    c3().args(["endpoint", "sweep", "--ipc-root"])
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot open"));
    assert!(!missing.exists());
}

#[cfg(unix)]
#[test]
fn root_maintenance_preserves_unrelated_directories_and_rejects_replaced_gate_nonce() {
    let root = short_root();
    let address = unique_address("nonce");
    let host = host_at(&address, root.path());
    let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
    let endpoint = context.endpoint(&address).unwrap();
    let namespace = std::path::Path::new(endpoint.os_name()).parent().unwrap();
    let document = credential_at(&address, root.path());
    let file = root.path().join("credential.json");
    std::fs::write(&file, document.to_string()).unwrap();
    assert!(host.shutdown().runtime_barrier_error.is_none());

    // Maintenance does not recurse into application-owned directories.
    let other_namespace = root.path().join("application-data");
    std::fs::create_dir_all(&other_namespace).unwrap();
    let sentinel = other_namespace.join("do-not-adopt");
    std::fs::write(&sentinel, b"unrelated uid namespace").unwrap();
    c3().args(["endpoint", "sweep", "--ipc-root"])
        .arg(root.path())
        .assert()
        .success();
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"unrelated uid namespace"
    );

    let gate = namespace.join(".gate");
    let marker = namespace.join(".gate.marker");
    let before_marker = std::fs::read(&marker).unwrap();
    let mut replaced_gate = std::fs::read(&gate).unwrap();
    assert_eq!(replaced_gate.len(), 25, "format-2 gate has a nonce");
    replaced_gate[9] ^= 1;
    std::fs::write(&gate, &replaced_gate).unwrap();
    c3().args(["endpoint", "inspect", &address, "--ipc-root"])
        .arg(root.path())
        .assert()
        .code(1)
        .stdout(predicate::str::contains("coordinator-replaced"));
    c3().args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("coordinator-replaced"));
    let report = c3()
        .args(["endpoint", "sweep", "--ipc-root"])
        .arg(root.path())
        .assert()
        .success();
    let report: serde_json::Value = serde_json::from_slice(&report.get_output().stdout).unwrap();
    assert_eq!(report["sweep"]["reaped"], 0);
    assert_eq!(std::fs::read(&gate).unwrap(), replaced_gate);
    assert_eq!(std::fs::read(&marker).unwrap(), before_marker);
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"unrelated uid namespace"
    );
}

#[cfg(windows)]
#[test]
fn windows_root_override_is_explicitly_not_applicable() {
    for command in ["inspect", "sweep"] {
        let mut cmd = c3();
        cmd.args(["endpoint", command]);
        if command == "inspect" {
            cmd.arg(unique_address("windows-root"));
        }
        cmd.args(["--ipc-root", r"C:\c2-root"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("not applicable"));
    }
    let address = unique_address("windows-reap-root");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credential.json");
    let document = serde_json::json!({"schemaVersion":1,"address":address,"protocol":"named-pipe","platform":"windows"});
    std::fs::write(&file, document.to_string()).unwrap();
    c3().args(["endpoint", "reap", &address, "--credential"])
        .arg(&file)
        .args(["--ipc-root", r"C:\c2-root"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not applicable"));
}

/// A current-format document missing its required incarnation (Unix), or
/// incompatible Unix format metadata on a kernel-managed pipe (Windows).
fn invalid_credential() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.json");
    #[cfg(unix)]
    let document = {
        let mut value = described_unix_credential(
            "ipc://c3-cli-bad",
            &c2_config::LocalEndpointContext::default_for_platform().unwrap(),
        );
        value.as_object_mut().unwrap().remove("incarnation");
        value.to_string()
    };
    #[cfg(windows)]
    let document = r#"{"schemaVersion":3,"address":"ipc://c3-cli-bad","protocol":"named-pipe","platform":"windows"}"#;
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
    let mut command = c3();
    command.args(["endpoint", "inspect", &address]);
    #[cfg(unix)]
    let namespace = TestNamespace::new();
    #[cfg(unix)]
    command.arg("--ipc-root").arg(namespace.root.path());
    let assert = command.assert().success();
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
fn sweep_rejects_zero_and_unbounded_budgets() {
    for args in [
        vec!["--max-entries", "0"],
        vec!["--max-entries", "100000"],
        vec!["--max-ms", "0"],
        vec!["--max-ms", "60000"],
        vec!["--max-batches", "0"],
        vec!["--max-batches", "100000"],
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
    let document = described_unix_credential(
        &address,
        &c2_config::LocalEndpointContext::default_for_platform().unwrap(),
    );
    #[cfg(windows)]
    let document = serde_json::json!({"schemaVersion":1,"address":address,"protocol":"named-pipe","platform":"windows"});
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
    std::fs::write(
        &path,
        "x".repeat(c2_core::ENDPOINT_CREDENTIAL_MAX_BYTES + 1),
    )
    .unwrap();
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
    let namespace = TestNamespace::new();
    let address = unique_address("sweep");
    let mut command = c3();
    command.args([
        "endpoint",
        "sweep",
        "--address",
        &address,
        "--max-entries",
        "4096",
        "--max-ms",
        "1000",
        "--max-batches",
        "4096",
    ]);
    #[cfg(unix)]
    command.arg("--ipc-root").arg(namespace.root.path());
    let assert = command.assert().success();
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
        let selected_socket = TestSocket::bind(&namespace, &selected);
        let other_socket = TestSocket::bind(&namespace, &other);
        let limited = namespace
            .command(&[
                "endpoint",
                "sweep",
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

/// `reap` validates the credential against the sole native endpoint derived
/// from its logical address; format metadata cannot select another backend.
#[test]
fn reap_uses_the_native_credential_endpoint() {
    #[cfg(unix)]
    {
        use c2_core::{EndpointInspection, inspect_endpoint, ping_direct_ipc_with_context};
        let root = short_root();
        let address = unique_address("derive");
        let host = host_at(&address, root.path());
        let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
        let endpoint = context.endpoint(&address).unwrap();
        let credential = match inspect_endpoint(&endpoint) {
            EndpointInspection::Present(credential) => credential,
            other => panic!("ready listener credential observation failed: {other:?}"),
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
            ping_direct_ipc_with_context(&address, &context, std::time::Duration::from_secs(1))
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
        // Named Pipe credentials carry kernel namespace metadata on Windows.
        let document = serde_json::json!({"schemaVersion":1,"address":address,"protocol":"named-pipe","platform":"windows"});
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
        let unsupported = serde_json::json!({"schemaVersion":3,"address":address,"protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4});
        std::fs::write(&path, unsupported.to_string()).unwrap();
        c3().args(["endpoint", "reap", &address])
            .arg("--credential")
            .arg(&path)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unsupported-platform"));
    }
}

#[test]
fn sweep_rejects_invalid_and_oversized_address_scopes() {
    for address in ["tcp://host", "ipc://..", "ipc://x/y", "/tmp/c_two_ipc"] {
        c3().args(["endpoint", "sweep", "--address", address])
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid endpoint sweep scope"));
    }
    let address = unique_address("scope-limit");
    #[cfg(unix)]
    {
        let mut command = c3();
        command.args(["endpoint", "sweep"]);
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
        let endpoint = c2_core::LocalEndpoint::from_address(&address).unwrap();
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
    let namespace = TestNamespace::new();
    #[cfg(unix)]
    let selected_socket = TestSocket::bind(&namespace, &selected);
    #[cfg(unix)]
    let unselected_socket = TestSocket::bind(&namespace, &other);
    let second = unique_address("second");
    let mut command = c3();
    command.args([
        "endpoint",
        "sweep",
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
    ]);
    #[cfg(unix)]
    command.arg("--ipc-root").arg(namespace.root.path());
    let assert = command
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
    }
}
