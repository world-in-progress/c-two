//! Portable waiting budgets and checked monotonic deadlines.

use std::fmt;
use std::time::{Duration, Instant};

// Linux and macOS Instant use signed 64-bit seconds with nanoseconds;
// Windows Instant has a wider unsigned Duration representation. Use their
// common positive range on every architecture, independent of this host.
const MAX_TIMEOUT: Duration = Duration::new(i64::MAX as u64, 999_999_999);

/// Why a waiting budget cannot be represented on all supported platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeoutError {
    NotANumber,
    Infinite,
    Negative,
    NotRepresentable,
}

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotANumber => "timeout is not a number (NaN)",
            Self::Infinite => "timeout is infinite",
            Self::Negative => "timeout is negative",
            Self::NotRepresentable => "timeout exceeds the portable monotonic clock range",
        })
    }
}

impl std::error::Error for TimeoutError {}

/// Convert finite non-negative seconds using standard nanosecond rounding.
/// Zero (including negative zero and sub-nanosecond values rounding to zero)
/// is an immediate deadline, not an unlimited budget.
pub fn timeout_from_seconds(seconds: f64) -> Result<Duration, TimeoutError> {
    if seconds.is_nan() {
        return Err(TimeoutError::NotANumber);
    }
    if seconds.is_infinite() {
        return Err(TimeoutError::Infinite);
    }
    if seconds < 0.0 {
        return Err(TimeoutError::Negative);
    }
    let timeout =
        Duration::try_from_secs_f64(seconds).map_err(|_| TimeoutError::NotRepresentable)?;
    validate_timeout(timeout)?;
    Ok(timeout)
}

/// Check a native Duration against the same range as float-second inputs.
pub fn validate_timeout(timeout: Duration) -> Result<(), TimeoutError> {
    if timeout > MAX_TIMEOUT {
        return Err(TimeoutError::NotRepresentable);
    }
    Ok(())
}

/// Form a deadline from a portable budget. The actual clock addition also
/// needs checking: the start instant and nanosecond carry consume headroom.
pub fn deadline_after(start: Instant, timeout: Duration) -> Result<Instant, TimeoutError> {
    validate_timeout(timeout)?;
    start
        .checked_add(timeout)
        .ok_or(TimeoutError::NotRepresentable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_clock_boundary_uses_signed_seconds_on_every_platform() {
        assert_eq!(validate_timeout(MAX_TIMEOUT), Ok(()));
        assert_eq!(
            validate_timeout(MAX_TIMEOUT + Duration::from_nanos(1)),
            Err(TimeoutError::NotRepresentable)
        );
        // i64::MAX rounds up to 2^63 as f64. Its preceding float is in range.
        let boundary = i64::MAX as f64;
        assert_eq!(
            timeout_from_seconds(boundary),
            Err(TimeoutError::NotRepresentable)
        );
        assert!(timeout_from_seconds(boundary.next_down()).is_ok());
        for seconds in [1e19, 1.8e19, 2e19, 1e300] {
            assert_eq!(
                timeout_from_seconds(seconds),
                Err(TimeoutError::NotRepresentable)
            );
        }
    }

    #[test]
    fn invalid_timeout_seconds_are_rejected_without_panicking() {
        for (seconds, expected) in [
            (f64::NAN, TimeoutError::NotANumber),
            (f64::INFINITY, TimeoutError::Infinite),
            (f64::NEG_INFINITY, TimeoutError::Infinite),
            (-1.0, TimeoutError::Negative),
        ] {
            let error = timeout_from_seconds(seconds).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().is_empty());
            let _: &dyn std::error::Error = &error;
        }
    }

    #[test]
    fn timeout_seconds_preserve_standard_nanosecond_rounding() {
        for seconds in [
            0.0,
            -0.0,
            1e-10,
            f64::MIN_POSITIVE,
            0.125,
            0.333333333333,
            5.0,
        ] {
            assert_eq!(
                timeout_from_seconds(seconds),
                Duration::try_from_secs_f64(seconds).map_err(|_| TimeoutError::NotRepresentable)
            );
        }
    }

    #[test]
    fn deadline_addition_checks_the_actual_start_instant() {
        let start = Instant::now();
        assert_eq!(deadline_after(start, Duration::ZERO), Ok(start));
        // Windows can make the first addition with its wider clock; a second
        // addition then overflows. Unix already overflows on the first one.
        let start = start.checked_add(MAX_TIMEOUT).unwrap_or(start);
        assert!(start.checked_add(MAX_TIMEOUT).is_none());
        assert_eq!(
            deadline_after(start, MAX_TIMEOUT),
            Err(TimeoutError::NotRepresentable)
        );
    }
}
