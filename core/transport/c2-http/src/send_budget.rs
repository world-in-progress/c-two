//! One atomic admission for the real input and both transport copy buffers.

use std::time::Instant;

use c2_mem::{DomainBudget, MemoryReservation, PayloadBufferError, ReusableWindow};

use crate::client::HttpError;

/// Sizes derived from the selected path, before allocating any of its buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBufferPlan {
    input_bytes: u64,
    upload_bytes: u64,
    download_bytes: u64,
}

/// Independent move-only owners: no completed direction retains the input. The
/// download area becomes the forwarding [`ReusableWindow`] once its final size
/// is known; until then it stays a plain reservation so callers may re-slice
/// it against the remaining domain capacity.
pub struct SendBuffers {
    pub input: MemoryReservation,
    pub upload: MemoryReservation,
    pub download: MemoryReservation,
}

impl SendBuffers {
    /// Freeze both transport areas: the upload copy window keeps its plain
    /// reservation, the download area becomes the reusable forwarding window
    /// that each response slice refunds after the body consumes it.
    pub fn into_forwarding(self) -> ForwardingBuffers {
        ForwardingBuffers {
            upload: self.upload,
            download: self.download.into_reusable_window(),
        }
    }
}

/// The upload copy window and the reusable download window of one forwarding.
pub struct ForwardingBuffers {
    pub upload: MemoryReservation,
    pub download: ReusableWindow,
}

impl SendBufferPlan {
    pub fn new(
        budget_limit: u64,
        input_bytes: u64,
        upload_cap: u64,
        download_cap: u64,
    ) -> Result<Self, PayloadBufferError> {
        let upload_cap = upload_cap.min(input_bytes);
        let upload_min = u64::from(input_bytes != 0 && upload_cap != 0);
        let download_min = u64::from(download_cap != 0);
        let required = u128::from(input_bytes) + u128::from(upload_min) + u128::from(download_min);
        if required > u128::from(budget_limit) {
            return Err(PayloadBufferError::TooLarge {
                requested: required,
                limit: budget_limit,
            });
        }
        let remaining = budget_limit - input_bytes;
        let upload_bytes = if upload_min == 0 {
            0
        } else {
            upload_cap.min((remaining / 2).max(upload_min))
        };
        let download_bytes = download_cap.min(remaining - upload_bytes);
        Ok(Self {
            input_bytes,
            upload_bytes,
            download_bytes,
        })
    }

    pub fn total_bytes(self) -> u64 {
        // Construction admits this exact sum against a u64-sized domain.
        self.input_bytes + self.upload_bytes + self.download_bytes
    }

    pub fn split(
        self,
        mut reservation: MemoryReservation,
    ) -> Result<SendBuffers, c2_mem::MemoryReservationError> {
        if reservation.bytes() != self.total_bytes() {
            return Err(c2_mem::MemoryReservationError {
                requested: self.total_bytes(),
                reserved: reservation.bytes(),
            });
        }
        let input = reservation.split_off(self.input_bytes)?;
        let upload = reservation.split_off(self.upload_bytes)?;
        Ok(SendBuffers {
            input,
            upload,
            download: reservation,
        })
    }
}

pub async fn reserve_send_buffers(
    budget: &DomainBudget,
    plan: SendBufferPlan,
    deadline: Option<Instant>,
) -> Result<SendBuffers, HttpError> {
    let reservation =
        crate::payload::reserve_payload(budget, plan.total_bytes(), deadline, "pre_dispatch")
            .await?;
    Ok(plan
        .split(reservation)
        .expect("exact atomic send admission"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn budget(limit: u64) -> DomainBudget {
        crate::payload::memory_only_budget(limit)
    }

    #[test]
    fn both_directions_share_one_small_budget_and_refund_independently() {
        let domain = budget(7);
        let plan = SendBufferPlan::new(7, 4, 128, 128).unwrap();
        assert_eq!(
            plan,
            SendBufferPlan {
                input_bytes: 4,
                upload_bytes: 1,
                download_bytes: 2
            }
        );
        let buffers = plan
            .split(domain.try_reserve_memory(plan.total_bytes()).unwrap())
            .unwrap();
        assert_eq!(domain.snapshot().memory.used_bytes, 7);
        drop(buffers.input);
        assert_eq!(domain.snapshot().memory.used_bytes, 3);
        drop(buffers.upload);
        assert_eq!(domain.snapshot().memory.used_bytes, 2);
        drop(buffers.download);
        assert_eq!(domain.snapshot().memory.used_bytes, 0);
    }

    #[test]
    fn minimum_admission_handles_empty_input_and_u64_boundary() {
        assert!(matches!(
            SendBufferPlan::new(5, 4, 128, 128),
            Err(PayloadBufferError::TooLarge { requested: 6, .. })
        ));
        assert!(SendBufferPlan::new(0, 0, 128, 128).is_err());
        assert_eq!(
            SendBufferPlan::new(1, 0, 128, 128).unwrap().total_bytes(),
            1
        );
        assert!(SendBufferPlan::new(u64::MAX, u64::MAX, 1, 1).is_err());
        assert_eq!(
            SendBufferPlan::new(u64::MAX, u64::MAX - 2, 128, 128)
                .unwrap()
                .total_bytes(),
            u64::MAX
        );
    }

    #[tokio::test]
    async fn waiting_for_the_group_never_owns_a_partial_admission() {
        let domain = budget(8);
        let occupied = domain.try_reserve_memory(4).unwrap();
        let plan = SendBufferPlan::new(8, 4, 2, 2).unwrap();
        let mut waiting = Box::pin(reserve_send_buffers(&domain, plan, None));
        assert!(waiting.as_mut().now_or_never().is_none());
        assert_eq!(domain.snapshot().memory.used_bytes, 4);
        drop(occupied);
        let buffers = waiting.await.unwrap();
        assert_eq!(domain.snapshot().memory.used_bytes, 8);
        drop(buffers);
        assert_eq!(domain.snapshot().memory.used_bytes, 0);
    }

    #[tokio::test]
    async fn download_area_becomes_a_refunding_reusable_window() {
        let domain = budget(6);
        let plan = SendBufferPlan::new(6, 4, 2, 4).unwrap();
        let buffers = reserve_send_buffers(&domain, plan, None).await.unwrap();
        let forwarding = buffers.into_forwarding();
        assert_eq!(forwarding.download.capacity(), 1);
        assert_eq!(domain.snapshot().memory.used_bytes, 2);
        for _ in 0..2 {
            let slice = forwarding.download.reserve(1).await.unwrap();
            assert!(forwarding.download.reserve(1).now_or_never().is_none());
            drop(slice);
        }
        // Refunded window capacity stays reserved by the window itself.
        assert_eq!(domain.snapshot().memory.used_bytes, 2);
        drop(forwarding.upload);
        assert_eq!(domain.snapshot().memory.used_bytes, 1);
        drop(forwarding.download);
        assert_eq!(domain.snapshot().memory.used_bytes, 0);
    }
}
