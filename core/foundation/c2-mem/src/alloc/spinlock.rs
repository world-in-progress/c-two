//! Cross-process atomic spinlock for SHM buddy allocator.
//!
//! Uses a single AtomicU64 in the SHM header for mutual exclusion. The word
//! doubles as the crash-safety state machine:
//!
//! - `0` — unlocked.
//! - a bare holder PID in the lower 32 bits — locked. The complete `u32`
//!   process identifier is stored verbatim with no narrowing, so every value
//!   the OS can hand out (including high-bit Windows PIDs) round-trips
//!   exactly and cannot collide with the poison flag at bit 63.
//! - `POISON_FLAG` set — the holder panicked in its critical section.
//!   Every later acquire refuses and the segment must be recreated.
//!
//! The lock never steals a dead holder's word. A dead holder may have been
//! in the middle of mutating the buddy bitmaps, so resuming with a stolen
//! lock could corrupt the allocator. The observer refuses without changing
//! the word. This also prevents a delayed death probe from poisoning a new
//! holder after the old holder released cleanly and its PID was reused.
//!
//! Contention is bounded: every failed acquire iteration — CAS losses after
//! observing an unlocked word as well as holder-observed waits — counts
//! against the spin budget, and on exhaustion the caller gets
//! `SpinlockError::Contended` instead of hanging or panicking.
//!
//! Liveness probes are used conservatively. Unknown or permission-denied
//! status never proves death, and a live PID is not proof that the original
//! owner incarnation is alive (the OS may have reused the PID): liveness is
//! only ever used to classify a holder as "proven dead" versus "not proven
//! dead", never to take the lock over. When PID reuse makes a dead
//! predecessor look alive, acquirers keep refusing through their bounded
//! budgets instead of resuming a possibly half-updated allocator.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bit 63: the backing is poisoned and must refuse all further mutation.
pub(crate) const POISON_FLAG: u64 = 0x8000_0000_0000_0000;
/// Holder PID storage: the complete lower `u32`, stored verbatim.
pub(crate) const PID_MASK: u64 = 0x0000_0000_FFFF_FFFF;

const UNLOCKED: u64 = 0;
/// Every bit a well-formed lock word may carry: PID plus poison flags.
const WORD_MASK: u64 = PID_MASK | POISON_FLAG;
/// Backoff: spin with `spin_loop` hints for this many iterations, then yield.
const BACKOFF_SPIN_PHASE: u32 = 16;
/// Backoff: yield until the phase counter reaches this, then reset it.
const BACKOFF_YIELD_THRESHOLD: u32 = 1_000;
/// Default acquire spin budget (`try_lock` / `with_lock`).
const DEFAULT_MAX_SPINS: u32 = 10_000_000;
/// Default number of failed acquire iterations before the first
/// holder-liveness probe.
const DEFAULT_RECOVERY_CHECK_SPINS: u32 = 5_000_000;

/// Why a bounded lock acquisition failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinlockError {
    /// A holder panicked; every later allocator mutation must be
    /// refused until the segment is recreated.
    Poisoned {
        /// Complete PID preserved from the poisoned holder for diagnostics.
        holder: u32,
    },
    /// The observed holder PID was dead when probed. The shared word is not
    /// changed: a reused PID may now belong to a different lock holder.
    DeadHolder { holder: u32 },
    /// The spin budget ran out while the holder appeared alive or its status
    /// was unknown. Ordinary contention: no state was mutated and a later
    /// retry may succeed. A `holder` of 0 means no holder word was ever
    /// observed — every iteration lost the acquire CAS race to a peer.
    Contended { holder: u32 },
}

impl fmt::Display for SpinlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned { holder } => write!(
                f,
                "backing poisoned (critical section panicked; holder pid {holder}); further allocator mutation refused"
            ),
            Self::DeadHolder { holder } => write!(
                f,
                "dead holder observed at pid {holder}; acquisition refused without changing the shared lock"
            ),
            Self::Contended { holder } => write!(
                f,
                "lock contention exceeded the spin budget; holder pid {holder} appears alive or its status is unknown"
            ),
        }
    }
}

/// Check if a process is alive using kill(pid, 0).
#[cfg(unix)]
pub(crate) fn is_process_alive(pid: u32) -> bool {
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

#[cfg(windows)]
pub(crate) fn is_process_alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    if pid == 0 {
        return false;
    }
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        // Access denied and other ambiguous failures cannot prove death.
        return std::io::Error::last_os_error().raw_os_error()
            != Some(ERROR_INVALID_PARAMETER as i32);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) != WAIT_OBJECT_0 }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn is_process_alive(_pid: u32) -> bool {
    true // Conservative: assume alive on non-Unix
}

fn current_pid() -> u32 {
    std::process::id()
}

fn holder_of(word: u64) -> u32 {
    (word & PID_MASK) as u32
}

/// Spinlock stored in shared memory.
///
/// Only the `u64` word lives in SHM; the budget fields are per-handle policy
/// and are not shared. All supported backends (x64, arm64) implement the
/// lock-free AtomicU64 the shared allocator counters already rely on.
pub struct ShmSpinlock {
    lock: *const AtomicU64,
    /// Spin budget for `try_lock` / `with_lock`.
    max_spins: u32,
    /// Failed acquire iterations before the first holder-liveness probe.
    recovery_after: u32,
}

unsafe impl Send for ShmSpinlock {}
unsafe impl Sync for ShmSpinlock {}

impl ShmSpinlock {
    /// Create a spinlock pointing at SHM memory.
    ///
    /// # Safety
    /// `ptr` must point to a properly aligned (`u64`/8-byte) AtomicU64 word
    /// in SHM that will outlive this struct.
    pub unsafe fn new(ptr: *mut u8) -> Self {
        Self {
            lock: ptr as *const AtomicU64,
            max_spins: DEFAULT_MAX_SPINS,
            recovery_after: DEFAULT_RECOVERY_CHECK_SPINS,
        }
    }

    /// Initialize the spinlock (must be called once during segment creation).
    ///
    /// Never use this to "reset" a poisoned backing: poison is terminal and
    /// the segment must be recreated.
    pub fn init(&self) {
        self.atomic().store(UNLOCKED, Ordering::Release);
    }

    /// Acquire the lock within the configured spin budget.
    ///
    /// Fails closed instead of hanging: a poisoned backing refuses
    /// immediately, a live or unknown holder yields `SpinlockError::Contended`
    /// when the budget runs out, and an observed-dead holder refuses without
    /// changing the shared word.
    #[inline]
    pub fn try_lock(&self) -> Result<(), SpinlockError> {
        self.try_lock_budget(current_pid(), self.max_spins, self.recovery_after)
    }

    /// Bounded acquire engine.
    ///
    /// Progress accounting: every loop iteration that does not acquire — a
    /// lost acquire CAS after observing an unlocked word or a holder-observed
    /// wait — increments the saturating
    /// iteration counter and is subject to `max_total_spins`. No path can
    /// loop past the budget, so contention can never spin indefinitely.
    ///
    /// After `recovery_after` failed iterations the holder PID is probed once:
    /// a dead holder means the critical section may be half-applied, so the
    /// observer refuses. An OS probe cannot atomically identify an acquisition
    /// in a PID-only word; the observer must not rewrite it.
    #[inline]
    pub(crate) fn try_lock_budget(
        &self,
        my_pid: u32,
        max_total_spins: u32,
        recovery_after: u32,
    ) -> Result<(), SpinlockError> {
        self.try_lock_budget_with_probe(my_pid, max_total_spins, recovery_after, is_process_alive)
    }

    fn try_lock_budget_with_probe(
        &self,
        my_pid: u32,
        max_total_spins: u32,
        recovery_after: u32,
        mut is_alive: impl FnMut(u32) -> bool,
    ) -> Result<(), SpinlockError> {
        let mut total = 0u32;
        let mut phase = 0u32;
        let mut recovery_checked = false;
        let mut last_holder = 0u32;

        loop {
            let current = self.atomic().load(Ordering::Relaxed);
            if current == UNLOCKED {
                if self
                    .atomic()
                    .compare_exchange_weak(
                        UNLOCKED,
                        my_pid as u64,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return Ok(());
                }
                // CAS lost to a racing acquirer — counted against the budget
                // below like any other failed iteration.
            } else if current & POISON_FLAG != 0 {
                return Err(SpinlockError::Poisoned {
                    holder: holder_of(current),
                });
            } else {
                debug_assert_eq!(
                    current & !WORD_MASK,
                    0,
                    "lock word must contain only PID and poison bits"
                );
                last_holder = holder_of(current);

                // After the probe threshold, check whether the holder is
                // proven dead. A live or unknown PID (including a reused PID
                // belonging to an unrelated process) never authorizes a
                // takeover; the acquire simply stays bounded and refuses.
                if !recovery_checked && total >= recovery_after {
                    recovery_checked = true;
                    if !is_alive(last_holder) {
                        return Err(SpinlockError::DeadHolder {
                            holder: last_holder,
                        });
                    }
                }
            }

            // Bounded progress: saturating counter checked on every failed
            // iteration regardless of which branch produced the failure.
            total = total.saturating_add(1);
            if total >= max_total_spins {
                return Err(SpinlockError::Contended {
                    holder: last_holder,
                });
            }

            phase += 1;
            if phase < BACKOFF_SPIN_PHASE {
                std::hint::spin_loop();
            } else if phase < BACKOFF_YIELD_THRESHOLD {
                std::thread::yield_now();
            } else {
                phase = 0;
                std::thread::yield_now();
            }
        }
    }

    /// Release the lock after a clean critical section.
    ///
    /// Only the current holder may call this. A poisoned word is never
    /// cleared: poison is terminal and the segment must be recreated, so even
    /// a stray unlock cannot resurrect a backing that must stay unavailable.
    #[inline]
    pub fn unlock(&self) {
        let _ = self
            .atomic()
            .fetch_update(Ordering::Release, Ordering::Relaxed, |word| {
                if word & POISON_FLAG != 0 {
                    None // Keep the poison.
                } else {
                    Some(UNLOCKED)
                }
            });
    }

    /// Execute a closure while holding the lock.
    ///
    /// Returns `Err(SpinlockError)` when the backing is unavailable; `f` is
    /// then never invoked, so no allocator state is touched.
    ///
    /// If `f` panics, the word is poisoned before
    /// the panic resumes unwinding, so no process can resume a half-updated
    /// allocator. Under `panic = "abort"` the word keeps the dead holder's
    /// PID and peers reach the same refusal through the dead-holder probe.
    #[inline]
    pub fn with_lock<F, R>(&self, f: F) -> Result<R, SpinlockError>
    where
        F: FnOnce() -> R,
    {
        self.try_lock()?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match result {
            Ok(value) => {
                self.unlock();
                Ok(value)
            }
            Err(payload) => {
                self.poison_panicked(current_pid());
                std::panic::resume_unwind(payload);
            }
        }
    }

    /// Whether the backing is poisoned and refuses all further mutation.
    pub fn is_poisoned(&self) -> bool {
        self.atomic().load(Ordering::Relaxed) & POISON_FLAG != 0
    }

    /// Poison the word as a panicked critical section. The caller must hold
    /// the lock; used by `with_lock`'s unwind path and by tests.
    pub(crate) fn poison_panicked(&self, pid: u32) {
        self.atomic()
            .store(POISON_FLAG | pid as u64, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn force_budgets(&mut self, max_spins: u32, recovery_after: u32) {
        self.max_spins = max_spins;
        self.recovery_after = recovery_after;
    }

    #[cfg(test)]
    pub(crate) fn load_word_for_test(&self) -> u64 {
        self.atomic().load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn store_word_for_test(&self, word: u64) {
        self.atomic().store(word, Ordering::Relaxed);
    }

    fn atomic(&self) -> &AtomicU64 {
        unsafe { &*self.lock }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn make_lock() -> (ShmSpinlock, Box<AtomicU64>) {
        // Box so the word's address stays stable when the tuple is returned.
        let storage = Box::new(AtomicU64::new(0));
        let spinlock = unsafe { ShmSpinlock::new(storage.as_ptr() as *mut u8) };
        spinlock.init();
        (spinlock, storage)
    }

    #[test]
    fn test_lock_unlock() {
        let (spinlock, storage) = make_lock();

        spinlock.try_lock().unwrap();
        assert_eq!(storage.load(Ordering::Relaxed), current_pid() as u64);
        spinlock.unlock();
        assert_eq!(storage.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn test_with_lock_returns_closure_result() {
        let (spinlock, storage) = make_lock();

        let result = spinlock.with_lock(|| 42u32);
        assert_eq!(result.unwrap(), 42);
        assert_eq!(storage.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn test_concurrent_lock() {
        use std::sync::atomic::Ordering as AtOrd;
        let shared = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let s = shared.clone();
                std::thread::spawn(move || {
                    let lock_ptr = s[0].as_ptr() as *mut u8;
                    let spinlock = unsafe { ShmSpinlock::new(lock_ptr) };
                    for _ in 0..1000 {
                        spinlock
                            .try_lock()
                            .expect("bounded acquire under contention");
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
    fn test_panic_poisons_backing() {
        let (spinlock, storage) = make_lock();
        let my_pid = current_pid();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = spinlock.with_lock(|| -> u32 { panic!("intentional panic inside with_lock") });
        }));
        assert!(result.is_err(), "closure should have panicked");

        // The unwind path must poison instead of releasing.
        assert_eq!(storage.load(Ordering::Relaxed), POISON_FLAG | my_pid as u64);

        // Refusal is immediate, before any spin budget is consumed.
        let err = spinlock.try_lock().unwrap_err();
        assert_eq!(err, SpinlockError::Poisoned { holder: my_pid });

        // Unlock must not clear the poison.
        spinlock.unlock();
        assert!(spinlock.is_poisoned());
        assert_eq!(storage.load(Ordering::Relaxed), POISON_FLAG | my_pid as u64);
    }

    #[test]
    fn delayed_dead_pid_probe_cannot_poison_a_reused_live_holder() {
        let (spinlock, storage) = make_lock();
        let observed_pid = 42;
        storage.store(observed_pid, Ordering::Release);
        let err = spinlock
            .try_lock_budget_with_probe(current_pid(), 100, 0, |pid| {
                assert_eq!(u64::from(pid), observed_pid);
                // During the OS probe, the original holder releases cleanly
                // and exits. After the death observation, a new process reuses
                // its PID and acquires the lock before the observer resumes.
                storage.store(UNLOCKED, Ordering::Release);
                storage.store(observed_pid, Ordering::Release);
                false
            })
            .unwrap_err();
        assert_eq!(
            err,
            SpinlockError::DeadHolder {
                holder: observed_pid as u32
            }
        );
        assert_eq!(storage.load(Ordering::Acquire), observed_pid);
        assert!(!spinlock.is_poisoned());
        // The new holder can finish normally; a later acquisition still works.
        spinlock.unlock();
        spinlock.try_lock().unwrap();
        spinlock.unlock();
    }

    #[test]
    fn test_contention_is_bounded_and_never_poisons_live_holder() {
        let storage = Arc::new(AtomicU64::new(0));
        let spinlock = unsafe { ShmSpinlock::new(storage.as_ptr() as *mut u8) };
        spinlock.init();
        let holder_acquired = Arc::new(AtomicBool::new(false));
        let holder_release = Arc::new(AtomicBool::new(false));
        let my_pid = current_pid();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                spinlock
                    .try_lock()
                    .expect("holder acquires an uncontended lock");
                holder_acquired.store(true, Ordering::Release);
                while !holder_release.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                spinlock.unlock();
            });

            while !holder_acquired.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            // Same-process holder: the word holds our own PID, yet the
            // acquire must stay bounded and must never poison a live holder.
            let err = spinlock.try_lock_budget(my_pid, 2_000, 1_000).unwrap_err();
            assert_eq!(err, SpinlockError::Contended { holder: my_pid });
            assert_eq!(
                storage.load(Ordering::Relaxed),
                my_pid as u64,
                "holder word unchanged"
            );
            assert!(!spinlock.is_poisoned(), "a live holder is never poison");

            holder_release.store(true, Ordering::Release);
        });

        // After the holder releases, acquisition succeeds again.
        spinlock.try_lock().expect("acquire after holder release");
        spinlock.unlock();
        assert_eq!(storage.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn test_lock_word_preserves_boundary_pids() {
        // PIDs at every u32 boundary, including values that would collide
        // with the old 32-bit poison flags. The engine paths used here never
        // probe liveness (probe threshold beyond the budget, and immediate
        // poison refusal), so nothing relies on these being live or dead.
        for pid in [1u32, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
            let (spinlock, storage) = make_lock();

            // Bare holder word: exact PID preserved, never read as poison.
            spinlock.store_word_for_test(pid as u64);
            assert!(!spinlock.is_poisoned());
            let err = spinlock
                .try_lock_budget(current_pid(), 64, u32::MAX)
                .unwrap_err();
            assert_eq!(err, SpinlockError::Contended { holder: pid });
            assert_eq!(
                storage.load(Ordering::Relaxed),
                pid as u64,
                "word untouched"
            );

            // The same PID survives intact through a poisoned word.
            spinlock.poison_panicked(pid);
            assert!(spinlock.is_poisoned());
            let err = spinlock.try_lock().unwrap_err();
            assert_eq!(err, SpinlockError::Poisoned { holder: pid });
            assert_eq!(storage.load(Ordering::Relaxed), POISON_FLAG | pid as u64);
        }
    }

    #[test]
    fn test_cas_race_loss_stays_within_budget() {
        // Several threads hammer one word with a small budget. Losing the
        // acquire CAS after observing an unlocked word must consume budget
        // exactly like a holder-observed wait; the pre-fix accounting only
        // counted holder-observed iterations and could spin indefinitely.
        let storage = Arc::new(AtomicU64::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let my_pid = current_pid();

        for _ in 0..4 {
            let storage = Arc::clone(&storage);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let spinlock = unsafe { ShmSpinlock::new(storage.as_ptr() as *mut u8) };
                for _ in 0..500 {
                    match spinlock.try_lock_budget(my_pid, 2_048, u32::MAX) {
                        Ok(()) => spinlock.unlock(),
                        // Probe disabled via the threshold: refusal must be
                        // pure budget exhaustion, never a liveness decision.
                        Err(SpinlockError::Contended { .. }) => {}
                        Err(other) => {
                            panic!("unexpected lock error under fair contention: {other}")
                        }
                    }
                }
                done_tx.send(()).expect("test channel alive");
            });
        }
        drop(done_tx);

        // Every worker must finish well within the deadline; an unbounded
        // spin path would hang here instead.
        for _ in 0..4 {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("all contended workers finished within the deadline");
        }
        assert_eq!(storage.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn test_process_liveness_tracks_real_child_exit() {
        // Spawn a direct child with no intermediate shell, so killing the
        // child cannot orphan a grandchild that keeps the probe target alive.
        let mut command = if cfg!(windows) {
            let mut command = std::process::Command::new("ping");
            command.args(["-n", "30", "127.0.0.1"]);
            command
        } else {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            command
        };
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let alive = is_process_alive(pid);
        let _ = child.kill();
        child.wait().unwrap();
        assert!(alive);
        assert!(!is_process_alive(pid));
        assert!(is_process_alive(std::process::id()));
    }
}
