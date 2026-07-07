//! Error-rate monitors that decide when a link is too degraded to use.
//!
//! Two independent monitors run at different points in the link lifecycle:
//!
//! * [`Aerm`] - the Alignment Error Rate Monitor (Q.703 §6). Active only while
//!   *proving* a link during initial alignment. It counts signal units received
//!   in error and aborts proving once the count reaches a threshold, so a link
//!   that is already lossy never reaches service.
//! * [`Suerm`] - the Signal Unit Error Rate Monitor (Q.703 §10). Active while the
//!   link is *in service*. It is a leaky bucket: `+1` per errored SU, `-1` for
//!   every `D` signal units received, and it declares a link failure when the
//!   count reaches `T`.
//!
//! "In error" means the layer-1 framer's CRC-16 check failed on the received SU.
//! Because this crate does no I/O, the driver is told about an errored SU through
//! [`crate::Mtp2Link`], which forwards it to the active monitor.

/// Default AERM abort threshold for **normal** proving (Q.703 §6.3).
pub const AERM_THRESHOLD_NORMAL: u32 = 4;
/// Default AERM abort threshold for **emergency** proving.
pub const AERM_THRESHOLD_EMERGENCY: u32 = 1;
/// Default SUERM failure threshold `T` (Q.703 §10.2).
pub const SUERM_THRESHOLD: u32 = 64;
/// Default SUERM leak interval `D`: one decrement per this many received SUs.
pub const SUERM_DECREMENT_INTERVAL: u32 = 256;

/// Alignment Error Rate Monitor (Q.703 §6).
///
/// Reset at the start of each proving attempt, incremented for every errored SU
/// seen during proving, and consulted to decide whether the proving period may
/// complete. The threshold differs for normal versus emergency proving.
#[derive(Debug, Clone)]
pub struct Aerm {
    count: u32,
    threshold: u32,
}

impl Aerm {
    /// Create an AERM with the given abort threshold.
    pub fn new(threshold: u32) -> Self {
        Self {
            count: 0,
            threshold,
        }
    }

    /// Reset the counter and set the threshold for a fresh proving attempt
    /// (normal or emergency).
    pub fn restart(&mut self, threshold: u32) {
        self.count = 0;
        self.threshold = threshold;
    }

    /// Record one signal unit received in error during proving.
    ///
    /// Returns `true` once the count has reached the threshold, meaning proving
    /// must be aborted.
    pub fn record_error(&mut self) -> bool {
        self.count = self.count.saturating_add(1);
        self.is_tripped()
    }

    /// The current error count.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The configured abort threshold.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Whether the monitor has reached its abort threshold.
    pub fn is_tripped(&self) -> bool {
        self.count >= self.threshold
    }
}

/// Signal Unit Error Rate Monitor (Q.703 §10).
///
/// A saturating leaky-bucket counter: `+1` for each errored SU, `-1` (never below
/// zero) once per `decrement_interval` signal units received. When the count
/// reaches `threshold`, the link has failed and must be taken out of service.
#[derive(Debug, Clone)]
pub struct Suerm {
    count: u32,
    threshold: u32,
    decrement_interval: u32,
    received_since_leak: u32,
    failed: bool,
}

impl Suerm {
    /// Create a SUERM with failure threshold `T` and leak interval `D`.
    pub fn new(threshold: u32, decrement_interval: u32) -> Self {
        Self {
            count: 0,
            threshold,
            decrement_interval,
            received_since_leak: 0,
            failed: false,
        }
    }

    /// Reset the monitor for a freshly in-service link.
    pub fn reset(&mut self) {
        self.count = 0;
        self.received_since_leak = 0;
        self.failed = false;
    }

    /// Record one received signal unit. `in_error` is `true` when the framer's
    /// CRC check failed. Returns `true` when the failure threshold is reached.
    pub fn record(&mut self, in_error: bool) -> bool {
        self.received_since_leak += 1;
        if in_error {
            self.count = self.count.saturating_add(1);
        }
        if self.received_since_leak >= self.decrement_interval {
            self.received_since_leak = 0;
            self.count = self.count.saturating_sub(1);
        }
        if self.count >= self.threshold {
            self.failed = true;
        }
        self.failed
    }

    /// The current bucket count.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Whether the monitor has declared a link failure.
    pub fn is_failed(&self) -> bool {
        self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aerm_trips_at_normal_threshold() {
        let mut aerm = Aerm::new(AERM_THRESHOLD_NORMAL);
        assert!(!aerm.record_error()); // 1
        assert!(!aerm.record_error()); // 2
        assert!(!aerm.record_error()); // 3
        assert!(aerm.record_error()); // 4 → trips
        assert!(aerm.is_tripped());
        assert_eq!(aerm.count(), 4);
    }

    #[test]
    fn aerm_emergency_trips_on_first_error() {
        let mut aerm = Aerm::new(AERM_THRESHOLD_EMERGENCY);
        assert!(aerm.record_error());
    }

    #[test]
    fn aerm_restart_clears_count_and_threshold() {
        let mut aerm = Aerm::new(AERM_THRESHOLD_NORMAL);
        aerm.record_error();
        aerm.restart(AERM_THRESHOLD_EMERGENCY);
        assert_eq!(aerm.count(), 0);
        assert_eq!(aerm.threshold(), AERM_THRESHOLD_EMERGENCY);
    }

    #[test]
    fn suerm_trips_after_threshold_errors() {
        let mut suerm = Suerm::new(SUERM_THRESHOLD, SUERM_DECREMENT_INTERVAL);
        // 63 consecutive errored SUs: not failed (leak interval not reached).
        for _ in 0..63 {
            assert!(!suerm.record(true));
        }
        assert!(suerm.record(true)); // 64th error → failure
        assert!(suerm.is_failed());
    }

    #[test]
    fn suerm_leaks_one_per_interval() {
        let mut suerm = Suerm::new(SUERM_THRESHOLD, 4); // small D for the test
        assert!(!suerm.record(true)); // count 1, received 1
        assert!(!suerm.record(false)); // count 1, received 2
        assert!(!suerm.record(false)); // count 1, received 3
        assert!(!suerm.record(false)); // received 4 → leak → count 0
        assert_eq!(suerm.count(), 0);
    }

    #[test]
    fn suerm_error_rate_below_leak_never_trips() {
        let mut suerm = Suerm::new(8, 4);
        // One error then three good SUs, repeated: each error is leaked away.
        for _ in 0..1000 {
            assert!(!suerm.record(true));
            assert!(!suerm.record(false));
            assert!(!suerm.record(false));
            assert!(!suerm.record(false));
        }
        assert!(!suerm.is_failed());
    }

    #[test]
    fn suerm_reset_clears_failure() {
        let mut suerm = Suerm::new(2, 256);
        suerm.record(true);
        suerm.record(true);
        assert!(suerm.is_failed());
        suerm.reset();
        assert!(!suerm.is_failed());
        assert_eq!(suerm.count(), 0);
    }
}
