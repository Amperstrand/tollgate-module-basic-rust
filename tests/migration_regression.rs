//! Regression tests for the PR #21 Codex findings on first-boot migration
//! value safety. Kept in `tests/` (public API only) so the §9 red-proof
//! protocol can check out pre-fix `src/` while these tests remain.

use std::collections::HashMap;
use std::path::PathBuf;

use tollgate_module_basic_rust::migration::{
    should_import_tokens, ExportOutcome, FirstBootMigration, JournalEntry, TokenOutcome, TokenSink,
    JOURNAL_NAME, OLD_DB_NAME, TOKENS_FILE_NAME,
};

#[derive(Debug, Default)]
struct FakeSink {
    /// token -> mint-side NUT-07 answer: `Some(amount)` = all proofs spent,
    /// `None` = still spendable.
    spent: HashMap<String, Option<u64>>,
    /// token -> receive result.
    receive: HashMap<String, Rx>,
    /// When set, every NUT-07 pre-check errors (mint unreachable).
    spent_err: bool,
    calls: std::sync::Mutex<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Rx {
    Ok(u64),
    Timeout,
    Definitive,
}

#[async_trait::async_trait]
impl TokenSink for FakeSink {
    async fn receive(
        &self,
        token: &str,
    ) -> Result<u64, tollgate_module_basic_rust::wallet::WalletError> {
        use tollgate_module_basic_rust::wallet::WalletError;
        self.calls.lock().unwrap().push(format!("receive:{token}"));
        match self.receive.get(token).cloned().unwrap_or(Rx::Ok(1)) {
            Rx::Ok(v) => Ok(v),
            Rx::Timeout => Err(WalletError::Timeout(std::time::Duration::from_secs(30))),
            Rx::Definitive => Err(WalletError::TokenParse("definitive failure".into())),
        }
    }

    async fn token_spent(
        &self,
        token: &str,
    ) -> Result<Option<u64>, tollgate_module_basic_rust::wallet::WalletError> {
        use tollgate_module_basic_rust::wallet::WalletError;
        self.calls.lock().unwrap().push(format!("spent:{token}"));
        if self.spent_err {
            return Err(WalletError::Timeout(std::time::Duration::from_secs(30)));
        }
        Ok(self.spent.get(token).copied().flatten())
    }
}

fn setup(dir: &std::path::Path, tokens: &[&str]) -> FirstBootMigration {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(OLD_DB_NAME), b"fake bbolt db").unwrap();
    std::fs::write(
        dir.join(TOKENS_FILE_NAME),
        tokens.iter().map(|t| format!("{t}\n")).collect::<String>(),
    )
    .unwrap();
    FirstBootMigration::new(dir)
}

fn write_journal(dir: &std::path::Path, entries: &[JournalEntry]) {
    let body = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join(JOURNAL_NAME), format!("{body}\n")).unwrap();
}

fn read_journal(dir: &std::path::Path) -> Vec<JournalEntry> {
    std::fs::read_to_string(dir.join(JOURNAL_NAME))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn fold_last(entries: &[JournalEntry]) -> HashMap<String, TokenOutcome> {
    let mut folded = HashMap::new();
    for e in entries {
        folded.insert(e.token.clone(), e.outcome.clone());
    }
    folded
}

/// Codex P1 finding 1 (migration.rs:132): a durable Pending intent must
/// exist before `receive()` is called, so death between receive and the
/// outcome append leaves a reconcilable record instead of nothing.
#[tokio::test]
async fn pending_intent_is_durable_before_receive_touches_the_mint() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);
    let journal: PathBuf = dir.path().join(JOURNAL_NAME);

    struct ObservingSink {
        journal: PathBuf,
        saw_intent: std::sync::Mutex<Vec<bool>>,
    }
    #[async_trait::async_trait]
    impl TokenSink for ObservingSink {
        async fn receive(
            &self,
            token: &str,
        ) -> Result<u64, tollgate_module_basic_rust::wallet::WalletError> {
            let body = std::fs::read_to_string(&self.journal).unwrap_or_default();
            let saw = body.lines().any(|l| {
                serde_json::from_str::<JournalEntry>(l)
                    .map(|e| e.token == token && e.outcome == TokenOutcome::Pending)
                    .unwrap_or(false)
            });
            self.saw_intent.lock().unwrap().push(saw);
            Ok(7)
        }

        async fn token_spent(
            &self,
            _token: &str,
        ) -> Result<Option<u64>, tollgate_module_basic_rust::wallet::WalletError> {
            Ok(None)
        }
    }

    let sink = ObservingSink {
        journal,
        saw_intent: std::sync::Mutex::new(Vec::new()),
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.imported, 7);

    let observed = sink.saw_intent.lock().unwrap();
    assert_eq!(observed.len(), 1, "one mint call per token");
    assert!(
        observed[0],
        "Pending intent must be journaled and fsynced BEFORE receive()"
    );

    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Imported { amount_sat: 7 })
    );
}

/// Simulated restart for the death-after-receive window: the on-disk
/// journal's last entry for the token is Pending (receive happened, the
/// outcome append did not). The next boot must NOT treat the token as
/// fresh — the intent row must survive and be visible to reconciliation.
#[tokio::test]
async fn death_after_receive_leaves_pending_row_for_next_boot() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);
    write_journal(
        dir.path(),
        &[JournalEntry {
            token: "cashuA1".to_string(),
            outcome: TokenOutcome::Pending,
        }],
    );

    let folded = fold_last(&read_journal(dir.path()));
    assert_eq!(
        folded.get("cashuA1"),
        Some(&TokenOutcome::Pending),
        "hand-serialized post-crash journal folds to Pending"
    );

    // Re-run with the real import loop: the Pending row is not Imported,
    // so the token is re-attempted through the normal path (in this test
    // the fake receive succeeds — the reconcile-vs-retry decision is the
    // next test's job; here we only pin that Pending is not silently
    // dropped nor counted as clean).
    let sink = FakeSink {
        receive: [("cashuA1".to_string(), Rx::Ok(7))].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.skipped_already_imported, 0);
    assert_eq!(summary.imported, 7);
    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Imported { amount_sat: 7 })
    );
}

/// Codex P1 finding 3 (main.rs:84): after a failed (nonzero-exit) re-export,
/// the import gate must NOT run on bare tokens.jsonl existence — the
/// pre-atomicity exporter truncated the file before finishing, so a partial
/// artifact could be imported and the migration finalized with unexported
/// tokens silently unmigrated.
#[test]
fn failed_export_blocks_import_even_when_tokens_file_exists() {
    assert!(should_import_tokens(true, true, ExportOutcome::Succeeded));
    assert!(
        !should_import_tokens(true, true, ExportOutcome::Failed),
        "a nonzero-exit export must block import for this boot"
    );
    // Spawn failure (tool missing) leaves a manually exported file importable.
    assert!(should_import_tokens(true, true, ExportOutcome::NotRun));
    assert!(!should_import_tokens(false, true, ExportOutcome::Succeeded));
    assert!(!should_import_tokens(true, false, ExportOutcome::Succeeded));
}

/// Codex P1 finding 2 (migration.rs:140): a receive Timeout is ambiguous —
/// the mint may have accepted the swap — and must NOT be journaled as an
/// ordinary Failed outcome (blind resubmission of a spent token every boot,
/// migration permanently partial). The Pending intent row stays as the
/// durable record and blocks finalization.
#[tokio::test]
async fn timeout_is_journaled_pending_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);

    let sink = FakeSink {
        receive: [("cashuA1".to_string(), Rx::Timeout)].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.failed, 0, "timeout is ambiguous, not a failure");
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.imported, 0);

    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Pending),
        "last journal entry for a timed-out receive must remain Pending"
    );

    let finish = m.finish(summary).unwrap();
    assert_eq!(
        finish,
        tollgate_module_basic_rust::migration::MigrationFinish::Partial
    );
    assert!(m.old_db.exists(), "unsettled outcome retains wallet.db");
}

/// Next boot after a Timeout: ensure_mint has settled CDK's saga; the NUT-07
/// pre-check reports the proofs spent (the receive actually landed) — the
/// token is terminal `Spent`, never resubmitted, and completion proceeds.
#[tokio::test]
async fn pending_reconciles_to_spent_via_checkstate() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);
    write_journal(
        dir.path(),
        &[JournalEntry {
            token: "cashuA1".to_string(),
            outcome: TokenOutcome::Pending,
        }],
    );

    let sink = FakeSink {
        spent: [("cashuA1".to_string(), Some(7u64))].into_iter().collect(),
        receive: [("cashuA1".to_string(), Rx::Ok(999))].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.spent, 1);
    assert_eq!(summary.spent_sat, 7);
    assert_eq!(summary.imported, 0);
    assert_eq!(summary.failed, 0);

    let calls = sink.calls.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|c| c.starts_with("receive:")),
        "a Spent-classified token must never be resubmitted to the mint: {calls:?}"
    );

    let finish = m.finish(summary).unwrap();
    assert_eq!(
        finish,
        tollgate_module_basic_rust::migration::MigrationFinish::Complete
    );
    assert!(
        !m.old_db.exists(),
        "terminal Spent does not block completion"
    );
    let marker = std::fs::read_to_string(&m.marker).unwrap();
    assert!(marker.contains("state=complete"));
    assert!(marker.contains("spent_sat=7"));
}

/// Next boot after a Timeout where the mint never accepted the swap
/// (CDK compensated the saga): NUT-07 says unspent, the receive is
/// re-attempted and succeeds.
#[tokio::test]
async fn pending_reconciles_forward_when_unspent() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);
    write_journal(
        dir.path(),
        &[JournalEntry {
            token: "cashuA1".to_string(),
            outcome: TokenOutcome::Pending,
        }],
    );

    let sink = FakeSink {
        receive: [("cashuA1".to_string(), Rx::Ok(7))].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.imported, 7);
    assert_eq!(summary.pending, 0);
    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Imported { amount_sat: 7 })
    );
}

/// NUT-07 unreachable (mint down): reconciliation is impossible this boot.
/// The token must NOT be failed (the outcome is still unknown) — it stays
/// pending and blocks finalization until a boot that can reconcile.
#[tokio::test]
async fn unreconcilable_token_stays_pending_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);

    let sink = FakeSink {
        spent_err: true,
        receive: [("cashuA1".to_string(), Rx::Timeout)].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.failed, 0, "cannot-reconcile is not a failure");
    assert_eq!(summary.pending, 1);
    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Pending)
    );
    assert_eq!(
        m.finish(summary).unwrap(),
        tollgate_module_basic_rust::migration::MigrationFinish::Partial
    );
}

/// First contact with a token that was already spent (e.g. imported under
/// the pre-journal migration, or spent by an earlier partial run that lost
/// its journal): the pre-check classifies it terminal without a doomed
/// receive attempt — the convergence path issue #12 asks for
/// ("already-spent → skipped-spent").
#[tokio::test]
async fn already_spent_token_classified_without_receive() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);

    let sink = FakeSink {
        spent: [("cashuA1".to_string(), Some(9u64))].into_iter().collect(),
        receive: [("cashuA1".to_string(), Rx::Ok(999))].into_iter().collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.spent, 1);
    assert_eq!(summary.imported, 0);
    assert_eq!(summary.failed, 0);

    let calls = sink.calls.lock().unwrap().clone();
    assert!(!calls.iter().any(|c| c.starts_with("receive:")));

    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Spent { amount_sat: 9 })
    );
    assert_eq!(
        m.finish(summary).unwrap(),
        tollgate_module_basic_rust::migration::MigrationFinish::Complete
    );
}

/// Definitive receive failures remain ordinary Failed (retriable on a later
/// boot — "failure is not cached") and still block finalization.
#[tokio::test]
async fn definitive_failure_is_failed_and_retriable() {
    let dir = tempfile::tempdir().unwrap();
    let m = setup(dir.path(), &["cashuA1"]);

    let sink = FakeSink {
        receive: [("cashuA1".to_string(), Rx::Definitive)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let summary = m.import_tokens(&sink).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Failed {
            reason: "token parse error: definitive failure".into()
        })
    );
    assert_eq!(
        m.finish(summary).unwrap(),
        tollgate_module_basic_rust::migration::MigrationFinish::Partial
    );

    // Next boot with the cause fixed converges to Imported.
    let sink2 = FakeSink {
        receive: [("cashuA1".to_string(), Rx::Ok(4))].into_iter().collect(),
        ..Default::default()
    };
    let summary2 = m.import_tokens(&sink2).await.unwrap();
    assert_eq!(summary2.imported, 4);
    assert_eq!(summary2.failed, 0);
    assert_eq!(
        fold_last(&read_journal(dir.path())).get("cashuA1"),
        Some(&TokenOutcome::Imported { amount_sat: 4 })
    );
}
