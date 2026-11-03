use std::fmt;

use c2_core::{Error, HeldResponse};

/// A retained SDK value paired with its local response copy.
///
/// The current portable receive path is copy-backed. Retention is a lifetime
/// contract, not a zero-copy claim.
#[must_use = "held values must remain owned or be explicitly released"]
pub struct Held<T> {
    // The response owns only local bytes; transport storage is already released.
    owners: Option<(HeldResponse, T)>,
    invalidate: fn(&T) -> Result<(), String>,
}

impl<T> Held<T> {
    pub(crate) fn new(
        value: T,
        response: HeldResponse,
        invalidate: fn(&T) -> Result<(), String>,
    ) -> Self {
        Self {
            owners: Some((response, value)),
            invalidate,
        }
    }

    /// Borrow the retained value while this owner remains live.
    pub fn value(&self) -> Option<&T> {
        self.owners
            .as_ref()
            .and_then(|(response, value)| (!response.is_released()).then_some(value))
    }

    pub fn is_released(&self) -> bool {
        self.owners
            .as_ref()
            .is_none_or(|(response, _)| response.is_released())
    }

    /// Invalidate the retained value owner before dropping the local bytes.
    ///
    /// Invalidation runs once, including when it fails or unwinds. The original
    /// invalidation error is preserved and subsequent releases are no-ops.
    pub fn release(&mut self) -> Result<(), Error> {
        let Some((mut response, value)) = self.owners.take() else {
            return Ok(());
        };
        response
            .invalidate_then_release(|| (self.invalidate)(&value))
            .map_err(Error::from)
    }
}

/// Report failed cleanup while the caller preserves its primary decode error.
pub fn release_held_after_error<T>(held: &mut Held<T>) {
    if let Err(cleanup_error) = held.release() {
        eprintln!("Warning: held payload decode cleanup failed: {cleanup_error}");
    }
}

impl<T> fmt::Debug for Held<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Held")
            .field("released", &self.is_released())
            .finish_non_exhaustive()
    }
}

impl<T> Drop for Held<T> {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("Warning: final Rust held value release failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_ipc::ResponseData;
    use c2_mem::{MemPool, PoolConfig};
    use c2_wire::chunk::backing::ReassemblyBacking;
    use parking_lot::RwLock;
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ReleaseBeforeDrop {
        pool: Arc<RwLock<MemPool>>,
        observed: Arc<AtomicBool>,
    }

    impl Drop for ReleaseBeforeDrop {
        fn drop(&mut self) {
            self.observed
                .store(self.pool.read().stats().alloc_count == 0, Ordering::SeqCst);
        }
    }

    #[test]
    fn invalidator_unwind_releases_response_before_value_drop() {
        let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default()).unwrap()));
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        backing.write_at(0, &[7; 512]).unwrap();
        let response = HeldResponse::from_response_data(ResponseData::Handle(backing)).unwrap();
        let observed = Arc::new(AtomicBool::new(false));
        let panic = catch_unwind(AssertUnwindSafe({
            let pool = Arc::clone(&pool);
            let observed = Arc::clone(&observed);
            move || {
                let mut held = Held::new(ReleaseBeforeDrop { pool, observed }, response, |_| {
                    panic!("invalidation panic")
                });
                let _ = held.release();
            }
        }));
        assert!(panic.is_err());
        assert!(observed.load(Ordering::SeqCst));
        drop(pool);
    }

    #[test]
    fn decode_cleanup_preserves_primary_error_and_invalidates_once() {
        const CHILD: &str = "C2_TEST_RUST_HELD_INVALIDATION_ERROR_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "held::tests::decode_cleanup_preserves_primary_error_and_invalidates_once",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "child failed: {stderr}");
            assert!(stderr.lines().any(|line| {
                line.starts_with("Warning: held payload decode cleanup failed:")
                    && line.contains("invalidation failed")
            }));
            assert_eq!(stderr.matches("Warning:").count(), 1);
            return;
        }

        let invalidations = Cell::new(0);
        let mut held = Held::new(
            &invalidations,
            HeldResponse::from_owned_bytes(vec![7]),
            |calls| {
                calls.set(calls.get() + 1);
                Err("invalidation failed".into())
            },
        );
        let primary = Err::<(), _>("original decode error").inspect_err(|_| {
            release_held_after_error(&mut held);
        });
        assert_eq!(primary, Err("original decode error"));
        assert!(held.value().is_none());
        held.release().unwrap();
        drop(held);
        assert_eq!(invalidations.get(), 1);
    }

    #[test]
    fn drop_during_unwind_preserves_primary_panic_and_invalidates_once() {
        let invalidations = Cell::new(0);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _held = Held::new(
                &invalidations,
                HeldResponse::from_owned_bytes(vec![7]),
                |calls| {
                    calls.set(calls.get() + 1);
                    Err("invalidation failed during unwind".into())
                },
            );
            panic!("original business panic");
        }));
        assert_eq!(
            panic.unwrap_err().downcast_ref::<&str>(),
            Some(&"original business panic")
        );
        assert_eq!(invalidations.get(), 1);
    }

    #[test]
    fn failed_invalidation_consumes_held_owners_once() {
        for panics in [false, true] {
            let invalidations = Cell::new(0);
            let mut held = Held::new(
                (&invalidations, panics),
                HeldResponse::from_owned_bytes(Vec::new()),
                |(invalidations, panics)| {
                    invalidations.set(invalidations.get() + 1);
                    assert!(!panics, "invalidation panic");
                    Err("invalidation failed".into())
                },
            );
            let result = catch_unwind(AssertUnwindSafe(|| held.release()));
            assert_eq!(result.is_err(), panics);
            match result {
                Ok(Err(Error::Lifecycle(c2_core::LifecycleError::HeldResponseRelease {
                    invalidation_error,
                }))) => assert_eq!(invalidation_error, "invalidation failed"),
                Err(panic) => assert_eq!(panic.downcast_ref::<&str>(), Some(&"invalidation panic")),
                other => panic!("unexpected invalidation result: {other:?}"),
            }
            assert!(held.is_released());
            assert!(held.value().is_none());
            held.release().expect("already released");
            drop(held);
            assert_eq!(invalidations.get(), 1);
        }
    }
}
