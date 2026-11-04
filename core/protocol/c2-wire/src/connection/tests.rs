use super::*;
use crate::flags::{FLAG_CTRL, FLAG_RESPONSE, FLAG_RPC_RECORD};
use crate::frame::decode_frame;
use crate::release::{ReleaseBatch, encode_release_frame};
use c2_mem::{MemoryBudget, config::PoolConfig};
use std::sync::atomic::{AtomicUsize, Ordering};

fn established(side: ConnectionSide) -> ConnectionCore {
    let core = ConnectionCore::new(side, || {});
    core.finish_handshake().unwrap();
    core
}

// File-backed move-only handles let the control-plane algorithm exercise real
// allocation ownership without requiring OS SHM. SHM integration is separate.
fn allocation() -> (Arc<RwLock<MemPool>>, MemoryBudget, MemHandle) {
    let budget = MemoryBudget::new(0, 4096, 0);
    let mut pool = MemPool::new_with_prefix_and_budget(
        PoolConfig {
            buddy_enabled: false,
            min_retained_segments: 0,
            max_segments: 0,
            max_dedicated_segments: 0,
            ..PoolConfig::default()
        },
        "/p3-state".into(),
        budget.clone(),
    )
    .unwrap();
    let handle = pool.alloc_handle(512).unwrap();
    (Arc::new(RwLock::new(pool)), budget, handle)
}

fn receive(core: &ConnectionCore, ids: &[u64], response: bool) -> ReleaseOutcome {
    let bytes = encode_release_frame(&ReleaseBatch::new(ids).unwrap(), response);
    let (header, payload) = decode_frame(&bytes).unwrap();
    core.receive_release(&header, payload)
}

fn published(core: &ConnectionCore) -> (u64, MemoryBudget, WriteBatch) {
    let (pool, budget, handle) = allocation();
    let loan = core.prepare_loan(pool, handle).unwrap();
    let id = loan.id();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    (id, budget, write)
}

#[test]
fn publication_fast_release_and_write_completion_settle_once() {
    let core = established(ConnectionSide::Client);
    let (id, budget, mut write) = published(&core);
    let outcome = receive(&core, &[id], true);
    assert!(outcome.error.is_none());
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    outcome.reclaims.release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    write.frame_completed();
    write.flush_completed();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn cancellation_before_first_write_releases_the_prepared_owner() {
    let core = established(ConnectionSide::Client);
    let (pool, budget, handle) = allocation();
    let loan = core.prepare_loan(pool, handle).unwrap();
    let write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    drop(write);
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(!core.is_deactivated());
}

#[test]
fn flush_failure_after_complete_frame_retains_until_peer_end() {
    let core = established(ConnectionSide::Client);
    let (_, budget, mut write) = published(&core);
    write.frame_completed();
    write.write_failed();
    drop(write);
    assert!(core.is_deactivated());
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    let reclaims = core.peer_end();
    assert!(!core.try_close());
    reclaims.release();
    assert!(core.try_close());
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn remote_read_drop_queues_release_and_wakes_idle_writer() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let notify = wakes.clone();
    let core = ConnectionCore::new(ConnectionSide::Server, move || {
        notify.fetch_add(1, Ordering::SeqCst);
    });
    core.finish_handshake().unwrap();
    let node = core.admit_remote_loan(17, Box::new(())).unwrap();
    assert!(core.begin_write(WriteRequest::Releases).unwrap().is_none());
    drop(node);
    assert!(wakes.load(Ordering::SeqCst) > 0);
    let mut write = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    let (header, payload) = decode_frame(write.release_bytes()).unwrap();
    assert_eq!(header.flags, FLAG_RPC_RECORD | FLAG_RESPONSE);
    assert_eq!(
        crate::release::decode_release_frame(&header, payload, true, true)
            .unwrap()
            .ids(),
        &[17]
    );
    assert!(!write.has_business_frame());
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
}

#[test]
fn zero_unknown_duplicate_unpublished_and_cross_connection_ids_deactivate() {
    for invalid in ["zero", "unknown", "duplicate", "unpublished", "other"] {
        let core = established(ConnectionSide::Client);
        let (id, budget, mut write) = published(&core);
        write.frame_completed();
        write.flush_completed();
        let other = established(ConnectionSide::Client);
        let (other_id, other_budget, mut other_write) = published(&other);
        other_write.frame_completed();
        other_write.flush_completed();
        let (pool, pending_budget, handle) = allocation();
        let pending = core.prepare_loan(pool, handle).unwrap();
        let ids = match invalid {
            "zero" => vec![0],
            "unknown" => vec![u64::MAX],
            "duplicate" => vec![id, id],
            "unpublished" => vec![pending.id()],
            _ => vec![other_id],
        };
        let outcome = receive(&core, &ids, true);
        assert!(
            matches!(outcome.error, Some(ConnectionError::ProtocolViolation(_))),
            "{invalid}"
        );
        assert!(core.is_deactivated());
        outcome.reclaims.release();
        assert_eq!(pending_budget.snapshot().file.used_bytes, 0);
        assert_eq!(
            budget.snapshot().file.used_bytes,
            if invalid == "duplicate" { 0 } else { 512 }
        );
        assert_eq!(other_budget.snapshot().file.used_bytes, 512);
        core.peer_end().release();
        other.peer_end().release();
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(other_budget.snapshot().file.used_bytes, 0);
        drop(pending);
    }
}

#[test]
fn malformed_release_deactivates_without_settling_published_loans() {
    for malformed in ["type", "flags", "length", "count", "direction", "phase"] {
        let core = established(ConnectionSide::Client);
        let (_, budget, mut write) = published(&core);
        write.frame_completed();
        write.flush_completed();
        let bytes = encode_release_frame(&ReleaseBatch::new(&[1]).unwrap(), true);
        let (mut header, payload) = decode_frame(&bytes).unwrap();
        let mut payload = payload.to_vec();
        match malformed {
            "type" => payload[0] = 0x16,
            "flags" => header.flags = FLAG_RESPONSE,
            "length" => payload.push(0),
            "count" => payload[1] = 0,
            "direction" => header.flags = FLAG_CTRL,
            _ => core.shared.state.lock().handshake_done = false,
        }
        let outcome = core.receive_release(&header, &payload);
        assert!(matches!(
            outcome.error,
            Some(ConnectionError::ProtocolViolation(_))
        ));
        outcome.reclaims.release();
        assert!(core.is_deactivated());
        assert_eq!(budget.snapshot().file.used_bytes, 512, "{malformed}");
        core.peer_end().release();
    }
}

#[test]
fn process_sequence_is_unique_across_connections_and_both_directions() {
    let mut ids = std::collections::HashSet::new();
    for side in [ConnectionSide::Client, ConnectionSide::Server] {
        for _ in 0..8 {
            let core = established(side);
            let (pool, budget, handle) = allocation();
            let loan = core.prepare_loan(pool, handle).unwrap();
            assert!(ids.insert(loan.id()));
            assert_ne!(loan.id(), 0);
            drop(loan);
            assert_eq!(budget.snapshot().file.used_bytes, 0);
        }
    }
}

#[test]
fn sequence_exhaustion_issues_last_u64_then_stops_without_wrapping() {
    // A private sequence exercises exhaustion without poisoning the process
    // sequence used by other tests or giving applications another ID authority.
    let sequence = AtomicU64::new(u64::MAX - 1);
    let core = established(ConnectionSide::Client);
    let (pool, retained, handle) = allocation();
    let last = core.prepare_loan_from(pool, handle, &sequence).unwrap();
    assert_eq!(last.id(), u64::MAX);
    let mut write = core
        .begin_write(WriteRequest::Business(vec![last]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    let (pool, rejected, handle) = allocation();
    assert!(matches!(
        core.prepare_loan_from(pool, handle, &sequence),
        Err(ConnectionError::LoanIdExhausted)
    ));
    assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    assert_eq!(rejected.snapshot().file.used_bytes, 0);
    assert!(core.is_deactivated());
    assert_eq!(retained.snapshot().file.used_bytes, 512);
    receive(&core, &[u64::MAX], true).reclaims.release();
    assert_eq!(retained.snapshot().file.used_bytes, 0);
}

#[test]
fn deactivation_between_reservation_and_io_revokes_unpublished_owner() {
    let core = established(ConnectionSide::Client);
    let (pool, budget, handle) = allocation();
    let loan = core.prepare_loan(pool, handle).unwrap();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    let reclaims = core.deactivate();
    assert!(!core.try_close()); // actual allocator release has not completed
    assert_eq!(write.begin_io(), Err(ConnectionError::Deactivated));
    reclaims.release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(!core.try_close());
    drop(write);
    assert!(core.try_close());
    let (pool, rejected, handle) = allocation();
    assert!(matches!(
        core.prepare_loan(pool, handle),
        Err(ConnectionError::Closed)
    ));
    assert_eq!(rejected.snapshot().file.used_bytes, 0);
}

#[test]
fn partial_write_cancellation_keeps_loans_and_never_replays_release_prefix() {
    let core = established(ConnectionSide::Client);
    core.admit_remote_loan(41, Box::new(())).unwrap().finish();
    let (id, budget, mut write) = published(&core);
    assert!(!write.release_bytes().is_empty());
    write.frame_completed(); // prefix complete; business frame may be partial
    drop(write);
    assert!(core.is_deactivated());
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    assert!(matches!(
        core.begin_write(WriteRequest::Releases),
        Err(ConnectionError::WriterBroken)
    ));
    let outcome = receive(&core, &[id], true);
    assert!(outcome.error.is_none());
    outcome.reclaims.release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(core.try_close());
}

fn release_ids(bytes: &[u8], response: bool) -> (Vec<u64>, usize) {
    let mut rest = bytes;
    let mut ids = Vec::new();
    let mut frames = 0;
    while !rest.is_empty() {
        let (header, payload) = decode_frame(rest).unwrap();
        let batch = crate::release::decode_release_frame(&header, payload, response, true).unwrap();
        assert!(batch.ids().len() <= IPC_RELEASE_BATCH_CAPACITY);
        ids.extend_from_slice(batch.ids());
        frames += 1;
        rest = &rest[4 + header.total_len as usize..];
    }
    (ids, frames)
}

#[test]
fn release_queue_batches_and_cosubmits_with_one_business_frame() {
    let core = established(ConnectionSide::Server);
    for id in 1..=130 {
        core.admit_remote_loan(id, Box::new(())).unwrap().finish();
    }
    let mut write = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    let (ids, frames) = release_ids(write.release_bytes(), true);
    assert_eq!(ids, (1..=130).collect::<Vec<_>>());
    assert_eq!(frames, 3);
    assert_eq!(write.frame_count(), 4);
    assert!(write.has_business_frame());
    // The driver performs one write with these bytes; core never receives the
    // business payload or its byte-budget guard.
    let business = crate::rpc::encode_call_end_frame(
        99,
        crate::rpc::CallMode::Unary,
        crate::rpc::CallStatus::Success,
        0,
        crate::rpc::ValuePart::whole(crate::rpc::ValueStorage::Inline(b"business")),
    )
    .unwrap();
    let submission = [write.release_bytes(), &business].concat();
    let tail = &submission[write.release_bytes().len()..];
    assert_eq!(&decode_frame(tail).unwrap().1[2..], b"business");
    write.begin_io().unwrap();
    for _ in 0..4 {
        write.frame_completed();
    }
    write.flush_completed();
}

#[test]
fn releases_wait_for_current_frame_and_advance_between_chunks() {
    let core = established(ConnectionSide::Client);
    let mut first = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    first.begin_io().unwrap();
    core.admit_remote_loan(7, Box::new(())).unwrap().finish();
    assert!(matches!(
        core.begin_write(WriteRequest::Releases),
        Err(ConnectionError::WriterBusy)
    ));
    first.frame_completed();
    first.flush_completed();
    let mut second = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    assert_eq!(release_ids(second.release_bytes(), false).0, [7]);
    assert_eq!(second.frame_count(), 2);
    second.begin_io().unwrap();
    second.frame_completed();
    core.admit_remote_loan(8, Box::new(())).unwrap().finish();
    assert!(matches!(
        core.begin_write(WriteRequest::Releases),
        Err(ConnectionError::WriterBusy)
    ));
    second.frame_completed();
    second.flush_completed();
    let mut idle = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    assert_eq!(release_ids(idle.release_bytes(), false).0, [8]);
    idle.begin_io().unwrap();
    idle.frame_completed();
    idle.flush_completed();
}

#[test]
fn cancelled_unsubmitted_prefix_restores_fifo_even_with_new_admissions() {
    let core = established(ConnectionSide::Client);
    for id in 1..=130 {
        core.admit_remote_loan(id, Box::new(())).unwrap().finish();
    }
    let write = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    let mut nodes = Vec::new();
    for id in 131..=194 {
        nodes.push(core.admit_remote_loan(id, Box::new(())).unwrap());
    }
    for node in nodes {
        node.finish();
    }
    let capacity = core.shared.state.lock().releases.capacity();
    drop(write);
    assert_eq!(core.shared.state.lock().releases.capacity(), capacity);
    let mut retry = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    let (ids, frames) = release_ids(retry.release_bytes(), false);
    assert_eq!(ids, (1..=194).collect::<Vec<_>>());
    assert_eq!(frames, 4);
    retry.begin_io().unwrap();
    for _ in 0..frames {
        retry.frame_completed();
    }
    retry.flush_completed();
}

struct ReadGuard {
    core: ConnectionCore,
    ended: Arc<AtomicUsize>,
}
impl Drop for ReadGuard {
    fn drop(&mut self) {
        // Ending the actual mapping/stream read precedes B decrement. Closing
        // from another task cannot pass the read interval still in this Drop.
        assert!(!self.core.try_close());
        self.ended.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn peer_end_reclaims_l_but_close_waits_for_actual_remote_read_guard() {
    let core = established(ConnectionSide::Client);
    let (_, budget, mut write) = published(&core);
    write.frame_completed();
    write.flush_completed();
    let ended = Arc::new(AtomicUsize::new(0));
    let node = core
        .admit_remote_loan(
            5,
            Box::new(ReadGuard {
                core: core.clone(),
                ended: ended.clone(),
            }),
        )
        .unwrap();
    core.peer_end().release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(!core.try_close());
    node.finish();
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    assert!(core.try_close());
    assert!(core.shared.state.lock().releases.is_empty());
    assert!(matches!(
        core.admit_remote_loan(6, Box::new(())),
        Err(ConnectionError::Closed)
    ));
}

#[test]
fn deactivated_readable_connection_still_releases_late_shm_and_can_close_with_unsent_records() {
    let core = established(ConnectionSide::Server);
    core.deactivate().release();
    let node = core.admit_remote_loan(13, Box::new(())).unwrap();
    assert!(!core.try_close());
    node.finish();
    assert_eq!(core.shared.state.lock().releases.len(), 1);
    assert!(core.try_close());
    assert!(core.shared.state.lock().releases.is_empty());
}

#[test]
fn peer_end_before_first_write_fences_publication() {
    let core = established(ConnectionSide::Client);
    let (pool, budget, handle) = allocation();
    let loan = core.prepare_loan(pool, handle).unwrap();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    core.peer_end().release();
    assert_eq!(write.begin_io(), Err(ConnectionError::PeerEnded));
    drop(write);
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn release_reclaims_do_not_take_pool_lock_in_the_receive_state_machine() {
    use std::time::Duration;
    let core = established(ConnectionSide::Client);
    let (pool, budget, handle) = allocation();
    let loan = core.prepare_loan(pool.clone(), handle).unwrap();
    let id = loan.id();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    let guard = pool.write();
    let receiver = core.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(receive(&receiver, &[id], true)).unwrap();
    });
    let received_while_pool_busy = rx.recv_timeout(Duration::from_secs(2));
    // Core remains accessible while actual allocator release is deferred.
    core.deactivate().release();
    assert!(!core.try_close());
    drop(guard);
    worker.join().unwrap();
    let outcome = received_while_pool_busy.expect("receive waited for an allocator lock");
    assert!(outcome.error.is_none());
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    outcome.reclaims.release();
    assert!(core.try_close());
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn release_write_completion_and_peer_end_race_without_double_reclaim() {
    let core = established(ConnectionSide::Client);
    let (id, budget, mut write) = published(&core);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let receive_core = core.clone();
    let receive_start = barrier.clone();
    let receiver = std::thread::spawn(move || {
        receive_start.wait();
        receive(&receive_core, &[id], true).reclaims.release();
    });
    let end_core = core.clone();
    let end_start = barrier.clone();
    let end = std::thread::spawn(move || {
        end_start.wait();
        end_core.peer_end().release();
    });
    barrier.wait();
    write.frame_completed();
    write.flush_completed();
    receiver.join().unwrap();
    end.join().unwrap();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(core.try_close());
}

#[test]
fn wake_callback_can_observe_state_without_reentering_a_held_lock() {
    let slot: Arc<Mutex<Option<ConnectionCore>>> = Arc::new(Mutex::new(None));
    let reader = slot.clone();
    let core = ConnectionCore::new(ConnectionSide::Client, move || {
        if let Some(core) = reader.lock().as_ref() {
            let _ = core.is_deactivated();
        }
    });
    *slot.lock() = Some(core.clone());
    core.finish_handshake().unwrap();
    core.admit_remote_loan(3, Box::new(())).unwrap().finish();
    core.deactivate().release();
    core.peer_end().release();
    *slot.lock() = None; // break the test callback's intentional reference cycle
}

// Test-thread-local allocation instrumentation verifies the node Drop contract
// without counting allocations made by other concurrent tests or the harness.
struct ProbeAllocator;
thread_local! {
    static TRACK_ALLOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn record_allocation() {
    if TRACK_ALLOCATIONS.try_with(|on| on.get()).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}
unsafe impl std::alloc::GlobalAlloc for ProbeAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation();
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation();
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { std::alloc::GlobalAlloc::realloc(&std::alloc::System, ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}
#[global_allocator]
static TEST_ALLOCATOR: ProbeAllocator = ProbeAllocator;

#[test]
fn remote_node_drop_allocates_nothing_with_a_reserved_prefix_in_flight() {
    let core = established(ConnectionSide::Server);
    let mut nodes = Vec::new();
    for id in 1..=130 {
        nodes.push(core.admit_remote_loan(id, Box::new(())).unwrap());
    }
    for node in nodes {
        node.finish();
    }
    let write = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    let mut nodes = Vec::new();
    for id in 131..=260 {
        nodes.push(core.admit_remote_loan(id, Box::new(())).unwrap());
    }
    ALLOCATIONS.with(|count| count.set(0));
    TRACK_ALLOCATIONS.with(|on| on.set(true));
    drop(nodes);
    TRACK_ALLOCATIONS.with(|on| on.set(false));
    assert_eq!(ALLOCATIONS.with(|count| count.get()), 0);
    drop(write); // restores original prefix into its reserved queue positions
    let next = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    assert_eq!(
        release_ids(next.release_bytes(), true).0,
        (1..=260).collect::<Vec<_>>()
    );
}

#[test]
fn invalid_id_mid_batch_keeps_prior_settlement_and_leaves_later_loan_live() {
    let core = established(ConnectionSide::Client);
    let (first, first_budget, mut first_write) = published(&core);
    first_write.frame_completed();
    first_write.flush_completed();
    let (second, second_budget, mut second_write) = published(&core);
    second_write.frame_completed();
    second_write.flush_completed();
    let outcome = receive(&core, &[first, 0, second], true);
    assert!(matches!(
        outcome.error,
        Some(ConnectionError::ProtocolViolation(_))
    ));
    outcome.reclaims.release();
    assert_eq!(first_budget.snapshot().file.used_bytes, 0);
    assert_eq!(second_budget.snapshot().file.used_bytes, 512);
    core.peer_end().release();
    assert_eq!(second_budget.snapshot().file.used_bytes, 0);
}

#[test]
fn copy_failure_unwind_ends_read_and_queues_exactly_one_record() {
    let core = established(ConnectionSide::Client);
    let reader = core.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _node = reader.admit_remote_loan(71, Box::new(())).unwrap();
        panic!("injected copy failure");
    }));
    assert!(result.is_err());
    let mut write = core.begin_write(WriteRequest::Releases).unwrap().unwrap();
    assert_eq!(release_ids(write.release_bytes(), false), (vec![71], 1));
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    assert!(core.begin_write(WriteRequest::Releases).unwrap().is_none());
}

#[test]
fn exhausted_queue_reservation_deactivates_before_accepting_another_read() {
    let core = established(ConnectionSide::Client);
    let (id, budget, mut write) = published(&core);
    write.frame_completed();
    write.flush_completed();
    // Force checked reservation arithmetic failure without allocating an
    // enormous queue. No node is admitted or allowed to decrement this count.
    core.shared.state.lock().inflight_releases = usize::MAX;
    assert!(matches!(
        core.admit_remote_loan(1, Box::new(())),
        Err(ConnectionError::Capacity("release queue"))
    ));
    core.shared.state.lock().inflight_releases = 0;
    assert!(core.is_deactivated());
    assert_eq!(core.shared.state.lock().remote_reads, 0);
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    receive(&core, &[id], true).reclaims.release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn handshake_fence_and_zero_remote_id_stop_admission() {
    let core = ConnectionCore::new(ConnectionSide::Client, || {});
    let (pool, budget, handle) = allocation();
    assert!(matches!(
        core.prepare_loan(pool, handle),
        Err(ConnectionError::BeforeHandshake)
    ));
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    core.finish_handshake().unwrap();
    assert!(matches!(
        core.admit_remote_loan(0, Box::new(())),
        Err(ConnectionError::ProtocolViolation(_))
    ));
    assert!(core.is_deactivated());
    let other = established(ConnectionSide::Client);
    assert!(matches!(
        other.finish_handshake(),
        Err(ConnectionError::ProtocolViolation(_))
    ));
    assert!(other.is_deactivated());
}

fn shm_allocation() -> (Arc<RwLock<MemPool>>, MemoryBudget, MemHandle) {
    let budget = MemoryBudget::new(64 * 1024, 0, 0);
    let mut pool = MemPool::new_with_prefix_and_budget(
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            min_retained_segments: 0,
            max_dedicated_segments: 0,
            spill_threshold: 1.0,
            ..PoolConfig::default()
        },
        "/p3-shm-proof".into(),
        budget.clone(),
    )
    .unwrap();
    let handle = pool
        .alloc_handle(512)
        .expect("Host must run this proof with POSIX/Windows SHM available");
    assert!(handle.is_buddy());
    (Arc::new(RwLock::new(pool)), budget, handle)
}

#[test]
fn real_shm_release_and_peer_end_consume_each_original_handle_once() {
    let core = established(ConnectionSide::Client);
    let (pool, _budget, handle) = shm_allocation();
    let first = core.prepare_loan(pool.clone(), handle).unwrap();
    let first_id = first.id();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![first]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    assert_eq!(pool.read().stats().alloc_count, 1);
    receive(&core, &[first_id], true).reclaims.release();
    assert_eq!(pool.read().stats().alloc_count, 0);
    let second = core
        .prepare_loan(pool.clone(), pool.write().alloc_handle(512).unwrap())
        .unwrap();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![second]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    core.peer_end().release();
    assert_eq!(pool.read().stats().alloc_count, 0);
}

#[test]
fn final_core_drop_does_not_free_unconfirmed_shm_from_a_live_original_pool() {
    let core = established(ConnectionSide::Server);
    let (pool, budget, handle) = shm_allocation();
    let loan = core.prepare_loan(pool.clone(), handle).unwrap();
    let mut write = core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    drop(core); // no release record or peer-end proof
    assert_eq!(pool.read().stats().alloc_count, 1);
    assert_eq!(budget.snapshot().shm.used_bytes, 64 * 1024);
    drop(pool); // actual pool teardown is allowed to end the retained backing
    assert_eq!(budget.snapshot().shm.used_bytes, 0);
}

#[test]
fn review_started_inline_frame_must_finish_before_physical_close() {
    let core = established(ConnectionSide::Client);
    let mut write = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    core.deactivate().release();
    assert!(
        !core.try_close(),
        "started frame is still owned by the writer"
    );
    write.frame_completed();
    write.flush_completed();
    assert!(core.try_close());
}

#[test]
fn review_writer_is_not_available_until_flush_has_resolved() {
    let core = established(ConnectionSide::Client);
    let mut write = core
        .begin_write(WriteRequest::Business(vec![]))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    assert!(matches!(
        core.begin_write(WriteRequest::Business(vec![])),
        Err(ConnectionError::WriterBusy)
    ));
    write.write_failed();
    assert!(core.is_deactivated());
}

#[test]
fn review_cancellation_during_flush_keeps_uncertain_loan_and_deactivates() {
    let core = established(ConnectionSide::Client);
    let (_, budget, mut write) = published(&core);
    write.frame_completed();
    drop(write); // cancelled before the final flush outcome is known
    assert!(core.is_deactivated());
    assert_eq!(budget.snapshot().file.used_bytes, 512);
    core.peer_end().release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
}

#[test]
fn queued_control_keeps_deactivated_connection_open_through_flush() {
    let core = established(ConnectionSide::Server);
    let control = core.prepare_control_frame().unwrap();
    core.deactivate().release();
    assert!(!core.try_close());
    assert!(matches!(
        core.begin_write(WriteRequest::Business(vec![])),
        Err(ConnectionError::Deactivated)
    ));
    let mut write = core
        .begin_write(WriteRequest::Control(control))
        .unwrap()
        .unwrap();
    assert_eq!(write.frame_count(), 1);
    assert!(!write.has_business_frame());
    write.begin_io().unwrap();
    assert!(!core.try_close());
    write.frame_completed();
    assert!(!core.try_close(), "a complete frame still owns its flush");
    write.flush_completed();
    assert!(core.try_close());
}

#[test]
fn control_can_be_queued_after_deactivation_and_dropped_without_io() {
    let core = established(ConnectionSide::Server);
    core.deactivate().release();
    let control = core.prepare_control_frame().unwrap();
    assert!(!core.try_close());
    drop(control);
    assert!(core.try_close());
    assert!(matches!(
        core.prepare_control_frame(),
        Err(ConnectionError::Closed)
    ));
}

#[test]
fn negotiation_controls_use_the_same_writer_before_handshake() {
    let core = ConnectionCore::new(ConnectionSide::Server, || {});
    let control = core.prepare_control_frame().unwrap();
    let mut write = core
        .begin_write(WriteRequest::Control(control))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.frame_completed();
    write.flush_completed();
    core.finish_handshake().unwrap();
    assert!(!core.is_deactivated());
}

#[test]
fn foreign_control_token_is_rejected_and_releases_its_original_reservation() {
    let original = established(ConnectionSide::Server);
    let other = established(ConnectionSide::Server);
    let control = original.prepare_control_frame().unwrap();
    original.deactivate().release();
    assert!(!original.try_close());
    assert!(matches!(
        other.begin_write(WriteRequest::Control(control)),
        Err(ConnectionError::ProtocolViolation(_))
    ));
    assert!(original.try_close());
    assert!(other.try_close());
}

#[test]
fn deadline_deactivation_keeps_the_whole_started_batch_until_flush() {
    let core = established(ConnectionSide::Server);
    core.admit_remote_loan(17, Box::new(())).unwrap().finish();
    let (id, budget, mut write) = published(&core);
    assert_eq!(write.frame_count(), 2);
    core.deactivate().release(); // the driver records a caller timeout
    receive(&core, &[id], false).reclaims.release();
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    for _ in 0..2 {
        let control = core.prepare_control_frame().unwrap();
        assert!(matches!(
            core.begin_write(WriteRequest::Control(control)),
            Err(ConnectionError::WriterBusy)
        ));
        assert!(matches!(
            core.begin_write(WriteRequest::Releases),
            Err(ConnectionError::WriterBusy)
        ));
        assert!(!core.try_close());
        write.frame_completed();
    }
    let control = core.prepare_control_frame().unwrap();
    assert!(matches!(
        core.begin_write(WriteRequest::Control(control)),
        Err(ConnectionError::WriterBusy)
    ));
    assert!(!core.try_close());
    write.flush_completed();
    assert!(core.try_close());
}

#[test]
fn failed_batch_blocks_all_later_frames_but_still_settles_loans() {
    for peer_end in [false, true] {
        let core = established(ConnectionSide::Server);
        let (id, budget, mut write) = published(&core);
        let queued = core.prepare_control_frame().unwrap();
        write.write_failed();
        drop(write);
        assert!(matches!(
            core.begin_write(WriteRequest::Control(queued)),
            Err(ConnectionError::WriterBroken)
        ));
        assert!(matches!(
            core.begin_write(WriteRequest::Business(vec![])),
            Err(ConnectionError::WriterBroken)
        ));
        assert!(matches!(
            core.begin_write(WriteRequest::Releases),
            Err(ConnectionError::WriterBroken)
        ));
        assert!(matches!(
            core.prepare_control_frame(),
            Err(ConnectionError::WriterBroken)
        ));
        assert_eq!(budget.snapshot().file.used_bytes, 512);
        if peer_end {
            core.peer_end().release();
        } else {
            receive(&core, &[id], false).reclaims.release();
        }
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert!(core.try_close());
    }
}

#[test]
fn failed_control_ends_its_reservation_even_if_the_batch_is_retained() {
    let core = established(ConnectionSide::Server);
    let control = core.prepare_control_frame().unwrap();
    let mut write = core
        .begin_write(WriteRequest::Control(control))
        .unwrap()
        .unwrap();
    write.begin_io().unwrap();
    write.write_failed();
    assert!(core.try_close());
    drop(write);
}
