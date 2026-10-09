//! Caller budget for one connection acquisition, independent of business calls.

use std::fmt;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectOptions {
    timeout: Option<Duration>,
}

impl ConnectOptions {
    pub const fn new() -> Self {
        Self { timeout: None }
    }

    /// Zero expires immediately; omission preserves existing phase guards.
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub const fn timeout(self) -> Option<Duration> {
        self.timeout
    }

    pub fn from_timeout_secs(seconds: Option<f64>) -> Result<Self, ConnectTimeoutError> {
        match seconds {
            None => Ok(Self::new()),
            Some(seconds) if !seconds.is_finite() || seconds < 0.0 => Err(ConnectTimeoutError),
            Some(seconds) => Duration::try_from_secs_f64(seconds)
                .map(|duration| Self::new().with_timeout(duration))
                .map_err(|_| ConnectTimeoutError),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectTimeoutError;

impl fmt::Display for ConnectTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connect timeout must be finite, non-negative and representable as a monotonic deadline")
    }
}
impl std::error::Error for ConnectTimeoutError {}

/// Computed once by Core and copied unchanged through every acquisition stage.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectDeadline(Option<Instant>);

impl ConnectDeadline {
    pub fn start(options: ConnectOptions) -> Result<Self, ConnectTimeoutError> {
        let now = Instant::now();
        options
            .timeout
            .map(|duration| now.checked_add(duration).ok_or(ConnectTimeoutError))
            .transpose()
            .map(Self)
    }

    pub const fn instant(self) -> Option<Instant> {
        self.0
    }

    pub fn check(self, stage: &'static str) -> Result<(), ConnectDeadlineExceeded> {
        self.remaining(stage).map(|_| ())
    }

    pub fn remaining(
        self,
        stage: &'static str,
    ) -> Result<Option<Duration>, ConnectDeadlineExceeded> {
        match self.0 {
            None => Ok(None),
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .map(Some)
                .ok_or(ConnectDeadlineExceeded { stage }),
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectDeadlineExceeded {
    pub stage: &'static str,
}

impl fmt::Display for ConnectDeadlineExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connect deadline exceeded during {}", self.stage)
    }
}
impl std::error::Error for ConnectDeadlineExceeded {}
