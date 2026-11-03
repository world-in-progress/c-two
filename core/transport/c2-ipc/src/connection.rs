//! The I/O drivers of one physical client connection. ConnectionCore alone
//! owns publication, release ordering and the normal close decision.

use crate::client::{IpcCallControl, IpcError, PendingMap, ServerPoolState};
use c2_local::{
    AbortHandle, ConnectionEndReason, ConnectionPublisher, LocalReadHalf, LocalWriteHalf,
    RemoteReadGuard,
};
use c2_mem::MemPool;
use c2_wire::{
    chunk::ChunkRegistry,
    connection::{
        ConnectionCore, ConnectionError, ConnectionSide, ControlFrame, WriteBatch, WriteRequest,
    },
    flags::FLAG_SIGNAL,
    frame::encode_frame,
    msg_type::DISCONNECT_BYTES,
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
    peer: Mutex<Option<Arc<RwLock<ServerPoolState>>>>,
}
impl PhysicalRetirement {
    pub fn new(guard: RemoteReadGuard, end: Arc<EndState>) -> Arc<Self> {
        Arc::new(Self {
            guard: Some(guard),
            end,
            peer: Mutex::new(None),
        })
    }
    pub fn set_peer(&self, peer: Arc<RwLock<ServerPoolState>>) {
        *self.peer.lock() = Some(peer);
    }
}
impl Drop for PhysicalRetirement {
    fn drop(&mut self) {
        // Both I/O drivers and every admitted remote read have ended. Cache
        // metadata retained by an evicted client must not retain OS mappings.
        if let Some(peer) = self.peer.get_mut().take() {
            peer.write().cache.take();
        }
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
    disconnect: Arc<Mutex<Disconnect>>,
    disconnect_bytes: Option<Vec<u8>>,
}
impl Drop for WriteDriver {
    fn drop(&mut self) {
        // Cancel an unsubmitted control reservation when the driver ends.
        *self.disconnect.lock() = Disconnect::Finished;
        self.stop_gc.send_replace(true);
        drop(self.stream.take());
    }
}

// Drop the GC task's storage before its retirement fence, including if the
// executor drops the task without polling it. Physical close then also proves
// that maintenance no longer retains idle backing.
struct GcDriver {
    pool: Arc<RwLock<MemPool>>,
    peer: Arc<RwLock<ServerPoolState>>,
    registry: Arc<ChunkRegistry<()>>,
    _retirement: Arc<PhysicalRetirement>,
}

enum Disconnect {
    NotRequested,
    Pending(ControlFrame),
    Finished,
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
    disconnect: Arc<Mutex<Disconnect>>,
    runtime: tokio::runtime::Handle,
    write_timeout: Duration,
    #[cfg(any(test, feature = "test-support"))]
    pub pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    #[cfg(any(test, feature = "test-support"))]
    pub prefix_pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}
impl ClientConnection {
    pub fn spawn(
        stream: LocalWriteHalf,
        pending: Arc<Mutex<PendingMap>>,
        abort: AbortHandle,
        retirement: Arc<PhysicalRetirement>,
        write_timeout: Duration,
    ) -> Arc<Self> {
        let wake = Arc::new(Notify::new());
        let notify = wake.clone();
        let core = ConnectionCore::new(ConnectionSide::Client, move || {
            notify.notify_waiters();
            notify.notify_one();
        });
        core.finish_handshake().expect("a fresh connection core");
        let (queue, receiver) = mpsc::channel(64);
        let disconnect = Arc::new(Mutex::new(Disconnect::NotRequested));
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
            disconnect: disconnect.clone(),
            runtime: tokio::runtime::Handle::current(),
            write_timeout,
            #[cfg(any(test, feature = "test-support"))]
            pause: Mutex::new(None),
            #[cfg(any(test, feature = "test-support"))]
            prefix_pause: Mutex::new(None),
        });
        let driver = WriteDriver {
            stream: Some(stream),
            _retirement: retirement,
            stop_gc: conn.gc_stop.clone(),
            disconnect,
            disconnect_bytes: Some(encode_frame(0, FLAG_SIGNAL, &DISCONNECT_BYTES)),
        };
        tokio::spawn(run(driver, conn.clone(), receiver));
        conn
    }
    fn stop_admission(&self) {
        drop(self.core.deactivate());
        // A queued future need not be polled again to stop retaining a pending
        // entry. Sent waiters remain until their own reply or a framing failure.
        let mut pending = self.pending.lock();
        for (_, entry) in pending.extract_if(|_, entry| {
            // The writer holds this lock while publication linearizes. It
            // may itself report a preparation failure here, so never wait on
            // that lock; its own receipt resolves the in-progress submission.
            entry.state.try_lock().is_some_and(|state| !state.started())
        }) {
            entry.complete(Err(IpcError::Closed));
        }
        drop(pending);
        self.deactivated.send_replace(true);
    }
    /// Stop admission and drain sent calls and loans before physical close.
    pub fn deactivate(&self) {
        {
            let mut disconnect = self.disconnect.lock();
            // Reserve before deactivation so the writer cannot seal B=L=0 in
            // between. Only the already-deactivated driver can submit it.
            let control = if matches!(*disconnect, Disconnect::NotRequested) {
                self.core.prepare_control_frame().ok()
            } else {
                None
            };
            self.end
                .record(ConnectionEndReason::LocalClose, "client_deactivate");
            self.stop_admission();
            if matches!(*disconnect, Disconnect::NotRequested) {
                *disconnect = control.map_or(Disconnect::Finished, Disconnect::Pending);
            }
        }
        self.wake.notify_waiters();
        self.wake.notify_one();
    }
    fn take_disconnect(&self) -> Option<ControlFrame> {
        // DISCONNECT stops the peer's admission too. Send it only after every
        // dispatched request (including its continuation frames) has replied.
        if !self.pending.lock().is_empty() {
            return None;
        }
        let mut disconnect = self.disconnect.lock();
        match std::mem::replace(&mut *disconnect, Disconnect::Finished) {
            Disconnect::Pending(control) => Some(control),
            previous => {
                *disconnect = previous;
                None
            }
        }
    }
    pub fn end(&self, reason: ConnectionEndReason, operation: &'static str) {
        self.end.record(reason, operation);
        self.stop_admission();
    }
    pub fn end_io(&self, operation: &'static str, error: &io::Error, read: bool) {
        self.end.record_io(operation, error, read);
        self.stop_admission();
    }
    /// Readable framing preserves every other call's outcome, including while
    /// admission is stopped for a release violation or local bookkeeping error.
    pub fn response_failed(&self, rid: u64, error: IpcError) {
        let target = self.pending.lock().remove(&rid);
        if let Some(entry) = target {
            entry.complete(Err(error.response_failure()));
        }
        self.wake.notify_one();
    }
    pub fn framing_failed(&self, operation: &'static str) {
        self.end(ConnectionEndReason::ProtocolError, operation);
        self.fail_pending();
    }
    fn fail_pending(&self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, entry) in pending {
            entry.complete(Err(IpcError::Closed));
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
        // ConnectionCore accounts for loans and real remote reads; the client
        // also has sent calls whose replies still have trustworthy framing.
        if !self.pending.lock().is_empty() {
            return false;
        }
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

    #[cfg(test)]
    pub fn queue_capacity_for_test(&self) -> usize {
        self.queue.capacity()
    }

    pub fn start_gc(
        &self,
        pool: Arc<RwLock<MemPool>>,
        peer: Arc<RwLock<ServerPoolState>>,
        registry: Arc<ChunkRegistry<()>>,
        retirement: Arc<PhysicalRetirement>,
        decay_seconds: f64,
    ) {
        let mut stop = self.gc_stop.subscribe();
        let period = registry
            .config()
            .gc_interval
            .min(Duration::from_secs_f64(decay_seconds))
            .max(Duration::from_millis(5));
        let driver = GcDriver {
            pool,
            peer,
            registry,
            _retirement: retirement,
        };
        tokio::spawn(async move {
            let driver = driver;
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
                        driver.registry.try_gc_sweep();
                        if let Some(mut pool) = driver.pool.try_write() { pool.gc_buddy(); }
                        if let Some(mut pool) = driver.registry.pool().try_write() { pool.gc_buddy(); }
                        if let Some(peer) = driver.peer.try_read() {
                            if let Some(cache) = &peer.cache { cache.gc_idle(); }
                        }
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
        // Subscribe before waiting for queue capacity. Deactivation must wake
        // unpublished calls even while the writer and all 64 queue slots stall.
        let mut stopped = self.deactivated.subscribe();
        let started = state.lock().started();
        if *stopped.borrow() && !started {
            return Err(IpcError::Closed);
        }
        let request = Arc::new(Mutex::new(Some(request)));
        let mut cancellation = QueuedWrite {
            request: request.clone(),
            finished: false,
        };
        let (reply, mut receipt) = oneshot::channel();
        let permit = tokio::select! {
            biased;
            _ = stopped.changed(), if !started => return Err(IpcError::Closed),
            permit = self.queue.reserve() => permit.map_err(|_| IpcError::Closed)?,
        };
        if *stopped.borrow() && !state.lock().started() {
            return Err(IpcError::Closed);
        }
        permit.send(Submission {
            bytes,
            request,
            state: state.clone(),
            policy,
            reply: Some(reply),
        });
        let is_stopped = *stopped.borrow();
        let result = if is_stopped && !state.lock().started() {
            Err(IpcError::Closed)
        } else {
            tokio::select! {
                biased;
                result = &mut receipt => result.map_err(|_| IpcError::Closed)?,
                _ = stopped.changed(), if !is_stopped => {
                    let started = state.lock().started();
                    if started {
                        receipt.await.map_err(|_| IpcError::Closed)?
                    } else {
                        Err(IpcError::Closed)
                    }
                }
            }
        };
        cancellation.finished = state.lock().started();
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
fn run(
    driver: WriteDriver,
    conn: Arc<ClientConnection>,
    receiver: mpsc::Receiver<Submission>,
) -> impl std::future::Future<Output = ()> + Send {
    // Queued owners and the connection's book go before the retirement fence,
    // even if the writer is cancelled before its first poll.
    struct WriteTask {
        receiver: mpsc::Receiver<Submission>,
        conn: Arc<ClientConnection>,
        driver: WriteDriver,
    }
    let task = WriteTask {
        receiver,
        conn,
        driver,
    };
    async move {
        let mut task = task;
        let conn = &task.conn;
        let receiver = &mut task.receiver;
        let driver = &mut task.driver;
        let stream = driver.stream.as_mut().unwrap();
        loop {
            if conn.close_ready() || conn.read_stopped.load(Ordering::Acquire) {
                break;
            }
            if let Some(control) = conn.take_disconnect() {
                if let Ok(Some(batch)) = conn.core.begin_write(WriteRequest::Control(control)) {
                    let frame = driver
                        .disconnect_bytes
                        .take()
                        .expect("one DISCONNECT per connection");
                    if let Ok(batch) = prepare(batch, frame, &conn) {
                        let _ = write_batch(stream, &conn, batch, &mut None).await;
                    }
                }
                continue;
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
                    if !state.started && (state.cancelled || conn.core.is_deactivated()) {
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
    #[cfg(any(test, feature = "test-support"))]
    if prepared.is_ok() {
        let pause = conn.pause.lock().take();
        if let Some((entered, resume)) = pause {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }
    let result = match prepared {
        Ok(batch) => {
            #[cfg(any(test, feature = "test-support"))]
            let batch = {
                let mut batch = batch;
                batch.prefix_pause = conn.prefix_pause.lock().take();
                batch
            };
            write_batch(stream, conn, batch, &mut submission.reply).await
        }
        Err(error) => Err(error),
    };
    if let Some(reply) = submission.reply {
        let _ = reply.send(result);
    }
}

struct PreparedWrite {
    batch: WriteBatch,
    bytes: Vec<u8>,
    #[cfg(any(test, feature = "test-support"))]
    frame_start: usize,
    #[cfg(any(test, feature = "test-support"))]
    prefix_pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

fn prepare(
    mut batch: WriteBatch,
    frame: Vec<u8>,
    conn: &ClientConnection,
) -> Result<PreparedWrite, IpcError> {
    let prefix = batch.release_bytes();
    #[cfg(any(test, feature = "test-support"))]
    let frame_start = prefix.len();
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
    Ok(PreparedWrite {
        batch,
        bytes,
        #[cfg(any(test, feature = "test-support"))]
        frame_start,
        #[cfg(any(test, feature = "test-support"))]
        prefix_pause: None,
    })
}

async fn write_batch<W: AsyncWrite + Unpin>(
    stream: &mut W,
    conn: &ClientConnection,
    mut write: PreparedWrite,
    receipt: &mut Option<Receipt>,
) -> Result<(), IpcError> {
    let deadline = tokio::time::Instant::now() + conn.write_timeout;
    let mut timed_out = false;
    let mut written = 0;
    let mut boundary = 4 + u32::from_le_bytes(write.bytes[..4].try_into().unwrap()) as usize;
    while written < write.bytes.len() {
        let limit = write.bytes.len();
        #[cfg(any(test, feature = "test-support"))]
        let limit = if write.prefix_pause.is_some() {
            write.frame_start + 8
        } else {
            limit
        };
        let mut operation = Box::pin(AsyncWriteExt::write(stream, &write.bytes[written..limit]));
        let result = if timed_out {
            operation.await
        } else {
            tokio::select! {
                result = &mut operation => result,
                _ = tokio::time::sleep_until(deadline) => {
                    timeout(conn, "write_timeout"); timed_out = true; operation.await
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
        #[cfg(any(test, feature = "test-support"))]
        if written == write.frame_start + 8 {
            if let Some((entered, resume)) = write.prefix_pause.take() {
                let _ = entered.send(());
                let _ = resume.await;
            }
        }
    }
    let mut operation = Box::pin(AsyncWriteExt::flush(stream));
    let result = if timed_out {
        operation.await
    } else {
        tokio::select! {
            result = &mut operation => result,
            _ = tokio::time::sleep_until(deadline) => { timeout(conn, "flush_timeout"); operation.await }
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

fn timeout(conn: &ClientConnection, operation: &'static str) {
    let error = io::Error::new(io::ErrorKind::TimedOut, "local write deadline expired");
    // The timeout stops new work; it cannot cancel an already-started write
    // or turn that call's eventual readable reply into an unknown outcome.
    conn.end_io(operation, &error, false);
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
