use std::fmt;

use c2_core::{Error, HeldResponse};

/// A retained SDK value paired with its still-live C-Two response lease.
///
/// The current portable receive path is copy-backed. Retention is a lifetime
/// contract, not a zero-copy claim.
#[must_use = "held values must remain owned or be explicitly released"]
pub struct Held<T> {
    owners: Option<(T, HeldResponse)>,
    invalidate: fn(&T) -> Result<(), String>,
}

impl<T> Held<T> {
    pub(crate) fn new(
        value: T,
        response: HeldResponse,
        invalidate: fn(&T) -> Result<(), String>,
    ) -> Self {
        Self {
            owners: Some((value, response)),
            invalidate,
        }
    }

    /// Borrow the retained value while this owner remains live.
    pub fn value(&self) -> Option<&T> {
        self.owners.as_ref().map(|(value, _)| value)
    }

    pub fn is_released(&self) -> bool {
        self.owners.is_none()
    }

    /// Invalidate the retained value owner before releasing transport storage.
    ///
    /// Both actions are attempted exactly once. Calling `release` again is a
    /// no-op. Prefer this explicit path when cleanup failures must be audited.
    pub fn release(&mut self) -> Result<(), Error> {
        let Some(owners) = self.owners.take() else {
            return Ok(());
        };
        // Separate locals preserve response-before-value drop during unwinding.
        let value = owners.0;
        let mut response = owners.1;
        let result = response.invalidate_then_release(|| (self.invalidate)(&value));
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
        let _ = self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn failed_invalidation_consumes_held_owners_once() {
        for panics in [false, true] {
            let invalidations = Cell::new(0);
            let mut held = Held::new(
                (&invalidations, panics),
                HeldResponse::from_owned_bytes(Vec::new()),
                |(invalidations, panics)| {
                    invalidations.set(invalidations.get() + 1);
                    assert!(!panics, "invalidation panic");
                    Err("invalidation failed".into())
                },
            );
            let result = catch_unwind(AssertUnwindSafe(|| held.release()));
            assert_eq!(result.is_err(), panics);
            if let Ok(result) = result {
                assert!(result.is_err());
            }
            assert!(held.is_released());
            assert!(held.value().is_none());
            held.release().expect("already released");
            drop(held);
            assert_eq!(invalidations.get(), 1);
        }
    }
}
