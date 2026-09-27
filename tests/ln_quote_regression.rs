//! Regression tests pinning the Codex review findings from PRs #22/#23
//! (merged 5df33eb / 6c68e21). Each test names the failure mode it guards.


use tollgate_module_basic_rust::lightning_quotes::{
    now_secs, LightningQuoteRecord, QuoteStore, QUOTES_FILE,
};

fn quote_record(quote: &str) -> LightningQuoteRecord {
    LightningQuoteRecord {
        quote: quote.to_string(),
        mint_url: "https://mint.example".to_string(),
        mac: "aa:bb:cc:dd:ee:ff".to_string(),
        amount_sat: 10,
        created_at: now_secs(),
        expiry: now_secs() + 3600,
        minted: false,
        allotment_added: false,
        session_granted: false,
    }
}

/// PR #22 r4070018380 / PR #23 r4070120762 (P1): a full or read-only
/// filesystem must surface an error from `upsert` — the pre-fix code
/// logged and reported success, exposing invoices whose only record was
/// in memory.
///
/// The tmp path is made a directory so `fs::write` fails with `EISDIR`
/// regardless of the uid the suite runs under (chmod is not root-proof).
#[tokio::test]
async fn upsert_reports_persist_failure_and_keeps_memory_truth() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(format!("{QUOTES_FILE}.tmp"))).unwrap();

    let store = QuoteStore::load(dir.path());
    let outcome = store.upsert(quote_record("q1")).await;
    assert!(
        outcome.is_err(),
        "persist failure must propagate, not be swallowed"
    );
    // The in-memory transition is kept: the record's state is true even
    // though the disk snapshot is not.
    assert!(store.get("q1").await.is_some());

    // Once the obstruction is gone, a later transition persists fine.
    std::fs::remove_dir(dir.path().join(format!("{QUOTES_FILE}.tmp"))).unwrap();
    store.upsert(quote_record("q1")).await.unwrap();
    let reloaded = QuoteStore::load(dir.path());
    assert_eq!(reloaded.get("q1").await.unwrap().quote, "q1");
}

/// PR #23 r4070120800 companion: sessions and the quote marker live in two
/// files; the store must expose removal so terminal records do not linger
/// (and so a withheld create can roll back).
#[tokio::test]
async fn remove_persists_and_is_noop_for_absent_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = QuoteStore::load(dir.path());
    store.upsert(quote_record("q1")).await.unwrap();

    store.remove("q1").await.unwrap();
    assert!(store.get("q1").await.is_none());
    assert!(QuoteStore::load(dir.path()).get("q1").await.is_none());

    // Absent record: no error, no file churn.
    store.remove("nope").await.unwrap();
}
