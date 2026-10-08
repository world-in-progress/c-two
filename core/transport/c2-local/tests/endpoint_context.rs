//! Public native context tests. Keep roots short enough for macOS sun_path;
//! endpoint derivation must never be bypassed with an arbitrary socket path.
#![cfg(unix)]

use c2_config::{ConfigResolver, ConfigSources, EnvFilePolicy, LocalEndpointOptions};
use c2_local::{
    DEFAULT_CONNECT_TIMEOUT, EndpointCredential, EndpointInspection, EndpointReapResult,
    EndpointSweep, LocalEndpoint, LocalEndpointContext, LocalListener, LocalStream, SweepBudget,
    inspect_endpoint, reap_endpoint,
};
use std::io::{self, BufRead};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ACTION: &str = "C2_LOCAL_CONTEXT_FIXTURE";
const ROOT: &str = "C2_LOCAL_CONTEXT_TEST_ROOT";
const OTHER_ROOT: &str = "C2_LOCAL_CONTEXT_OTHER_ROOT";
const ADDRESS: &str = "ipc://context-same-address";

fn short_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("c2c-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn context(root: &Path) -> LocalEndpointContext {
    LocalEndpointContext::with_unix_root(root).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn retry_inspect(endpoint: &LocalEndpoint) -> EndpointCredential {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match inspect_endpoint(endpoint) {
            EndpointInspection::Present(credential) => return credential,
            EndpointInspection::IoError(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            other => panic!("expected a native credential: {other:?}"),
        }
    }
}

fn retry_reap(endpoint: &LocalEndpoint, credential: &EndpointCredential) -> EndpointReapResult {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match reap_endpoint(endpoint, credential) {
            EndpointReapResult::Busy => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn finish_sweep(sweep: &mut EndpointSweep) -> usize {
    let mut reaped = 0;
    for _ in 0..64 {
        let batch = sweep.next_batch(SweepBudget {
            max_entries: 1,
            max_duration: Duration::ZERO,
        });
        assert_eq!(batch.io_errors, 0, "{batch:?}");
        assert_eq!(batch.unverified, 0, "{batch:?}");
        assert!(!batch.round_interrupted, "{batch:?}");
        assert!(!batch.namespace_changed, "{batch:?}");
        reaped += batch.reaped;
        if batch.round_complete {
            return reaped;
        }
    }
    panic!("private namespace never reached EOF");
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture_command(action: &str, root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "endpoint_context_child_fixture", "--nocapture"])
        .env(ACTION, action)
        .env(ROOT, root)
        .stdin(Stdio::null());
    command
}

fn holder(root: &Path) -> (ChildGuard, EndpointCredential) {
    let mut child = ChildGuard(
        fixture_command("hold", root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        for line in io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(json) = line.strip_prefix("CREDENTIAL:") {
                tx.send(EndpointCredential::from_json(json).unwrap())
                    .unwrap();
                return;
            }
        }
    });
    let credential = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("child readiness deadline");
    reader.join().unwrap();
    (child, credential)
}

fn kill_and_wait(child: &mut ChildGuard) {
    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

#[test]
fn endpoint_context_child_fixture() {
    let Ok(action) = std::env::var(ACTION) else {
        return;
    };
    let root = std::env::var(ROOT).unwrap();
    if action == "codec-environment" {
        // This exact fixture is the only test in an isolated child. No runtime
        // or other environment-reading application threads have been started.
        let sources = || ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: std::env::vars().collect(),
        };
        let captured =
            ConfigResolver::resolve_local_endpoint(LocalEndpointOptions::default(), sources())
                .unwrap();
        assert_eq!(captured.unix_root(), Some(Path::new(&root)));
        let endpoint = captured.endpoint(ADDRESS).unwrap();
        let json = format!(
            r#"{{"schemaVersion":3,"address":"{ADDRESS}","protocol":"managed-v2","platform":"unix","unixRoot":"{root}","namespaceId":"{}","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4}}"#,
            captured.namespace_id()
        );
        let default = LocalEndpointContext::default_for_platform().unwrap();
        let default_json = r#"{"schemaVersion":2,"address":"ipc://context-default","protocol":"managed-v2","platform":"unix","incarnation":"00112233445566778899aabbccddeeff","device":1,"inode":2,"changedSecs":3,"changedNanos":4}"#;
        let other = std::env::var(OTHER_ROOT).unwrap();
        // SAFETY: this isolated one-test child has no application workers,
        // reactors, or concurrent readers of the process environment.
        unsafe {
            std::env::set_var("C2_IPC_ROOT", &other);
        }
        assert_eq!(
            ConfigResolver::resolve_local_endpoint(LocalEndpointOptions::default(), sources())
                .unwrap(),
            context(Path::new(&other))
        );
        assert_eq!(captured.endpoint(ADDRESS).unwrap(), endpoint);
        assert_eq!(
            EndpointCredential::from_json(&json).unwrap().endpoint(),
            &endpoint
        );
        assert_eq!(
            EndpointCredential::from_json(default_json)
                .unwrap()
                .endpoint()
                .context(),
            &default
        );
        // Scope derivation must remain pure and use the old context.
        let scope = EndpointSweep::scope_for_addresses(&endpoint, &[ADDRESS.to_owned()]).unwrap();
        // A nonexistent old namespace stays nonexistent, even if the new env
        // root has a different namespace. Opening never falls back to it.
        assert!(EndpointSweep::for_scope(&scope).is_err());
        assert!(!Path::new(endpoint.os_name()).exists());
        println!("CODEC_ENVIRONMENT_PASSED");
        return;
    }
    runtime().block_on(async {
        let captured = context(Path::new(&root));
        let endpoint = captured.endpoint(ADDRESS).unwrap();
        match action.as_str() {
            "hold" => {
                let listener = LocalListener::bind(&endpoint).unwrap();
                println!("CREDENTIAL:{}", listener.credential().to_json().unwrap());
                use std::io::Write;
                io::stdout().flush().unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            "sweep-environment" => {
                let other = std::env::var(OTHER_ROOT).unwrap();
                let other_endpoint = context(Path::new(&other)).endpoint(ADDRESS).unwrap();
                let other_listener = LocalListener::bind(&other_endpoint).unwrap();
                let (mut child, credential) = holder(Path::new(&root));
                assert_eq!(credential.endpoint(), &endpoint);
                let mut sweep = EndpointSweep::for_context(&captured).unwrap();
                kill_and_wait(&mut child);
                // The child is waited; binds have finished their short-lived
                // descriptor helpers, and this current-thread runtime has no
                // worker threads or concurrent environment readers.
                unsafe {
                    std::env::set_var("C2_IPC_ROOT", &other);
                }
                let scope =
                    EndpointSweep::scope_for_addresses(&endpoint, &[ADDRESS.to_owned()]).unwrap();
                assert_eq!(
                    EndpointCredential::from_json(&credential.to_json().unwrap()).unwrap(),
                    credential
                );
                assert_eq!(finish_sweep(&mut sweep), 1);
                assert!(matches!(
                    inspect_endpoint(&endpoint),
                    EndpointInspection::Absent
                ));
                assert_eq!(retry_inspect(&other_endpoint), other_listener.credential());
                // Open a second scoped round after the environment changed.
                let mut sweep = EndpointSweep::for_scope(&scope).unwrap();
                assert_eq!(finish_sweep(&mut sweep), 0);
                assert!(matches!(other_listener.close(), EndpointReapResult::Reaped));
                println!("SWEEP_ENVIRONMENT_PASSED");
            }
            unknown => panic!("unknown context fixture: {unknown}"),
        }
    });
}

#[tokio::test]
async fn same_address_in_two_roots_has_independent_listeners_and_credentials() {
    let a = short_root();
    let b = short_root();
    let ea = context(a.path()).endpoint(ADDRESS).unwrap();
    let eb = context(b.path()).endpoint(ADDRESS).unwrap();
    assert_ne!(ea.context().namespace_id(), eb.context().namespace_id());
    let mut la = LocalListener::bind(&ea).unwrap();
    let mut lb = LocalListener::bind(&eb).unwrap();
    for (endpoint, listener, message) in [(&ea, &mut la, b'a'), (&eb, &mut lb, b'b')] {
        let (mut client, mut server) = tokio::try_join!(
            LocalStream::connect(endpoint, DEFAULT_CONNECT_TIMEOUT),
            listener.accept()
        )
        .unwrap();
        server.write_all(&[message]).await.unwrap();
        use tokio::io::AsyncReadExt;
        let mut byte = [0];
        client.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [message]);
        drop(client);
        drop(server);
    }
    let ca = EndpointCredential::from_json(&la.credential().to_json().unwrap()).unwrap();
    assert_eq!(retry_inspect(&ea), ca);
    assert_eq!(retry_inspect(&eb), lb.credential());
    assert!(matches!(
        reap_endpoint(&eb, &ca),
        EndpointReapResult::StaleTarget
    ));
    assert!(matches!(la.close(), EndpointReapResult::Reaped));
    assert_eq!(retry_inspect(&eb), lb.credential());
    assert!(matches!(lb.close(), EndpointReapResult::Reaped));
}

#[test]
fn custom_root_kill_wait_and_old_credential_preserve_new_incarnation() {
    let root = short_root();
    let endpoint = context(root.path()).endpoint(ADDRESS).unwrap();
    let (mut first, old) = holder(root.path());
    assert!(matches!(
        reap_endpoint(&endpoint, &old),
        EndpointReapResult::Busy
    ));
    kill_and_wait(&mut first);
    assert_eq!(retry_inspect(&endpoint), old);
    assert!(matches!(
        retry_reap(&endpoint, &old),
        EndpointReapResult::Reaped
    ));
    let (mut second, current) = holder(root.path());
    assert_ne!(old.incarnation(), current.incarnation());
    kill_and_wait(&mut second);
    assert!(matches!(
        retry_reap(&endpoint, &old),
        EndpointReapResult::StaleTarget
    ));
    assert_eq!(retry_inspect(&endpoint), current);
    assert!(matches!(
        retry_reap(&endpoint, &current),
        EndpointReapResult::Reaped
    ));
    assert!(matches!(
        retry_reap(&endpoint, &current),
        EndpointReapResult::AlreadyAbsent
    ));
}

#[test]
fn captured_codec_context_is_stable_after_environment_change() {
    let a = short_root();
    let b = short_root();
    let output = fixture_command("codec-environment", a.path())
        .env("C2_IPC_ROOT", a.path())
        .env(OTHER_ROOT, b.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("CODEC_ENVIRONMENT_PASSED"));
}

#[test]
fn captured_open_and_scoped_sweeps_are_stable_after_environment_change() {
    let a = short_root();
    let b = short_root();
    let output = fixture_command("sweep-environment", a.path())
        .env("C2_IPC_ROOT", a.path())
        .env(OTHER_ROOT, b.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("SWEEP_ENVIRONMENT_PASSED"));
}
