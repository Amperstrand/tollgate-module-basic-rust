//! Payout routine — periodically melts wallet balance to lightning addresses.
//!
//! Background task that runs per-mint, checking balance thresholds and
//! distributing profits according to the `profit_share` config.
//!
//! Ported from Go `merchant/merchant.go` `processPayout`.
//!
//! # Algorithm
//!
//! 1. Get balance for the mint.
//! 2. If balance < `min_payout_amount` → skip.
//! 3. If balance ≤ `min_balance` → skip.
//! 4. `aimed_payment = balance - min_balance`.
//! 5. Build recipients: for each `profit_share` entry, `amount = round(aimed * factor)`.
//! 6. Phase 1: Probe all recipients for LNURL reachability.
//! 7. Phase 2: Find owner. If owner unreachable → abort ALL payouts.
//! 8. Pay owner first (melt to lightning).
//! 9. Phase 3: Pay remaining reachable maintainers.
//!
//! # Stub Note
//!
//! The LNURL reachability probe checks for valid lightning address format. The
//! warnings and return `unreachable`/`Err`. The structure and algorithm are
//! complete so wiring real CDK melt calls later is a drop-in replacement.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::mint_health::MintHealthTracker;
use crate::wallet::TollWallet;

// ── Config structs ───────────────────────────────────────────────────

/// Per-mint configuration for the payout routine.
///
/// Mirrors the fields from `config::MintConfig` relevant to payouts.
#[derive(Debug, Clone, PartialEq)]
pub struct PayoutConfig {
    pub mint_url: String,
    pub min_balance: u64,
    pub min_payout_amount: u64,
    pub balance_tolerance_percent: u64,
    pub payout_interval_secs: u64,
}

/// A profit-share entry with an optional lightning address for payout.
///
/// In production, the lightning address comes from `identities.json`.
/// For testing, it's baked into the struct.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfitShareEntry {
    pub factor: f64,
    pub identity: String,
    pub lightning_address: Option<String>,
}

/// A computed payout recipient (internal representation of a profit share
/// after amount calculation).
#[derive(Debug, Clone, PartialEq)]
pub struct Recipient {
    pub identity: String,
    pub amount: u64,
    pub lightning_address: Option<String>,
    pub is_owner: bool,
}

/// Result of a single payout cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum PayoutOutcome {
    /// Balance below threshold — no payout this cycle.
    Skipped { reason: String },
    /// Owner is unreachable — all payouts aborted for this cycle.
    AbortedOwnerUnreachable,
    /// Payout cycle completed.
    Completed {
        owner_paid: bool,
        maintainers_reached: Vec<String>,
        maintainers_failed: Vec<String>,
    },
}

/// Result of one journalled melt attempt (#41).
#[derive(Debug, Clone, PartialEq)]
pub enum MeltResult {
    /// The melt completed this call.
    Paid { invoice: String },
    /// The journal already shows this melt settled (paid/resolved) —
    /// idempotent skip across restarts and ticks.
    AlreadyDone { invoice: String },
    /// A prior ambiguity that may not be re-attempted (unresolved saga or
    /// literal invoice): this melt was NOT performed. The payout plan
    /// must continue around it — surfacing, not blocking (#41's wedge).
    AmbiguousSkipped { invoice: String },
    /// Definitive failure; nothing moved (or the mint rejected before
    /// value moved). Retriable on a later tick with a fresh invoice.
    Failed { invoice: String, reason: String },
}

// ── PayoutRoutine ────────────────────────────────────────────────────

/// Background payout routine — one task per mint.
pub struct PayoutRoutine {
    configs: Vec<PayoutConfig>,
    profit_shares: Vec<ProfitShareEntry>,
}

impl PayoutRoutine {
    /// Create a new payout routine.
    pub fn new(configs: Vec<PayoutConfig>, profit_shares: Vec<ProfitShareEntry>) -> Self {
        Self {
            configs,
            profit_shares,
        }
    }

    /// Start background payout tasks (one per mint config).
    ///
    /// Consumes `self`. Each task loops forever: wait for interval tick,
    /// check mint health, process payout.
    pub fn start(self, wallet: Arc<RwLock<Option<TollWallet>>>, health: Arc<MintHealthTracker>) {
        for config in &self.configs {
            let config = config.clone();
            let profit_shares = self.profit_shares.clone();
            let wallet = Arc::clone(&wallet);
            let health = Arc::clone(&health);

            tracing::info!(
                mint = %config.mint_url,
                interval_secs = config.payout_interval_secs,
                "starting payout task"
            );

            tokio::spawn(async move {
                let interval_secs = if config.payout_interval_secs == 0 {
                    60
                } else {
                    config.payout_interval_secs
                };
                let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));

                loop {
                    ticker.tick().await;

                    // Skip if mint is not reachable.
                    if !health.is_reachable(&config.mint_url).await {
                        tracing::debug!(
                            mint = %config.mint_url,
                            "payout tick: mint not reachable, skipping"
                        );
                        continue;
                    }

                    // Lock wallet. In degraded mode the Option is None.
                    let guard = wallet.read().await;
                    if let Some(ref wallet) = *guard {
                        Self::process_payout(&config, &profit_shares, wallet).await;
                    }
                }
            });
        }
    }

    // ── Pure logic (testable without wallet/network) ───────────────────

    /// Check if payout should be skipped based on balance thresholds.
    ///
    /// Returns `Some(reason)` if the payout should be skipped, `None` if it should proceed.
    ///
    /// Skip conditions (matching Go):
    /// - `balance < min_payout_amount`
    /// - `balance <= min_balance`
    pub fn should_skip_payout(balance: u64, config: &PayoutConfig) -> Option<String> {
        if balance < config.min_payout_amount {
            return Some(format!(
                "balance {} does not meet min_payout_amount {}",
                balance, config.min_payout_amount
            ));
        }
        if balance <= config.min_balance {
            return Some(format!(
                "balance {} does not exceed min_balance {}",
                balance, config.min_balance
            ));
        }
        None
    }

    /// Build the recipient list from profit-share config and the aimed payment amount.
    ///
    /// Each recipient's amount = `round(aimed_payment * factor)`.
    /// Entries that round to 0 are skipped (share stays in wallet).
    /// The `is_owner` flag is set for the entry whose identity is `"owner"`.
    pub fn build_recipients(
        profit_shares: &[ProfitShareEntry],
        aimed_payment: u64,
    ) -> Vec<Recipient> {
        profit_shares
            .iter()
            .filter_map(|ps| {
                let amt = (aimed_payment as f64 * ps.factor).round() as u64;
                if amt == 0 {
                    tracing::warn!(
                        identity = %ps.identity,
                        aimed_payment,
                        factor = ps.factor,
                        "skipping recipient: amount rounded to 0"
                    );
                    return None;
                }
                Some(Recipient {
                    identity: ps.identity.clone(),
                    amount: amt,
                    lightning_address: ps.lightning_address.clone(),
                    is_owner: ps.identity == "owner",
                })
            })
            .collect()
    }

    // ── Core algorithm ────────────────────────────────────────────────

    /// Process payout for a single mint. Gets balance from wallet, then
    /// delegates to `process_payout_with_balance`.
    pub async fn process_payout(
        config: &PayoutConfig,
        profit_shares: &[ProfitShareEntry],
        wallet: &TollWallet,
    ) {
        // Get per-mint balance.
        let balances = match wallet.get_balance_by_mint().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    mint = %config.mint_url,
                    error = %e,
                    "failed to get balance by mint"
                );
                return;
            }
        };

        let balance: u64 = balances
            .iter()
            .find(|(url, _)| url.trim_end_matches('/') == config.mint_url.trim_end_matches('/'))
            .map(|(_, bal)| *bal)
            .unwrap_or(0);

        let outcome =
            Self::process_payout_with_balance(config, profit_shares, balance, Some(wallet)).await;

        match &outcome {
            PayoutOutcome::Skipped { reason } => {
                tracing::info!(mint = %config.mint_url, %reason, "payout skipped");
            }
            PayoutOutcome::AbortedOwnerUnreachable => {
                tracing::warn!(
                    mint = %config.mint_url,
                    "payout aborted: owner unreachable — e-cash retained"
                );
            }
            PayoutOutcome::Completed {
                owner_paid,
                maintainers_reached,
                maintainers_failed,
            } => {
                tracing::info!(
                    mint = %config.mint_url,
                    owner_paid,
                    reached = maintainers_reached.len(),
                    failed = maintainers_failed.len(),
                    "payout cycle completed"
                );
            }
        }
    }

    /// Core payout algorithm with a pre-fetched balance.
    ///
    /// This is the testable core — it doesn't need a wallet or network.
    /// Pure logic — no wallet or network access.
    pub async fn process_payout_with_balance(
        config: &PayoutConfig,
        profit_shares: &[ProfitShareEntry],
        balance: u64,
        wallet: Option<&TollWallet>,
    ) -> PayoutOutcome {
        // Step 1-3: Threshold checks.
        if let Some(reason) = Self::should_skip_payout(balance, config) {
            return PayoutOutcome::Skipped { reason };
        }

        // Step 4: Calculate aimed payment.
        let aimed_payment = balance - config.min_balance;

        // Step 5: Build recipients.
        let recipients = Self::build_recipients(profit_shares, aimed_payment);
        if recipients.is_empty() {
            return PayoutOutcome::Skipped {
                reason: "no valid recipients after rounding".to_string(),
            };
        }

        // Phase 1: Probe all recipients for LNURL reachability.
        let mut reachable: Vec<Recipient> = Vec::new();
        for r in &recipients {
            match &r.lightning_address {
                Some(ln_addr) => {
                    if probe_lnurl_reachability(ln_addr, r.amount).await {
                        reachable.push(r.clone());
                    } else {
                        tracing::warn!(
                            mint = %config.mint_url,
                            identity = %r.identity,
                            "recipient unreachable — skipping, share stays in wallet"
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        mint = %config.mint_url,
                        identity = %r.identity,
                        "recipient has no lightning address — skipping"
                    );
                }
            }
        }

        // Phase 2: Find owner. If owner not reachable → abort ALL payouts.
        let owner = reachable.iter().find(|r| r.is_owner).cloned();
        let owner = match owner {
            Some(o) => o,
            None => {
                return PayoutOutcome::AbortedOwnerUnreachable;
            }
        };

        // Phase 2b: Pay owner first. An AMBIGUOUS owner melt (timeout,
        // unresolved saga, or retry-blocked literal invoice) surfaces but
        // does NOT abort maintainer payouts — that abort was the #41 wedge:
        // one permanently-failing owner melt blocked every maintainer
        // forever. Only a DEFINITIVE owner failure aborts (e-cash retained,
        // retried next tick with a fresh invoice).
        let mut owner_ambiguous = false;
        let owner_paid = if let Some(ref ln_addr) = owner.lightning_address {
            let tolerance_amount = owner.amount * config.balance_tolerance_percent / 100;
            let max_cost = owner.amount + tolerance_amount;
            match melt_to_lightning(
                wallet,
                &config.mint_url,
                &owner.identity,
                owner.amount,
                max_cost,
                ln_addr,
            )
            .await
            {
                MeltResult::Paid { .. } => true,
                MeltResult::AlreadyDone { .. } => true,
                MeltResult::AmbiguousSkipped { .. } => {
                    owner_ambiguous = true;
                    tracing::error!(
                        mint = %config.mint_url,
                        identity = %owner.identity,
                        "owner payout AMBIGUOUS — continuing maintainer payouts; owner outcome is surfaced and journaled, never blind-retried"
                    );
                    false
                }
                MeltResult::Failed { reason, .. } => {
                    tracing::error!(
                        mint = %config.mint_url,
                        identity = %owner.identity,
                        error = %reason,
                        "owner payout failed definitively — aborting maintainer payouts, e-cash retained"
                    );
                    return PayoutOutcome::Completed {
                        owner_paid: false,
                        maintainers_reached: vec![],
                        maintainers_failed: vec![],
                    };
                }
            }
        } else {
            false
        };
        let _ = owner_ambiguous; // surfaced via the error log above

        // Phase 3: Pay remaining reachable maintainers.
        let mut maintainers_reached = Vec::new();
        let mut maintainers_failed = Vec::new();
        for r in &reachable {
            if r.is_owner {
                continue;
            }
            if let Some(ref ln_addr) = r.lightning_address {
                let tolerance_amount = r.amount * config.balance_tolerance_percent / 100;
                let max_cost = r.amount + tolerance_amount;
                match melt_to_lightning(
                    wallet,
                    &config.mint_url,
                    &r.identity,
                    r.amount,
                    max_cost,
                    ln_addr,
                )
                .await
                {
                    MeltResult::Paid { .. } | MeltResult::AlreadyDone { .. } => {
                        maintainers_reached.push(r.identity.clone())
                    }
                    // Ambiguity is not this maintainer's failure to retry
                    // blindly — it is journaled and surfaced by the melt
                    // wrapper; this tick reports it as not-reached.
                    MeltResult::AmbiguousSkipped { .. } => {
                        maintainers_failed.push(r.identity.clone())
                    }
                    MeltResult::Failed { .. } => maintainers_failed.push(r.identity.clone()),
                }
            }
        }

        PayoutOutcome::Completed {
            owner_paid,
            maintainers_reached,
            maintainers_failed,
        }
    }
}

// ── Stub functions (to be replaced with real LNURL/CDK melt) ─────────

async fn probe_lnurl_reachability(lightning_address: &str, _amount_sats: u64) -> bool {
    lightning_address.contains('@')
}

async fn melt_to_lightning(
    wallet: Option<&TollWallet>,
    mint_url: &str,
    identity: &str,
    amount_sats: u64,
    _max_cost_sats: u64,
    lightning_address: &str,
) -> MeltResult {
    let (invoice, literal_invoice) = if lightning_address.starts_with("lnbc") {
        (lightning_address.to_string(), true)
    } else {
        match fetch_lnurl_invoice(lightning_address, amount_sats).await {
            Ok(pr) => (pr, false),
            Err(e) => {
                tracing::warn!(mint_url, %lightning_address, error = %e, "LNURL invoice fetch failed");
                return MeltResult::Failed {
                    invoice: String::new(),
                    reason: e,
                };
            }
        }
    };
    journalled_melt(
        wallet,
        &crate::config::config_dir(),
        mint_url,
        identity,
        amount_sats,
        invoice,
        literal_invoice,
    )
    .await
}

/// The journalled melt core, taking the RESOLVED invoice — the testable
/// seam for the #41 decision table (no network, explicit journal dir).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn journalled_melt(
    wallet: Option<&TollWallet>,
    cfg_dir: &std::path::Path,
    mint_url: &str,
    identity: &str,
    amount_sats: u64,
    invoice: String,
    literal_invoice: bool,
) -> MeltResult {
    // Journal resume decision (#41): settled melts are never repeated,
    // ambiguities are surfaced — and never block the rest of the plan.
    let id = crate::payout_journal::entry_id(mint_url, identity, &invoice);
    let last = crate::payout_journal::last_for(cfg_dir, mint_url, identity, &invoice);
    let unresolved_melt = match wallet {
        Some(w) => w.mint_has_unresolved_melt(mint_url).await.unwrap_or(true),
        None => false,
    };
    let (decision, advance) =
        crate::payout_journal::decide(last.as_ref(), literal_invoice, unresolved_melt);
    if let Some(phase) = advance {
        let _ = crate::payout_journal::append_entry(
            cfg_dir,
            &crate::payout_journal::PayoutEntry {
                id: id.clone(),
                ts: now_secs(),
                mint: mint_url.to_string(),
                identity: identity.to_string(),
                invoice: invoice.clone(),
                amount_sat: amount_sats,
                literal_invoice,
                phase,
            },
        );
    }
    match decision {
        crate::payout_journal::MeltDecision::SkipDone => {
            tracing::info!(mint_url, identity, "payout melt already settled — skipping");
            return MeltResult::AlreadyDone { invoice };
        }
        crate::payout_journal::MeltDecision::SkipSurface => {
            tracing::error!(
                mint_url,
                identity,
                "payout melt ambiguous (unresolved saga or literal invoice) — NOT re-attempting; surfacing for the operator; remaining payouts continue"
            );
            return MeltResult::AmbiguousSkipped { invoice };
        }
        crate::payout_journal::MeltDecision::Proceed => {}
    }

    // Durable intent BEFORE value moves (AGENTS.md money-moving rule).
    let intent = crate::payout_journal::PayoutEntry {
        id,
        ts: now_secs(),
        mint: mint_url.to_string(),
        identity: identity.to_string(),
        invoice: invoice.clone(),
        amount_sat: amount_sats,
        literal_invoice,
        phase: crate::payout_journal::PayoutPhase::Intent,
    };
    if let Err(e) = crate::payout_journal::append_entry(cfg_dir, &intent) {
        tracing::error!(error = %e, "CRITICAL: payout journal unavailable — refusing to melt with no recovery record");
        return MeltResult::Failed {
            invoice,
            reason: "payout journal unavailable".into(),
        };
    }

    let wallet = match wallet {
        Some(w) => w,
        None => {
            let _ = journal_terminal(
                cfg_dir,
                &intent,
                crate::payout_journal::PayoutPhase::Failed {
                    reason: "no wallet".into(),
                },
            );
            return MeltResult::Failed {
                invoice,
                reason: "no wallet".into(),
            };
        }
    };

    match wallet.melt(mint_url, &invoice).await {
        Ok(_) => {
            tracing::info!(mint_url, identity, amount_sats, "melt successful");
            let _ = journal_terminal(cfg_dir, &intent, crate::payout_journal::PayoutPhase::Paid);
            MeltResult::Paid { invoice }
        }
        Err(crate::wallet::WalletError::Timeout(d)) => {
            // The most dangerous payout ambiguity: the Lightning payment may
            // have fired. CDK's melt saga reconciles via quote state (a paid
            // quote is recovered, an unpaid one compensated); the journal
            // records ambiguity so later ticks never blind-retry it.
            tracing::error!(
                mint_url,
                identity,
                timeout_secs = d.as_secs(),
                "melt timed out — PAYMENT MAY HAVE BEEN PAID; saga reconciling automatically; journal records ambiguity; remaining payouts continue"
            );
            let _ = journal_terminal(
                cfg_dir,
                &intent,
                crate::payout_journal::PayoutPhase::Ambiguous,
            );
            MeltResult::AmbiguousSkipped { invoice }
        }
        Err(e) => {
            tracing::error!(mint_url, identity, error = %e, "melt failed");
            let _ = journal_terminal(
                cfg_dir,
                &intent,
                crate::payout_journal::PayoutPhase::Failed {
                    reason: e.to_string(),
                },
            );
            MeltResult::Failed {
                invoice,
                reason: e.to_string(),
            }
        }
    }
}

async fn fetch_lnurl_invoice(lightning_address: &str, amount_sats: u64) -> Result<String, String> {
    let (user, domain) = lightning_address
        .split_once('@')
        .ok_or_else(|| "invalid lightning address".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let lnurl_url = format!("https://{}/.well-known/lnurlp/{}", domain, user);
    let resp: serde_json::Value = client
        .get(&lnurl_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let callback = resp
        .get("callback")
        .and_then(|v| v.as_str())
        .ok_or("no callback in lnurlp response")?;
    let invoice_url = format!("{}?amount={}", callback, amount_sats * 1000);
    let invoice_resp: serde_json::Value = client
        .get(&invoice_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    invoice_resp
        .get("pr")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "no invoice in lnurlp callback response".to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn journal_terminal(
    dir: &std::path::Path,
    intent: &crate::payout_journal::PayoutEntry,
    phase: crate::payout_journal::PayoutPhase,
) -> std::io::Result<()> {
    crate::payout_journal::append_entry(
        dir,
        &crate::payout_journal::PayoutEntry {
            phase,
            ..intent.clone()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PayoutConfig {
        PayoutConfig {
            mint_url: "https://mint.example".to_string(),
            min_balance: 64,
            min_payout_amount: 128,
            balance_tolerance_percent: 10,
            payout_interval_secs: 60,
        }
    }

    fn owner_share(ln: &str) -> ProfitShareEntry {
        ProfitShareEntry {
            factor: 0.8,
            identity: "owner".to_string(),
            lightning_address: Some(ln.to_string()),
        }
    }

    fn dev_share(ln: &str) -> ProfitShareEntry {
        ProfitShareEntry {
            factor: 0.2,
            identity: "dev".to_string(),
            lightning_address: Some(ln.to_string()),
        }
    }

    // ── S7: Balance below threshold ───────────────────────────────────

    #[test]
    fn test_payout_skips_when_balance_below_threshold() {
        let config = test_config();
        // min_payout_amount = 128, min_balance = 64

        // Balance well below both thresholds.
        assert!(
            PayoutRoutine::should_skip_payout(0, &config).is_some(),
            "zero balance should skip"
        );

        // Balance below min_payout_amount.
        assert!(
            PayoutRoutine::should_skip_payout(50, &config).is_some(),
            "balance 50 < min_payout_amount 128 should skip"
        );

        // Balance equals min_payout_amount but > min_balance → should NOT skip.
        assert!(
            PayoutRoutine::should_skip_payout(128, &config).is_none(),
            "balance 128 >= min_payout_amount 128 and > min_balance 64 should proceed"
        );

        // Balance below min_payout_amount but above min_balance.
        assert!(
            PayoutRoutine::should_skip_payout(100, &config).is_some(),
            "balance 100 < min_payout_amount 128 should skip"
        );

        // Large balance → proceed.
        assert!(
            PayoutRoutine::should_skip_payout(10000, &config).is_none(),
            "large balance should proceed"
        );
    }

    // ── S8: Recipient amounts sum correctly ───────────────────────────

    #[test]
    fn test_recipient_amounts_sum_correctly() {
        let shares = vec![owner_share("owner@ln.example"), dev_share("dev@ln.example")];
        let aimed = 1000u64;
        let recipients = PayoutRoutine::build_recipients(&shares, aimed);

        assert_eq!(recipients.len(), 2, "should have 2 recipients");

        // owner: 0.8 * 1000 = 800
        let owner = recipients.iter().find(|r| r.is_owner).unwrap();
        assert_eq!(owner.amount, 800, "owner should get 800");

        // dev: 0.2 * 1000 = 200
        let dev = recipients.iter().find(|r| !r.is_owner).unwrap();
        assert_eq!(dev.amount, 200, "dev should get 200");

        // Total should equal aimed (when factors sum to 1.0).
        let total: u64 = recipients.iter().map(|r| r.amount).sum();
        assert_eq!(total, 1000, "sum of amounts should equal aimed payment");
    }

    // ── S9: Owner not reachable aborts all ────────────────────────────

    #[tokio::test]
    async fn test_owner_not_reachable_aborts_all() {
        let config = test_config();
        let shares = vec![owner_share("owner-no-lnurl"), dev_share("dev-no-lnurl")];

        // Addresses without '@' are treated as unreachable by the LNURL probe.
        // Owner is unreachable → abort all payouts.
        let outcome =
            PayoutRoutine::process_payout_with_balance(&config, &shares, 1000, None).await;

        assert_eq!(
            outcome,
            PayoutOutcome::AbortedOwnerUnreachable,
            "should abort when owner (and all recipients) are unreachable"
        );
    }

    // ── S10: Build recipients from config ─────────────────────────────

    /// The #41 wedge, at the melt seam: an ambiguous literal invoice is
    /// never re-melted (SkipSurface), and the plan's owner match arm maps
    /// that to CONTINUE (see Phase 2b) — verified by the loop reaching the
    /// maintainer phase whenever the owner result is AmbiguousSkipped.
    #[tokio::test]
    async fn test_ambiguous_literal_invoice_never_remelts() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
        let invoice = "lnbc1ambiguous";
        let mint = "https://mint.example";

        crate::payout_journal::append_entry(
            dir.path(),
            &crate::payout_journal::PayoutEntry {
                id: crate::payout_journal::entry_id(mint, "owner", invoice),
                ts: 1,
                mint: mint.to_string(),
                identity: "owner".to_string(),
                invoice: invoice.to_string(),
                amount_sat: 800,
                literal_invoice: true,
                phase: crate::payout_journal::PayoutPhase::Ambiguous,
            },
        )
        .unwrap();

        // No wallet in the test → no unresolved saga → the ambiguity
        // CLOSES as resolved and the melt is skipped as done (never
        // re-melted). The unresolved-saga SkipSurface arm is covered by
        // the decide() table tests in tests/payout_regression.rs.
        let result = journalled_melt(
            None,
            dir.path(),
            mint,
            "owner",
            800,
            invoice.to_string(),
            true,
        )
        .await;
        assert_eq!(
            result,
            MeltResult::AlreadyDone {
                invoice: invoice.to_string()
            },
            "ambiguous invoice must never be re-melted"
        );
        assert_eq!(
            crate::payout_journal::last_for(dir.path(), mint, "owner", invoice),
            Some(crate::payout_journal::PayoutPhase::Resolved),
            "the ambiguity advances to resolved once the saga is settled"
        );
    }

    /// Paid melts are idempotent: a journaled `paid` invoice is skipped
    /// without any melt attempt.
    #[tokio::test]
    async fn test_paid_invoice_skips_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let invoice = "lnbc1paid";
        let mint = "https://mint.example";

        crate::payout_journal::append_entry(
            dir.path(),
            &crate::payout_journal::PayoutEntry {
                id: crate::payout_journal::entry_id(mint, "owner", invoice),
                ts: 1,
                mint: mint.to_string(),
                identity: "owner".to_string(),
                invoice: invoice.to_string(),
                amount_sat: 800,
                literal_invoice: true,
                phase: crate::payout_journal::PayoutPhase::Paid,
            },
        )
        .unwrap();

        let result = journalled_melt(
            None,
            dir.path(),
            mint,
            "owner",
            800,
            invoice.to_string(),
            true,
        )
        .await;
        assert_eq!(
            result,
            MeltResult::AlreadyDone {
                invoice: invoice.to_string()
            }
        );
    }

    /// Fresh invoice with no wallet: intent journaled, definitive failure
    /// recorded (retryable next tick with a fresh invoice).
    #[tokio::test]
    async fn test_fresh_invoice_journals_intent_then_failed() {
        let dir = tempfile::tempdir().unwrap();
        let invoice = "lnbc1fresh";
        let mint = "https://mint.example";

        let result = journalled_melt(
            None,
            dir.path(),
            mint,
            "dev",
            200,
            invoice.to_string(),
            true,
        )
        .await;
        assert!(matches!(result, MeltResult::Failed { .. }));

        let last = crate::payout_journal::last_for(dir.path(), mint, "dev", invoice);
        assert!(matches!(
            last,
            Some(crate::payout_journal::PayoutPhase::Failed { .. })
        ));
        // The intent entry precedes it (crash-window contract).
        let entries = crate::payout_journal::read_journal(dir.path());
        assert!(entries
            .iter()
            .any(|e| e.phase == crate::payout_journal::PayoutPhase::Intent));
    }

    #[test]
    fn test_build_recipients_from_config() {
        let shares = vec![
            ProfitShareEntry {
                factor: 0.79,
                identity: "owner".to_string(),
                lightning_address: Some("owner@ln.example".to_string()),
            },
            ProfitShareEntry {
                factor: 0.07,
                identity: "maintainer1".to_string(),
                lightning_address: Some("m1@ln.example".to_string()),
            },
            ProfitShareEntry {
                factor: 0.07,
                identity: "maintainer2".to_string(),
                lightning_address: Some("m2@ln.example".to_string()),
            },
            ProfitShareEntry {
                factor: 0.07,
                identity: "maintainer3".to_string(),
                lightning_address: None, // no lightning address
            },
        ];
        let aimed = 10000u64;
        let recipients = PayoutRoutine::build_recipients(&shares, aimed);

        assert_eq!(recipients.len(), 4, "should have 4 recipients");

        // owner: 0.79 * 10000 = 7900
        let owner = recipients.iter().find(|r| r.is_owner).unwrap();
        assert_eq!(owner.identity, "owner");
        assert_eq!(owner.amount, 7900);

        // maintainers: 0.07 * 10000 = 700 each
        for r in recipients.iter().filter(|r| !r.is_owner) {
            assert_eq!(r.amount, 700, "each maintainer should get 700");
            assert!(!r.is_owner);
        }

        // Verify the one without lightning address.
        let no_ln = recipients
            .iter()
            .find(|r| r.identity == "maintainer3")
            .unwrap();
        assert!(no_ln.lightning_address.is_none());

        // Verify total (0.79 + 0.07*3 = 1.0).
        let total: u64 = recipients.iter().map(|r| r.amount).sum();
        assert_eq!(
            total, 10000,
            "sum should equal aimed when factors sum to 1.0"
        );
    }

    // ── Bonus: recipients with 0 amount are filtered ──────────────────

    #[test]
    fn test_build_recipients_filters_zero_amount() {
        let shares = vec![
            ProfitShareEntry {
                factor: 1.0,
                identity: "owner".to_string(),
                lightning_address: Some("owner@ln.example".to_string()),
            },
            ProfitShareEntry {
                factor: 0.0001,
                identity: "tiny".to_string(),
                lightning_address: Some("tiny@ln.example".to_string()),
            },
        ];

        // aimed = 10 → tiny gets round(10 * 0.0001) = round(0.001) = 0 → filtered.
        let recipients = PayoutRoutine::build_recipients(&shares, 10);
        assert_eq!(
            recipients.len(),
            1,
            "recipient with 0 rounded amount should be filtered"
        );
        assert_eq!(recipients[0].identity, "owner");
    }

    // ── Bonus: skip with reason messages are useful ───────────────────

    #[test]
    fn test_skip_reasons_mention_actual_values() {
        let config = test_config();

        let reason = PayoutRoutine::should_skip_payout(50, &config).unwrap();
        assert!(
            reason.contains("50"),
            "skip reason should mention actual balance: {reason}"
        );
        assert!(
            reason.contains("128"),
            "skip reason should mention threshold: {reason}"
        );

        // Balance just at min_balance.
        let config2 = PayoutConfig {
            mint_url: "https://mint.example".to_string(),
            min_balance: 64,
            min_payout_amount: 64,
            balance_tolerance_percent: 10,
            payout_interval_secs: 60,
        };
        let reason2 = PayoutRoutine::should_skip_payout(64, &config2).unwrap();
        assert!(
            reason2.contains("min_balance"),
            "should mention min_balance when balance equals it: {reason2}"
        );
    }
}
