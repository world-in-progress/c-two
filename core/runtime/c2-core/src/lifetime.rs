use c2_ipc::ResponseLease;

use crate::LifecycleError;

/// Core-owned response bytes paired with their still-live transport lease.
///
/// The Core treats the bytes as opaque. An SDK-owned `Held<Payload>` opens its
/// payload owner from `bytes` and supplies that owner's invalidation callback
/// to `invalidate_then_release`.
///
/// Checked payload views can be invalidated by their owner. Unsafe raw pointers
/// obtained outside that checked access model cannot be revoked by C-Two.
pub struct HeldResponse {
    bytes: Vec<u8>,
    lease: Option<ResponseLease>,
    released: bool,
}

impl HeldResponse {
    /// Construct a held result whose HTTP transport has already produced an
    /// owned byte vector. The same invalidation API is retained even though no
    /// transport allocation remains to release.
    pub fn from_owned_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            lease: None,
            released: false,
        }
    }

    /// Copy the response while retaining its transport lease.
    ///
    /// A failed copy still attempts transport cleanup and preserves both causes
    /// in the returned lifecycle error.
    pub fn from_response_lease(mut lease: ResponseLease) -> Result<Self, LifecycleError> {
        match lease.copy_bytes() {
            Ok(bytes) => Ok(Self {
                bytes,
                lease: Some(lease),
                released: false,
            }),
            Err(copy_error) => {
                let transport_error = match lease.release() {
                    Ok(()) => None,
                    Err(error) => {
                        lease.abandon();
                        Some(error)
                    }
                };
                Err(LifecycleError::HeldResponseCopy {
                    copy_error,
                    transport_error,
                })
            }
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn is_released(&self) -> bool {
        self.released
    }

    /// Whether transport storage still has an unsettled release obligation.
    pub const fn has_pending_release(&self) -> bool {
        self.lease.is_some()
    }

    /// Invalidate the SDK payload owner, then release transport storage.
    ///
    /// Invalidation is attempted once, including when it panics. A failed
    /// transport release stays with this owner for an explicit retry.
    pub fn invalidate_then_release<F>(&mut self, invalidate: F) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let invalidation_error = if self.released {
            None
        } else {
            self.released = true;
            invalidate().err()
        };
        let transport_error = match self.lease.as_mut() {
            Some(lease) => match lease.release() {
                Ok(()) => {
                    self.lease.take();
                    None
                }
                Err(error) => Some(error),
            },
            None => None,
        };

        if invalidation_error.is_none() && transport_error.is_none() {
            Ok(())
        } else {
            Err(LifecycleError::HeldResponseRelease {
                invalidation_error,
                transport_error,
            })
        }
    }

    /// Consume an unsettled transport obligation without retrying release or
    /// replaying SDK invalidation.
    pub fn abandon(mut self) {
        if let Some(lease) = self.lease.take() {
            lease.abandon();
        }
        self.released = true;
    }
}

impl Drop for HeldResponse {
    fn drop(&mut self) {
        if let Some(mut lease) = self.lease.take() {
            if let Err(error) = lease.release() {
                eprintln!("Warning: final held response release failed: {error}");
                lease.abandon();
            }
        }
        self.released = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::{MemPool, PoolConfig};
    use c2_wire::chunk::ReassemblyBacking;
    use parking_lot::RwLock;
    use std::{cell::Cell, sync::Arc};

    fn pool() -> MemPool {
        MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                ..PoolConfig::default()
            },
            format!("/c2r07-{}", uuid::Uuid::new_v4().simple()),
        )
    }

    #[test]
    fn failed_held_release_retries_original_lease_without_reinvalidating() {
        let pool = Arc::new(RwLock::new(pool()));
        let budget = pool.read().budget().unwrap().clone();
        let backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        let mut held = HeldResponse::from_response_lease(ResponseLease::new(
            c2_ipc::ResponseData::Handle(backing),
        ))
        .unwrap();
        assert!(held.has_pending_release());
        let original = std::mem::replace(&mut *pool.write(), self::pool());
        let calls = Cell::new(0);
        let first = held.invalidate_then_release(|| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert!(
            first.is_err(),
            "foreign pool must reject the original lease"
        );
        assert!(held.is_released());
        assert!(held.has_pending_release());
        *pool.write() = original;
        let second = held.invalidate_then_release(|| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        let remaining = pool.read().stats().alloc_count;
        let charge = budget.snapshot().reassembly.used_bytes;
        assert!(!held.has_pending_release());
        drop(held);
        // Only this test's original owner is torn down, including on the red run.
        pool.write().destroy();
        assert!(second.is_ok());
        assert_eq!(calls.get(), 1, "invalidation must not replay");
        assert_eq!(remaining, 0, "retry must release the original allocation");
        assert_eq!(charge, 0, "retry must refund the original reservation");
    }

    #[test]
    fn copy_and_cleanup_failure_abandons_once_and_keeps_original_error() {
        let pool = Arc::new(RwLock::new(pool()));
        let budget = pool.read().budget().unwrap().clone();
        let backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        let original = std::mem::replace(&mut *pool.write(), self::pool());

        let error = HeldResponse::from_response_lease(ResponseLease::new(
            c2_ipc::ResponseData::Handle(backing),
        ))
        .err()
        .expect("foreign pool must reject copy and cleanup");
        match error {
            LifecycleError::HeldResponseCopy {
                copy_error,
                transport_error,
            } => {
                assert!(copy_error.contains("pool owner incarnation mismatch"));
                assert!(
                    transport_error
                        .expect("cleanup cause")
                        .contains("pool owner incarnation mismatch")
                );
            }
            error => panic!("unexpected error: {error}"),
        }
        assert_eq!(budget.snapshot().abandoned_releases, 1);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        *pool.write() = original;
        assert_eq!(pool.read().stats().alloc_count, 1);
        pool.write().destroy();
    }

    #[test]
    fn final_drop_abandons_failed_release_once_without_refunding() {
        let pool = Arc::new(RwLock::new(pool()));
        let budget = pool.read().budget().unwrap().clone();
        let backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        let held = HeldResponse::from_response_lease(ResponseLease::new(
            c2_ipc::ResponseData::Handle(backing),
        ))
        .unwrap();
        let original = std::mem::replace(&mut *pool.write(), self::pool());

        drop(held);
        assert_eq!(budget.snapshot().abandoned_releases, 1);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        *pool.write() = original;
        assert_eq!(pool.read().stats().alloc_count, 1);
        pool.write().destroy();
    }

    #[test]
    fn caught_invalidator_panic_does_not_replay_callback() {
        let mut held = HeldResponse::from_owned_bytes(vec![1]);
        let calls = Cell::new(0);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            held.invalidate_then_release(|| {
                calls.set(calls.get() + 1);
                panic!("R07 injected invalidator panic");
            })
        }));
        assert!(panic.is_err());
        assert!(held.is_released());
        assert!(!held.has_pending_release());
        held.invalidate_then_release(|| {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "caught panic must not replay user code");
    }
}
