//! Canonical finite memory-budget limits for C-Two-owned backing.
//!
//! These limits are plain data resolved by configuration code. The
//! reservation primitive lives in `c2-mem` ([`c2_mem::MemoryBudget`]), which
//! depends on `c2-config`; keeping the shared budget object out of this crate
//! avoids a dependency cycle. Callers resolve limits here, build a
//! `c2_mem::MemoryBudget` from them, and inject it into owner pools such as
//! `c2_mem::MemPool::new_with_prefix_and_budget`.
//!
//! A zero limit rejects every positive charge in that cell; it is not
//! unlimited. The limits cover C-Two-owned IPC backing and live reassembly
//! only — not SDK heaps, HTTP buffers, peer-opened mappings, or whole-process
//! RSS.

/// Canonical finite byte limits for owner-created backing and live reassembly.
///
/// Defaults are 8 GiB SHM backing, 16 GiB file backing, and 8 GiB live
/// reassembly. The finite defaults are a deliberate behavior change for
/// workloads that previously exceeded aggregate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudgetLimits {
    /// Owner-created buddy and dedicated mapped backing, including header and
    /// alignment overhead.
    pub shm_backing_budget_bytes: u64,
    /// Owner-created file backing length.
    pub file_backing_budget_bytes: u64,
    /// Allocated capacity of live chunk reassembly storage.
    pub live_reassembly_budget_bytes: u64,
}

impl MemoryBudgetLimits {
    /// Limits with every cell set to zero: every positive charge is rejected.
    ///
    /// Useful for tests and for explicitly disabling a backing tier without
    /// introducing an unlimited mode.
    pub const fn zeroed() -> Self {
        Self {
            shm_backing_budget_bytes: 0,
            file_backing_budget_bytes: 0,
            live_reassembly_budget_bytes: 0,
        }
    }
}

const GIB: u64 = 1 << 30;

impl Default for MemoryBudgetLimits {
    fn default() -> Self {
        Self {
            shm_backing_budget_bytes: 8 * GIB,
            file_backing_budget_bytes: 16 * GIB,
            live_reassembly_budget_bytes: 8 * GIB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_defaults_are_the_accepted_finite_limits() {
        let limits = MemoryBudgetLimits::default();
        assert_eq!(limits.shm_backing_budget_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(limits.file_backing_budget_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(limits.live_reassembly_budget_bytes, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn zeroed_limits_are_finite_and_rejective_not_unlimited() {
        assert_eq!(
            MemoryBudgetLimits::zeroed(),
            MemoryBudgetLimits {
                shm_backing_budget_bytes: 0,
                file_backing_budget_bytes: 0,
                live_reassembly_budget_bytes: 0,
            }
        );
    }
}
