use super::*;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn owned_halves_exchange_bytes_and_abort_wakes_a_pending_read() {
    let (client, server) = LocalStream::pair().await.unwrap();
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
    let (mut client, _server) = LocalStream::pair().await.unwrap();
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
    let address = format!("ipc://c2-listener-test-{}", std::process::id());
    let endpoint = LocalEndpoint::from_address(&address).unwrap();
    let mut first = LocalListener::bind(&endpoint).unwrap();
    assert!(LocalListener::bind(&endpoint).is_err());
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        first.accept()
    )
    .unwrap();
    drop(first);
    let _restarted = LocalListener::bind(&endpoint).unwrap();
}

#[tokio::test]
async fn cancelled_accept_can_accept_the_next_client() {
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://c2-accept-test-{}", std::process::id()))
            .unwrap();
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
}

#[tokio::test]
async fn completed_reply_remains_readable_after_server_stream_is_dropped() {
    let (mut client, mut server) = LocalStream::pair().await.unwrap();
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
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://c2-restart-test-{}", std::process::id()))
            .unwrap();
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
}

#[test]
fn listener_process_fixture() {
    let Ok(action) = std::env::var("C2_LOCAL_LISTENER_TEST_ACTION") else {
        return;
    };
    let address = std::env::var("C2_LOCAL_LISTENER_TEST_ADDRESS").unwrap();
    let endpoint = LocalEndpoint::from_address(&address).unwrap();
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
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::listener_process_fixture", "--nocapture"])
        .env("C2_LOCAL_LISTENER_TEST_ADDRESS", endpoint.address())
        .env("C2_LOCAL_LISTENER_TEST_ACTION", action)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test]
async fn another_process_cannot_displace_the_listener() {
    let endpoint = LocalEndpoint::from_address(&format!(
        "ipc://c2-process-duplicate-{}",
        std::process::id()
    ))
    .unwrap();
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
}

#[tokio::test]
async fn listener_restarts_after_owner_process_exits_without_destructors() {
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://c2-process-exit-{}", std::process::id()))
            .unwrap();
    run_listener_process(&endpoint, "exit-owner");
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        owner.accept(),
    )
    .unwrap();
}

#[tokio::test]
async fn listener_ownership_can_be_released_on_another_thread() {
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://c2-thread-drop-{}", std::process::id()))
            .unwrap();
    let owner = LocalListener::bind(&endpoint).unwrap();
    std::thread::spawn(move || drop(owner)).join().unwrap();
    let _restarted = LocalListener::bind(&endpoint).unwrap();
}

#[tokio::test]
async fn listener_restarts_after_cancelling_a_pending_accept() {
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://c2-pending-restart-{}", std::process::id()))
            .unwrap();
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
}
