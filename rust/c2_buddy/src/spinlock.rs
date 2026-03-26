//! Cross-process atomic spinlock for SHM buddy allocator.
//!
//! Uses a single AtomicU32 in the SHM header for mutual exclusion.
//! Stores the holder PID so other processes can detect dead holders
//! and recover from crashes (SIGKILL, etc.) without permanent deadlock.
//!
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
const MAX_SPINS: u32 = 1000;
/// After this many spins, check if the holder process is still alive.
const RECOVERY_CHECK_SPINS: u32 = 5_000_000;

/// Check if a process is alive using kill(pid, 0).
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) returns 0 if the process exists and we can signal it,
    // or -1 with EPERM if it exists but we lack permission (still alive).
    // Returns -1 with ESRCH if the process does not exist.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    true // Conservative: assume alive on non-Unix
}

fn current_pid() -> u32 {
    std::process::id()
}

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
    /// Attempts crash recovery if the holder process is dead.
    #[inline]
    pub fn lock(&self) {
        let pid = current_pid();
        if !self.try_lock_with_recovery(pid, 10_000_000) {
            panic!("ShmSpinlock: deadlock detected (>10M spins, holder still alive)");
        }
    }

    /// Try to acquire the lock within a spin budget.
    /// After RECOVERY_CHECK_SPINS, checks if holder is dead and recovers.
    #[inline]
    pub fn try_lock_with_recovery(&self, my_pid: u32, max_total_spins: u32) -> bool {
        let mut total = 0u32;
        let mut phase = 0u32;
        let mut recovery_checked = false;

        loop {
            let current = self.atomic().load(Ordering::Relaxed);
            if current == UNLOCKED {
                if self
                    .atomic()
                    .compare_exchange_weak(UNLOCKED, my_pid, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
            }

            total += 1;
            if total >= max_total_spins {
                return false;
            }

            // After many spins, check if holder is dead and attempt recovery.
            if !recovery_checked && total >= RECOVERY_CHECK_SPINS && current != UNLOCKED {
                recovery_checked = true;
                if !is_process_alive(current) {
                    // Holder is dead — try to take over. CAS ensures only one
                    // process succeeds if multiple detect the dead holder.
                    if self
                        .atomic()
                        .compare_exchange(current, my_pid, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        return true;
                    }
                    // Another process recovered first — continue spinning.
                }
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

    /// Try to acquire the lock within a spin budget. Returns true on success.
    #[inline]
    pub fn try_lock_spins(&self, max_total_spins: u32) -> bool {
        self.try_lock_with_recovery(current_pid(), max_total_spins)
    }

    /// Release the spinlock.
    #[inline]
    pub fn unlock(&self) {
        self.atomic().store(UNLOCKED, Ordering::Release);
    }

    /// Execute a closure while holding the lock.
    ///
    /// Uses an RAII guard so the lock is released even if `f` panics,
    /// preventing permanent deadlock across all processes sharing the SHM.
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

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spinlock.with_lock(|| {
                panic!("intentional panic inside with_lock");
            });
        }));
        assert!(result.is_err(), "closure should have panicked");

        assert!(
            spinlock.try_lock_spins(1000),
            "lock should be acquirable after panic"
        );
        spinlock.unlock();
    }

    #[test]
    fn test_pid_based_lock_stores_pid() {
        let mut val = 0u32;
        let spinlock = unsafe { ShmSpinlock::new(&mut val as *mut u32 as *mut u8) };
        spinlock.init();

        spinlock.lock();
        // After lock, the atomic should contain our PID (non-zero).
        let stored = spinlock.atomic().load(Ordering::Relaxed);
        assert_eq!(stored, current_pid());
        spinlock.unlock();

        // After unlock, should be UNLOCKED (0).
        let stored = spinlock.atomic().load(Ordering::Relaxed);
        assert_eq!(stored, UNLOCKED);
    }

    #[test]
    fn test_dead_pid_recovery() {
        use std::sync::atomic::AtomicU32;
        let storage = AtomicU32::new(0);
        let spinlock = unsafe { ShmSpinlock::new(storage.as_ptr() as *mut u8) };
        spinlock.init();

        // Simulate a dead holder by storing a PID that doesn't exist.
        // PID 2^30 is extremely unlikely to exist.
        let dead_pid: u32 = 1 << 30;
        storage.store(dead_pid, Ordering::Release);

        // try_lock_with_recovery should eventually detect the dead PID and recover.
        let acquired = spinlock.try_lock_with_recovery(current_pid(), 10_000_000);
        assert!(acquired, "should recover lock from dead PID");

        let stored = spinlock.atomic().load(Ordering::Relaxed);
        assert_eq!(stored, current_pid());
        spinlock.unlock();
    }
}
