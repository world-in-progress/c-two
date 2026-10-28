use super::*;
use tokio::io::AsyncReadExt;

struct TestEndpoint {
    _root: Option<tempfile::TempDir>,
    endpoint: LocalEndpoint,
}

fn isolated_endpoint(label: &str) -> TestEndpoint {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let address = format!(
        "ipc://{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    #[cfg(unix)]
    {
        let root = tempfile::Builder::new()
            .prefix("c2-local-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let endpoint = LocalEndpointContext::with_unix_root(root.path())
            .unwrap()
            .endpoint(&address)
            .unwrap();
        TestEndpoint {
            _root: Some(root),
            endpoint,
        }
    }
    #[cfg(windows)]
    {
        TestEndpoint {
            _root: None,
            endpoint: LocalEndpoint::from_address(&address).unwrap(),
        }
    }
}

async fn isolated_pair() -> (TestEndpoint, LocalStream, LocalStream) {
    let fixture = isolated_endpoint("c2-pair-test");
    let mut listener = LocalListener::bind(&fixture.endpoint).unwrap();
    let (client, server) = tokio::try_join!(
        LocalStream::connect(&fixture.endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept()
    )
    .unwrap();
    close_listener(listener);
    (fixture, client, server)
}

pub(super) fn close_listener(listener: LocalListener) {
    let outcome = listener.close();
    assert!(
        matches!(
            outcome,
            EndpointReapResult::Reaped
                | EndpointReapResult::AlreadyAbsent
                | EndpointReapResult::NotApplicable
        ),
        "listener cleanup failed: {outcome:?}"
    );
}

#[tokio::test]
async fn owned_halves_exchange_bytes_and_abort_wakes_a_pending_read() {
    let (_fixture, client, server) = isolated_pair().await;
    let (mut reader, _) = server.into_split();
    let (_, mut writer) = client.into_split();
    writer.write_all(b"hello").await.unwrap();
    let mut bytes = [0; 5];
    reader.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"hello");
    writer.abort_handle().abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), reader.read_exact(&mut bytes))
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn cancelling_a_partial_write_poisoned_the_connection() {
    let (_fixture, mut client, _server) = isolated_pair().await;
    let abort = client.abort_handle();
    let bytes = vec![0; 16 * 1024 * 1024];
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.write_all(&bytes))
            .await
            .is_err()
    );
    assert!(abort.is_aborted());
    assert!(client.write_all(b"new frame").await.is_err());
}

#[tokio::test]
async fn duplicate_listener_cannot_displace_owner_and_restart_works() {
    let fixture = isolated_endpoint("c2-listener-test");
    let endpoint = &fixture.endpoint;
    let mut first = LocalListener::bind(&endpoint).unwrap();
    assert!(LocalListener::bind(&endpoint).is_err());
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        first.accept()
    )
    .unwrap();
    drop(first);
    close_listener(LocalListener::bind(&endpoint).unwrap());
}

#[tokio::test]
async fn cancelled_accept_can_accept_the_next_client() {
    let fixture = isolated_endpoint("c2-accept-test");
    let endpoint = &fixture.endpoint;
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept()
    )
    .unwrap();
    close_listener(listener);
}

#[tokio::test]
async fn completed_reply_remains_readable_after_server_stream_is_dropped() {
    let (_fixture, mut client, mut server) = isolated_pair().await;
    let expected = b"shutdown initiate acknowledged";
    server.write_all(expected).await.unwrap();
    drop(server);
    let mut actual = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut actual))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn listener_restarts_after_server_closes_even_with_an_old_client_handle() {
    let fixture = isolated_endpoint("c2-restart-test");
    let endpoint = &fixture.endpoint;
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    let (client, server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept(),
    )
    .unwrap();
    drop(server);
    drop(listener);
    let mut next = LocalListener::bind(&endpoint).unwrap();
    assert!(LocalListener::bind(&endpoint).is_err());
    let (mut next_client, mut next_server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        next.accept(),
    )
    .unwrap();
    next_server.write_all(b"new listener").await.unwrap();
    let mut bytes = [0; 12];
    next_client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"new listener");
    drop(client);
    close_listener(next);
}

#[test]
fn listener_process_fixture() {
    let Ok(action) = std::env::var("C2_LOCAL_LISTENER_TEST_ACTION") else {
        return;
    };
    let address = std::env::var("C2_LOCAL_LISTENER_TEST_ADDRESS").unwrap();
    #[cfg(unix)]
    let context = LocalEndpointContext::with_unix_root(std::path::Path::new(
        &std::env::var_os("C2_LOCAL_LISTENER_TEST_ROOT").unwrap(),
    ))
    .unwrap();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();
    let endpoint = context.endpoint(&address).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    match action.as_str() {
        "duplicate" => assert_eq!(
            LocalListener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse,
        ),
        "exit-owner" => {
            let _listener = LocalListener::bind(&endpoint).unwrap();
            // Release kernel handles without Rust destructors, as on process death.
            std::process::exit(0);
        }
        _ => panic!("unknown listener test action: {action}"),
    }
}

fn run_listener_process(endpoint: &LocalEndpoint, action: &str) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "tests::listener_process_fixture", "--nocapture"])
        .env("C2_LOCAL_LISTENER_TEST_ADDRESS", endpoint.address())
        .env("C2_LOCAL_LISTENER_TEST_ACTION", action);
    if let Some(root) = endpoint.context().unix_root() {
        command.env("C2_LOCAL_LISTENER_TEST_ROOT", root);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test]
async fn another_process_cannot_displace_the_listener() {
    let fixture = isolated_endpoint("c2-process-duplicate");
    let endpoint = &fixture.endpoint;
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    run_listener_process(&endpoint, "duplicate");
    let (mut client, mut server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        owner.accept(),
    )
    .unwrap();
    server.write_all(b"owner").await.unwrap();
    let mut bytes = [0; 5];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"owner");
    close_listener(owner);
}

#[tokio::test]
async fn listener_restarts_after_owner_process_exits_without_destructors() {
    let fixture = isolated_endpoint("c2-process-exit");
    let endpoint = &fixture.endpoint;
    run_listener_process(&endpoint, "exit-owner");
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        owner.accept(),
    )
    .unwrap();
    close_listener(owner);
}

#[tokio::test]
async fn listener_ownership_can_be_released_on_another_thread() {
    let fixture = isolated_endpoint("c2-thread-drop");
    let endpoint = &fixture.endpoint;
    let owner = LocalListener::bind(&endpoint).unwrap();
    std::thread::spawn(move || drop(owner)).join().unwrap();
    close_listener(LocalListener::bind(&endpoint).unwrap());
}

#[tokio::test]
async fn listener_restarts_after_cancelling_a_pending_accept() {
    let fixture = isolated_endpoint("c2-pending-restart");
    let endpoint = &fixture.endpoint;
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), owner.accept())
            .await
            .is_err()
    );
    drop(owner);
    let mut next = LocalListener::bind(&endpoint).unwrap();
    let (mut client, mut server) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
            next.accept(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    server.write_all(b"new listener").await.unwrap();
    let mut bytes = [0; 12];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"new listener");
    close_listener(next);
}

#[tokio::test]
async fn connected_pair_allows_same_session_shm_and_abort_is_an_error() {
    let (_fixture, client, server) = isolated_pair().await;
    assert!(client.shared_memory_available().unwrap());
    assert!(server.shared_memory_available().unwrap());
    client.abort_handle().abort();
    assert_eq!(
        client.shared_memory_available().unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}
