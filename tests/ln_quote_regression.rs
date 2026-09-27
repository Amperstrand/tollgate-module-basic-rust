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
        allotment: 0,
        metric: String::new(),
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

/// PR #22 r4070018382 / PR #23 r4070120792 (P1): sub-minimum amounts must
/// be rejected BEFORE a payable invoice is created — otherwise settlement
/// mints the paid value and grants a zero-step (unusable) session.
#[tokio::test]
#[serial_test::serial]
async fn sub_minimum_amount_rejected_before_invoice_creation() {
    let cfg_dir = tempfile::tempdir().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", cfg_dir.path());

    let mut config = tollgate_module_basic_rust::config::Config::default();
    config.accepted_mints = vec![tollgate_module_basic_rust::config::MintConfig {
        url: "https://mint.example".to_string(),
        price_per_step: 2,
        min_purchase_steps: 1,
        ..tollgate_module_basic_rust::config::MintConfig::default_production(
            "https://mint.example",
        )
    }];

    let identity = std::sync::Arc::new(
        tollgate_module_basic_rust::identity::MerchantIdentity::load_or_generate().unwrap(),
    );
    let state = tollgate_module_basic_rust::http::AppState {
        config: std::sync::Arc::new(config),
        identity,
        wallet: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
            tollgate_module_basic_rust::session::SessionManager::new(),
        )),
        portal: std::sync::Arc::new(tollgate_module_basic_rust::portal::NdsPortal::new()),
        verifier: std::sync::Arc::new(
            tollgate_module_basic_rust::wallet::verify::TokenVerifier::new(vec![]),
        ),
        rate_limiter: std::sync::Arc::new(
            tollgate_module_basic_rust::rate_limiter::RateLimiter::new(1000),
        ),
        ln_quotes: std::sync::Arc::new(QuoteStore::load(cfg_dir.path())),
    };

    let addr: std::net::SocketAddr = "127.0.0.1:59999".parse().unwrap();
    let response = tollgate_module_basic_rust::http::routes::ln_invoice::handle_create_ln_invoice(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(addr),
        axum::Json(tollgate_module_basic_rust::http::routes::ln_invoice::CreateInvoiceRequest {
            amount: 1,
            unit: None,
        }),
    )
    .await;

    assert_eq!(
        response.status(),
        axum::http::StatusCode::BAD_REQUEST,
        "1 sat against a 2 sat/step mint must be rejected"
    );
    let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("below minimum purchase"),
        "rejection must name the minimum-purchase reason, got: {body}"
    );

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

/// PR #22 r4070018389 / PR #23 r4070120810 (P1): alias-spelled mint URLs
/// must resolve to the registered wallet (one canonical identity), or a
/// quote surviving a restart with a re-spelled config is permanently
/// WalletNotFound and a paid invoice never grants.
#[tokio::test]
async fn alias_spelled_mint_resolves_to_single_registered_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = tollgate_module_basic_rust::wallet::TollWallet::new(
        [0u8; 64],
        vec![],
        dir.path().to_path_buf(),
    );

    wallet
        .ensure_mint("https://mint.example")
        .await
        .unwrap();
    wallet
        .ensure_mint("HTTPS://Mint.Example/")
        .await
        .expect("alias spelling must resolve to the registered wallet");
    assert_eq!(
        wallet.get_balance_by_mint().await.unwrap().len(),
        1,
        "alias spelling must not fork the wallet map"
    );

    let err = wallet
        .check_mint_quote_state("HTTPS://Mint.Example/", "nonexistent")
        .await
        .expect_err("unknown quote must error");
    assert!(
        !matches!(
            err,
            tollgate_module_basic_rust::error::WalletError::WalletNotFound(_)
        ),
        "alias spelling resolved past the wallet map, got {err:?}"
    );
}

/// PR #22 r4070018376 (P1): an invoice paid just before expiry — polled
/// after it — must remain settleable. The pre-fix `ungranted()` filter
/// excluded expired records forever, so the customer paid and never got
/// a session.
#[tokio::test]
async fn expired_ungranted_quotes_stay_visible_to_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let store = QuoteStore::load(dir.path());

    let mut expired = quote_record("paid-late");
    expired.expiry = now_secs() - 1;
    store.upsert(expired).await.unwrap();

    let pending = store.ungranted().await;
    assert!(
        pending.iter().any(|q| q.quote == "paid-late"),
        "expired-but-ungranted records must reach settlement"
    );
}

struct FakePortal {
    granted: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl tollgate_module_basic_rust::portal::CaptivePortal for FakePortal {
    async fn grant_access(&self, mac: &str) -> Result<(), tollgate_module_basic_rust::error::AppError> {
        self.granted.lock().unwrap().push(mac.to_string());
        Ok(())
    }
    async fn revoke_access(&self, _mac: &str) -> Result<(), tollgate_module_basic_rust::error::AppError> {
        Ok(())
    }
    async fn poll_usage(&self, _mac: &str) -> Result<(u64, u64), tollgate_module_basic_rust::error::AppError> {
        Ok((0, 0))
    }
    async fn is_authenticated(&self, _mac: &str) -> bool {
        false
    }
}

/// PR #22 r4070018414 (P2): settlement must use the pricing terms frozen
/// into the quote at creation, not a re-derivation from the (possibly
/// changed) config — a config change + restart while a quote is payable
/// used to grant a different product than the portal displayed.
#[tokio::test]
async fn settlement_uses_pricing_terms_frozen_at_creation() {
    use tollgate_module_basic_rust::lightning_quotes::{settle_quote, SettleOutcome};

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(QuoteStore::load(dir.path()));

    // Quote created under: metric "bytes", allotment 999. Config NOW says
    // metric "time", price 1 sat/step, step_size 100 → re-derivation
    // would grant 1000 "time" units instead of the paid 999 "bytes".
    let mut rec = quote_record("priced-then");
    rec.minted = true;
    rec.allotment = 999;
    rec.metric = "bytes".to_string();
    store.upsert(rec).await.unwrap();

    let config = tollgate_module_basic_rust::config::Config {
        metric: "time".to_string(),
        step_size: 100,
        ..tollgate_module_basic_rust::config::Config::default()
    };

    let wallet = tollgate_module_basic_rust::wallet::TollWallet::new(
        [0u8; 64],
        vec![],
        dir.path().to_path_buf(),
    );
    let sessions = tokio::sync::Mutex::new(tollgate_module_basic_rust::session::SessionManager::new());
    let portal = FakePortal {
        granted: std::sync::Mutex::new(vec![]),
    };

    let outcome = settle_quote(
        store.clone(),
        &wallet,
        &sessions,
        &portal,
        &config,
        store.get("priced-then").await.unwrap(),
    )
    .await;

    assert_eq!(outcome, SettleOutcome::Granted { allotment: 999 });
    let guard = sessions.lock().await;
    let session = guard.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(session.allotment, 999, "stored allotment must win");
    assert_eq!(session.metric, "bytes", "stored metric must win");
}

/// PR #22 r4070018400 / PR #23 r4070120831: the monitor sweeps every 5s;
/// a rewrite on every tick is ~17k flash writes/day on OpenWrt while
/// idle. A sweep that removes nothing must not touch the file.
#[tokio::test]
async fn sweep_touches_nothing_when_no_record_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = QuoteStore::load(dir.path());
    store.upsert(quote_record("live")).await.unwrap();

    let path = dir.path().join(QUOTES_FILE);
    let before = std::fs::metadata(&path).unwrap();
    let content_before = std::fs::read(&path).unwrap();

    // Exceed the kernel's coarse timestamp granularity so a rewrite
    // would be observable in mtime.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    for _ in 0..3 {
        store.sweep().await;
    }

    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(
        before.modified().unwrap(),
        after.modified().unwrap(),
        "no-op sweep must not rewrite the quote file"
    );
    assert_eq!(content_before, std::fs::read(&path).unwrap());

    // A sweep that DOES remove a terminal record still persists.
    let mut expired_granted = quote_record("done");
    expired_granted.expiry = now_secs() - 1;
    expired_granted.session_granted = true;
    store.upsert(expired_granted).await.unwrap();
    store.sweep().await;
    assert!(store.get("done").await.is_none());
}
