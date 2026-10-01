//! Payout journal (#41) — business-level recovery for profit-share melts.
//!
//! AGENTS.md: "Partial successes must never be discarded." The payout loop
//! melts owner-first then maintainers; any crash or ambiguity in that chain
//! could wedge later ticks (a literal bolt11 that was actually paid fails
//! forever on re-melt, permanently blocking maintainer payouts) or discard
//! the fact that some recipients were already paid.
//!
//! Crash-window table (per melt, id = mint + identity + invoice):
//!
//! | Death point | Journal | Mint/LN state | Next tick |
//! |---|---|---|---|
//! | before intent append | nothing | untouched | fresh plan |
//! | after intent, before/during melt | `intent` | maybe swap-requested | fresh-invoice (LNURL) retry safe; literal bolt11 → skip + surface (indistinguishable) |
//! | melt Ok, before `paid` append | `intent` | paid | LNURL: fresh retry safe; literal bolt11 → skip + surface |
//! | after `paid` | `paid` | paid | skip (idempotent across restarts) |
//! | timeout, before terminal | `ambiguous` | maybe paid | unresolved melt saga → skip + surface, OTHERS PROCEED; saga resolved → `resolved`, skip |
//!
//! The journal never blocks maintainers on an ambiguous owner: that is the
//! wedge #41 exists to remove. bolt11 invoices pay at most once, so a
//! literal-invoice ambiguity can never be safely retried — it is surfaced
//! for the operator instead, while the plan continues around it.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PAYOUT_JOURNAL_NAME: &str = "payout-journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum PayoutPhase {
    /// Durable intent recorded before `wallet.melt`.
    Intent,
    /// Melt completed; the recipient was paid.
    Paid,
    /// Melt timed out — the Lightning payment may have fired. CDK's melt
    /// saga reconciliation (quote-state check) settles it; never retried
    /// on the same invoice.
    Ambiguous,
    /// Definitive melt failure (mint rejected, LNURL fetch failed, ...).
    Failed { reason: String },
    /// An ambiguous melt whose saga has since settled; paid-vs-compensated
    /// is not distinguishable at journal level (the wallet balance is the
    /// operator's signal). Terminal; never re-melted.
    Resolved,
}

impl PayoutPhase {
    pub fn is_terminal_done(&self) -> bool {
        matches!(self, PayoutPhase::Paid | PayoutPhase::Resolved)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayoutEntry {
    /// Deterministic id: sha256(mint + "|" + identity + "|" + invoice).
    pub id: String,
    pub ts: u64,
    pub mint: String,
    pub identity: String,
    /// The invoice actually melted (bolt11) — the single-pay unit.
    pub invoice: String,
    pub amount_sat: u64,
    /// true when the invoice came verbatim from config (no fresh invoice
    /// is possible on retry — ambiguity can never be safely re-attempted).
    pub literal_invoice: bool,
    pub phase: PayoutPhase,
}

pub fn journal_path(dir: &Path) -> PathBuf {
    dir.join(PAYOUT_JOURNAL_NAME)
}

pub fn entry_id(mint: &str, identity: &str, invoice: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("{mint}|{identity}|{invoice}").as_bytes());
    hex::encode(h.finalize())
}

/// Append + fsync (journal is 0600; contains no bearer material — the
/// invoice identifies a payment, it cannot spend — but keep it private
/// with the wallet's siblings for uniformity).
/// Payout tasks for multiple mints append to one journal concurrently —
/// appends and compaction must be serialized or a compacted snapshot can
/// drop an intent appended after the snapshot was read (Codex P1 on #45).
static JOURNAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn append_entry(dir: &Path, entry: &PayoutEntry) -> std::io::Result<()> {
    let _guard = JOURNAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    // The directory entry must be durable or a first-created journal can
    // vanish with the file on power loss (Codex P2 on #45) — propagate
    // the failure so callers never believe a non-durable intent.
    if let Some(parent) = path.parent() {
        let dirf = std::fs::File::open(parent)?;
        dirf.sync_all()?;
    }
    Ok(())
}

pub fn read_journal(dir: &Path) -> Vec<PayoutEntry> {
    let Ok(file) = std::fs::File::open(journal_path(dir)) else {
        return Vec::new();
    };
    std::io::BufRead::lines(&mut std::io::BufReader::new(file))
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Last phase per id (append-only journal, last-write-wins).
pub fn fold_last(entries: &[PayoutEntry]) -> HashMap<String, &PayoutEntry> {
    let mut folded: HashMap<String, &PayoutEntry> = HashMap::new();
    for e in entries {
        folded.insert(e.id.clone(), e);
    }
    folded
}

/// The journal-level decision for a melt attempt about to happen, given the
/// entry's last state and whether an unresolved CDK melt saga exists for
/// this mint. This is the resume-the-plan core of #41: settled ⇒ skip,
/// ambiguous-unresolved ⇒ skip but NEVER block the rest of the plan,
/// ambiguous-resolved ⇒ advance to terminal, everything else ⇒ proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeltDecision {
    /// No journal objection — melt (and journal a fresh intent first).
    Proceed,
    /// Already paid or resolved — skip, plan continues.
    SkipDone,
    /// Ambiguous and either unresolved or a literal invoice (unretryable)
    /// — skip THIS melt, surface to the operator, plan continues around it.
    SkipSurface,
}

pub fn decide(
    last: Option<&PayoutPhase>,
    literal_invoice: bool,
    mint_has_unresolved_melt_saga: bool,
) -> (MeltDecision, Option<PayoutPhase>) {
    match last {
        None => {
            // A fresh invoice has no journal history of its own, but an
            // unresolved melt saga for this mint means a PRIOR payout's
            // outcome is undecided — starting another melt from the
            // reserved/reduced balance risks double-payment or split
            // skew once that saga settles (Codex P1 on #45, round 4).
            // Wait one tick; the saga settles within the recovery budget.
            if mint_has_unresolved_melt_saga {
                (MeltDecision::SkipSurface, None)
            } else {
                (MeltDecision::Proceed, None)
            }
        }
        Some(PayoutPhase::Intent) => {
            // Crash between intent and terminal append: the melt may or
            // may not have run, and that is indistinguishable locally.
            // NEVER re-attempt the same invoice (Codex P1 on #45) — a
            // fresh-invoice attempt gets a new journal key anyway, so this
            // only guards the same-invoice case, where a re-melt is
            // exactly what must not happen. Surface; the balance-driven
            // plan re-pays from remaining balance with a fresh key.
            (MeltDecision::SkipSurface, None)
        }
        Some(PayoutPhase::Paid) => (MeltDecision::SkipDone, None),
        Some(PayoutPhase::Resolved) => {
            // A resolved AMBIGUITY is not proof of payment — compensated
            // melts resolve identically. Fresh invoices are safe to treat
            // as done (a compensated melt restored the balance and the
            // next tick re-pays); a literal invoice has no fallback and
            // must stay surfaced forever (Codex P1 on #45, round 2).
            if literal_invoice {
                (MeltDecision::SkipSurface, None)
            } else {
                (MeltDecision::SkipDone, None)
            }
        }
        Some(PayoutPhase::Failed { .. }) => (MeltDecision::Proceed, None),
        Some(PayoutPhase::Ambiguous) => {
            if mint_has_unresolved_melt_saga {
                (MeltDecision::SkipSurface, None)
            } else if literal_invoice {
                // The saga settled, but a COMPENSATED (unpaid) melt is
                // indistinguishable from a paid one — and a literal
                // invoice has no fresh-invoice retry to fall back on.
                // Close the entry but keep it surfaced for the operator
                // (the wallet balance shows compensated-vs-paid); never
                // report AlreadyDone for a possibly-unpaid literal melt
                // (Codex P1 on #45).
                (MeltDecision::SkipSurface, Some(PayoutPhase::Resolved))
            } else {
                // Settled + fresh invoices available: even if this melt
                // was compensated, the next tick's plan re-pays from the
                // (restored) balance with a new invoice.
                (MeltDecision::SkipDone, Some(PayoutPhase::Resolved))
            }
        }
    }
}

/// Rewrite the journal to the last entry per id once it grows past
/// `threshold_lines` (atomic tmp+rename+dirsync). Successful LNURL
/// payouts use a fresh invoice id every tick, so the append-only file
/// grows without bound on a long-running router (Codex P2 on #45);
/// last-per-id is exactly the reconcile-relevant state.
pub fn compact_if_large(dir: &Path, threshold_lines: usize) -> std::io::Result<bool> {
    let _guard = JOURNAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let entries = read_journal(dir);
    if entries.len() <= threshold_lines {
        return Ok(false);
    }
    let folded: Vec<PayoutEntry> = {
        let mut ordered: Vec<PayoutEntry> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in entries.into_iter().rev() {
            if seen.insert(e.id.clone()) {
                ordered.push(e);
            }
        }
        ordered.reverse();
        ordered
    };
    let path = journal_path(dir);
    let tmp = path.with_extension("tmp");
    let mut body = String::new();
    for e in &folded {
        body.push_str(&serde_json::to_string(e).map_err(std::io::Error::other)?);
        body.push('\n');
    }
    // fsync the compacted data BEFORE the rename (Codex P2 on #45): a
    // rename-visible but unsynced file can survive power loss empty,
    // discarding paid/ambiguous states later ticks rely on.
    use std::io::Write;
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    if let Some(parent) = path.parent() {
        let dirf = std::fs::File::open(parent)?;
        dirf.sync_all()?;
    }
    tracing::info!(
        kept = folded.len(),
        "payout journal compacted to last entry per id"
    );
    Ok(true)
}

/// Operator summary for CLI `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayoutsSummary {
    pub total: u64,
    pub paid_sat: u64,
    pub ambiguous: u64,
}

pub fn summarize(dir: &Path) -> PayoutsSummary {
    let mut s = PayoutsSummary::default();
    for (_, e) in fold_last(&read_journal(dir)) {
        s.total += 1;
        match e.phase {
            // Only Paid counts as paid: Resolved is a settled AMBIGUITY
            // that may have been compensated — counting it as paid would
            // overstate what recipients actually received (Codex P2 on
            // #45). It stays visible via `total`.
            PayoutPhase::Paid => s.paid_sat += e.amount_sat,
            PayoutPhase::Resolved => {}
            PayoutPhase::Ambiguous => s.ambiguous += 1,
            _ => {}
        }
    }
    s
}

/// The last phase recorded for a specific melt identity key, if any.
pub fn last_for(dir: &Path, mint: &str, identity: &str, invoice: &str) -> Option<PayoutPhase> {
    fold_last(&read_journal(dir))
        .get(&entry_id(mint, identity, invoice))
        .map(|e| e.phase.clone())
}
