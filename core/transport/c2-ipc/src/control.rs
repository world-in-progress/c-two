use std::future::Future;
use std::io;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

use crate::client::IpcError;
use c2_local::{LocalEndpoint, LocalStream};
use c2_wire::flags::{FLAG_RESPONSE, FLAG_SIGNAL};
use c2_wire::frame::{self, HEADER_SIZE};
use c2_wire::msg_type::{PING_BYTES, PONG_BYTES};
use c2_wire::shutdown_control::{DirectShutdownAck, decode_shutdown_ack, encode_shutdown_initiate};

pub fn local_endpoint_from_ipc_address(address: &str) -> Result<LocalEndpoint, IpcError> {
    LocalEndpoint::from_address(address).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidInput {
            IpcError::Config(error.to_string())
        } else {
            IpcError::Io(error)
        }
    })
}

enum Exchange {
    Absent,
    NoReply,
    Reply(u32, Vec<u8>),
}

async fn read_reply(stream: &mut LocalStream) -> io::Result<(u32, Vec<u8>)> {
    let mut header = [0_u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let (total_len, body) = frame::decode_total_len(&header)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
    let (header, prefix) = frame::decode_frame_body(body, total_len)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
    // Administrative acknowledgements contain a control document, never a CRM
    // payload. Reject hostile lengths before reserving a buffer.
    if header.payload_len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local control reply exceeds 1 MiB",
        ));
    }
    let mut payload = prefix.to_vec();
    let prefix_len = payload.len();
    payload.resize(header.payload_len(), 0);
    stream.read_exact(&mut payload[prefix_len..]).await?;
    Ok((header.flags, payload))
}

async fn exchange(
    endpoint: &LocalEndpoint,
    timeout: Duration,
    signal: &[u8],
) -> Result<Exchange, IpcError> {
    let operation = async {
        let mut stream = match LocalStream::connect(endpoint, timeout).await {
            Ok(stream) => stream,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Exchange::Absent);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(IpcError::Io(error));
            }
            // On macOS, ConnectionRefused can mean a live listener's backlog
            // is full. It cannot prove that a server has stopped.
            Err(_) => return Ok(Exchange::NoReply),
        };
        let frame = frame::encode_frame(0, FLAG_SIGNAL, signal);
        if stream
            .write_all_with_timeout(&frame, timeout)
            .await
            .is_err()
        {
            return Ok(Exchange::NoReply);
        }
        Ok(match read_reply(&mut stream).await {
            Ok((flags, payload)) => Exchange::Reply(flags, payload),
            Err(_) => Exchange::NoReply,
        })
    };
    tokio::time::timeout(timeout, operation)
        .await
        .unwrap_or(Ok(Exchange::NoReply))
}

// Synchronous probes can run on a thread that already has a Tokio runtime.
// Their bounded I/O uses a separate runtime, never nested block_on or a second
// implementation of the operating-system transport.
fn blocking<T: Send + 'static>(
    future: impl Future<Output = Result<T, IpcError>> + Send + 'static,
) -> Result<T, IpcError> {
    std::thread::Builder::new()
        .name("c2-local-control".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(IpcError::Io)?
                .block_on(future)
        })
        .map_err(IpcError::Io)?
        .join()
        .map_err(|_| IpcError::Io(io::Error::other("local control worker panicked")))?
}

pub fn ping(address: &str, timeout: Duration) -> Result<bool, IpcError> {
    let endpoint = local_endpoint_from_ipc_address(address)?;
    blocking(async move {
        let started = Instant::now();
        while let Some(remaining) = timeout.checked_sub(started.elapsed()) {
            if remaining.is_zero() {
                break;
            }
            match exchange(
                &endpoint,
                remaining.min(Duration::from_millis(100)),
                &PING_BYTES,
            )
            .await?
            {
                Exchange::Reply(flags, payload) => {
                    return Ok(flags & FLAG_SIGNAL != 0
                        && flags & FLAG_RESPONSE != 0
                        && payload == PONG_BYTES);
                }
                Exchange::Absent | Exchange::NoReply => {
                    tokio::time::sleep(remaining.min(Duration::from_millis(10))).await
                }
            }
        }
        Ok(false)
    })
}

fn unacknowledged() -> DirectShutdownAck {
    DirectShutdownAck {
        acknowledged: false,
        shutdown_started: false,
        server_stopped: false,
        route_outcomes: Vec::new(),
    }
}

pub fn shutdown(address: &str, timeout: Duration) -> Result<DirectShutdownAck, IpcError> {
    let endpoint = local_endpoint_from_ipc_address(address)?;
    blocking(async move {
        let started = Instant::now();
        let request = encode_shutdown_initiate();
        while let Some(remaining) = timeout.checked_sub(started.elapsed()) {
            if remaining.is_zero() {
                break;
            }
            match exchange(
                &endpoint,
                remaining.min(Duration::from_millis(500)),
                &request,
            )
            .await?
            {
                Exchange::Absent => {
                    return Ok(DirectShutdownAck {
                        acknowledged: true,
                        shutdown_started: false,
                        server_stopped: true,
                        route_outcomes: Vec::new(),
                    });
                }
                Exchange::Reply(flags, payload) => {
                    if flags & FLAG_SIGNAL == 0 || flags & FLAG_RESPONSE == 0 {
                        return Ok(unacknowledged());
                    }
                    return Ok(decode_shutdown_ack(&payload).unwrap_or_else(|_| unacknowledged()));
                }
                Exchange::NoReply => {
                    tokio::time::sleep(remaining.min(Duration::from_millis(10))).await
                }
            }
        }
        Ok(unacknowledged())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_local::LocalListener;
    use c2_wire::shutdown_control::encode_shutdown_ack;
    use std::sync::mpsc;

    fn address(label: &str) -> String {
        format!("ipc://control-{label}-{}", std::process::id())
    }

    fn responder(
        address: String,
        delay: Duration,
        discard_first: bool,
        expected: Vec<u8>,
        reply: Vec<u8>,
    ) -> (mpsc::Receiver<()>, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    tokio::time::sleep(delay).await;
                    let endpoint = LocalEndpoint::from_address(&address).unwrap();
                    let mut listener = LocalListener::bind(&endpoint).unwrap();
                    tx.send(()).unwrap();
                    if discard_first {
                        let mut first = listener.accept().await.unwrap();
                        let _ = read_reply(&mut first).await.unwrap();
                    }
                    let mut stream = listener.accept().await.unwrap();
                    let (_, payload) = read_reply(&mut stream).await.unwrap();
                    assert_eq!(payload, expected);
                    stream
                        .write_all(&frame::encode_frame(0, FLAG_SIGNAL | FLAG_RESPONSE, &reply))
                        .await
                        .unwrap();
                });
        });
        (rx, thread)
    }

    #[test]
    fn invalid_addresses_are_configuration_errors() {
        for address in [
            "tcp://host",
            "ipc://",
            "ipc://..",
            "ipc://bad/name",
            "ipc://bad\\name",
        ] {
            assert!(matches!(
                ping(address, Duration::from_millis(10)),
                Err(IpcError::Config(_))
            ));
            assert!(matches!(
                shutdown(address, Duration::from_millis(10)),
                Err(IpcError::Config(_))
            ));
        }
    }

    #[test]
    fn absent_endpoint_has_no_ping_and_is_already_stopped() {
        let address = address("absent");
        assert!(!ping(&address, Duration::from_millis(20)).unwrap());
        let result = shutdown(&address, Duration::from_millis(20)).unwrap();
        assert!(result.acknowledged && result.server_stopped && !result.shutdown_started);
        assert!(result.route_outcomes.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn refused_socket_does_not_prove_shutdown_completed() {
        let address = address("refused");
        let endpoint = local_endpoint_from_ipc_address(&address).unwrap();
        let path = std::path::Path::new(endpoint.os_name());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // An unowned socket produces ConnectionRefused. The same error can
        // come from a live macOS listener whose queue is full.
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
        let result = shutdown(&address, Duration::from_millis(20));
        std::fs::remove_file(path).unwrap();
        let result = result.unwrap();
        assert!(!result.acknowledged);
        assert!(!result.server_stopped);
    }

    #[test]
    fn ping_retries_until_endpoint_appears() {
        let address = address("late");
        let (_ready, thread) = responder(
            address.clone(),
            Duration::from_millis(50),
            false,
            PING_BYTES.to_vec(),
            PONG_BYTES.to_vec(),
        );
        assert!(ping(&address, Duration::from_secs(1)).unwrap());
        thread.join().unwrap();
    }

    #[test]
    fn shutdown_ack_only_proves_initiation_and_retries_dropped_exchange() {
        for discard_first in [false, true] {
            let address = address(if discard_first { "retry" } else { "initiate" });
            let expected = DirectShutdownAck {
                acknowledged: true,
                shutdown_started: true,
                server_stopped: false,
                route_outcomes: Vec::new(),
            };
            let (ready, thread) = responder(
                address.clone(),
                Duration::ZERO,
                discard_first,
                encode_shutdown_initiate().to_vec(),
                encode_shutdown_ack(&expected).unwrap(),
            );
            ready.recv_timeout(Duration::from_secs(1)).unwrap();
            let result = shutdown(&address, Duration::from_secs(1)).unwrap();
            assert!(result.acknowledged && result.shutdown_started && !result.server_stopped);
            assert!(result.route_outcomes.is_empty());
            thread.join().unwrap();
        }
    }
}
