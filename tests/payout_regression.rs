//! Regression tests for the payout journal (#41): public API only, so the
//! §9 red-proof survives a parent-src checkout (compile-level red — the
//! module does not exist pre-fix; behavioral green at HEAD).

use tollgate_module_basic_rust::payout_journal::{
    append_entry, decide, entry_id, fold_last, journal_path, last_for, read_journal, summarize,
    MeltDecision, PayoutEntry, PayoutPhase,
};

fn entry(_id: &str, phase: PayoutPhase, literal: bool) -> PayoutEntry {
    let (mint, identity, invoice) = ("https://mint.example", "owner", "lnbc1invoice");
    PayoutEntry {
        id: entry_id(mint, identity, invoice),
        token: None,
        ts: 1,
        mint: mint.to_string(),
        identity: identity.to_string(),
        invoice: invoice.to_string(),
        amount_sat: 100,
        literal_invoice: literal,
        phase,
    }
}

#[test]
fn journal_roundtrip_last_wins_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    append_entry(dir.path(), &entry("a", PayoutPhase::Intent, false)).unwrap();
    append_entry(dir.path(), &entry("a", PayoutPhase::Paid, false)).unwrap();

    let entries = read_journal(dir.path());
    assert_eq!(entries.len(), 2);
    let folded = fold_last(&entries);
    let id = entry_id("https://mint.example", "owner", "lnbc1invoice");
    assert_eq!(folded[&id].phase, PayoutPhase::Paid, "last entry wins");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(journal_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// The #41 wedge, decided: an ambiguous melt never blocks the plan —
/// SkipSurface means "this melt is skipped, the others proceed".
#[test]
fn ambiguous_unresolved_saga_skips_but_does_not_block() {
    let (d, _) = decide(Some(&PayoutPhase::Ambiguous), false, true);
    assert_eq!(d, MeltDecision::SkipSurface);
}

/// Once the CDK melt saga settles, the ambiguity closes as resolved and
/// the melt is never re-attempted.
#[test]
fn ambiguous_resolved_saga_closes_as_resolved() {
    let (d, advance) = decide(Some(&PayoutPhase::Ambiguous), false, false);
    assert_eq!(d, MeltDecision::SkipDone);
    assert_eq!(advance, Some(PayoutPhase::Resolved));
}

/// Crash between intent and melt: retry is safe with a fresh invoice
/// (LNURL fetches one per attempt) but NEVER with a literal bolt11 —
/// the already-paid wedge from the issue.
#[test]
fn intent_only_crash_never_remelts_same_invoice() {
    // Codex P1 on #45: the melt may have run before the crash — the same
    // invoice is never re-attempted. Fresh-invoice attempts land on a new
    // journal key and are unaffected.
    let (d, _) = decide(Some(&PayoutPhase::Intent), false, false);
    assert_eq!(d, MeltDecision::SkipSurface);
    let (d, _) = decide(Some(&PayoutPhase::Intent), true, false);
    assert_eq!(d, MeltDecision::SkipSurface);
}

#[test]
fn settled_ambiguity_literal_stays_surfaced() {
    // Compensated is indistinguishable from paid, and a literal invoice
    // has no fresh-invoice fallback — close the entry but surface it.
    let (d, advance) = decide(Some(&PayoutPhase::Ambiguous), true, false);
    assert_eq!(d, MeltDecision::SkipSurface);
    assert_eq!(advance, Some(PayoutPhase::Resolved));
}

/// …and it STAYS surfaced on every later tick: a literal `Resolved`
/// record must never fall into the unconditional done arm and report
/// AlreadyDone for a melt that may have been compensated (Codex P1 on
/// #45, round 2).
#[test]
fn resolved_literal_stays_surfaced_forever() {
    let (d, _) = decide(Some(&PayoutPhase::Resolved), true, false);
    assert_eq!(d, MeltDecision::SkipSurface);
    // Fresh invoices: done is correct — a compensated melt restored the
    // balance and the next tick's plan re-pays.
    let (d, _) = decide(Some(&PayoutPhase::Resolved), false, false);
    assert_eq!(d, MeltDecision::SkipDone);
}

#[test]
fn settled_ambiguity_fresh_invoice_closes_done() {
    // A compensated melt restores the balance; the next tick's plan
    // re-pays with a fresh invoice.
    let (d, advance) = decide(Some(&PayoutPhase::Ambiguous), false, false);
    assert_eq!(d, MeltDecision::SkipDone);
    assert_eq!(advance, Some(PayoutPhase::Resolved));
}

#[test]
fn paid_and_failed_semantics() {
    assert_eq!(
        decide(Some(&PayoutPhase::Paid), false, false).0,
        MeltDecision::SkipDone
    );
    assert_eq!(
        decide(Some(&PayoutPhase::Resolved), false, false).0,
        MeltDecision::SkipDone
    );
    assert_eq!(
        decide(
            Some(&PayoutPhase::Failed { reason: "x".into() }),
            false,
            false
        )
        .0,
        MeltDecision::Proceed,
        "definitive failures are retriable with a fresh invoice"
    );
    assert_eq!(decide(None, false, false).0, MeltDecision::Proceed);
}

/// A fresh invoice waits while ANY melt saga for the mint is unresolved
/// (Codex P1 on #45, round 4): the prior payout's outcome is undecided,
/// and melting from the reserved/reduced balance risks double-payment or
/// split skew once it settles.
#[test]
fn fresh_invoice_waits_while_prior_saga_unresolved() {
    let (d, _) = decide(None, false, true);
    assert_eq!(d, MeltDecision::SkipSurface);
    let (d, _) = decide(None, true, true);
    assert_eq!(d, MeltDecision::SkipSurface);
}

#[test]
fn summarize_and_last_for() {
    let dir = tempfile::tempdir().unwrap();
    append_entry(dir.path(), &entry("a", PayoutPhase::Ambiguous, false)).unwrap();
    let s = summarize(dir.path());
    assert_eq!((s.total, s.ambiguous, s.paid_sat), (1, 1, 0));

    append_entry(
        dir.path(),
        &PayoutEntry {
            phase: PayoutPhase::Paid,
            ..entry("a", PayoutPhase::Paid, false)
        },
    )
    .unwrap();
    assert_eq!(
        last_for(dir.path(), "https://mint.example", "owner", "lnbc1invoice"),
        Some(PayoutPhase::Paid)
    );
    let s = summarize(dir.path());
    assert_eq!((s.total, s.ambiguous, s.paid_sat), (1, 0, 100));
}

#[test]
fn resolved_ambiguity_is_visible_but_not_counted_paid() {
    // Codex P2 on #45: Resolved may have been compensated — it must not
    // inflate paid_sat, but it stays visible in total.
    let dir = tempfile::tempdir().unwrap();
    append_entry(
        dir.path(),
        &PayoutEntry {
            phase: PayoutPhase::Resolved,
            ..entry("a", PayoutPhase::Resolved, true)
        },
    )
    .unwrap();
    let s = summarize(dir.path());
    assert_eq!((s.total, s.paid_sat, s.ambiguous), (1, 0, 0));
}

#[test]
fn compaction_keeps_last_per_id_beyond_threshold() {
    use tollgate_module_basic_rust::payout_journal::compact_if_large;
    let dir = tempfile::tempdir().unwrap();
    // Real churn shape: each invoice id accrues an intent + terminal line
    // (2 lines per payout); compaction keeps the last line per id.
    for i in 0..30 {
        let inv = format!("inv-{i}");
        let mut e = entry("a", PayoutPhase::Intent, false);
        e.id = entry_id("https://mint.example", "owner", &inv);
        e.invoice = inv;
        append_entry(dir.path(), &e).unwrap();
        e.phase = PayoutPhase::Paid;
        append_entry(dir.path(), &e).unwrap();
    }
    assert_eq!(read_journal(dir.path()).len(), 60);

    assert!(compact_if_large(dir.path(), 10).unwrap());
    let after = read_journal(dir.path());
    assert_eq!(
        after.len(),
        30,
        "one line per id — the 2x factor is bounded"
    );
    assert!(
        after.iter().all(|e| e.phase == PayoutPhase::Paid),
        "last per id wins"
    );
    // Below threshold: no-op.
    assert!(!compact_if_large(dir.path(), 100).unwrap());
}

#[test]
fn drain_token_field_round_trips() {
    // #46: successful drains record the delivered token for audit.
    let dir = tempfile::tempdir().unwrap();
    append_entry(
        dir.path(),
        &PayoutEntry {
            token: Some("cashuAdelivered".to_string()),
            phase: PayoutPhase::Paid,
            ..entry("a", PayoutPhase::Paid, false)
        },
    )
    .unwrap();
    let last = read_journal(dir.path()).pop().unwrap();
    assert_eq!(last.token.as_deref(), Some("cashuAdelivered"));
    assert_eq!(last.phase, PayoutPhase::Paid);
}
