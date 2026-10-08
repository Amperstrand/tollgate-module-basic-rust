//! TollWallet — CDK wallet wrapper for tollgate-module-basic-rust.
//!
//! Replaces gonuts `wallet.Wallet` with CDK `cdk::Wallet`. CDK's saga pattern
//! makes operations atomic, eliminating the swap-counter race.
//!
//! # Mapping (13 gonuts call sites → CDK)
//!
//! | gonuts method            | CDK equivalent                                |
//! |-------------------------|-----------------------------------------------|
//! | `wallet.LoadWallet`     | `Wallet::new(mint_url, unit, localstore, seed)` |
//! | `wallet.AddMint`        | `Wallet::new` for that mint (multi-mint map)   |
//! | `wallet.Shutdown`       | drop `Wallet` (closes DB)                     |
//! | `wallet.Receive`        | `wallet.receive(token_str, ReceiveOptions)`    |
//! | `wallet.Send`           | `wallet.prepare_send(amount, opts).confirm()` |
//! | `wallet.SendWithOptions`| `prepare_send` with `SendKind::OnlineTolerance` |
//! | `wallet.RequestMint`    | `wallet.mint_quote(BOLT11, amount, ...)`      |
//! | `wallet.MintQuoteState` | `wallet.check_mint_quote_status(&id)`          |
//! | `wallet.MintTokens`     | `wallet.mint(&id, SplitTarget, None)`          |
//! | `wallet.GetBalance`     | `wallet.total_balance()`                       |
//! | `wallet.GetBalanceByMints` | per-wallet `total_balance()`                |
//! | `wallet.RequestMeltQuote`| `wallet.melt_quote(BOLT11, invoice, ...)`     |
//! | `wallet.Melt`           | `wallet.prepare_melt(quote_id, meta).confirm()`|

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cdk::amount::SplitTarget;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod, Token as CdkToken};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;
#[cfg(test)]
use rand::Rng;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

/// Keyset hygiene report for one mint (R10/#13): what fraction of
/// unspent proofs sit on keysets the mint reports inactive, and
/// which held keysets expire soon (final_expiry within `days`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeysetHygiene {
    pub unspent_proofs: usize,
    pub proofs_on_inactive_keysets: usize,
    pub soonest_expiry_keysets: Vec<String>,
}

/// Tri-state NUT-07 outcome for a token (reconciliation callers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCheckState {
    Spent(u64),
    Unspent,
    /// Proofs are in-flight at the mint — the outcome is still ambiguous.
    Pending,
}

/// Operator-facing result of a NUT-13 restore per mint (sats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoredSummary {
    pub spent_sat: u64,
    pub unspent_sat: u64,
    pub pending_sat: u64,
}

/// BIP39 mnemonic phrase -> 64-byte wallet seed (passphrase-less, the
/// ecosystem convention CDK's own tooling uses). Rejects invalid phrases.
fn mnemonic_to_seed(phrase: &str) -> Result<[u8; 64], WalletError> {
    let mnemonic: bip39::Mnemonic = phrase
        .parse()
        .map_err(|e| WalletError::Database(format!("invalid wallet mnemonic: {e}")))?;
    Ok(mnemonic.to_seed(""))
}

/// Default receive/send/melt timeout (matches Go's 30s).
const OP_TIMEOUT: Duration = Duration::from_secs(30);
/// Recovery may replay/restore several sagas; CDK's own replay budget is
/// ~60s, so 120s covers one full attempt (replay + checkstate + restore).
/// While it runs it holds the mint's wallet mutex, so this is also the
/// worst-case starvation window for same-mint ops after a timeout event —
/// anything longer trades availability for nothing (the saga survives a
/// failed attempt and is retried on the next trigger or boot).
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(120);

pub use crate::error::WalletError;

/// One canonical mint identity for every persisted/compared URL.
/// Delegates to [`crate::mint_url::canonicalize_mint_url`] (Go
/// `NormalizeMintURL` parity) so the whole crate compares and persists
/// through a single canonical form — alias spellings must not fork wallet
/// map keys, quote records, and DB filenames.
pub fn canonical_mint_url(url: &str) -> String {
    crate::mint_url::canonicalize_mint_url(url)
}

/// Pure NUT-07 classification of one token from per-proof mint states.
///
/// Y = hash_to_curve(secret), so no keyset resolution is needed. A proof
/// absent from the mint's response is treated as Indeterminate (deferred),
/// never as silently unspent.
fn classify_spend_state(
    pairs: &[(cdk::Amount, cdk::secret::Secret)],
    amount_sat: u64,
    spent_ys: &std::collections::HashSet<cdk::nuts::PublicKey>,
    pending_ys: &std::collections::HashSet<cdk::nuts::PublicKey>,
    answered_ys: &std::collections::HashSet<cdk::nuts::PublicKey>,
) -> Result<crate::migration::TokenSpendState, WalletError> {
    use crate::migration::TokenSpendState;
    use cdk::dhke::hash_to_curve;

    if pairs.is_empty() {
        return Err(WalletError::TokenParse("token has no proofs".into()));
    }
    let token_ys: Vec<Option<cdk::nuts::PublicKey>> = pairs
        .iter()
        .map(|(_, secret)| hash_to_curve(secret.as_bytes()).ok())
        .collect();
    let unanswerable = token_ys.iter().any(|y| match y {
        Some(y) => !answered_ys.contains(y),
        None => true,
    });
    let spent_sat: u64 = token_ys
        .iter()
        .zip(pairs.iter().map(|(amount, _)| u64::from(*amount)))
        .filter(|(y, _)| matches!(y, Some(y) if spent_ys.contains(y)))
        .map(|(_, amount)| amount)
        .sum();
    let has_pending = token_ys
        .iter()
        .any(|y| matches!(y, Some(y) if pending_ys.contains(y)));

    if unanswerable || has_pending {
        Ok(TokenSpendState::Indeterminate)
    } else if spent_sat == amount_sat {
        Ok(TokenSpendState::AllSpent { amount_sat })
    } else if spent_sat == 0 {
        Ok(TokenSpendState::Unspent)
    } else {
        Ok(TokenSpendState::PartiallySpent {
            spent_sat,
            unspent_sat: amount_sat - spent_sat,
        })
    }
}

/// Per-proof (face value, secret) pairs from a token, V3/V4 agnostic and
/// keyset-free — the inputs for NUT-07 spend classification.
fn token_proof_amounts_secrets(token: &CdkToken) -> Vec<(cdk::Amount, cdk::secret::Secret)> {
    use cdk::nuts::Token;
    match token {
        Token::TokenV3(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| (p.amount, p.secret.clone())))
            .collect(),
        Token::TokenV4(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| (p.amount, p.secret.clone())))
            .collect(),
    }
}

/// TollWallet wraps multiple CDK Wallet instances (one per mint URL) behind
/// a tokio Mutex for thread-safe serialized access. CDK's saga pattern
/// ensures operations are atomic — no swap-counter race.
pub struct TollWallet {
    wallets: HashMap<String, Arc<Mutex<Wallet>>>,
    recovery_in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    seed: [u8; 64],
    accepted_mints: Vec<String>,
    db_dir: PathBuf,
}

impl TollWallet {
    /// Create a new TollWallet. Does NOT open any wallets — call `ensure_mint`.
    pub fn new(seed: [u8; 64], accepted_mints: Vec<String>, db_dir: PathBuf) -> Self {
        Self {
            wallets: HashMap::new(),
            recovery_in_flight: Arc::default(),
            seed,
            accepted_mints,
            db_dir,
        }
    }

    fn is_mint_accepted(&self, mint_url: &str) -> bool {
        self.accepted_mints.is_empty()
            || self
                .accepted_mints
                .iter()
                .any(|m| crate::mint_url::mint_urls_equal(m, mint_url))
    }

    /// Register a mint and open a CDK wallet for it.
    /// Maps gonuts `AddMint(mintURL)` + `LoadWallet`.
    pub async fn ensure_mint(&mut self, mint_url: &str) -> Result<(), WalletError> {
        if !self.is_mint_accepted(mint_url) {
            return Err(WalletError::MintNotAccepted(mint_url.to_string()));
        }

        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        if self.wallets.contains_key(&normalized) {
            return Ok(());
        }

        let db_path = self.db_path_for_mint(&normalized);
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let localstore = WalletSqliteDatabase::new(db_path.to_str().unwrap_or(":memory:"))
            .await
            .map_err(|e| WalletError::Database(e.to_string()))?;

        let wallet = Wallet::new(
            mint_url,
            CurrencyUnit::Sat,
            Arc::new(localstore),
            self.seed,
            None,
        )?;

        let recovery = wallet.recover_incomplete_sagas().await?;
        if !recovery.is_empty() {
            tracing::info!(
                recovered = recovery.recovered,
                compensated = recovery.compensated,
                skipped = recovery.skipped,
                failed = recovery.failed,
                "recovered incomplete sagas for {}",
                normalized
            );
        }

        self.wallets
            .insert(normalized.to_string(), Arc::new(Mutex::new(wallet)));
        Ok(())
    }

    fn db_path_for_mint(&self, mint_url: &str) -> PathBuf {
        let sanitized: String = mint_url
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.db_dir.join(format!("{sanitized}.sqlite"))
    }

    /// Settle incomplete CDK sagas before a money-moving operation.
    ///
    /// CDK documents recovery as *required* before swap/send/receive/melt;
    /// cashu-service runs it before every send. Called under the wallet
    /// lock, inside the operation's timeout: with a settled wallet it is a
    /// local SQLite query (no network, no per-poll cost — quote-status
    /// polling never enters here); when a saga IS incomplete — e.g. one
    /// left behind by a mid-session `timeout()` cancellation — the
    /// operation must not move value over unreconciled state, so the
    /// recovery error aborts the money move with a distinct, observable
    /// cause (issue #33).
    async fn recover_before_op(w: &Wallet, op: &str, withdrawal: bool) -> Result<(), WalletError> {
        let report = w
            .recover_incomplete_sagas()
            .await
            .map_err(|e| WalletError::SagaRecovery(format!("{op}: {e}")))?;
        if !report.is_empty() {
            tracing::info!(
                op,
                recovered = report.recovered,
                compensated = report.compensated,
                skipped = report.skipped,
                failed = report.failed,
                "settled incomplete sagas before money-moving op"
            );
        }
        // Only fully-settled wallets may move value: a saga that recovery
        // failed on or skipped (e.g. mint unreachable mid-recovery) is still
        // an undecided operation — proceeding would spend over unreconciled
        // state (Codex P1 on #42; AGENTS.md fund-safety hard rules). The
        // next op or boot retries recovery; nothing is lost by waiting.
        if report.failed > 0 || report.skipped > 0 {
            return Err(WalletError::SagaRecovery(format!(
                "{op}: recovery left {} failed and {} skipped saga(s) unresolved; refusing to move value over unreconciled wallet state",
                report.failed, report.skipped
            )));
        }
        // Withdrawal-shaped ops (send/melt) must not replay right after
        // recovery COMPLETED an earlier ambiguous operation of the same
        // shape: the earlier send may already have moved tokens (or the
        // earlier melt already paid), so an immediate fresh op doubles the
        // caller's intent. The caller reconciles the earlier outcome against
        // its own durable intent (payout derives a fresh invoice per cycle;
        // CLI callers reconcile manually) and re-issues on a later call,
        // when recovery reports no further activity. Receive/mint are
        // replay-safe: the mint one-shots their inputs/quotes.
        if withdrawal && (report.recovered > 0 || report.compensated > 0) {
            return Err(WalletError::SagaRecovery(format!(
                "{op}: recovery completed an earlier ambiguous operation (recovered {}, compensated {}); refusing an immediate replay — reconcile that outcome before re-issuing",
                report.recovered, report.compensated
            )));
        }
        Ok(())
    }

    /// Receive a Cashu token (maps gonuts `Receive`).
    ///
    /// CDK's receive is atomic — no counter race. Wrapped in 30s timeout.
    pub async fn receive(&self, token_str: &str) -> Result<u64, WalletError> {
        let token: CdkToken = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = crate::mint_url::canonicalize_mint_url(&mint_url);

        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            Self::recover_before_op(&w, "receive", false).await?;
            w.receive(token_str, ReceiveOptions::default())
                .await
                .map_err(WalletError::from)
        })
        .await;

        match result {
            Ok(Ok(amount)) => {
                let sat: u64 = amount.into();
                Ok(sat)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.spawn_saga_recovery(&normalized);
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// NUT-07 check whether every proof of `token_str` is spent at the mint.
    ///
    /// `Ok(Some(amount_sat))` — all proofs `State::Spent`: this token can
    /// never be received again (an earlier receive of ours completed, or it
    /// was spent elsewhere). `Ok(None)` — at least one proof is still
    /// spendable.
    ///
    /// Reconciliation surface for the migration (AGENTS.md: "Ambiguous
    /// network results must be reconciled, not blindly retried"): after
    /// `ensure_mint` has run CDK's `recover_incomplete_sagas`, this answers
    /// definitively whether a timed-out receive actually landed — no
    /// inference from error text.
    pub async fn token_spent(&self, token_str: &str) -> Result<Option<u64>, WalletError> {
        match self.token_check_state(token_str).await? {
            TokenCheckState::Spent(amount_sat) => Ok(Some(amount_sat)),
            TokenCheckState::Unspent => Ok(None),
            // PENDING (in-flight at the mint) or mixed states are AMBIGUOUS,
            // not unspent — callers that would close an outcome on "None"
            // (payment reconciliation) must not treat this as settled
            // (Codex P1 on #43).
            TokenCheckState::Pending => Err(WalletError::Database(
                "token proofs pending at mint — outcome still ambiguous".into(),
            )),
        }
    }

    /// Tri-state NUT-07 outcome for reconciliation callers.
    pub async fn token_check_state(&self, token_str: &str) -> Result<TokenCheckState, WalletError> {
        use cdk::nuts::nut07::State;
        use cdk::nuts::{KeySetInfo, Token};
        use cdk::wallet::types::KeysetLoadPolicy;

        let token: Token = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = crate::mint_url::canonicalize_mint_url(&mint_url);
        let amount_sat: u64 = token.value().map(|a| a.into()).unwrap_or(0);

        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            // Same keyset-resolution path receive() itself uses (via the
            // metadata cache): proofs() maps each proof's short keyset id to
            // the mint's long id before NUT-07.
            let keysets = w.keysets(KeysetLoadPolicy::default()).await?;
            let keyset_infos: Vec<KeySetInfo> = keysets
                .iter()
                .map(|ks| KeySetInfo {
                    id: ks.id,
                    unit: ks.unit.clone(),
                    active: ks.active.unwrap_or(true),
                    input_fee_ppk: ks.input_fee_ppk,
                    final_expiry: ks.final_expiry,
                })
                .collect();
            let proofs = token.proofs(&keyset_infos)?;
            let states = w.check_proofs_spent(proofs).await?;
            let all_spent = states.iter().all(|s| s.state == State::Spent);
            let any_pending = states
                .iter()
                .any(|s| matches!(s.state, State::Pending | State::Reserved));
            Ok::<_, cdk::Error>((all_spent, any_pending))
        })
        .await;

        match result {
            Ok(Ok((true, _))) => Ok(TokenCheckState::Spent(amount_sat)),
            Ok(Ok((false, true))) => Ok(TokenCheckState::Pending),
            Ok(Ok((false, false))) => Ok(TokenCheckState::Unspent),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Batched NUT-07 spend classification for the migration pre-check
    /// (issue #34): one checkstate call per mint carrying every candidate
    /// token's Ys, instead of one call per token per boot.
    ///
    /// Classification needs only each proof's Y (hash_to_curve of the
    /// secret — keyset-independent); keyset resolution is needed solely to
    /// build V3 request proofs, so it runs once per mint alongside the
    /// checkstate call. `Wallet::check_proofs_spent` is used (not a raw
    /// connector call) to keep its side effect of marking mint-confirmed
    /// Spent Ys in the local store.
    pub async fn check_tokens_spent(
        &self,
        tokens: &[String],
    ) -> HashMap<String, Result<crate::migration::TokenSpendState, WalletError>> {
        use crate::migration::TokenSpendState;
        use cdk::nuts::nut07::State;
        use cdk::nuts::{KeySetInfo, Token};
        use cdk::wallet::types::KeysetLoadPolicy;

        let mut out: HashMap<String, Result<TokenSpendState, WalletError>> = HashMap::new();
        let mut by_mint: HashMap<String, Vec<(String, Token)>> = HashMap::new();

        for token_str in tokens {
            match token_str.parse::<CdkToken>() {
                Ok(token) => match token.mint_url() {
                    Ok(url) => by_mint
                        .entry(canonical_mint_url(&url.to_string()))
                        .or_default()
                        .push((token_str.clone(), token)),
                    Err(e) => {
                        out.insert(
                            token_str.clone(),
                            Err(WalletError::TokenParse(format!("{e}"))),
                        );
                    }
                },
                Err(e) => {
                    out.insert(
                        token_str.clone(),
                        Err(WalletError::TokenParse(format!("{e}"))),
                    );
                }
            }
        }

        for (mint, entries) in by_mint {
            let Some(wallet) = self.wallets.get(&mint) else {
                for (token_str, _) in &entries {
                    out.insert(
                        token_str.clone(),
                        Err(WalletError::WalletNotFound(mint.clone())),
                    );
                }
                continue;
            };

            let result = timeout(OP_TIMEOUT, async {
                let w = wallet.lock().await;
                let keysets = w.keysets(KeysetLoadPolicy::default()).await?;
                let keyset_infos: Vec<KeySetInfo> = keysets
                    .iter()
                    .map(|ks| KeySetInfo {
                        id: ks.id,
                        unit: ks.unit.clone(),
                        active: ks.active.unwrap_or(true),
                        input_fee_ppk: ks.input_fee_ppk,
                        final_expiry: ks.final_expiry,
                    })
                    .collect();
                let mut proofs = Vec::new();
                for (_, token) in &entries {
                    proofs.extend(token.proofs(&keyset_infos)?);
                }
                let states = w.check_proofs_spent(proofs).await?;
                Ok::<_, cdk::Error>(states)
            })
            .await;

            match result {
                Ok(Ok(states)) => {
                    let spent_ys: std::collections::HashSet<cdk::nuts::PublicKey> = states
                        .iter()
                        .filter(|s| s.state == State::Spent)
                        .map(|s| s.y)
                        .collect();
                    // PENDING/RESERVED proofs are mid-operation at the mint —
                    // their word is not final, so any token holding one is
                    // indeterminate regardless of its spent mix (same
                    // tri-state contract as `token_check_state`).
                    let pending_ys: std::collections::HashSet<cdk::nuts::PublicKey> = states
                        .iter()
                        .filter(|s| matches!(s.state, State::Pending | State::Reserved))
                        .map(|s| s.y)
                        .collect();
                    // A successful-but-incomplete NUT-07 batch (a mint that
                    // omits proof states) is not an all-unspent answer: any
                    // proof the mint did not speak to is an unanswered
                    // question, and terminalizing on it could finalize over
                    // still-moving value.
                    let answered_ys: std::collections::HashSet<cdk::nuts::PublicKey> =
                        states.iter().map(|s| s.y).collect();
                    for (token_str, token) in entries {
                        let amount_sat: u64 = token.value().map(|a| a.into()).unwrap_or(0);
                        let pairs = token_proof_amounts_secrets(&token);
                        out.insert(
                            token_str,
                            classify_spend_state(
                                &pairs,
                                amount_sat,
                                &spent_ys,
                                &pending_ys,
                                &answered_ys,
                            ),
                        );
                    }
                }
                Ok(Err(e)) => {
                    // cdk::Error is not Clone: each per-token error is built
                    // from the mint-level cause text (the migration loop only
                    // branches on Ok/Err and TokenParse).
                    let cause = format!("NUT-07 batch checkstate failed: {e}");
                    for (token_str, _) in entries {
                        out.insert(token_str, Err(WalletError::Database(cause.clone())));
                    }
                }
                Err(_) => {
                    for (token_str, _) in entries {
                        out.insert(token_str, Err(WalletError::Timeout(OP_TIMEOUT)));
                    }
                }
            }
        }

        out
    }

    /// Whether an incomplete CDK *receive* saga still holds inputs that
    /// overlap THIS token's proofs — the per-Y linkage issue #31 asks for
    /// (the old per-mint gate deferred every sibling token of a mint with
    /// one stuck saga, delaying value that was never at risk).
    ///
    /// Local SQLite query (no network): the migration reconciliation gate
    /// works even when the mint is unreachable (AGENTS.md: ambiguous results
    /// are reconciled, not retried).
    /// Whether the mint's wallet still holds an incomplete MELT saga —
    /// i.e. a payout melt whose Lightning outcome is still being settled
    /// by CDK's recovery. Local query (issue #41): used by the payout
    /// journal to decide whether an ambiguous melt may advance.
    /// Inspect keyset hygiene without moving value (R10/#13): lists the
    /// mint's keysets, the wallet's unspent proofs, and reports proofs
    /// held on keysets the mint no longer reports active, plus keysets
    /// with a final_expiry inside `expiry_warn_days`. Read-only.
    pub async fn keyset_hygiene(
        &self,
        mint_url: &str,
        expiry_warn_days: u64,
    ) -> Result<KeysetHygiene, WalletError> {
        use cdk::wallet::types::KeysetLoadPolicy;

        let normalized = canonical_mint_url(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(KeysetHygiene::default()),
        };

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            // Refresh from the mint (CacheThenNetwork) so retirement and
            // expiry are current, not stale cache.
            let keysets = w.keysets(KeysetLoadPolicy::default()).await?;
            let unspent = w.get_unspent_proofs().await?;

            let inactive: std::collections::HashSet<cdk::nuts::nut02::Id> = keysets
                .iter()
                .filter(|ks| ks.active == Some(false))
                .map(|ks| ks.id)
                .collect();

            let on_inactive = unspent
                .iter()
                .filter(|p| inactive.contains(&p.keyset_id))
                .count();

            let warn_cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + expiry_warn_days * 24 * 60 * 60;
            // Codex P2 on #57: only keysets we actually HOLD unspent
            // proofs on belong in the expiry warning — mints advertise
            // historical keysets we never used, and warning about those
            // every sweep is permanent noise that obscures the fund-risk
            // signal.
            let held: std::collections::HashSet<cdk::nuts::nut02::Id> =
                unspent.iter().map(|p| p.keyset_id).collect();
            let soonest: Vec<String> = keysets
                .iter()
                .filter_map(|ks| {
                    ks.final_expiry
                        .filter(|_| held.contains(&ks.id))
                        .filter(|e| *e < warn_cutoff)
                        .map(|_| ks.id.to_string())
                })
                .collect();

            Ok::<_, cdk::Error>(KeysetHygiene {
                unspent_proofs: unspent.len(),
                proofs_on_inactive_keysets: on_inactive,
                soonest_expiry_keysets: soonest,
            })
        })
        .await;

        match result {
            Ok(Ok(h)) => Ok(h),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Proactively rotate proofs off retired keysets (R10/#13): swaps
    /// every unspent proof on an inactive keyset to the mint's active
    /// keyset via CDK's public self-swap. Returns the number of proofs
    /// rotated. Errors surface to the caller — the sweep logs and
    /// retries next tick; value never leaves the mint (swap is same-mint).
    pub async fn rotate_inactive_keyset_proofs(&self, mint_url: &str) -> Result<u32, WalletError> {
        use cdk::amount::SplitTarget;
        use cdk::wallet::types::KeysetLoadPolicy;

        let normalized = canonical_mint_url(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(0),
        };

        let normalized_for_recovery = normalized.clone();
        let result = timeout(RECOVERY_TIMEOUT, async {
            let w = wallet.lock().await;
            // Settle incomplete sagas before the swap, exactly like every
            // other money-moving wrapper (Codex P2 on #57): a recovery that
            // FAILED or SKIPPED a saga must abort — stacking a self-swap
            // over unreconciled state can spend proofs an unresolved saga
            // still owns. withdrawal=false: a self-swap preserves value and
            // re-selects its inputs from unspent proofs, so a recovery that
            // just COMPLETED an earlier saga does not double the caller's
            // intent the way a send/melt replay would.
            Self::recover_before_op(&w, "rotate", false).await?;
            let keysets = w
                .keysets(KeysetLoadPolicy::default())
                .await
                .map_err(WalletError::from)?;
            let inactive: std::collections::HashSet<cdk::nuts::nut02::Id> = keysets
                .iter()
                .filter(|ks| ks.active == Some(false))
                .map(|ks| ks.id)
                .collect();
            if inactive.is_empty() {
                return Ok::<_, WalletError>(0u32);
            }
            let unspent = w.get_unspent_proofs().await.map_err(WalletError::from)?;
            let stale: Vec<cdk::nuts::nut00::Proof> = unspent
                .into_iter()
                .filter(|p| inactive.contains(&p.keyset_id))
                .collect();
            if stale.is_empty() {
                return Ok(0);
            }
            let count = stale.len() as u32;
            // Self-swap to the active keyset (same mint — value stays).
            w.swap(None, SplitTarget::default(), stale, None, true, false)
                .await
                .map_err(WalletError::from)?;
            Ok(count)
        })
        .await;

        match result {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => {
                // A swap error may have left an incomplete saga with the
                // stale inputs reserved — recover now, not at next boot
                // (Codex P1 on #57: the hygiene sweep reads only unspent
                // proofs, so reserved inputs would otherwise be invisible
                // to it until restart).
                self.spawn_saga_recovery(&normalized_for_recovery);
                Err(e)
            }
            Err(_) => {
                // Timeout cancels the swap mid-saga (AGENTS.md L141-144):
                // same recovery story as every other money-moving wrapper.
                self.spawn_saga_recovery(&normalized_for_recovery);
                Err(WalletError::Timeout(RECOVERY_TIMEOUT))
            }
        }
    }

    /// Whether the mint's wallet still holds an incomplete SEND saga —
    /// e.g. a CLI drain whose output token may have been created without
    /// being delivered. Local query (issue #46): the drain path consults
    /// it before re-issuing, mirroring the melt gate from #45.
    pub async fn mint_has_unresolved_send(&self, mint_url: &str) -> Result<bool, WalletError> {
        let normalized = canonical_mint_url(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(false),
        };
        let w = wallet.lock().await;
        let sagas = w
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(|e| WalletError::Database(e.to_string()))?;
        Ok(sagas
            .into_iter()
            .any(|s| matches!(s.state, cdk::wallet::types::WalletSagaState::Send(_))))
    }

    pub async fn mint_has_unresolved_melt(&self, mint_url: &str) -> Result<bool, WalletError> {
        let normalized = canonical_mint_url(mint_url);
        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            None => return Ok(false),
        };
        let w = wallet.lock().await;
        let sagas = w
            .localstore
            .get_incomplete_sagas()
            .await
            .map_err(|e| WalletError::Database(e.to_string()))?;
        Ok(sagas
            .into_iter()
            .any(|s| matches!(s.state, cdk::wallet::types::WalletSagaState::Melt(_))))
    }

    pub async fn mint_has_unresolved_receive(&self, token_str: &str) -> Result<bool, WalletError> {
        use cdk::nuts::KeySetInfo;
        use cdk::wallet::types::KeysetLoadPolicy;

        let token: CdkToken = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = crate::mint_url::canonicalize_mint_url(&mint_url);

        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            // No wallet for this mint => nothing of this token can be
            // in flight locally; the receive path surfaces the rest.
            None => return Ok(false),
        };

        // Per-token linkage (issue #31): instead of "any receive saga for
        // this mint" (which deferred every sibling token), ask whether THIS
        // token's input Ys are reserved by an incomplete RECEIVE saga —
        // i.e. an earlier attempt of this very token is still in flight.
        // Scoped to receive sagas (Codex P2 on #44): sends also reserve
        // proofs, and a token minted by this wallet and sent to a customer
        // would otherwise false-positive when the customer pays it back.
        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let keysets = w.keysets(KeysetLoadPolicy::default()).await?;
            let keyset_infos: Vec<KeySetInfo> = keysets
                .iter()
                .map(|ks| KeySetInfo {
                    id: ks.id,
                    unit: ks.unit.clone(),
                    active: ks.active.unwrap_or(true),
                    input_fee_ppk: ks.input_fee_ppk,
                    final_expiry: ks.final_expiry,
                })
                .collect();
            let proofs = token.proofs(keyset_infos.as_slice())?;
            let token_ys: std::collections::HashSet<_> = proofs
                .iter()
                .map(|p| p.y())
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect();

            let mut in_flight_ys: std::collections::HashSet<cdk::nuts::PublicKey> =
                std::collections::HashSet::new();
            for saga in w
                .localstore
                .get_incomplete_sagas()
                .await?
                .into_iter()
                .filter(|s| matches!(s.state, cdk::wallet::types::WalletSagaState::Receive(_)))
            {
                for info in w.localstore.get_reserved_proofs(&saga.id).await? {
                    in_flight_ys.insert(info.y);
                }
            }
            Ok::<_, cdk::Error>(in_flight_ys.intersection(&token_ys).next().is_some())
        })
        .await;

        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// One-shot recovery of a partially-spent token's unspent remainder
    /// (#47): NUT-07 per-proof, then receive ONLY the definitively-UNSPENT
    /// proofs as a fresh sub-token. Proofs the mint reported SPENT,
    /// PENDING/RESERVED, or omitted from its answer are excluded — only an
    /// answered UNSPENT proof may move. `Ok(None)` when nothing is
    /// recoverable right now (caller keeps its terminal classification and
    /// the manual runbook applies).
    pub async fn receive_unspent_remainder(
        &self,
        token_str: &str,
    ) -> Result<Option<u64>, WalletError> {
        use cdk::nuts::nut07::State;
        use cdk::nuts::{KeySetInfo, Token as NutToken};
        use cdk::wallet::types::KeysetLoadPolicy;

        let token: CdkToken = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = canonical_mint_url(&mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        // Build the sub-token under the wallet lock; the receive itself
        // runs through the normal money path (own lock, own recovery).
        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let keysets = w.keysets(KeysetLoadPolicy::default()).await?;
            let keyset_infos: Vec<KeySetInfo> = keysets
                .iter()
                .map(|ks| KeySetInfo {
                    id: ks.id,
                    unit: ks.unit.clone(),
                    active: ks.active.unwrap_or(true),
                    input_fee_ppk: ks.input_fee_ppk,
                    final_expiry: ks.final_expiry,
                })
                .collect();
            let proofs = token.proofs(&keyset_infos)?;
            let states = w.check_proofs_spent(proofs.clone()).await?;
            let answered: std::collections::HashSet<cdk::nuts::PublicKey> =
                states.iter().map(|s| s.y).collect();
            let blocked: std::collections::HashSet<cdk::nuts::PublicKey> = states
                .iter()
                .filter(|s| s.state != State::Unspent)
                .map(|s| s.y)
                .collect();
            let unspent: Vec<_> = proofs
                .into_iter()
                .filter(|p| {
                    p.y()
                        .map(|y| answered.contains(&y) && !blocked.contains(&y))
                        .unwrap_or(false)
                })
                .collect();
            if unspent.is_empty() {
                return Ok::<_, cdk::Error>(None);
            }
            let sub = NutToken::new(w.mint_url.clone(), unspent, None, w.unit.clone());
            Ok(Some(sub.to_string()))
        })
        .await;

        match result {
            Ok(Ok(Some(sub_token))) => self.receive(&sub_token).await.map(Some),
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Send tokens (maps gonuts `Send`).
    /// Returns the serialized Cashu V4 token string.
    pub async fn send(
        &self,
        mint_url: &str,
        amount_sat: u64,
        include_fee: bool,
    ) -> Result<String, WalletError> {
        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let opts = SendOptions {
            include_fee,
            ..Default::default()
        };

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            Self::recover_before_op(&w, "send", true).await?;
            let prepared = w
                .prepare_send(Amount::from(amount_sat), opts)
                .await
                .map_err(WalletError::from)?;
            let token = prepared.confirm(None).await.map_err(WalletError::from)?;
            Ok::<_, WalletError>(token.to_string())
        })
        .await;

        match result {
            Ok(Ok(token_str)) => Ok(token_str),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.spawn_saga_recovery(&normalized);
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Get total balance across all mints (maps gonuts `GetBalance`).
    pub async fn get_balance(&self) -> Result<u64, WalletError> {
        let mut total: u64 = 0;
        for wallet in self.wallets.values() {
            let w = wallet.lock().await;
            let bal: u64 = w.total_balance().await?.into();
            total += bal;
        }
        Ok(total)
    }

    /// Get per-mint balances (maps gonuts `GetBalanceByMints`).
    pub async fn get_balance_by_mint(&self) -> Result<Vec<(String, u64)>, WalletError> {
        let mut result = Vec::new();
        for (mint_url, wallet) in &self.wallets {
            let w = wallet.lock().await;
            let bal: u64 = w.total_balance().await?.into();
            result.push((mint_url.clone(), bal));
        }
        Ok(result)
    }

    /// Request a mint quote (NUT-04, maps gonuts `RequestMintQuote`).
    pub async fn request_mint_quote(
        &self,
        mint_url: &str,
        amount_sat: u64,
    ) -> Result<MintQuoteInfo, WalletError> {
        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.mint_quote(
                PaymentMethod::BOLT11,
                Some(Amount::from(amount_sat)),
                None,
                None,
            )
            .await
        })
        .await;

        match result {
            Ok(Ok(quote)) => Ok(MintQuoteInfo {
                id: quote.id,
                request: quote.request,
                amount: quote.amount.map(|a| -> u64 { a.into() }).unwrap_or(0),
                expiry: quote.expiry,
            }),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Check mint quote status (maps gonuts `MintQuoteState`).
    ///
    /// Returns the typed NUT-04 state. Callers must compare the exact
    /// variant: the debug name of `Unpaid` lowercased contains "paid", so
    /// substring matching treats every unpaid invoice as paid
    /// (AGENTS.md "do not guess about Cashu protocol behavior").
    pub async fn check_mint_quote_state(
        &self,
        mint_url: &str,
        quote_id: &str,
    ) -> Result<MintQuoteState, WalletError> {
        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.check_mint_quote_status(quote_id).await
        })
        .await;

        match result {
            Ok(Ok(quote)) => Ok(quote.state),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Mint tokens from a paid quote (NUT-04, maps gonuts `MintTokens`).
    /// CDK API: `wallet.mint(quote_id, SplitTarget, Option<SpendingConditions>)`.
    pub async fn mint_tokens(&self, mint_url: &str, quote_id: &str) -> Result<u64, WalletError> {
        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            Self::recover_before_op(&w, "mint", false).await?;
            let proofs = w
                .mint(quote_id, SplitTarget::default(), None)
                .await
                .map_err(WalletError::from)?;
            let total: u64 = proofs.iter().map(|p| -> u64 { p.amount.into() }).sum();
            Ok::<_, WalletError>(total)
        })
        .await;

        match result {
            Ok(Ok(total)) => Ok(total),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.spawn_saga_recovery(&normalized);
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Request a melt quote + prepare melt (NUT-05, maps gonuts `RequestMeltQuote` + `Melt`).
    /// CDK flow: `melt_quote(BOLT11, invoice)` → `prepare_melt(quote_id, meta)` → `confirm()`.
    pub async fn melt(&self, mint_url: &str, invoice: &str) -> Result<MeltQuoteInfo, WalletError> {
        let normalized = crate::mint_url::canonicalize_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(&normalized)
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let invoice_owned = invoice.to_string();
        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            Self::recover_before_op(&w, "melt", true).await?;
            // Step 1: create melt quote
            let quote = w
                .melt_quote(PaymentMethod::BOLT11, invoice_owned, None, None)
                .await
                .map_err(WalletError::from)?;
            // Step 2: prepare melt with the quote ID
            let prepared = w
                .prepare_melt(&quote.id, HashMap::new())
                .await
                .map_err(WalletError::from)?;
            // Step 3: confirm
            let finalized = prepared.confirm().await.map_err(WalletError::from)?;
            Ok::<_, WalletError>((quote, finalized))
        })
        .await;

        match result {
            Ok(Ok((quote, _finalized))) => Ok(MeltQuoteInfo {
                quote_id: quote.id,
                amount: quote.amount.into(),
                fee: quote.fee_reserve.into(),
            }),
            Ok(Err(e)) => {
                // A CDK error after confirm() started (connection reset
                // mid-melt) is as ambiguous as a timeout — the payment may
                // have fired. Trigger the same in-session saga recovery
                // (Codex P1 on #45): the melt quote state settles it.
                self.spawn_saga_recovery(&normalized);
                Err(e)
            }
            Err(_) => {
                self.spawn_saga_recovery(&normalized);
                Err(WalletError::Timeout(OP_TIMEOUT))
            }
        }
    }

    /// Shutdown — drop all wallets (closes sqlite DB handles).
    /// Maps gonuts `Shutdown`.
    pub async fn shutdown(self) {
        // Dropping self drops wallets, which closes sqlite connections.
        drop(self);
    }

    /// Load or generate a wallet seed (64 bytes) from the given path.
    /// Load the wallet seed, creating it on first boot.
    ///
    /// New wallets get a 24-word BIP39 mnemonic: `wallet_mnemonic.txt`
    /// (0600, the operator's disaster-recovery backup — see MIGRATION.md)
    /// plus the unchanged 64-byte `wallet_seed.bin` that CDK consumes.
    /// The mnemonic is written FIRST so every crash point self-heals:
    ///
    /// - mnemonic only  → seed is (re)derived from it;
    /// - seed only      → pre-mnemonic wallet, loads as before;
    /// - both present   → they must derive consistently, otherwise startup
    ///   fails loudly instead of silently forking the wallet (a mismatch
    ///   means one file was replaced).
    ///
    /// A seed file of the wrong size is now a hard error (restore from the
    /// mnemonic or a backup) rather than a silent regeneration — a fresh
    /// seed over an existing wallet would orphan every deterministic
    /// derivation (AGENTS.md: never silently recreate wallet state).
    pub async fn load_or_create_seed(path: &Path) -> Result<[u8; 64], WalletError> {
        let mnemonic_path = Self::mnemonic_path_for(path);

        let mnemonic_on_disk = match tokio::fs::read_to_string(&mnemonic_path).await {
            Ok(phrase) => Some(phrase.trim().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(WalletError::Io(e)),
        };

        let derived: Option<[u8; 64]> = match &mnemonic_on_disk {
            Some(phrase) => Some(mnemonic_to_seed(phrase)?),
            None => None,
        };

        if path.exists() {
            let data = tokio::fs::read(path).await?;
            if data.len() != 64 {
                tracing::error!(
                    seed_file = %path.display(),
                    mnemonic_file = %mnemonic_path.display(),
                    "wallet seed file is corrupt (wrong size); refusing to regenerate a fresh seed over an existing wallet. Restore wallet_seed.bin from backup, or re-derive it from the mnemonic in {} and retry",
                    mnemonic_path.display()
                );
                return Err(WalletError::Database(
                    "wallet seed file has wrong size".into(),
                ));
            }
            let mut seed = [0u8; 64];
            seed.copy_from_slice(&data);

            if let Some(expected) = derived {
                if expected != seed {
                    tracing::error!(
                        seed_file = %path.display(),
                        mnemonic_file = %mnemonic_path.display(),
                        "wallet_seed.bin does not match wallet_mnemonic.txt — one of them was replaced. Refusing to start with an ambiguous wallet identity; resolve manually (keep the file that matches the wallet that holds the funds)"
                    );
                    return Err(WalletError::Database("seed/mnemonic mismatch".into()));
                }
            }
            return Ok(seed);
        }

        match derived {
            // Crash between the two writes: re-derive the seed.
            Some(seed) => {
                Self::write_seed_file(path, &seed).await?;
                Ok(seed)
            }
            // First boot ever: generate the mnemonic first, then the seed.
            None => {
                let mnemonic = bip39::Mnemonic::generate(24)
                    .map_err(|e| WalletError::Database(format!("mnemonic generation: {e}")))?;
                let seed = mnemonic.to_seed("");
                Self::write_file_private(&mnemonic_path, mnemonic.to_string().as_bytes()).await?;
                tracing::info!(
                    mnemonic_file = %mnemonic_path.display(),
                    "generated 24-word wallet mnemonic — back it up to be able to restore this wallet (NUT-13)"
                );
                Self::write_seed_file(path, &seed).await?;
                Ok(seed)
            }
        }
    }

    async fn write_seed_file(path: &Path, seed: &[u8; 64]) -> Result<(), WalletError> {
        Self::write_file_private(path, seed).await
    }

    /// Atomic + durable write of a wallet-identity file: temp file (0600,
    /// same directory so the rename is same-filesystem), fsync, rename,
    /// fsync the parent directory. A power loss at any point leaves either
    /// the previous complete content or the new complete content — never a
    /// truncated file the next boot would reject (Codex P1 on #37).
    async fn write_file_private(path: &Path, bytes: &[u8]) -> Result<(), WalletError> {
        use tokio::io::AsyncWriteExt;

        let tmp = path.with_extension("tmp");
        #[cfg(unix)]
        let mut open = {
            // tokio::fs::OpenOptions exposes mode() directly on unix.
            let mut o = tokio::fs::OpenOptions::new();
            o.mode(0o600);
            o
        };
        #[cfg(not(unix))]
        let mut open = tokio::fs::OpenOptions::new();
        let mut f = open
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
        drop(f);

        // Explicit chmod: OpenOptions.mode() only applies at creation, and
        // a leftover tmp from an earlier crash could carry wider perms.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }

        tokio::fs::rename(&tmp, path).await?;
        // The rename's durability must be proven before the seed is used:
        // an open/sync failure propagates so first boot fails loudly
        // instead of running on an identity a power loss can strand
        // (Codex P1 on #37). Bare relative filenames have parent ""
        // (not openable) — normalize to ".".
        if let Some(parent) = path.parent() {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    }

    fn mnemonic_path_for(seed_path: &Path) -> PathBuf {
        seed_path.with_file_name("wallet_mnemonic.txt")
    }

    /// AGENTS.md ("if you add a timeout, add the reconciliation that
    /// follows it"): every TollWallet op wraps CDK in `timeout(OP_TIMEOUT)`,
    /// which cancels the saga mid-flight — the saga survives in SQLite and,
    /// without this, nothing retries it until the next boot's `ensure_mint`.
    /// This fires a detached `recover_incomplete_sagas` for the mint so the
    /// cancelled saga settles in-session (receive: NUT-19 replay / NUT-09
    /// restore / compensate; melt: quote-state check that detects an
    /// already-paid Lightning payment). Never blocks the caller; recovery
    /// failures are logged and left for the next trigger or boot.
    fn spawn_saga_recovery(&self, normalized_mint: &str) {
        let Some(wallet) = self.wallets.get(normalized_mint) else {
            return;
        };
        // Coalesce per mint (Codex P2 on #39): while a recovery holds the
        // wallet mutex, queued same-mint ops time out before starting
        // their own sagas and would otherwise each spawn another recovery
        // run. One in-flight recovery per mint is sufficient — the saga
        // survives, and the next timeout or boot re-triggers.
        {
            let mut in_flight = self.recovery_in_flight.lock().unwrap();
            if in_flight.contains(normalized_mint) {
                tracing::debug!(mint = %normalized_mint, "saga recovery already in flight for mint");
                return;
            }
            in_flight.insert(normalized_mint.to_string());
        }
        let wallet = wallet.clone();
        let mint = normalized_mint.to_string();
        let in_flight = self.recovery_in_flight.clone();
        tokio::spawn(async move {
            struct Guard(
                std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
                String,
            );
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.lock().unwrap().remove(&self.1);
                }
            }
            let _guard = Guard(in_flight, mint.clone());

            match timeout(RECOVERY_TIMEOUT, async {
                wallet.lock().await.recover_incomplete_sagas().await
            })
            .await
            {
                Ok(Ok(report)) if report.recovered > 0 || report.compensated > 0 => {
                    tracing::info!(
                        mint = %mint,
                        recovered = report.recovered,
                        compensated = report.compensated,
                        failed = report.failed,
                        "post-timeout saga recovery settled operations"
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(mint = %mint, error = %e, "post-timeout saga recovery errored; retrying on next trigger or boot");
                }
                Err(_) => {
                    tracing::warn!(mint = %mint, "post-timeout saga recovery timed out; retrying on next trigger or boot");
                }
            }
        });
    }

    /// NUT-13 restore for every registered mint (CDK `Wallet::restore`:
    /// batched NUT-09 restore with NUT-07 pruning, batch 100 / gap 3).
    /// The operator-facing recovery path after seed/mnemonic loss — see
    /// MIGRATION.md.
    pub async fn restore_all(
        &self,
    ) -> Result<Vec<(String, Result<RestoredSummary, String>)>, WalletError> {
        // Per-mint isolation (Codex P2 on #37): one offline or
        // restore-less mint must not block or swallow the recovery of the
        // others — restore is often run precisely when things are broken.
        let mut out = Vec::new();
        for (mint_url, wallet) in &self.wallets {
            let result = match wallet.lock().await.restore().await {
                Ok(restored) => Ok(RestoredSummary {
                    spent_sat: u64::from(restored.spent),
                    unspent_sat: u64::from(restored.unspent),
                    pending_sat: u64::from(restored.pending),
                }),
                Err(e) => Err(e.to_string()),
            };
            out.push((mint_url.clone(), result));
        }
        Ok(out)
    }
}

/// Mint quote info.
#[derive(Debug, Clone)]
pub struct MintQuoteInfo {
    pub id: String,
    pub request: String,
    pub amount: u64,
    pub expiry: u64,
}

/// Melt quote info.
#[derive(Debug, Clone)]
pub struct MeltQuoteInfo {
    pub quote_id: String,
    pub amount: u64,
    pub fee: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Codex P1 on #42 (round 2): when pre-op recovery COMPLETES an earlier
    /// ambiguous withdrawal (here: compensates a planted interrupted receive
    /// locally), a fresh send must not replay immediately — the caller must
    /// reconcile the earlier outcome first. Pre-fix, send proceeded to its
    /// own swap (and failed at the dead mint); the abort is the fix.
    #[tokio::test]
    async fn send_refuses_replay_right_after_recovery_activity() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Id, Proof};
        use cdk::wallet::types::{
            OperationData, ProofInfo, ReceiveOperationData, ReceiveSagaState, WalletSaga,
            WalletSagaState,
        };
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();

        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();
        let saga_id = uuid::Uuid::new_v4();
        let proof = Proof::new(
            cdk::Amount::from(1),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("interrupted-withdrawal-adjacent-saga"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let y = hash_to_curve(proof.secret.as_bytes()).unwrap();
        {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof,
                        y,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
            guard
                .localstore
                .reserve_proofs(vec![y], &saga_id)
                .await
                .unwrap();
            guard
                .localstore
                .add_saga(WalletSaga::new(
                    saga_id,
                    WalletSagaState::Receive(ReceiveSagaState::ProofsPending),
                    cdk::Amount::from(1),
                    mint_url,
                    cdk::nuts::CurrencyUnit::Sat,
                    OperationData::Receive(ReceiveOperationData {
                        token: None,
                        counter_start: None,
                        counter_end: None,
                        amount: Some(cdk::Amount::from(1)),
                        blinded_messages: None,
                    }),
                ))
                .await
                .unwrap();
        }

        let err = wallet.send(mint, 1, false).await.expect_err("must abort");
        assert!(
            matches!(err, WalletError::SagaRecovery(ref m) if m.contains("refusing an immediate replay")),
            "expected withdrawal replay refusal, got {err:?}"
        );
    }

    /// Codex P2 on #57: the rotation self-swap must run the same
    /// pre-op saga recovery as every other money-moving wrapper. A
    /// receive saga that recovery FAILS on (mint unreachable) leaves the
    /// wallet unreconciled — rotation must refuse instead of stacking a
    /// swap over state an unresolved saga still owns.
    #[tokio::test]
    async fn rotation_refuses_when_saga_recovery_fails() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Id, Proof};
        use cdk::wallet::types::{
            OperationData, ProofInfo, ReceiveOperationData, ReceiveSagaState, WalletSaga,
            WalletSagaState,
        };
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        // Nothing listens on 127.0.0.1:1 — resuming the planted saga
        // fails fast (connection refused) instead of hanging.
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();

        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();
        let saga_id = uuid::Uuid::new_v4();
        let proof = Proof::new(
            cdk::Amount::from(1),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("rotation-preflight-saga"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let y = hash_to_curve(proof.secret.as_bytes()).unwrap();
        {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof,
                        y,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
            guard
                .localstore
                .reserve_proofs(vec![y], &saga_id)
                .await
                .unwrap();
            // SwapRequested requires an external mint call to resume —
            // recovery of it FAILS at the dead mint (failed > 0), which
            // is exactly the unresolved state the preflight must refuse.
            guard
                .localstore
                .add_saga(WalletSaga::new(
                    saga_id,
                    WalletSagaState::Receive(ReceiveSagaState::SwapRequested),
                    cdk::Amount::from(1),
                    mint_url,
                    cdk::nuts::CurrencyUnit::Sat,
                    OperationData::Receive(ReceiveOperationData {
                        token: None,
                        counter_start: None,
                        counter_end: None,
                        amount: Some(cdk::Amount::from(1)),
                        blinded_messages: None,
                    }),
                ))
                .await
                .unwrap();
        }

        let err = wallet
            .rotate_inactive_keyset_proofs(mint)
            .await
            .expect_err("rotation must abort over unreconciled state");
        assert!(
            matches!(err, WalletError::SagaRecovery(ref m) if m.contains("rotate")),
            "expected preflight refusal naming the rotate op, got {err:?}"
        );
    }

    fn classifier_fixture() -> (
        Vec<(cdk::Amount, cdk::secret::Secret)>,
        Vec<cdk::nuts::PublicKey>,
    ) {
        use cdk::dhke::hash_to_curve;
        let pairs: Vec<(cdk::Amount, cdk::secret::Secret)> = ["a", "b", "c"]
            .iter()
            .map(|s| (cdk::Amount::from(2), cdk::secret::Secret::new(*s)))
            .collect();
        let ys = pairs
            .iter()
            .map(|(_, s)| hash_to_curve(s.as_bytes()).unwrap())
            .collect();
        (pairs, ys)
    }

    /// Codex P1 on #42 (round 2): a successful-but-incomplete NUT-07 batch
    /// must defer, never terminalize — a proof the mint did not speak to is
    /// an unanswered question, not an unspent proof.
    #[test]
    fn classifier_defers_when_the_mint_omits_a_proof_state() {
        let (pairs, ys) = classifier_fixture();
        let spent: std::collections::HashSet<_> = [ys[0]].into_iter().collect();
        let pending: std::collections::HashSet<_> = [].into_iter().collect();
        // The mint answered only two of the three requested Ys.
        let answered: std::collections::HashSet<_> = [ys[0], ys[1]].into_iter().collect();
        assert_eq!(
            classify_spend_state(&pairs, 6, &spent, &pending, &answered).unwrap(),
            crate::migration::TokenSpendState::Indeterminate
        );
        // Same spent set, complete answer: definitive PartiallySpent.
        let answered_all: std::collections::HashSet<_> = ys.clone().into_iter().collect();
        assert_eq!(
            classify_spend_state(&pairs, 6, &spent, &pending, &answered_all).unwrap(),
            crate::migration::TokenSpendState::PartiallySpent {
                spent_sat: 2,
                unspent_sat: 4
            }
        );
        // Every Y answered UNSPENT elsewhere is Unspent, not Indeterminate.
        assert_eq!(
            classify_spend_state(&pairs, 6, &pending, &pending, &answered_all).unwrap(),
            crate::migration::TokenSpendState::Unspent
        );
        // A PENDING answer defers even when everything else is spent.
        let spent_2: std::collections::HashSet<_> = [ys[0], ys[1]].into_iter().collect();
        let pending_1: std::collections::HashSet<_> = [ys[2]].into_iter().collect();
        assert_eq!(
            classify_spend_state(&pairs, 6, &spent_2, &pending_1, &answered_all).unwrap(),
            crate::migration::TokenSpendState::Indeterminate
        );
        assert_eq!(
            classify_spend_state(&pairs, 6, &spent_all(&ys), &pending_1, &answered_all).unwrap(),
            crate::migration::TokenSpendState::Indeterminate
        );
        // All spent, nothing pending: terminal.
        assert_eq!(
            classify_spend_state(&pairs, 6, &spent_all(&ys), &pending, &answered_all).unwrap(),
            crate::migration::TokenSpendState::AllSpent { amount_sat: 6 }
        );
        // Empty token cannot be classified.
        assert!(classify_spend_state(&[], 0, &spent, &pending, &answered_all).is_err());
    }

    fn spent_all(ys: &[cdk::nuts::PublicKey]) -> std::collections::HashSet<cdk::nuts::PublicKey> {
        ys.iter().copied().collect()
    }

    fn make_test_wallet(dir: &Path, accepted_mints: Vec<String>) -> TollWallet {
        let mut seed = [0u8; 64];
        rand::thread_rng().fill(&mut seed);
        TollWallet::new(seed, accepted_mints, dir.to_path_buf())
    }

    /// Codex P2 on #57: the expiry warning must only name keysets the
    /// wallet actually holds unspent proofs on. A mint advertising an
    /// unused near-expiry keyset must NOT appear in
    /// `soonest_expiry_keysets` — that is permanent warning noise
    /// obscuring the fund-risk signal.
    #[tokio::test]
    async fn expiry_warning_only_names_keysets_with_held_proofs() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Proof};
        use cdk::wallet::types::ProofInfo;
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();
        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let soon = now + 3600;

        // Two distinct valid secp256k1 points (generator G and 2G) so the
        // two keysets derive distinct, self-consistent v1 keyset IDs
        // (add_keys verifies id ↔ keys).
        let g = cdk::nuts::PublicKey::from_str(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();
        let g2 = cdk::nuts::PublicKey::from_str(
            "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
        )
        .unwrap();

        let keyset_for = |key: cdk::nuts::PublicKey, expiry| -> cdk::nuts::nut02::KeySet {
            let mut keys = std::collections::BTreeMap::new();
            keys.insert(cdk::Amount::from(1), key);
            let keys = cdk::nuts::Keys::new(keys);
            cdk::nuts::nut02::KeySet {
                id: cdk::nuts::nut02::Id::v1_from_keys(&keys),
                unit: cdk::nuts::CurrencyUnit::Sat,
                active: Some(true),
                keys,
                input_fee_ppk: 0,
                final_expiry: expiry,
            }
        };
        let held_ks = keyset_for(g, Some(soon));
        let unused_ks = keyset_for(g2, Some(soon));
        let held_id = held_ks.id;
        let unused_id = unused_ks.id;
        assert_ne!(held_id, unused_id, "plant sanity: distinct keysets");

        {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            guard
                .localstore
                .add_mint(mint_url.clone(), None)
                .await
                .unwrap();
            guard
                .localstore
                .add_mint_keysets(
                    mint_url.clone(),
                    vec![
                        cdk::nuts::nut02::KeySetInfo {
                            id: held_id,
                            unit: cdk::nuts::CurrencyUnit::Sat,
                            active: true,
                            input_fee_ppk: 0,
                            final_expiry: Some(soon),
                        },
                        cdk::nuts::nut02::KeySetInfo {
                            id: unused_id,
                            unit: cdk::nuts::CurrencyUnit::Sat,
                            active: true,
                            input_fee_ppk: 0,
                            final_expiry: Some(soon),
                        },
                    ],
                )
                .await
                .unwrap();
            for ks in [held_ks, unused_ks] {
                guard.localstore.add_keys(ks).await.unwrap();
            }

            // One unspent proof on held_id — nothing on unused_id.
            let proof = Proof::new(
                cdk::Amount::from(1),
                held_id,
                cdk::secret::Secret::new("expiry-warning-held-proof"),
                g,
            );
            let y = hash_to_curve(proof.secret.as_bytes()).unwrap();
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof,
                        y,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
        }

        let h = wallet.keyset_hygiene(mint, 30).await.unwrap();
        assert_eq!(h.unspent_proofs, 1, "plant sanity: one held proof");
        assert_eq!(
            h.soonest_expiry_keysets,
            vec![held_id.to_string()],
            "only the keyset with held proofs may be flagged as expiring"
        );
    }

    #[test]
    fn canonical_mint_url_lowercases_host_and_trims_slash() {
        assert_eq!(
            canonical_mint_url("HTTPS://Mint.Example/"),
            "https://mint.example"
        );
        assert_eq!(
            canonical_mint_url("https://mint.example/"),
            "https://mint.example"
        );
        // Path case is preserved per CDK MintUrl semantics.
        assert_eq!(
            canonical_mint_url("https://mint.example/Path/TO/mint"),
            "https://mint.example/Path/TO/mint"
        );
    }

    /// PR #22 r4070018389 / PR #23 r4070120810: an alias-spelled lookup
    /// (uppercase host) must find the registered wallet — otherwise a
    /// quote surviving a restart with a re-spelled config is permanently
    /// `WalletNotFound` and a paid invoice never grants.
    #[tokio::test]
    async fn alias_spelled_mint_lookups_hit_the_registered_wallet() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);

        wallet.ensure_mint("https://mint.example").await.unwrap();
        // Same mint, alias spelling: must not create a second wallet.
        wallet
            .ensure_mint("HTTPS://Mint.Example/")
            .await
            .expect("alias spelling must resolve to the registered wallet");
        assert_eq!(
            wallet.wallets.len(),
            1,
            "alias spelling must not fork the wallet map"
        );

        // Lookups through alias spellings resolve past the map to the
        // store layer (a non-WalletNotFound error: UnknownQuote), proving
        // the canonical key was found.
        let err = wallet
            .check_mint_quote_state("HTTPS://Mint.Example/", "nonexistent")
            .await
            .expect_err("unknown quote must error");
        assert!(
            !matches!(err, WalletError::WalletNotFound(_)),
            "alias spelling resolved to a wallet, got {err:?}"
        );
    }

    #[tokio::test]
    async fn open_close_cycle_releases_file_lock() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut wallet = make_test_wallet(dir, vec![]);
        wallet
            .ensure_mint("https://test-mint.example")
            .await
            .unwrap();
        assert!(wallet.wallets.contains_key("https://test-mint.example"));

        wallet.shutdown().await;

        let mut wallet2 = make_test_wallet(dir, vec![]);
        wallet2
            .ensure_mint("https://test-mint.example")
            .await
            .expect("should reopen DB after shutdown");
    }

    #[tokio::test]
    async fn rejects_unlisted_mint() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec!["https://allowed.example".into()]);
        let result = wallet.ensure_mint("https://evil.example").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            WalletError::MintNotAccepted(url) => assert_eq!(url, "https://evil.example"),
            other => panic!("expected MintNotAccepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_accepted_mints_accepts_all() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://any-mint.example")
            .await
            .expect("empty accepted_mints should accept all");
    }

    #[tokio::test]
    async fn seed_load_or_create_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("seed.bin");

        let seed1 = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed1.len(), 64);

        let seed2 = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed1, seed2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&seed_path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn first_boot_writes_mnemonic_and_matching_seed() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");

        let seed = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        let phrase = std::fs::read_to_string(&mnemonic_path).unwrap();
        assert_eq!(phrase.split_whitespace().count(), 24);

        // BIP39 determinism: the phrase on disk must re-derive the seed.
        assert_eq!(mnemonic_to_seed(phrase.trim()).unwrap(), seed);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&mnemonic_path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn mnemonic_only_boot_self_heals_seed() {
        // Crash between the two writes: mnemonic persisted, seed not.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");

        let mnemonic = bip39::Mnemonic::generate(24).unwrap();
        std::fs::write(&mnemonic_path, mnemonic.to_string()).unwrap();

        let seed = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed, mnemonic.to_seed(""));
        assert!(seed_path.exists());
    }

    #[tokio::test]
    async fn seed_only_boot_is_backward_compatible() {
        // Pre-mnemonic wallet: raw seed, no mnemonic file.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let original = [7u8; 64];
        std::fs::write(&seed_path, original).unwrap();

        let seed = TollWallet::load_or_create_seed(&seed_path).await.unwrap();
        assert_eq!(seed, original);
        assert!(!tmp.path().join("wallet_mnemonic.txt").exists());
    }

    #[tokio::test]
    async fn seed_mnemonic_mismatch_fails_loudly() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        let mnemonic_path = tmp.path().join("wallet_mnemonic.txt");

        std::fs::write(&seed_path, [9u8; 64]).unwrap();
        let other = bip39::Mnemonic::generate(24).unwrap();
        std::fs::write(&mnemonic_path, other.to_string()).unwrap();

        let err = TollWallet::load_or_create_seed(&seed_path)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }

    #[tokio::test]
    async fn corrupt_seed_file_fails_instead_of_regenerating() {
        // AGENTS.md / cashu-service pattern: never silently recreate wallet
        // state — a fresh seed over an existing wallet orphans every
        // deterministic derivation.
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("wallet_seed.bin");
        std::fs::write(&seed_path, b"too short").unwrap();

        let err = TollWallet::load_or_create_seed(&seed_path)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("wrong size"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_dir_sync_failure_propagates() {
        // Codex P1 on #37: a write+rename that succeeds while the parent
        // directory cannot be opened for fsync is NOT a known-durable
        // identity write — the error must surface, not be discarded. A
        // 0o333 directory (write+execute, no read) admits file
        // create/rename but refuses directory open, isolating exactly the
        // discarded branch.
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ids");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o333)).unwrap();

        // Root bypasses permission bits; the branch under test cannot
        // fire there, so the red-proof does not apply in that context.
        if std::fs::File::open(&dir).is_ok() {
            eprintln!("skipping: directory permission bits not enforced (root?)");
            return;
        }

        let result = TollWallet::write_file_private(&dir.join("wallet_seed.bin"), &[7u8; 64]).await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "unproven durability must fail loudly");
    }

    #[test]
    fn mnemonic_derivation_matches_bip39_vector() {
        // BIP39 vector (independently computed, empty passphrase): the
        // all-zeros-entropy 12-word phrase.
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let seed = mnemonic_to_seed(phrase).unwrap();
        let expected = hex_literal_expect(
            "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc19a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4",
        );
        assert_eq!(seed, expected);
    }

    fn hex_literal_expect(hex_str: &str) -> [u8; 64] {
        let mut out = [0u8; 64];
        let bytes = hex::decode(hex_str).unwrap();
        out.copy_from_slice(&bytes);
        out
    }

    #[tokio::test]
    async fn get_balance_returns_zero_for_new_wallet() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet.ensure_mint("https://mint.example").await.unwrap();
        let bal = wallet.get_balance().await.unwrap();
        assert_eq!(bal, 0);
    }

    #[tokio::test]
    async fn get_balance_by_mint_returns_per_mint() {
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet.ensure_mint("https://mint1.example").await.unwrap();
        wallet.ensure_mint("https://mint2.example").await.unwrap();
        let balances = wallet.get_balance_by_mint().await.unwrap();
        assert_eq!(balances.len(), 2);
        for (_, bal) in &balances {
            assert_eq!(*bal, 0);
        }
    }

    #[tokio::test]
    async fn db_path_sanitizes_url() {
        let tmp = TempDir::new().unwrap();
        let wallet = make_test_wallet(tmp.path(), vec![]);
        let path = wallet.db_path_for_mint("https://mint.coinos.io");
        assert!(path.starts_with(tmp.path()));
        assert!(path.extension().is_some_and(|e| e == "sqlite"));
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(!fname.contains('/'));
        assert!(!fname.contains(':'));
    }

    #[tokio::test]
    async fn receive_with_nonexistent_mint_errors() {
        let tmp = TempDir::new().unwrap();
        let wallet = make_test_wallet(tmp.path(), vec![]);
        let token = "cashuBo2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEalpQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";
        let result = wallet.receive(token).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn concurrent_operations_dont_panic() {
        // TDD Task 3.3: concurrent operations should be serialized by the mutex
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://testnut.cashu.space")
            .await
            .unwrap();

        let wallet_arc = Arc::new(wallet);
        let mut handles = Vec::new();

        for _ in 0..3 {
            let w = wallet_arc.clone();
            handles.push(tokio::spawn(async move {
                let _ = w.get_balance().await;
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn timeout_protection_on_receive() {
        // Receive should not hang forever — timeout wrapper exists
        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        wallet
            .ensure_mint("https://nonexistent.localhost.invalid")
            .await
            .unwrap();

        let token = "cashuBo2FteBtodHRwczovL25vbmV4aXN0ZW50LmxvY2FsaG9zdC5pbnZhbGlkYXVjc2F0YQ==";
        let result = tokio::time::timeout(Duration::from_secs(35), wallet.receive(token)).await;

        assert!(result.is_ok(), "receive should not hang forever");
    }

    /// Codex P1 on #42: recovery that merely SKIPS a saga (mint unreachable —
    /// cdk returns Ok(Skipped) by design) has NOT reconciled the wallet, so
    /// the money op must abort with a distinct cause instead of proceeding
    /// to spend over undecided state. A `SwapRequested` saga needs the mint
    /// to settle; against an instantly-refused one, recovery skips and the
    /// receive must refuse to move value.
    #[tokio::test]
    async fn receive_aborts_when_recovery_skips_a_saga() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Id, Proof};
        use cdk::wallet::types::{
            OperationData, ProofInfo, ReceiveOperationData, ReceiveSagaState, WalletSaga,
            WalletSagaState,
        };
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        // TCP port 1 on loopback: refused instantly, no external network.
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();

        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();
        let saga_id = uuid::Uuid::new_v4();
        let proof = Proof::new(
            cdk::Amount::from(1),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("interrupted-saga-input-secret"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let y = hash_to_curve(proof.secret.as_bytes()).unwrap();

        let store = {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof: proof.clone(),
                        y,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
            guard
                .localstore
                .reserve_proofs(vec![y], &saga_id)
                .await
                .unwrap();
            guard
                .localstore
                .add_saga(WalletSaga::new(
                    saga_id,
                    WalletSagaState::Receive(ReceiveSagaState::SwapRequested),
                    cdk::Amount::from(1),
                    mint_url.clone(),
                    cdk::nuts::CurrencyUnit::Sat,
                    OperationData::Receive(ReceiveOperationData {
                        token: None,
                        counter_start: None,
                        counter_end: None,
                        amount: Some(cdk::Amount::from(1)),
                        blinded_messages: None,
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(
                guard.localstore.get_incomplete_sagas().await.unwrap().len(),
                1,
                "plant sanity: one incomplete saga before the op"
            );
            guard.localstore.clone()
        };

        let fresh = Proof::new(
            cdk::Amount::from(2),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("fresh-token-secret"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let token_str =
            cdk::nuts::Token::new(mint_url, vec![fresh], None, cdk::nuts::CurrencyUnit::Sat)
                .to_string();

        let err = wallet.receive(&token_str).await.expect_err("must abort");
        assert!(
            matches!(err, WalletError::SagaRecovery(ref m) if m.contains("skipped")),
            "expected SagaRecovery abort with unresolved sagas, got {err:?}"
        );

        let remaining = store.get_incomplete_sagas().await.unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "the skipped saga survives for the next recovery attempt"
        );
    }

    /// Issue #33: an interrupted saga left behind by a mid-session timeout
    /// must be settled BEFORE the next money move on that wallet — not left
    /// unresolved until the next boot. Plants exactly what a killed process
    /// leaves in SQLite (a reserved input proof + a `ProofsPending` receive
    /// saga — the state a pre-swap crash leaves, compensable locally) and
    /// asserts the saga is gone after the next receive, proving pre-op
    /// recovery actually ran. The receive itself still fails (dead mint),
    /// so no money moves in either version — the observable is the saga's
    /// convergence, not the error kind.
    #[tokio::test]
    async fn receive_settles_interrupted_saga_before_moving_money() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Id, Proof};
        use cdk::wallet::types::{
            OperationData, ProofInfo, ReceiveOperationData, ReceiveSagaState, WalletSaga,
            WalletSagaState,
        };
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        // TCP port 1 on loopback: refused instantly, no external network.
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();

        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();
        let saga_id = uuid::Uuid::new_v4();
        let proof = Proof::new(
            cdk::Amount::from(1),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("interrupted-saga-input-secret"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let y = hash_to_curve(proof.secret.as_bytes()).unwrap();

        let store = {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof: proof.clone(),
                        y,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
            guard
                .localstore
                .reserve_proofs(vec![y], &saga_id)
                .await
                .unwrap();
            guard
                .localstore
                .add_saga(WalletSaga::new(
                    saga_id,
                    WalletSagaState::Receive(ReceiveSagaState::ProofsPending),
                    cdk::Amount::from(1),
                    mint_url.clone(),
                    cdk::nuts::CurrencyUnit::Sat,
                    OperationData::Receive(ReceiveOperationData {
                        token: None,
                        counter_start: None,
                        counter_end: None,
                        amount: Some(cdk::Amount::from(1)),
                        blinded_messages: None,
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(
                guard.localstore.get_incomplete_sagas().await.unwrap().len(),
                1,
                "plant sanity: one incomplete saga before the op"
            );
            guard.localstore.clone()
        };

        let fresh = Proof::new(
            cdk::Amount::from(2),
            Id::from_bytes(&[0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap(),
            cdk::secret::Secret::new("fresh-token-secret"),
            cdk::nuts::PublicKey::from_str(
                "026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198",
            )
            .unwrap(),
        );
        let token_str =
            cdk::nuts::Token::new(mint_url, vec![fresh], None, cdk::nuts::CurrencyUnit::Sat)
                .to_string();

        let err = wallet.receive(&token_str).await.expect_err("dead mint");
        assert!(
            matches!(err, WalletError::Cdk(_)),
            "receive itself fails at the dead mint, got {err:?}"
        );

        let remaining = store.get_incomplete_sagas().await.unwrap();
        assert!(
            remaining.is_empty(),
            "pre-op recovery must settle the interrupted saga this call, not at next boot; still present: {:?}",
            remaining.iter().map(|s| s.state).collect::<Vec<_>>()
        );
    }

    /// Issue #31: the saga gate links per-token (by input Ys), not per-mint.
    /// One stuck receive saga holding token A's inputs must defer token A —
    /// but sibling token B of the SAME mint has disjoint inputs and proceeds
    /// (the old per-mint gate deferred B too, delaying value never at risk).
    /// A saga with nothing linkable on record still blocks (conservative).
    #[tokio::test]
    async fn saga_gate_links_per_token_not_per_mint() {
        use cdk::dhke::hash_to_curve;
        use cdk::nuts::{nut07::State, Id, Proof};
        use cdk::wallet::types::{
            OperationData, ProofInfo, ReceiveOperationData, ReceiveSagaState, WalletSaga,
            WalletSagaState,
        };
        use std::str::FromStr;

        let tmp = TempDir::new().unwrap();
        let mut wallet = make_test_wallet(tmp.path(), vec![]);
        let mint = "http://127.0.0.1:1";
        wallet.ensure_mint(mint).await.unwrap();

        let mint_url = cdk::mint_url::MintUrl::from_str(mint).unwrap();
        let pubkey = |hex: &str| cdk::nuts::PublicKey::from_str(hex).unwrap();
        // NUT-02: a keyset id is derived from its keys — the planted store
        // rows must carry a self-consistent id or `keysets()` rejects them.
        let mut key_map = std::collections::BTreeMap::new();
        key_map.insert(
            cdk::Amount::from(1),
            pubkey("026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198"),
        );
        let test_keys = cdk::nuts::Keys::new(key_map);
        let keyset = Id::v1_from_keys(&test_keys);

        let make_token = |secret: &str| {
            cdk::nuts::Token::new(
                mint_url.clone(),
                vec![Proof::new(
                    cdk::Amount::from(1),
                    keyset,
                    cdk::secret::Secret::new(secret),
                    pubkey("026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198"),
                )],
                None,
                cdk::nuts::CurrencyUnit::Sat,
            )
            .to_string()
        };
        let token_a = make_token("token-a-input-secret");
        let token_b = make_token("token-b-input-secret");

        // Interrupted receive for token A: its proof reserved by the saga.
        let proof_a = Proof::new(
            cdk::Amount::from(1),
            keyset,
            cdk::secret::Secret::new("token-a-input-secret"),
            pubkey("026562efcfadc8e86d44da6a8adf80633d974302e62c850774db1fb36ff4cc7198"),
        );
        let y_a = hash_to_curve(proof_a.secret.as_bytes()).unwrap();
        let saga_id = uuid::Uuid::new_v4();

        {
            let w = wallet.wallets.get(mint).unwrap().clone();
            let guard = w.lock().await;
            // #44's gate resolves V3/V4 proofs through the keyset cache
            // (CacheThenNetwork): seed the keyset row so the query stays
            // purely local, as it is in production after ensure_mint.
            guard
                .localstore
                .add_mint(mint_url.clone(), None)
                .await
                .unwrap();
            guard
                .localstore
                .add_mint_keysets(
                    mint_url.clone(),
                    vec![cdk::nuts::KeySetInfo {
                        id: keyset,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        active: true,
                        input_fee_ppk: 0,
                        final_expiry: None,
                    }],
                )
                .await
                .unwrap();
            // `Wallet::keysets` drops infos without key material.
            guard
                .localstore
                .add_keys(cdk::nuts::KeySet {
                    id: keyset,
                    unit: cdk::nuts::CurrencyUnit::Sat,
                    active: Some(true),
                    keys: test_keys.clone(),
                    input_fee_ppk: 0,
                    final_expiry: None,
                })
                .await
                .unwrap();
            guard
                .localstore
                .update_proofs(
                    vec![ProofInfo {
                        proof: proof_a,
                        y: y_a,
                        mint_url: mint_url.clone(),
                        state: State::Unspent,
                        spending_condition: None,
                        unit: cdk::nuts::CurrencyUnit::Sat,
                        derivation_index: None,
                        used_by_operation: None,
                        created_by_operation: None,
                    }],
                    vec![],
                )
                .await
                .unwrap();
            guard
                .localstore
                .reserve_proofs(vec![y_a], &saga_id)
                .await
                .unwrap();
            guard
                .localstore
                .add_saga(WalletSaga::new(
                    saga_id,
                    WalletSagaState::Receive(ReceiveSagaState::SwapRequested),
                    cdk::Amount::from(1),
                    mint_url.clone(),
                    cdk::nuts::CurrencyUnit::Sat,
                    OperationData::Receive(ReceiveOperationData {
                        token: Some(token_a.clone()),
                        counter_start: None,
                        counter_end: None,
                        amount: Some(cdk::Amount::from(1)),
                        blinded_messages: None,
                    }),
                ))
                .await
                .unwrap();
        }

        assert!(
            wallet.mint_has_unresolved_receive(&token_a).await.unwrap(),
            "token A's own inputs are held by the interrupted saga — deferred"
        );
        assert!(
            !wallet.mint_has_unresolved_receive(&token_b).await.unwrap(),
            "sibling token B has disjoint inputs — must proceed (issue #31)"
        );

        // (#44's gate is proof-state based: an orphan saga holding no
        // reserved proofs defers nothing — no conservative block needed.)
    }
}
