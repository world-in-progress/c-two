//! Split-directory regression tests. Every root is owned by this test process;
//! the real default endpoint namespace is never opened.
use super::*;
use crate::unix_common::fault::{self, Failure};
use std::fs::OpenOptions;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::process::{Child, Command};
use std::time::Duration;
use tokio::io::AsyncReadExt;

struct TestRoots(Option<(PathBuf, PathBuf)>);

impl TestRoots {
    fn install(socket: &Path, records: &Path) -> Self {
        Self(
            directories::TEST_ROOTS
                .with(|roots| roots.replace(Some((socket.to_owned(), records.to_owned())))),
        )
    }
}

impl Drop for TestRoots {
    fn drop(&mut self) {
        directories::TEST_ROOTS.with(|roots| {
            roots.replace(self.0.take());
        });
    }
}

struct SplitFixture {
    _parent: tempfile::TempDir,
    sockets: PathBuf,
    records: PathBuf,
    alias: PathBuf,
    _roots: TestRoots,
}

impl SplitFixture {
    fn new() -> Self {
        let parent = tempfile::Builder::new()
            .prefix("c2-split-")
            .tempdir_in("/tmp")
            .unwrap();
        let sockets = parent.path().join("ipc");
        let records = parent.path().join("coord");
        fs::create_dir(&sockets).unwrap();
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700)).unwrap();
        let alias_parent = parent.path().join("alias");
        symlink(parent.path(), &alias_parent).unwrap();
        let alias = alias_parent.join("ipc");
        let roots = TestRoots::install(&sockets, &records);
        Self {
            _parent: parent,
            sockets,
            records,
            alias,
            _roots: roots,
        }
    }

    fn endpoint(&self, root: &Path, label: &str) -> LocalEndpoint {
        let address = format!("ipc://{label}-{}", uuid::Uuid::new_v4().simple());
        LocalEndpointContext::with_unix_root(root)
            .unwrap()
            .endpoint(&address)
            .unwrap()
    }

    fn slot(&self, root: &Path, endpoint: &LocalEndpoint, suffix: &str) -> PathBuf {
        let mut name = endpoint_stem(endpoint).unwrap().into_vec();
        name.extend_from_slice(suffix.as_bytes());
        root.join(OsString::from_vec(name))
    }

    fn socket(&self, endpoint: &LocalEndpoint) -> PathBuf {
        self.slot(&self.sockets, endpoint, "")
    }

    fn lease(&self, endpoint: &LocalEndpoint) -> PathBuf {
        self.slot(&self.records, endpoint, LEASE_SUFFIX)
    }

    fn assert_only_coordinator(&self) {
        let mut entries: Vec<_> = fs::read_dir(&self.records)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [OsString::from(GATE_NAME), OsString::from(MARKER_NAME)]
        );
        assert_eq!(fs::read_dir(&self.sockets).unwrap().count(), 0);
    }
}

fn sweep(endpoint: &LocalEndpoint) -> ManagedSweep {
    // The native sweep lease is process-wide, including parallel legacy tests.
    for _ in 0..400 {
        match ManagedSweep::for_endpoint(endpoint) {
            Ok(sweep) => return sweep,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("sweep open failed: {error}"),
        }
    }
    panic!("sweep lease remained busy");
}

#[tokio::test]
async fn split_bind_alias_exchange_and_close() {
    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "exchange");
    let alias = LocalEndpointContext::with_unix_root(&fixture.alias)
        .unwrap()
        .endpoint(endpoint.address())
        .unwrap();
    let mut listener = crate::LocalListener::bind(&endpoint).unwrap();
    assert!(fixture.socket(&endpoint).exists());
    assert!(fixture.lease(&endpoint).exists());
    assert!(fixture.records.join(GATE_NAME).exists());
    assert!(!fixture.sockets.join(GATE_NAME).exists());
    assert_eq!(
        crate::LocalListener::bind(&alias).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    let (mut client, mut server) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            crate::LocalStream::connect(&alias, crate::DEFAULT_CONNECT_TIMEOUT),
            listener.accept(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    server.write_all(b"split").await.unwrap();
    let mut received = [0; 5];
    client.read_exact(&mut received).await.unwrap();
    assert_eq!(&received, b"split");
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    fixture.assert_only_coordinator();
}

#[tokio::test]
async fn split_gate_pin_and_directory_replacements_fail_closed() {
    for replace_records in [false, true] {
        let fixture = SplitFixture::new();
        let endpoint = fixture.endpoint(&fixture.sockets, "replace-dir");
        let listener = crate::LocalListener::bind(&endpoint).unwrap();
        let path = if replace_records {
            &fixture.records
        } else {
            &fixture.sockets
        };
        let mut sweep = sweep(&endpoint);
        let saved = fixture._parent.path().join("saved");
        fs::rename(path, &saved).unwrap();
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("replacement"), b"foreign").unwrap();
        let batch = sweep.next_batch(SweepBudget::default());
        assert!(
            batch.round_interrupted && batch.namespace_changed,
            "{batch:?}"
        );
        assert!(!matches!(listener.close(), EndpointReapResult::Reaped));
        assert_eq!(fs::read(path.join("replacement")).unwrap(), b"foreign");
        let original_entry = if replace_records {
            saved.join(GATE_NAME)
        } else {
            fixture.slot(&saved, &endpoint, "")
        };
        assert!(original_entry.exists());
    }

    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "pin-gate");
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let gate = fixture.records.join(GATE_NAME);
    let original = fstat(listener.0._gate_pin.as_raw_fd()).unwrap();
    fs::remove_file(&gate).unwrap();
    assert_eq!(fstat(listener.0._gate_pin.as_raw_fd()).unwrap().st_nlink, 0);
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
        inspect_managed(&endpoint),
        EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced)
    ));
    assert!(!matches!(listener.close(), EndpointReapResult::Reaped));
    assert!(fixture.socket(&endpoint).exists());
}

#[tokio::test]
async fn split_socket_loss_exact_reap_and_stale_credential() {
    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "loss");
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let old = listener.credential();
    listener.0.abandon_for_test();
    fs::remove_file(fixture.socket(&endpoint)).unwrap();
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &old),
        EndpointReapResult::Reaped
    ));
    assert!(!fixture.lease(&endpoint).exists());

    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let current = listener.credential();
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &old),
        EndpointReapResult::Busy | EndpointReapResult::StaleTarget
    ));
    assert!(fixture.socket(&endpoint).exists());
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));

    // Losing the entire temporary socket directory still permits exact
    // retirement of a complete, unlocked record in the durable coordinator.
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let lost = listener.credential();
    listener.0.abandon_for_test();
    fs::remove_file(fixture.socket(&endpoint)).unwrap();
    fs::remove_dir(&fixture.sockets).unwrap();
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &lost),
        EndpointReapResult::Reaped
    ));
    fs::create_dir(&fixture.sockets).unwrap();
    fs::set_permissions(&fixture.sockets, fs::Permissions::from_mode(0o700)).unwrap();
    let replacement = crate::LocalListener::bind(&endpoint).unwrap();
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &current),
        EndpointReapResult::Busy | EndpointReapResult::StaleTarget
    ));
    assert!(fixture.socket(&endpoint).exists());
    assert!(matches!(replacement.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn split_sweep_counts_lease_only_and_unknown_socket_in_one_budget() {
    let fixture = SplitFixture::new();
    let orphan = fixture.endpoint(&fixture.sockets, "orphan");
    let listener = crate::LocalListener::bind(&orphan).unwrap();
    listener.0.abandon_for_test();
    fs::remove_file(fixture.socket(&orphan)).unwrap();
    let unknown = fixture.endpoint(&fixture.sockets, "unknown");
    fs::write(fixture.socket(&unknown), b"foreign").unwrap();
    let mut sweep = sweep(&orphan);
    let mut reaped = 0;
    let mut unverified = 0;
    let mut complete = false;
    for _ in 0..16 {
        let batch = sweep.next_batch(SweepBudget {
            max_entries: 1,
            max_duration: Duration::ZERO,
        });
        assert!(batch.entries_visited <= 1, "{batch:?}");
        assert!(!batch.round_interrupted, "{batch:?}");
        reaped += batch.leases_retired;
        unverified += batch.unverified;
        if batch.round_complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(reaped, 1);
    assert!(unverified >= 1);
    assert!(!fixture.lease(&orphan).exists());
    assert_eq!(fs::read(fixture.socket(&unknown)).unwrap(), b"foreign");
}

#[tokio::test]
async fn split_invalid_lease_and_failed_bind_preserve_foreign_records() {
    let fixture = SplitFixture::new();
    let seed = fixture.endpoint(&fixture.sockets, "seed");
    let listener = crate::LocalListener::bind(&seed).unwrap();
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    let empty = fixture.endpoint(&fixture.sockets, "empty");
    let lease = fixture.lease(&empty);
    let _file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lease)
        .unwrap();
    assert!(matches!(
        reap_slot(empty.context(), &endpoint_stem(&empty).unwrap()),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
    ));
    assert_eq!(fs::metadata(&lease).unwrap().len(), 0);

    let invalid = fixture.endpoint(&fixture.sockets, "invalid");
    let invalid_lease = fixture.lease(&invalid);
    let mut invalid_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&invalid_lease)
        .unwrap();
    invalid_file.write_all(b"broken").unwrap();
    drop(invalid_file);
    assert!(matches!(
        reap_slot(invalid.context(), &endpoint_stem(&invalid).unwrap()),
        EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
    ));
    assert_eq!(fs::read(&invalid_lease).unwrap(), b"broken");

    let failed = fixture.endpoint(&fixture.sockets, "rollback");
    fault::inject(Failure::ManagedRecordWrite);
    assert!(crate::LocalListener::bind(&failed).is_err());
    assert!(!fault::take_if(Failure::ManagedRecordWrite));
    assert!(!fixture.socket(&failed).exists());
    assert!(!fixture.lease(&failed).exists());
    assert!(lease.exists(), "rollback may remove only its own new lease");
    assert!(invalid_lease.exists());
}

#[test]
fn split_process_fixture() {
    let Ok(address) = std::env::var("C2_SPLIT_TEST_ADDRESS") else {
        return;
    };
    let sockets = PathBuf::from(std::env::var_os("C2_SPLIT_TEST_SOCKETS").unwrap());
    let records = PathBuf::from(std::env::var_os("C2_SPLIT_TEST_RECORDS").unwrap());
    let _roots = TestRoots::install(&sockets, &records);
    let endpoint = LocalEndpointContext::with_unix_root(&sockets)
        .unwrap()
        .endpoint(&address)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let _listener = crate::LocalListener::bind(&endpoint).unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn split_killed_owner_os_wait_then_exact_reap() {
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "child");
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .arg("split_process_fixture")
            .env("C2_SPLIT_TEST_ADDRESS", endpoint.address())
            .env("C2_SPLIT_TEST_SOCKETS", &fixture.sockets)
            .env("C2_SPLIT_TEST_RECORDS", &fixture.records)
            .spawn()
            .unwrap(),
    );
    let mut credential = None;
    for _ in 0..500 {
        if let EndpointInspection::Present(found) = inspect_managed(&endpoint) {
            credential = Some(found);
            break;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("child exited before bind: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let credential = credential.expect("child endpoint became inspectable");
    child.0.kill().unwrap();
    child.0.wait().unwrap(); // OS wait is the death proof for this exact owner.
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &credential),
        EndpointReapResult::Reaped
    ));
    fixture.assert_only_coordinator();
}

#[tokio::test]
async fn custom_root_keeps_socket_and_records_together() {
    let fixture = SplitFixture::new();
    let custom = fixture._parent.path().join("custom");
    fs::create_dir(&custom).unwrap();
    fs::set_permissions(&custom, fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = fixture.endpoint(&custom, "custom");
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    assert!(custom.join(GATE_NAME).exists());
    assert!(fixture.slot(&custom, &endpoint, LEASE_SUFFIX).exists());
    assert!(fixture.slot(&custom, &endpoint, "").exists());
    assert!(!fixture.records.exists());
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
}

#[tokio::test]
async fn split_preserves_legacy_lease_and_refuses_slot_adoption() {
    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "legacy");
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let credential = listener.credential();
    assert!(matches!(listener.close(), EndpointReapResult::Reaped));
    let legacy = fixture.slot(&fixture.sockets, &endpoint, LEASE_SUFFIX);
    fs::write(&legacy, b"old-format-or-incomplete").unwrap();
    assert!(crate::LocalListener::bind(&endpoint).is_err());
    assert!(matches!(
        inspect_managed(&endpoint),
        EndpointInspection::Unverified(_)
    ));
    assert!(matches!(
        crate::reap_endpoint(&endpoint, &credential),
        EndpointReapResult::Unverified(_)
    ));
    let batch = sweep(&endpoint).next_batch(SweepBudget::default());
    assert!(batch.round_complete, "{batch:?}");
    assert_eq!(batch.unverified, 1);
    assert_eq!(batch.already_absent, 0);
    assert_eq!(fs::read(legacy).unwrap(), b"old-format-or-incomplete");
    assert!(!fixture.lease(&endpoint).exists());
}

#[tokio::test]
async fn split_lost_socket_root_keeps_live_lease_and_alias_reap() {
    let fixture = SplitFixture::new();
    let endpoint = fixture.endpoint(&fixture.sockets, "lost-live");
    let alias = LocalEndpointContext::with_unix_root(&fixture.alias)
        .unwrap()
        .endpoint(endpoint.address())
        .unwrap();
    let listener = crate::LocalListener::bind(&endpoint).unwrap();
    let alias_credential = match inspect_managed(&alias) {
        EndpointInspection::Present(credential) => credential,
        other => panic!("alias inspection: {other:?}"),
    };
    fs::remove_file(fixture.socket(&endpoint)).unwrap();
    fs::remove_dir(&fixture.sockets).unwrap();
    assert!(matches!(
        crate::reap_endpoint(&alias, &alias_credential),
        EndpointReapResult::Busy
    ));
    // Explicit aliases remain application-created roots, just like before.
    assert_eq!(
        crate::LocalListener::bind(&alias).err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        crate::LocalListener::bind(&endpoint).err().unwrap().kind(),
        io::ErrorKind::AddrInUse
    );
    assert!(matches!(
        listener.close(),
        EndpointReapResult::Unverified(_)
    ));
    assert!(fixture.lease(&endpoint).exists());
    assert!(matches!(
        crate::reap_endpoint(&alias, &alias_credential),
        EndpointReapResult::Reaped
    ));
    let restarted = crate::LocalListener::bind(&endpoint).unwrap();
    assert!(matches!(restarted.close(), EndpointReapResult::Reaped));
    fixture.assert_only_coordinator();
}
