use crate::LifecycleError;
use c2_mem::retention::{BudgetedBuffer, PayloadBufferReservation};

/// Local response bytes owned independently of transport storage or connections.
/// SDKs invalidate their checked payload owner before ending this local buffer.
pub struct HeldResponse {
    bytes: Vec<u8>,
    reservation: Option<PayloadBufferReservation>,
    released: bool,
}

impl HeldResponse {
    pub fn from_owned_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reservation: None,
            released: false,
        }
    }

    pub fn from_budgeted_bytes(buffer: BudgetedBuffer) -> Self {
        let (bytes, reservation) = buffer.into_parts();
        Self {
            bytes,
            reservation: Some(reservation),
            released: false,
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

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn is_released(&self) -> bool {
        self.released
    }

    /// Run invalidation once, then drop the local buffer, including on error
    /// or unwind. The original invalidation failure is preserved.
    pub fn invalidate_then_release<F>(&mut self, invalidate: F) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>,
    {
        if self.released {
            return Ok(());
        }
        self.released = true;
        // Keep the allocation alive through invalidation. Unwinding drops
        // this same local owner and cannot replay the callback on retry.
        let owned = (std::mem::take(&mut self.bytes), self.reservation.take());
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
    use c2_mem::retention::PayloadBufferBudget;
    use std::cell::Cell;

    #[test]
    fn http_hold_keeps_b_until_invalidation_and_buffer_release() {
        let budget = PayloadBufferBudget::new(4);
        let bytes = BudgetedBuffer::new(vec![1; 4], budget.reserve_blocking(4, None).unwrap());
        let mut held = HeldResponse::from_budgeted_bytes(bytes);
        assert_eq!(budget.snapshot().used_bytes, 4);
        held.invalidate_then_release(|| {
            assert_eq!(budget.snapshot().used_bytes, 4);
            Ok(())
        })
        .unwrap();
        assert_eq!(budget.snapshot().used_bytes, 0);
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
