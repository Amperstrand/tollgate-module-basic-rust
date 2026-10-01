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
fn intent_only_crash_retries_only_with_fresh_invoice() {
    let (d, _) = decide(Some(&PayoutPhase::Intent), false, false);
    assert_eq!(d, MeltDecision::Proceed);
    let (d, _) = decide(Some(&PayoutPhase::Intent), true, false);
    assert_eq!(
        d,
        MeltDecision::SkipSurface,
        "literal bolt11 ambiguity must never re-melt"
    );
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
