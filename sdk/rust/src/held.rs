use std::fmt;

use c2_core::{Error, HeldResponse, LifecycleError};

/// A retained SDK value paired with its still-live C-Two response lease.
///
/// The current portable receive path is copy-backed. Retention is a lifetime
/// contract, not a zero-copy claim.
#[must_use = "held values must remain owned or be explicitly released"]
pub struct Held<T> {
    value: Option<T>,
    response: Option<HeldResponse>,
    invalidate: fn(&T) -> Result<(), String>,
}

impl<T> Held<T> {
    pub(crate) fn new(
        value: T,
        response: HeldResponse,
        invalidate: fn(&T) -> Result<(), String>,
    ) -> Self {
        Self {
            value: Some(value),
            response: Some(response),
            invalidate,
        }
    }

    /// Borrow the retained value while this owner remains live.
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    pub fn is_released(&self) -> bool {
        self.value.is_none() && self.response.is_none()
    }

    /// Invalidate the retained value owner before releasing transport storage.
    ///
    /// Both actions are attempted exactly once. Calling `release` again is a
    /// no-op. Prefer this explicit path when cleanup failures must be audited.
    pub fn release(&mut self) -> Result<(), Error> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<(), Error> {
        if self.is_released() {
            return Ok(());
        }

        let value = self.value.take();
        let Some(mut response) = self.response.take() else {
            let invalidation_error = value
                .as_ref()
                .and_then(|value| (self.invalidate)(value).err());
            drop(value);
            return match invalidation_error {
                Some(invalidation_error) => Err(LifecycleError::HeldResponseRelease {
                    invalidation_error: Some(invalidation_error),
                    transport_error: None,
                }
                .into()),
                None => Ok(()),
            };
        };

        let result = response.invalidate_then_release(|| match value.as_ref() {
            Some(value) => (self.invalidate)(value),
            None => Ok(()),
        });
        drop(value);
        drop(response);
        result.map_err(Error::from)
    }
}

impl<T> fmt::Debug for Held<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Held")
            .field("released", &self.is_released())
            .finish_non_exhaustive()
    }
}

impl<T> Drop for Held<T> {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}
