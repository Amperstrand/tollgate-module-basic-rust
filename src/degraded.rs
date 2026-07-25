//! Degraded mode — "service unavailable" responses when all mints are down.
//!
//! A simple state wrapper that provides formatted messages for the degraded
//! merchant path. The actual upgrade logic (checking mints, swapping
//! merchant implementations) lives in main.rs.
//!
//! Ported from Go `merchant/merchant_degraded.go`.

use std::time::Instant;

/// State wrapper for degraded mode operation.
///
/// When all mints are unreachable at startup, the binary enters degraded mode.
/// It serves these formatted messages to clients while health checks continue
/// in the background. Once a mint recovers, `main.rs` upgrades to full mode.
pub struct DegradedState {
    #[allow(dead_code)]
    reason: String,
    started_at: Instant,
}

impl DegradedState {
    /// Create a new degraded state with the given reason.
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
            started_at: Instant::now(),
        }
    }

    /// How long the binary has been in degraded mode.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Generate the message for a kind 21023 payment rejection notice.
    ///
    /// Matches Go `PurchaseSession` degraded message:
    /// "TollGate is initializing. No reachable mints. Please try again in a few minutes."
    pub fn rejection_message(&self) -> String {
        "TollGate is initializing. No reachable mints. Please try again in a few minutes."
            .to_string()
    }

    /// Generate the message for a kind 21023 advertisement notice.
    ///
    /// Matches Go `GetAdvertisement` degraded message:
    /// "TollGate is initializing. No reachable mints detected. Service will auto-recover."
    pub fn advertisement_message(&self) -> String {
        "TollGate is initializing. No reachable mints detected. Service will auto-recover."
            .to_string()
    }

    /// Check if we should try to upgrade from degraded mode.
    ///
    /// Always returns `true` — the caller (main.rs) is responsible for
    /// checking actual mint reachability before upgrading.
    pub fn should_try_upgrade(&self) -> bool {
        true
    }
}

impl Default for DegradedState {
    fn default() -> Self {
        Self::new("all mints unreachable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rejection_message_contains_initializing() {
        let deg = DegradedState::new("startup");
        let msg = deg.rejection_message();

        assert!(
            msg.contains("initializing"),
            "rejection message should contain 'initializing', got: {msg}"
        );
        assert!(
            msg.contains("No reachable mints"),
            "rejection message should mention no reachable mints, got: {msg}"
        );
        assert!(
            msg.contains("try again"),
            "rejection message should advise to try again, got: {msg}"
        );
    }

    #[test]
    fn test_advertisement_message_contains_recover() {
        let deg = DegradedState::new("startup");
        let msg = deg.advertisement_message();

        assert!(
            msg.contains("recover"),
            "advertisement message should contain 'recover', got: {msg}"
        );
        assert!(
            msg.contains("initializing"),
            "advertisement message should contain 'initializing', got: {msg}"
        );
        assert!(
            msg.contains("auto-recover"),
            "advertisement message should mention auto-recover, got: {msg}"
        );
    }

    #[test]
    fn test_should_try_upgrade_always_true() {
        let deg = DegradedState::new("startup");
        assert!(deg.should_try_upgrade());
    }

    #[test]
    fn test_elapsed_increases() {
        let deg = DegradedState::new("startup");
        let e1 = deg.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let e2 = deg.elapsed();
        assert!(e2 > e1, "elapsed time should increase");
    }
}
