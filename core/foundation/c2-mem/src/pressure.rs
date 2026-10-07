//! Creation-time OS-memory pressure decisions for owner pools.
//!
//! [`PressureEngine`] is the single heuristic that decides whether creating
//! one more shared-memory backing is admissible right now. It is consulted
//! only at the backing-creation seams — buddy segment creation and dedicated
//! backing creation — with the validated total mapped bytes from the one
//! checked sizing helper per backing. Reusing blocks inside an already-mapped
//! segment never consults the engine and never takes a new charge.
//!
//! The heuristic is deliberately simple and explicit:
//!
//! - `spill_threshold >= 1.0` disables it entirely: every creation attempt is
//!   admitted (finite budget cells remain enforced regardless).
//! - `spill_threshold <= 0.0` (or non-finite) forces a conservative denial of
//!   every new shared-memory backing: `alloc_handle` then takes file backing,
//!   SHM-only entry points return an error, and no OS sample is taken.
//! - Otherwise a candidate of `bytes` is denied when
//!   `bytes > observed_available * spill_threshold`. The observation is one
//!   cached OS snapshot per owner pool, refreshed at most once per
//!   [`SAMPLE_TTL`], so repeated allocations do not syscall per allocation.
//! - Recovery band: after a creation of `B` bytes is denied, the pool is in
//!   the pressure state for backings of that magnitude. Candidates at least
//!   as large as the largest denied backing re-admit only when they fit
//!   within `spill_threshold * RECOVERY_FACTOR` of observed availability —
//!   observed availability must exceed the direct requirement by 25% — and
//!   any such admission ends the pressure state. Strictly smaller candidates
//!   keep the direct bar, so a pressure-denied full buddy segment never
//!   suppresses an eligible smaller dedicated backing: the band is keyed to
//!   the denied backing's size, never a pool-global bit.
//!
//! A failed availability query reports zero bytes, which denies every
//! positive candidate conservatively. The engine never reads or writes
//! payload bytes, never reserves budget, and never creates a mapping; it only
//! advises the seams that already own those mechanisms.

use std::time::{Duration, Instant};

/// How long one OS availability observation serves repeated creation
/// decisions before a fresh sample is taken.
const SAMPLE_TTL: Duration = Duration::from_secs(1);

/// Recovery hysteresis factor applied to the configured threshold while the
/// pool is in the pressure state.
const RECOVERY_FACTOR: f64 = 0.8;

/// Deterministic availability and clock sources for tests.
pub(crate) struct PressureHooks {
    pub available: Box<dyn Fn() -> u64 + Send + Sync>,
    pub now: Box<dyn Fn() -> Instant + Send + Sync>,
}

#[cfg(test)]
impl PressureHooks {
    /// Hooks over shared scripted atomics: availability bytes, a sample
    /// counter incremented on every availability read, and a monotonic test
    /// clock advanced in milliseconds.
    pub(crate) fn scripted(
        available: std::sync::Arc<std::sync::atomic::AtomicU64>,
        samples: std::sync::Arc<std::sync::atomic::AtomicU64>,
        clock_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        use std::sync::atomic::Ordering;
        let start = Instant::now();
        let samples_clock = std::sync::Arc::clone(&clock_ms);
        Self {
            available: Box::new(move || {
                samples.fetch_add(1, Ordering::SeqCst);
                available.load(Ordering::SeqCst)
            }),
            now: Box::new(move || {
                start + Duration::from_millis(samples_clock.load(Ordering::SeqCst))
            }),
        }
    }
}

/// Per-owner OS-memory pressure state.
pub(crate) struct PressureEngine {
    threshold: f64,
    /// Cached `(available bytes, observed at)`; refreshed when older than
    /// [`SAMPLE_TTL`].
    observation: Option<(u64, Instant)>,
    /// While set: the largest pressure-denied backing size since recovery.
    /// Candidates at least this large face the stricter recovery bar; strictly
    /// smaller candidates keep the direct bar.
    pressured_for: Option<u64>,
    denials: u64,
    hooks: Option<PressureHooks>,
}

/// Outcome of one candidate evaluation.
pub(crate) enum PressureDecision {
    Admit,
    Deny { reason: String },
}

impl PressureEngine {
    pub(crate) fn new(threshold: f64) -> Self {
        Self {
            threshold,
            observation: None,
            pressured_for: None,
            denials: 0,
            hooks: None,
        }
    }

    /// Total pressure denials observed by this owner pool.
    pub(crate) fn denials(&self) -> u64 {
        self.denials
    }

    /// Install deterministic availability/clock sources (tests only).
    #[cfg(test)]
    pub(crate) fn install_hooks(&mut self, hooks: PressureHooks) {
        self.hooks = Some(hooks);
        // A new clock invalidates any cached observation age.
        self.observation = None;
    }

    /// Decide whether one candidate backing creation is admissible.
    ///
    /// `candidate_bytes` is the validated total mapped size for the backing
    /// (allocator metadata and page alignment included) and `what` names the
    /// seam for error messages.
    pub(crate) fn evaluate(&mut self, candidate_bytes: u64, what: &str) -> PressureDecision {
        if !self.threshold.is_finite() || self.threshold <= 0.0 {
            // Zero, negative, or non-finite thresholds force the conservative tier:
            // no sample is taken and every positive candidate is denied.
            self.denials = self.denials.saturating_add(1);
            return PressureDecision::Deny {
                reason: format!(
                    "spill threshold {} forces new shared-memory backings away from SHM",
                    self.threshold
                ),
            };
        }
        if self.threshold >= 1.0 {
            return PressureDecision::Admit;
        }
        let available = self.current_available();
        let latched = self.pressured_for;
        let recovering = latched.is_some_and(|denied| candidate_bytes >= denied);
        let effective = if recovering {
            self.threshold * RECOVERY_FACTOR
        } else {
            self.threshold
        };
        if candidate_bytes <= (available as f64 * effective) as u64 {
            if recovering {
                // A backing of the denied magnitude fits again: genuine
                // recovery ends the pressure state. Smaller admissions never
                // clear it — the denied tier stays on the recovery bar.
                self.pressured_for = None;
            }
            PressureDecision::Admit
        } else {
            self.pressured_for = Some(latched.unwrap_or(0).max(candidate_bytes));
            self.denials = self.denials.saturating_add(1);
            let bar_kind = if recovering { "recovery" } else { "threshold" };
            PressureDecision::Deny {
                reason: format!(
                    "OS memory pressure denied a {candidate_bytes}-byte {what}: {} bytes available, {:.0}% {bar_kind} bar",
                    available,
                    effective * 100.0,
                ),
            }
        }
    }

    /// Cached availability, sampled through the installed hook or the raw OS
    /// query at most once per [`SAMPLE_TTL`].
    fn current_available(&mut self) -> u64 {
        let now = self.now();
        if let Some((available, observed_at)) = self.observation {
            if now.duration_since(observed_at) < SAMPLE_TTL {
                return available;
            }
        }
        let available = match &self.hooks {
            Some(hooks) => (hooks.available)(),
            None => crate::spill::available_physical_memory(),
        };
        self.observation = Some((available, now));
        available
    }

    fn now(&self) -> Instant {
        match &self.hooks {
            Some(hooks) => (hooks.now)(),
            None => Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Scripted availability and clock shared with the installed hooks.
    struct Scripted {
        available: Arc<AtomicU64>,
        samples: Arc<AtomicU64>,
        clock: Arc<AtomicU64>,
    }

    impl Scripted {
        fn new(available: u64) -> Self {
            Self {
                available: Arc::new(AtomicU64::new(available)),
                samples: Arc::new(AtomicU64::new(0)),
                clock: Arc::new(AtomicU64::new(0)),
            }
        }

        fn set_available(&self, bytes: u64) {
            self.available.store(bytes, Ordering::SeqCst);
        }

        fn advance_ms(&self, millis: u64) {
            self.clock.fetch_add(millis, Ordering::SeqCst);
        }

        fn samples(&self) -> u64 {
            self.samples.load(Ordering::SeqCst)
        }

        fn install(&self, engine: &mut PressureEngine) {
            engine.install_hooks(PressureHooks::scripted(
                Arc::clone(&self.available),
                Arc::clone(&self.samples),
                Arc::clone(&self.clock),
            ));
        }
    }

    fn admits(engine: &mut PressureEngine, bytes: u64) -> bool {
        matches!(
            engine.evaluate(bytes, "test backing"),
            PressureDecision::Admit
        )
    }

    fn deny_reason(engine: &mut PressureEngine, bytes: u64) -> String {
        match engine.evaluate(bytes, "test backing") {
            PressureDecision::Deny { reason } => reason,
            PressureDecision::Admit => panic!("expected a denial"),
        }
    }

    #[test]
    fn threshold_one_or_more_disables_the_heuristic_without_sampling() {
        let script = Scripted::new(0);
        let mut engine = PressureEngine::new(1.0);
        script.install(&mut engine);
        assert!(admits(&mut engine, u64::MAX));
        assert!(admits(&mut engine, u64::MAX));
        assert_eq!(script.samples(), 0, "disabled heuristic never samples");
        assert_eq!(engine.denials(), 0);
    }

    #[test]
    fn zero_negative_and_nonfinite_thresholds_force_denial_without_sampling() {
        for threshold in [0.0, -0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let script = Scripted::new(u64::MAX);
            let mut engine = PressureEngine::new(threshold);
            script.install(&mut engine);
            let reason = deny_reason(&mut engine, 1);
            assert!(reason.contains("forces"), "{reason}");
            assert_eq!(script.samples(), 0, "forced denial never samples");
            assert_eq!(engine.denials(), 1);
        }
    }

    #[test]
    fn candidates_are_judged_against_the_threshold_fraction() {
        let script = Scripted::new(1_000);
        let mut engine = PressureEngine::new(0.5);
        script.install(&mut engine);
        assert!(admits(&mut engine, 500));
        // One byte over the fraction of observed availability is denied.
        let reason = deny_reason(&mut engine, 501);
        assert!(reason.contains("501"), "{reason}");
        assert!(reason.contains("pressure"), "{reason}");
        assert_eq!(engine.denials(), 1);
    }

    #[test]
    fn cached_observation_bounds_os_sampling_to_once_per_ttl() {
        let script = Scripted::new(1_000);
        let mut engine = PressureEngine::new(0.5);
        script.install(&mut engine);

        // Many decisions inside one TTL take exactly one sample.
        for candidate in [100u64, 200, 300, 400, 500] {
            assert!(admits(&mut engine, candidate));
        }
        assert_eq!(script.samples(), 1);

        // After the TTL expires, the next decision takes a fresh sample.
        script.advance_ms(1_001);
        assert!(admits(&mut engine, 500));
        assert_eq!(script.samples(), 2);
    }

    #[test]
    fn a_failed_availability_query_denies_conservatively() {
        let script = Scripted::new(0);
        let mut engine = PressureEngine::new(0.8);
        script.install(&mut engine);
        assert!(deny_reason(&mut engine, 1).contains("pressure"));
    }

    #[test]
    fn recovery_band_prevents_same_tier_threshold_thrash() {
        // Threshold 0.5: a 1000-byte candidate needs 2000 bytes of observed
        // availability directly, or 2500 bytes (1000 / (0.5*0.8)) to be
        // re-admitted after a denial.
        let script = Scripted::new(1_999);
        let mut engine = PressureEngine::new(0.5);
        script.install(&mut engine);
        assert!(!admits(&mut engine, 1_000), "1999 < 2000 denies");

        // Availability rises just past the direct bar (2010 bytes): without
        // the recovery band this would flip back to admitting and oscillate
        // at the threshold on every fresh sample.
        script.advance_ms(1_001);
        script.set_available(2_010);
        assert!(
            !admits(&mut engine, 1_000),
            "2010 passes the direct bar but stays denied inside the recovery band"
        );

        // Still inside the band at 2499 bytes.
        script.advance_ms(1_001);
        script.set_available(2_499);
        assert!(!admits(&mut engine, 1_000));

        // Genuine recovery (2500 bytes) admits and ends the pressure state.
        script.advance_ms(1_001);
        script.set_available(2_500);
        assert!(admits(&mut engine, 1_000));

        // Un-latched again: the direct bar governs the next decision.
        script.advance_ms(1_001);
        script.set_available(2_010);
        assert!(admits(&mut engine, 1_000));
    }

    #[test]
    fn a_latched_denial_never_suppresses_a_smaller_eligible_candidate() {
        // Deny a 1000-byte candidate at 1999 bytes available (T=0.5), then
        // verify strictly smaller candidates keep the direct bar while the
        // denied magnitude stays on the recovery bar.
        let script = Scripted::new(1_999);
        let mut engine = PressureEngine::new(0.5);
        script.install(&mut engine);
        assert!(!admits(&mut engine, 1_000));

        // Availability rises into the band (2100 bytes): the direct bar is
        // 1050, the recovery bar is 840.
        script.advance_ms(1_001);
        script.set_available(2_100);
        assert!(!admits(&mut engine, 1_000), "denied magnitude stays banded");
        // A 900-byte candidate is strictly smaller: direct bar 1050 admits.
        assert!(admits(&mut engine, 900));
        // That smaller admission did not clear the pressure state.
        assert!(!admits(&mut engine, 1_000));
        // 1060 bytes exceeds the direct bar too.
        assert!(!admits(&mut engine, 1_060));
    }

    #[test]
    fn denials_accumulate_across_decisions() {
        let script = Scripted::new(100);
        let mut engine = PressureEngine::new(0.5);
        script.install(&mut engine);
        for _ in 0..3 {
            assert!(!admits(&mut engine, 1_000));
        }
        assert_eq!(engine.denials(), 3);
    }
}
