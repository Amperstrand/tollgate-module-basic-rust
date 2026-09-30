//! Regression tests for the payment journal (#40): public API only, so
//! the §9 red-proof survives a parent-src checkout.

use std::collections::HashMap;

use tollgate_module_basic_rust::migration::TokenSink;
use tollgate_module_basic_rust::payment_journal::{
    append_entry, fold_last, journal_path, read_journal, reconcile, settled_outcome, summarize,
    token_id, PaymentEntry, PaymentPhase,
};
use tollgate_module_basic_rust::wallet::WalletError;

fn entry(_dir: &std::path::Path, id: &str, phase: PaymentPhase) -> PaymentEntry {
    let token = format!("cashuAtoken-{id}");
    PaymentEntry {
        id: token_id(&token),
        ts: 1,
        token,
        mac: "aa:bb:cc:dd:ee:01".into(),
        mint: "https://mint.example".into(),
        price_per_step: 2,
        step_size: 5000,
        metric: "milliseconds".into(),
        phase,
    }
}

#[derive(Default)]
struct FakeSink {
    spent: HashMap<String, Option<u64>>,
    err: bool,
}

#[async_trait::async_trait]
impl TokenSink for FakeSink {
    async fn receive(&self, _token: &str) -> Result<u64, WalletError> {
        unreachable!("reconcile never receives")
    }

    async fn token_spent(&self, token: &str) -> Result<Option<u64>, WalletError> {
        if self.err {
            return Err(WalletError::Timeout(std::time::Duration::from_secs(30)));
        }
        Ok(self.spent.get(token).copied().flatten())
    }

    async fn mint_has_unresolved_receive(&self, _token: &str) -> Result<bool, WalletError> {
        Ok(false)
    }
}

#[test]
fn journal_roundtrip_and_fold_last_wins() {
    let dir = tempfile::tempdir().unwrap();
    let e1 = entry(dir.path(), "a", PaymentPhase::Intent);
    append_entry(dir.path(), &e1).unwrap();
    append_entry(
        dir.path(),
        &entry(dir.path(), "a", PaymentPhase::Received { amount_sat: 8 }),
    )
    .unwrap();
    append_entry(dir.path(), &entry(dir.path(), "b", PaymentPhase::Intent)).unwrap();

    let entries = read_journal(dir.path());
    assert_eq!(entries.len(), 3);
    let folded = fold_last(&entries);
    assert_eq!(
        folded[&token_id("cashuAtoken-a")].phase,
        PaymentPhase::Received { amount_sat: 8 },
        "last entry per id wins"
    );
    assert_eq!(
        folded[&token_id("cashuAtoken-b")].phase,
        PaymentPhase::Intent
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(journal_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "journal carries bearer tokens: must be 0600");
    }
}

#[tokio::test]
async fn reconcile_spent_grants_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let e = entry(dir.path(), "a", PaymentPhase::TimeoutUnknown);
    append_entry(dir.path(), &e).unwrap();

    let sink = FakeSink {
        spent: [(e.token.clone(), Some(8u64))].into_iter().collect(),
        err: false,
    };
    let (report, grants) = reconcile(dir.path(), &sink).await;
    assert_eq!(report.granted_sessions, 1);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].1, 8);
    assert_eq!(
        grants[0].0.mac, "aa:bb:cc:dd:ee:01",
        "grant carries the customer MAC"
    );
    assert_eq!(
        grants[0].0.step_size, 5000,
        "grant carries the frozen pricing facts"
    );

    // The CALLER applies each grant durably, then appends the terminal
    // phase (the main.rs contract): simulate that here.
    for (g, amount) in &grants {
        append_entry(
            dir.path(),
            &PaymentEntry {
                phase: PaymentPhase::ReconcileSpent {
                    amount_sat: *amount,
                },
                ..g.clone()
            },
        )
        .unwrap();
    }
    assert_eq!(
        settled_outcome(dir.path(), &e.token),
        Some(PaymentPhase::ReconcileSpent { amount_sat: 8 })
    );

    // Second pass: journal settled — no new grants (idempotent).
    let (report2, grants2) = reconcile(dir.path(), &sink).await;
    assert_eq!(report2.granted_sessions, 0);
    assert!(grants2.is_empty());
}

#[tokio::test]
async fn reconcile_unspent_closes_with_nothing_owed() {
    let dir = tempfile::tempdir().unwrap();
    let e = entry(dir.path(), "a", PaymentPhase::Intent);
    append_entry(dir.path(), &e).unwrap();

    let sink = FakeSink {
        spent: [(e.token.clone(), None)].into_iter().collect(),
        err: false,
    };
    let (report, grants) = reconcile(dir.path(), &sink).await;
    assert_eq!(report.closed_unspent, 1);
    assert!(grants.is_empty());
    let last = fold_last(&read_journal(dir.path()))
        .remove(&token_id(&e.token))
        .map(|e| e.phase.clone());
    assert_eq!(last, Some(PaymentPhase::ReconcileUnspent));
}

#[tokio::test]
async fn reconcile_mint_down_leaves_undecided_for_next_boot() {
    let dir = tempfile::tempdir().unwrap();
    append_entry(
        dir.path(),
        &entry(dir.path(), "a", PaymentPhase::TimeoutUnknown),
    )
    .unwrap();

    let sink = FakeSink {
        err: true,
        ..Default::default()
    };
    let (report, grants) = reconcile(dir.path(), &sink).await;
    assert_eq!(report.undecided, 1);
    assert!(grants.is_empty());
    // Still needs reconciliation — never failed.
    assert_eq!(summarize(dir.path()).needs_reconciliation, 1);
}

#[tokio::test]
async fn reconcile_zero_steps_records_credit_material() {
    let dir = tempfile::tempdir().unwrap();
    // price_per_step = 2, spent amount 1 → zero steps (fees).
    let e = entry(dir.path(), "a", PaymentPhase::Intent);
    append_entry(dir.path(), &e).unwrap();

    let sink = FakeSink {
        spent: [(e.token.clone(), Some(1u64))].into_iter().collect(),
        err: false,
    };
    let (report, grants) = reconcile(dir.path(), &sink).await;
    assert_eq!(report.zero_steps, 1);
    assert!(
        grants.is_empty(),
        "zero steps must not grant an empty session"
    );
    let last = fold_last(&read_journal(dir.path()))
        .remove(&token_id(&e.token))
        .map(|e| e.phase.clone());
    assert_eq!(
        last,
        Some(PaymentPhase::ReconcileZeroSteps { amount_sat: 1 })
    );
}

#[test]
fn settled_outcome_ignores_unsettled_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    append_entry(dir.path(), &entry(dir.path(), "a", PaymentPhase::Intent)).unwrap();
    let token_a = "cashuAtoken-a".to_string();
    assert_eq!(settled_outcome(dir.path(), &token_a), None);
    assert_eq!(settled_outcome(dir.path(), "cashuAnever-seen"), None);
    assert_eq!(token_id("t1").len(), 64);
}

#[test]
fn summarize_counts_needing_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    append_entry(
        dir.path(),
        &entry(dir.path(), "a", PaymentPhase::TimeoutUnknown),
    )
    .unwrap();
    append_entry(
        dir.path(),
        &entry(dir.path(), "b", PaymentPhase::Received { amount_sat: 5 }),
    )
    .unwrap();
    let s = summarize(dir.path());
    assert_eq!(s.total, 2);
    assert_eq!(s.needs_reconciliation, 1);
    assert_eq!(s.received_sat, 5);
}
