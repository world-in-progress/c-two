//! One atomic admission for the real input and both transport copy buffers.

use std::time::Instant;

use c2_mem::retention::{
    PayloadBufferBudget, PayloadBufferError, PayloadBufferReservation, PayloadBufferSplitError,
};

use crate::client::HttpError;

/// Sizes derived from the selected path, before allocating any of its buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBufferPlan {
    input_bytes: u64,
    upload_bytes: u64,
    download_bytes: u64,
}

/// Independent move-only owners: no completed direction retains the input.
pub struct SendBuffers {
    pub input: PayloadBufferReservation,
    pub upload: PayloadBufferReservation,
    pub download: PayloadBufferReservation,
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
        mut reservation: PayloadBufferReservation,
    ) -> Result<SendBuffers, PayloadBufferSplitError> {
        if reservation.bytes() != self.total_bytes() {
            return Err(PayloadBufferSplitError {
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
    budget: &PayloadBufferBudget,
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

    #[test]
    fn both_directions_share_one_small_budget_and_refund_independently() {
        let budget = PayloadBufferBudget::new(7);
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
            .split(budget.try_reserve(plan.total_bytes()).unwrap())
            .unwrap();
        assert_eq!(budget.snapshot().used_bytes, 7);
        drop(buffers.input);
        assert_eq!(budget.snapshot().used_bytes, 3);
        drop(buffers.upload);
        assert_eq!(budget.snapshot().used_bytes, 2);
        drop(buffers.download);
        assert_eq!(budget.snapshot().used_bytes, 0);
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
        let budget = PayloadBufferBudget::new(8);
        let occupied = budget.try_reserve(4).unwrap();
        let plan = SendBufferPlan::new(8, 4, 2, 2).unwrap();
        let mut waiting = Box::pin(reserve_send_buffers(&budget, plan, None));
        assert!(waiting.as_mut().now_or_never().is_none());
        assert_eq!(budget.snapshot().used_bytes, 4);
        drop(occupied);
        let buffers = waiting.await.unwrap();
        assert_eq!(budget.snapshot().used_bytes, 8);
        drop(buffers);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }
}
