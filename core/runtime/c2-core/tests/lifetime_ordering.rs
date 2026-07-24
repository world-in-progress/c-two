use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use c2_core::{HeldResponse, LifecycleError};
use c2_ipc::{ResponseData, ResponseLease, ServerPoolState};
use c2_mem::{MemPool, PoolConfig};
use c2_server::{RequestData, RequestLease};
use parking_lot::{Mutex, RwLock};

#[derive(Clone)]
struct CheckedView {
    owner: Arc<FakeOwner>,
}

struct FakeOwner {
    valid: AtomicBool,
    bytes: Vec<u8>,
}

impl CheckedView {
    fn checked_bytes(&self) -> Result<&[u8], &'static str> {
        if self.owner.valid.load(Ordering::Acquire) {
            Ok(&self.owner.bytes)
        } else {
            Err("owner invalidated")
        }
    }

    fn materialize(&self) -> Vec<u8> {
        self.checked_bytes().expect("owner must be valid").to_vec()
    }
}

struct InvalidationGuard {
    owner: Arc<FakeOwner>,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl Drop for InvalidationGuard {
    fn drop(&mut self) {
        self.owner.valid.store(false, Ordering::Release);
        self.events.lock().push("fastdb-invalidate");
    }
}

#[derive(Clone, Copy)]
enum AdapterExit {
    Normal,
    Error,
    EarlyReturn,
    Unwind,
}

fn run_adapter_scope(
    exit: AdapterExit,
    owner: Arc<FakeOwner>,
    events: Arc<Mutex<Vec<&'static str>>>,
) -> Result<(), &'static str> {
    let _guard = InvalidationGuard { owner, events };
    if matches!(exit, AdapterExit::EarlyReturn) {
        return Err("early return");
    }
    match exit {
        AdapterExit::Normal => Ok(()),
        AdapterExit::Error => Err("adapter error"),
        AdapterExit::EarlyReturn => unreachable!("early return handled above"),
        AdapterExit::Unwind => panic!("adapter unwind"),
    }
}

#[test]
fn borrowed_input_invalidates_before_request_release_on_every_exit() {
    for exit in [
        AdapterExit::Normal,
        AdapterExit::Error,
        AdapterExit::EarlyReturn,
        AdapterExit::Unwind,
    ] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new(FakeOwner {
            valid: AtomicBool::new(true),
            bytes: b"borrowed".to_vec(),
        });
        let view = CheckedView {
            owner: owner.clone(),
        };
        let shared_clone = view.clone();
        let detached = view.materialize();
        let mut request = RequestLease::new(RequestData::Inline(b"transport".to_vec()));

        events.lock().push("callback-enter");
        let _ = catch_unwind(AssertUnwindSafe({
            let owner = owner.clone();
            let events = events.clone();
            move || run_adapter_scope(exit, owner, events)
        }));
        events.lock().push("callback-exit");
        request.release().expect("request release");
        events.lock().push("request-lease-release");

        assert_eq!(
            events.lock().as_slice(),
            [
                "callback-enter",
                "fastdb-invalidate",
                "callback-exit",
                "request-lease-release",
            ]
        );
        assert_eq!(view.checked_bytes(), Err("owner invalidated"));
        assert_eq!(shared_clone.checked_bytes(), Err("owner invalidated"));
        assert_eq!(detached, b"borrowed");
    }
}

#[test]
fn owned_response_copies_releases_then_opens_the_detached_copy() {
    let events = Mutex::new(Vec::new());
    let mut lease = inline_response_lease(b"owned");

    let copied = lease.copy_bytes().expect("response copy");
    events.lock().push("response-copy");
    lease.release().expect("response release");
    events.lock().push("response-lease-release");
    let opened = copied.clone();
    events.lock().push("adapter-open-copy");
    events.lock().push("return");

    assert_eq!(opened, b"owned");
    assert_eq!(
        events.lock().as_slice(),
        [
            "response-copy",
            "response-lease-release",
            "adapter-open-copy",
            "return",
        ]
    );
}

#[test]
fn held_response_explicit_release_invalidates_then_releases_exactly_once() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let invalidations = Arc::new(AtomicUsize::new(0));
    let mut held =
        HeldResponse::from_response_lease(inline_response_lease(b"held")).expect("held response");

    let events_for_invalidate = events.clone();
    let invalidations_for_release = invalidations.clone();
    held.invalidate_then_release(move || {
        invalidations_for_release.fetch_add(1, Ordering::AcqRel);
        events_for_invalidate.lock().push("fastdb-invalidate");
        Ok(())
    })
    .expect("held release");
    events.lock().push("response-lease-release");

    held.invalidate_then_release(|| {
        invalidations.fetch_add(1, Ordering::AcqRel);
        Err("must not run twice".to_string())
    })
    .expect("second release is idempotent");

    assert_eq!(invalidations.load(Ordering::Acquire), 1);
    assert!(held.is_released());
    assert_eq!(
        events.lock().as_slice(),
        ["fastdb-invalidate", "response-lease-release"]
    );
}

#[test]
fn sdk_owned_drop_guard_invalidates_before_core_transport_cleanup_once() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let invalidations = Arc::new(AtomicUsize::new(0));
    {
        let _held = SdkHeldDropGuard {
            held: Some(
                HeldResponse::from_response_lease(inline_response_lease(b"drop"))
                    .expect("held response"),
            ),
            events: events.clone(),
            invalidations: invalidations.clone(),
        };
    }

    assert_eq!(invalidations.load(Ordering::Acquire), 1);
    assert_eq!(
        events.lock().as_slice(),
        ["fastdb-invalidate", "response-lease-release"]
    );
}

#[test]
fn held_release_attempts_invalidation_and_transport_cleanup_and_retains_both_failures() {
    let (mut release_failure, release_pool) = breakable_handle_response(b"release failure");
    *release_pool.write() = replacement_pool("release_failure");
    let release_error = release_failure
        .invalidate_then_release(|| Ok(()))
        .expect_err("transport cleanup must fail");
    assert_held_release_error(&release_error, None, Some("response handle release failed"));

    let mut invalidation_failure =
        HeldResponse::from_response_lease(inline_response_lease(b"invalidation failure"))
            .expect("held response");
    let invalidation_error = invalidation_failure
        .invalidate_then_release(|| Err("invalidation failed".to_string()))
        .expect_err("invalidation must fail");
    assert_held_release_error(&invalidation_error, Some("invalidation failed"), None);

    let (mut combined_failure, combined_pool) = breakable_handle_response(b"combined failure");
    *combined_pool.write() = replacement_pool("combined_failure");
    let combined_error = combined_failure
        .invalidate_then_release(|| Err("invalidation failed".to_string()))
        .expect_err("both cleanup actions must fail");
    assert_held_release_error(
        &combined_error,
        Some("invalidation failed"),
        Some("response handle release failed"),
    );

    assert!(release_failure.is_released());
    assert!(invalidation_failure.is_released());
    assert!(combined_failure.is_released());
}

#[test]
fn held_response_copy_failure_attempts_cleanup_and_retains_both_causes() {
    let pool = Arc::new(RwLock::new(replacement_pool("copy_source")));
    let handle = {
        let mut pool = pool.write();
        let mut handle = pool.alloc_handle(16).expect("handle allocation");
        pool.handle_slice_mut(&mut handle)
            .copy_from_slice(b"copy and release");
        handle
    };
    let lease = ResponseLease::new(
        ResponseData::Handle(handle),
        Arc::new(Mutex::new(None::<ServerPoolState>)),
        Arc::clone(&pool),
    );
    *pool.write() = replacement_pool("copy_failure");

    let error = match HeldResponse::from_response_lease(lease) {
        Ok(_) => panic!("copy and cleanup must fail"),
        Err(error) => error,
    };
    let LifecycleError::HeldResponseCopy {
        copy_error,
        transport_error,
    } = error
    else {
        panic!("expected HeldResponseCopy, got {error:?}");
    };
    assert!(copy_error.contains("response handle copy failed"));
    assert!(
        transport_error
            .as_deref()
            .is_some_and(|error| error.contains("response handle release failed"))
    );
}

struct SdkHeldDropGuard {
    held: Option<HeldResponse>,
    events: Arc<Mutex<Vec<&'static str>>>,
    invalidations: Arc<AtomicUsize>,
}

impl Drop for SdkHeldDropGuard {
    fn drop(&mut self) {
        let Some(mut held) = self.held.take() else {
            return;
        };
        let events = self.events.clone();
        let invalidations = self.invalidations.clone();
        let _ = held.invalidate_then_release(move || {
            invalidations.fetch_add(1, Ordering::AcqRel);
            events.lock().push("fastdb-invalidate");
            Ok(())
        });
        self.events.lock().push("response-lease-release");
    }
}

fn inline_response_lease(bytes: &[u8]) -> ResponseLease {
    ResponseLease::new(
        ResponseData::Inline(bytes.to_vec()),
        Arc::new(Mutex::new(None::<ServerPoolState>)),
        Arc::new(RwLock::new(replacement_pool("inline"))),
    )
}

fn breakable_handle_response(bytes: &[u8]) -> (HeldResponse, Arc<RwLock<MemPool>>) {
    let pool = Arc::new(RwLock::new(replacement_pool("handle")));
    let handle = {
        let mut pool = pool.write();
        let mut handle = pool.alloc_handle(bytes.len()).expect("handle allocation");
        pool.handle_slice_mut(&mut handle).copy_from_slice(bytes);
        handle
    };
    let lease = ResponseLease::new(
        ResponseData::Handle(handle),
        Arc::new(Mutex::new(None::<ServerPoolState>)),
        pool.clone(),
    );
    (
        HeldResponse::from_response_lease(lease).expect("held handle response"),
        pool,
    )
}

fn replacement_pool(label: &str) -> MemPool {
    static NEXT_POOL: AtomicUsize = AtomicUsize::new(0);
    let sequence = NEXT_POOL.fetch_add(1, Ordering::Relaxed);
    MemPool::new_with_prefix(
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            ..PoolConfig::default()
        },
        format!(
            "/c2lt{:x}{:x}{}",
            std::process::id(),
            sequence,
            label.chars().next().unwrap_or('x')
        )
        .chars()
        .take(24)
        .collect(),
    )
}

fn assert_held_release_error(
    error: &LifecycleError,
    expected_invalidation: Option<&str>,
    expected_transport: Option<&str>,
) {
    let LifecycleError::HeldResponseRelease {
        invalidation_error,
        transport_error,
    } = error
    else {
        panic!("expected HeldResponseRelease, got {error:?}");
    };
    assert_eq!(invalidation_error.as_deref(), expected_invalidation);
    match expected_transport {
        Some(expected) => assert!(
            transport_error
                .as_deref()
                .is_some_and(|actual| actual.contains(expected)),
            "transport error {transport_error:?} did not contain {expected:?}"
        ),
        None => assert!(transport_error.is_none()),
    }
}
