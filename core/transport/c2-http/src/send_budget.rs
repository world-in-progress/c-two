//! One atomic admission for the real input and both transport copy buffers.

use std::time::Instant;

use c2_mem::{DomainBudget, MemoryReservation, PayloadBufferError, ReusableWindow};

use crate::client::HttpError;

/// Bounds for one atomic forwarding admission: fixed retained input plus
/// flexible upload/download transport copy windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBufferPlan {
    input_bytes: u64,
    upload_cap: u64,
    download_cap: u64,
    max_window: u64,
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
        Self::build(
            budget_limit,
            input_bytes,
            upload_cap.min(input_bytes),
            download_cap,
        )
    }

    /// Plan a streaming forward that retains no full input body and streams
    /// both directions through bounded copy windows.
    pub fn streaming(
        budget_limit: u64,
        upload_cap: u64,
        download_cap: u64,
    ) -> Result<Self, PayloadBufferError> {
        Self::build(budget_limit, 0, upload_cap, download_cap)
    }

    fn build(
        budget_limit: u64,
        input_bytes: u64,
        upload_cap: u64,
        download_cap: u64,
    ) -> Result<Self, PayloadBufferError> {
        let upload_min = u64::from(upload_cap != 0);
        let download_min = u64::from(download_cap != 0);
        let required = u128::from(input_bytes) + u128::from(upload_min) + u128::from(download_min);
        if required > u128::from(budget_limit) {
            return Err(PayloadBufferError::TooLarge {
                requested: required,
                limit: budget_limit,
            });
        }
        let remaining = budget_limit - input_bytes;
        let max_window = upload_cap.saturating_add(download_cap).min(remaining);
        Ok(Self {
            input_bytes,
            upload_cap,
            download_cap,
            max_window,
        })
    }

    fn upload_min(self) -> u64 {
        u64::from(self.upload_cap != 0)
    }

    fn download_min(self) -> u64 {
        u64::from(self.download_cap != 0)
    }

    pub fn min_window(self) -> u64 {
        self.upload_min() + self.download_min()
    }

    pub fn max_window(self) -> u64 {
        self.max_window
    }

    pub fn min_total_bytes(self) -> u64 {
        self.input_bytes + self.min_window()
    }

    pub fn max_total_bytes(self) -> u64 {
        self.input_bytes + self.max_window
    }

    pub fn split(
        self,
        mut reservation: MemoryReservation,
    ) -> Result<SendBuffers, c2_mem::MemoryReservationError> {
        let reserved = reservation.bytes();
        let min_total = self.min_total_bytes();
        let max_total = self.max_total_bytes();
        if reserved < min_total || reserved > max_total {
            return Err(c2_mem::MemoryReservationError {
                requested: min_total,
                reserved,
            });
        }
        let input = reservation.split_off(self.input_bytes)?;
        let window = reservation.bytes();
        let upload_min = self.upload_min();
        let download_min = self.download_min();
        let upload_bytes = if upload_min == 0 {
            0
        } else {
            self.upload_cap
                .min(
                    (window / 2)
                        .max(upload_min)
                        .max(window.saturating_sub(self.download_cap)),
                )
                .min(window - download_min)
        };
        let upload = reservation.split_off(upload_bytes)?;
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
    let reservation = crate::payload::reserve_payload_with_window(
        budget,
        plan.input_bytes,
        plan.min_window(),
        plan.max_window(),
        deadline,
        "pre_dispatch",
    )
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
        assert_eq!(plan.min_total_bytes(), 6);
        assert_eq!(plan.max_total_bytes(), 7);
        let buffers = plan
            .split(domain.try_reserve_memory(plan.max_total_bytes()).unwrap())
            .unwrap();
        assert_eq!(buffers.input.bytes(), 4);
        assert_eq!(buffers.upload.bytes(), 1);
        assert_eq!(buffers.download.bytes(), 2);
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
            SendBufferPlan::new(1, 0, 128, 128)
                .unwrap()
                .max_total_bytes(),
            1
        );
        assert!(SendBufferPlan::new(u64::MAX, u64::MAX, 1, 1).is_err());
        assert_eq!(
            SendBufferPlan::new(u64::MAX, u64::MAX - 2, 128, 128)
                .unwrap()
                .max_total_bytes(),
            u64::MAX
        );
        let streaming = SendBufferPlan::streaming(128, 64, 128).unwrap();
        assert_eq!(streaming.min_total_bytes(), 2);
        assert_eq!(streaming.max_total_bytes(), 128);
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
        assert_eq!(buffers.input.bytes(), 4);
        assert_eq!(buffers.upload.bytes(), 1);
        assert_eq!(buffers.download.bytes(), 1);
        assert_eq!(domain.snapshot().memory.used_bytes, 6);
        drop(buffers);
        assert_eq!(domain.snapshot().memory.used_bytes, 0);
    }

    #[tokio::test]
    async fn fair_share_admits_calls_alongside_long_lived_memory_holders() {
        let domain = budget(128 * 1024);
        // Simulate a TopicHub holding a small latest frame inside a 128 KiB relay.
        let held = domain.try_reserve_memory(15).unwrap();
        let local_plan = SendBufferPlan::new(128 * 1024, 16, 128 * 1024, 128 * 1024).unwrap();
        let local = reserve_send_buffers(&domain, local_plan, None)
            .await
            .unwrap();
        assert_eq!(local.input.bytes(), 16);
        assert_eq!(local.upload.bytes(), 16);
        assert!(local.download.bytes() >= 32 * 1024);
        // A second concurrent streaming call still admits immediately.
        let peer_plan = SendBufferPlan::streaming(128 * 1024, 128 * 1024, 128 * 1024).unwrap();
        let peer = reserve_send_buffers(&domain, peer_plan, None)
            .await
            .unwrap();
        assert!(peer.upload.bytes() >= 16_000);
        assert!(peer.download.bytes() >= 16_000);
        assert!(domain.snapshot().memory.used_bytes < 128 * 1024);
        drop((peer, local, held));
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
