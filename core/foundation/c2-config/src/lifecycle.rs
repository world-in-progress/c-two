//! Server lifecycle policy types with Rust-owned strict validation.
//!
//! The policy decides who may end a local host: `Persistent` keeps the
//! existing explicit-shutdown semantics, while `OwnerBound` ties the host to
//! one controller-held native owner control capability. `c2-core` consumes
//! this policy; the capability itself never appears in configuration data,
//! environment variables, argv, or logs.

use std::fmt;
use std::time::Duration;

/// Upper bound for an `OwnerBound` owner-missing grace window.
///
/// The grace must stay finite so an abandoned owner-bound host always reaches
/// its drain transaction in bounded time. Zero is allowed and means the drain
/// transaction starts immediately after the control EOF.
pub const MAX_OWNER_MISSING_GRACE: Duration = Duration::from_secs(60);

/// Validated server lifecycle policy.
///
/// Naming a policy is never sufficient to start an owner-bound host: the
/// explicit native owner control capability must also be attached to the
/// Runtime and consumed before the host publishes readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerLifecyclePolicy {
    /// Default policy. The host runs until an explicit shutdown; ordinary
    /// business client disconnects never change its lifecycle.
    Persistent,
    /// The host is bound to one controller through the native owner control
    /// capability. When the controller closes, new business admission stops
    /// immediately and, after at most `owner_missing_grace`, the existing
    /// native drain/shutdown transaction runs.
    OwnerBound {
        owner_missing_grace: Duration,
    },
}

impl Default for ServerLifecyclePolicy {
    fn default() -> Self {
        Self::Persistent
    }
}

impl fmt::Display for ServerLifecyclePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistent => formatter.write_str("persistent"),
            Self::OwnerBound {
                owner_missing_grace,
            } => write!(
                formatter,
                "owner_bound(grace={}ms)",
                owner_missing_grace.as_millis()
            ),
        }
    }
}

impl ServerLifecyclePolicy {
    /// Construct the validated `OwnerBound` policy.
    pub fn owner_bound(owner_missing_grace: Duration) -> Result<Self, String> {
        validate_owner_missing_grace(owner_missing_grace)?;
        Ok(Self::OwnerBound {
            owner_missing_grace,
        })
    }

    /// Strict Rust-side validation of the policy values.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Persistent => Ok(()),
            Self::OwnerBound {
                owner_missing_grace,
            } => validate_owner_missing_grace(*owner_missing_grace),
        }
    }

    pub fn is_owner_bound(&self) -> bool {
        matches!(self, Self::OwnerBound { .. })
    }

    /// The configured grace window, only for the `OwnerBound` policy.
    pub fn owner_missing_grace(&self) -> Option<Duration> {
        match self {
            Self::Persistent => None,
            Self::OwnerBound {
                owner_missing_grace,
            } => Some(*owner_missing_grace),
        }
    }
}

/// Validate one owner-missing grace duration against the bounded range.
pub fn validate_owner_missing_grace(grace: Duration) -> Result<(), String> {
    if grace > MAX_OWNER_MISSING_GRACE {
        return Err(format!(
            "owner-missing grace {}ms exceeds the maximum {}ms",
            grace.as_millis(),
            MAX_OWNER_MISSING_GRACE.as_millis()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_is_the_default_and_always_valid() {
        let policy = ServerLifecyclePolicy::default();
        assert_eq!(policy, ServerLifecyclePolicy::Persistent);
        assert!(policy.validate().is_ok());
        assert!(!policy.is_owner_bound());
        assert_eq!(policy.owner_missing_grace(), None);
    }

    #[test]
    fn owner_bound_accepts_only_finite_bounded_grace() {
        assert!(ServerLifecyclePolicy::owner_bound(Duration::ZERO).is_ok());
        assert!(
            ServerLifecyclePolicy::owner_bound(Duration::from_millis(3_000))
                .is_ok()
        );
        let too_large = MAX_OWNER_MISSING_GRACE + Duration::from_millis(1);
        let error =
            ServerLifecyclePolicy::owner_bound(too_large).expect_err("grace must be bounded");
        assert!(error.contains("exceeds the maximum"));
        let policy =
            ServerLifecyclePolicy::OwnerBound {
                owner_missing_grace: too_large,
            };
        assert!(policy.validate().is_err());
    }
}
