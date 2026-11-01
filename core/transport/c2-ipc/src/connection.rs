//! The I/O drivers of one physical client connection. ConnectionCore alone
//! owns publication, release ordering and the normal close decision.

use crate::client::{IpcCallControl, IpcError, PendingMap};
use c2_local::{
    AbortHandle, ConnectionEndReason, ConnectionPublisher, DEFAULT_WRITE_TIMEOUT, LocalReadHalf,
    LocalWriteHalf, RemoteReadGuard,
};
use c2_mem::MemPool;
use c2_wire::{
    chunk::ChunkRegistry,
    connection::{ConnectionCore, ConnectionError, ConnectionSide, WriteBatch, WriteRequest},
};
use parking_lot::{Mutex, RwLock};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};

type Receipt = oneshot::Sender<Result<(), IpcError>>;

/// Publication and cancellation linearize at the writer's first I/O attempt.
/// After that point cancellation removes only the caller's response waiter.
#[derive(Default)]
pub(crate) struct SendState {
    started: bool,
    cancelled: bool,
}
impl SendState {
    pub fn started(&self) -> bool {
        self.started
    }
    pub fn cancel(&mut self) -> bool {
        self.cancelled = true;
        !self.started
    }
}

struct Submission {
    bytes: Vec<u8>,
    request: Arc<Mutex<Option<WriteRequest>>>,
    state: Arc<Mutex<SendState>>,
    policy: Option<IpcCallControl>,
    reply: Option<Receipt>,
}

#[derive(Clone, Copy)]
enum End {
    Explicit(ConnectionEndReason, &'static str),
    Io(&'static str, io::ErrorKind, Option<i32>, bool),
}

/// The sole source for the eventual public terminal record, including failed
/// handshakes. It is intentionally independent of c2-local's fallback record.
pub(crate) struct EndState {
    pub publisher: ConnectionPublisher,
    first: Mutex<Option<End>>,
}
impl EndState {
    pub fn new(publisher: ConnectionPublisher) -> Arc<Self> {
        Arc::new(Self {
            publisher,
            first: Mutex::new(None),
        })
    }
    pub fn record(&self, reason: ConnectionEndReason, operation: &'static str) {
        self.first
            .lock()
            .get_or_insert(End::Explicit(reason, operation));
    }
    pub fn record_io(&self, operation: &'static str, error: &io::Error, read: bool) {
        self.first.lock().get_or_insert(End::Io(
            operation,
            error.kind(),
            error.raw_os_error(),
            read,
        ));
    }
    pub fn publish(&self) {
        let first = *self.first.lock();
        match first {
            Some(End::Explicit(reason, operation)) => {
                self.publisher.close(reason, operation, None);
            }
            Some(End::Io(operation, kind, os, read)) => {
                let error = os
                    .map(io::Error::from_raw_os_error)
                    .unwrap_or_else(|| io::Error::from(kind));
                if read {
                    self.publisher.close_read_error(operation, &error);
                } else {
                    self.publisher.close_io(operation, &error);
                }
            }
            None => {
                self.publisher
                    .close(ConnectionEndReason::LocalClose, "stream_retired", None);
            }
        }
    }
}

/// I/O drivers and real remote-read intervals share this owner. Business
/// references and CloseRecord metadata never keep a native stream alive.
pub(crate) struct PhysicalRetirement {
    guard: Option<RemoteReadGuard>,
    pub end: Arc<EndState>,
}
impl PhysicalRetirement {
    pub fn new(guard: RemoteReadGuard, end: Arc<EndState>) -> Arc<Self> {
        Arc::new(Self {
            guard: Some(guard),
            end,
        })
    }
}
impl Drop for PhysicalRetirement {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.end.publish();
    }
}

pub(crate) struct ReadDriver {
    pub stream: Option<LocalReadHalf>,
    pub retirement: Arc<PhysicalRetirement>,
}
impl Drop for ReadDriver {
    fn drop(&mut self) {
        drop(self.stream.take());
    }
}

struct WriteDriver {
    stream: Option<LocalWriteHalf>,
    _retirement: Arc<PhysicalRetirement>,
    stop_gc: watch::Sender<bool>,
}
impl Drop for WriteDriver {
    fn drop(&mut self) {
        self.stop_gc.send_replace(true);
        drop(self.stream.take());
    }
}

pub(crate) struct ClientConnection {
    pub core: ConnectionCore,
    pub wake: Arc<Notify>,
    pub publisher: ConnectionPublisher,
    end: Arc<EndState>,
    pending: Arc<Mutex<PendingMap>>,
    queue: mpsc::Sender<Submission>,
    abort: AbortHandle,
    read_stopped: AtomicBool,
    deactivated: watch::Sender<bool>,
    gc_stop: watch::Sender<bool>,
    runtime: tokio::runtime::Handle,
    #[cfg(test)]
    pub pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}
impl ClientConnection {
    pub fn spawn(
        stream: LocalWriteHalf,
        pending: Arc<Mutex<PendingMap>>,
        abort: AbortHandle,
        retirement: Arc<PhysicalRetirement>,
    ) -> Arc<Self> {
        let wake = Arc::new(Notify::new());
        let notify = wake.clone();
        let core = ConnectionCore::new(ConnectionSide::Client, move || {
            notify.notify_waiters();
            notify.notify_one();
        });
        core.finish_handshake().expect("a fresh connection core");
        let (queue, receiver) = mpsc::channel(64);
        let conn = Arc::new(Self {
            core,
            wake,
            publisher: retirement.end.publisher.clone(),
            end: retirement.end.clone(),
            pending,
            queue,
            abort,
            read_stopped: AtomicBool::new(false),
            deactivated: watch::channel(false).0,
            gc_stop: watch::channel(false).0,
            runtime: tokio::runtime::Handle::current(),
            #[cfg(test)]
            pause: Mutex::new(None),
        });
        let driver = WriteDriver {
            stream: Some(stream),
            _retirement: retirement,
            stop_gc: conn.gc_stop.clone(),
        };
        tokio::spawn(run(driver, conn.clone(), receiver));
        conn
    }
    fn stop_admission(&self) {
        drop(self.core.deactivate());
        self.deactivated.send_replace(true);
    }
    pub fn end(&self, reason: ConnectionEndReason, operation: &'static str) {
        self.end.record(reason, operation);
        self.stop_admission();
        self.fail_pending();
    }
    pub fn end_io(&self, operation: &'static str, error: &io::Error, read: bool) {
        self.end.record_io(operation, error, read);
        self.stop_admission();
        self.fail_pending();
    }
    /// Stop admission before waking the affected waiter. Its response failure
    /// remains DispatchUncertain even when deactivation also wakes its sender.
    pub fn response_failed(&self, rid: u64, error: IpcError) {
        let target = self.pending.lock().remove(&rid);
        self.end
            .record(ConnectionEndReason::ProtocolError, "response_decode");
        drop(self.core.deactivate());
        if let Some(entry) = target {
            let _ = entry.tx.send(Err(error.response_failure()));
        }
        self.deactivated.send_replace(true);
        self.fail_pending();
    }
    pub fn fail_pending(&self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, entry) in pending {
            let _ = entry.tx.send(Err(IpcError::Closed));
        }
    }
    pub fn read_stopped(&self, peer_end: bool) {
        if peer_end {
            drop(self.core.peer_end());
        }
        self.fail_pending();
        self.read_stopped.store(true, Ordering::Release);
        // This only cancels local I/O. PhysicalRetirement still keeps the
        // actual stream alive until the last remote read ends; L is untouched.
        self.abort.abort();
        self.wake.notify_waiters();
        self.wake.notify_one();
    }
    fn close_ready(&self) -> bool {
        if !self.core.try_close() {
            return false;
        }
        self.abort.abort();
        true
    }
    pub fn spawn_send<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    pub fn start_gc(
        &self,
        pool: Arc<RwLock<MemPool>>,
        registry: Arc<ChunkRegistry<()>>,
        decay_seconds: f64,
    ) {
        let mut stop = self.gc_stop.subscribe();
        let period = registry
            .config()
            .gc_interval
            .min(Duration::from_secs_f64(decay_seconds.max(0.005)));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.tick().await;
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    changed = stop.changed() => { if changed.is_err() || *stop.borrow() { break; } }
                    _ = tick.tick() => {
                        registry.try_gc_sweep();
                        if let Some(mut pool) = pool.try_write() { pool.gc_buddy(); }
                        if let Some(mut pool) = registry.pool().try_write() { pool.gc_buddy(); }
                    }
                }
            }
        });
    }
    pub async fn submit(
        self: &Arc<Self>,
        bytes: Vec<u8>,
        request: WriteRequest,
        state: Arc<Mutex<SendState>>,
        policy: Option<IpcCallControl>,
    ) -> Result<(), IpcError> {
        let request = Arc::new(Mutex::new(Some(request)));
        let mut cancellation = QueuedWrite {
            request: request.clone(),
            finished: false,
        };
        let (reply, mut receipt) = oneshot::channel();
        self.queue
            .send(Submission {
                bytes,
                request,
                state,
                policy,
                reply: Some(reply),
            })
            .await
            .map_err(|_| IpcError::Closed)?;
        let mut stopped = self.deactivated.subscribe();
        let is_stopped = *stopped.borrow();
        let result = if is_stopped {
            receipt.try_recv().unwrap_or(Err(IpcError::Closed))
        } else {
            tokio::select! { biased; result = receipt => result.map_err(|_| IpcError::Closed)?, _ = stopped.changed() => Err(IpcError::Closed), }
        };
        cancellation.finished = true;
        result
    }
}
struct QueuedWrite {
    request: Arc<Mutex<Option<WriteRequest>>>,
    finished: bool,
}
impl Drop for QueuedWrite {
    fn drop(&mut self) {
        if !self.finished {
            drop(self.request.lock().take());
        }
    }
}
async fn run(
    mut driver: WriteDriver,
    conn: Arc<ClientConnection>,
    mut receiver: mpsc::Receiver<Submission>,
) {
    let stream = driver.stream.as_mut().unwrap();
    loop {
        if conn.close_ready() || conn.read_stopped.load(Ordering::Acquire) {
            break;
        }
        if let Ok(submission) = receiver.try_recv() {
            process(stream, &conn, submission).await;
            continue;
        }
        match conn.core.begin_write(WriteRequest::Releases) {
            Ok(Some(batch)) => {
                match prepare(batch, Vec::new(), &conn) {
                    Ok(batch) => {
                        let _ = write_batch(stream, &conn, batch, &mut None).await;
                    }
                    Err(_) => conn.end(ConnectionEndReason::ProtocolError, "release_write"),
                }
                continue;
            }
            Ok(None) => {}
            Err(
                ConnectionError::WriterBroken
                | ConnectionError::PeerEnded
                | ConnectionError::Closed,
            ) => {}
            Err(_) => conn.end(ConnectionEndReason::ProtocolError, "release_writer"),
        }
        tokio::select! { biased; _ = conn.wake.notified() => {} Some(submission) = receiver.recv() => process(stream, &conn, submission).await, }
    }
}
async fn process(stream: &mut LocalWriteHalf, conn: &ClientConnection, mut submission: Submission) {
    let prepared = {
        let mut slot = submission.request.lock();
        match slot.take() {
            None => return,
            Some(request) => {
                if submission
                    .reply
                    .as_ref()
                    .is_some_and(oneshot::Sender::is_closed)
                {
                    return;
                }
                (|| {
                    let mut state = submission.state.lock();
                    if state.cancelled && !state.started {
                        return Err(IpcError::Closed);
                    }
                    IpcCallControl::begin_dispatch(submission.policy.as_ref())?;
                    let batch = conn
                        .core
                        .begin_write(request)
                        .map_err(core_error)?
                        .expect("one submitted frame");
                    let batch = prepare(batch, submission.bytes, conn)?;
                    state.started = true;
                    Ok(batch)
                })()
            }
        }
    };
    #[cfg(test)]
    if prepared.is_ok() {
        let pause = conn.pause.lock().take();
        if let Some((entered, resume)) = pause {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }
    let result = match prepared {
        Ok(batch) => write_batch(stream, conn, batch, &mut submission.reply).await,
        Err(error) => Err(error),
    };
    if let Some(reply) = submission.reply {
        let _ = reply.send(result);
    }
}

struct PreparedWrite {
    batch: WriteBatch,
    bytes: Vec<u8>,
}

fn prepare(
    mut batch: WriteBatch,
    frame: Vec<u8>,
    conn: &ClientConnection,
) -> Result<PreparedWrite, IpcError> {
    let prefix = batch.release_bytes();
    let bytes = if prefix.is_empty() {
        frame
    } else {
        let length = prefix.len().checked_add(frame.len()).ok_or_else(|| {
            conn.end(ConnectionEndReason::ProtocolError, "write_capacity");
            IpcError::Protocol("write length overflow".into())
        })?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|error| {
            conn.end(ConnectionEndReason::ProtocolError, "write_capacity");
            IpcError::Io(io::Error::new(io::ErrorKind::OutOfMemory, error))
        })?;
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&frame);
        bytes
    };
    batch.begin_io().map_err(core_error)?;
    Ok(PreparedWrite { batch, bytes })
}

async fn write_batch<W: AsyncWrite + Unpin>(
    stream: &mut W,
    conn: &ClientConnection,
    mut write: PreparedWrite,
    receipt: &mut Option<Receipt>,
) -> Result<(), IpcError> {
    let deadline = tokio::time::Instant::now() + DEFAULT_WRITE_TIMEOUT;
    let mut timed_out = false;
    let mut written = 0;
    let mut boundary = 4 + u32::from_le_bytes(write.bytes[..4].try_into().unwrap()) as usize;
    while written < write.bytes.len() {
        let mut operation = Box::pin(AsyncWriteExt::write(stream, &write.bytes[written..]));
        let result = if timed_out {
            operation.await
        } else {
            tokio::select! {
                result = &mut operation => result,
                _ = tokio::time::sleep_until(deadline) => {
                    timeout(conn, receipt, "write_timeout"); timed_out = true; operation.await
                }
            }
        };
        let result = result.and_then(|count| {
            if count == 0 {
                Err(io::Error::from(io::ErrorKind::WriteZero))
            } else {
                Ok(count)
            }
        });
        match result {
            Ok(count) => written += count,
            Err(error) => {
                report_io(conn, receipt, "write", &error);
                write.batch.write_failed();
                return Err(error.into());
            }
        }
        while boundary <= written {
            write.batch.frame_completed();
            if boundary == write.bytes.len() {
                break;
            }
            boundary +=
                4 + u32::from_le_bytes(write.bytes[boundary..boundary + 4].try_into().unwrap())
                    as usize;
        }
    }
    let mut operation = Box::pin(AsyncWriteExt::flush(stream));
    let result = if timed_out {
        operation.await
    } else {
        tokio::select! {
            result = &mut operation => result,
            _ = tokio::time::sleep_until(deadline) => { timeout(conn, receipt, "flush_timeout"); operation.await }
        }
    };
    match result {
        Ok(()) => {
            write.batch.flush_completed();
            Ok(())
        }
        Err(error) => {
            report_io(conn, receipt, "flush", &error);
            write.batch.write_failed();
            Err(error.into())
        }
    }
}

fn report_io(
    conn: &ClientConnection,
    receipt: &mut Option<Receipt>,
    operation: &'static str,
    error: &io::Error,
) {
    // Publish this submission's original error before waking deactivation
    // waiters; otherwise their Closed projection could win the receipt race.
    if let Some(reply) = receipt.take() {
        let copy = error
            .raw_os_error()
            .map(io::Error::from_raw_os_error)
            .unwrap_or_else(|| io::Error::new(error.kind(), error.to_string()));
        let _ = reply.send(Err(IpcError::Io(copy)));
    }
    conn.end_io(operation, error, false);
}

fn timeout(conn: &ClientConnection, receipt: &mut Option<Receipt>, operation: &'static str) {
    let error = io::Error::new(io::ErrorKind::TimedOut, "local write deadline expired");
    report_io(conn, receipt, operation, &error);
}

pub(crate) fn core_error(error: ConnectionError) -> IpcError {
    match error {
        ConnectionError::Deactivated
        | ConnectionError::Closed
        | ConnectionError::PeerEnded
        | ConnectionError::WriterBroken => IpcError::Closed,
        error => IpcError::Protocol(error.to_string()),
    }
}
