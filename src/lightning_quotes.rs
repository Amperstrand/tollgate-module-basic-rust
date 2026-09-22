//! Durable Lightning quote lifecycle (#9): create → persist → monitor →
//! mint-on-paid → grant session + gate → mark granted.
//!
//! The pre-hardening flow kept quotes in a process-memory map and never
//! granted anything when an invoice was paid — a paid invoice minted
//! nobody's tokens and the customer had no access and no refund. This
//! module persists every quote to disk (atomic tmp+rename), restores
//! them at boot, and runs a monitor that settles paid quotes exactly
//! once: mint (once, tracked by `minted_sat`), session (durable before
//! the gate attempt), gate, then `session_granted`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

pub const QUOTES_FILE: &str = "lightning-quotes.json";
const RETENTION_SECS: u64 = 6 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LightningQuoteRecord {
    pub quote: String,
    pub mint_url: String,
    pub mac: String,
    pub amount_sat: u64,
    pub created_at: u64,
    pub expiry: u64,
    /// Mint succeeded; `amount_sat` tokens are in the wallet. Minting must
    /// never run again for this quote (a second mint on an ISSUED quote
    /// fails at the mint).
    pub minted: bool,
    /// The session's allotment was added exactly once. Gate retries must
    /// not accumulate allotment on every tick.
    pub allotment_added: bool,
    pub session_granted: bool,
}

impl LightningQuoteRecord {
    fn expired(&self, now: u64) -> bool {
        self.expiry != 0 && self.expiry < now
    }

    fn stale(&self, now: u64) -> bool {
        now.saturating_sub(self.created_at) > RETENTION_SECS
    }
}

pub struct QuoteStore {
    path: PathBuf,
    quotes: RwLock<HashMap<String, LightningQuoteRecord>>,
}

impl QuoteStore {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(QUOTES_FILE);
        let quotes = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<Vec<LightningQuoteRecord>>(&body).ok())
            .map(|v| v.into_iter().map(|q| (q.quote.clone(), q)).collect())
            .unwrap_or_default();
        QuoteStore { path, quotes: RwLock::new(quotes) }
    }

    pub async fn upsert(&self, record: LightningQuoteRecord) {
        let mut quotes = self.quotes.write().await;
        quotes.insert(record.quote.clone(), record);
        let snapshot: Vec<LightningQuoteRecord> = quotes.values().cloned().collect();
        persist(&self.path, &snapshot);
    }

    pub async fn get(&self, quote: &str) -> Option<LightningQuoteRecord> {
        self.quotes.read().await.get(quote).cloned()
    }

    pub async fn ungranted(&self) -> Vec<LightningQuoteRecord> {
        let now = now_secs();
        self.quotes
            .read()
            .await
            .values()
            .filter(|q| !q.session_granted && !q.expired(now) && !q.stale(now))
            .cloned()
            .collect()
    }

    /// Drop expired/stale settled records; called by the monitor's janitor
    /// pass. Kept records are persisted back atomically.
    pub async fn sweep(&self) {
        let now = now_secs();
        let mut quotes = self.quotes.write().await;
        quotes.retain(|_, q| !(q.expired(now) || q.stale(now)) || !q.session_granted);
        let snapshot: Vec<LightningQuoteRecord> = quotes.values().cloned().collect();
        persist(&self.path, &snapshot);
    }
}

fn persist(path: &Path, quotes: &[LightningQuoteRecord]) {
    let Ok(json) = serde_json::to_string_pretty(quotes) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
        tracing::warn!(path = %path.display(), "failed to persist lightning quotes");
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Settlement outcome for one monitored quote, returned for tests and
/// logging.
#[derive(Debug, Clone, PartialEq)]
pub enum SettleOutcome {
    StillUnpaid,
    Granted { allotment: u64 },
    /// Paid and minted, but the session/gate step failed; retried on the
    /// next tick without re-minting.
    GrantFailed,
    Failed(String),
}

/// Settle one quote: mint if paid-and-unminted, then grant session + gate.
/// `minted`/`session_granted` transitions are persisted through the store
/// as they happen, so a crash mid-settle resumes correctly: minting never
/// repeats, granting retries.
pub async fn settle_quote(
    store: Arc<QuoteStore>,
    wallet: &crate::wallet::wallet::TollWallet,
    sessions: &Mutex<crate::session::SessionManager>,
    portal: &dyn crate::portal::CaptivePortal,
    config: &crate::config::Config,
    record: LightningQuoteRecord,
) -> SettleOutcome {
    let mut record = record;

    if !record.minted {
        let status = match wallet.check_mint_quote(&record.mint_url, &record.quote).await {
            Ok(s) => s,
            Err(e) => return SettleOutcome::Failed(e.to_string()),
        };
        if !status.to_lowercase().contains("paid") && !status.to_lowercase().contains("issued") {
            return SettleOutcome::StillUnpaid;
        }
        match wallet.mint_tokens(&record.mint_url, &record.quote).await {
            Ok(_) => {
                record.minted = true;
                store.upsert(record.clone()).await;
            }
            Err(e) => {
                // A quote already ISSUED at the mint rejects a second mint;
                // treat that specific case as minted rather than stuck.
                let msg = e.to_string().to_lowercase();
                if msg.contains("issued") || msg.contains("already") {
                    record.minted = true;
                    store.upsert(record.clone()).await;
                } else {
                    return SettleOutcome::Failed(e.to_string());
                }
            }
        }
    }

    let mint_cfg = config
        .accepted_mints
        .iter()
        .find(|m| m.url.trim_end_matches('/') == record.mint_url.trim_end_matches('/'));
    let price_per_step = mint_cfg.map(|m| m.price_per_step.max(1)).unwrap_or(1);
    let steps = record.amount_sat / price_per_step;
    let allotment = steps * config.step_size;

    if !record.allotment_added {
        let mut sm = sessions.lock().await;
        sm.add_allotment(&record.mac, &config.metric, allotment, 3600);
        if let Err(e) = sm.save_to_disk(&crate::config::config_dir()) {
            tracing::warn!(error = %e, "session save debounced; monitor flush will persist");
        }
        drop(sm);
        record.allotment_added = true;
        store.upsert(record.clone()).await;
    }

    if let Err(e) = portal.grant_access(&record.mac).await {
        tracing::warn!(mac = %record.mac, error = %e, "gate open failed for paid lightning quote; retrying next tick");
        return SettleOutcome::GrantFailed;
    }

    record.session_granted = true;
    store.upsert(record).await;
    SettleOutcome::Granted { allotment }
}

/// Monitor loop: poll ungranted quotes, settle them, sweep stale records.
pub async fn run_monitor(
    state: Arc<crate::http::AppState>,
    store: Arc<QuoteStore>,
    poll_interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    loop {
        ticker.tick().await;
        let records = store.ungranted().await;
        if records.is_empty() {
            store.sweep().await;
            continue;
        }
        let wallet_guard = state.wallet.read().await;
        let Some(wallet) = wallet_guard.as_ref() else {
            continue;
        };
        for record in records {
            match settle_quote(
                store.clone(),
                wallet,
                &state.sessions,
                state.portal.as_ref(),
                &state.config,
                record,
            )
            .await
            {
                SettleOutcome::Granted { allotment } => tracing::info!(
                    quote = "…",
                    mac = "…",
                    allotment,
                    "lightning quote settled: session granted"
                ),
                SettleOutcome::GrantFailed | SettleOutcome::StillUnpaid => {}
                SettleOutcome::Failed(e) => {
                    tracing::warn!(error = %e, "lightning quote settle attempt failed; retrying next poll")
                }
            }
        }
        store.sweep().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::AppError;
    use crate::portal::CaptivePortal;
    use crate::session::SessionManager;
    use async_trait::async_trait;

    struct FakePortal {
        grant_fails: bool,
        granted: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CaptivePortal for FakePortal {
        async fn grant_access(&self, mac: &str) -> Result<(), AppError> {
            if self.grant_fails {
                return Err(AppError::Wallet(crate::error::WalletError::Timeout(
                    std::time::Duration::from_secs(1),
                )));
            }
            self.granted.lock().unwrap().push(mac.to_string());
            Ok(())
        }
        async fn revoke_access(&self, _mac: &str) -> Result<(), AppError> {
            Ok(())
        }
        async fn poll_usage(&self, _mac: &str) -> Result<(u64, u64), AppError> {
            Ok((0, 0))
        }
        async fn is_authenticated(&self, _mac: &str) -> bool {
            false
        }
    }

    fn record(quote: &str, minted: bool) -> LightningQuoteRecord {
        LightningQuoteRecord {
            quote: quote.to_string(),
            mint_url: "https://mint.example".to_string(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            amount_sat: 10,
            created_at: now_secs(),
            expiry: now_secs() + 3600,
            minted,
            allotment_added: false,
            session_granted: false,
        }
    }

    #[tokio::test]
    async fn store_survives_reload_and_tolerates_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = QuoteStore::load(dir.path());
        store.upsert(record("q1", false)).await;

        let reloaded = QuoteStore::load(dir.path());
        assert_eq!(reloaded.get("q1").await.unwrap().quote, "q1");

        std::fs::write(dir.path().join(QUOTES_FILE), "not json").unwrap();
        assert!(QuoteStore::load(dir.path()).get("q1").await.is_none());
    }

    #[tokio::test]
    async fn settle_grants_session_and_gate_without_reminting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(QuoteStore::load(dir.path()));
        store.upsert(record("q1", true)).await;

        let mut wallet = crate::wallet::wallet::TollWallet::new(
            [0u8; 64],
            vec![],
            dir.path().to_path_buf(),
        );
        // No mints are registered, but with minted=true the settle path
        // never touches the wallet — minting must not repeat.
        let _ = &mut wallet;

        let sessions = Mutex::new(SessionManager::new());
        let portal = FakePortal { grant_fails: false, granted: std::sync::Mutex::new(vec![]) };
        let config = Config::default();

        let outcome = settle_quote(
            store.clone(),
            &wallet,
            &sessions,
            &portal,
            &config,
            store.get("q1").await.unwrap(),
        )
        .await;

        assert_eq!(outcome, SettleOutcome::Granted { allotment: 10 * config.step_size });
        assert_eq!(*portal.granted.lock().unwrap(), vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert!(sessions.lock().await.is_active("aa:bb:cc:dd:ee:ff"));
        assert!(store.get("q1").await.unwrap().session_granted);
    }

    #[tokio::test]
    async fn settle_with_failing_gate_retries_without_reminting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(QuoteStore::load(dir.path()));
        store.upsert(record("q2", true)).await;

        let wallet = crate::wallet::wallet::TollWallet::new(
            [0u8; 64],
            vec![],
            dir.path().to_path_buf(),
        );
        let sessions = Mutex::new(SessionManager::new());
        let portal = FakePortal { grant_fails: true, granted: std::sync::Mutex::new(vec![]) };
        let config = Config::default();

        let rec = store.get("q2").await.unwrap();
        let outcome =
            settle_quote(store.clone(), &wallet, &sessions, &portal, &config, rec).await;
        assert_eq!(outcome, SettleOutcome::GrantFailed);

        let rec = store.get("q2").await.unwrap();
        assert!(rec.minted, "minted stays recorded so the retry never mints twice");
        assert!(rec.allotment_added, "allotment was added once despite the gate failure");
        assert!(!rec.session_granted);

        let sessions2 = Mutex::new(SessionManager::new());
        let portal2 = FakePortal { grant_fails: false, granted: std::sync::Mutex::new(vec![]) };
        let outcome = settle_quote(
            store.clone(),
            &wallet,
            &sessions2,
            &portal2,
            &config,
            store.get("q2").await.unwrap(),
        )
        .await;
        assert_eq!(outcome, SettleOutcome::Granted { allotment: 10 * config.step_size });
    }

    #[tokio::test]
    async fn ungranted_filters_expired_and_granted() {
        let dir = tempfile::tempdir().unwrap();
        let store = QuoteStore::load(dir.path());

        let mut expired = record("old", false);
        expired.expiry = now_secs() - 1;
        store.upsert(expired).await;

        let mut granted = record("done", true);
        granted.session_granted = true;
        store.upsert(granted).await;

        store.upsert(record("live", false)).await;

        let pending: Vec<String> =
            store.ungranted().await.into_iter().map(|q| q.quote).collect();
        assert_eq!(pending, vec!["live".to_string()]);
    }
}
