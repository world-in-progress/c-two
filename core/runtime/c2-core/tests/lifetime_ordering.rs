use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use c2_core::{HeldResponse, LifecycleError};
use c2_ipc::ResponseData;
use c2_mem::{MemPool, PoolConfig};
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
fn borrowed_input_invalidates_before_local_buffer_drop_on_every_exit() {
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
        let request = b"local copy".to_vec();

        events.lock().push("callback-enter");
        let _ = catch_unwind(AssertUnwindSafe({
            let owner = owner.clone();
            let events = events.clone();
            move || run_adapter_scope(exit, owner, events)
        }));
        events.lock().push("callback-exit");
        drop(request);
        events.lock().push("local-buffer-drop");

        assert_eq!(
            events.lock().as_slice(),
            [
                "callback-enter",
                "fastdb-invalidate",
                "callback-exit",
                "local-buffer-drop",
            ]
        );
        assert_eq!(view.checked_bytes(), Err("owner invalidated"));
        assert_eq!(shared_clone.checked_bytes(), Err("owner invalidated"));
        assert_eq!(detached, b"borrowed");
    }
}

#[test]
fn owned_response_moves_inline_storage_before_adapter_open() {
    let bytes = b"owned".to_vec();
    let original = bytes.as_ptr();
    let owned = ResponseData::Inline(bytes).into_owned_bytes().unwrap();
    assert_eq!(owned, b"owned");
    assert_eq!(owned.as_ptr(), original);
}

#[test]
fn held_response_explicit_release_invalidates_then_drops_local_bytes_exactly_once() {
    let owner = Arc::new(FakeOwner {
        valid: AtomicBool::new(true),
        bytes: b"held".to_vec(),
    });
    let view = CheckedView {
        owner: owner.clone(),
    };
    let detached = view.materialize();
    let mut held = HeldResponse::from_owned_bytes(b"held".to_vec());
    let invalidations = AtomicUsize::new(0);
    held.invalidate_then_release(|| {
        invalidations.fetch_add(1, Ordering::AcqRel);
        owner.valid.store(false, Ordering::Release);
        Ok(())
    })
    .unwrap();
    assert!(held.bytes().is_empty());
    assert_eq!(view.checked_bytes(), Err("owner invalidated"));
    assert_eq!(detached, b"held");
    held.invalidate_then_release(|| {
        invalidations.fetch_add(1, Ordering::AcqRel);
        Err("must not run twice".into())
    })
    .unwrap();
    assert_eq!(invalidations.load(Ordering::Acquire), 1);
    assert!(held.is_released());
}

#[test]
fn held_invalidation_error_is_preserved_after_local_cleanup() {
    let mut held = HeldResponse::from_owned_bytes(b"invalidation failure".to_vec());
    let error = held
        .invalidate_then_release(|| Err("original invalidator error".into()))
        .unwrap_err();
    assert_eq!(
        error,
        LifecycleError::HeldResponseRelease {
            invalidation_error: "original invalidator error".into(),
        }
    );
    assert!(held.is_released());
    assert!(held.bytes().is_empty());
    held.invalidate_then_release(|| panic!("must not repeat invalidation"))
        .unwrap();
}

fn local_pool() -> Arc<RwLock<MemPool>> {
    Arc::new(RwLock::new(
        MemPool::new_with_prefix(
            PoolConfig {
                buddy_enabled: false,
                max_dedicated_segments: 0,
                ..PoolConfig::default()
            },
            format!("/c2-local-{}", uuid::Uuid::new_v4().simple()),
        )
        .unwrap(),
    ))
}

#[test]
fn consumed_reassembly_refunds_before_hold_and_does_not_retain_pool() {
    let pool = local_pool();
    let weak_pool = Arc::downgrade(&pool);
    let budget = pool.read().budget().unwrap().clone();
    let mut backing = c2_wire::chunk::ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
    let bytes = vec![0x5a; 512];
    backing.write_at(0, &bytes).unwrap();
    assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
    let mut held = HeldResponse::from_response_data(ResponseData::Handle(backing)).unwrap();
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    drop(pool);
    assert!(
        weak_pool.upgrade().is_none(),
        "local hold must not pin the original pool"
    );
    assert_eq!(held.bytes(), bytes);
    held.invalidate_then_release(|| Ok(())).unwrap();
    assert!(held.bytes().is_empty());
}

#[test]
fn consumption_failure_is_reported_before_a_hold_exists() {
    let pool = local_pool();
    let budget = pool.read().budget().unwrap().clone();
    let mut backing = c2_wire::chunk::ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
    backing.release();
    let error = HeldResponse::from_response_data(ResponseData::Handle(backing))
        .err()
        .expect("a consumed backing cannot create another hold");
    assert!(matches!(error, LifecycleError::ResponseCopy(message) if message.contains("released")));
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
}
