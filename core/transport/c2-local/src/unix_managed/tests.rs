//! Managed-v2 protocol tests: real processes, real public paths, and the
//! conservative boundaries of the gate and owner record.

use super::*;
use crate::{SweepBatch, SweepBudget};
use std::fs::OpenOptions;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::{Child, Command};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// An isolated, application-provisioned final private directory.
struct TestNamespace {
    _parent: tempfile::TempDir,
    root: PathBuf,
}

impl TestNamespace {
    fn new() -> Self {
        let parent = tempfile::Builder::new()
            .prefix("c2m-")
            .tempdir_in("/tmp")
            .expect("isolated managed parent under /tmp");
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = namespace_path(parent.path());
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _parent: parent,
            root,
        }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

fn managed_endpoint(label: &str) -> LocalEndpoint {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    LocalEndpoint::from_address(&format!("ipc://{label}-{}", &unique[..16])).unwrap()
}

// Each fixture selects and provisions its final endpoint directory.
fn namespace_path(container: &Path) -> PathBuf {
    container.join("ipc")
}

fn context_for_namespace(root: &Path) -> LocalEndpointContext {
    LocalEndpointContext::with_unix_root(root).unwrap()
}

fn managed_endpoint_at(root: &Path, label: &str) -> LocalEndpoint {
    context_for_namespace(root)
        .endpoint(&format!("ipc://{label}-{}", uuid::Uuid::new_v4().simple()))
        .unwrap()
}

fn sweep_at(root: &Path) -> io::Result<ManagedSweep> {
    ManagedSweep::for_endpoint(&context_for_namespace(root).endpoint("ipc://sweep-fixture")?)
}

fn scoped_sweep_at(root: &Path, targets: &[LocalEndpoint]) -> io::Result<ManagedSweep> {
    ManagedSweep::for_scope(
        &context_for_namespace(root).endpoint("ipc://sweep-fixture")?,
        targets,
    )
}

#[test]
fn context_mismatch_is_refused_before_any_namespace_access() {
    let a = TestNamespace::new();
    let b = TestNamespace::new();
    let ea = managed_endpoint_at(a.path(), "context-mismatch");
    let eb = context_for_namespace(b.path())
        .endpoint(ea.address())
        .unwrap();
    let credential = EndpointCredential::unix_managed(
        ea.clone(),
        SocketIdentity {
            device: 1,
            inode: 2,
            changed_secs: 3,
            changed_nanos: 4,
        },
        [0x5a; 16],
    );
    // If either path were opened, an absent namespace would report absence or
    // IO instead. Neither root is initialized and neither may be created.
    assert!(matches!(
        crate::reap_endpoint(&eb, &credential),
        EndpointReapResult::StaleTarget
    ));
    assert!(matches!(
        reap_managed_at(&ea, &credential, b.path()),
        EndpointReapResult::StaleTarget
    ));
    assert_eq!(
        bind_managed_at(&ea, b.path()).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(matches!(
        inspect_managed_at(&ea, b.path()),
        EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch)
    ));
    assert!(ManagedSweep::for_scope(&ea, &[eb]).is_err());
    assert!(!a.path().join(GATE_NAME).exists());
    assert!(!b.path().join(GATE_NAME).exists());
}

fn socket_path(endpoint: &LocalEndpoint) -> PathBuf {
    PathBuf::from(endpoint.os_name())
}

fn slot_in(root: &Path, endpoint: &LocalEndpoint, suffix: &str) -> PathBuf {
    let stem = endpoint_stem(endpoint).unwrap();
    let mut name = stem.into_vec();
    name.extend_from_slice(suffix.as_bytes());
    root.join(OsString::from_vec(name))
}

fn socket_path_at(root: &Path, endpoint: &LocalEndpoint) -> PathBuf {
    slot_in(root, endpoint, "")
}

fn lease_path_at(root: &Path, endpoint: &LocalEndpoint) -> PathBuf {
    slot_in(root, endpoint, LEASE_SUFFIX)
}

fn lease_path(endpoint: &LocalEndpoint) -> PathBuf {
    let names = managed_names(endpoint).unwrap();
    PathBuf::from(endpoint.os_name())
        .parent()
        .unwrap()
        .join(OsStr::from_bytes(names.lock.as_bytes()))
}

fn identity_of(path: &Path) -> SocketIdentity {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_secs: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

fn managed_credential(
    endpoint: &LocalEndpoint,
    path: &Path,
    incarnation: [u8; 16],
) -> EndpointCredential {
    EndpointCredential::unix_managed(endpoint.clone(), identity_of(path), incarnation)
}

fn write_lease(path: &Path, record: &OwnerRecord) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    write_record(&file, record).unwrap();
    file.sync_all().unwrap();
}

/// The process-wide maintenance lease is shared with the v1 sweep tests that
/// run in parallel; a real caller retries on `WouldBlock` instead of failing.
fn retry_sweep<T>(mut attempt: impl FnMut() -> io::Result<T>) -> T {
    for _ in 0..400 {
        match attempt() {
            Ok(value) => return value,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("sweep open failed: {error}"),
        }
    }
    panic!("process-wide sweep lease never became available");
}

fn unreachable_portal() -> [u8; 16] {
    [0x5a; 16]
}

/// Guards the handful of tests that inspect slots in the *shared* derived
/// namespace while another test may be running an explicit maintenance sweep
/// over that same namespace. A concurrent sweep is allowed to retire a
/// registered leftover, so those tests would otherwise race a legitimate
/// retirement. Only these tests take the guard; the rest of the suite keeps
/// running in parallel.
fn shared_namespace_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Child fixture for real-process ownership and crash convergence.
#[test]
fn managed_process_fixture() {
    let Ok(action) = std::env::var("C2_LOCAL_MANAGED_TEST_ACTION") else {
        return;
    };
    let address = std::env::var("C2_LOCAL_MANAGED_TEST_ADDRESS").unwrap();
    let context = match std::env::var("C2_LOCAL_MANAGED_TEST_ROOT") {
        Ok(root) => LocalEndpointContext::with_unix_root(Path::new(&root)).unwrap(),
        Err(_) => LocalEndpointContext::default_for_platform().unwrap(),
    };
    let endpoint = context.endpoint(&address).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    match action.as_str() {
        "duplicate" => assert_eq!(
            crate::LocalListener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse,
        ),
        "exit-owner" => {
            let _listener = crate::LocalListener::bind(&endpoint).unwrap();
            // Process exit releases kernel handles and flock without Rust Drop.
            std::process::exit(0);
        }
        "hold" => {
            let _listener = crate::LocalListener::bind(&endpoint).unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "cwd-probe" => {
            // Independent subprocess negative: prove that a real managed bind
            // changes neither the default follow-process cwd nor a thread that
            // already carries its own thread-local cwd, and that the process
            // working directory itself never moves.
            #[cfg(target_os = "macos")]
            {
                let elsewhere = std::env::var("C2_LOCAL_MANAGED_TEST_CWD").unwrap();
                let elsewhere = PathBuf::from(elsewhere);
                let process_before = std::env::current_dir().unwrap();

                // Thread P: an existing thread-local cwd set via the same
                // primitive the bind thread uses. It stays alive for the whole
                // probe so its thread-local directory is observable across the
                // bind.
                let pinned_before_ready =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let pinned_observed = std::sync::Arc::new(std::sync::Mutex::new(None));
                let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                let pinned = {
                    let elsewhere = elsewhere.clone();
                    let ready = pinned_before_ready.clone();
                    let observed = pinned_observed.clone();
                    std::thread::spawn(move || {
                        let other_dir =
                            crate::unix_common::EndpointDirectory::open(&elsewhere, false).unwrap();
                        crate::unix_common::set_thread_directory(other_dir.fd()).unwrap();
                        let before = std::env::current_dir().unwrap();
                        *observed.lock().unwrap() = Some((before.clone(), before));
                        ready.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Hold the thread alive across the bind.
                        let _ = release_rx.recv();
                        let after = std::env::current_dir().unwrap();
                        let mut guard = observed.lock().unwrap();
                        let entry = guard.as_mut().unwrap();
                        entry.1 = after;
                    })
                };
                while !pinned_before_ready.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                let (pinned_before, _) = pinned_observed
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("pinned thread recorded its directory");

                // Thread F: default follow-process. It reads the process-wide
                // cwd, which must not move either.
                let follow_before = std::env::current_dir().unwrap();
                assert_ne!(
                    pinned_before, follow_before,
                    "the pinned thread must actually differ from the process cwd"
                );

                let _listener = crate::LocalListener::bind(&endpoint).unwrap();

                let follow_after = std::env::current_dir().unwrap();
                let process_after = std::env::current_dir().unwrap();
                release_tx.send(()).unwrap();
                pinned.join().unwrap();
                let (_, pinned_after) = pinned_observed
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("pinned thread recorded both readings");

                assert_eq!(follow_after, follow_before, "follow-process cwd moved");
                assert_eq!(pinned_after, pinned_before, "pinned thread cwd moved");
                assert_eq!(process_after, process_before, "process cwd moved");

                // The strongest form of the Host's probe: the *calling* thread
                // itself carries a thread-local cwd, and a real managed bind on
                // it must leave that directory exactly as it was. An
                // implementation that switched and restored the caller's thread
                // directory would be indistinguishable from a no-op only if its
                // restore were perfectly faithful; requiring that the bind never
                // touches the caller at all is what this asserts.
                let caller_dir =
                    crate::unix_common::EndpointDirectory::open(&elsewhere, false).unwrap();
                crate::unix_common::set_thread_directory(caller_dir.fd()).unwrap();
                let caller_before = std::env::current_dir().unwrap();
                let endpoint2 = context
                    .endpoint(&format!(
                        "ipc://cwd-caller-{}",
                        uuid::Uuid::new_v4().simple()
                    ))
                    .unwrap();
                let _listener2 = crate::LocalListener::bind(&endpoint2).unwrap();
                let caller_after = std::env::current_dir().unwrap();
                assert_eq!(
                    caller_after, caller_before,
                    "the calling thread's own directory moved across a managed bind"
                );

                println!("cwd-probe-ok {process_before:?} {pinned_before:?}");
            }
            #[cfg(not(target_os = "macos"))]
            {
                // On Linux the descriptor is named through /proc/self/fd, so the
                // process cwd is the only observable and must not move either.
                let before = std::env::current_dir().unwrap();
                let _listener = crate::LocalListener::bind(&endpoint).unwrap();
                assert_eq!(std::env::current_dir().unwrap(), before);
                println!("cwd-probe-ok {before:?}");
            }
        }
        _ => panic!("unknown managed test action: {action}"),
    }
}

fn run_managed_process(endpoint: &LocalEndpoint, action: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unix_managed::tests::managed_process_fixture",
            "--nocapture",
        ])
        .env("C2_LOCAL_MANAGED_TEST_ADDRESS", endpoint.address())
        .env(
            "C2_LOCAL_MANAGED_TEST_ROOT",
            endpoint.context().unix_root().unwrap(),
        )
        .env("C2_LOCAL_MANAGED_TEST_ACTION", action)
        .output()
        .unwrap()
}

fn spawn_managed_holder(endpoint: &LocalEndpoint) -> Child {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unix_managed::tests::managed_process_fixture",
            "--nocapture",
        ])
        .env("C2_LOCAL_MANAGED_TEST_ADDRESS", endpoint.address())
        .env(
            "C2_LOCAL_MANAGED_TEST_ROOT",
            endpoint.context().unix_root().unwrap(),
        )
        .env("C2_LOCAL_MANAGED_TEST_ACTION", "hold")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if socket_path(endpoint).exists() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "holder child never bound {}",
        socket_path(endpoint).display()
    );
}

/// The real public path: derived namespace, bind, accept, and retirement. Each
/// of the 120 iterations must return the namespace to gate + marker only.
#[tokio::test]
async fn managed_public_namespace_lifecycle_converges_over_repeated_runs() {
    let _shared = shared_namespace_guard();
    let guard = managed_endpoint("public-guard");
    let guard_root = socket_path(&guard).parent().unwrap().to_owned();
    for _ in 0..120 {
        let endpoint = managed_endpoint("public");
        let mut listener = crate::LocalListener::bind(&endpoint).unwrap();
        assert_eq!(
            crate::LocalListener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        let (mut client, mut server) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(
                crate::LocalStream::connect(&endpoint, crate::DEFAULT_CONNECT_TIMEOUT),
                listener.accept(),
            )
        })
        .await
        .unwrap()
        .unwrap();
        server.write_all(b"managed").await.unwrap();
        let mut bytes = [0_u8; 7];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"managed");
        assert!(matches!(listener.close(), EndpointReapResult::Reaped));
        assert!(!socket_path(&endpoint).exists());
        assert!(!lease_path(&endpoint).exists());
    }
    assert!(guard_root.join(GATE_NAME).exists());
    assert!(guard_root.join(MARKER_NAME).exists());
}

#[tokio::test]
async fn managed_concurrent_same_address_bind_has_exactly_one_winner() {
    let endpoint = managed_endpoint("race");
    let root = socket_path(&endpoint).parent().unwrap().to_owned();
    let start = std::sync::Arc::new(std::sync::Barrier::new(8));
    let runtime = tokio::runtime::Handle::current();
    let results = std::thread::scope(|scope| {
        let contenders: Vec<_> = (0..8)
            .map(|_| {
                let start = start.clone();
                let endpoint = &endpoint;
                let root = &root;
                let runtime = &runtime;
                scope.spawn(move || {
                    let _entered = runtime.enter();
                    start.wait();
                    bind_managed_at(endpoint, root)
                })
            })
            .collect();
        contenders
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut winners = Vec::new();
    for result in results {
        match result {
            Ok(listener) => winners.push(listener),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}"),
        }
    }
    assert_eq!(winners.len(), 1, "one managed endpoint must have one owner");
    let winner = winners.pop().unwrap();
    let credential = winner.credential();
    assert!(matches!(
        reap_managed_at(&endpoint, &credential, &root),
        EndpointReapResult::Busy
    ));
    assert!(matches!(winner.close(), EndpointReapResult::Reaped));
}

/// A real second process holds the address: duplicate bind is refused, and a
/// registered SIGKILL leaves socket and lease that a later reap converges.
#[tokio::test]
async fn managed_two_process_competition_and_registered_kill_converge() {
    let _shared = shared_namespace_guard();
    let endpoint = managed_endpoint("two-process");
    let mut holder = spawn_managed_holder(&endpoint);
    assert_eq!(
        crate::LocalListener::bind(&endpoint).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    let duplicate = run_managed_process(&endpoint, "duplicate");
    assert!(duplicate.status.success(), "{duplicate:?}");

    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(socket_path(&endpoint).exists());
    assert!(lease_path(&endpoint).exists());

    let EndpointInspection::Present(credential) = inspect_managed(&endpoint) else {
        panic!("killed registered owner must leave a verifiable record");
    };
    assert!(matches!(
        reap_managed(&endpoint, &credential),
        EndpointReapResult::Reaped
    ));
    assert!(!socket_path(&endpoint).exists());
    assert!(!lease_path(&endpoint).exists());
    assert!(matches!(
        reap_managed(&endpoint, &credential),
        EndpointReapResult::AlreadyAbsent
    ));
}

#[tokio::test]
async fn managed_registered_exit_without_destructors_is_reapable() {
    let _shared = shared_namespace_guard();
    let endpoint = managed_endpoint("exit-owner");
    let output = run_managed_process(&endpoint, "exit-owner");
    assert!(output.status.success(), "{output:?}");
    assert!(socket_path(&endpoint).exists());
    assert!(lease_path(&endpoint).exists());
    let EndpointInspection::Present(credential) = inspect_managed(&endpoint) else {
        panic!("registered owner exit must leave a complete record");
    };
    assert!(matches!(
        reap_managed(&endpoint, &credential),
        EndpointReapResult::Reaped
    ));
    assert!(!socket_path(&endpoint).exists());
    assert!(!lease_path(&endpoint).exists());
}

#[tokio::test]
async fn managed_live_listener_is_busy_and_disconnects_keep_service_available() {
    let _shared = shared_namespace_guard();
    let endpoint = managed_endpoint("busy");
    let mut listener = crate::LocalListener::bind(&endpoint).unwrap();
    let credential = listener.credential();
    assert!(matches!(
        reap_managed(&endpoint, &credential),
        EndpointReapResult::Busy
    ));
    for _ in 0..2 {
        let (client, server) = tokio::try_join!(
            crate::LocalStream::connect(&endpoint, crate::DEFAULT_CONNECT_TIMEOUT),
            listener.accept(),
        )
        .unwrap();
        drop(client);
        drop(server);
    }
    assert!(socket_path(&endpoint).exists());
    assert!(lease_path(&endpoint).exists());
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn managed_old_credential_never_removes_a_new_incarnation() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "stale-credential");
    let first = bind_managed_at(&endpoint, root).unwrap();
    let old = first.credential();
    assert!(matches!(first.close(), EndpointReapResult::Reaped));

    let second = bind_managed_at(&endpoint, root).unwrap();
    let current = second.credential();
    assert_ne!(
        old, current,
        "a retired credential must not equal a new one"
    );
    // A live new incarnation is Busy for any credential, old or current.
    assert!(matches!(
        reap_managed_at(&endpoint, &old, root),
        EndpointReapResult::Busy
    ));
    // After the new incarnation is gone, the old credential still cannot
    // remove its replacement: identity plus incarnation must both match.
    second.abandon_for_test();
    assert!(matches!(
        reap_managed_at(&endpoint, &old, root),
        EndpointReapResult::StaleTarget
    ));
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Present(_)
    ));
    assert!(socket_path_at(root, &endpoint).exists());
    assert!(matches!(
        reap_managed_at(&endpoint, &current, root),
        EndpointReapResult::Reaped
    ));
    assert!(!socket_path_at(root, &endpoint).exists());
    assert!(!lease_path_at(root, &endpoint).exists());
}

#[tokio::test]
async fn managed_missing_or_replaced_gate_never_creates_a_second_lock() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "gate");
    let listener = bind_managed_at(&endpoint, root).unwrap();
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    let gate = root.join(GATE_NAME);
    let marker_before = std::fs::read(root.join(MARKER_NAME)).unwrap();

    std::fs::remove_file(&gate).unwrap();
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorMissing)
    ));
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert!(
        !gate.exists(),
        "a marker without its gate must never create a second coordinator inode"
    );

    // A replacement inode with the right permissions and ownership is still a
    // different coordinator: the marker identity refuses it.
    let replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&gate)
        .unwrap();
    drop(replacement);
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        std::fs::read(root.join(MARKER_NAME)).unwrap(),
        marker_before,
        "marker must not be rewritten to adopt a replacement gate"
    );

    // Deterministically model inode ABA even on filesystems that do not reuse
    // the just-unlinked inode: keep the original marker's persistent identity,
    // but make its device/inode describe the new, empty gate. A dev/ino-only
    // protocol incorrectly adopts this fresh OS object.
    let replacement_identity = identity_of(&gate);
    let mut aba_marker = marker_before.clone();
    aba_marker[9..17].copy_from_slice(&replacement_identity.device.to_le_bytes());
    aba_marker[17..25].copy_from_slice(&replacement_identity.inode.to_le_bytes());
    std::fs::write(root.join(MARKER_NAME), &aba_marker).unwrap();
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(std::fs::read(root.join(MARKER_NAME)).unwrap(), aba_marker);
    assert_eq!(
        std::fs::metadata(&gate).unwrap().len(),
        0,
        "replacement gate must never be initialized"
    );

    // A well-formed new coordinator identity is also rejected when dev/ino
    // match the old marker: validity of the object does not prove continuity.
    let mut new_identity = Vec::new();
    new_identity.extend_from_slice(GATE_MAGIC);
    new_identity.push(GATE_VERSION);
    new_identity.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    std::fs::write(&gate, &new_identity).unwrap();
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(std::fs::read(&gate).unwrap(), new_identity);
    assert_eq!(std::fs::read(root.join(MARKER_NAME)).unwrap(), aba_marker);
}

#[tokio::test]
async fn managed_listener_pins_original_gate_until_lease_ownership_ends() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "gate-pin");
    let listener = bind_managed_at(&endpoint, root).unwrap();
    let gate = root.join(GATE_NAME);
    let original = fstat(listener._gate_pin.as_raw_fd()).unwrap();
    std::fs::remove_file(&gate).unwrap();
    let unlinked = fstat(listener._gate_pin.as_raw_fd()).unwrap();
    assert!(same_file(&original, &unlinked));
    assert_eq!(
        unlinked.st_nlink, 0,
        "listener keeps the unlinked OS object alive"
    );
    let replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&gate)
        .unwrap();
    assert!(!same_file(
        &original,
        &fstat(replacement.as_raw_fd()).unwrap()
    ));
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lease_path_at(root, &endpoint))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert!(matches!(
        listener.close(),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "failed closed retirement must still end the old listener's lease"
    );
    assert_eq!(replacement.metadata().unwrap().len(), 0);
}

#[tokio::test]
async fn custom_context_lease_replacement_preserves_the_foreign_entry() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(root, "lease-replacement");
    let listener = bind_managed_at(&endpoint, root).unwrap();
    let credential = listener.credential();
    let lease = lease_path(&endpoint);
    // Move the still-locked original inode aside, so inode recycling cannot
    // disguise replacement. A different incarnation occupies its public name.
    let moved = root.join("saved-lease");
    std::fs::rename(&lease, &moved).unwrap();
    let record = OwnerRecord {
        address: endpoint.address().to_owned(),
        identity: credential.identity(),
        incarnation: uuid::Uuid::new_v4().into_bytes(),
    };
    write_lease(&lease, &record);
    let replacement_identity = identity_of(&lease);
    let replacement_bytes = std::fs::read(&lease).unwrap();
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &credential),
        EndpointReapResult::StaleTarget
    ));
    assert!(socket_path(&endpoint).exists());
    assert!(matches!(
        listener.close(),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidOwnership)
    ));
    assert_eq!(identity_of(&lease), replacement_identity);
    assert_eq!(std::fs::read(&lease).unwrap(), replacement_bytes);
    assert!(socket_path(&endpoint).exists());
    assert!(moved.exists());
}

#[test]
fn captured_sweep_directory_never_adopts_a_replacement_slot() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    test_namespace_root(root).unwrap();
    let endpoint = managed_endpoint_at(root, "sweep-replacement");
    let captured = EndpointDirectory::open(root, false).unwrap();
    let moved = namespace._parent.path().join("saved-namespace");
    std::fs::rename(root, &moved).unwrap();
    test_namespace_root(root).unwrap();
    // A complete lease-only record in the replacement would ordinarily be
    // sweepable. The old sweep descriptor must refuse it before reading it.
    let lease = lease_path(&endpoint);
    write_lease(
        &lease,
        &OwnerRecord {
            address: endpoint.address().to_owned(),
            identity: SocketIdentity {
                device: 1,
                inode: 2,
                changed_secs: 3,
                changed_nanos: 4,
            },
            incarnation: [0x5a; 16],
        },
    );
    let before = std::fs::read(&lease).unwrap();
    assert!(matches!(
        reap_slot_with_directory(
            endpoint.context(),
            &endpoint_stem(&endpoint).unwrap(),
            Some(&captured)
        ),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory)
    ));
    assert_eq!(std::fs::read(&lease).unwrap(), before);
    assert!(moved.join(GATE_NAME).exists());
}

#[tokio::test]
async fn managed_device_inode_only_marker_is_rejected_without_online_upgrade() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "old-gate-marker");
    assert!(matches!(
        bind_managed_at(&endpoint, root).unwrap().close(),
        EndpointReapResult::Reaped
    ));
    let gate = root.join(GATE_NAME);
    let marker = root.join(MARKER_NAME);
    let mut old_marker = std::fs::read(&marker).unwrap();
    old_marker.truncate(25);
    old_marker[8] = 1;
    std::fs::write(&marker, &old_marker).unwrap();
    std::fs::write(&gate, []).unwrap();
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(std::fs::read(marker).unwrap(), old_marker);
    assert_eq!(std::fs::metadata(gate).unwrap().len(), 0);
}

#[tokio::test]
async fn managed_corrupt_record_and_symlink_are_unverified_and_untouched() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    test_namespace_root(root).unwrap();

    let corrupt = managed_endpoint_at(&root, "corrupt");
    let stem = endpoint_stem(&corrupt)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let corrupt_socket = root.join(&stem);
    drop(std::os::unix::net::UnixListener::bind(&corrupt_socket).unwrap());
    let corrupt_lease = root.join(format!("{stem}{LEASE_SUFFIX}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&corrupt_lease)
        .unwrap();
    file.write_all(b"not-a-managed-record").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(matches!(
        inspect_managed_at(&corrupt, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord)
    ));
    let credential = managed_credential(&corrupt, &corrupt_socket, unreachable_portal());
    assert!(matches!(
        reap_managed_at(&corrupt, &credential, root),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
    ));
    assert!(corrupt_socket.exists());
    assert!(corrupt_lease.exists());

    let linked = managed_endpoint_at(&root, "symlink");
    let target = root.join("target-marker");
    std::fs::write(&target, b"keep").unwrap();
    let linked_socket = socket_path_at(root, &linked);
    std::os::unix::fs::symlink(&target, &linked_socket).unwrap();
    assert!(matches!(
        inspect_managed_at(&linked, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::Symlink)
    ));
    let credential = managed_credential(&linked, &target, unreachable_portal());
    assert!(matches!(
        reap_managed_at(&linked, &credential, root),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::Symlink)
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    std::fs::remove_file(&linked_socket).unwrap();
}

#[tokio::test]
async fn managed_budgeted_sweep_advances_past_busy_and_corrupt_slots() {
    let namespace = TestNamespace::new();
    let root = namespace.path();

    let live = managed_endpoint_at(&root, "sweep-live");
    let live_listener = bind_managed_at(&live, root).unwrap();

    let crashed = managed_endpoint_at(&root, "sweep-crashed");
    let crashed_listener = bind_managed_at(&crashed, root).unwrap();
    crashed_listener.abandon_for_test();
    assert!(socket_path_at(root, &crashed).exists());

    let orphan = managed_endpoint_at(&root, "sweep-orphan");
    let orphan_record = OwnerRecord {
        address: orphan.address().to_owned(),
        incarnation: uuid::Uuid::new_v4().into_bytes(),
        identity: SocketIdentity {
            device: 11,
            inode: 22,
            changed_secs: 33,
            changed_nanos: 44,
        },
    };
    write_lease(&lease_path_at(root, &orphan), &orphan_record);
    assert!(!socket_path_at(root, &orphan).exists());

    let corrupt = managed_endpoint_at(&root, "sweep-corrupt");
    let corrupt_lease = lease_path_at(root, &corrupt);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&corrupt_lease)
        .unwrap();
    file.write_all(b"broken").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mut sweep = retry_sweep(|| sweep_at(root));
    let mut total = SweepBatch::default();
    let mut complete = false;
    for _ in 0..64 {
        let batch = sweep.next_batch(SweepBudget {
            max_entries: 1,
            max_duration: Duration::ZERO,
        });
        assert!(
            batch.entries_visited <= 1,
            "entry budget exceeded: {batch:?}"
        );
        total.entries_visited += batch.entries_visited;
        total.endpoints_examined += batch.endpoints_examined;
        total.reaped += batch.reaped;
        total.busy += batch.busy;
        total.unverified += batch.unverified;
        total.leases_retired += batch.leases_retired;
        assert!(!batch.round_interrupted, "{batch:?}");
        complete = batch.round_complete;
        if complete {
            break;
        }
    }
    assert!(complete, "stable namespace must finish a bounded round");
    assert!(total.entries_visited > total.endpoints_examined);
    assert!(total.busy >= 1, "live lease must stay Busy");
    assert!(total.unverified >= 1, "corrupt slot must stay Unverified");
    assert!(
        total.reaped >= 2,
        "crashed socket and orphan lease converge"
    );
    assert!(total.leases_retired >= 1, "socket-less lease retires");
    assert!(!socket_path_at(root, &crashed).exists());
    assert!(!lease_path_at(root, &crashed).exists());
    assert!(!lease_path_at(root, &orphan).exists());
    assert!(corrupt_lease.exists());
    assert!(socket_path_at(root, &live).exists());
    assert!(lease_path_at(root, &live).exists());
    assert!(matches!(live_listener.close(), EndpointReapResult::Reaped));
}

/// Public sweep dispatch: `EndpointSweep::for_scope` enumerates the real
/// managed-v2 namespace, converges registered leftovers, and never touches the
/// unselected slots.
#[tokio::test]
async fn managed_public_sweep_targets_the_selected_namespace_only() {
    let _shared = shared_namespace_guard();
    let live = managed_endpoint("public-sweep-live");
    let live_listener = crate::LocalListener::bind(&live).unwrap();
    let dead = managed_endpoint("public-sweep-dead");
    let output = run_managed_process(&dead, "exit-owner");
    assert!(output.status.success(), "{output:?}");
    assert!(socket_path(&dead).exists());

    let scope = crate::EndpointSweep::scope_for_addresses(
        &live,
        &[live.address().to_owned(), dead.address().to_owned()],
    )
    .unwrap();
    let mut sweep = retry_sweep(|| crate::EndpointSweep::for_scope(&scope));
    let mut complete = false;
    let mut reaped = 0;
    // Scope constrains retirement, not directory enumeration. Other native
    // runtimes may add names, so a fixed batch count cannot prove directory EOF.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut visited = 0;
    while std::time::Instant::now() < deadline {
        let batch = sweep.next_batch(SweepBudget {
            max_entries: 1,
            max_duration: Duration::ZERO,
        });
        assert!(batch.entries_visited <= 1, "{batch:?}");
        assert_eq!(batch.io_errors, 0, "{batch:?}");
        reaped += batch.reaped;
        visited += batch.entries_visited;
        assert!(!batch.round_interrupted, "{batch:?}");
        assert!(!batch.namespace_changed, "{batch:?}");
        if batch.round_complete {
            complete = true;
            break;
        }
        assert_eq!(
            batch.entries_visited, 1,
            "round stopped advancing: {batch:?}"
        );
    }
    assert!(
        complete,
        "managed namespace must reach EOF before deadline; visited={visited}, reaped={reaped}"
    );
    assert!(reaped >= 1, "registered leftover must converge: {reaped}");
    assert!(!socket_path(&dead).exists());
    assert!(!lease_path(&dead).exists());
    assert!(socket_path(&live).exists());
    assert!(lease_path(&live).exists());
    assert!(matches!(live_listener.close(), EndpointReapResult::Reaped));
}

/// Bind rollback: a failed managed initialization withdraws only the socket it
/// created and leaves the address reusable.
#[tokio::test]
async fn managed_failed_initialization_withdraws_its_socket_and_rebinds() {
    use crate::unix_common::fault;
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "managed-rollback");
    fault::inject(fault::Failure::SocketPermissions);
    let error = bind_managed_at(&endpoint, root)
        .err()
        .expect("injected socket permission failure must fail bind");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    assert!(
        !socket_path_at(root, &endpoint).exists(),
        "failed initialization must withdraw the socket it bound"
    );

    let listener = bind_managed_at(&endpoint, root).expect("the fixed address must be reusable");
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    assert!(!socket_path_at(root, &endpoint).exists());
    assert!(!lease_path_at(root, &endpoint).exists());
}

/// A Drop that cannot take the gate stays bounded, releases its handles in
/// order, and leaves complete metadata for the reaper.
#[tokio::test]
async fn managed_drop_without_gate_leaves_metadata_for_the_reaper() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "drop-gate");
    let listener = bind_managed_at(&endpoint, root).unwrap();
    let credential = listener.credential();

    let held_root = root.to_owned();
    let holder = std::thread::spawn(move || {
        let namespace = ManagedNamespace::open_root(&held_root, false, true).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(namespace);
    });
    // Wait until the gate is provably held by the other thread.
    let mut holding = false;
    for _ in 0..400 {
        match ManagedNamespace::open_root(root, false, false) {
            Err(NamespaceError::Busy) => {
                holding = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert!(holding, "gate holder never acquired the coordinator lock");

    drop(listener);
    assert!(
        socket_path_at(root, &endpoint).exists(),
        "a Drop without the gate must keep its registered metadata"
    );
    assert!(lease_path_at(root, &endpoint).exists());
    holder.join().unwrap();

    assert!(matches!(
        reap_managed_at(&endpoint, &credential, root),
        EndpointReapResult::Reaped
    ));
    assert!(!socket_path_at(root, &endpoint).exists());
    assert!(!lease_path_at(root, &endpoint).exists());
}

/// Explicit close cannot wait forever for a foreign coordinator. A bounded
/// failure preserves exact retirement metadata and releases listener ownership.
#[tokio::test]
async fn managed_close_busy_gate_is_bounded_and_preserves_owner_record() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "close-busy-gate");
    let listener = bind_managed_at(&endpoint, root).unwrap();
    let credential = listener.credential();
    let held_gate = ManagedNamespace::open_root(root, false, true).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let closer = std::thread::spawn(move || tx.send(listener.close()).unwrap());
    let result = rx.recv_timeout(Duration::from_secs(2));
    // Keep the gate held until close itself returns, rather than ending the
    // contention after a delay. Release before asserting so a broken close
    // cannot strand a test worker.
    drop(held_gate);
    closer.join().unwrap();
    assert!(matches!(result.unwrap(), EndpointReapResult::Busy));
    assert!(socket_path_at(root, &endpoint).exists());
    assert!(lease_path_at(root, &endpoint).exists());
    assert!(std::os::unix::net::UnixStream::connect(socket_path_at(root, &endpoint)).is_err());

    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lease_path_at(root, &endpoint))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "a failed close must release its listener lease"
    );
    let record = read_record(&lease).unwrap().unwrap();
    assert_eq!(record.identity, credential.identity());
    assert_eq!(record.incarnation, credential.incarnation());
}

/// The P1 blocker as a real concurrent negative case: an opener is parked
/// inside the gate-hold window while the verified `v2` directory is renamed
/// away and a fresh `v2` takes its name. The opener must never create its
/// socket in the replacement directory, and the replacement must keep exactly
/// its own objects.
#[tokio::test]
async fn managed_directory_rename_inside_the_gate_window_never_binds_into_the_replacement() {
    let parent = tempfile::Builder::new()
        .prefix("c2r-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let moved = parent.path().join("moved");
    let endpoint = managed_endpoint_at(&root, "rename-window");

    // Every future managed open now parks just after the gate is held.
    barrier::arm(&root);

    let opener_root = root.clone();
    let opener_endpoint = endpoint.clone();
    let opener = std::thread::spawn(move || bind_managed_at(&opener_endpoint, &opener_root));

    assert!(
        barrier::wait_entered(),
        "opener never reached the gate-hold window"
    );

    // The rename race the blocker describes: the verified directory leaves its
    // name and a different, private directory takes the same name.
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(
        root.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A pre-existing foreign object in the replacement directory must survive.
    let foreign = root.join("foreign-marker");
    std::fs::write(&foreign, b"keep").unwrap();

    barrier::release();
    let result = opener.join().unwrap();

    // The stale opener must fail rather than bind into the replacement, and
    // must not adopt or delete anything there.
    assert!(
        result.is_err(),
        "an opener whose namespace was renamed away must not succeed"
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep");
    let replacement_entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        replacement_entries.len(),
        1,
        "replacement directory must contain only its own object: {replacement_entries:?}"
    );

    // The decisive property: no socket for this endpoint was created in the
    // replacement directory, and none was created in the moved-aside directory
    // either. Whatever is left there names the directory the opener actually
    // verified through its descriptor, never the path that was renamed away.
    assert!(
        !socket_path_at(&root, &endpoint).exists(),
        "the replacement directory must never receive this process's socket"
    );
    assert!(
        !socket_path_at(&moved, &endpoint).exists(),
        "the opener must fail before binding once its directory identity moved"
    );
    // The lease alone may already exist in the verified directory: it is
    // created under the gate before the rename is observable. With no readable
    // record it stays conservatively Unverified rather than being guessed
    // away; a fresh owner reuses the slot and converges it.
    if lease_path_at(&moved, &endpoint).exists() {
        let retired = match read_lease_record(
            &ManagedNamespace::open_root(&moved, false, false).unwrap(),
            &endpoint_stem(&endpoint).unwrap(),
        ) {
            Err(reason) => EndpointReapResult::Unverified(reason),
            other => panic!("partial slot must remain unverifiable: {other:?}"),
        };
        assert!(
            matches!(
                retired,
                EndpointReapResult::Reaped
                    | EndpointReapResult::AlreadyAbsent
                    | EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
            ),
            "a socket-less slot without a readable record stays conservative: {retired:?}"
        );
        let _ = std::fs::remove_file(lease_path_at(&moved, &endpoint));
    }
    barrier::disarm();
}

/// Two concurrent first initializations of a brand-new namespace must share the
/// single coordinator inode: exactly one gate file, one marker, and every
/// successful opener bound to the same gate.
#[tokio::test]
async fn managed_parallel_first_initialization_shares_one_gate_inode() {
    let parent = tempfile::Builder::new()
        .prefix("c2f-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoints: Vec<_> = (0..6)
        .map(|i| managed_endpoint_at(&root, &format!("first-{i}")))
        .collect();
    let root = std::sync::Arc::new(root);
    let start = std::sync::Arc::new(std::sync::Barrier::new(endpoints.len()));
    // Every opener binds a real Tokio listener, so each thread enters the
    // reactor this test-harness runtime already owns.
    let runtime = tokio::runtime::Handle::current();

    let listeners = std::thread::scope(|scope| {
        let handles: Vec<_> = endpoints
            .iter()
            .map(|endpoint| {
                let root = root.clone();
                let start = start.clone();
                let runtime = &runtime;
                scope.spawn(move || {
                    let _entered = runtime.enter();
                    start.wait();
                    bind_managed_at(endpoint, &root)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut live = Vec::new();
    for result in listeners {
        live.push(result.expect("a fresh namespace must be adoptable by every opener"));
    }

    let gate = root.join(GATE_NAME);
    let marker = root.join(MARKER_NAME);
    assert!(gate.exists() && marker.exists());
    let gate_identity = identity_of(&gate);

    // Every live endpoint must be present and verifiable against that same
    // coordinator, and every successful opener must have used the one gate.
    for (endpoint, listener) in endpoints.iter().zip(live.iter()) {
        assert!(
            matches!(
                inspect_managed_at(endpoint, &root),
                EndpointInspection::Present(_)
            ),
            "endpoint under the shared gate must be verifiable"
        );
        assert_eq!(
            listener.gate_identity_for_test(),
            gate_identity,
            "every opener must have bound under the same coordinator inode"
        );
    }
    assert_eq!(
        std::fs::read_dir(root.as_path())
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_name() == GATE_NAME)
            .count(),
        1,
        "there must never be a second coordinator entry"
    );
    for listener in live {
        assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    }
}
#[tokio::test]
async fn managed_interrupted_sweep_never_reports_a_completed_round() {
    let parent = tempfile::Builder::new()
        .prefix("c2i-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    test_namespace_root(&root).unwrap();
    let endpoint = managed_endpoint_at(&root, "sweep-interrupt");
    let listener = bind_managed_at(&endpoint, &root).unwrap();
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));

    let mut sweep = retry_sweep(|| sweep_at(&root));
    let first = sweep.next_batch(SweepBudget {
        max_entries: 1,
        max_duration: Duration::ZERO,
    });
    assert!(!first.round_complete && !first.round_interrupted);

    let moved = parent.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(
        root.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

    let interrupted = sweep.next_batch(SweepBudget::default());
    assert!(interrupted.namespace_changed, "{interrupted:?}");
    assert!(interrupted.round_interrupted, "{interrupted:?}");
    assert!(!interrupted.round_complete, "{interrupted:?}");
    let terminal = sweep.next_batch(SweepBudget::default());
    assert!(terminal.round_interrupted, "{terminal:?}");
    assert!(!terminal.round_complete, "{terminal:?}");
    assert_eq!(terminal.entries_visited, 0);
}

#[test]
fn managed_initialization_window_stays_unverified_without_creating_locks() {
    let parent = tempfile::Builder::new()
        .prefix("c2w-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(
        root.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = managed_endpoint_at(&root, "window");
    let stem = endpoint_stem(&endpoint)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    drop(std::os::unix::net::UnixListener::bind(root.join(&stem)).unwrap());

    // An endpoint object without a verifiable first-initialization identity is
    // never adopted: no marker is written and the namespace stays Unverified.
    let error = ManagedNamespace::open_root(&root, true, true).map(|_| ());
    assert!(matches!(
        error,
        Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::InitializationIncomplete
        ))
    ));
    assert!(!root.join(MARKER_NAME).exists());

    // The fixed coordinator is a long-lived constant and is never unlinked,
    // not even by a refused first initialization: a drop-then-unlink window
    // would let a third party bind a *different* gate inode at the same name.
    let gate = root.join(GATE_NAME);
    assert!(gate.exists(), "the fixed gate is never unlinked online");
    let gate_identity = identity_of(&gate);
    // A retried initialization keeps the same coordinator and stays refused.
    assert!(matches!(
        ManagedNamespace::open_root(&root, true, true).map(|_| ()),
        Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::InitializationIncomplete
        ))
    ));
    assert_eq!(
        identity_of(&gate),
        gate_identity,
        "a refused initialization must not replace the coordinator inode"
    );

    // Read-only maintenance reports the same boundary.
    assert!(matches!(
        ManagedNamespace::open_root(&root, false, true).map(|_| ()),
        Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::InitializationIncomplete
        ))
    ));
    assert_eq!(identity_of(&gate), gate_identity);
    assert!(!root.join(MARKER_NAME).exists());
}

#[test]
fn managed_record_decode_rejects_hash_only_and_oversized_payloads() {
    let endpoint = managed_endpoint("record-shape");
    let incarnation = uuid::Uuid::new_v4().into_bytes();
    let identity = SocketIdentity {
        device: 1,
        inode: 2,
        changed_secs: 3,
        changed_nanos: 4,
    };
    let record = OwnerRecord {
        address: endpoint.address().to_owned(),
        incarnation,
        identity,
    };
    let encoded = record.encode().unwrap();
    assert!(encoded.len() <= RECORD_MAX_LEN);
    assert_eq!(OwnerRecord::decode(&encoded), Some(record.clone()));
    assert!(record_matches_endpoint(&record, &endpoint));

    // A different logical address cannot reuse the same hashed basename.
    let other = LocalEndpoint::from_address("ipc://record-shape-other").unwrap();
    assert!(!record_matches_endpoint(&record, &other));

    // Undecodable shapes never become a record: truncated, oversized, and a
    // corrupt protocol byte.
    assert_eq!(OwnerRecord::decode(&encoded[..encoded.len() - 1]), None);
    let mut oversized = encoded.clone();
    oversized.push(0);
    assert_eq!(OwnerRecord::decode(&oversized), None);
    let mut old_format = encoded.clone();
    old_format[8] = 1;
    assert_eq!(OwnerRecord::decode(&old_format), None);
    let mut wrong_protocol = encoded.clone();
    wrong_protocol[9] = 1;
    assert_eq!(OwnerRecord::decode(&wrong_protocol), None);
}

/// P2 blocker: a coordinator held by another process must not block
/// maintenance. `inspect`, `reap`, and a budgeted sweep all return promptly and
/// still advance their budget, and a later pass converges the slot once the
/// foreign gate is released.
#[tokio::test]
async fn managed_busy_gate_never_blocks_maintenance_and_budget_still_advances() {
    let namespace = TestNamespace::new();
    let root = namespace.path();

    let dead = managed_endpoint_at(&root, "busy-gate-dead");
    let dead_listener = bind_managed_at(&dead, root).unwrap();
    let dead_credential = dead_listener.credential();
    dead_listener.abandon_for_test();
    assert!(socket_path_at(root, &dead).exists());

    let live = managed_endpoint_at(&root, "busy-gate-live");
    let live_listener = bind_managed_at(&live, root).unwrap();

    // A foreign holder takes the same coordinator the maintenance paths need.
    let held_root = root.to_owned();
    let holding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let holder = {
        let holding = holding.clone();
        let release = release.clone();
        std::thread::spawn(move || {
            let namespace = ManagedNamespace::open_root(&held_root, false, true).unwrap();
            holding.store(true, std::sync::atomic::Ordering::SeqCst);
            while !release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(namespace);
        })
    };
    for _ in 0..1000 {
        if holding.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        holding.load(std::sync::atomic::Ordering::SeqCst),
        "foreign gate holder never acquired the coordinator"
    );

    // Every maintenance entry point answers Busy instead of waiting.
    let started = std::time::Instant::now();
    assert!(matches!(
        reap_managed_at(&dead, &dead_credential, root),
        EndpointReapResult::Busy
    ));
    assert!(matches!(
        reap_slot(&context_for_namespace(root), &endpoint_stem(&dead).unwrap()),
        EndpointReapResult::Busy
    ));
    match inspect_managed_at(&dead, root) {
        EndpointInspection::IoError(error) => {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        }
        other => panic!("busy gate must be an observable WouldBlock, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "maintenance waited on a foreign gate for {:?}",
        started.elapsed()
    );

    // A budgeted sweep round still advances: it must visit entries, count the
    // busy candidate against the budget, and finish the round while the foreign
    // holder is still in place.
    let mut sweep = retry_sweep(|| sweep_at(root));
    let mut visited = 0;
    let mut busy = 0;
    let mut complete = false;
    for _ in 0..128 {
        let batch = sweep.next_batch(SweepBudget {
            max_entries: 2,
            max_duration: Duration::ZERO,
        });
        visited += batch.entries_visited;
        busy += batch.busy;
        assert!(!batch.round_interrupted, "{batch:?}");
        if batch.round_complete {
            complete = true;
            break;
        }
    }
    assert!(
        complete,
        "a busy gate must not stop a bounded round from reaching EOF"
    );
    assert!(visited >= 2, "round must still advance: {visited}");
    assert!(
        busy >= 1,
        "the blocked candidate must be counted as Busy: {busy}"
    );

    // The foreign holder leaves and a later pass converges the slot.
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    holder.join().unwrap();
    assert!(matches!(
        reap_managed_at(&dead, &dead_credential, root),
        EndpointReapResult::Reaped
    ));
    assert!(!socket_path_at(root, &dead).exists());
    assert!(!lease_path_at(root, &dead).exists());
    assert!(matches!(live_listener.close(), EndpointReapResult::Reaped));
}

/// Independent subprocess negative: a real managed bind in a fresh process
/// leaves both the default follow-process cwd and an already-established
/// thread-local cwd untouched, and never moves the process working directory.
/// Running this in its own process keeps the ambient-cwd assertion honest
/// without this suite ever being forced to run serially.
#[test]
fn managed_bind_does_not_change_cwd_in_an_independent_subprocess() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    test_namespace_root(root).unwrap();
    let endpoint = managed_endpoint_at(&root, "cwd-probe");
    // A real, existing directory the probe thread can pin as its thread cwd.
    let elsewhere = TestNamespace::new();
    let elsewhere_dir = elsewhere
        .path()
        .parent()
        .expect("isolated parent directory")
        .to_owned();

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "unix_managed::tests::managed_process_fixture",
            "--nocapture",
        ])
        .env("C2_LOCAL_MANAGED_TEST_ADDRESS", endpoint.address())
        .env(
            "C2_LOCAL_MANAGED_TEST_ROOT",
            endpoint.context().unix_root().unwrap(),
        )
        .env("C2_LOCAL_MANAGED_TEST_ACTION", "cwd-probe")
        .env("C2_LOCAL_MANAGED_TEST_CWD", &elsewhere_dir);
    // Linux shares its process cwd across threads. Change it only in this
    // isolated child, so the real public bind proves an arbitrary launch cwd is
    // preserved without mutating the parallel parent harness.
    #[cfg(target_os = "linux")]
    command.current_dir(&elsewhere_dir);
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("cwd-probe-ok"),
        "subprocess cwd probe failed:\nstdout={stdout}\nstderr={stderr}"
    );
}

/// A held coordinator must never make `bind_managed_at` wait unboundedly: server
/// startup reaches it, so a foreign gate holder becomes a bounded `AddrInUse`
/// that the upper start layer can retry with its own timeout.
#[tokio::test]
async fn managed_bind_is_bounded_when_the_gate_is_held() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    // Establish the namespace first so the bind path only contends on the gate.
    let seed = bind_managed_at(&managed_endpoint_at(&root, "bounded-seed"), root).unwrap();
    seed.abandon_for_test();

    // A foreign holder keeps the coordinator for well past the bind's bounded
    // retry window.
    let held_root = root.to_owned();
    let holding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let holder = {
        let holding = holding.clone();
        let release = release.clone();
        std::thread::spawn(move || {
            let namespace = ManagedNamespace::open_root(&held_root, false, true).unwrap();
            holding.store(true, std::sync::atomic::Ordering::SeqCst);
            while !release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(namespace);
        })
    };
    for _ in 0..1000 {
        if holding.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        holding.load(std::sync::atomic::Ordering::SeqCst),
        "foreign gate holder never acquired the coordinator"
    );

    let endpoint = managed_endpoint_at(&root, "bounded-bind");
    let started = std::time::Instant::now();
    let error = bind_managed_at(&endpoint, root)
        .err()
        .expect("a held gate must refuse a bind, not block it");
    let elapsed = started.elapsed();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
    assert!(
        elapsed < Duration::from_secs(5),
        "a held gate blocked bind for {elapsed:?}; startup must stay bounded"
    );
    // The refused bind created nothing.
    assert!(!socket_path_at(root, &endpoint).exists());
    assert!(!lease_path_at(root, &endpoint).exists());

    // Once the caller (not the bind) releases the gate, a fresh attempt wins the
    // address under the normal retry timeout.
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    holder.join().unwrap();
    let listener = bind_managed_at(&endpoint, root).expect("retry after release must bind");
    assert!(socket_path_at(root, &endpoint).exists());
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
}

/// Managed record failures are conservative: a failed or partial write rolls
/// back only this call's socket, and a foreign replacement is never deleted.
#[tokio::test]
async fn managed_record_failures_roll_back_conservatively() {
    use crate::unix_common::fault;

    // 1. The owner record write fails outright: the bound socket is withdrawn
    //    and the address is reusable.
    {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = managed_endpoint_at(&root, "record-write");
        fault::inject(fault::Failure::ManagedRecordWrite);
        let error = bind_managed_at(&endpoint, root)
            .err()
            .expect("injected record write failure must fail bind");
        assert_eq!(error.kind(), io::ErrorKind::WriteZero, "{error}");
        assert!(
            !socket_path_at(root, &endpoint).exists(),
            "a failed record write must withdraw this call's socket"
        );
        // The lease entry itself is created under the gate before the record
        // write. With no readable record it cannot prove ownership, so it stays
        // Unverified rather than being deleted -- but it is never a live
        // endpoint either, so a fresh bind simply reuses the same slot.
        let lease = lease_path_at(root, &endpoint);
        assert!(
            lease.exists(),
            "the lease entry is created before the record write and is retained"
        );
        assert!(
            matches!(
                reap_slot(
                    &context_for_namespace(root),
                    &endpoint_stem(&endpoint).unwrap()
                ),
                EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
                    | EndpointReapResult::Reaped
                    | EndpointReapResult::AlreadyAbsent
            ),
            "a socket-less slot without a readable record stays unverified"
        );
        assert!(
            matches!(
                inspect_managed_at(&endpoint, root),
                EndpointInspection::Absent
            ),
            "a withdrawn socket must not be reported present"
        );
    }

    // 2. A partial record write cannot read back, so the slot is Unverified and
    //    the incomplete record is never mistaken for ownership.
    {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = managed_endpoint_at(&root, "record-partial");
        fault::inject(fault::Failure::ManagedRecordPartialWrite);
        assert!(
            bind_managed_at(&endpoint, root).is_err(),
            "a partial record must not be accepted"
        );
        assert!(matches!(
            inspect_managed_at(&endpoint, root),
            EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord)
                | EndpointInspection::Absent
        ));
        assert!(!socket_path_at(root, &endpoint).exists());
        // The address is still usable once the fault is gone.
        let listener = bind_managed_at(&endpoint, root).unwrap();
        assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    }

    // 3. A foreign replacement between bind and record write is never removed.
    {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = managed_endpoint_at(&root, "record-replaced");
        fault::inject(fault::Failure::ManagedRecordWriteAfterReplacement);
        let error = bind_managed_at(&endpoint, root)
            .err()
            .expect("a replaced socket entry must fail bind");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        let replacement = socket_path_at(root, &endpoint);
        assert!(
            replacement.exists(),
            "the foreign replacement must be left in place"
        );
        assert_eq!(std::fs::read(&replacement).unwrap(), b"replacement-marker");
        assert!(matches!(
            inspect_managed_at(&endpoint, root),
            EndpointInspection::Unverified(_)
        ));
    }
}

/// A duplicate lease handle inherited across a fork-style clone cannot extend
/// listener ownership.
///
/// The duplicate shares the listener's own open file description, exactly like
/// a descriptor inherited across `fork`: *closing* the listener's descriptor
/// would leave the flock held by the duplicate. The listener's explicit
/// `LOCK_UN` on that shared description is what releases ownership, and every
/// assertion below holds while the duplicate is still open -- never after it is
/// dropped.
#[tokio::test]
async fn managed_duplicate_lease_cannot_extend_ownership_across_retirement() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "dup-lease");

    // --- Part 1: explicit close under the gate. ---
    let listener = bind_managed_at(&endpoint, root).unwrap();
    // A true duplicate of this listener's lease open file description.
    let duplicate = listener.duplicate_lease_fd_for_test();

    // Retirement under the gate is refused while the duplicate's flock is the
    // same one the listener holds, so ownership is never silently transferred.
    assert!(matches!(
        reap_managed_at(&endpoint, &listener.credential(), root),
        EndpointReapResult::Busy
    ));

    // The listener retires while the duplicate is STILL OPEN. The explicit
    // `LOCK_UN` on the shared open file description releases ownership even
    // though the duplicate keeps the OFD alive.
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    assert!(
        !socket_path_at(root, &endpoint).exists(),
        "an explicit close must withdraw its socket even with a live duplicate"
    );
    assert!(!lease_path_at(root, &endpoint).exists());

    // Ownership transferred deterministically: a fresh bind wins the address
    // while the unrelated duplicate is still open.
    let replacement =
        bind_managed_at(&endpoint, root).expect("re-bind must win the released lease");
    assert!(matches!(replacement.close(), EndpointReapResult::Reaped));
    drop(duplicate);

    // --- Part 2: a Drop that never takes the gate. ---
    // This is the case where the duplicate would otherwise keep the *same*
    // lease inode locked forever: the entry is not unlinked, so the reaper must
    // observe a released flock on the surviving inode even though the duplicate
    // still references it.
    let listener = bind_managed_at(&endpoint, root).unwrap();
    let duplicate = listener.duplicate_lease_fd_for_test();

    // Hold the coordinator so the Drop cannot retire and is forced onto the
    // bounded "leave metadata for the reaper" path.
    let held_root = root.to_owned();
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let holder = {
        let release = release.clone();
        std::thread::spawn(move || {
            let namespace = ManagedNamespace::open_root(&held_root, false, true).unwrap();
            while !release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(namespace);
        })
    };
    // Wait until the gate is provably held, then Drop the listener.
    let mut holding = false;
    for _ in 0..1000 {
        if matches!(
            ManagedNamespace::open_root(root, false, false),
            Err(NamespaceError::Busy)
        ) {
            holding = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(holding, "gate holder never acquired the coordinator");

    let credential = listener.credential();
    drop(listener);
    assert!(
        socket_path_at(root, &endpoint).exists(),
        "a Drop without the gate must keep its registered metadata"
    );

    release.store(true, std::sync::atomic::Ordering::SeqCst);
    holder.join().unwrap();

    // The duplicate is STILL OPEN. The reaper must not be blocked by it: the
    // listener already issued `LOCK_UN` on the shared description during Drop.
    assert!(
        matches!(
            reap_managed_at(&endpoint, &credential, root),
            EndpointReapResult::Reaped
        ),
        "a live duplicate must not keep the retired lease locked"
    );
    assert!(!socket_path_at(root, &endpoint).exists());
    assert!(!lease_path_at(root, &endpoint).exists());
    // The retired lease entry never comes back, so the duplicate cannot
    // recreate ownership.
    assert!(matches!(
        reap_managed_at(&endpoint, &credential, root),
        EndpointReapResult::AlreadyAbsent
    ));
    drop(duplicate);
}

/// A first-initialization scan is bounded: a namespace that exceeds the entry
/// cap fails closed instead of traversing an unbounded directory.
#[test]
fn managed_first_initialization_scan_is_bounded_and_fails_closed() {
    let parent = tempfile::Builder::new()
        .prefix("c2s-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(
        root.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    for index in 0..(INIT_SCAN_LIMIT + 4) {
        std::fs::write(root.join(format!("noise-{index:03}")), b"x").unwrap();
    }

    let error = ManagedNamespace::open_root(&root, true, true).map(|_| ());
    assert!(matches!(
        error,
        Err(NamespaceError::Io(_)) | Err(NamespaceError::Unverified(_))
    ));
    assert!(
        !root.join(MARKER_NAME).exists(),
        "an over-budget namespace must never be adopted"
    );
}

/// The descriptor-anchored bind never touches the *calling* thread's working
/// directory.
///
/// On macOS the bind switches a dedicated short-lived thread's directory; that
/// thread exits with the bind, so a caller thread that already had its own
/// working directory keeps exactly that directory. The real public bind path is
/// exercised, not a helper, and the process-wide directory is asserted
/// unchanged too. On Linux the descriptor is named through `/proc/self/fd` and
/// no directory switch happens at all.
#[tokio::test]
async fn managed_bind_never_changes_the_calling_thread_directory() {
    let namespace = TestNamespace::new();
    let root = namespace.path().to_owned();
    test_namespace_root(&root).unwrap();
    let endpoint = managed_endpoint_at(&root, "thread-cwd");
    let endpoint_a = managed_endpoint_at(&root, "thread-cwd-a");
    let endpoint_b = managed_endpoint_at(&root, "thread-cwd-b");

    // A second, unrelated existing directory the calling thread will switch to,
    // so a bind that restored the process default instead of the thread
    // directory would be observable.
    let other = TestNamespace::new();
    let other_root = other
        .path()
        .parent()
        .expect("isolated parent directory")
        .to_owned();
    let process_cwd_before = std::env::current_dir().unwrap();
    #[cfg(target_os = "macos")]
    let expected_caller_dir = std::fs::canonicalize(&other_root).unwrap();
    // Linux has no thread-local cwd API here: the caller follows the process
    // cwd and /proc/self/fd binds must leave that exact directory untouched.
    #[cfg(not(target_os = "macos"))]
    let expected_caller_dir = process_cwd_before.clone();
    // Keep the namespace tempdir alive inside the thread for the whole test.
    let caller = std::thread::spawn(move || {
        let _namespace = namespace;
        // Give the calling thread its own *thread-local* directory distinct from
        // the process default, using the same primitive the bind thread uses but
        // on this thread. On macOS this never moves the process-wide directory,
        // so parallel tests and unrelated threads are unaffected.
        let other_dir = EndpointDirectory::open(&other_root, false).unwrap();
        #[cfg(target_os = "macos")]
        crate::unix_common::set_thread_directory(other_dir.fd()).unwrap();
        #[cfg(not(target_os = "macos"))]
        let _ = &other_dir;
        let caller_dir_before = std::env::current_dir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (bound_a, bound_b, inside_a, inside_b) = runtime.block_on(async {
            let mut first = bind_managed_at(&endpoint_a, &root).unwrap();
            let inside_a = std::env::current_dir().unwrap();
            let mut second = bind_managed_at(&endpoint_b, &root).unwrap();
            let inside_b = std::env::current_dir().unwrap();
            // Both bindings are live rendezvous in the verified directory.
            let probe_a = crate::platform::connect(&endpoint_a).await.is_ok();
            let probe_b = crate::platform::connect(&endpoint_b).await.is_ok();
            assert!(probe_a && probe_b, "both managed sockets must be reachable");
            let _ = first.accept().await;
            let _ = second.accept().await;
            (
                matches!(first.close(), EndpointReapResult::Reaped),
                matches!(second.close(), EndpointReapResult::Reaped),
                inside_a,
                inside_b,
            )
        });
        let caller_dir_after = std::env::current_dir().unwrap();
        (
            bound_a,
            bound_b,
            caller_dir_before,
            caller_dir_after,
            inside_a,
            inside_b,
        )
    });
    let (bound_a, bound_b, caller_before, caller_after, inside_a, inside_b) =
        caller.join().unwrap();

    assert!(bound_a && bound_b, "managed binds must retire cleanly");
    // The calling thread's own directory survived every bind unchanged: the
    // dedicated bind thread never wrote, read, or restored it.
    assert_eq!(
        caller_before, caller_after,
        "the managed bind must not move the calling thread's directory"
    );
    assert_eq!(
        caller_after, expected_caller_dir,
        "the calling thread keeps its platform-specific directory unchanged"
    );
    assert_eq!(
        inside_a, caller_before,
        "binding must not change the caller's directory mid-call"
    );
    assert_eq!(inside_b, caller_before);

    // The process-wide directory was never moved by the dedicated bind thread.
    assert_eq!(std::env::current_dir().unwrap(), process_cwd_before);

    // The derived production slot was never created by these binds.
    assert!(!socket_path(&endpoint).exists());
}

/// Every Unix target that is not Linux or macOS cannot anchor a socket bind to
/// a verified descriptor, so `bind_in_directory_on_thread` fails closed instead
/// of falling back to the absolute path. On the two supported targets the same
/// call either binds through the descriptor (macOS) or is not used because the
/// `/proc/self/fd` path exists (Linux).
#[test]
fn managed_descriptor_bind_support_matches_the_platform() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Supported targets never take the fail-closed branch; this is asserted
        // by the real bind tests above rather than a fabricated call.
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        test_namespace_root(root).unwrap();
        let directory = EndpointDirectory::open(root, false).unwrap();
        let error = crate::unix_common::bind_in_directory_on_thread(
            &directory,
            OsStr::new("whatever.sock"),
        )
        .err()
        .expect("unsupported targets must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}

/// The dirfd-relative bind target is never the absolute path a concurrent
/// rename can redirect, and on macOS there is no descriptor path at all so the
/// dedicated-thread primitive is the only descriptor-anchored option.
#[tokio::test]
async fn managed_bind_target_is_descriptor_relative_not_the_absolute_path() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    test_namespace_root(root).unwrap();
    let directory = EndpointDirectory::open(root, false).unwrap();
    let endpoint = managed_endpoint_at(&root, "dirfd-probe");
    let names = managed_names(&endpoint).unwrap();

    let absolute = directory.socket_path(&names.socket_os);
    match dirfd_relative_socket_path(&directory, &names.socket_os) {
        // Linux names the descriptor explicitly.
        Some(relative) => assert_ne!(relative, absolute),
        // macOS has no descriptor path, so the bind runs on a dedicated thread
        // with a bare relative name instead of the absolute path.
        None => {
            assert!(
                !names.socket_os.to_string_lossy().contains('/'),
                "the dedicated-thread bind name must be relative, never the absolute path"
            );
            assert_ne!(Path::new(&names.socket_os), absolute.as_path());
        }
    }

    // A real bind through the descriptor still lands in the verified directory
    // and produces a working rendezvous. The client connects to the actual
    // socket path in that root, proving the bind did not go to the derived
    // production path.
    let mut listener = bind_managed_at(&endpoint, root).unwrap();
    let bound_path = socket_path_at(root, &endpoint);
    assert!(bound_path.exists());
    let (mut client, mut server) = tokio::try_join!(
        async {
            let stream = tokio::net::UnixStream::connect(&bound_path).await?;
            Ok::<_, io::Error>(stream)
        },
        listener.accept(),
    )
    .unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut server, b"dirfd")
        .await
        .unwrap();
    let mut bytes = [0_u8; 5];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut bytes)
        .await
        .unwrap();
    assert_eq!(&bytes, b"dirfd");
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    assert!(!bound_path.exists());
    // Closing retires the exact public-derived slot.
    assert_eq!(bound_path, socket_path(&endpoint));
    assert!(!socket_path(&endpoint).exists());
}

/// Two real abandoned listeners share a private test root. Scope selects one
/// orphan and one active listener, leaving every byte and identity of the
/// unselected orphan and its ownership record unchanged.
#[tokio::test]
async fn sweep_scope_reaps_only_selected_slots_and_preserves_active_owner() {
    for mode in [0o700, 0o755] {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).unwrap();
        let selected = managed_endpoint_at(&root, "scope-selected");
        let unselected = managed_endpoint_at(&root, "scope-unselected");
        let active = managed_endpoint_at(&root, "scope-active");
        bind_managed_at(&selected, root).unwrap().abandon_for_test();
        bind_managed_at(&unselected, root)
            .unwrap()
            .abandon_for_test();
        let active_listener = bind_managed_at(&active, root).unwrap();
        let untouched_socket = socket_path_at(root, &unselected);
        let untouched_lease = lease_path_at(root, &unselected);
        let socket_identity = identity_of(&untouched_socket);
        let lease_identity = identity_of(&untouched_lease);
        let lease_bytes = std::fs::read(&untouched_lease).unwrap();
        for _ in 0..2 {
            let mut sweep =
                retry_sweep(|| scoped_sweep_at(root, &[selected.clone(), active.clone()]));
            let mut complete = false;
            let mut busy = 0;
            for _ in 0..32 {
                let batch = sweep.next_batch(SweepBudget {
                    max_entries: 1,
                    max_duration: Duration::ZERO,
                });
                assert!(batch.entries_visited <= 1);
                assert!(!batch.round_interrupted);
                busy += batch.busy;
                if batch.round_complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete, "private stable scope must reach EOF");
            assert!(busy >= 1, "selected active listener must remain protected");
            assert!(sweep.next_batch(SweepBudget::default()).round_complete);
            assert!(!socket_path_at(root, &selected).exists());
            assert!(!lease_path_at(root, &selected).exists());
            assert_eq!(identity_of(&untouched_socket), socket_identity);
            assert_eq!(identity_of(&untouched_lease), lease_identity);
            assert_eq!(std::fs::read(&untouched_lease).unwrap(), lease_bytes);
        }
        assert!(matches!(
            active_listener.close(),
            EndpointReapResult::Reaped
        ));
        let mut empty = retry_sweep(|| scoped_sweep_at(root, &[]));
        let batch = empty.next_batch(SweepBudget::default());
        assert!(batch.round_complete);
        assert_eq!(batch.endpoints_examined, 0);
        assert_eq!(identity_of(&untouched_socket), socket_identity);
        assert_eq!(std::fs::read(&untouched_lease).unwrap(), lease_bytes);
    }
}

#[tokio::test]
async fn sweep_scope_interrupted_namespace_never_reports_completion() {
    let parent = tempfile::Builder::new()
        .prefix("c2i-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = namespace_path(parent.path());
    test_namespace_root(&root).unwrap();
    let endpoint = managed_endpoint_at(&root, "sweep-interrupt");
    let listener = bind_managed_at(&endpoint, &root).unwrap();
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));

    let mut sweep = retry_sweep(|| scoped_sweep_at(&root, &[endpoint.clone()]));
    let first = sweep.next_batch(SweepBudget {
        max_entries: 1,
        max_duration: Duration::ZERO,
    });
    assert!(!first.round_complete && !first.round_interrupted);

    let moved = parent.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(
        root.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

    let interrupted = sweep.next_batch(SweepBudget::default());
    assert!(interrupted.namespace_changed, "{interrupted:?}");
    assert!(interrupted.round_interrupted, "{interrupted:?}");
    assert!(!interrupted.round_complete, "{interrupted:?}");
    let terminal = sweep.next_batch(SweepBudget::default());
    assert!(terminal.round_interrupted, "{terminal:?}");
    assert!(!terminal.round_complete, "{terminal:?}");
    assert_eq!(terminal.entries_visited, 0);
}

#[test]
fn native_umask_child() {
    let (Ok(root), Ok(mask)) = (
        std::env::var("C2_NATIVE_UMASK_ROOT"),
        std::env::var("C2_NATIVE_UMASK"),
    ) else {
        return;
    };
    unsafe {
        libc::umask(libc::mode_t::from_str_radix(&mask, 8).unwrap());
    }
    let directory = EndpointDirectory::open(Path::new(&root), true).unwrap();
    directory.validate().unwrap();
    assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn native_namespace_creation_ignores_permissive_umask() {
    let parent = tempfile::Builder::new()
        .prefix("c2um-")
        .tempdir_in("/tmp")
        .unwrap();
    for mask in ["002", "000"] {
        let root = parent.path().join(mask);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix_managed::tests::native_umask_child",
                "--nocapture",
            ])
            .env("C2_NATIVE_UMASK_ROOT", &root)
            .env("C2_NATIVE_UMASK", mask)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::symlink_metadata(root).unwrap().mode() & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn native_full_backlog_probe_is_bounded_and_preserves_the_listener() {
    use socket2::{Domain, SockAddr, Socket, Type};
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let setup = managed_endpoint_at(&root, "backlog-setup");
    assert!(matches!(
        bind_managed_at(&setup, root).unwrap().close(),
        EndpointReapResult::Reaped
    ));
    let endpoint = managed_endpoint_at(&root, "backlog");
    let path = socket_path_at(root, &endpoint);
    let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
    let address = SockAddr::unix(&path).unwrap();
    listener.bind(&address).unwrap();
    listener.listen(1).unwrap();
    let identity = identity_of(&path);
    let mut clients = Vec::new();
    let mut saturated = false;
    for _ in 0..128 {
        let client = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        client.set_nonblocking(true).unwrap();
        if let Err(error) = client.connect(&address) {
            assert!(
                error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(libc::EINPROGRESS)
                    || error.kind() == io::ErrorKind::ConnectionRefused,
                "{error}"
            );
            saturated = true;
            break;
        }
        clients.push(client);
    }
    assert!(saturated, "fixture must fill listen backlog");
    let handle = tokio::runtime::Handle::current();
    let root = root.to_owned();
    let tested_endpoint = endpoint.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = std::thread::spawn(move || {
        let _entered = handle.enter();
        tx.send(
            bind_managed_at(&tested_endpoint, &root)
                .err()
                .map(|e| e.kind()),
        )
        .unwrap();
    });
    let outcome = rx.recv_timeout(Duration::from_secs(1));
    assert_eq!(
        identity_of(&path),
        identity,
        "duplicate probe changed active socket"
    );
    drop(listener);
    drop(clients);
    probe.join().unwrap();
    assert_eq!(outcome.unwrap(), Some(io::ErrorKind::AddrInUse));
}

#[tokio::test]
async fn native_restart_does_not_close_existing_streams() {
    use tokio::io::AsyncWriteExt;
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(&root, "old-stream");
    let mut first = bind_managed_at(&endpoint, root).unwrap();
    let path = socket_path_at(root, &endpoint);
    let (mut old_client, mut old_server) =
        tokio::try_join!(tokio::net::UnixStream::connect(&path), first.accept()).unwrap();
    assert!(matches!(first.close(), EndpointReapResult::Reaped));
    let mut second = bind_managed_at(&endpoint, root).unwrap();
    old_client.write_all(b"old").await.unwrap();
    let mut bytes = [0; 3];
    old_server.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"old");
    let (mut new_client, mut new_server) =
        tokio::try_join!(tokio::net::UnixStream::connect(&path), second.accept()).unwrap();
    new_client.write_all(b"new").await.unwrap();
    new_server.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"new");
    assert!(matches!(second.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn native_unknown_socket_and_unsafe_permissions_are_preserved() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let setup = managed_endpoint_at(&root, "unknown-setup");
    bind_managed_at(&setup, root).unwrap().close();
    let endpoint = managed_endpoint_at(&root, "unknown");
    let path = socket_path_at(root, &endpoint);
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let identity = identity_of(&path);
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership)
    ));
    let credential = EndpointCredential::unix_managed(endpoint.clone(), identity, [7; 16]);
    let lease = lease_path_at(root, &endpoint);
    assert!(!lease.exists());
    assert!(matches!(
        reap_managed_at(&endpoint, &credential, root),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership)
    ));
    assert_eq!(identity_of(&path), identity);
    assert!(
        !lease.exists(),
        "unknown socket reap must not create a lease"
    );
    assert_eq!(
        bind_managed_at(&endpoint, root).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert_eq!(identity_of(&path), identity);
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord)
    ));
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(bind_managed_at(&endpoint, root).is_err());
    assert_eq!(identity_of(&path), identity);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn native_concurrent_stale_bind_keeps_the_winner_reachable() {
    let endpoint = managed_endpoint("stale-race");
    let output = run_managed_process(&endpoint, "exit-owner");
    assert!(output.status.success(), "{output:?}");
    assert!(socket_path(&endpoint).exists());
    let root = socket_path(&endpoint).parent().unwrap().to_owned();
    let start = std::sync::Arc::new(std::sync::Barrier::new(8));
    let runtime = tokio::runtime::Handle::current();
    let results = std::thread::scope(|scope| {
        let contenders: Vec<_> = (0..8)
            .map(|_| {
                let start = start.clone();
                let endpoint = &endpoint;
                let root = &root;
                let runtime = &runtime;
                scope.spawn(move || {
                    let _entered = runtime.enter();
                    start.wait();
                    bind_managed_at(endpoint, root)
                })
            })
            .collect();
        contenders
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut winners = Vec::new();
    for result in results {
        match result {
            Ok(listener) => winners.push(listener),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}"),
        }
    }
    assert_eq!(winners.len(), 1, "one managed endpoint must have one owner");
    let mut winner = winners.pop().unwrap();
    let credential = winner.credential();
    assert!(matches!(
        reap_managed_at(&endpoint, &credential, &root),
        EndpointReapResult::Busy
    ));
    use tokio::io::AsyncWriteExt;
    let (mut client, mut server) = tokio::try_join!(
        tokio::net::UnixStream::connect(socket_path(&endpoint)),
        winner.accept()
    )
    .unwrap();
    client.write_all(b"owned").await.unwrap();
    let mut reply = [0; 5];
    server.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"owned");
    assert!(matches!(winner.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn socket_permission_and_listen_failures_withdraw_only_the_bound_identity() {
    use crate::unix_common::fault::{self, Failure};
    for failure in [
        Failure::SocketPermissions,
        Failure::SocketModeMismatch,
        Failure::Listen,
        Failure::SocketPermissionsAfterReplacement,
        Failure::ListenAfterReplacement,
    ] {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let unrelated = root.join("application-file");
        std::fs::write(&unrelated, b"preserved").unwrap();
        let endpoint = managed_endpoint_at(root, "initialization-failure");
        fault::inject(failure);
        let error = bind_managed_at(&endpoint, root)
            .err()
            .expect("injected failure must fail bind");
        assert!(
            !fault::take_if(failure),
            "injected failure must have been reached: {error}"
        );
        if failure == Failure::Listen {
            assert_eq!(
                error.raw_os_error(),
                Some(libc::ENOTSOCK),
                "the injected descriptor damage must reach the real OS listen: {error}"
            );
        }
        let path = socket_path_at(root, &endpoint);
        if matches!(
            failure,
            Failure::SocketPermissionsAfterReplacement | Failure::ListenAfterReplacement
        ) {
            assert_eq!(std::fs::read(path).unwrap(), b"replacement-marker");
        } else {
            assert!(
                !path.exists(),
                "{failure:?}: this bind's socket must be withdrawn"
            );
            let listener = bind_managed_at(&endpoint, root)
                .expect("failed bind must leave a reusable address");
            assert!(matches!(listener.close(), EndpointReapResult::Reaped));
        }
        assert_eq!(std::fs::read(unrelated).unwrap(), b"preserved");
        assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, 0o755);
    }
}

#[test]
fn socket_before_listen_umask_child() {
    use crate::unix_common::fault::{self, Failure};
    let Some(root) = std::env::var_os("C2_SOCKET_UMASK_ROOT") else {
        return;
    };
    let mask = std::env::var("C2_SOCKET_UMASK").unwrap();
    // Only this isolated child changes umask, never production or the parent harness.
    let mask = libc::mode_t::from_str_radix(&mask, 8).unwrap();
    unsafe {
        libc::umask(mask);
    }
    let root = Path::new(&root);
    let endpoint = managed_endpoint_at(root, "pre-listen");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            fault::inject(Failure::BeforeListen);
            let mut listener = bind_managed_at(&endpoint, root).unwrap();
            assert!(
                !fault::take_if(Failure::BeforeListen),
                "pre-listen checkpoint must run"
            );
            assert_eq!(
                std::fs::metadata(socket_path_at(root, &endpoint))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
            for name in [
                GATE_NAME.to_owned(),
                MARKER_NAME.to_owned(),
                lease_path_at(root, &endpoint)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ] {
                assert_eq!(
                    std::fs::metadata(root.join(name)).unwrap().mode() & 0o777,
                    0o600
                );
            }
            let (mut client, mut server) =
                tokio::try_join!(crate::platform::connect(&endpoint), listener.accept()).unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut client, b"private")
                .await
                .unwrap();
            let mut bytes = [0; 7];
            server.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"private");
            assert!(matches!(listener.close(), EndpointReapResult::Reaped));
        });
    // Read back the process mask in this isolated child and restore it immediately.
    let after = unsafe { libc::umask(mask) };
    assert_eq!(
        after, mask,
        "production bind must not change the process umask"
    );
    println!("PRE_LISTEN_0600_VERIFIED");
}

#[test]
fn socket_is_0600_before_listen_under_permissive_umask() {
    for mask in ["000", "002"] {
        let namespace = TestNamespace::new();
        std::fs::set_permissions(namespace.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix_managed::tests::socket_before_listen_umask_child",
                "--nocapture",
            ])
            .env("C2_SOCKET_UMASK_ROOT", namespace.path())
            .env("C2_SOCKET_UMASK", mask)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("PRE_LISTEN_0600_VERIFIED"));
    }
}

#[tokio::test]
async fn directory_policy_and_diagnostics_are_shared_by_all_entry_points() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let endpoint = managed_endpoint_at(root, "policy");
    let credential = EndpointCredential::unix_managed(
        endpoint.clone(),
        SocketIdentity {
            device: 1,
            inode: 2,
            changed_secs: 3,
            changed_nanos: 4,
        },
        [7; 16],
    );
    let mut rejected_modes = vec![0o777, 0o775, 0o770];
    if unsafe { libc::geteuid() } != 0 {
        // Root may have actual access despite absent owner bits. The policy
        // checks effective access, so these are denials only for an ordinary uid.
        rejected_modes.extend([0o555, 0o600, 0o300]);
    }
    for mode in rejected_modes {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).unwrap();
        let mut errors = vec![
            bind_managed_at(&endpoint, root).err().unwrap(),
            crate::platform::connect(&endpoint).await.err().unwrap(),
            ManagedSweep::for_scope(&endpoint, &[endpoint.clone()])
                .err()
                .unwrap(),
        ];
        match inspect_managed_at(&endpoint, root) {
            EndpointInspection::IoError(error) => errors.push(error),
            other => panic!("directory diagnostic lost in inspect: {other:?}"),
        }
        match reap_managed_at(&endpoint, &credential, root) {
            EndpointReapResult::IoError(error) => errors.push(error),
            other => panic!("directory diagnostic lost in reap: {other:?}"),
        }
        for error in errors {
            let message = error.to_string();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{message}");
            assert!(message.contains(&root.display().to_string()), "{message}");
            assert!(
                message.contains(&format!("owner {}", unsafe { libc::geteuid() })),
                "{message}"
            );
            assert!(message.contains(&format!("mode {mode:04o}")), "{message}");
            assert!(
                message.contains("must not have write")
                    || message.contains("lacks read/write/traverse")
                    || message.contains("cannot open"),
                "{message}"
            );
        }
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        std::fs::read_dir(root).unwrap().count(),
        0,
        "failed directory validation must create nothing"
    );
    for mode in [0o700, 0o750, 0o755] {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).unwrap();
        let directory = EndpointDirectory::open(root, false).unwrap();
        directory.validate().unwrap();
        assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, mode);
    }
}

#[test]
fn final_directory_symlink_and_replacement_are_refused_with_context() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    let moved = root.with_file_name("moved");
    let directory = EndpointDirectory::open(root, false).unwrap();
    std::fs::rename(root, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, root).unwrap();
    let error = EndpointDirectory::open(root, false).err().unwrap();
    let message = error.to_string();
    assert!(
        message.contains(&root.display().to_string())
            && message.contains("owner")
            && message.contains("mode")
            && message.contains("symlinks"),
        "{message}"
    );
    assert!(!directory.path_still_names_open_directory());
    std::fs::remove_file(root).unwrap();
    std::fs::create_dir(root).unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = directory.validate().unwrap_err().to_string();
    assert!(
        error.contains(&root.display().to_string())
            && error.contains("owner")
            && error.contains("mode")
            && error.contains("no longer names"),
        "{error}"
    );
    assert!(moved.is_dir());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn cross_user_socket_child() {
    let Some(root) = std::env::var_os("C2_CROSS_USER_ROOT") else {
        return;
    };
    let address = std::env::var("C2_CROSS_USER_ADDRESS").unwrap();
    let context = LocalEndpointContext::with_unix_root(Path::new(&root)).unwrap();
    let endpoint = context.endpoint(&address).unwrap();
    let error = std::os::unix::net::UnixStream::connect(endpoint.os_name()).unwrap_err();
    assert_eq!(
        error.kind(),
        io::ErrorKind::PermissionDenied,
        "a second uid must not connect: {error}"
    );
    let error = EndpointDirectory::open(Path::new(&root), false)
        .err()
        .unwrap();
    let message = error.to_string();
    assert!(
        message.contains("owner must equal")
            && message.contains("owner 0")
            && message.contains("mode 0755"),
        "{message}"
    );
    println!("CROSS_USER_DENIED_VERIFIED");
}

#[tokio::test]
#[ignore = "requires root to spawn a child as a second ordinary uid; run explicitly on a suitable host"]
async fn cross_user_cannot_connect_to_0600_socket_in_0755_directory() {
    use std::os::unix::process::CommandExt;
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "this real cross-user test requires root; do not count a skipped child as evidence"
    );
    // Use the actual local nobody account rather than assuming a platform uid.
    let account = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
    assert!(!account.is_null(), "a second ordinary account must exist");
    let (uid, gid) = unsafe { ((*account).pw_uid, (*account).pw_gid) };
    assert_ne!(uid, 0);
    let namespace = TestNamespace::new();
    std::fs::set_permissions(
        namespace._parent.path(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(namespace.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let endpoint = managed_endpoint_at(namespace.path(), "cross-user");
    let listener = bind_managed_at(&endpoint, namespace.path()).unwrap();
    let identity = identity_of(&socket_path(&endpoint));
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unix_managed::tests::cross_user_socket_child",
            "--nocapture",
        ])
        .env("C2_CROSS_USER_ROOT", namespace.path())
        .env("C2_CROSS_USER_ADDRESS", endpoint.address())
        .gid(gid)
        .uid(uid)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("CROSS_USER_DENIED_VERIFIED"));
    assert_eq!(identity_of(&socket_path(&endpoint)), identity);
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn missing_coordinator_in_0755_directory_is_never_recreated() {
    let namespace = TestNamespace::new();
    let root = namespace.path();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    drop(ManagedNamespace::open_root(root, true, true).unwrap());
    let marker = std::fs::read(root.join(MARKER_NAME)).unwrap();
    std::fs::remove_file(root.join(GATE_NAME)).unwrap();
    let endpoint = managed_endpoint_at(root, "missing-gate-0755");
    let credential = EndpointCredential::unix_managed(
        endpoint.clone(),
        SocketIdentity {
            device: 1,
            inode: 2,
            changed_secs: 3,
            changed_nanos: 4,
        },
        [7; 16],
    );
    assert!(matches!(
        ManagedNamespace::open_root(root, true, false),
        Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorMissing
        ))
    ));
    assert!(bind_managed_at(&endpoint, root).is_err());
    assert!(matches!(
        inspect_managed_at(&endpoint, root),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorMissing)
    ));
    assert!(matches!(
        reap_managed_at(&endpoint, &credential, root),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::CoordinatorMissing)
    ));
    assert!(!root.join(GATE_NAME).exists());
    assert_eq!(std::fs::read(root.join(MARKER_NAME)).unwrap(), marker);
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
}

/// The positive pre-listen check must fail if a fixture listens too early.
/// This changes only the fixture socket; production listen ordering is intact.
#[tokio::test]
async fn before_listen_checkpoint_rejects_an_already_listening_socket() {
    use crate::unix_common::fault::{self, Failure};
    let fixture = TestNamespace::new();
    let root = fixture.path();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let endpoint = managed_endpoint_at(root, "pre-listen-negative-control");
    let namespace = open_root_bounded(root, true).unwrap();
    let names = managed_names(&endpoint).unwrap();
    let lease = namespace
        .directory
        .open_file(
            &names.lock,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .unwrap()
        .unwrap();
    try_lock_lease(&lease).unwrap();
    let socket = bind_in_verified_directory(&namespace, &names).unwrap();
    let guard = BoundSocketGuard::capture(&namespace.directory, &names, &lease).unwrap();
    set_socket_permissions(&namespace.directory, &names, &guard).unwrap();
    socket.listen(128).unwrap();
    fault::inject(Failure::BeforeListen);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        listen_verified_socket(&socket, &namespace.directory, &names, &guard)
    }))
    .expect_err("an already listening socket must fail the pre-listen checkpoint");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("same-user connect must be refused before listen"),
        "the actual connect assertion must reject the early listener: {message}"
    );
    assert!(
        !fault::take_if(Failure::BeforeListen),
        "checkpoint must have run"
    );
    // The guard still owns the exact bound inode and cleans only this fixture.
    drop(guard);
    assert!(!socket_path(&endpoint).exists());
}
