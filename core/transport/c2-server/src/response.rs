//! Response payload preparation helpers.

use std::sync::Arc;

use c2_mem::{
    DomainBudget, MemPool, MemoryReservation, PayloadBufferError, SlabAllocError, WritableSlab,
};

use crate::dispatcher::PreparedShmResponse;
use crate::outcome::HostOutcomeStore;

/// One retry allowance for a Host allocation, including its fallback tiers.
pub(crate) struct OutcomePressure<'a> {
    store: Option<&'a HostOutcomeStore>,
}

impl<'a> OutcomePressure<'a> {
    pub(crate) fn new(store: Option<&'a HostOutcomeStore>) -> Self {
        Self { store }
    }

    pub(crate) fn shed(&mut self, needed_bytes: u64) -> bool {
        let Some(store) = self.store.take() else {
            return false;
        };
        // Released owners can reacquire a pool or domain lock. The failed
        // allocation has returned before we call into the store.
        let owners = store.shed_for_pressure(needed_bytes);
        let released = !owners.is_empty();
        owners.deliver();
        released
    }

    pub(crate) fn reserve_memory(
        &mut self,
        budget: &DomainBudget,
        bytes: u64,
    ) -> Result<MemoryReservation, PayloadBufferError> {
        let result = budget.try_reserve_memory(bytes);
        if let Err(PayloadBufferError::InsufficientCapacity { available, .. }) = &result
            && self.shed(bytes - available)
        {
            return budget.try_reserve_memory(bytes);
        }
        result
    }
}

/// The server's fixed response-pool owner.
///
/// Callbacks may inspect its configuration or prepare a response through
/// [`try_prepare_shm_response`]. Safe access cannot replace the pool advertised
/// to connected peers or release a backing still owned by a prepared response.
pub struct ResponsePool {
    pub(crate) inner: Arc<parking_lot::RwLock<MemPool>>,
    outcome: Option<Arc<HostOutcomeStore>>,
    max_payload_size: u64,
}

impl ResponsePool {
    pub(crate) fn new(pool: MemPool) -> Self {
        Self {
            inner: Arc::new(parking_lot::RwLock::new(pool)),
            outcome: None,
            max_payload_size: u64::MAX,
        }
    }

    pub(crate) fn with_outcome_store(
        mut self,
        store: Arc<HostOutcomeStore>,
        max_payload_size: u64,
    ) -> Self {
        self.outcome = Some(store);
        self.max_payload_size = max_payload_size;
        self
    }

    pub(crate) fn pressure(&self) -> OutcomePressure<'_> {
        OutcomePressure::new(self.outcome.as_deref())
    }

    pub fn try_alloc_slab(
        &self,
        shm_threshold: u64,
        len: usize,
    ) -> Result<Option<WritableSlab>, String> {
        self.alloc_slab(shm_threshold, len, &mut self.pressure())
    }

    pub(crate) fn alloc_slab(
        &self,
        shm_threshold: u64,
        len: usize,
        pressure: &mut OutcomePressure<'_>,
    ) -> Result<Option<WritableSlab>, String> {
        if len as u64 > self.max_payload_size {
            return Err(format!(
                "response payload size {len} exceeds max_payload_size {}",
                self.max_payload_size
            ));
        }
        if len as u64 <= shm_threshold {
            return Ok(None);
        }
        match WritableSlab::try_alloc_with_reason(Arc::clone(&self.inner), len) {
            Ok(slab) => Ok(Some(slab)),
            Err(SlabAllocError::CapacityExhausted { needed_bytes })
                if pressure.shed(needed_bytes) =>
            {
                WritableSlab::try_alloc(Arc::clone(&self.inner), len)
            }
            Err(SlabAllocError::Other(message)) => Err(message),
            Err(_) => Ok(None),
        }
    }

    pub fn finish_slab(&self, slab: WritableSlab) -> Result<PreparedShmResponse, String> {
        if !slab.belongs_to(&self.inner) {
            return Err("prepared response belongs to another SHM pool".into());
        }
        Ok(PreparedShmResponse::from_slab(slab))
    }

    #[inline]
    pub fn read(&self) -> parking_lot::MappedRwLockReadGuard<'_, MemPool> {
        // An ordinary read guard exposes RwLockReadGuard::rwlock, which would
        // let safe callers recover the mutable lock and replace this owner.
        parking_lot::RwLockReadGuard::map(self.inner.read(), |pool| pool)
    }

    /// Raw access for integrations that manage pool storage themselves.
    ///
    /// # Safety
    /// The caller must preserve the advertised pool identity and every live
    /// allocation's access/release authority. In particular it must not replace
    /// or destroy the pool, or free a backing, while the server, a callback,
    /// a prepared response or a connected peer can still use that storage.
    pub unsafe fn write(&self) -> parking_lot::RwLockWriteGuard<'_, MemPool> {
        self.inner.write()
    }
}

/// Try to prepare a large response directly in the server response SHM pool.
///
/// Returns `Ok(None)` when the caller should materialise owned inline bytes and
/// let `send_response_meta()` choose inline or chunked fallback. Allocation
/// failure is therefore not a CRM failure. Errors report an over-limit payload
/// or a failure to access or write the allocated backing.
pub fn try_prepare_shm_response<F>(
    response_pool: &ResponsePool,
    shm_threshold: u64,
    len: usize,
    write_into: F,
) -> Result<Option<PreparedShmResponse>, String>
where
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    let Some(mut slab) = response_pool.try_alloc_slab(shm_threshold, len)? else {
        return Ok(None);
    };
    write_into(slab.as_mut_slice())?;
    Ok(Some(PreparedShmResponse::from_slab(slab)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::ResponsePool;
    use c2_mem::{MemPool, PoolConfig};

    static TEST_POOL_ID: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix(label: &str) -> String {
        let id = TEST_POOL_ID.fetch_add(1, Ordering::Relaxed);
        format!("/c2r{:04x}{:04x}", std::process::id() as u16, id) + label
    }

    fn test_pool() -> ResponsePool {
        ResponsePool::new(
            MemPool::new_with_prefix(
                PoolConfig {
                    segment_size: 64 * 1024,
                    max_segments: 1,
                    max_dedicated_segments: 1,
                    buddy_idle_decay_secs: 60.0,
                    spill_threshold: 0.8,
                    spill_dir: std::env::temp_dir().join("c2_response_helper_test"),
                    ..PoolConfig::default()
                },
                unique_prefix("a"),
            )
            .unwrap(),
        )
    }

    #[test]
    fn small_payload_stays_owned_inline_without_pool_allocation() {
        let pool = test_pool();

        let meta = super::try_prepare_shm_response(&pool, 1024, 32, |_| {
            panic!("small payload should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn large_payload_is_written_directly_into_response_pool() {
        let pool = test_pool();
        let payload = b"x".repeat(8192);

        let mut prepared = super::try_prepare_shm_response(&pool, 1024, payload.len(), |dst| {
            dst.copy_from_slice(&payload);
            Ok(())
        })
        .unwrap()
        .expect("large payload should allocate response SHM");

        assert_eq!(prepared.data_size() as usize, payload.len());
        assert_eq!(prepared.copy_logical_bytes().unwrap(), payload);
        prepared.release();
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn prepared_response_moves_original_owner_into_connection() {
        use c2_wire::connection::{ConnectionCore, ConnectionSide};

        let pool = test_pool();
        let prepared = super::try_prepare_shm_response(&pool, 1024, 8192, |dst| {
            dst.fill(0x5a);
            Ok(())
        })
        .unwrap()
        .unwrap();
        let core = ConnectionCore::new(ConnectionSide::Server, || {});
        core.finish_handshake().unwrap();
        let loan = prepared.into_loan(&core).unwrap();
        assert_eq!(pool.read().stats().alloc_count, 1);
        drop(loan);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn write_failure_frees_allocated_response_block() {
        let pool = test_pool();

        let err = super::try_prepare_shm_response(&pool, 1024, 8193, |dst| {
            assert_eq!(dst.len(), 8193);
            Err("copy failed".to_string())
        })
        .expect_err("copy failure should be reported");

        assert!(err.contains("copy failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(pool.read().stats().buddy_occupied_bytes, 0);
    }

    #[test]
    fn fill_panic_releases_original_allocation() {
        let pool = test_pool();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::try_prepare_shm_response(&pool, 1024, 8192, |_| panic!("fill panic"))
        }));
        assert!(panic.is_err());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn allocation_failure_falls_back_to_owned_inline_path() {
        let pool = ResponsePool::new(
            MemPool::new_with_prefix(
                PoolConfig {
                    segment_size: 64 * 1024,
                    max_segments: 0,
                    max_dedicated_segments: 0,
                    buddy_idle_decay_secs: 60.0,
                    spill_threshold: 0.8,
                    spill_dir: std::env::temp_dir().join("c2_response_helper_alloc_fail"),
                    // max_segments 0 requires a zero retirement floor.
                    min_retained_segments: 0,
                    ..PoolConfig::default()
                },
                unique_prefix("b"),
            )
            .unwrap(),
        );

        let meta = super::try_prepare_shm_response(&pool, 1024, 8192, |_| {
            panic!("allocation failure should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn oversized_buddy_wire_payload_falls_back_without_allocation() {
        let pool = test_pool();

        let meta = super::try_prepare_shm_response(&pool, 1024, u32::MAX as usize + 1, |_| {
            panic!("oversized buddy response should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn full_response_pool_sheds_retained_shm_after_leaving_pool_lock() {
        use crate::outcome::{
            AdmitOutcome, ExecutionAdmission, HostOutcomeStore, OutcomeLimits, OutcomeResponse,
        };
        use std::sync::Arc;
        use std::time::Duration;

        const LEN: usize = 64 * 1024;
        let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: LEN as u64,
            ..Default::default()
        });
        let store = Arc::new(HostOutcomeStore::new(
            OutcomeLimits {
                max_entries: 2,
                max_retained_bytes: LEN as u64,
            },
            budget.clone(),
        ));
        let pool = ResponsePool::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    pool_enabled: false,
                    max_dedicated_segments: 1,
                    spill_threshold: 1.0,
                    ..PoolConfig::default()
                },
                unique_prefix("pressure"),
                budget,
            )
            .unwrap(),
        )
        .with_outcome_store(Arc::clone(&store), LEN as u64);

        let route: Arc<str> = Arc::from("route");
        let AdmitOutcome::Miss { generation } = store.admit(&route, "old", 0) else {
            panic!("expected first admission");
        };
        let ExecutionAdmission::Won(owners) =
            store.begin_execution(&route, "old", generation, [0; 32], || true)
        else {
            panic!("expected execution admission");
        };
        owners.deliver();
        let block = Arc::new(
            pool.try_alloc_slab(0, LEN)
                .unwrap()
                .unwrap()
                .seal()
                .unwrap(),
        );
        store
            .publish_success(&route, "old", generation, OutcomeResponse::Shm(block))
            .deliver();

        let error = super::try_prepare_shm_response(&pool, 2 * LEN as u64, LEN + 1, |_| {
            panic!("over-limit value must fail before writing")
        })
        .unwrap_err();
        assert!(error.contains("exceeds max_payload_size"));
        assert!(
            store
                .query(&route, "old", |_| Some("m".into()))
                .result_available
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(pool.try_alloc_slab(0, LEN)).unwrap();
        });
        let slab = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("release of the previous sealed block must not wait on allocation's pool lock")
            .unwrap()
            .expect("releasing the retained block makes room for the new slab");
        worker.join().unwrap();
        assert_eq!(slab.len(), LEN);
        assert_eq!(
            store
                .query(&route, "old", |_| Some("m".into()))
                .result_unavailable_reason,
            Some(c2_wire::rpc::WireResultUnavailableReason::Evicted),
        );
    }
}
