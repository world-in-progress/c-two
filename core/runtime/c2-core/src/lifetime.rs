use crate::LifecycleError;
use c2_mem::{BudgetedBuffer, DomainBudget, RecycledBytes};

enum HeldStorage {
    Local(RecycledBytes),
    Budgeted(BudgetedBuffer),
}

/// Local response bytes owned independently of transport storage or connections.
/// SDKs invalidate their checked payload owner before ending this local buffer.
pub struct HeldResponse {
    storage: Option<HeldStorage>,
}

impl HeldResponse {
    pub fn from_owned_bytes(bytes: Vec<u8>) -> Self {
        Self {
            storage: Some(HeldStorage::Local(bytes.into())),
        }
    }

    pub fn from_budgeted_bytes(buffer: BudgetedBuffer) -> Self {
        Self {
            storage: Some(HeldStorage::Budgeted(buffer)),
        }
    }

    /// Consume transport storage before publishing the local response.
    /// Inline bytes move directly, without another copy.
    pub fn from_response_data(response: c2_ipc::ResponseData) -> Result<Self, LifecycleError> {
        response
            .into_owned_bytes()
            .map(Self::from_owned_bytes)
            .map_err(LifecycleError::ResponseCopy)
    }

    pub fn from_response_data_in(
        response: c2_ipc::ResponseData,
        budget: &DomainBudget,
    ) -> Result<Self, LifecycleError> {
        let bytes = response
            .into_owned_bytes_in(budget)
            .map_err(LifecycleError::ResponseCopy)?;
        Ok(Self {
            storage: Some(HeldStorage::Local(RecycledBytes::new(
                bytes,
                budget.clone(),
            ))),
        })
    }

    /// Release a local response's transport budget while retaining its bytes.
    /// The allocation returns to the original domain's warm pool on drop.
    pub fn detach_budget(&mut self) {
        if let Some(HeldStorage::Budgeted(_)) = &self.storage {
            let Some(HeldStorage::Budgeted(buffer)) = self.storage.take() else {
                unreachable!("storage was budgeted");
            };
            let (bytes, reservation) = buffer.into_parts();
            let budget = reservation.domain_budget();
            drop(reservation);
            self.storage = Some(HeldStorage::Local(RecycledBytes::new(bytes, budget)));
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.storage {
            Some(HeldStorage::Local(bytes)) => bytes,
            Some(HeldStorage::Budgeted(buffer)) => buffer,
            None => &[],
        }
    }

    pub const fn is_released(&self) -> bool {
        self.storage.is_none()
    }

    /// Run invalidation once, then drop the local buffer, including on error
    /// or unwind. The original invalidation failure is preserved.
    pub fn invalidate_then_release<F>(&mut self, invalidate: F) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>,
    {
        // Keep the allocation alive through invalidation. Unwinding drops
        // this same local owner and cannot replay the callback on retry.
        let Some(owned) = self.storage.take() else {
            return Ok(());
        };
        let result = invalidate();
        drop(owned);
        result.map_err(|invalidation_error| LifecycleError::HeldResponseRelease {
            invalidation_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn http_hold_keeps_memory_charged_through_invalidation() {
        let budget = DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 4,
            ..Default::default()
        });
        let bytes = BudgetedBuffer::new(vec![1; 4], budget.try_reserve_memory(4).unwrap());
        let mut held = HeldResponse::from_budgeted_bytes(bytes);
        assert_eq!(budget.snapshot().memory.used_bytes, 4);
        held.invalidate_then_release(|| {
            assert_eq!(budget.snapshot().memory.used_bytes, 4);
            Ok(())
        })
        .unwrap();
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
    }

    #[test]
    fn detach_budget_refunds_memory_and_recycles_on_drop() {
        let size = 256 * 1024;
        let budget = DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: size as u64,
            ..Default::default()
        });
        let bytes = vec![5; size];
        let ptr = bytes.as_ptr();
        let mut held = HeldResponse::from_budgeted_bytes(BudgetedBuffer::new(
            bytes,
            budget.try_reserve_memory(size as u64).unwrap(),
        ));
        assert_eq!(budget.snapshot().memory.used_bytes, size as u64);
        held.detach_budget();
        held.detach_budget();
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        assert_eq!(held.bytes().as_ptr(), ptr);
        assert_eq!(held.bytes()[size - 1], 5);
        drop(held);
        let reused = budget.allocate_vec(size).unwrap();
        assert_eq!(reused.as_ptr(), ptr);
    }

    #[test]
    fn ipc_hold_recycles_after_invalidation_and_on_drop_without_a_charge() {
        let size = 256 * 1024;
        let budget = DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: size as u64,
            ..Default::default()
        });
        let bytes = vec![42; size];
        let ptr = bytes.as_ptr();
        let mut held =
            HeldResponse::from_response_data_in(c2_ipc::ResponseData::Inline(bytes), &budget)
                .unwrap();
        assert_eq!(held.bytes().as_ptr(), ptr);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        held.invalidate_then_release(|| {
            let other = budget.allocate_vec(size).unwrap();
            assert_ne!(other.as_ptr(), ptr);
            Ok(())
        })
        .unwrap();
        assert!(held.is_released());
        let mut bytes = budget.allocate_vec(size).unwrap();
        assert_eq!(bytes.as_ptr(), ptr);
        bytes.push(7);
        let held =
            HeldResponse::from_response_data_in(c2_ipc::ResponseData::Inline(bytes), &budget)
                .unwrap();
        assert_eq!(held.bytes(), &[7]);
        drop(held);
        let bytes = budget.allocate_vec(size).unwrap();
        assert_eq!(bytes.as_ptr(), ptr);
        assert_eq!(budget.snapshot().memory.peak_bytes, 0);
    }

    #[test]
    fn caught_invalidator_panic_does_not_replay_callback() {
        let mut held = HeldResponse::from_owned_bytes(vec![1]);
        let calls = Cell::new(0);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            held.invalidate_then_release(|| {
                calls.set(calls.get() + 1);
                panic!("injected invalidator panic");
            })
        }));
        assert!(panic.is_err());
        assert!(held.is_released());
        assert!(held.bytes().is_empty());
        held.invalidate_then_release(|| {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
    }
}
