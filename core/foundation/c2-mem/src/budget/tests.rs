use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, mpsc};
use std::task::{Wake, Waker};
use std::thread;
use std::time::Duration;

fn domain(
    memory_budget_bytes: u64,
    file_spill_budget_bytes: u64,
    max_outstanding_calls: u64,
) -> DomainBudget {
    DomainBudget::from_limits(&DomainLimits {
        memory_budget_bytes,
        file_spill_budget_bytes,
        max_outstanding_calls,
    })
}

struct CountWake(AtomicUsize);
impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn buffer_wait_and_last_owner_refund() {
    let budget = domain(8, 0, 0);
    let first = Arc::new(BudgetedBuffer::new(
        vec![1; 8],
        budget.reserve_memory_blocking(8, None).unwrap(),
    ));
    let alias = Arc::clone(&first);
    let mut waiting = Box::pin(budget.reserve_memory(1));
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
    drop(first);
    assert_eq!(budget.snapshot().memory.used_bytes, 8);
    drop(alias);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    match waiting.as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(Ok(reservation)) => assert_eq!(reservation.bytes(), 1),
        other => panic!("release must wake and admit the waiter: {other:?}"),
    }
    drop(waiting);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn deadline_wait_is_not_a_rejection() {
    let budget = domain(4, 0, 0);
    let first = budget.reserve_memory_blocking(4, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    thread::scope(|scope| {
        let budget = budget.clone();
        let child_barrier = barrier.clone();
        let join = scope.spawn(move || {
            child_barrier.wait();
            budget.reserve_memory_blocking(1, Some(Instant::now() + Duration::from_millis(20)))
        });
        barrier.wait();
        assert_eq!(
            join.join().unwrap().unwrap_err(),
            PayloadBufferError::DeadlineExceeded
        );
    });
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
    drop(first);
    assert_eq!(budget.reserve_memory_blocking(4, None).unwrap().bytes(), 4);
}

#[test]
fn oversize_zero_and_u64_boundary() {
    let budget = domain(0, 0, 0);
    assert_eq!(budget.reserve_memory_blocking(0, None).unwrap().bytes(), 0);
    assert_eq!(
        budget.reserve_memory_blocking(1, None).unwrap_err(),
        PayloadBufferError::TooLarge {
            requested: 1,
            limit: 0
        }
    );
    assert_eq!(budget.snapshot().memory.rejected_allocations, 1);
    assert_eq!(budget.snapshot().memory.rejected_bytes, 1);
    let budget = domain(u64::MAX, 0, 0);
    let all = budget.reserve_memory_blocking(u64::MAX, None).unwrap();
    assert_eq!(budget.snapshot().memory.used_bytes, u64::MAX);
    drop(all);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    let mut all = budget.reserve_memory_blocking(u64::MAX, None).unwrap();
    assert_eq!(
        all.try_grow(1).unwrap_err(),
        PayloadBufferError::TooLarge {
            requested: u128::from(u64::MAX) + 1,
            limit: u64::MAX,
        }
    );
    assert_eq!(budget.snapshot().memory.used_bytes, u64::MAX);
}

#[test]
fn growth_counts_transient_copies_and_refunds() {
    let budget = domain(8, 0, 0);
    let mut first = budget.reserve_memory_blocking(2, None).unwrap();
    first.try_grow(2).unwrap();
    let second = budget.reserve_memory_blocking(4, None).unwrap();
    assert_eq!(budget.snapshot().memory.peak_bytes, 8);
    drop(first);
    assert_eq!(budget.snapshot().memory.used_bytes, 4);
    drop(second);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn split_charge_refunds_only_when_each_owner_drops() {
    let budget = domain(10, 0, 0);
    let mut input = budget.reserve_memory_blocking(10, None).unwrap();
    let upstream = input.split_off(3).unwrap();
    let downstream = input.split_off(4).unwrap();
    assert_eq!(
        (input.bytes(), upstream.bytes(), downstream.bytes()),
        (3, 3, 4)
    );
    assert_eq!(budget.snapshot().memory.used_bytes, 10);
    assert_eq!(budget.snapshot().memory.peak_bytes, 10);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
    let mut waiting = Box::pin(budget.reserve_memory(3));
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    drop(input);
    assert_eq!(budget.snapshot().memory.used_bytes, 7);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    let admitted = match waiting.as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(Ok(reservation)) => reservation,
        other => panic!("split owner release must wake the waiter: {other:?}"),
    };
    assert_eq!(budget.snapshot().memory.used_bytes, 10);
    drop(admitted);
    drop(waiting);
    drop(upstream);
    assert_eq!(budget.snapshot().memory.used_bytes, 4);
    drop(downstream);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
}

#[test]
fn split_out_of_range_preserves_charge_and_zero_split_has_no_effect() {
    let budget = domain(u64::MAX, 0, 0);
    let mut whole = budget.reserve_memory_blocking(4, None).unwrap();
    assert_eq!(
        whole.split_off(5).unwrap_err(),
        MemoryReservationError {
            requested: 5,
            reserved: 4,
        }
    );
    let empty = whole.split_off(0).unwrap();
    assert_eq!(empty.bytes(), 0);
    assert_eq!(budget.snapshot().memory.used_bytes, 4);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
    drop(empty);
    drop(whole);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    let mut all = budget.reserve_memory_blocking(u64::MAX, None).unwrap();
    let split = all.split_off(u64::MAX).unwrap();
    assert_eq!(all.bytes(), 0);
    assert_eq!(budget.snapshot().memory.used_bytes, u64::MAX);
    drop(all);
    assert_eq!(budget.snapshot().memory.used_bytes, u64::MAX);
    drop(split);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn operation_slots_are_separate_and_closing_keeps_bytes_usable() {
    let budget = domain(8, 0, 1);
    let first = budget.reserve_operation().unwrap();
    assert_eq!(
        budget.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );
    let bytes = budget.reserve_memory_blocking(8, None).unwrap();
    budget.close_operations();
    assert_eq!(
        budget.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::Closed
    );
    drop(bytes);
    let later = budget.reserve_memory_blocking(8, None).unwrap();
    assert_eq!(budget.snapshot().operations.used_operations, 1);
    assert_eq!(budget.snapshot().memory.used_bytes, 8);
    drop((first, later));
    assert_eq!(budget.snapshot().operations.used_operations, 0);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn operation_slot_moves_across_threads() {
    let budget = domain(4, 0, 1);
    let permit = budget.reserve_operation().unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        drop(permit);
        send.send(()).unwrap();
    })
    .join()
    .unwrap();
    receive.recv().unwrap();
    assert_eq!(budget.snapshot().operations.used_operations, 0);
}

#[test]
fn zero_slots_reject_even_empty_calls_and_clones_share_accounting() {
    let closed = domain(8, 0, 0);
    assert_eq!(
        closed.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );
    let budget = domain(8, 0, 1);
    let clone = budget.clone();
    let slot = clone.reserve_operation().unwrap();
    assert_eq!(budget.snapshot().operations.used_operations, 1);
    let charge = budget.reserve_memory_blocking(8, None).unwrap();
    assert_eq!(clone.snapshot().memory.used_bytes, 8);
    drop(budget);
    drop(slot);
    assert_eq!(clone.snapshot().operations.used_operations, 0);
    drop(charge);
    assert_eq!(clone.snapshot().memory.used_bytes, 0);
}

#[test]
fn cancelled_waiter_leaves_capacity_available() {
    let budget = domain(2, 0, 0);
    let held = budget.reserve_memory_blocking(2, None).unwrap();
    let mut pending = Box::pin(budget.reserve_memory(1));
    let waker = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
    assert!(matches!(
        pending.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    drop(pending);
    drop(held);
    assert_eq!(budget.reserve_memory_blocking(2, None).unwrap().bytes(), 2);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
}

#[test]
fn growth_above_total_limit_rejects_without_corrupting_original_charge() {
    let budget = domain(4, 0, 0);
    let mut held = budget.reserve_memory_blocking(3, None).unwrap();
    assert_eq!(
        held.try_grow(2).unwrap_err(),
        PayloadBufferError::TooLarge {
            requested: 5,
            limit: 4
        }
    );
    assert_eq!(held.bytes(), 3);
    assert_eq!(budget.snapshot().memory.used_bytes, 3);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 1);
    drop(held);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn call_rejections_exclude_payload_buffer_rejections() {
    let budget = domain(4, 0, 1);
    let slot = budget.reserve_operation().unwrap();
    let payload = &budget;
    assert!(matches!(
        payload.try_reserve_memory(5),
        Err(PayloadBufferError::TooLarge { .. })
    ));
    assert_eq!(payload.snapshot().memory.rejected_allocations, 1);
    assert_eq!(budget.snapshot().operations.rejected_reservations, 0);
    assert_eq!(
        budget.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );
    budget.close_operations();
    assert_eq!(
        budget.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::Closed
    );
    assert_eq!(budget.snapshot().operations.rejected_reservations, 2);
    drop(slot);
}

#[test]
fn two_partial_unknown_inputs_reject_growth_without_waiting_for_each_other() {
    let budget = domain(8, 0, 0);
    let mut first = budget.reserve_memory_blocking(4, None).unwrap();
    let mut second = budget.reserve_memory_blocking(4, None).unwrap();
    assert_eq!(
        first.try_grow(1),
        Err(PayloadBufferError::InsufficientCapacity {
            requested: 5,
            available: 0,
        })
    );
    assert_eq!(
        second.try_grow(1),
        Err(PayloadBufferError::InsufficientCapacity {
            requested: 5,
            available: 0,
        })
    );
    let snapshot = budget.snapshot();
    assert_eq!(snapshot.memory.used_bytes, 8);
    assert_eq!(snapshot.memory.rejected_allocations, 2);
    assert_eq!(snapshot.memory.rejected_bytes, 10);
    drop(first);
    second.try_grow(1).unwrap();
    assert_eq!(budget.snapshot().memory.used_bytes, 5);
    drop(second);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn immediate_first_reservation_rejects_temporary_shortage_and_zero_still_succeeds() {
    let budget = domain(4, 0, 0);
    let held = budget.reserve_memory_blocking(4, None).unwrap();
    assert_eq!(budget.try_reserve_memory(0).unwrap().bytes(), 0);
    assert_eq!(
        budget.try_reserve_memory(1).unwrap_err(),
        PayloadBufferError::InsufficientCapacity {
            requested: 1,
            available: 0,
        }
    );
    assert_eq!(budget.snapshot().memory.rejected_bytes, 1);
    drop(held);
    assert_eq!(budget.try_reserve_memory(4).unwrap().bytes(), 4);
}

#[test]
fn retired_budget_observer_is_weak_and_tracks_real_owner() {
    let budget = domain(4, 0, 0);
    let observer = budget.observe().downgrade();
    let reservation = budget.reserve_memory_blocking(4, None).unwrap();
    drop(budget);
    assert_eq!(observer.snapshot().unwrap().memory.used_bytes, 4);
    drop(reservation);
    assert!(observer.snapshot().is_none());
}

#[test]
fn domain_limits_select_slots_and_storage_without_changing_the_snapshot() {
    let limits = c2_config::DomainLimits {
        max_outstanding_calls: 3,
        memory_budget_bytes: 7,
        file_spill_budget_bytes: 11,
    };
    let budget = DomainBudget::from_limits(&limits);
    let before = budget.snapshot();
    assert_eq!(before.operations.max_operations, 3);
    assert_eq!(before.file_spill.limit_bytes, 11);
    assert_eq!(budget.snapshot().memory.limit_bytes, 7);
    assert_eq!(budget.snapshot(), before);
    let reservation = budget.reserve_memory_blocking(7, None).unwrap();
    assert_eq!(budget.snapshot().memory.used_bytes, 7);
    drop(reservation);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn operation_permit_survives_creating_handle_and_unwind_refunds_storage() {
    let budget = domain(4, 0, 1);
    let observer = budget.observe().downgrade();
    let slot = budget.reserve_operation().unwrap();
    drop(budget);
    assert!(observer.is_alive());
    drop(slot);
    assert!(!observer.is_alive());

    let budget = domain(4, 0, 0);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _owned =
            BudgetedBuffer::new(vec![1; 4], budget.reserve_memory_blocking(4, None).unwrap());
        panic!("drop storage and its charge on unwind");
    }));
    assert!(panic.is_err());
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn concurrent_admission_limits_both_byte_cells_and_operations() {
    let budget = domain(8, 6, 1);
    let start = Barrier::new(3);
    let attempted = Barrier::new(3);
    let observed = Barrier::new(3);
    thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                start.wait();
                let owners = (
                    budget.try_reserve_memory(8),
                    budget.try_reserve_file_spill(6),
                    budget.reserve_operation(),
                );
                attempted.wait();
                observed.wait();
                drop(owners);
            });
        }
        start.wait();
        attempted.wait();
        let snapshot = budget.snapshot();
        observed.wait();
        assert_eq!(snapshot.memory.used_bytes, 8);
        assert_eq!(snapshot.file_spill.used_bytes, 6);
        assert_eq!(snapshot.operations.used_operations, 1);
        assert_eq!(snapshot.memory.rejected_allocations, 1);
        assert_eq!(snapshot.file_spill.rejected_allocations, 1);
        assert_eq!(snapshot.operations.rejected_reservations, 1);
    });
    let snapshot = budget.snapshot();
    assert_eq!(snapshot.memory.used_bytes, 0);
    assert_eq!(snapshot.file_spill.used_bytes, 0);
    assert_eq!(snapshot.operations.used_operations, 0);
}

#[test]
fn closing_operations_allows_existing_work_to_reserve_both_kinds_of_storage() {
    let budget = domain(4, u64::MAX, 1);
    let operation = budget.reserve_operation().unwrap();
    budget.close_operations();
    let snapshot = budget.snapshot();
    assert!(snapshot.closed);
    assert!(snapshot.operations.closed);
    assert_eq!(snapshot.operations.used_operations, 1);
    assert_eq!(
        budget.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::Closed
    );
    let memory = budget.try_reserve_memory(4).unwrap();
    let file = budget.try_reserve_file_spill(u64::MAX).unwrap();
    assert_eq!(
        budget.try_reserve_file_spill(1).unwrap_err(),
        BudgetError {
            requested: 1,
            used: u64::MAX,
            limit: u64::MAX,
        }
    );

    let observer = budget.observe().downgrade();
    let other = domain(4, 4, 1);
    assert!(other.domain_id() > budget.domain_id());
    drop((budget, operation, memory));
    assert!(observer.is_alive());
    assert_eq!(
        observer.snapshot().unwrap().file_spill.used_bytes,
        file.bytes()
    );
    drop(file);
    assert!(!observer.is_alive());
}

#[test]
fn shrinking_a_charge_wakes_waiters_without_waiting_for_the_owner_to_drop() {
    let budget = domain(8, 0, 1);
    let mut held = budget.try_reserve_memory(8).unwrap();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut waiting = Box::pin(budget.reserve_memory(5));
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    assert!(held.shrink_to(9).is_err());
    assert_eq!(budget.snapshot().memory.used_bytes, 8);
    assert_eq!(wake.0.load(Ordering::SeqCst), 0);
    held.shrink_to(3).unwrap();
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert!(matches!(waiting.as_mut().poll(&mut cx), Poll::Ready(Ok(owner)) if owner.bytes() == 5));
    assert_eq!(held.bytes(), 3);
    assert_eq!(budget.snapshot().memory.used_bytes, 3);
}

#[test]
fn reusable_window_keeps_its_domain_charge_through_multiple_chunks() {
    let budget = domain(8, 0, 1);
    let observer = budget.observe().downgrade();
    let window = budget.try_reserve_memory(8).unwrap().into_reusable_window();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut public = Box::pin(budget.reserve_memory(1));
    assert!(public.as_mut().poll(&mut cx).is_pending());
    let mut first = Box::pin(window.reserve(8));
    let Poll::Ready(Ok(first_slice)) = first.as_mut().poll(&mut cx) else {
        panic!("the newly reserved window must fit the first full chunk");
    };
    drop(first);
    let mut chunk = first_slice.into_buffer(vec![0; 8]);
    let mut cancelled = Box::pin(window.reserve(1));
    assert!(cancelled.as_mut().poll(&mut cx).is_pending());
    drop(cancelled);

    for byte in 1..=3 {
        let mut next = Box::pin(window.reserve(8));
        assert!(next.as_mut().poll(&mut cx).is_pending());
        drop(chunk);
        assert_eq!(wake.0.load(Ordering::SeqCst), byte as usize);
        let Poll::Ready(Ok(slice)) = next.as_mut().poll(&mut cx) else {
            panic!("window must admit the next chunk after the previous one drops");
        };
        chunk = slice.into_buffer(vec![byte; 8]);
        assert_eq!(chunk.as_ref(), [byte; 8]);
        assert_eq!(budget.snapshot().memory.used_bytes, 8);
        assert!(public.as_mut().poll(&mut cx).is_pending());
    }
    drop(window);
    assert_eq!(budget.snapshot().memory.used_bytes, 8);
    assert!(public.as_mut().poll(&mut cx).is_pending());
    drop(chunk);
    assert_eq!(wake.0.load(Ordering::SeqCst), 4);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    assert!(public.as_mut().poll(&mut cx).is_ready());
    drop(public);
    let window = budget.try_reserve_memory(8).unwrap().into_reusable_window();
    drop(budget);
    assert!(observer.is_alive());
    drop(window);
    assert!(!observer.is_alive());
}

#[test]
fn subscription_slot_guard_preserves_control_headroom_without_blocking_n1() {
    let zero = domain(8, 0, 0);
    assert_eq!(
        zero.reserve_subscription_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );

    let one = domain(8, 0, 1);
    let sub1 = one.reserve_subscription_operation().unwrap();
    assert_eq!(one.snapshot().operations.used_operations, 1);
    assert_eq!(
        one.reserve_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );
    drop(sub1);
    assert_eq!(one.snapshot().operations.used_operations, 0);

    // N = 2: at most one long-lived subscription so one ordinary call can always enter.
    let two = domain(8, 0, 2);
    let s1 = two.reserve_subscription_operation().unwrap();
    assert_eq!(
        two.reserve_subscription_operation().unwrap_err().reason,
        RetentionRejectReason::LimitReached
    );
    let unary = two.reserve_operation().unwrap();
    assert_eq!(two.snapshot().operations.used_operations, 2);
    drop(unary);
    // Dropping s1 lets another subscription enter.
    drop(s1);
    let s2 = two.reserve_subscription_operation().unwrap();
    assert_eq!(two.snapshot().operations.used_operations, 1);
    drop(s2);
    assert_eq!(two.snapshot().operations.used_operations, 0);
}

#[test]
fn warm_pool_reuses_the_smallest_matching_capacity() {
    let size = WARM_BUFFER_MIN_BYTES;
    let budget = domain((size * 8) as u64, 0, 0);
    let larger = Vec::with_capacity(size + size / 4);
    let larger_ptr = larger.as_ptr();
    let exact = vec![7; size];
    let exact_ptr = exact.as_ptr();
    budget.recycle_vec(larger);
    budget.recycle_vec(exact);
    let exact = budget.allocate_vec(size).unwrap();
    assert!(exact.is_empty());
    assert_eq!(exact.as_ptr(), exact_ptr);
    let larger = budget.allocate_vec(size).unwrap();
    assert!(larger.is_empty());
    assert_eq!(larger.as_ptr(), larger_ptr);

    budget.recycle_vec(larger);
    let mut reservation = budget.try_reserve_memory(size as u64).unwrap();
    let larger = reservation.allocate_vec(size).unwrap();
    assert_eq!(larger.as_ptr(), larger_ptr);
    assert_eq!(reservation.bytes(), larger.capacity() as u64);
    drop(reservation.into_buffer(larger));
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}

#[test]
fn warm_pool_excludes_small_buffers_and_evicts_oldest_by_count() {
    let size = WARM_BUFFER_MIN_BYTES;
    let budget = domain((size * 8) as u64, 0, 0);
    budget.recycle_vec(Vec::with_capacity(size - 1));
    assert!(budget.shared.lock().warm_buffers.is_empty());
    let buffers: Vec<_> = (0..=WARM_BUFFER_MAX_COUNT)
        .map(|index| Vec::with_capacity(size + index))
        .collect();
    for bytes in buffers {
        budget.recycle_vec(bytes);
    }
    let state = budget.shared.lock();
    assert_eq!(
        state
            .warm_buffers
            .iter()
            .map(Vec::capacity)
            .collect::<Vec<_>>(),
        (size + 1..=size + WARM_BUFFER_MAX_COUNT).collect::<Vec<_>>()
    );
}

#[test]
fn warm_pool_respects_byte_limits_and_evicts_oldest_for_recycling() {
    let size = WARM_BUFFER_MIN_BYTES;
    let budget = domain((size * 3) as u64, 0, 0);
    let first = Vec::with_capacity(size);
    let second = Vec::with_capacity(size + 1);
    let third = Vec::with_capacity(size + 2);
    budget.recycle_vec(first);
    budget.recycle_vec(second);
    budget.recycle_vec(third);
    budget.recycle_vec(Vec::with_capacity(size * 3 + 1));
    let state = budget.shared.lock();
    assert_eq!(state.warm_idle_bytes, (size * 2 + 3) as u64);
    assert_eq!(state.warm_buffers.len(), 2);
    assert_eq!(state.warm_buffers.front().unwrap().capacity(), size + 1);
    assert_eq!(state.warm_capacity(), (size * 3) as u64);
    drop(state);

    let large_domain = domain(u64::MAX, 0, 0);
    assert_eq!(
        large_domain.shared.lock().warm_capacity(),
        2 * 1024 * 1024 * 1024
    );
}

#[test]
fn full_budget_buffer_recycles_atomically_then_yields_to_an_async_reservation() {
    let size = 2 * WARM_BUFFER_MIN_BYTES;
    let budget = domain(size as u64, 0, 0);
    let bytes = vec![3; size];
    let ptr = bytes.as_ptr();
    let buffer = budget
        .try_reserve_memory(size as u64)
        .unwrap()
        .into_buffer(bytes);
    let mut waiting = Box::pin(budget.reserve_memory(1));
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(waiting.as_mut().poll(&mut cx).is_pending());

    drop(buffer);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    let recycled = budget.allocate_vec(size).unwrap();
    assert_eq!(recycled.as_ptr(), ptr);
    assert!(recycled.is_empty());
    budget.recycle_vec(recycled);

    let Poll::Ready(Ok(reservation)) = waiting.as_mut().poll(&mut cx) else {
        panic!("idle buffers blocked reservation");
    };
    assert_eq!(budget.snapshot().memory.used_bytes, 1);
    assert!(budget.shared.lock().warm_buffers.is_empty());
    drop(reservation);
}

#[test]
fn each_admission_path_evicts_only_as_much_idle_capacity_as_needed() {
    let size = WARM_BUFFER_MIN_BYTES as u64;
    for admit in [
        |budget: &DomainBudget, size: u64| budget.try_reserve_memory(size + 2).unwrap(),
        |budget: &DomainBudget, size: u64| budget.reserve_memory_blocking(size + 2, None).unwrap(),
        |budget: &DomainBudget, size: u64| {
            let mut reservation = budget.try_reserve_memory(size).unwrap();
            reservation.try_grow(2).unwrap();
            reservation
        },
    ] {
        let budget = domain(size * 3 + 1, 0, 0);
        budget.recycle_vec(Vec::with_capacity(size as usize));
        budget.recycle_vec(Vec::with_capacity(size as usize + 1));
        let reservation = admit(&budget, size);
        let state = budget.shared.lock();
        assert_eq!(state.warm_idle_bytes, size + 1);
        assert_eq!(state.warm_buffers.len(), 1);
        assert!(state.memory.used_bytes + state.warm_idle_bytes <= state.memory.limit_bytes);
        drop(state);
        drop(reservation);
    }
}

#[test]
fn exhausted_budget_falls_back_to_exact_capacity() {
    let size = WARM_BUFFER_MIN_BYTES;
    let budget = domain(3 * size as u64, 0, 0);
    let mut reservation = budget.try_reserve_memory(size as u64).unwrap();
    budget.recycle_vec(Vec::with_capacity(size + size / 4));
    // Exhaust remaining headroom so the warm buffer is evicted before allocation;
    // fallback must allocate exact capacity without recording a rejection.
    let other = budget.try_reserve_memory(2 * size as u64).unwrap();
    let bytes = reservation.allocate_vec(size).unwrap();
    assert!(bytes.is_empty());
    assert_eq!(bytes.capacity(), size);
    assert_eq!(bytes.capacity() as u64, reservation.bytes());
    let snap = budget.snapshot().memory;
    assert_eq!(snap.rejected_allocations, 0);
    assert_eq!(snap.rejected_bytes, 0);
    drop(reservation.into_buffer(bytes));
    drop(other);
}

#[test]
fn buffer_into_parts_transfers_storage_and_reservation_without_recycling() {
    let size = WARM_BUFFER_MIN_BYTES;
    let budget = domain(size as u64, 0, 0);
    let bytes = vec![5; size];
    let ptr = bytes.as_ptr();
    let buffer = budget
        .try_reserve_memory(size as u64)
        .unwrap()
        .into_buffer(bytes);
    let (bytes, reservation) = buffer.into_parts();
    assert_eq!(bytes.as_ptr(), ptr);
    assert_eq!(bytes[0], 5);
    assert_eq!(budget.snapshot().memory.used_bytes, size as u64);
    drop(reservation);
    assert!(budget.shared.lock().warm_buffers.is_empty());
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
}
