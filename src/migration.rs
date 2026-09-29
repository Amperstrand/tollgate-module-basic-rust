//! First-boot gonuts→CDK wallet migration, hardened for value retention (#12).
//!
//! The pre-hardening behavior dropped value two ways: failed token imports
//! were only counted (never retained) before `wallet.db` was renamed away,
//! and a crash mid-import left the completion marker unwritten so the next
//! boot re-ran everything, counting already-imported tokens as failures.
//!
//! Contract:
//! - a `Pending` intent entry is journaled (`migration-journal.jsonl`) and
//!   **fsynced before `receive()` touches the mint** — the write-ahead-intent
//!   discipline AGENTS.md mandates for money-moving CDK calls ("a TollGate
//!   durable record created **before** the call, advanced after each step,
//!   and reconciled at startup"). Death between receive and the outcome
//!   append leaves the intent row, not nothing;
//! - ambiguous receive outcomes (timeout) rest in `Pending` and are
//!   reconciled on the next boot via NUT-07 checkstate — after CDK's
//!   `ensure_mint`/`recover_incomplete_sagas` has settled the wallet-side
//!   saga — never blindly resubmitted (AGENTS.md: "Ambiguous network
//!   results must be reconciled, not blindly retried");
//! - tokens proven spent at the mint are terminal (`Spent`): retrying can
//!   never import them, so they do not block completion — the value either
//!   already sits in this deterministic wallet or is unrecoverable;
//! - `wallet.db` is renamed only when **zero** imports failed and **zero**
//!   outcomes are still unsettled (`Pending`) — otherwise it stays in place
//!   as the recovery source and the marker records `partial`;
//! - a re-run skips tokens the journal already shows as imported or spent,
//!   so crash-recovery converges instead of re-failing spent tokens;
//! - the journal and marker are fsynced; a torn trailing line is skipped
//!   by the reader (a torn intent line means receive never started).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wallet::TollWallet;

#[async_trait::async_trait]
impl TokenSink for TollWallet {
    async fn receive(&self, token: &str) -> Result<u64, crate::wallet::WalletError> {
        TollWallet::receive(self, token).await
    }

    async fn token_spent(&self, token: &str) -> Result<Option<u64>, crate::wallet::WalletError> {
        TollWallet::token_spent(self, token).await
    }

    async fn mint_has_unresolved_receive(
        &self,
        token: &str,
    ) -> Result<bool, crate::wallet::WalletError> {
        TollWallet::mint_has_unresolved_receive(self, token).await
    }
}

pub const JOURNAL_NAME: &str = "migration-journal.jsonl";
pub const MARKER_NAME: &str = ".migration_complete";
pub const OLD_DB_NAME: &str = "wallet.db";
pub const OLD_DB_BACKUP_NAME: &str = "wallet.db.pre-migration";
pub const TOKENS_FILE_NAME: &str = "tokens.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TokenOutcome {
    /// The token was received into the CDK wallet (this boot or a previous
    /// one — a re-run treats `Imported` as done).
    Imported { amount_sat: u64 },
    /// Receive failed definitively; the reason is preserved for the operator
    /// and the token is retried on a later boot ("failure is not cached").
    /// The token string is retained in the journal so it can be retried or
    /// hand-fed to another wallet.
    Failed { reason: String },
    /// Intent record: this process was about to (or did) hand the token to
    /// `receive()`, but no settled outcome is durable. Written and fsynced
    /// BEFORE the receive call, mirroring how CDK persists saga state before
    /// its network calls. Covers death mid-receive and ambiguous (timed-out)
    /// receives; reconciled on a later boot after CDK saga recovery.
    Pending,
    /// Every proof of the token is spent at the mint (NUT-07 checkstate):
    /// an earlier receive of ours completed (the value already sits in the
    /// deterministic wallet) or the token was spent elsewhere — in either
    /// case retrying can never import it. Terminal; the amount is re-derived
    /// from the token so the summary stays honest.
    Spent { amount_sat: u64 },
}

impl TokenOutcome {
    fn is_imported(&self) -> bool {
        matches!(self, TokenOutcome::Imported { .. })
    }

    fn is_spent(&self) -> bool {
        matches!(self, TokenOutcome::Spent { .. })
    }

    fn is_pending(&self) -> bool {
        matches!(self, TokenOutcome::Pending)
    }
}

/// What the import loop may call on the wallet. A trait so crash-window
/// tests can fake the mint side and observe journal ordering without a
/// live CDK wallet.
#[async_trait::async_trait]
pub trait TokenSink {
    async fn receive(&self, token: &str) -> Result<u64, crate::wallet::WalletError>;

    /// NUT-07 reconciliation: `Ok(Some(amount_sat))` iff every proof of the
    /// token is spent at the mint. Errors mean "cannot reconcile right now"
    /// (mint unreachable), NOT "unspent".
    async fn token_spent(&self, token: &str) -> Result<Option<u64>, crate::wallet::WalletError>;

    /// Whether the token's mint wallet still holds an incomplete CDK
    /// receive saga — a purely local query (works with the mint down).
    /// While true, an earlier receive's outcome is undecided and the token
    /// must be neither re-submitted nor terminalized this boot.
    async fn mint_has_unresolved_receive(
        &self,
        token: &str,
    ) -> Result<bool, crate::wallet::WalletError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub token: String,
    pub outcome: TokenOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MigrationSummary {
    pub imported: u64,
    pub failed: u64,
    pub skipped_already_imported: u64,
    /// Tokens whose outcome is still unsettled (`Pending`): death mid-loop,
    /// or a receive whose result was ambiguous. Pending does not block
    /// re-runs — these are reconciled on the next boot — but it DOES block
    /// finalization (rename + complete marker).
    pub pending: u64,
    /// Tokens proven spent at the mint via NUT-07: terminal (value already
    /// in the wallet from an earlier receive, or unrecoverable). Does NOT
    /// block finalization — retrying can never import them.
    pub spent: u64,
    pub spent_sat: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MigrationFinish {
    /// All tokens imported; old DB renamed to `wallet.db.pre-migration`.
    Complete,
    /// One or more tokens failed; `wallet.db` is RETAINED in place as the
    /// recovery source and the marker records a partial migration.
    Partial,
}

pub struct FirstBootMigration {
    pub old_db: PathBuf,
    pub tokens_file: PathBuf,
    pub journal: PathBuf,
    pub marker: PathBuf,
}

impl FirstBootMigration {
    pub fn new(db_dir: &Path) -> Self {
        FirstBootMigration {
            old_db: db_dir.join(OLD_DB_NAME),
            tokens_file: db_dir.join(TOKENS_FILE_NAME),
            journal: db_dir.join(JOURNAL_NAME),
            marker: db_dir.join(MARKER_NAME),
        }
    }

    /// Whether the migration should run at all: an old wallet exists and
    /// no `state=complete` marker was written. A `partial` marker (or no
    /// marker after a crash) means a re-run: the journal's `Imported`
    /// entries dedupe tokens that already landed, so re-runs converge
    /// instead of re-failing spent tokens — this also auto-heals imports
    /// that failed because a mint was merely down at first boot.
    pub fn should_run(&self) -> bool {
        if !self.old_db.exists() {
            return false;
        }
        !marker_is_complete(&self.marker)
    }

    pub fn previous_outcomes(&self) -> Vec<JournalEntry> {
        read_journal(&self.journal)
    }

    /// Import every token from `tokens_file`, journaling outcomes. Tokens
    /// whose last journal entry is `Imported` are skipped (crash-recovery
    /// convergence). Every attempt is preceded by a fsynced `Pending`
    /// intent entry, so death after the mint moved value but before the
    /// outcome append leaves a durable record instead of nothing.
    pub async fn import_tokens(
        &self,
        sink: &dyn TokenSink,
    ) -> Result<MigrationSummary, MigrationError> {
        let mut imported = 0u64;
        let mut failed = 0u64;
        let mut skipped = 0u64;
        let mut pending = 0u64;
        let mut spent = 0u64;
        let mut spent_sat = 0u64;

        let already = fold_last_outcomes(&read_journal(&self.journal));
        let tokens = read_tokens(&self.tokens_file)?;
        let mut journal_file = open_append(&self.journal)?;

        for token in tokens {
            match already.get(&token) {
                Some(outcome) if outcome.is_imported() => {
                    skipped += 1;
                    continue;
                }
                Some(outcome) if outcome.is_spent() => {
                    spent += 1;
                    if let TokenOutcome::Spent { amount_sat } = outcome {
                        spent_sat += amount_sat;
                    }
                    continue;
                }
                _ => {}
            }

            // AGENTS.md ("Ambiguous network results must be reconciled, not
            // blindly retried"): before (re)attempting a token that is not
            // terminally done, ask the mint whether its proofs are already
            // spent. This settles both death-mid-receive Pending rows (CDK's
            // recover_incomplete_sagas has already run via ensure_mint at
            // boot) and tokens spent under a pre-journal migration, without
            // inferring anything from receive error text.
            let spent_check = sink.token_spent(&token).await;
            let prior_pending = matches!(already.get(&token), Some(TokenOutcome::Pending));

            // r4126804510: an all-spent NUT-07 answer proves only that the
            // INPUTS were consumed — not that this wallet recovered the
            // replacement outputs. CDK deletes a receive saga exactly when
            // its outcome is persisted (outputs recovered via NUT-19 replay
            // or NUT-09 /restore, compensated, or closed value-less with a
            // logged warning), so "no incomplete receive saga" is CDK's own
            // "operation decided" signal. While one remains, defer.
            //
            // r4126804498: a token whose earlier attempt may still be in
            // flight (unresolved saga, or Pending with the mint
            // unreachable) must not receive a fresh submit — that replays
            // the same inputs without reconciling and burns a second
            // derivation range (AGENTS.md forbids both).
            let unresolved = match sink.mint_has_unresolved_receive(&token).await {
                Ok(v) => v,
                // Deterministic local answers: an unparseable token cannot
                // have a saga. Only storage-level failures are "cannot
                // determine" — those defer conservatively.
                Err(crate::wallet::WalletError::TokenParse(_)) => false,
                Err(e) => {
                    tracing::warn!(error = %e, "migration: cannot inspect local saga state; treating token as unresolved");
                    true
                }
            };
            let untouchable = unresolved || (prior_pending && spent_check.is_err());
            if untouchable {
                tracing::warn!(
                    "migration: token outcome undecided (unresolved receive saga or unreachable mint); deferring to next boot"
                );
                pending += 1;
                continue;
            }

            match spent_check {
                Ok(Some(amount_sat)) => {
                    tracing::warn!(
                        amount_sat,
                        "migration: token already spent at mint; marking terminal Spent (value sits in the wallet from an earlier receive, or is unrecoverable)"
                    );
                    spent += 1;
                    spent_sat += amount_sat;
                    append_entry(
                        &mut journal_file,
                        &JournalEntry {
                            token: token.clone(),
                            outcome: TokenOutcome::Spent { amount_sat },
                        },
                    )?;
                    journal_file.sync_all()?;
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "migration: NUT-07 pre-check unavailable; proceeding to receive");
                }
            }

            append_entry(
                &mut journal_file,
                &JournalEntry {
                    token: token.clone(),
                    outcome: TokenOutcome::Pending,
                },
            )?;
            // AGENTS.md ("Fund safety..." hard rules / CDK boundaries): the
            // intent must be durable BEFORE the money-moving call — without
            // this fsync a crash after receive leaves no record that the
            // token was ever attempted, and the next boot replays a now-
            // spent token as a permanent failure.
            journal_file.sync_all()?;

            let outcome = match sink.receive(&token).await {
                Ok(amount_sat) => {
                    imported += amount_sat;
                    TokenOutcome::Imported { amount_sat }
                }
                Err(crate::wallet::WalletError::Timeout(d)) => {
                    // Ambiguous, not failed: the mint may have accepted the
                    // swap and CDK's saga survives in SQLite for the next
                    // boot's ensure_mint/recover_incomplete_sagas to settle.
                    // The Pending intent row written above IS the durable
                    // record; next boot's NUT-07 pre-check classifies it.
                    tracing::warn!(
                        timeout_secs = d.as_secs(),
                        "migration: receive timed out (ambiguous); reconciling on next boot"
                    );
                    pending += 1;
                    TokenOutcome::Pending
                }
                Err(e) => {
                    tracing::warn!(error = %e, "migration: token import failed (retained in journal)");
                    failed += 1;
                    TokenOutcome::Failed {
                        reason: e.to_string(),
                    }
                }
            };
            if !outcome.is_pending() {
                append_entry(
                    &mut journal_file,
                    &JournalEntry {
                        token: token.clone(),
                        outcome,
                    },
                )?;
            }
        }
        journal_file.sync_all()?;

        Ok(MigrationSummary {
            imported,
            failed,
            skipped_already_imported: skipped,
            pending,
            spent,
            spent_sat,
        })
    }

    /// Finalize the migration: rename the old DB only when no token failed
    /// AND no outcome is still unsettled, and write the marker (fsynced)
    /// recording the outcome.
    pub fn finish(&self, summary: MigrationSummary) -> Result<MigrationFinish, MigrationError> {
        let clean = summary.failed == 0 && summary.pending == 0;
        let finish = if clean {
            let backup = self.old_db.with_file_name(OLD_DB_BACKUP_NAME);
            std::fs::rename(&self.old_db, &backup).map_err(MigrationError::Io)?;
            MigrationFinish::Complete
        } else {
            tracing::error!(
                failed = summary.failed,
                pending = summary.pending,
                old_db = %self.old_db.display(),
                "migration incomplete: {} token(s) failed, {} unsettled; wallet.db RETAINED as recovery source",
                summary.failed,
                summary.pending
            );
            MigrationFinish::Partial
        };

        let marker_body = format!(
            "state={}\nimported_sat={}\nfailed={}\nskipped_already_imported={}\npending={}\nspent={}\nspent_sat={}\ndate={}\n",
            if clean { "complete" } else { "partial" },
            summary.imported,
            summary.failed,
            summary.skipped_already_imported,
            summary.pending,
            summary.spent,
            summary.spent_sat,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        let mut f = File::create(&self.marker).map_err(MigrationError::Io)?;
        f.write_all(marker_body.as_bytes())
            .map_err(MigrationError::Io)?;
        f.sync_all().map_err(MigrationError::Io)?;

        Ok(finish)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportOutcome {
    /// Export ran this boot and exited 0.
    Succeeded,
    /// Export ran this boot and exited nonzero — the artifact on disk was
    /// not refreshed by a clean run and must not be imported/finalized
    /// this boot (retry the export next boot).
    Failed,
    /// Export did not run, or could not be spawned (tool missing) — in
    /// which case nothing touched `tokens.jsonl` this boot, so a file put
    /// there by the deliberate manual export flow (MIGRATION.md) stays
    /// importable.
    NotRun,
}

/// Codex P1 on PR #21 (main.rs:84): the import gate must key on the
/// export's exit status, not bare `tokens.jsonl` existence — the exporter
/// historically truncated the file before finishing, so an existence-only
/// gate could import a partial artifact and finalize the migration with
/// unexported tokens silently unmigrated.
pub fn should_import_tokens(should_run: bool, tokens_exist: bool, export: ExportOutcome) -> bool {
    should_run && tokens_exist && export != ExportOutcome::Failed
}

/// Operator-facing snapshot of migration health for the CLI `status`
/// surface (issue #32): journal-derived counts plus the marker state, so a
/// stuck migration is one command to diagnose instead of log archaeology.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationState {
    pub marker: Option<String>,
    pub imported_sat: u64,
    pub failed: u64,
    pub pending: u64,
    pub spent: u64,
    pub spent_sat: u64,
}

impl MigrationState {
    /// `true` when every attempted token reached a terminal outcome and
    /// none failed — i.e. nothing is waiting on a later boot.
    pub fn is_settled(&self) -> bool {
        self.pending == 0 && self.failed == 0
    }
}

pub fn summarize_state(db_dir: &Path) -> MigrationState {
    let m = FirstBootMigration::new(db_dir);
    let mut state = MigrationState {
        marker: None,
        imported_sat: 0,
        failed: 0,
        pending: 0,
        spent: 0,
        spent_sat: 0,
    };
    if let Ok(body) = std::fs::read_to_string(&m.marker) {
        let marker_state = body
            .lines()
            .find_map(|l| l.trim().strip_prefix("state="))
            .map(str::to_string);
        state.marker = Some(marker_state.unwrap_or_else(|| "legacy".into()));
    }
    for outcome in fold_last_outcomes(&read_journal(&m.journal)).into_values() {
        match outcome {
            TokenOutcome::Imported { amount_sat } => state.imported_sat += amount_sat,
            TokenOutcome::Failed { .. } => state.failed += 1,
            TokenOutcome::Pending => state.pending += 1,
            TokenOutcome::Spent { amount_sat } => {
                state.spent += 1;
                state.spent_sat += amount_sat;
            }
        }
    }
    state
}

/// Fold a raw journal into per-token outcome; the LAST entry for a token
/// wins (Pending → Failed → Imported is a normal retry sequence; a trailing
/// Pending is an unsettled attempt to be reconciled).
fn fold_last_outcomes(entries: &[JournalEntry]) -> std::collections::HashMap<String, TokenOutcome> {
    let mut folded = std::collections::HashMap::new();
    for entry in entries {
        folded.insert(entry.token.clone(), entry.outcome.clone());
    }
    folded
}

/// Whether the completion marker says the migration is done. Accepts the
/// formats that can predate the `state=` field (Codex P2 on PR #21:
/// rejecting them restarted money-moving imports on a completed system):
///
/// - current: a `state=complete` line;
/// - legacy auto-migration: `imported=N`/`failed=N`/`date=N` with no
///   `state=` line — complete only when `failed=0` (a legacy marker with
///   failures describes an incomplete migration; the re-run converges via
///   the journal's `Spent` terminal state instead of re-failing spent
///   tokens);
/// - empty marker (the `touch` procedure MIGRATION.md documents as the
///   final step of a manual migration): the operator's explicit "done"
///   signal — honored as complete.
fn marker_is_complete(path: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    if body.trim().is_empty() {
        return true;
    }
    let mut saw_imported = false;
    let mut legacy_failed: Option<u64> = None;
    for line in body.lines() {
        let l = line.trim();
        if l == "state=complete" {
            return true;
        }
        if l.starts_with("state=") || l.starts_with("pending=") {
            // Any other explicit state (partial, …) is authoritative.
            return false;
        }
        if l.starts_with("imported=") {
            saw_imported = true;
        }
        if let Some(v) = l.strip_prefix("failed=") {
            legacy_failed = v.parse().ok();
        }
    }
    saw_imported && legacy_failed == Some(0)
}

fn read_tokens(path: &Path) -> Result<Vec<String>, MigrationError> {
    let content = std::fs::read_to_string(path).map_err(MigrationError::Io)?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn read_journal(path: &Path) -> Vec<JournalEntry> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<JournalEntry>(&line).ok())
        .collect()
}

fn open_append(path: &Path) -> Result<File, MigrationError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(MigrationError::Io)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(MigrationError::Io)
}

fn append_entry(file: &mut File, entry: &JournalEntry) -> Result<(), MigrationError> {
    let mut line = serde_json::to_string(entry).map_err(MigrationError::Serde)?;
    line.push('\n');
    file.write_all(line.as_bytes()).map_err(MigrationError::Io)
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_line(token: &str) -> String {
        format!("{token}\n")
    }

    fn wallet_with_no_mints(dir: &Path) -> TollWallet {
        let mut seed = [0u8; 64];
        seed[..8].copy_from_slice(&[7u8; 8]);
        TollWallet::new(seed, vec![], dir.to_path_buf())
    }

    fn setup(dir: &Path, tokens: &[&str], journal: &[JournalEntry]) -> FirstBootMigration {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(OLD_DB_NAME), b"fake bbolt db").unwrap();
        std::fs::write(
            dir.join(TOKENS_FILE_NAME),
            tokens.iter().map(|t| token_line(t)).collect::<String>(),
        )
        .unwrap();
        if !journal.is_empty() {
            let body = journal
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(dir.join(JOURNAL_NAME), format!("{body}\n")).unwrap();
        }
        FirstBootMigration::new(dir)
    }

    #[tokio::test]
    async fn failed_imports_retain_old_db_and_journal() {
        let dir = tempfile::tempdir().unwrap();
        let m = setup(dir.path(), &["cashuAtoken1", "cashuAtoken2"], &[]);
        assert!(m.should_run());

        let summary = m
            .import_tokens(&wallet_with_no_mints(dir.path()))
            .await
            .unwrap();
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.imported, 0);

        match m.finish(summary).unwrap() {
            MigrationFinish::Partial => {}
            other => panic!("expected Partial, got {other:?}"),
        }
        assert!(
            m.old_db.exists(),
            "old wallet.db must be retained on partial"
        );
        let entries = read_journal(&m.journal);
        assert_eq!(entries.len(), 4);
        assert!(entries
            .iter()
            .all(|e| matches!(e.outcome, TokenOutcome::Failed { .. })
                || e.outcome == TokenOutcome::Pending));
        let marker = std::fs::read_to_string(&m.marker).unwrap();
        assert!(marker.contains("state=partial"));
        assert!(m.should_run(), "partial marker must allow a retry run");
    }

    #[tokio::test]
    async fn already_imported_tokens_are_skipped_not_refailed() {
        let dir = tempfile::tempdir().unwrap();
        let m = setup(
            dir.path(),
            &["cashuAtoken1", "cashuAtoken2"],
            &[JournalEntry {
                token: "cashuAtoken1".to_string(),
                outcome: TokenOutcome::Imported { amount_sat: 5 },
            }],
        );

        let summary = m
            .import_tokens(&wallet_with_no_mints(dir.path()))
            .await
            .unwrap();
        assert_eq!(summary.skipped_already_imported, 1);
        assert_eq!(
            summary.failed, 1,
            "only the never-imported token is re-attempted"
        );
    }

    #[test]
    fn complete_marker_blocks_rerun_and_clean_finish_renames() {
        let dir = tempfile::tempdir().unwrap();
        let m = setup(dir.path(), &[], &[]);
        assert!(m.should_run());

        let finish = m
            .finish(MigrationSummary {
                imported: 9,
                failed: 0,
                skipped_already_imported: 0,
                pending: 0,
                spent: 0,
                spent_sat: 0,
            })
            .unwrap();
        assert_eq!(finish, MigrationFinish::Complete);
        assert!(!m.old_db.exists(), "old db renamed away on clean finish");
        assert!(m.old_db.with_file_name(OLD_DB_BACKUP_NAME).exists());
        assert!(!m.should_run(), "complete marker must block further runs");
    }

    #[test]
    fn unsettled_pending_blocks_rename_and_completion() {
        let dir = tempfile::tempdir().unwrap();
        let m = setup(dir.path(), &["cashuAtoken1"], &[]);

        let finish = m
            .finish(MigrationSummary {
                imported: 5,
                failed: 0,
                skipped_already_imported: 0,
                pending: 1,
                spent: 0,
                spent_sat: 0,
            })
            .unwrap();
        assert_eq!(finish, MigrationFinish::Partial);
        assert!(
            m.old_db.exists(),
            "unsettled outcome must retain wallet.db (value may have moved)"
        );
        let marker = std::fs::read_to_string(&m.marker).unwrap();
        assert!(marker.contains("state=partial"));
        assert!(marker.contains("pending=1"));
    }

    #[test]
    fn journal_reader_tolerates_torn_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&JournalEntry {
            token: "t1".to_string(),
            outcome: TokenOutcome::Imported { amount_sat: 1 },
        })
        .unwrap();
        let body = format!("{good}\n{{\"token\": \"torn");
        std::fs::write(dir.path().join(JOURNAL_NAME), body).unwrap();

        let entries = read_journal(&dir.path().join(JOURNAL_NAME));
        assert_eq!(
            entries.len(),
            1,
            "torn trailing line must be skipped, not fatal"
        );
    }

    #[test]
    fn no_old_db_means_no_migration() {
        let dir = tempfile::tempdir().unwrap();
        let m = FirstBootMigration::new(dir.path());
        assert!(!m.should_run());
    }

    /// Fakes the mint side and records, at the moment `receive` runs,
    /// whether the journal already contains a durable Pending intent for
    /// the token — the write-ahead-intent contract under test.
    #[derive(Debug, Default)]
    struct ObservingSink {
        journal_path: std::sync::Mutex<Option<PathBuf>>,
        saw_intent_before_receive: std::sync::Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl TokenSink for ObservingSink {
        async fn token_spent(
            &self,
            _token: &str,
        ) -> Result<Option<u64>, crate::wallet::WalletError> {
            Ok(None)
        }

        async fn mint_has_unresolved_receive(
            &self,
            _token: &str,
        ) -> Result<bool, crate::wallet::WalletError> {
            Ok(false)
        }

        async fn receive(&self, token: &str) -> Result<u64, crate::wallet::WalletError> {
            let journal = self.journal_path.lock().unwrap().clone();
            let saw_intent = match journal
                .map(std::fs::read_to_string)
                .map(|body| body.unwrap_or_default())
            {
                Some(body) => body.lines().any(|line| {
                    serde_json::from_str::<JournalEntry>(line)
                        .map(|e| e.token == token && e.outcome == TokenOutcome::Pending)
                        .unwrap_or(false)
                }),
                None => false,
            };
            self.saw_intent_before_receive
                .lock()
                .unwrap()
                .push(saw_intent);
            Ok(7)
        }
    }

    #[tokio::test]
    async fn receive_intent_is_durable_before_the_mint_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let m = setup(dir.path(), &["cashuAtoken1"], &[]);

        let sink = ObservingSink::default();
        *sink.journal_path.lock().unwrap() = Some(m.journal.clone());

        let summary = m.import_tokens(&sink).await.unwrap();
        assert_eq!(summary.imported, 7);

        let observations = sink.saw_intent_before_receive.lock().unwrap();
        assert_eq!(
            observations.len(),
            1,
            "each attempted token hits the mint exactly once per run"
        );
        assert!(
            observations[0],
            "Pending intent must be journaled (and fsynced) BEFORE receive() is called"
        );

        let folded = fold_last_outcomes(&read_journal(&m.journal));
        assert_eq!(
            folded.get("cashuAtoken1"),
            Some(&TokenOutcome::Imported { amount_sat: 7 })
        );
    }
}
