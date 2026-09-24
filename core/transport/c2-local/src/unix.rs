use super::LocalEndpoint;
use socket2::{Domain, SockAddr, Socket, Type};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
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
    inner: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    // This rendezvous inode remains on disk. Unlinking it would let a contender
    // open a new inode while another process still holds the previous lock.
    // Keep the lock until both socket cleanup and native listener close finish.
    _ownership: File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    changed_secs: i64,
    changed_nanos: i64,
}

impl SocketIdentity {
    const MAGIC: &[u8; 8] = b"c2sock01";
    const RECORD_SIZE: usize = 40;

    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_secs: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }
    }

    fn read(file: &mut File) -> io::Result<Option<Self>> {
        if file.metadata()?.len() != Self::RECORD_SIZE as u64 {
            return Ok(None);
        }
        let mut record = [0_u8; Self::RECORD_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut record)?;
        if &record[..8] != Self::MAGIC {
            return Ok(None);
        }
        Ok(Some(Self {
            device: u64::from_le_bytes(record[8..16].try_into().unwrap()),
            inode: u64::from_le_bytes(record[16..24].try_into().unwrap()),
            changed_secs: i64::from_le_bytes(record[24..32].try_into().unwrap()),
            changed_nanos: i64::from_le_bytes(record[32..40].try_into().unwrap()),
        }))
    }

    fn write(self, file: &mut File) -> io::Result<()> {
        let mut record = [0_u8; Self::RECORD_SIZE];
        record[..8].copy_from_slice(Self::MAGIC);
        record[8..16].copy_from_slice(&self.device.to_le_bytes());
        record[16..24].copy_from_slice(&self.inode.to_le_bytes());
        record[24..32].copy_from_slice(&self.changed_secs.to_le_bytes());
        record[32..40].copy_from_slice(&self.changed_nanos.to_le_bytes());
        // Every reader and writer holds the same exclusive flock. A partial
        // or corrupt record cannot authorize cleanup of a different identity.
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&record)?;
        file.set_len(Self::RECORD_SIZE as u64)
    }
}

fn ownership_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.with_extension("lock"))?;
    // Nonblocking acquisition keeps duplicate startup off the runtime's wait
    // path. The OS releases this lock when the owner exits, including crashes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            endpoint_in_use()
        } else {
            error
        });
    }
    Ok(file)
}

fn endpoint_in_use() -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        "IPC endpoint already has an active listener",
    )
}

fn remove_stale_socket(path: &Path, ownership: &mut File) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(endpoint_in_use());
    }
    let probe = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    probe.set_nonblocking(true)?;
    match probe.connect(&SockAddr::unix(path)?) {
        Ok(()) => return Err(endpoint_in_use()),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || matches!(
                    error.raw_os_error(),
                    Some(libc::EINPROGRESS | libc::EALREADY)
                ) =>
        {
            return Err(endpoint_in_use());
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Err(error) => return Err(error),
    }
    let identity = SocketIdentity::from_metadata(&metadata);
    // macOS also reports ECONNREFUSED for a live listener with a full queue.
    // A released flock plus our exact socket record proves a previous C-Two
    // owner is gone. Unknown sockets need explicit external cleanup instead.
    if SocketIdentity::read(ownership)? != Some(identity) {
        return Err(endpoint_in_use());
    }
    match std::fs::symlink_metadata(path) {
        Ok(current) if SocketIdentity::from_metadata(&current) == identity => {
            std::fs::remove_file(path)
        }
        Ok(_) => Err(endpoint_in_use()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl Listener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let path = PathBuf::from(endpoint.os_name());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut ownership = ownership_lock(&path)?;
        remove_stale_socket(&path, &mut ownership)?;
        let inner = UnixListener::bind(&path)?;
        let metadata = std::fs::metadata(&path)?;
        let mut listener = Self {
            inner,
            path,
            identity: SocketIdentity::from_metadata(&metadata),
            _ownership: ownership,
        };
        std::fs::set_permissions(&listener.path, std::fs::Permissions::from_mode(0o600))?;
        listener.identity = SocketIdentity::from_metadata(&std::fs::metadata(&listener.path)?);
        listener.identity.write(&mut listener._ownership)?;
        Ok(listener)
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        self.inner.accept().await.map(|(stream, _)| stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && SocketIdentity::from_metadata(&metadata) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_CONNECT_TIMEOUT, LocalStream};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[test]
    fn crash_owner_child() {
        let Ok(address) = std::env::var("C2_LOCAL_TEST_CRASH_OWNER") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let _listener = Listener::bind(&endpoint).unwrap();
        // Process exit releases kernel handles and flock without Rust Drop.
        std::process::exit(0);
    }

    #[tokio::test]
    async fn concurrent_stale_startup_keeps_the_winner_reachable() {
        for round in 0..8 {
            let endpoint = LocalEndpoint::from_address(&format!(
                "ipc://c2-stale-{}-{round}",
                std::process::id()
            ))
            .unwrap();
            let path = Path::new(endpoint.os_name());
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::tests::crash_owner_child",
                    "--nocapture",
                ])
                .env("C2_LOCAL_TEST_CRASH_OWNER", endpoint.address())
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
                            Listener::bind(&endpoint)
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
            let (mut client, server) = tokio::try_join!(
                LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
                winner.accept()
            )
            .unwrap();
            let mut server = LocalStream::from_inner(server);
            server.write_all(b"owned").await.unwrap();
            let mut reply = [0; 5];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"owned");
            drop(winner);
            assert!(Listener::bind(&endpoint).is_ok());
        }
    }

    #[tokio::test]
    async fn probing_a_full_backlog_never_waits_for_accept() {
        let endpoint =
            LocalEndpoint::from_address(&format!("ipc://c2-backlog-{}", std::process::id()))
                .unwrap();
        let path = PathBuf::from(endpoint.os_name());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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
        let probe = std::thread::spawn(move || {
            let _entered = runtime.enter();
            tx.send(Listener::bind(&endpoint).err().map(|error| error.kind()))
                .unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        // Release the raw fixture even if a regression blocked its connect.
        drop(listener);
        drop(clients);
        probe.join().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(result.unwrap(), Some(io::ErrorKind::AddrInUse));
    }

    #[tokio::test]
    async fn unknown_stale_socket_requires_explicit_cleanup() {
        let endpoint =
            LocalEndpoint::from_address(&format!("ipc://c2-unknown-{}", std::process::id()))
                .unwrap();
        let path = Path::new(endpoint.os_name());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
        let identity = SocketIdentity::from_metadata(&std::fs::metadata(path).unwrap());
        assert_eq!(
            Listener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        assert!(SocketIdentity::from_metadata(&std::fs::metadata(path).unwrap()) == identity);
        std::fs::remove_file(path).unwrap();
    }
}
