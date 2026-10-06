use super::{
    EndpointCredential, EndpointInspection, EndpointReapResult, LocalEndpoint, SweepBatch,
    SweepBudget,
};
use crate::unix_endpoint::{
    BoundSocketGuard, EndpointDirectory, EndpointNames, Ownership, SocketIdentity, bind_lock_error,
    cleanup_listener_socket, endpoint_names, inspect_endpoint as inspect_native, ownership_lock,
    reap_endpoint as reap_native, record_bound_socket, remove_stale_socket, set_socket_permissions,
};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::net::{UnixListener, UnixStream};

pub type Stream = UnixStream;

pub async fn connect(endpoint: &LocalEndpoint) -> io::Result<Stream> {
    UnixStream::connect(Path::new(endpoint.os_name())).await
}

pub fn raw_handle(stream: &Stream) -> usize {
    stream.as_raw_fd() as usize
}

pub fn cancel(raw: usize) {
    unsafe {
        libc::shutdown(raw as i32, libc::SHUT_RDWR);
    }
}

pub fn poll_flush(stream: &mut Stream, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    Pin::new(stream).poll_flush(cx)
}

pub struct Listener {
    state: ListenerState,
}

enum ListenerState {
    Legacy(LegacyListener),
    Managed(crate::unix_managed::ManagedListener),
}

struct LegacyListener {
    // Keep this order: cleanup occurs in Drop while the listening socket is
    // still open; the listener field then closes before the v1 flock is
    // released. The lock inode is intentionally never unlinked.
    inner: UnixListener,
    endpoint: LocalEndpoint,
    directory: EndpointDirectory,
    names: EndpointNames,
    identity: SocketIdentity,
    _ownership: Ownership,
    cleaned: bool,
}

impl Listener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        match endpoint.protocol() {
            c2_config::LocalEndpointProtocol::ManagedV2 => {
                crate::unix_managed::bind_managed(endpoint).map(|listener| Self {
                    state: ListenerState::Managed(listener),
                })
            }
            _ => {
                let directory = EndpointDirectory::for_endpoint(endpoint, true)?;
                Self::bind_legacy(endpoint, directory)
            }
        }
    }

    fn bind_legacy(endpoint: &LocalEndpoint, directory: EndpointDirectory) -> io::Result<Self> {
        let legacy = LegacyListener::bind_in(endpoint, directory)?;
        Ok(Self {
            state: ListenerState::Legacy(legacy),
        })
    }

    pub fn credential(&self) -> EndpointCredential {
        match &self.state {
            ListenerState::Legacy(listener) => listener.credential(),
            ListenerState::Managed(listener) => listener.credential(),
        }
    }

    pub fn close(self) -> EndpointReapResult {
        match self.state {
            ListenerState::Legacy(listener) => listener.close(),
            ListenerState::Managed(listener) => listener.close(),
        }
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        match &mut self.state {
            ListenerState::Legacy(listener) => listener.accept().await,
            ListenerState::Managed(listener) => listener.accept().await,
        }
    }

    #[cfg(test)]
    fn ownership_file(&self) -> Option<&std::fs::File> {
        match &self.state {
            ListenerState::Legacy(listener) => Some(&listener._ownership.0),
            ListenerState::Managed(_) => None,
        }
    }
}

impl LegacyListener {
    fn bind_in(endpoint: &LocalEndpoint, directory: EndpointDirectory) -> io::Result<Self> {
        let names = endpoint_names(endpoint)?;
        let ownership = ownership_lock(&directory, &names.lock).map_err(bind_lock_error)?;
        remove_stale_socket(&directory, &names, &ownership.0)?;
        let inner = UnixListener::bind(directory.socket_path(&names.socket_os))?;
        // Everything after a successful bind must either finish initialization
        // or withdraw the exact object this call created. The guard is declared
        // after `ownership` so its rollback runs while this instance still
        // holds the v1 ownership lock and before the listening descriptor
        // closes; it never depends on the owner record it may have failed to
        // write, and an early `?` return drops it.
        let mut bound = BoundSocketGuard::capture(&directory, &names, &ownership.0)?;
        set_socket_permissions(&directory, &names)?;
        let identity = record_bound_socket(&directory, &names, &ownership.0)?;
        bound.disarm();
        drop(bound);
        Ok(Self {
            inner,
            endpoint: endpoint.clone(),
            directory,
            names,
            identity,
            _ownership: ownership,
            cleaned: false,
        })
    }

    fn cleanup(&mut self) -> EndpointReapResult {
        cleanup_listener_socket(
            &self.directory,
            &self.names,
            &self._ownership.0,
            self.identity,
        )
    }

    fn credential(&self) -> EndpointCredential {
        EndpointCredential::unix(self.endpoint.clone(), self.identity)
    }

    fn close(mut self) -> EndpointReapResult {
        let result = self.cleanup();
        self.cleaned = true;
        result
    }

    async fn accept(&mut self) -> io::Result<Stream> {
        self.inner.accept().await.map(|(stream, _)| stream)
    }
}

impl Drop for LegacyListener {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
            self.cleaned = true;
        }
    }
}

#[cfg(test)]
fn test_bind_in(endpoint: &LocalEndpoint, root: &std::path::Path) -> io::Result<Listener> {
    Listener::bind_legacy(endpoint, EndpointDirectory::open(root, false)?)
}

#[cfg(test)]
fn inspect_in(root: &std::path::Path, endpoint: &LocalEndpoint) -> EndpointInspection {
    match EndpointDirectory::open(root, false) {
        Ok(directory) => crate::unix_endpoint::inspect_in(&directory, endpoint),
        Err(error) => crate::unix_endpoint::inspection_error(error),
    }
}

#[cfg(test)]
fn reap_in(
    root: &std::path::Path,
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    match EndpointDirectory::open(root, false) {
        Ok(directory) => crate::unix_endpoint::reap_in(&directory, endpoint, credential),
        Err(error) => crate::unix_endpoint::reap_error(error),
    }
}

pub(crate) fn inspect_endpoint(endpoint: &LocalEndpoint) -> EndpointInspection {
    if endpoint.protocol() == c2_config::LocalEndpointProtocol::ManagedV2 {
        return crate::unix_managed::inspect_managed(endpoint);
    }
    inspect_native(endpoint)
}

pub(crate) fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    if endpoint.protocol() == c2_config::LocalEndpointProtocol::ManagedV2 {
        return crate::unix_managed::reap_managed(endpoint, credential);
    }
    reap_native(endpoint, credential)
}

/// Protocol-aware maintenance sweep. Managed-v2 endpoints enumerate their real
/// versioned namespace; legacy endpoints keep the v1 namespace unchanged.
pub(crate) enum EndpointSweep {
    Legacy(crate::unix_endpoint::EndpointSweep),
    Managed(crate::unix_managed::ManagedSweep),
}

impl EndpointSweep {
    pub(crate) fn open() -> io::Result<Self> {
        crate::unix_endpoint::EndpointSweep::open().map(Self::Legacy)
    }

    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        if endpoint.protocol() == c2_config::LocalEndpointProtocol::ManagedV2 {
            crate::unix_managed::ManagedSweep::for_endpoint(endpoint).map(Self::Managed)
        } else {
            crate::unix_endpoint::EndpointSweep::for_endpoint(endpoint).map(Self::Legacy)
        }
    }

    pub(crate) fn for_scope(
        endpoint: &LocalEndpoint,
        targets: &[LocalEndpoint],
    ) -> io::Result<Self> {
        if endpoint.protocol() == c2_config::LocalEndpointProtocol::ManagedV2 {
            crate::unix_managed::ManagedSweep::for_scope(endpoint, targets).map(Self::Managed)
        } else {
            crate::unix_endpoint::EndpointSweep::for_scope(endpoint, targets).map(Self::Legacy)
        }
    }

    pub(crate) fn next_batch(&mut self, budget: SweepBudget) -> SweepBatch {
        match self {
            Self::Legacy(sweep) => sweep.next_batch(budget),
            Self::Managed(sweep) => sweep.next_batch(budget),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unix_endpoint::{
        EndpointSweep, SWEEP_NAMESPACE_PROBE, SocketIdentity, endpoint_names, fault,
    };
    use crate::{EndpointUnverifiedReason, SweepBatch, SweepBudget};
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An isolated, private namespace root under `/tmp`.
    ///
    /// The Unix sockaddr path bound is short on macOS, so every fixture lives
    /// under a short random `/tmp` directory instead of inheriting the checkout
    /// or Cargo target path length. Each namespace owns only its own UUID
    /// endpoints and is removed together with its own sockets.
    struct TestNamespace {
        root: tempfile::TempDir,
    }

    impl TestNamespace {
        fn new() -> Self {
            let root = tempfile::Builder::new()
                .prefix("c2l1-")
                .tempdir_in("/tmp")
                .expect("isolated endpoint root under /tmp");
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let namespace = Self { root };
            assert!(
                namespace.path().as_os_str().as_bytes().len() <= 32,
                "test root must leave room for a bounded socket path: {}",
                namespace.path().display()
            );
            namespace
        }

        fn path(&self) -> &Path {
            self.root.path()
        }
    }

    fn test_endpoint(label: &str) -> LocalEndpoint {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        LocalEndpoint::from_address(&format!("ipc://{label}-{}", &unique[..16])).unwrap()
    }

    fn bounded_path(root: &Path, name: std::ffi::OsString) -> PathBuf {
        let path = root.join(name);
        let length = path.as_os_str().as_bytes().len();
        assert!(
            length < 100,
            "test socket path exceeds the SUN_LEN budget ({length} bytes): {}",
            path.display()
        );
        path
    }

    fn socket_path(root: &Path, endpoint: &LocalEndpoint) -> PathBuf {
        let names = endpoint_names(endpoint).unwrap();
        bounded_path(root, names.socket_os)
    }

    fn lock_path(root: &Path, endpoint: &LocalEndpoint) -> PathBuf {
        let names = endpoint_names(endpoint).unwrap();
        bounded_path(
            root,
            std::ffi::OsStr::from_bytes(names.lock.as_bytes()).to_owned(),
        )
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

    fn raw_stale_socket(root: &Path, endpoint: &LocalEndpoint) -> SocketIdentity {
        let path = socket_path(root, endpoint);
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        identity_of(&path)
    }

    fn write_corrupt_record(root: &Path, endpoint: &LocalEndpoint) {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(lock_path(root, endpoint))
            .unwrap();
        file.write_all(b"c2sock01-corrupt").unwrap();
        file.sync_all().unwrap();
    }

    fn reap_in_temp(
        root: &Path,
        endpoint: &LocalEndpoint,
        credential: &EndpointCredential,
    ) -> EndpointReapResult {
        reap_in(root, endpoint, credential)
    }

    fn bind_error_kind(root: &Path, endpoint: &LocalEndpoint) -> Option<io::ErrorKind> {
        test_bind_in(endpoint, root).err().map(|error| error.kind())
    }

    #[tokio::test]
    async fn listener_unlocks_ownership_before_inherited_descriptors_are_closed() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("inherited-lock");
        let listener = test_bind_in(&endpoint, root).unwrap();
        // fork during a concurrent process spawn duplicates this open file
        // description until CLOEXEC takes effect in the child.
        let inherited = listener.ownership_file().unwrap().try_clone().unwrap();
        drop(listener);
        let _next = test_bind_in(&endpoint, root).unwrap();
        drop(inherited);
    }

    #[test]
    fn crash_owner_child() {
        let (Ok(address), Ok(root)) = (
            std::env::var("C2_LOCAL_TEST_CRASH_OWNER"),
            std::env::var("C2_LOCAL_TEST_CRASH_ROOT"),
        ) else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let _listener = test_bind_in(&endpoint, Path::new(&root)).unwrap();
        // Process exit releases kernel handles and flock without Rust Drop.
        std::process::exit(0);
    }

    #[test]
    fn umask_child() {
        let (Ok(path), Ok(umask)) = (
            std::env::var("C2_LOCAL_TEST_UMASK_DIR"),
            std::env::var("C2_LOCAL_TEST_UMASK"),
        ) else {
            return;
        };
        let umask = libc::mode_t::from_str_radix(&umask, 8).unwrap();
        // The child owns this process-wide setting; the parallel harness never
        // changes its own umask.
        unsafe {
            libc::umask(umask);
        }
        match EndpointDirectory::open(Path::new(&path), true) {
            Ok(directory) => {
                drop(directory);
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("namespace creation failed: {error}");
                std::process::exit(3);
            }
        }
    }

    #[test]
    fn new_namespace_directory_ignores_permissive_process_umask() {
        let parent = tempfile::Builder::new()
            .prefix("c2l1-umask-")
            .tempdir_in("/tmp")
            .unwrap();
        for umask in ["002", "000"] {
            let namespace = parent.path().join("ns");
            let _ = std::fs::remove_dir_all(&namespace);
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "platform::tests::umask_child", "--nocapture"])
                .env("C2_LOCAL_TEST_UMASK_DIR", &namespace)
                .env("C2_LOCAL_TEST_UMASK", umask)
                .output()
                .unwrap();
            assert!(child.status.success(), "umask {umask}: {child:?}");
            let mode = std::fs::symlink_metadata(&namespace).unwrap().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "umask {umask} must not relax the namespace mode"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_stale_startup_keeps_the_winner_reachable() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("stale-race");
        let path = socket_path(root, &endpoint);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::tests::crash_owner_child",
                "--nocapture",
            ])
            .env("C2_LOCAL_TEST_CRASH_OWNER", endpoint.address())
            .env("C2_LOCAL_TEST_CRASH_ROOT", root)
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        assert!(path.exists(), "crash fixture must leave its socket behind");

        let start = Arc::new(Barrier::new(8));
        let runtime = tokio::runtime::Handle::current();
        let results = std::thread::scope(|scope| {
            let contenders: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let _entered = runtime.enter();
                        start.wait();
                        test_bind_in(&endpoint, root)
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
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse),
            }
        }
        assert_eq!(winners.len(), 1, "one stale endpoint must have one owner");
        let mut winner = winners.pop().unwrap();
        let (mut client, mut server) =
            tokio::try_join!(UnixStream::connect(&path), winner.accept()).unwrap();
        client.write_all(b"owned").await.unwrap();
        let mut reply = [0; 5];
        server.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"owned");
        drop(client);
        drop(server);
        drop(winner);
        assert!(test_bind_in(&endpoint, root).is_ok());
    }

    #[tokio::test]
    async fn probing_a_full_backlog_never_waits_for_accept() {
        let namespace = TestNamespace::new();
        let root = namespace.path().to_owned();
        let endpoint = test_endpoint("full-backlog");
        let path = socket_path(&root, &endpoint);
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        let address = SockAddr::unix(&path).unwrap();
        listener.bind(&address).unwrap();
        listener.listen(1).unwrap();
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
                    "unexpected backlog error: {error}"
                );
                saturated = true;
                break;
            }
            clients.push(client);
        }
        assert!(saturated, "fixture must fill the kernel listen queue");

        let runtime = tokio::runtime::Handle::current();
        let (tx, rx) = std::sync::mpsc::channel();
        let probe_root = root.clone();
        let probe_endpoint = endpoint.clone();
        let probe = std::thread::spawn(move || {
            let _entered = runtime.enter();
            tx.send(bind_error_kind(&probe_root, &probe_endpoint))
                .unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        // Release the raw fixture even if a regression blocked its connect.
        drop(listener);
        drop(clients);
        probe.join().unwrap();
        std::fs::remove_file(path).unwrap();
        // Maintenance and bind-time stale handling both preserve v1 lock files.
        assert_eq!(result.unwrap(), Some(io::ErrorKind::AddrInUse));
    }

    #[tokio::test]
    async fn unknown_stale_socket_requires_explicit_cleanup() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("unknown");
        let identity = raw_stale_socket(root, &endpoint);
        // A socket with no v1 identity record is never reaped by bind: the
        // address stays refused until an explicit, verifiable cleanup.
        assert_eq!(
            bind_error_kind(root, &endpoint),
            Some(io::ErrorKind::AddrInUse)
        );
        assert_eq!(
            identity_of(&socket_path(root, &endpoint)),
            identity,
            "a refused bind must not touch the unregistered socket"
        );
    }

    #[tokio::test]
    async fn active_reap_is_busy_and_normal_disconnect_keeps_listener_available() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("busy-reap");
        let path = socket_path(root, &endpoint);
        let mut listener = test_bind_in(&endpoint, root).unwrap();
        let credential = listener.credential();
        assert!(matches!(
            reap_in_temp(root, &endpoint, &credential),
            EndpointReapResult::Busy
        ));

        for _ in 0..2 {
            let (client, server) =
                tokio::try_join!(UnixStream::connect(&path), listener.accept()).unwrap();
            drop(client);
            drop(server);
        }
        assert!(path.exists());
        assert!(std::fs::symlink_metadata(lock_path(root, &endpoint)).is_ok());
    }

    #[test]
    fn inspect_and_reap_leave_unknown_and_corrupt_records_untouched() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let unknown = test_endpoint("unknown-record");
        let unknown_identity = raw_stale_socket(root, &unknown);
        let unknown_credential = EndpointCredential::unix(unknown.clone(), unknown_identity);
        assert!(matches!(
            inspect_in(root, &unknown),
            EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership)
        ));
        assert!(matches!(
            reap_in_temp(root, &unknown, &unknown_credential),
            EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership)
        ));
        assert!(
            !lock_path(root, &unknown).exists(),
            "reap must not create a lock"
        );

        let corrupt = test_endpoint("corrupt-record");
        let corrupt_identity = raw_stale_socket(root, &corrupt);
        write_corrupt_record(root, &corrupt);
        let corrupt_credential = EndpointCredential::unix(corrupt.clone(), corrupt_identity);
        assert!(matches!(
            inspect_in(root, &corrupt),
            EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord)
        ));
        assert!(matches!(
            reap_in_temp(root, &corrupt, &corrupt_credential),
            EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord)
        ));
        assert!(socket_path(root, &unknown).exists());
        assert!(socket_path(root, &corrupt).exists());
        std::fs::remove_file(socket_path(root, &unknown)).unwrap();
        std::fs::remove_file(socket_path(root, &corrupt)).unwrap();
    }

    #[test]
    fn symlink_endpoint_is_refused_without_following_its_target() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("symlink");
        let target = root.join("target-marker");
        std::fs::write(&target, b"keep").unwrap();
        let socket = socket_path(root, &endpoint);
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        let credential = EndpointCredential::unix(
            endpoint.clone(),
            SocketIdentity {
                device: 0,
                inode: 0,
                changed_secs: 0,
                changed_nanos: 0,
            },
        );
        assert!(matches!(
            inspect_in(root, &endpoint),
            EndpointInspection::Unverified(EndpointUnverifiedReason::Symlink)
        ));
        assert!(matches!(
            reap_in_temp(root, &endpoint, &credential),
            EndpointReapResult::Unverified(EndpointUnverifiedReason::Symlink)
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"keep");
        std::fs::remove_file(socket).unwrap();
    }

    #[tokio::test]
    async fn old_credential_cannot_remove_a_new_listener_instance() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("stale-credential");
        let first = test_bind_in(&endpoint, root).unwrap();
        let old = first.credential();
        drop(first);

        // Occupy the unlinked inode so the OS cannot recycle the same socket
        // identity for the immediately following bind.
        let guard_endpoint = test_endpoint("inode-guard");
        let guard_path = socket_path(root, &guard_endpoint);
        let guard = std::os::unix::net::UnixListener::bind(&guard_path).unwrap();
        let second = test_bind_in(&endpoint, root).unwrap();
        let current = second.credential();
        assert_ne!(old, current);
        assert!(matches!(
            reap_in_temp(root, &endpoint, &old),
            EndpointReapResult::StaleTarget
        ));
        assert!(socket_path(root, &endpoint).exists());
        drop(second);
        drop(guard);
        std::fs::remove_file(guard_path).unwrap();
    }

    #[tokio::test]
    async fn crashed_owner_can_be_reaped_twice_without_removing_v1_lock() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("crash-reap");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::tests::crash_owner_child",
                "--nocapture",
            ])
            .env("C2_LOCAL_TEST_CRASH_OWNER", endpoint.address())
            .env("C2_LOCAL_TEST_CRASH_ROOT", root)
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        assert!(socket_path(root, &endpoint).exists());
        let EndpointInspection::Present(credential) = inspect_in(root, &endpoint) else {
            panic!("crashed owner must leave its complete v1 identity record");
        };
        assert!(matches!(
            reap_in_temp(root, &endpoint, &credential),
            EndpointReapResult::Reaped
        ));
        assert!(matches!(
            reap_in_temp(root, &endpoint, &credential),
            EndpointReapResult::AlreadyAbsent
        ));
        assert!(!socket_path(root, &endpoint).exists());
        assert!(lock_path(root, &endpoint).exists());
    }

    #[tokio::test]
    async fn explicit_close_reports_socket_removal_and_retains_lock_file() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("explicit-close");
        let listener = test_bind_in(&endpoint, root).unwrap();
        assert!(matches!(listener.close(), EndpointReapResult::Reaped));
        assert!(!socket_path(root, &endpoint).exists());
        assert!(lock_path(root, &endpoint).exists());
    }

    #[tokio::test]
    async fn failed_initialization_withdraws_its_socket_and_allows_rebinding() {
        for failure in [
            fault::Failure::SocketPermissions,
            fault::Failure::RecordWrite,
        ] {
            let namespace = TestNamespace::new();
            let root = namespace.path();
            let endpoint = test_endpoint("rollback");
            fault::inject(failure);
            let error = test_bind_in(&endpoint, root)
                .err()
                .unwrap_or_else(|| panic!("{failure:?} must fail bind"));
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Other
                ),
                "{failure:?}: {error}"
            );
            assert!(
                !socket_path(root, &endpoint).exists(),
                "{failure:?} must withdraw the socket it bound"
            );
            assert!(
                lock_path(root, &endpoint).exists(),
                "the v1 ownership file is still retained"
            );
            // The fixed address must be reusable without manual cleanup.
            let listener = test_bind_in(&endpoint, root)
                .unwrap_or_else(|error| panic!("{failure:?} left the address pinned: {error}"));
            let path = socket_path(root, &endpoint);
            assert!(path.exists());
            drop(listener);
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn rollback_never_removes_a_replacement_object() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let endpoint = test_endpoint("replacement");
        fault::inject(fault::Failure::RecordWriteAfterReplacement);
        assert!(test_bind_in(&endpoint, root).is_err());
        let path = socket_path(root, &endpoint);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement-marker");
        // The replacement is foreign, so bind must keep refusing it and must
        // not delete it while refusing.
        assert_eq!(
            bind_error_kind(root, &endpoint),
            Some(io::ErrorKind::AddrInUse)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement-marker");
    }

    #[tokio::test]
    async fn budgeted_sweep_advances_failures_and_finishes_a_stable_round() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let busy_endpoint = test_endpoint("sweep-busy");
        let _busy_listener = test_bind_in(&busy_endpoint, root).unwrap();

        let unknown = test_endpoint("sweep-unknown");
        raw_stale_socket(root, &unknown);
        let crashed = test_endpoint("sweep-crashed");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::tests::crash_owner_child",
                "--nocapture",
            ])
            .env("C2_LOCAL_TEST_CRASH_OWNER", crashed.address())
            .env("C2_LOCAL_TEST_CRASH_ROOT", root)
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");

        let mut sweep = EndpointSweep::open_at(root).unwrap();
        let mut total = SweepBatch::default();
        let mut complete = false;
        for _ in 0..64 {
            let batch = sweep.next_batch(SweepBudget {
                max_entries: 1,
                max_duration: Duration::ZERO,
            });
            assert!(
                batch.entries_visited <= 1,
                "entry budget was exceeded: {batch:?}"
            );
            total.entries_visited += batch.entries_visited;
            total.endpoints_examined += batch.endpoints_examined;
            total.reaped += batch.reaped;
            total.busy += batch.busy;
            total.unverified += batch.unverified;
            assert!(
                !batch.round_interrupted,
                "a stable namespace must not interrupt the round: {batch:?}"
            );
            complete = batch.round_complete;
            if complete {
                break;
            }
        }
        assert!(
            complete,
            "stable test namespace must finish within its entry bound"
        );
        assert!(total.entries_visited > total.endpoints_examined);
        assert!(
            total.busy >= 1,
            "live listener must count as Busy across batches"
        );
        assert!(
            total.unverified >= 1,
            "unknown record must count and not pin the iterator"
        );
        assert_eq!(
            total.reaped, 1,
            "later valid candidate must be reached after failures"
        );
        assert!(!socket_path(root, &crashed).exists());
        assert!(socket_path(root, &busy_endpoint).exists());
        assert!(socket_path(root, &unknown).exists());
    }

    #[test]
    fn default_sweep_namespace_comes_from_the_local_endpoint_mapping() {
        // The default sweep target is derived, not a second hardcoded path:
        // the probe region must be a valid address and its parent directory
        // must be the same namespace any other logical address maps to.
        let probe = LocalEndpoint::from_address(&format!("ipc://{SWEEP_NAMESPACE_PROBE}")).unwrap();
        let other = LocalEndpoint::from_address("ipc://some-other-region").unwrap();
        assert_eq!(
            Path::new(probe.os_name()).parent(),
            Path::new(other.os_name()).parent()
        );
    }

    #[tokio::test]
    async fn interrupted_sweep_never_reports_a_completed_round() {
        let parent = tempfile::Builder::new()
            .prefix("c2l1-ns-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = parent.path().join("ns");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = test_endpoint("interrupt");
        let _listener = test_bind_in(&endpoint, &root).unwrap();

        let mut sweep = EndpointSweep::open_at(&root).unwrap();
        let first = sweep.next_batch(SweepBudget {
            max_entries: 1,
            max_duration: Duration::ZERO,
        });
        assert!(!first.round_complete && !first.round_interrupted);

        // Replace the verified namespace directory with a different inode.
        let moved = parent.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

        let interrupted = sweep.next_batch(SweepBudget::default());
        assert!(interrupted.namespace_changed, "{interrupted:?}");
        assert!(interrupted.round_interrupted, "{interrupted:?}");
        assert!(!interrupted.round_complete, "{interrupted:?}");
        let terminal = sweep.next_batch(SweepBudget::default());
        assert!(terminal.namespace_changed, "{terminal:?}");
        assert!(
            terminal.round_interrupted,
            "a replaced namespace must stay interrupted: {terminal:?}"
        );
        assert!(
            !terminal.round_complete,
            "an interrupted round must never be reported as complete: {terminal:?}"
        );
        assert_eq!(terminal.entries_visited, 0);
    }
    #[tokio::test]
    async fn sweep_scope_legacy_does_not_inspect_unselected_orphans() {
        let namespace = TestNamespace::new();
        let root = namespace.path();
        let selected = test_endpoint("scope-selected");
        let unselected = test_endpoint("scope-unselected");
        let active = test_endpoint("scope-active");
        let _listener = test_bind_in(&active, root).unwrap();
        for endpoint in [&selected, &unselected] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "platform::tests::crash_owner_child", "--nocapture"])
                .env("C2_LOCAL_TEST_CRASH_OWNER", endpoint.address())
                .env("C2_LOCAL_TEST_CRASH_ROOT", root).output().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        let untouched = socket_path(root, &unselected);
        let identity = identity_of(&untouched);
        let lock = lock_path(root, &unselected);
        let lock_identity = identity_of(&lock);
        let bytes = std::fs::read(&lock).unwrap();
        let mut sweep = EndpointSweep::open_scoped_at(root, &[selected.clone(), active]).unwrap();
        let mut complete = false;
        let mut busy = 0;
        for _ in 0..32 {
            let batch = sweep.next_batch(SweepBudget { max_entries: 1, max_duration: Duration::ZERO });
            assert!(batch.entries_visited <= 1);
            busy += batch.busy;
            if batch.round_complete { complete = true; break; }
        }
        assert!(complete);
        assert!(busy >= 1);
        assert!(!socket_path(root, &selected).exists());
        assert!(lock_path(root, &selected).exists(), "legacy ownership records persist");
        assert_eq!(identity_of(&untouched), identity);
        assert_eq!(identity_of(&lock), lock_identity);
        assert_eq!(std::fs::read(&lock).unwrap(), bytes);
    }

    #[test]
    fn sweep_scope_rejects_invalid_and_unbounded_targets_before_open() {
        let endpoint = test_endpoint("scope-validation");
        assert!(crate::EndpointSweepScope::from_addresses(&endpoint, &["http://wrong".into()]).is_err());
        assert!(crate::EndpointSweepScope::from_addresses(&endpoint, &["/tmp/forged.sock".into()]).is_err());
        let too_many = vec![endpoint.address().to_owned(); crate::EndpointSweepScope::MAX_ADDRESSES + 1];
        assert!(crate::EndpointSweepScope::from_addresses(&endpoint, &too_many).is_err());
        assert!(crate::EndpointSweepScope::from_addresses(&endpoint, &[]).is_ok());
    }

}
