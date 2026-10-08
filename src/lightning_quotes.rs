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

use std::collections::BTreeMap;
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
    /// Allotment priced at creation from the terms then in force;
    /// settlement and status responses use this immutable value, never a
    /// re-derivation from possibly-changed config (0 = legacy record
    /// predating the field; priced then from current config).
    #[serde(default)]
    pub allotment: u64,
    /// Metric in force at creation; empty = legacy record (use config).
    #[serde(default)]
    pub metric: String,
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
    // BTreeMap: deterministic snapshot content, so an unchanged store
    // serializes to identical bytes (no gratuitous flash rewrites).
    quotes: RwLock<BTreeMap<String, LightningQuoteRecord>>,
}

impl QuoteStore {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(QUOTES_FILE);
        let quotes = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<Vec<LightningQuoteRecord>>(&body).ok())
            .map(|v| v.into_iter().map(|q| (q.quote.clone(), q)).collect())
            .unwrap_or_default();
        QuoteStore {
            path,
            quotes: RwLock::new(quotes),
        }
    }

    /// Insert or update a record and persist the snapshot.
    ///
    /// On persistence failure the in-memory transition is KEPT (it is
    /// true — value may already have moved) and the error is returned so
    /// callers can refuse to expose or advance on undurable state
    /// (AGENTS.md: a payment response is never returned before the
    /// record backing it is durable).
    pub async fn upsert(&self, record: LightningQuoteRecord) -> std::io::Result<()> {
        let mut quotes = self.quotes.write().await;
        quotes.insert(record.quote.clone(), record);
        let snapshot: Vec<LightningQuoteRecord> = quotes.values().cloned().collect();
        persist(&self.path, &snapshot)
    }

    /// Remove a record and persist; absent records are a no-op (no write).
    pub async fn remove(&self, quote: &str) -> std::io::Result<()> {
        let mut quotes = self.quotes.write().await;
        if quotes.remove(quote).is_none() {
            return Ok(());
        }
        let snapshot: Vec<LightningQuoteRecord> = quotes.values().cloned().collect();
        persist(&self.path, &snapshot)
    }

    pub async fn get(&self, quote: &str) -> Option<LightningQuoteRecord> {
        self.quotes.read().await.get(quote).cloned()
    }

    /// All not-yet-granted records, including expired/stale ones —
    /// settlement reconciles the authoritative mint state before any
    /// record is considered terminal.
    pub async fn ungranted(&self) -> Vec<LightningQuoteRecord> {
        self.quotes
            .read()
            .await
            .values()
            .filter(|q| !q.session_granted)
            .cloned()
            .collect()
    }

    /// Drop expired/stale settled records; called by the monitor's janitor
    /// pass. Ungranted records are never dropped here — settlement owns
    /// their terminal transition.
    pub async fn sweep(&self) {
        let now = now_secs();
        let mut quotes = self.quotes.write().await;
        let before = quotes.len();
        quotes.retain(|_, q| !(q.expired(now) || q.stale(now)) || !q.session_granted);
        if quotes.len() == before {
            // Nothing removed: rewriting would wear OpenWrt flash ~17k
            // times/day while idle (AGENTS.md flash-wear rule).
            return;
        }
        let snapshot: Vec<LightningQuoteRecord> = quotes.values().cloned().collect();
        if let Err(e) = persist(&self.path, &snapshot) {
            tracing::warn!(error = %e, "failed to persist lightning quotes sweep");
        }
    }
}

/// Durable atomic snapshot: tmp write → fsync → rename → dir fsync.
///
/// Without both syncs a power cut can leave the rename (or the tmp
/// contents) only in the page cache, resurrecting the previous snapshot
/// after restart — losing invoices and minted/granted transitions that
/// were reported as persisted (AGENTS.md L31-L38).
fn persist(path: &Path, quotes: &[LightningQuoteRecord]) -> std::io::Result<()> {
    use std::io::Write;

    let json = serde_json::to_string_pretty(quotes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Find the config entry for a quote's mint by canonical identity
/// (AGENTS.md: raw-string mint comparisons fork on alias spellings).
pub(crate) fn find_mint_config<'a>(
    config: &'a crate::config::Config,
    mint_url: &str,
) -> Option<&'a crate::config::MintConfig> {
    config
        .accepted_mints
        .iter()
        .find(|m| crate::mint_url::mint_urls_equal(&m.url, mint_url))
}

/// Settlement outcome for one monitored quote, returned for tests and
/// logging.
#[derive(Debug, Clone, PartialEq)]
pub enum SettleOutcome {
    StillUnpaid,
    /// Terminal: expired/stale AND the mint authoritatively reports
    /// Unpaid. No value moved; the monitor removes the record.
    ExpiredUnpaid,
    Granted {
        allotment: u64,
    },
    /// Paid and minted, but the session/gate step failed; retried on the
    /// next tick without re-minting.
    GrantFailed,
    Failed(String),
}

/// Exact typed comparison: only `Paid` and `Issued` authorize a mint.
///
/// The debug name of `MintQuoteState::Unpaid` lowercased contains "paid",
/// so substring matching on a formatted state reports every unpaid invoice
/// as paid (AGENTS.md: do not parse protocol states as substrings).
pub fn quote_state_allows_settlement(state: cdk::nuts::MintQuoteState) -> bool {
    matches!(
        state,
        cdk::nuts::MintQuoteState::Paid | cdk::nuts::MintQuoteState::Issued
    )
}

/// Portal-facing state string for an invoice query. Exact typed match:
/// `Unpaid` must never display as "paid".
pub fn quote_state_display(state: cdk::nuts::MintQuoteState) -> &'static str {
    if quote_state_allows_settlement(state) {
        "paid"
    } else {
        "unpaid"
    }
}

/// Settle one quote: mint if paid-and-unminted, then grant session + gate.
/// `minted`/`session_granted` transitions are persisted through the store
/// as they happen, so a crash mid-settle resumes correctly: minting never
/// repeats, granting retries. `sessions_dir` is explicit (not the env
/// override) so concurrent callers cannot race on the process-wide var.
pub async fn settle_quote(
    store: Arc<QuoteStore>,
    wallet: &crate::wallet::wallet::TollWallet,
    sessions: &Mutex<crate::session::SessionManager>,
    portal: &dyn crate::portal::CaptivePortal,
    config: &crate::config::Config,
    sessions_dir: &Path,
    record: LightningQuoteRecord,
) -> SettleOutcome {
    let mut record = record;

    if !record.minted {
        let status = match wallet
            .check_mint_quote_state(&record.mint_url, &record.quote)
            .await
        {
            Ok(s) => s,
            Err(e) => return SettleOutcome::Failed(e.to_string()),
        };
        if !quote_state_allows_settlement(status) {
            // Expiry may terminate a record ONLY after the authoritative
            // state is reconciled (AGENTS.md L64-L70): a payment landing
            // just before expiry — or the mint being unreachable until
            // after it — must still settle. Paid/Issued quotes fall
            // through to minting below regardless of local expiry.
            let now = now_secs();
            if status == cdk::nuts::MintQuoteState::Unpaid
                && (record.expired(now) || record.stale(now))
            {
                return SettleOutcome::ExpiredUnpaid;
            }
            return SettleOutcome::StillUnpaid;
        }
        match wallet.mint_tokens(&record.mint_url, &record.quote).await {
            Ok(_) => {
                record.minted = true;
                if let Err(e) = store.upsert(record.clone()).await {
                    return SettleOutcome::Failed(format!("quote persist failed: {e}"));
                }
            }
            Err(e) => {
                // AGENTS.md: an ambiguous mint result (30s timeout cancels
                // the saga mid-flight; the mint may have already issued)
                // must be reconciled, never inferred from error text.
                // check_mint_quote_state re-queries the authoritative
                // state AND resumes the quote's in-progress CDK saga
                // (CDK links it via `used_by_operation`), recovering
                // proofs the cancelled call already produced. Only a
                // post-reconciliation Issued state may mark the quote
                // minted.
                let state = match wallet
                    .check_mint_quote_state(&record.mint_url, &record.quote)
                    .await
                {
                    Ok(s) => s,
                    Err(re) => {
                        return SettleOutcome::Failed(format!(
                            "mint failed ({e}) and reconciliation failed ({re})"
                        ))
                    }
                };
                if state == cdk::nuts::MintQuoteState::Issued {
                    tracing::warn!(
                        quote = "…",
                        error = %e,
                        "mint errored but reconciliation confirms issued; marking minted"
                    );
                    record.minted = true;
                    if let Err(pe) = store.upsert(record.clone()).await {
                        return SettleOutcome::Failed(format!("quote persist failed: {pe}"));
                    }
                } else {
                    return SettleOutcome::Failed(e.to_string());
                }
            }
        }
    }

    // Pricing terms are frozen into the record at creation; only legacy
    // records (allotment == 0) fall back to current config (PR #22
    // r4070018414).
    let allotment = if record.allotment > 0 {
        record.allotment
    } else {
        let price_per_step = find_mint_config(config, &record.mint_url)
            .map(|m| m.price_per_step.max(1))
            .unwrap_or(1);
        (record.amount_sat / price_per_step) * config.step_size
    };
    let metric = if record.metric.is_empty() {
        config.metric.clone()
    } else {
        record.metric.clone()
    };

    if !record.allotment_added {
        // Crash idempotency across the two files (sessions.json and the
        // quote store cannot be written atomically together): the grant
        // key `ln:<quote>` rides IN the session record, so recovery can
        // prove the allotment already reached disk after a crash between
        // the session flush and the quote-marker write.
        let grant_id = format!("ln:{}", record.quote);
        let mut sm = sessions.lock().await;
        if !sm.has_grant(&record.mac, &grant_id) {
            sm.add_allotment(&record.mac, &metric, allotment, 3600, Some(&grant_id));
            // The session grant must be DURABLE before the quote marker
            // advances (AGENTS.md): a debounced save returns Ok without
            // writing, and a crash in that window used to leave
            // allotment_added=true durable while the allotment was not —
            // recovery then skipped re-adding it, losing the paid session.
            if let Err(e) = sm.save_now(sessions_dir) {
                tracing::warn!(error = %e, "session save failed; retrying next tick");
                drop(sm);
                return SettleOutcome::GrantFailed;
            }
        }
        drop(sm);
        record.allotment_added = true;
        if let Err(e) = store.upsert(record.clone()).await {
            return SettleOutcome::Failed(format!("quote persist failed: {e}"));
        }
    }

    // A tombstoned ln id means THIS quote's allotment was already
    // applied and its session has since expired or was revoked — the
    // allotment skip above is correct idempotency, but the gate must
    // NOT open: no session remains for the monitor to revoke, and an
    // open gate with no session is indefinite free access (Codex P1 on
    // #68, round 10). Terminalize the quote without touching the gate.
    if !sessions.lock().await.is_active(&record.mac) {
        let quote_id = record.quote.clone();
        let mac = record.mac.clone();
        record.session_granted = true;
        let grant_id = format!("ln:{}", quote_id);
        if let Err(e) = store.upsert(record).await {
            return SettleOutcome::Failed(format!("quote persist failed: {e}"));
        }
        let mut sm = sessions.lock().await;
        sm.forget_grants([grant_id]);
        if let Err(e) = sm.save_now(sessions_dir) {
            tracing::warn!(error = %e, "session save after ln-grant cleanup failed; debounced save will retry");
        }
        tracing::info!(quote = %quote_id, mac = %mac, "lightning quote settled against an expired session — terminalized without reopening the gate");
        return SettleOutcome::Granted { allotment };
    }

    if let Err(e) = portal.grant_access(&record.mac).await {
        tracing::warn!(mac = %record.mac, error = %e, "gate open failed for paid lightning quote; retrying next tick");
        return SettleOutcome::GrantFailed;
    }

    record.session_granted = true;
    let grant_id = format!("ln:{}", record.quote);
    if let Err(e) = store.upsert(record).await {
        return SettleOutcome::Failed(format!("quote persist failed: {e}"));
    }
    // The settle marker is durable: the grant id's idempotency job is
    // done — forget it (memory AND disk) so it cannot later resurrect
    // as a permanent tombstone when the session is removed (Codex P2 on
    // #68, r8).
    {
        let mut sm = sessions.lock().await;
        sm.forget_grants([grant_id]);
        if let Err(e) = sm.save_now(sessions_dir) {
            tracing::warn!(error = %e, "session save after ln-grant cleanup failed; debounced save will retry");
        }
    }
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
            let quote_id = record.quote.clone();
            match settle_quote(
                store.clone(),
                wallet,
                &state.sessions,
                state.portal.as_ref(),
                &state.config,
                &crate::config::config_dir(),
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
                SettleOutcome::ExpiredUnpaid => {
                    tracing::info!(
                        quote = %quote_id,
                        "lightning quote expired unpaid (mint-confirmed); removing record"
                    );
                    if let Err(e) = store.remove(&quote_id).await {
                        tracing::warn!(error = %e, "failed to persist expired-quote removal");
                    }
                }
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
            allotment: 0,
            metric: String::new(),
            allotment_added: false,
            session_granted: false,
        }
    }

    #[tokio::test]
    async fn store_survives_reload_and_tolerates_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = QuoteStore::load(dir.path());
        store.upsert(record("q1", false)).await.unwrap();

        let reloaded = QuoteStore::load(dir.path());
        assert_eq!(reloaded.get("q1").await.unwrap().quote, "q1");

        std::fs::write(dir.path().join(QUOTES_FILE), "not json").unwrap();
        assert!(QuoteStore::load(dir.path()).get("q1").await.is_none());
    }

    #[tokio::test]
    async fn settle_grants_session_and_gate_without_reminting() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(QuoteStore::load(dir.path()));
        store.upsert(record("q1", true)).await.unwrap();

        let mut wallet =
            crate::wallet::wallet::TollWallet::new([0u8; 64], vec![], dir.path().to_path_buf());
        // No mints are registered, but with minted=true the settle path
        // never touches the wallet — minting must not repeat.
        let _ = &mut wallet;

        let sessions = Mutex::new(SessionManager::new());
        let portal = FakePortal {
            grant_fails: false,
            granted: std::sync::Mutex::new(vec![]),
        };
        let config = Config::default();

        let outcome = settle_quote(
            store.clone(),
            &wallet,
            &sessions,
            &portal,
            &config,
            cfg_dir.path(),
            store.get("q1").await.unwrap(),
        )
        .await;

        assert_eq!(
            outcome,
            SettleOutcome::Granted {
                allotment: 10 * config.step_size
            }
        );
        assert_eq!(
            *portal.granted.lock().unwrap(),
            vec!["aa:bb:cc:dd:ee:ff".to_string()]
        );
        assert!(sessions.lock().await.is_active("aa:bb:cc:dd:ee:ff"));
        assert!(store.get("q1").await.unwrap().session_granted);
    }

    #[tokio::test]
    async fn settle_with_failing_gate_retries_without_reminting() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(QuoteStore::load(dir.path()));
        store.upsert(record("q2", true)).await.unwrap();

        let wallet =
            crate::wallet::wallet::TollWallet::new([0u8; 64], vec![], dir.path().to_path_buf());
        let sessions = Mutex::new(SessionManager::new());
        let portal = FakePortal {
            grant_fails: true,
            granted: std::sync::Mutex::new(vec![]),
        };
        let config = Config::default();

        let rec = store.get("q2").await.unwrap();
        let outcome = settle_quote(
            store.clone(),
            &wallet,
            &sessions,
            &portal,
            &config,
            cfg_dir.path(),
            rec,
        )
        .await;
        assert_eq!(outcome, SettleOutcome::GrantFailed);

        let rec = store.get("q2").await.unwrap();
        assert!(
            rec.minted,
            "minted stays recorded so the retry never mints twice"
        );
        assert!(
            rec.allotment_added,
            "allotment was added once despite the gate failure"
        );
        assert!(!rec.session_granted);

        let sessions2 = Mutex::new(SessionManager::new());
        let portal2 = FakePortal {
            grant_fails: false,
            granted: std::sync::Mutex::new(vec![]),
        };
        let outcome = settle_quote(
            store.clone(),
            &wallet,
            &sessions2,
            &portal2,
            &config,
            cfg_dir.path(),
            store.get("q2").await.unwrap(),
        )
        .await;
        assert_eq!(
            outcome,
            SettleOutcome::Granted {
                allotment: 10 * config.step_size
            }
        );
    }

    #[tokio::test]
    async fn ungranted_keeps_expired_records_for_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let store = QuoteStore::load(dir.path());

        let mut expired = record("old", false);
        expired.expiry = now_secs() - 1;
        store.upsert(expired).await.unwrap();

        let mut granted = record("done", true);
        granted.session_granted = true;
        granted.expiry = now_secs() - 1;
        store.upsert(granted).await.unwrap();

        store.upsert(record("live", false)).await.unwrap();

        // Expired-but-ungranted records MUST reach settlement: a payment
        // landing just before expiry (or a mint unreachable until after)
        // must still grant (PR #22 r4070018376). Only granted records are
        // excluded; terminality is decided on authoritative mint state.
        let pending: Vec<String> = store
            .ungranted()
            .await
            .into_iter()
            .map(|q| q.quote)
            .collect();
        assert!(pending.contains(&"live".to_string()));
        assert!(pending.contains(&"old".to_string()));
        assert!(!pending.contains(&"done".to_string()));

        // Sweep never drops ungranted records, expired or not — a paid
        // quote must not be garbage-collected out of its session.
        store.sweep().await;
        assert!(store.get("old").await.is_some());
        assert!(store.get("live").await.is_some());
        assert!(
            store.get("done").await.is_none(),
            "granted+expired is dropped"
        );
    }

    #[test]
    fn unpaid_state_never_allows_settlement_or_displays_paid() {
        use cdk::nuts::MintQuoteState;
        // Regression (PR #22/#23 review): `"unpaid".contains("paid")` is
        // true, so substring matching treated every unpaid invoice as paid
        // and fired a mint attempt on every monitor tick.
        assert!(!quote_state_allows_settlement(MintQuoteState::Unpaid));
        assert_eq!(quote_state_display(MintQuoteState::Unpaid), "unpaid");
        assert!(quote_state_allows_settlement(MintQuoteState::Paid));
        assert_eq!(quote_state_display(MintQuoteState::Paid), "paid");
        assert!(quote_state_allows_settlement(MintQuoteState::Issued));
        assert_eq!(quote_state_display(MintQuoteState::Issued), "paid");
    }
}
