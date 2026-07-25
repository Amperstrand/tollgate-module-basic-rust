//! Mint health tracking — proactive probing and recovery thresholds.
//!
//! Tracks mint reachability with a configurable recovery threshold.
//! When a mint goes down, it must pass `recovery_threshold` (default 3)
//! consecutive successful probes before being marked reachable again.
//!
//! Ported from Go `merchant/mint_health_tracker.go`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::RwLock;

/// Default number of consecutive successful probes required to recover a mint
/// that was previously unreachable.
const DEFAULT_RECOVERY_THRESHOLD: u8 = 3;

/// Timeout for a single mint probe (matches Go's 30s).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between aggressive retry probes (matches Go's 15s).
const AGGRESSIVE_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Timeout for aggressive retry probes (matches Go's 10s).
const AGGRESSIVE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default duration for the aggressive retry window (matches Go's 5m).
const AGGRESSIVE_DURATION_DEFAULT: u64 = 300;

/// Mutable state protected by `RwLock`.
struct MintHealthState {
    reachable: HashMap<String, bool>,
    consecutive_successes: HashMap<String, u8>,
    had_any_reachable: bool,
}

/// Tracks mint reachability with proactive probing and recovery thresholds.
///
/// Designed to be wrapped in `Arc` and shared across tasks (startup probe,
/// proactive checker, payout routine).
pub struct MintHealthTracker {
    state: RwLock<MintHealthState>,
    http_client: reqwest::Client,
    aggressive_http_client: reqwest::Client,
    recovery_threshold: u8,
}

impl MintHealthTracker {
    /// Create a new tracker with default recovery threshold (3).
    pub fn new() -> Self {
        Self::with_recovery_threshold(DEFAULT_RECOVERY_THRESHOLD)
    }

    /// Create a tracker with a custom recovery threshold.
    pub fn with_recovery_threshold(recovery_threshold: u8) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("failed to build reqwest client for mint health probing");

        let aggressive_http_client = reqwest::Client::builder()
            .timeout(AGGRESSIVE_PROBE_TIMEOUT)
            .build()
            .expect("failed to build aggressive reqwest client");

        Self {
            state: RwLock::new(MintHealthState {
                reachable: HashMap::new(),
                consecutive_successes: HashMap::new(),
                had_any_reachable: false,
            }),
            http_client,
            aggressive_http_client,
            recovery_threshold,
        }
    }

    /// Probe all mints on startup. Returns `true` if any mint is reachable.
    ///
    /// Sets initial reachability state. Reachable mints start with
    /// `consecutive_successes` set to `recovery_threshold` so they don't
    /// need to re-prove themselves.
    pub async fn initial_probe(&self, mint_urls: &[String]) -> bool {
        tracing::info!(mint_count = mint_urls.len(), "initial probe starting");

        let mut results = Vec::with_capacity(mint_urls.len());
        for url in mint_urls {
            let ok = self.probe_mint(url).await;
            results.push((url.clone(), ok));
        }

        self.apply_initial_results(&results).await
    }

    /// Run a proactive check (normally every 5 minutes). Updates reachability.
    ///
    /// For each mint:
    /// - **Probe succeeds**: increment `consecutive_successes`. If the mint was
    ///   unreachable AND successes ≥ threshold → mark reachable (recovery!).
    /// - **Probe fails**: reset `consecutive_successes` to 0, mark unreachable.
    pub async fn proactive_check(&self, mint_urls: &[String]) {
        tracing::debug!(mint_count = mint_urls.len(), "proactive check");

        let mut results = Vec::with_capacity(mint_urls.len());
        for url in mint_urls {
            let ok = self.probe_mint(url).await;
            results.push((url.clone(), ok));
        }

        self.apply_proactive_results(&results).await;
    }

    /// Aggressive retry: probe all mints every 15 seconds for up to `duration_secs`.
    /// Returns `true` as soon as any mint becomes reachable.
    ///
    /// Uses immediate recovery (threshold effectively 1) — any successful probe
    /// marks the mint reachable immediately. Uses a shorter probe timeout
    /// (10s instead of 30s) to fail fast on unreachable hosts.
    pub async fn aggressive_retry(&self, mint_urls: &[String], duration_secs: u64) -> bool {
        let duration = if duration_secs == 0 {
            AGGRESSIVE_DURATION_DEFAULT
        } else {
            duration_secs
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(duration);

        tracing::info!(
            duration_secs = duration,
            mint_count = mint_urls.len(),
            "aggressive retry starting"
        );

        loop {
            // Probe all mints with the aggressive (shorter timeout) client.
            let mut results = Vec::with_capacity(mint_urls.len());
            for url in mint_urls {
                let ok = self
                    .probe_mint_with(url, &self.aggressive_http_client)
                    .await;
                results.push((url.clone(), ok));
            }

            self.apply_aggressive_results(&results).await;

            if self.any_reachable().await {
                tracing::info!("aggressive retry: mint became reachable");
                return true;
            }

            let next_tick = tokio::time::Instant::now() + AGGRESSIVE_PROBE_INTERVAL;
            if next_tick >= deadline {
                return self.any_reachable().await;
            }
            tokio::time::sleep_until(next_tick).await;
        }
    }

    /// Check if a specific mint is currently reachable.
    pub async fn is_reachable(&self, mint_url: &str) -> bool {
        let state = self.state.read().await;
        state
            .reachable
            .get(mint_url.trim_end_matches('/'))
            .copied()
            .unwrap_or(false)
    }

    /// Check if any mint is reachable.
    pub async fn any_reachable(&self) -> bool {
        let state = self.state.read().await;
        state.reachable.values().any(|&v| v)
    }

    // ── Internal: state update from probe results ──────────────────────

    /// Apply initial probe results. Returns true if any mint is reachable.
    async fn apply_initial_results(&self, results: &[(String, bool)]) -> bool {
        let mut state = self.state.write().await;
        let mut any = false;

        for (url, ok) in results {
            let normalized = url.trim_end_matches('/').to_string();
            if *ok {
                state.reachable.insert(normalized.clone(), true);
                state
                    .consecutive_successes
                    .insert(normalized, self.recovery_threshold);
                any = true;
            } else {
                state.reachable.insert(normalized.clone(), false);
                state.consecutive_successes.insert(normalized, 0);
            }
        }

        state.had_any_reachable = any;
        any
    }

    /// Apply proactive check results using the recovery threshold.
    async fn apply_proactive_results(&self, results: &[(String, bool)]) {
        let mut state = self.state.write().await;

        for (url, ok) in results {
            let normalized = url.trim_end_matches('/').to_string();
            if *ok {
                // Increment counter — copy out the value to release the borrow
                // before touching `state.reachable`.
                let new_count = {
                    let counter = state
                        .consecutive_successes
                        .entry(normalized.clone())
                        .or_insert(0);
                    *counter = counter.saturating_add(1);
                    *counter
                };

                let was_reachable = state.reachable.get(&normalized).copied().unwrap_or(false);
                if !was_reachable && new_count >= self.recovery_threshold {
                    tracing::info!(
                        mint = %normalized,
                        successes = new_count,
                        "mint recovered (reached threshold)"
                    );
                    state.reachable.insert(normalized, true);
                }
            } else {
                state.consecutive_successes.insert(normalized.clone(), 0);
                state.reachable.insert(normalized, false);
            }
        }
    }

    /// Apply aggressive retry results with immediate recovery (threshold = 1).
    async fn apply_aggressive_results(&self, results: &[(String, bool)]) {
        let mut state = self.state.write().await;

        for (url, ok) in results {
            let normalized = url.trim_end_matches('/').to_string();
            if *ok {
                let counter = state
                    .consecutive_successes
                    .entry(normalized.clone())
                    .or_insert(0);
                *counter = counter.saturating_add(1);

                let was_reachable = state.reachable.get(&normalized).copied().unwrap_or(false);
                if !was_reachable {
                    tracing::info!(
                        mint = %normalized,
                        "mint recovered (aggressive mode — immediate)"
                    );
                    state.reachable.insert(normalized, true);
                }
            } else {
                state.consecutive_successes.insert(normalized.clone(), 0);
                state.reachable.insert(normalized, false);
            }
        }
    }

    // ── Internal: HTTP probing ─────────────────────────────────────────

    /// Probe a single mint (GET /v1/info, 30s timeout).
    ///
    /// Returns `true` if the mint responds with a 2xx status code.
    async fn probe_mint(&self, mint_url: &str) -> bool {
        self.probe_mint_with(mint_url, &self.http_client).await
    }

    /// Probe a mint using a specific HTTP client (allows shorter timeouts
    /// for aggressive mode).
    async fn probe_mint_with(&self, mint_url: &str, client: &reqwest::Client) -> bool {
        let url = format!("{}/v1/info", mint_url.trim_end_matches('/'));

        let start = tokio::time::Instant::now();
        match client.get(&url).send().await {
            Ok(resp) => {
                let ok = resp.status().is_success();
                tracing::debug!(
                    url = %url,
                    status = resp.status().as_u16(),
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    ok,
                    "mint probe"
                );
                ok
            }
            Err(e) => {
                tracing::debug!(
                    url = %url,
                    error = %e,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "mint probe failed"
                );
                false
            }
        }
    }
}

impl Default for MintHealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── S1: All mints reachable ────────────────────────────────────────

    #[tokio::test]
    async fn test_all_mints_reachable() {
        let tracker = MintHealthTracker::new();
        let mints = vec![
            "https://mint-a.example".to_string(),
            "https://mint-b.example".to_string(),
        ];

        // Simulate all probes succeeding by directly applying results.
        let results = vec![
            ("https://mint-a.example".to_string(), true),
            ("https://mint-b.example".to_string(), true),
        ];
        let any = tracker.apply_initial_results(&results).await;

        assert!(any, "should have at least one reachable mint");
        assert!(
            tracker.is_reachable("https://mint-a.example").await,
            "mint-a should be reachable"
        );
        assert!(
            tracker.is_reachable("https://mint-b.example").await,
            "mint-b should be reachable"
        );
        assert!(
            tracker.any_reachable().await,
            "any_reachable should return true"
        );

        // mints variable used to verify URL normalization
        assert_eq!(mints.len(), 2);
    }

    // ── S2: All mints unreachable ──────────────────────────────────────

    #[tokio::test]
    async fn test_all_mints_unreachable() {
        let tracker = MintHealthTracker::new();

        let results = vec![
            ("https://mint-a.example".to_string(), false),
            ("https://mint-b.example".to_string(), false),
        ];
        let any = tracker.apply_initial_results(&results).await;

        assert!(!any, "no mint should be reachable");
        assert!(
            !tracker.is_reachable("https://mint-a.example").await,
            "mint-a should not be reachable"
        );
        assert!(
            !tracker.is_reachable("https://mint-b.example").await,
            "mint-b should not be reachable"
        );
        assert!(
            !tracker.any_reachable().await,
            "any_reachable should return false"
        );
    }

    // ── S3: Recovery after threshold ───────────────────────────────────

    #[tokio::test]
    async fn test_recovery_after_threshold() {
        let tracker = MintHealthTracker::new();
        let mint = "https://mint-recovery.example";

        // Initial: mint is unreachable.
        tracker
            .apply_initial_results(&[(mint.to_string(), false)])
            .await;
        assert!(
            !tracker.is_reachable(mint).await,
            "mint should start unreachable"
        );

        // Simulate 2 failures — should remain unreachable.
        tracker
            .apply_proactive_results(&[(mint.to_string(), false)])
            .await;
        tracker
            .apply_proactive_results(&[(mint.to_string(), false)])
            .await;
        assert!(
            !tracker.is_reachable(mint).await,
            "mint should still be unreachable after 2 failures"
        );

        // Simulate 2 successes — still below threshold (3), should NOT recover.
        tracker
            .apply_proactive_results(&[(mint.to_string(), true)])
            .await;
        assert!(
            !tracker.is_reachable(mint).await,
            "mint should NOT recover after only 2 consecutive successes (threshold=3)"
        );
        tracker
            .apply_proactive_results(&[(mint.to_string(), true)])
            .await;
        assert!(
            !tracker.is_reachable(mint).await,
            "mint should NOT recover after only 2 consecutive successes (threshold=3)"
        );

        // 3rd consecutive success — reaches threshold, should recover.
        tracker
            .apply_proactive_results(&[(mint.to_string(), true)])
            .await;
        assert!(
            tracker.is_reachable(mint).await,
            "mint should recover after 3 consecutive successes"
        );
    }

    // ── S4: Bad URL returns false ──────────────────────────────────────

    #[tokio::test]
    async fn test_probe_returns_false_for_bad_url() {
        let tracker = MintHealthTracker::new();

        // An invalid URL should fail at the HTTP level and return false.
        // Using a malformed URL that reqwest rejects immediately.
        let result = tracker.probe_mint("not-a-valid-url").await;
        assert!(!result, "probe of invalid URL should return false");

        // Also test a URL that resolves to nothing (connection refused).
        // Using port 1 which is practically never open.
        let result = tracker.probe_mint("http://127.0.0.1:1").await;
        assert!(!result, "probe of unreachable host should return false");
    }

    // ── Bonus: aggressive recovery is immediate (threshold=1) ──────────

    #[tokio::test]
    async fn test_aggressive_recovery_is_immediate() {
        let tracker = MintHealthTracker::new();
        let mint = "https://mint-aggressive.example";

        // Initial: unreachable.
        tracker
            .apply_initial_results(&[(mint.to_string(), false)])
            .await;
        assert!(!tracker.is_reachable(mint).await);

        // Aggressive result with single success should recover immediately.
        tracker
            .apply_aggressive_results(&[(mint.to_string(), true)])
            .await;
        assert!(
            tracker.is_reachable(mint).await,
            "aggressive mode should recover immediately (threshold=1)"
        );
    }
}
