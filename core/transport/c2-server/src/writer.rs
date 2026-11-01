//! The one writer for a physical server IPC stream.

use std::io;
use std::sync::Arc;

use c2_local::LocalWriteHalf;
use c2_wire::connection::{ConnectionError, WriteBatch, WriteRequest};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::connection::{Connection, PhysicalRetirement, WriteGuard};
use crate::dispatcher::PreparedShmResponse;

type WriteReceipt = Option<oneshot::Sender<io::Result<Option<PreparedShmResponse>>>>;

const QUEUE_CAPACITY: usize = 64;

struct WriterDriver {
    stream: Option<LocalWriteHalf>,
    retirement: Option<Arc<PhysicalRetirement>>,
}

impl Drop for WriterDriver {
    fn drop(&mut self) {
        // Cancellation can drop the task at any await point. The OS writer
        // reference must go before this driver's retirement reference.
        drop(self.stream.take());
        drop(self.retirement.take());
    }
}

struct Submission {
    bytes: Vec<u8>,
    request: WriteRequest,
    reply: WriteReceipt,
    prepared: Option<(u64, PreparedShmResponse)>,
    // Nonblocking replies retain drain ownership after their dispatcher returns.
    _flight: WriteGuard,
    _pending: Option<OwnedSemaphorePermit>,
}

/// Submit complete frames to the connection's sole write task. Dropping a
/// caller's reply receiver never cancels a frame already started by the task.
pub(crate) struct ConnectionWriter {
    queue: mpsc::Sender<Submission>,
    controls: mpsc::UnboundedSender<Submission>,
    control_budget: Arc<Semaphore>,
    conn: Arc<Connection>,
}

/// Receive-loop replies reserve their pending slot before any async work.
/// The bounded control backlog is independent of business queue occupancy.
pub(crate) struct ControlReplyPermit {
    queue: mpsc::UnboundedSender<Submission>,
    pending: OwnedSemaphorePermit,
    request: WriteRequest,
    flight: WriteGuard,
}

impl ControlReplyPermit {
    pub(crate) fn send(self, bytes: Vec<u8>) {
        let _ = self.queue.send(Submission {
            bytes,
            request: self.request,
            reply: None,
            prepared: None,
            _flight: self.flight,
            _pending: Some(self.pending),
        });
    }
}

impl ConnectionWriter {
    pub(crate) fn spawn(
        write_half: LocalWriteHalf,
        conn: Arc<Connection>,
        write_timeout: Duration,
        control_budget: Arc<Semaphore>,
    ) -> (Arc<Self>, JoinHandle<()>) {
        let (queue, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let (controls, control_receiver) = mpsc::unbounded_channel();
        let writer = Arc::new(Self {
            queue,
            controls,
            control_budget,
            conn: conn.clone(),
        });
        // Keep the physical retirement owner until the write half has gone.
        // Standalone writer tests construct connections without an accepted
        // stream guard, so only live server connections have one.
        let retirement = conn
            .has_remote_read_guard()
            .then(|| conn.remote_read_guard());
        let mut driver = WriterDriver {
            stream: Some(write_half),
            retirement,
        };
        let task = tokio::spawn(async move {
            run(
                driver.stream.as_mut().unwrap(),
                conn,
                receiver,
                control_receiver,
                write_timeout,
            )
            .await;
        });
        (writer, task)
    }

    pub(crate) async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.submit(bytes.to_vec(), None).await.map(|_| ())
    }

    /// Keep the local response owner until the sole writer can publish it.
    /// Deactivation returns the unpublished owner to the caller for an owned
    /// inline/chunked reply; no accepted outcome is lost to that race.
    pub(crate) async fn write_prepared(
        &self,
        request_id: u64,
        prepared: PreparedShmResponse,
    ) -> io::Result<Option<PreparedShmResponse>> {
        self.submit(Vec::new(), Some((request_id, prepared))).await
    }

    async fn submit(
        &self,
        bytes: Vec<u8>,
        prepared: Option<(u64, PreparedShmResponse)>,
    ) -> io::Result<Option<PreparedShmResponse>> {
        let (reply, receipt) = oneshot::channel();
        // Every queued response retains write access through deactivation;
        // only publication of a new SHM loan requires business admission.
        let request =
            WriteRequest::Control(self.conn.core.prepare_control_frame().map_err(core_error)?);
        self.queue
            .send(Submission {
                bytes,
                request,
                prepared,
                reply: Some(reply),
                _flight: WriteGuard::new(&self.conn),
                _pending: None,
            })
            .await
            .map_err(|_| broken_pipe())?;
        receipt.await.map_err(|_| broken_pipe())?
    }

    /// The receive loop only enqueues; it never waits for a blocked OS write.
    pub(crate) fn reserve_control(&self) -> io::Result<ControlReplyPermit> {
        let control = self.conn.core.prepare_control_frame().map_err(core_error)?;
        let pending = self
            .control_budget
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                // This is the configured pending budget, not the writer's
                // implementation queue. Preserve accepted calls and keep
                // reading releases after sealing further admission.
                self.conn.end(
                    c2_local::ConnectionEndReason::LocalClose,
                    "reply_pending_capacity",
                );
                io::Error::new(io::ErrorKind::OutOfMemory, "pending reply budget exhausted")
            })?;
        Ok(ControlReplyPermit {
            queue: self.controls.clone(),
            pending,
            request: WriteRequest::Control(control),
            flight: WriteGuard::new(&self.conn),
        })
    }

    pub(crate) fn queue_control(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.reserve_control()?.send(bytes);
        Ok(())
    }
}

async fn run(
    stream: &mut LocalWriteHalf,
    conn: Arc<Connection>,
    mut receiver: mpsc::Receiver<Submission>,
    mut controls: mpsc::UnboundedReceiver<Submission>,
    write_timeout: Duration,
) {
    let mut queue_closed = false;
    let mut controls_closed = false;
    loop {
        if conn.reading_stopped() {
            break;
        }
        if !controls_closed {
            match controls.try_recv() {
                Ok(submission) => {
                    process_submission(stream, &conn, submission, write_timeout).await;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => controls_closed = true,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        if !queue_closed {
            match receiver.try_recv() {
                Ok(submission) => {
                    process_submission(stream, &conn, submission, write_timeout).await;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => queue_closed = true,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        // A completed remote read wakes this task even when no RPC is queued.
        if conn.handshake_done() {
            match conn.core.begin_write(WriteRequest::Releases) {
                Ok(Some(batch)) => {
                    if write_batch(stream, &conn, batch, None, write_timeout)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                Ok(None) => {}
                Err(ConnectionError::Closed | ConnectionError::PeerEnded) => {}
                Err(error) => {
                    conn.end(
                        c2_local::ConnectionEndReason::ProtocolError,
                        "release_writer",
                    );
                    tracing::warn!(%error, "release writer stopped");
                    break;
                }
            }
        }
        if conn.try_close() {
            break;
        }
        tokio::select! {
            biased;
            _ = conn.writer_wake.notified() => {}
            next = controls.recv(), if !controls_closed => {
                match next {
                    Some(submission) => process_submission(stream, &conn, submission, write_timeout).await,
                    None => controls_closed = true,
                }
            }
            next = receiver.recv(), if !queue_closed => {
                match next {
                    Some(submission) => process_submission(stream, &conn, submission, write_timeout).await,
                    None => queue_closed = true,
                }
            }
        }
    }
}

async fn process_submission(
    stream: &mut LocalWriteHalf,
    conn: &Connection,
    mut submission: Submission,
    write_timeout: Duration,
) {
    if submission
        .reply
        .as_ref()
        .is_some_and(oneshot::Sender::is_closed)
    {
        // A waiter cancelled before publication. Dropping the original loan
        // token releases its unpublished allocation through ConnectionCore.
        return;
    }
    let result = if let Some((request_id, prepared)) = submission.prepared.take() {
        match prepare_response_write(conn, request_id, prepared) {
            Ok(PreparedResponseWrite::Owned(prepared)) => Ok(Some(prepared)),
            Ok(PreparedResponseWrite::Published(batch)) => {
                drive_write(stream, conn, batch, &mut submission.reply, write_timeout)
                    .await
                    .map(|_| None)
            }
            Err(error) => Err(error),
        }
    } else {
        match conn.core.begin_write(submission.request) {
            Ok(Some(batch)) => write_batch(
                stream,
                conn,
                batch,
                Some((&submission.bytes, &mut submission.reply)),
                write_timeout,
            )
            .await
            .map(|_| None),
            Ok(None) => unreachable!("queued submission always supplies a frame"),
            Err(error) => Err(core_error(error)),
        }
    };
    if let Err(error) = &result
        && error.kind() == io::ErrorKind::OutOfMemory
    {
        // prepare_response_write has already dropped publication_gate. Do
        // not re-enter it from its fallible assembly/publication section.
        conn.end(
            c2_local::ConnectionEndReason::LocalClose,
            "write_batch_capacity",
        );
    }
    if let Some(reply) = submission.reply.take() {
        let _ = reply.send(result);
    }
}

struct PendingWrite {
    batch: WriteBatch,
    bytes: Vec<u8>,
    boundaries: Vec<usize>,
}

enum PreparedResponseWrite {
    Owned(PreparedShmResponse),
    Published(PendingWrite),
}

fn assemble_write(
    _conn: &Connection,
    batch: WriteBatch,
    frame: Option<&[u8]>,
) -> io::Result<PendingWrite> {
    #[cfg(test)]
    if _conn
        .fail_next_write_allocation
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "injected write buffer allocation failure",
        ));
    }
    let prefix = batch.release_bytes();
    let frame_bytes = frame.unwrap_or(&[]);
    let total_len = prefix
        .len()
        .checked_add(frame_bytes.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "write batch size overflow"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_len)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(frame_bytes);
    let mut boundaries = Vec::new();
    boundaries
        .try_reserve_exact(batch.frame_count())
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    let mut offset = 0;
    while offset < prefix.len() {
        let total = u32::from_le_bytes(
            prefix[offset..offset + 4]
                .try_into()
                .expect("release prefix header"),
        ) as usize;
        let end = offset + 4 + total;
        boundaries.push(end);
        offset = end;
    }
    if frame.is_some() {
        boundaries.push(bytes.len());
    }
    Ok(PendingWrite {
        batch,
        bytes,
        boundaries,
    })
}

fn prepare_response_write(
    conn: &Connection,
    request_id: u64,
    prepared: PreparedShmResponse,
) -> io::Result<PreparedResponseWrite> {
    use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
    use c2_wire::flags::{FLAG_BUDDY, FLAG_REPLY_V2, FLAG_RESPONSE};
    let _publication = conn.publication_gate.lock();
    if conn.core.is_deactivated() {
        return Ok(PreparedResponseWrite::Owned(prepared));
    }
    let allocation = prepared.shm_allocation().expect("prepared SHM owner");
    let data_size = prepared.data_size();
    let loan = prepared.into_loan(&conn.core).map_err(core_error)?;
    let descriptor = BuddyPayload {
        seg_idx: allocation.seg_idx as u16,
        generation: allocation.generation,
        offset: allocation.offset,
        data_size,
        is_dedicated: allocation.is_dedicated,
        loan_id: loan.id(),
    };
    let frame = c2_wire::frame::encode_frame_parts(
        request_id,
        FLAG_REPLY_V2 | FLAG_RESPONSE | FLAG_BUDDY,
        &[
            &encode_buddy_payload(&descriptor),
            &[c2_wire::control::STATUS_SUCCESS],
        ],
    );
    let batch = conn
        .core
        .begin_write(WriteRequest::Business(vec![loan]))
        .map_err(core_error)?
        .expect("response frame");
    let mut write = assemble_write(conn, batch, Some(&frame))?;
    write.batch.begin_io().map_err(core_error)?;
    Ok(PreparedResponseWrite::Published(write))
}

async fn write_batch<W: AsyncWrite + Unpin>(
    stream: &mut W,
    conn: &Connection,
    batch: WriteBatch,
    mut frame: Option<(&[u8], &mut WriteReceipt)>,
    write_timeout: Duration,
) -> io::Result<()> {
    let mut write = assemble_write(conn, batch, frame.as_ref().map(|(bytes, _)| *bytes))
        .inspect_err(|_| {
            conn.end(
                c2_local::ConnectionEndReason::LocalClose,
                "write_batch_capacity",
            );
        })?;
    write.batch.begin_io().map_err(core_error)?;
    let mut no_reply = None;
    let receipt = frame
        .as_mut()
        .map(|(_, receipt)| &mut **receipt)
        .unwrap_or(&mut no_reply);
    drive_write(stream, conn, write, receipt, write_timeout).await
}

async fn drive_write<W: AsyncWrite + Unpin>(
    stream: &mut W,
    conn: &Connection,
    write: PendingWrite,
    receipt: &mut WriteReceipt,
    write_timeout: Duration,
) -> io::Result<()> {
    let PendingWrite {
        mut batch,
        bytes,
        boundaries,
    } = write;
    let deadline = tokio::time::Instant::now() + write_timeout;
    let mut timed_out = false;
    let mut written = 0;
    let mut completed = 0;
    while written < bytes.len() {
        let mut write = Box::pin(AsyncWriteExt::write(stream, &bytes[written..]));
        let result = if timed_out {
            write.await
        } else {
            tokio::select! {
                result = &mut write => result,
                _ = tokio::time::sleep_until(deadline) => {
                    report_timeout(conn, receipt, "write_timeout");
                    timed_out = true;
                    write.await
                }
            }
        };
        let count = match result {
            Ok(0) => {
                let error =
                    io::Error::new(io::ErrorKind::WriteZero, "connection write returned zero");
                conn.end_io("write", &error);
                batch.write_failed();
                return Err(error);
            }
            Ok(count) => count,
            Err(error) => {
                conn.end_io("write", &error);
                batch.write_failed();
                return Err(error);
            }
        };
        written += count;
        while completed < boundaries.len() && boundaries[completed] <= written {
            batch.frame_completed();
            completed += 1;
        }
    }
    let mut flush = Box::pin(AsyncWriteExt::flush(stream));
    let result = if timed_out {
        flush.await
    } else {
        tokio::select! {
            result = &mut flush => result,
            _ = tokio::time::sleep_until(deadline) => {
                report_timeout(conn, receipt, "flush_timeout");
                flush.await
            }
        }
    };
    match result {
        Ok(()) => {
            batch.flush_completed();
            Ok(())
        }
        Err(error) => {
            conn.end_io("flush", &error);
            batch.write_failed();
            Err(error)
        }
    }
}

fn report_timeout(conn: &Connection, receipt: &mut WriteReceipt, operation: &'static str) {
    let timeout = io::Error::new(io::ErrorKind::TimedOut, "local write deadline expired");
    conn.end_io(operation, &timeout);
    if let Some(sender) = receipt.take() {
        let _ = sender.send(Err(timeout));
    }
}

fn core_error(error: ConnectionError) -> io::Error {
    let kind = match error {
        ConnectionError::Capacity(_) | ConnectionError::LoanIdExhausted => {
            io::ErrorKind::OutOfMemory
        }
        _ => io::ErrorKind::BrokenPipe,
    };
    io::Error::new(kind, error)
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection writer stopped")
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_wire::frame::encode_frame;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn queued_release_precedes_business_in_one_core_batch() {
        let (server, mut client) = crate::test_support::isolated_pair().await;
        let (_reader, mut write_half) = server.into_split();
        let conn = Connection::new(2);
        conn.set_handshake_done(true).unwrap();
        conn.core
            .admit_remote_loan(17, Box::new(()))
            .unwrap()
            .finish();
        let batch = conn
            .core
            .begin_write(WriteRequest::Business(Vec::new()))
            .unwrap()
            .unwrap();
        let release = batch.release_bytes().to_vec();
        let business = encode_frame(7, 0, b"reply");
        let mut expected = release;
        expected.extend_from_slice(&business);
        let mut received = vec![0; expected.len()];
        let mut no_reply = None;
        let (written, read) = tokio::join!(
            write_batch(
                &mut write_half,
                &conn,
                batch,
                Some((&business, &mut no_reply)),
                Duration::from_secs(30),
            ),
            client.read_exact(&mut received),
        );
        written.unwrap();
        read.unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn caller_cancellation_keeps_the_queued_stream_write_owned_by_driver() {
        let (server, mut client) = crate::test_support::isolated_pair().await;
        let (reader, write_half) = server.into_split();
        let conn = Arc::new(Connection::new(1));
        let (writer, task) = ConnectionWriter::spawn(
            write_half,
            conn.clone(),
            Duration::from_secs(30),
            Arc::new(Semaphore::new(1024)),
        );
        let bytes = vec![0x5a; 1024 * 1024];
        let expected_len = bytes.len();
        let caller = tokio::spawn({
            let writer = writer.clone();
            async move { writer.write_all(&bytes).await }
        });
        let mut first = [0u8; 1];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut first),
        )
        .await
        .expect("the stream write must start")
        .unwrap();
        caller.abort();
        let mut received = vec![0; expected_len - 1];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut received),
        )
        .await
        .expect("driver must finish the caller's submitted frame")
        .unwrap();
        assert_eq!(first, [0x5a]);
        assert!(received.iter().all(|byte| *byte == 0x5a));
        conn.end(c2_local::ConnectionEndReason::LocalClose, "test_done");
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("writer must close after deactivation")
            .unwrap();
        drop(reader);
    }
    #[tokio::test(start_paused = true)]
    async fn timeout_keeps_the_same_batch_through_its_release_prefix_and_flush() {
        let conn = Arc::new(Connection::new(8));
        conn.set_handshake_done(true).unwrap();
        conn.core
            .admit_remote_loan(17, Box::new(()))
            .unwrap()
            .finish();
        let batch = conn
            .core
            .begin_write(WriteRequest::Business(vec![]))
            .unwrap()
            .unwrap();
        let business = encode_frame(7, 0, b"reply");
        let expected = [batch.release_bytes(), &business].concat();
        let (mut writer, mut peer) = tokio::io::duplex(4);
        let (reply, receipt) = oneshot::channel();
        let driver = tokio::spawn({
            let conn = conn.clone();
            async move {
                let mut reply = Some(reply);
                write_batch(
                    &mut writer,
                    &conn,
                    batch,
                    Some((&business, &mut reply)),
                    Duration::from_millis(45),
                )
                .await
            }
        });
        let mut received = vec![0; expected.len()];
        peer.read_exact(&mut received[..4]).await.unwrap();
        tokio::time::advance(Duration::from_millis(46)).await;
        let timeout = receipt.await.unwrap().unwrap_err();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(conn.core.is_deactivated());
        assert!(!driver.is_finished());
        let control = conn.core.prepare_control_frame().unwrap();
        assert!(matches!(
            conn.core.begin_write(WriteRequest::Control(control)),
            Err(ConnectionError::WriterBusy)
        ));
        assert!(!conn.try_close());
        peer.read_exact(&mut received[4..]).await.unwrap();
        driver.await.unwrap().unwrap();
        assert_eq!(
            received, expected,
            "timeout must not replace, replay, or split the co-submission"
        );
        assert!(conn.try_close());
    }
}
