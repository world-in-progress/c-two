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
                let transport_error = lease.release().err();
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

    /// Invalidate the SDK payload owner, then release transport storage.
    ///
    /// Both operations are attempted exactly once. A failure in either action
    /// cannot suppress the other, and a combined failure retains both causes.
    pub fn invalidate_then_release<F>(&mut self, invalidate: F) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>,
    {
        if self.released {
            return Ok(());
        }

        let invalidation_error = invalidate().err();
        let transport_error = self
            .lease
            .take()
            .and_then(|mut lease| lease.release().err());
        self.released = true;

        if invalidation_error.is_none() && transport_error.is_none() {
            Ok(())
        } else {
            Err(LifecycleError::HeldResponseRelease {
                invalidation_error,
                transport_error,
            })
        }
    }
}

impl Drop for HeldResponse {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Some(mut lease) = self.lease.take() {
            let _ = lease.release();
        }
        self.released = true;
    }
}
