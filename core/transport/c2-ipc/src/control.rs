use std::future::Future;
use std::io;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

use crate::client::IpcError;
use c2_config::LocalEndpointContext;
use c2_local::{LocalEndpoint, LocalStream};
use c2_wire::flags::{FLAG_RESPONSE, FLAG_SIGNAL};
use c2_wire::frame::{self, HEADER_SIZE};
use c2_wire::msg_type::{PING_BYTES, PONG_BYTES};
use c2_wire::shutdown_control::{DirectShutdownAck, decode_shutdown_ack, encode_shutdown_initiate};

/// Derive the automatic platform native endpoint for a logical IPC address.
pub fn local_endpoint_from_ipc_address(address: &str) -> Result<LocalEndpoint, IpcError> {
    LocalEndpoint::from_address(address).map_err(endpoint_error)
}

pub(crate) fn endpoint_error(error: io::Error) -> IpcError {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    ) {
        IpcError::Config(error.to_string())
    } else {
        IpcError::Io(error)
    }
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

/// Test-only seams for the control probes.
///
/// Both seams are keyed by the exact logical address, so a watcher or a
/// fault-injection entry only affects the probe that registered it. There is no
/// unattributed global hook: other tests in this binary, and their endpoints,
/// are never observed or changed.
#[cfg(test)]
mod test_seam {
    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};

    static ABSENT_WATCHERS: OnceLock<Mutex<Vec<(String, mpsc::Sender<()>)>>> = OnceLock::new();
    static ABSENT_RETRY_DISABLED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

    /// Reports the next `Exchange::Absent` for `address` exactly once.
    pub(super) fn watch_first_absent(address: &str) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        ABSENT_WATCHERS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("absence watcher registry poisoned")
            .push((address.to_owned(), sender));
        receiver
    }

    /// Called by `ping` every time an exchange classifies the endpoint as absent.
    pub(super) fn note_absent(address: &str) {
        ABSENT_WATCHERS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("absence watcher registry poisoned")
            .retain(|(watched, sender)| {
                if watched == address {
                    let _ = sender.send(());
                    false
                } else {
                    true
                }
            });
    }

    /// Test-only fault injection: `ping` treats its first `Absent` as final.
    pub(super) fn disable_absent_retry(address: &str) {
        ABSENT_RETRY_DISABLED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("absent-retry registry poisoned")
            .insert(address.to_owned());
    }

    pub(super) fn absent_retry_disabled(address: &str) -> bool {
        ABSENT_RETRY_DISABLED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("absent-retry registry poisoned")
            .contains(address)
    }
}

/// Ping the platform native endpoint for this logical address.
pub fn ping(address: &str, timeout: Duration) -> Result<bool, IpcError> {
    ping_with_endpoint(&local_endpoint_from_ipc_address(address)?, timeout)
}

/// Ping in a caller-resolved context; derive only once for the whole operation.
pub fn ping_with_context(
    address: &str,
    context: &LocalEndpointContext,
    timeout: Duration,
) -> Result<bool, IpcError> {
    ping_with_endpoint(&context.endpoint(address).map_err(endpoint_error)?, timeout)
}

/// Ping the exact endpoint, retaining one snapshot through all retries.
pub fn ping_with_endpoint(endpoint: &LocalEndpoint, timeout: Duration) -> Result<bool, IpcError> {
    let endpoint = endpoint.clone();
    #[cfg(test)]
    let address = endpoint.address().to_owned();
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
                Exchange::Absent => {
                    #[cfg(test)]
                    {
                        test_seam::note_absent(&address);
                        if test_seam::absent_retry_disabled(&address) {
                            return Ok(false);
                        }
                    }
                    tokio::time::sleep(remaining.min(Duration::from_millis(10))).await
                }
                Exchange::NoReply => {
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

/// The one endpoint this probe was addressed to owns no listener.
///
/// This is the only absence-shaped success: it is scoped to the single OS
/// native endpoint named by the address, so it is not a cross-namespace probe result.
fn already_stopped() -> DirectShutdownAck {
    DirectShutdownAck {
        acknowledged: true,
        shutdown_started: false,
        server_stopped: true,
        route_outcomes: Vec::new(),
    }
}

/// Initiate shutdown through the platform native endpoint.
///
/// An absent endpoint is reported as already stopped: this address names one
/// platform native endpoint, no listener owns it, and a
/// later server on that same endpoint would be a new incarnation rather than
/// the one this probe was addressed to. The reply `server_stopped: false` is
/// *not* re-derivable from absence — only a live server's acknowledgement
/// carries route outcomes — so absence is answered here instead of being
/// confused with a probe that reached the wrong namespace.
pub fn shutdown(address: &str, timeout: Duration) -> Result<DirectShutdownAck, IpcError> {
    shutdown_with_endpoint(&local_endpoint_from_ipc_address(address)?, timeout)
}

/// Initiate shutdown in a caller-resolved context, with one endpoint snapshot.
pub fn shutdown_with_context(
    address: &str,
    context: &LocalEndpointContext,
    timeout: Duration,
) -> Result<DirectShutdownAck, IpcError> {
    shutdown_with_endpoint(&context.endpoint(address).map_err(endpoint_error)?, timeout)
}

/// Initiate shutdown of exactly this endpoint. The acknowledgement still proves
/// initiation only; completion is observed through the server's native barrier.
pub fn shutdown_with_endpoint(
    endpoint: &LocalEndpoint,
    timeout: Duration,
) -> Result<DirectShutdownAck, IpcError> {
    let endpoint = endpoint.clone();
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
                    return Ok(already_stopped());
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

    /// Fixture-only bound: responder readiness, bind completion, and the
    /// first-absence event. It never bounds a probe under test.
    const FIXTURE_BUDGET: Duration = Duration::from_secs(10);

    /// The probe budget under test, unchanged from the CI assertion.
    const PROBE_BUDGET: Duration = Duration::from_secs(1);

    fn address(label: &str) -> String {
        format!("ipc://control-{label}-{}", std::process::id())
    }

    /// True when the endpoint does not exist at this instant.
    ///
    /// A live endpoint can also be busy or unresponsive, so absence is proven
    /// by the operating-system connect result, not by a missing reply.
    fn endpoint_is_absent(endpoint: &LocalEndpoint) -> bool {
        let endpoint = endpoint.clone();
        blocking(async move {
            match LocalStream::connect(&endpoint, Duration::from_millis(100)).await {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Ok(stream) => {
                    drop(stream);
                    Ok(false)
                }
                Err(_) => Ok(false),
            }
        })
        .expect("local absence probe")
    }

    /// A responder that is already running behind a gate.
    ///
    /// Readiness and binding are separate events: `ready` fires only after the
    /// responder's thread, runtime, and endpoint naming are initialized, so no
    /// fixture setup cost can be mistaken for probe time. The endpoint cannot
    /// exist before the gate opens, so the caller decides exactly when it
    /// appears. One abandoned exchange cannot strand the fixture: every
    /// connection gets its own attempt.
    struct Responder {
        gate: mpsc::Sender<()>,
        ready: mpsc::Receiver<()>,
        bound: mpsc::Receiver<()>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Responder {
        /// Waits until the responder thread, runtime, and endpoint naming are
        /// initialized and it is parked on the gate.
        fn wait_ready(&self) {
            self.ready
                .recv_timeout(FIXTURE_BUDGET)
                .expect("the responder never became ready");
        }

        /// Opens the gate and waits until the endpoint exists.
        fn bind(&self) {
            self.gate.send(()).expect("responder gate receiver dropped");
            self.bound
                .recv_timeout(FIXTURE_BUDGET)
                .expect("the responder never bound the endpoint");
        }

        /// Stops serving, then reports a fixture that failed on its own terms.
        fn stop(mut self) {
            let stop = self.stop.take().expect("responder is already stopped");
            let _ = stop.send(());
            self.thread.join().expect("responder thread panicked");
        }
    }

    fn responder(
        address: String,
        discard_first: bool,
        expected: Vec<u8>,
        reply: Vec<u8>,
    ) -> Responder {
        let (gate, gate_rx) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let (bound_tx, bound) = mpsc::channel();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    // Naming an endpoint costs token and hashing work on
                    // Windows, so it happens before readiness: opening the gate
                    // must only have to create the operating-system endpoint.
                    let endpoint = LocalEndpoint::from_address(&address).unwrap();
                    ready_tx.send(()).unwrap();
                    gate_rx.recv().expect("responder gate sender dropped");
                    let mut listener = LocalListener::bind(&endpoint).unwrap();
                    bound_tx.send(()).unwrap();
                    let mut dropped = discard_first;
                    loop {
                        tokio::select! {
                            _ = &mut stop_rx => break,
                            accepted = listener.accept() => {
                                let mut stream = accepted.unwrap();
                                if dropped {
                                    // Closing this connection is the dropped
                                    // exchange. An attempt the probe abandoned
                                    // before its frame landed is already a
                                    // non-reply, so the read result is moot.
                                    dropped = false;
                                    let _ = read_reply(&mut stream).await;
                                    continue;
                                }
                                let payload = match read_reply(&mut stream).await {
                                    Ok((_, payload)) => payload,
                                    // An exchange the probe abandoned is another
                                    // attempt to answer, not a fixture failure.
                                    Err(_) => continue,
                                };
                                assert_eq!(payload, expected);
                                // A reply the probe never read costs one more
                                // attempt, so writing it is best effort.
                                let _ = stream
                                    .write_all(&frame::encode_frame(
                                        0,
                                        FLAG_SIGNAL | FLAG_RESPONSE,
                                        &reply,
                                    ))
                                    .await;
                            }
                        }
                    }
                });
        });
        Responder {
            gate,
            ready,
            bound,
            stop: Some(stop_tx),
            thread,
        }
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

    /// Drives one probe through the absent-to-ready sequence and returns its
    /// outcome.
    ///
    /// The probe's own first `Exchange::Absent`, reported through the
    /// address-keyed seam, is the event that opens the responder gate. The
    /// endpoint therefore appears strictly after this probe observed it
    /// missing, and a successful probe can only be explained by retrying after
    /// that event. The probe keeps `PROBE_BUDGET`; the fixture waits use
    /// `FIXTURE_BUDGET`.
    fn probe_absent_to_ready(address: &str) -> Result<bool, IpcError> {
        let endpoint = local_endpoint_from_ipc_address(address).expect("fixture address");
        let first_absent = test_seam::watch_first_absent(address);
        let responder = responder(
            address.to_owned(),
            false,
            PING_BYTES.to_vec(),
            PONG_BYTES.to_vec(),
        );
        responder.wait_ready();
        // Sanity check on the fixture, not the retry evidence: the seam event
        // below is what proves this probe saw the endpoint missing.
        assert!(
            endpoint_is_absent(&endpoint),
            "the fixture endpoint existed before the probe started"
        );
        let probe = std::thread::spawn({
            let address = address.to_owned();
            move || ping(&address, PROBE_BUDGET)
        });
        first_absent
            .recv_timeout(FIXTURE_BUDGET)
            .expect("the probe never observed the endpoint as absent");
        // The endpoint appears only after this probe's first absence event.
        responder.bind();
        let outcome = probe.join().expect("probe thread panicked");
        responder.stop();
        outcome
    }

    /// The probe starts while the endpoint does not exist and must keep retrying
    /// until the endpoint appears and answers.
    #[test]
    fn ping_retries_until_endpoint_appears() {
        let address = address("late");
        let outcome = probe_absent_to_ready(&address);
        let probed = outcome.expect("ping must report absence, not a retry error");
        assert!(probed, "ping must succeed once the endpoint appears");
    }

    /// Deterministic counterexample for the retry evidence above: with the same
    /// fixture, the same events, and the same one-second budget, a probe that
    /// treats its first `Absent` as final never reaches the endpoint that binds
    /// right after that event. This is the negation of the positive test's
    /// assertion, so disabling the absent retry makes that test fail.
    #[test]
    fn ping_without_absent_retry_never_reaches_a_late_endpoint() {
        let address = address("no-retry");
        test_seam::disable_absent_retry(&address);
        let outcome = probe_absent_to_ready(&address);
        assert!(
            !matches!(outcome, Ok(true)),
            "without an absent retry the probe cannot succeed"
        );
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
            let responder = responder(
                address.clone(),
                discard_first,
                encode_shutdown_initiate().to_vec(),
                encode_shutdown_ack(&expected).unwrap(),
            );
            responder.wait_ready();
            responder.bind();
            let result = shutdown(&address, PROBE_BUDGET).unwrap();
            responder.stop();
            assert!(result.acknowledged && result.shutdown_started && !result.server_stopped);
            assert!(result.route_outcomes.is_empty());
        }
    }
}
