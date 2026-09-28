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
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::wallet::WalletSqliteDatabase;
#[cfg(test)]
use rand::Rng;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

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

pub use crate::error::WalletError;

/// One canonical mint identity for every persisted/compared URL
/// (AGENTS.md: route mint URLs through CDK's `MintUrl` form — lowercase
/// scheme/host, trailing slash trimmed — or alias spellings fork wallet
/// map keys, quote records, and DB filenames).
pub fn canonical_mint_url(url: &str) -> String {
    use std::str::FromStr;
    let trimmed = url.trim_end_matches('/');
    cdk::mint_url::MintUrl::from_str(trimmed)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| trimmed.to_string())
}

/// TollWallet wraps multiple CDK Wallet instances (one per mint URL) behind
/// a tokio Mutex for thread-safe serialized access. CDK's saga pattern
/// ensures operations are atomic — no swap-counter race.
pub struct TollWallet {
    wallets: HashMap<String, Arc<Mutex<Wallet>>>,
    seed: [u8; 64],
    accepted_mints: Vec<String>,
    db_dir: PathBuf,
}

impl TollWallet {
    /// Create a new TollWallet. Does NOT open any wallets — call `ensure_mint`.
    pub fn new(seed: [u8; 64], accepted_mints: Vec<String>, db_dir: PathBuf) -> Self {
        Self {
            wallets: HashMap::new(),
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
                .any(|m| canonical_mint_url(m) == canonical_mint_url(mint_url))
    }

    /// Register a mint and open a CDK wallet for it.
    /// Maps gonuts `AddMint(mintURL)` + `LoadWallet`.
    pub async fn ensure_mint(&mut self, mint_url: &str) -> Result<(), WalletError> {
        if !self.is_mint_accepted(mint_url) {
            return Err(WalletError::MintNotAccepted(mint_url.to_string()));
        }

        let normalized = canonical_mint_url(mint_url);
        if self.wallets.contains_key(normalized.as_str()) {
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

    /// Receive a Cashu token (maps gonuts `Receive`).
    ///
    /// CDK's receive is atomic — no counter race. Wrapped in 30s timeout.
    pub async fn receive(&self, token_str: &str) -> Result<u64, WalletError> {
        let token: cashu::nuts::Token = token_str
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

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            w.receive(token_str, ReceiveOptions::default()).await
        })
        .await;

        match result {
            Ok(Ok(amount)) => {
                let sat: u64 = amount.into();
                Ok(sat)
            }
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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
        let normalized = canonical_mint_url(&mint_url);
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
            Ok::<_, cdk::Error>(states.iter().all(|s| s.state == State::Spent))
        })
        .await;

        match result {
            Ok(Ok(true)) => Ok(Some(amount_sat)),
            Ok(Ok(false)) => Ok(None),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Whether the per-mint CDK wallet still holds an incomplete *receive*
    /// saga — an earlier receive for this mint that `recover_incomplete_sagas`
    /// has not settled. CDK deletes a saga exactly when its outcome is
    /// persisted: outputs recovered via NUT-19 replay or NUT-09 `/restore`,
    /// compensated, or — with a logged warning — closed value-less. While a
    /// receive saga is incomplete, the operation's outcome is undecided.
    ///
    /// Local SQLite query (no network): the migration reconciliation gate
    /// works even when the mint is unreachable (AGENTS.md: ambiguous results
    /// are reconciled, not retried).
    pub async fn mint_has_unresolved_receive(&self, token_str: &str) -> Result<bool, WalletError> {
        use cdk::wallet::types::WalletSagaState;

        let token: cashu::nuts::Token = token_str
            .parse()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| WalletError::TokenParse(format!("{e}")))?
            .to_string();
        let normalized = canonical_mint_url(&mint_url);

        let wallet = match self.wallets.get(normalized.as_str()) {
            Some(w) => w.clone(),
            // No wallet for this mint => no saga can exist for it; the
            // receive path will surface the underlying classification.
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
            .any(|s| matches!(s.state, WalletSagaState::Receive(_))))
    }

    /// Send tokens (maps gonuts `Send`).
    /// Returns the serialized Cashu V4 token string.
    pub async fn send(
        &self,
        mint_url: &str,
        amount_sat: u64,
        include_fee: bool,
    ) -> Result<String, WalletError> {
        let normalized = canonical_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let opts = SendOptions {
            include_fee,
            ..Default::default()
        };

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let prepared = w.prepare_send(Amount::from(amount_sat), opts).await?;
            let token = prepared.confirm(None).await?;
            Ok::<_, cdk::Error>(token.to_string())
        })
        .await;

        match result {
            Ok(Ok(token_str)) => Ok(token_str),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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
        let normalized = canonical_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
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
        let normalized = canonical_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
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
        let normalized = canonical_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            let proofs = w.mint(quote_id, SplitTarget::default(), None).await?;
            let total: u64 = proofs.iter().map(|p| -> u64 { p.amount.into() }).sum();
            Ok::<_, cdk::Error>(total)
        })
        .await;

        match result {
            Ok(Ok(total)) => Ok(total),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
        }
    }

    /// Request a melt quote + prepare melt (NUT-05, maps gonuts `RequestMeltQuote` + `Melt`).
    /// CDK flow: `melt_quote(BOLT11, invoice)` → `prepare_melt(quote_id, meta)` → `confirm()`.
    pub async fn melt(&self, mint_url: &str, invoice: &str) -> Result<MeltQuoteInfo, WalletError> {
        let normalized = canonical_mint_url(mint_url);
        let wallet = self
            .wallets
            .get(normalized.as_str())
            .ok_or_else(|| WalletError::WalletNotFound(normalized.to_string()))?
            .clone();

        let invoice_owned = invoice.to_string();
        let result = timeout(OP_TIMEOUT, async {
            let w = wallet.lock().await;
            // Step 1: create melt quote
            let quote = w
                .melt_quote(PaymentMethod::BOLT11, invoice_owned, None, None)
                .await?;
            // Step 2: prepare melt with the quote ID
            let prepared = w.prepare_melt(&quote.id, HashMap::new()).await?;
            // Step 3: confirm
            let finalized = prepared.confirm().await?;
            Ok::<_, cdk::Error>((quote, finalized))
        })
        .await;

        match result {
            Ok(Ok((quote, _finalized))) => Ok(MeltQuoteInfo {
                quote_id: quote.id,
                amount: quote.amount.into(),
                fee: quote.fee_reserve.into(),
            }),
            Ok(Err(e)) => Err(WalletError::Cdk(e)),
            Err(_) => Err(WalletError::Timeout(OP_TIMEOUT)),
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

    async fn write_file_private(path: &Path, bytes: &[u8]) -> Result<(), WalletError> {
        tokio::fs::write(path, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(())
    }

    fn mnemonic_path_for(seed_path: &Path) -> PathBuf {
        seed_path.with_file_name("wallet_mnemonic.txt")
    }

    /// NUT-13 restore for every registered mint (CDK `Wallet::restore`:
    /// batched NUT-09 restore with NUT-07 pruning, batch 100 / gap 3).
    /// The operator-facing recovery path after seed/mnemonic loss — see
    /// MIGRATION.md.
    pub async fn restore_all(&self) -> Result<Vec<(String, RestoredSummary)>, WalletError> {
        let mut out = Vec::new();
        for (mint_url, wallet) in &self.wallets {
            let restored = wallet
                .lock()
                .await
                .restore()
                .await
                .map_err(WalletError::Cdk)?;
            out.push((
                mint_url.clone(),
                RestoredSummary {
                    spent_sat: u64::from(restored.spent),
                    unspent_sat: u64::from(restored.unspent),
                    pending_sat: u64::from(restored.pending),
                },
            ));
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

    fn make_test_wallet(dir: &Path, accepted_mints: Vec<String>) -> TollWallet {
        let mut seed = [0u8; 64];
        rand::thread_rng().fill(&mut seed);
        TollWallet::new(seed, accepted_mints, dir.to_path_buf())
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
}
