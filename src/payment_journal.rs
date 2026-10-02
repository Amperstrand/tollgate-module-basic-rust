//! Durable payment journal — business-level recovery for payments (#40).
//!
//! AGENTS.md: "Wallet-level atomicity is NOT application-level atomicity …
//! business-level recovery is *this repo's* job." A payment is
//! `receive → session-create → gate → HTTP 200`; any crash or timeout
//! between those steps can leave the customer's token consumed without a
//! session. The migration journal (#29) solved this for first-boot
//! imports; this module is the payments equivalent, plus the startup
//! reconciliation that grants what the customer is owed.
//!
//! Crash-window table (per payment, id = SHA-256 of the token):
//!
//! | Death point | Journal state | Mint state | Reconcile outcome |
//! |---|---|---|---|
//! | before intent append | nothing | untouched | nothing owed |
//! | after intent, before/during receive | `intent` | maybe swap-requested | NUT-07: spent → session granted; unspent → closed, nothing owed |
//! | after receive Ok, before `received` append | `intent` | spent, outputs in wallet | spent → session granted |
//! | after `received`, before/after session | `received` | settled | none needed (session same-MAC overwrite is idempotent) |
//! | timeout, before reconcile | `timeout-unknown` | either | NUT-07 decides: spent → session granted; unspent → closed |
//!
//! The journal stores the token string itself: reconciliation must be able
//! to ask the mint about inputs that may never have reached the wallet's
//! own database. This is the cashu-ts preview-persistence precedent — the
//! file is mode 0600 and lives beside the wallet it protects. Terminal
//! entries are prunable (consolidation TODO with flash-wear data).
//!
//! Wear note: two fsync'd appends per payment (intent + terminal). A
//! payment is a discrete money event, not a loop — same order as the
//! migration journal and `sessions.json` writes, and bounded by the
//! router's payment rate.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wallet::WalletError;

pub const PAYMENT_JOURNAL_NAME: &str = "payment-journal.jsonl";

/// MAC marker for CLI `wallet fund` entries (#46): a receive initiated by
/// the operator, not a customer payment. Real client MACs are hex-colon
/// strings, so this can never collide. Reconciliation advances these like
/// any payment but owes no session/gate — the value lands in the wallet
/// via the same saga recovery; the operator sees it in the balance.
pub const CLI_FUND_MAC: &str = "cli-fund";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum PaymentPhase {
    /// Durable intent recorded before `wallet.receive()`.
    Intent,
    /// Receive completed with this amount (sat); session path ran.
    Received { amount_sat: u64 },
    /// Definitive pre- or post-receive rejection; nothing owed.
    Rejected { reason: String },
    /// Receive timed out — outcome unknown, reconciled later.
    TimeoutUnknown,
    /// Reconciliation proved the token spent at the mint; the owed
    /// session was granted (amount as reported by NUT-07).
    ReconcileSpent { amount_sat: u64 },
    /// Reconciliation proved the token unspent — the customer lost
    /// nothing; the token is still theirs to spend.
    ReconcileUnspent,
    /// Reconciliation proved the token spent but the amount bought zero
    /// steps (fees) — recorded as customer credit material (issue #5).
    ReconcileZeroSteps { amount_sat: u64 },
}

impl PaymentPhase {
    /// True when a later reconciliation pass must decide this payment.
    pub fn needs_reconciliation(&self) -> bool {
        matches!(self, PaymentPhase::Intent | PaymentPhase::TimeoutUnknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentEntry {
    /// SHA-256 of the token (audit/grep key; the raw token rides along
    /// for mint-side reconciliation).
    pub id: String,
    pub ts: u64,
    pub token: String,
    pub mac: String,
    pub mint: String,
    /// All pricing terms frozen at intent time so reconciliation grants
    /// what was paid for even if the operator later changes config
    /// (Codex P2 on #43).
    pub price_per_step: u64,
    pub step_size: u64,
    pub metric: String,
    pub phase: PaymentPhase,
}

pub fn journal_path(dir: &Path) -> PathBuf {
    dir.join(PAYMENT_JOURNAL_NAME)
}

pub fn token_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// Append + fsync. Creates the journal 0600 on first use.
pub fn append_entry(dir: &Path, entry: &PaymentEntry) -> std::io::Result<()> {
    let path = journal_path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // First write establishes the file: tighten mode before the token
    // bytes land (best-effort; the directory is the wallet's own).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    // First-creation durability: the file's directory entry must survive a
    // power cut too, or the intent (and its token) can vanish with the
    // file on reboot (Codex P2 on #43).
    if let Some(parent) = path.parent() {
        if let Ok(dirf) = std::fs::File::open(parent) {
            let _ = dirf.sync_all();
        }
    }
    Ok(())
}

pub fn read_journal(dir: &Path) -> Vec<PaymentEntry> {
    let Ok(file) = std::fs::File::open(journal_path(dir)) else {
        return Vec::new();
    };
    std::io::BufRead::lines(&mut std::io::BufReader::new(file))
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Last phase per payment id (append-only journal, last-write-wins).
pub fn fold_last(entries: &[PaymentEntry]) -> HashMap<String, &PaymentEntry> {
    let mut folded: HashMap<String, &PaymentEntry> = HashMap::new();
    for e in entries {
        folded.insert(e.id.clone(), e);
    }
    folded
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub granted_sessions: u64,
    pub closed_unspent: u64,
    pub zero_steps: u64,
    pub undecided: u64,
}

/// Reconciliation pass: decide every payment whose last phase is
/// `intent` (crash mid-flight) or `timeout-unknown`, by asking the mint
/// (NUT-07) whether the token's proofs were spent.
///
/// Two accepted trade-offs, deliberate and bounded (Codex P1/P2 on #43):
///
/// 1. Spent-at-mint is not proof THIS wallet received the outputs — an
///    intent-only payment whose token was later spent elsewhere grants a
///    session nobody paid for. The damage is bounded (one session priced
///    from the token's face value) and the alternative — requiring
///    per-proof recovery evidence before granting — deterministically
///    strands every paying customer whose router crashed mid-receive.
///    Precise attribution is the per-Y saga linkage tracked in #31; until
///    then we accept the bounded free-ride over certain harm.
/// 2. The amount is the token's FACE value (what NUT-07 reports), not the
///    net-after-input-fees the live path uses — on fee-charging mints the
///    reconciled session can over-grant by the fee delta, consistent with
///    the live path's posture of never shorting the customer after value
///    moved.
///
/// Spent → the customer paid: the returned grant list carries the entry
/// and spent amount; the (async) caller creates each session and opens the
/// gate — same-MAC session overwrite makes re-runs idempotent. Unspent →
/// nothing owed, closed. If the mint cannot be asked, the entry stays
/// undecided and is retried on the next boot (never failed — the outcome
/// is still unknown). Grants are journaled `reconcile-spent` BEFORE being
/// returned, so a crash while granting re-runs idempotently next boot.
pub async fn reconcile(
    dir: &Path,
    sink: &dyn crate::migration::TokenSink,
) -> (ReconcileReport, Vec<(PaymentEntry, u64)>) {
    let mut report = ReconcileReport::default();
    let mut grants: Vec<(PaymentEntry, u64)> = Vec::new();
    let entries = read_journal(dir);
    for (_, entry) in fold_last(&entries) {
        if !entry.phase.needs_reconciliation() {
            continue;
        }
        match sink.token_spent(&entry.token).await {
            Ok(Some(amount_sat)) => {
                let steps = amount_sat / entry.price_per_step.max(1);
                if steps == 0 {
                    // No side effect owed: safe to close inline.
                    report.zero_steps += 1;
                    let _ = append_entry(
                        dir,
                        &PaymentEntry {
                            phase: PaymentPhase::ReconcileZeroSteps { amount_sat },
                            ..entry.clone()
                        },
                    );
                } else {
                    // Grant returned UNJOURNALLED: the caller appends
                    // `reconcile-spent` only after the granted session is
                    // DURABLY saved (save_now), so every crash point
                    // converges via idempotent re-grant instead of
                    // stranding a settled-but-sessionless payment
                    // (Codex P1 on #43).
                    report.granted_sessions += 1;
                    grants.push((entry.clone(), amount_sat));
                }
            }
            Ok(None) => {
                // Close as unspent ONLY if no local receive saga is still
                // unresolved: an incomplete saga can coexist with an
                // UNSPENT NUT-07 answer while its swap is in flight
                // (Codex P1 on #43) — that outcome is still ambiguous.
                match sink.mint_has_unresolved_receive(&entry.token).await {
                    Ok(false) => {
                        let _ = append_entry(
                            dir,
                            &PaymentEntry {
                                phase: PaymentPhase::ReconcileUnspent,
                                ..entry.clone()
                            },
                        );
                        report.closed_unspent += 1;
                    }
                    _ => report.undecided += 1,
                }
            }
            Err(_) => {
                // Mint unreachable, check unavailable, or proofs PENDING
                // at the mint: still unknown — retried next pass.
                report.undecided += 1;
            }
        }
    }
    (report, grants)
}

/// Operator summary for CLI `status` (#32/#40).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaymentsSummary {
    pub total: u64,
    pub needs_reconciliation: u64,
    pub received_sat: u64,
}

pub fn summarize(dir: &Path) -> PaymentsSummary {
    let mut s = PaymentsSummary::default();
    for (_, e) in fold_last(&read_journal(dir)) {
        s.total += 1;
        if e.phase.needs_reconciliation() {
            s.needs_reconciliation += 1;
        }
        match e.phase {
            PaymentPhase::Received { amount_sat } | PaymentPhase::ReconcileSpent { amount_sat } => {
                s.received_sat += amount_sat
            }
            _ => {}
        }
    }
    s
}

/// The pricing fact recorded at intent time, for replay path use.
pub fn entry_price_per_step(dir: &Path, token: &str) -> Option<u64> {
    let id = token_id(token);
    fold_last(&read_journal(dir))
        .get(&id)
        .map(|e| e.price_per_step)
}

/// Lookup for idempotent replay: the settled (value-received) outcome for
/// a token, if any — `Received` or `ReconcileSpent`. Unsettled phases
/// (intent, timeout-unknown) and everything else return None.
pub fn settled_outcome(dir: &Path, token: &str) -> Option<PaymentPhase> {
    let id = token_id(token);
    match fold_last(&read_journal(dir))
        .get(&id)
        .map(|e| e.phase.clone())
    {
        Some(phase @ (PaymentPhase::Received { .. } | PaymentPhase::ReconcileSpent { .. })) => {
            Some(phase)
        }
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PaymentJournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Convert for call-site ergonomics where WalletError is already in play.
impl From<PaymentJournalError> for WalletError {
    fn from(e: PaymentJournalError) -> Self {
        WalletError::Database(e.to_string())
    }
}
