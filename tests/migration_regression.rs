//! Regression tests for the PR #21 Codex findings on first-boot migration
//! value safety. Kept in `tests/` (public API only) so the §9 red-proof
//! protocol can check out pre-fix `src/` while these tests remain.

use std::collections::HashMap;
use std::path::PathBuf;

use tollgate_module_basic_rust::migration::{
    FirstBootMigration, JournalEntry, TokenOutcome, TokenSink, JOURNAL_NAME, OLD_DB_NAME,
    TOKENS_FILE_NAME,
};

#[derive(Debug, Default)]
struct FakeSink {
    /// token -> mint-side NUT-07 answer: `Some(amount)` = all proofs spent,
    /// `None` = still spendable.
    spent: HashMap<String, Option<u64>>,
    /// token -> receive result.
    receive: HashMap<String, Rx>,
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
    async fn receive(&self, token: &str) -> Result<u64, tollgate_module_basic_rust::wallet::WalletError> {
        use tollgate_module_basic_rust::wallet::WalletError;
        self.calls.lock().unwrap().push(format!("receive:{token}"));
        match self.receive.get(token).cloned().unwrap_or(Rx::Ok(1)) {
            Rx::Ok(v) => Ok(v),
            Rx::Timeout => Err(WalletError::Timeout(std::time::Duration::from_secs(30))),
            Rx::Definitive => Err(WalletError::TokenParse("definitive failure".into())),
        }
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
