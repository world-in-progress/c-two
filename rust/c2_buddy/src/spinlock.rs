//! Cross-process atomic spinlock for SHM buddy allocator.
//!
//! Uses a single AtomicU32 in the SHM header for mutual exclusion.
//! The critical section (bitmap operations) is extremely short (~50ns),
//! making spin-wait more efficient than OS mutex context switches.

use std::sync::atomic::{AtomicU32, Ordering};

/// Spinlock stored in shared memory.
pub struct ShmSpinlock {
    lock: *const AtomicU32,
}

unsafe impl Send for ShmSpinlock {}
unsafe impl Sync for ShmSpinlock {}

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const MAX_SPINS: u32 = 1000;

impl ShmSpinlock {
    /// Create a spinlock pointing at SHM memory.
    ///
    /// # Safety
    /// `ptr` must point to a properly aligned u32 in SHM that will outlive this struct.
    pub unsafe fn new(ptr: *mut u8) -> Self {
        Self {
            lock: ptr as *const AtomicU32,
        }
    }

    /// Initialize the spinlock (must be called once during segment creation).
    pub fn init(&self) {
        self.atomic().store(UNLOCKED, Ordering::Release);
    }

    /// Acquire the spinlock. Spins with progressive backoff.
    #[inline]
    pub fn lock(&self) {
        if !self.try_lock_spins(10_000_000) {
            panic!("ShmSpinlock: deadlock detected (>10M spins)");
        }
    }

    /// Try to acquire the lock within a spin budget. Returns true on success.
    #[inline]
    pub fn try_lock_spins(&self, max_total_spins: u32) -> bool {
        let mut total = 0u32;
        let mut phase = 0u32;
        loop {
            if self
                .atomic()
                .compare_exchange_weak(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            total += 1;
            if total >= max_total_spins {
                return false;
            }
            phase += 1;
            if phase < 16 {
                std::hint::spin_loop();
            } else if phase < MAX_SPINS {
                std::thread::yield_now();
            } else {
                phase = 0;
                std::thread::yield_now();
            }
        }
    }

    /// Release the spinlock.
    #[inline]
    pub fn unlock(&self) {
        self.atomic().store(UNLOCKED, Ordering::Release);
    }

    /// Execute a closure while holding the lock.
    #[inline]
    pub fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.lock();
        struct Guard<'a>(&'a ShmSpinlock);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.unlock();
            }
        }
        let _guard = Guard(self);
        f()
    }

    fn atomic(&self) -> &AtomicU32 {
        unsafe { &*self.lock }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_lock_unlock() {
        let mut val = 0u32;
        let spinlock = unsafe { ShmSpinlock::new(&mut val as *mut u32 as *mut u8) };
        spinlock.init();
        spinlock.lock();
        spinlock.unlock();
    }

    #[test]
    fn test_with_lock() {
        let mut val = 0u32;
        let spinlock = unsafe { ShmSpinlock::new(&mut val as *mut u32 as *mut u8) };
        spinlock.init();
        let result = spinlock.with_lock(|| 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn test_concurrent_lock() {
        use std::sync::atomic::{AtomicU32, Ordering as AtOrd};
        // Use atomics directly for the shared state.
        // Layout: [lock_word: u32, counter: u32]
        let shared = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let s = shared.clone();
                std::thread::spawn(move || {
                    let lock_ptr = s[0].as_ptr() as *mut u8;
                    let spinlock = unsafe { ShmSpinlock::new(lock_ptr) };
                    for _ in 0..1000 {
                        spinlock.lock();
                        s[1].fetch_add(1, AtOrd::Relaxed);
                        spinlock.unlock();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(shared[1].load(AtOrd::SeqCst), 4000);
    }

    #[test]
    fn test_spinlock_panic_safety() {
        #[repr(C, align(4))]
        struct Aligned([u8; 4]);
        let mut buf = Aligned([0u8; 4]);
        let spinlock = unsafe { ShmSpinlock::new(buf.0.as_mut_ptr()) };
        spinlock.init();

        // Panic inside with_lock — RAII guard must still unlock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spinlock.with_lock(|| {
                panic!("intentional panic inside with_lock");
            });
        }));
        assert!(result.is_err(), "closure should have panicked");

        // Lock must be reacquirable after panic-induced unlock.
        assert!(
            spinlock.try_lock_spins(1000),
            "lock should be acquirable after panic"
        );
        spinlock.unlock();
    }
}
