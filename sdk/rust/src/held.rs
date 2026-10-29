use std::fmt;

use c2_core::{Error, HeldResponse};

/// A retained SDK value paired with its still-live C-Two response lease.
///
/// The current portable receive path is copy-backed. Retention is a lifetime
/// contract, not a zero-copy claim.
#[must_use = "held values must remain owned or be explicitly released"]
pub struct Held<T> {
    // Response first preserves native cleanup before value drop on unwind.
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

    /// Invalidate the retained value owner before releasing transport storage.
    ///
    /// Invalidation runs once. A failed native release retains the response
    /// for another explicit cleanup attempt, while the value stays inaccessible.
    pub fn release(&mut self) -> Result<(), Error> {
        let Some((response, value)) = self.owners.as_mut() else {
            return Ok(());
        };
        let result = response.invalidate_then_release(|| (self.invalidate)(&value));
        if !response.has_pending_release() {
            let (response, value) = self.owners.take().expect("held owners still present");
            if std::thread::panicking() {
                drop(response);
                drop(value);
            } else {
                drop(value);
                drop(response);
            }
        }
        result.map_err(Error::from)
    }
}

/// Report failed cleanup while the caller preserves its primary decode error.
/// A pending response remains owned by `held` for its final Drop attempt.
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
        if let Some((response, value)) = self.owners.take() {
            // The final native attempt failed. End its obligation once; a
            // nested response Drop must not retry it.
            response.abandon();
            drop(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_ipc::{ResponseData, ResponseLease};
    use c2_mem::{MemPool, PoolConfig};
    use c2_wire::chunk::ReassemblyBacking;
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
        let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default())));
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        backing.write_at(0, &[7; 512]).unwrap();
        let response =
            HeldResponse::from_response_lease(ResponseLease::new(ResponseData::Handle(backing)))
                .unwrap();
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
        pool.write().destroy();
    }

    #[test]
    fn failed_native_release_retains_response_and_never_reinvalidates() {
        let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default())));
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        backing.write_at(0, &[7; 512]).unwrap();
        let response =
            HeldResponse::from_response_lease(ResponseLease::new(ResponseData::Handle(backing)))
                .unwrap();
        let calls = Cell::new(0);
        let mut held = Held::new(&calls, response, |calls| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        let original = std::mem::replace(&mut *pool.write(), MemPool::new(PoolConfig::default()));

        let first = held
            .release()
            .expect_err("foreign pool must reject release");
        assert!(
            first
                .to_string()
                .contains("pool owner incarnation mismatch")
        );
        assert!(held.is_released());
        assert!(held.value().is_none());
        assert_eq!(calls.get(), 1);
        assert_eq!(budget.snapshot().abandoned_releases, 0);

        *pool.write() = original;
        held.release().unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(budget.snapshot().abandoned_releases, 0);
        pool.write().destroy();
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
            if let Ok(result) = result {
                assert!(result.is_err());
            }
            assert!(held.is_released());
            assert!(held.value().is_none());
            held.release().expect("already released");
            drop(held);
            assert_eq!(invalidations.get(), 1);
        }
    }
}
