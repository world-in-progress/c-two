//! Caller deadlines for logical calls.

use std::time::Duration;

use crate::{TimeoutError, timeout_from_seconds, validate_call_id};

/// Total-wait deadline policy for one logical call.
///
/// Each call made through a view with a finite deadline starts its own
/// monotonic deadline; concurrent calls with different budgets do not
/// interfere. [`CallTimeout::After`] with [`Duration::ZERO`] is an already
/// expired deadline: the call must be rejected before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CallTimeout {
    /// No per-call override: resolve the deadline from the path's default
    /// policy (the caller that owns the path decides; no environment is
    /// read here).
    #[default]
    Inherit,
    /// Explicitly disable the total wait deadline for this call.
    Unlimited,
    /// Total wait budget for this call, counting route (re)acquisition,
    /// admission, waits, sending, and response receipt inside one logical
    /// call. [`Duration::ZERO`] expires immediately.
    After(Duration),
}

impl CallTimeout {
    /// Checked conversion from finite float seconds to an explicit
    /// [`CallTimeout::After`] deadline.
    ///
    /// Rejects NaN, infinities, negative values, and finite values beyond
    /// the common range of supported monotonic clocks. Otherwise the standard
    /// [`Duration::try_from_secs_f64`] conversion applies with its normal
    /// nearest-nanosecond quantization — no additional precision gate is
    /// imposed on top of it. [`0.0`] (and `-0.0`, which compares equal to
    /// zero) is accepted as an already expired deadline — never as
    /// unlimited — and a positive value smaller than half a nanosecond
    /// quantizes to that same immediate [`Duration::ZERO`] deadline, which
    /// is still `After`, not [`CallTimeout::Unlimited`].
    pub fn try_after_seconds(seconds: f64) -> Result<Self, TimeoutError> {
        timeout_from_seconds(seconds).map(CallTimeout::After)
    }

    /// Resolves this policy against the path's inherited default.
    ///
    /// `None` means unlimited; `Some(duration)` is a finite total budget.
    /// `Inherit` passes the inherited policy through, `Unlimited` always
    /// wins over a finite inherited default, and an explicit `After` is
    /// returned unchanged. Path defaults are supplied by the caller that
    /// owns the path; this resolution is pure and reads nothing global.
    pub fn resolve(self, inherited: Option<Duration>) -> Option<Duration> {
        match self {
            CallTimeout::Inherit => inherited,
            CallTimeout::Unlimited => None,
            CallTimeout::After(duration) => Some(duration),
        }
    }
}

/// Immutable per-call policy view.
///
/// One value describes the options projected onto a call view (for example
/// `Client::with_call_options`); it never mutates global configuration or a
/// shared client, and every call made through the view derives its own
/// deadline from `timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CallOptions {
    timeout: CallTimeout,
}

impl CallOptions {
    /// Options that inherit the path's default policy.
    pub const fn new() -> Self {
        Self {
            timeout: CallTimeout::Inherit,
        }
    }

    /// Options carrying one explicit timeout policy.
    pub const fn with_timeout(timeout: CallTimeout) -> Self {
        Self { timeout }
    }

    /// The per-call timeout policy.
    pub const fn timeout(&self) -> CallTimeout {
        self.timeout
    }

    /// Resolves the effective deadline of this view against the path's
    /// inherited default. See [`CallTimeout::resolve`].
    pub fn effective_timeout(&self, inherited: Option<Duration>) -> Option<Duration> {
        self.timeout.resolve(inherited)
    }
}

/// Per-call options for a single unary invocation.
///
/// Unlike reusable [`CallOptions`] (which stays [`Copy`] and carries only
/// timeout policy), `UnaryCallOptions` can carry an explicit `call_id` bound
/// to one logical unary operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct UnaryCallOptions {
    timeout: CallTimeout,
    call_id: Option<String>,
}

impl UnaryCallOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_call_options(options: CallOptions) -> Self {
        Self {
            timeout: options.timeout(),
            call_id: None,
        }
    }

    pub fn with_timeout(mut self, timeout: CallTimeout) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Result<Self, String> {
        let call_id = call_id.into();
        validate_call_id(&call_id)?;
        self.call_id = Some(call_id);
        Ok(self)
    }

    pub const fn timeout(&self) -> CallTimeout {
        self.timeout
    }

    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    pub const fn call_options(&self) -> CallOptions {
        CallOptions::with_timeout(self.timeout)
    }

    pub fn effective_timeout(&self, inherited: Option<Duration>) -> Option<Duration> {
        self.timeout.resolve(inherited)
    }
}

impl From<CallOptions> for UnaryCallOptions {
    fn from(options: CallOptions) -> Self {
        Self::from_call_options(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn try_after_seconds_accepts_finite_non_negative_values() {
        assert_eq!(
            CallTimeout::try_after_seconds(0.0).unwrap(),
            CallTimeout::After(Duration::ZERO),
            "zero seconds is an expired deadline, never unlimited"
        );
        assert_eq!(
            CallTimeout::try_after_seconds(2.5).unwrap(),
            CallTimeout::After(Duration::from_millis(2500))
        );
        assert_eq!(
            CallTimeout::try_after_seconds(1e-9).unwrap(),
            CallTimeout::After(Duration::from_nanos(1))
        );
        // Non-representable decimals quantize to the nearest nanosecond,
        // exactly like Duration::try_from_secs_f64.
        assert_eq!(
            CallTimeout::try_after_seconds(0.123456789123).unwrap(),
            CallTimeout::After(Duration::from_nanos(123456789))
        );
        assert_eq!(
            CallTimeout::try_after_seconds(0.333333333333).unwrap(),
            CallTimeout::After(Duration::from_nanos(333333333))
        );
    }

    #[test]
    fn sub_nanosecond_values_quantize_to_zero_immediate_deadline() {
        // The standard conversion rounds these to Duration::ZERO; they stay
        // After(ZERO) — an immediate deadline — never Unlimited.
        assert_eq!(
            CallTimeout::try_after_seconds(1e-10).unwrap(),
            CallTimeout::After(Duration::ZERO)
        );
        assert_eq!(
            CallTimeout::try_after_seconds(f64::MIN_POSITIVE).unwrap(),
            CallTimeout::After(Duration::ZERO)
        );
    }

    #[test]
    fn try_after_seconds_rejects_nan_infinite_and_negative() {
        for seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -1e-9] {
            let err = CallTimeout::try_after_seconds(seconds)
                .expect_err("non-finite or negative seconds must be rejected");
            let expected = if seconds.is_nan() {
                TimeoutError::NotANumber
            } else if seconds.is_infinite() {
                TimeoutError::Infinite
            } else {
                TimeoutError::Negative
            };
            assert_eq!(err, expected);
            assert!(!err.to_string().is_empty());
        }

        // The error is a real std error.
        fn assert_std_error(_: &dyn Error) {}
        assert_std_error(&TimeoutError::Negative);
    }

    #[test]
    fn try_after_seconds_rejects_non_representable_durations() {
        // Includes values representable by Duration and Windows Instant,
        // but beyond the signed seconds used by Linux and macOS Instant.
        for seconds in [1e19, 1.8e19, 2e19, i64::MAX as f64] {
            assert_eq!(
                CallTimeout::try_after_seconds(seconds).unwrap_err(),
                TimeoutError::NotRepresentable
            );
        }
        assert_eq!(
            CallTimeout::try_after_seconds(8e18).unwrap(),
            CallTimeout::After(Duration::from_secs(8_000_000_000_000_000_000))
        );
    }

    #[test]
    fn negative_zero_is_zero_immediate_deadline() {
        assert_eq!(
            CallTimeout::try_after_seconds(-0.0).unwrap(),
            CallTimeout::After(Duration::ZERO)
        );
    }

    #[test]
    fn resolve_applies_inherit_unlimited_and_explicit_policies() {
        let inherited = Some(Duration::from_secs(300));

        assert_eq!(
            CallTimeout::Inherit.resolve(inherited),
            inherited,
            "inherit passes the path default through"
        );
        assert_eq!(
            CallTimeout::Inherit.resolve(None),
            None,
            "inherit of an unlimited path default stays unlimited"
        );
        assert_eq!(
            CallTimeout::Unlimited.resolve(inherited),
            None,
            "explicit unlimited wins over a finite path default"
        );
        assert_eq!(
            CallTimeout::After(Duration::from_secs(2)).resolve(inherited),
            Some(Duration::from_secs(2)),
            "an explicit budget is returned unchanged"
        );
    }

    #[test]
    fn call_options_default_to_inherit_and_project_one_policy() {
        let options = CallOptions::default();
        assert_eq!(options.timeout(), CallTimeout::Inherit);
        assert_eq!(
            options.effective_timeout(Some(Duration::from_secs(300))),
            Some(Duration::from_secs(300))
        );

        let bounded = CallOptions::with_timeout(CallTimeout::try_after_seconds(2.0).unwrap());
        assert_eq!(
            bounded.timeout(),
            CallTimeout::After(Duration::from_secs(2))
        );
        assert_eq!(
            bounded.effective_timeout(Some(Duration::from_secs(300))),
            Some(Duration::from_secs(2))
        );

        let unlimited = CallOptions::with_timeout(CallTimeout::Unlimited);
        assert_eq!(
            unlimited.effective_timeout(Some(Duration::from_secs(300))),
            None
        );

        // Immutable per-call views compare by value and stay Copy.
        assert_eq!(bounded, CallOptions::with_timeout(bounded.timeout()));
        let copied = bounded;
        assert_eq!(copied, bounded);

        let unary = UnaryCallOptions::from(bounded)
            .with_call_id("step-42_ok")
            .unwrap();
        assert_eq!(unary.timeout(), bounded.timeout());
        assert_eq!(unary.call_id(), Some("step-42_ok"));
        assert_eq!(unary.call_options(), bounded);
        assert!(UnaryCallOptions::new().with_call_id("bad/id").is_err());
    }
}
