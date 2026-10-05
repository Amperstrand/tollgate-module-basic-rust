//! Unix socket CLI server.
//!
//! Listens on /var/run/tollgate.sock (or TOLLGATE_TEST_CONFIG_DIR/tollgate.sock).
//! Mode 0660. Line-delimited JSON request/response.
//!
/// Commands: version, status, "wallet info", "wallet balance", "migrate <path>"
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::config;
use crate::http::AppState;

pub mod client;
pub mod drain_journal;
pub mod jsonproto;
pub mod ssl;
pub mod x509;

/// Socket path — honors TOLLGATE_TEST_CONFIG_DIR for tests.
pub fn socket_path() -> PathBuf {
    std::env::var("TOLLGATE_TEST_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("tollgate.sock"))
        .unwrap_or_else(|_| PathBuf::from("/var/run/tollgate.sock"))
}

/// Version info returned by the `version` command.
pub fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_COMMIT").unwrap_or("0000000");
    let build_time = option_env!("BUILD_TIME").unwrap_or("unknown");
    let rust_version = option_env!("RUSTC_VERSION").unwrap_or("unknown");

    format!(
        "version: {version}\n\
         commit: {commit}\n\
         build_time: {build_time}\n\
         rust_version: {rust_version}\n\
         openwrt: target={arch}\n",
        arch = std::env::consts::ARCH
    )
}

/// Start the Unix socket CLI server with shared AppState.
pub async fn serve(state: Arc<AppState>) -> std::io::Result<()> {
    let path = socket_path();

    // Remove stale socket
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    // Create parent dir if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&path)?;

    // Set mode 0660
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    }

    tracing::info!(socket = %path.display(), "CLI Unix socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(handle_connection(stream, state));
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed on CLI socket");
            }
        }
    }
}

async fn handle_connection(stream: tokio::net::UnixStream, state: Arc<AppState>) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let cmd = line.trim();
                let response = if cmd.starts_with('{') {
                    jsonproto::handle_json_line(cmd, &state).await
                } else {
                    handle_command(cmd, &state).await
                };
                if let Err(e) = writer.write_all(response.as_bytes()).await {
                    tracing::warn!(error = %e, "write failed on CLI socket");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "read failed on CLI socket");
                break;
            }
        }
    }
}

async fn handle_wallet_drain(state: &AppState) -> String {
    let wallet_guard = state.wallet.read().await;
    if let Some(ref wallet) = *wallet_guard {
        let balances = wallet.get_balance_by_mint().await.unwrap_or_default();

        let cfg_dir = config::config_dir();
        let mut tokens: Vec<serde_json::Value> = Vec::new();
        for (mint_url, balance) in &balances {
            if *balance > 0 {
                // Issue #46: consult the journal + local saga state BEFORE
                // re-issuing a drain — an earlier drain's send saga may
                // still be settling (its output token may exist undelivered).
                let op = format!(
                    "drain-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                );
                let id = crate::payout_journal::entry_id(mint_url, "cli-drain", &op);
                let unresolved = wallet
                    .mint_has_unresolved_send(mint_url)
                    .await
                    .unwrap_or(false);
                if unresolved {
                    tracing::error!(
                        mint = %mint_url,
                        "drain deferred: a previous drain's send saga is still settling — its token may exist undelivered; retry after settlement"
                    );
                    tokens.push(serde_json::json!({
                        "mint_url": mint_url,
                        "deferred": "previous drain still settling",
                    }));
                    continue;
                }
                let intent = crate::payout_journal::PayoutEntry {
                    id,
                    ts: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    token: None,
                    mint: mint_url.clone(),
                    identity: "cli-drain".to_string(),
                    invoice: op.clone(),
                    amount_sat: *balance,
                    literal_invoice: false,
                    phase: crate::payout_journal::PayoutPhase::Intent,
                };
                if let Err(e) = crate::payout_journal::append_entry(&cfg_dir, &intent) {
                    tracing::error!(error = %e, "CRITICAL: payout journal unavailable — refusing to drain with no recovery record");
                    tokens.push(serde_json::json!({
                        "mint_url": mint_url,
                        "error": "journal unavailable; drain refused",
                    }));
                    continue;
                }
                match wallet.send(mint_url, *balance, false).await {
                    Ok(token) => {
                        let _ = crate::payout_journal::append_entry(
                            &cfg_dir,
                            &crate::payout_journal::PayoutEntry {
                                token: Some(token.clone()),
                                phase: crate::payout_journal::PayoutPhase::Paid,
                                ..intent.clone()
                            },
                        );
                        tokens.push(serde_json::json!({
                            "mint_url": mint_url,
                            "token": token,
                            "amount": balance,
                        }));
                    }
                    Err(e) => {
                        // Ambiguous, not failed (#46 / #45 semantics): the
                        // send may have produced a token after the caller
                        // gave up — the saga settles at next recovery.
                        let _ = crate::payout_journal::append_entry(
                            &cfg_dir,
                            &crate::payout_journal::PayoutEntry {
                                phase: crate::payout_journal::PayoutPhase::Ambiguous,
                                ..intent.clone()
                            },
                        );
                        tracing::error!(
                            error = %e,
                            mint = %mint_url,
                            "drain outcome unknown — journaled ambiguous; the send saga settles at next recovery; a token may exist undelivered"
                        );
                        tokens.push(serde_json::json!({
                            "mint_url": mint_url,
                            "ambiguous": "drain outcome unknown; check 'status' and wallet transactions after settlement",
                        }));
                    }
                }
            }
        }

        serde_json::json!({
            "success": true,
            "message": serde_json::to_string(&tokens).unwrap_or_default()
        })
        .to_string()
            + "\n"
    } else {
        serde_json::json!({
            "success": false,
            "error": "wallet not initialized"
        })
        .to_string()
            + "\n"
    }
}

async fn handle_wallet_fund(state: &AppState, token: &str) -> String {
    let wallet_guard = state.wallet.read().await;
    if let Some(ref wallet) = *wallet_guard {
        // Issue #46: CLI receives get the same durable-intent discipline as
        // customer payments (#43) — a timed-out fund may still have landed;
        // reconciliation settles it (value lands in the wallet via the same
        // saga recovery; no session is owed for the CLI marker MAC).
        let cfg_dir = config::config_dir();
        let entry = crate::payment_journal::PaymentEntry {
            id: crate::payment_journal::token_id(token),
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            token: token.to_string(),
            mac: crate::payment_journal::CLI_FUND_MAC.to_string(),
            mint: String::new(),
            price_per_step: 1,
            step_size: 1,
            metric: "cli".to_string(),
            phase: crate::payment_journal::PaymentPhase::Intent,
        };
        if let Err(e) = crate::payment_journal::append_entry(&cfg_dir, &entry) {
            tracing::error!(error = %e, "CRITICAL: payment journal unavailable — refusing to move value with no recovery record");
            return serde_json::json!({
                "success": false,
                "error": "payment journal unavailable; fund refused"
            })
            .to_string()
                + "\n";
        }
        match wallet.receive(token).await {
            Ok(amount) => {
                tracing::info!(amount, "wallet funded via CLI");
                let _ = crate::payment_journal::append_entry(
                    &cfg_dir,
                    &crate::payment_journal::PaymentEntry {
                        phase: crate::payment_journal::PaymentPhase::Received {
                            amount_sat: amount,
                        },
                        ..entry
                    },
                );
                serde_json::json!({
                    "success": true,
                    "message": format!("received {amount} sats")
                })
                .to_string()
                    + "\n"
            }
            Err(e) => {
                // Ambiguous, not failed (#43 semantics): a lost response
                // after the mint accepted surfaces as a Cdk error too.
                let _ = crate::payment_journal::append_entry(
                    &cfg_dir,
                    &crate::payment_journal::PaymentEntry {
                        phase: crate::payment_journal::PaymentPhase::TimeoutUnknown,
                        ..entry
                    },
                );
                serde_json::json!({
                    "success": false,
                    "error": format!("wallet receive failed (outcome journaled; reconciliation will settle it): {e}")
                })
                .to_string()
                    + "\n"
            }
        }
    } else {
        serde_json::json!({
            "success": false,
            "error": "wallet not initialized"
        })
        .to_string()
            + "\n"
    }
}

async fn handle_health(state: &AppState) -> String {
    let wallet_loaded = state.wallet.read().await.is_some();
    let active_sessions = state.sessions.lock().await.sessions.len();

    let health = serde_json::json!({
        "http_running": true,
        "wallet_loaded": wallet_loaded,
        "mints_reachable": active_sessions,
    });
    serde_json::json!({
        "success": true,
        "message": health.to_string()
    })
    .to_string()
        + "\n"
}

fn handle_config_get(state: &AppState, key: &str) -> String {
    if key.is_empty() {
        let json = serde_json::to_string(&*state.config).unwrap_or_default();
        return serde_json::json!({
            "success": true,
            "message": json
        })
        .to_string()
            + "\n";
    }
    let value = match key {
        "metric" => state.config.metric.clone(),
        "step_size" => state.config.step_size.to_string(),
        "config_version" => state.config.config_version.clone(),
        "log_level" => state.config.log_level.clone(),
        "show_setup" => state.config.show_setup.to_string(),
        "reseller_mode" => state.config.reseller_mode.to_string(),
        _ => {
            return serde_json::json!({
                "success": false,
                "error": format!("unknown config key: {key}")
            })
            .to_string()
                + "\n";
        }
    };
    serde_json::json!({
        "success": true,
        "message": value
    })
    .to_string()
        + "\n"
}

fn handle_config_set(rest: &str) -> String {
    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
    if parts.len() != 2 {
        return serde_json::json!({
            "success": false,
            "error": "usage: config set <key> <value>"
        })
        .to_string()
            + "\n";
    }
    let key = parts[0];
    let value = parts[1];

    match key {
        "metric" | "step_size" => {
            let mut current = crate::config::load_config()
                .unwrap_or(None)
                .unwrap_or_default();
            match key {
                "metric" => {
                    if value != "bytes" && value != "milliseconds" {
                        return serde_json::json!({
                            "success": false,
                            "error": format!("metric must be 'bytes' or 'milliseconds', got '{value}'")
                        })
                        .to_string() + "\n";
                    }
                    current.metric = value.to_string();
                }
                "step_size" => {
                    match value.parse::<u64>() {
                        Ok(n) if n > 0 => current.step_size = n,
                        _ => return serde_json::json!({
                            "success": false,
                            "error": format!("step_size must be a positive integer, got '{value}'")
                        })
                        .to_string()
                            + "\n",
                    }
                }
                _ => {}
            }

            match crate::config::save_config(&current) {
                Ok(_) => serde_json::json!({
                    "success": true,
                    "message": format!("{key} updated to {value} (restart required to take effect)")
                })
                .to_string()
                    + "\n",
                Err(e) => {
                    serde_json::json!({
                        "success": false,
                        "error": format!("failed to save config: {e}")
                    })
                    .to_string()
                        + "\n"
                }
            }
        }
        _ => {
            serde_json::json!({
                "success": false,
                "error": format!("unsupported config key: {key} (supported: metric, step_size)")
            })
            .to_string()
                + "\n"
        }
    }
}

async fn handle_command(cmd: &str, state: &AppState) -> String {
    match cmd {
        "version" => version_string(),
        "status" => {
            let m = crate::migration::summarize_state(&config::config_dir());
            let p = crate::payment_journal::summarize(&config::config_dir());
            let po = crate::payout_journal::summarize(&config::config_dir());
            serde_json::json!({
                "success": true,
                "message": "running",
                "migration": {
                    "marker": m.marker,
                    "imported_sat": m.imported_sat,
                    "failed": m.failed,
                    "pending": m.pending,
                    "spent": m.spent,
                    "spent_sat": m.spent_sat,
                    "partially_spent": m.partially_spent,
                    "partially_spent_unspent_sat": m.partially_spent_unspent_sat,
                    "remainder_recovered_sat": m.remainder_recovered_sat,
                    "settled": m.is_settled(),
                },
                "payments": {
                    "total": p.total,
                    "needs_reconciliation": p.needs_reconciliation,
                    "received_sat": p.received_sat,
                },
                "payouts": {
                    "total": po.total,
                    "paid_sat": po.paid_sat,
                    "ambiguous": po.ambiguous,
                }
            })
            .to_string()
                + "\n"
        }
        "wallet info" => {
            let wallet_guard = state.wallet.read().await;
            if let Some(ref wallet) = *wallet_guard {
                let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
                let mints: Vec<serde_json::Value> = balances
                    .iter()
                    .map(|(url, bal)| serde_json::json!({"url": url, "balance": bal}))
                    .collect();
                drop(wallet_guard);
                serde_json::json!({
                    "success": true,
                    "message": serde_json::to_string(&mints).unwrap_or_default()
                })
                .to_string()
                    + "\n"
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "no wallet configured"
                })
                .to_string()
                    + "\n"
            }
        }
        "wallet balance" => {
            let wallet_guard = state.wallet.read().await;
            if let Some(ref wallet) = *wallet_guard {
                match wallet.get_balance().await {
                    Ok(balance) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": true,
                            "message": balance.to_string()
                        })
                        .to_string()
                            + "\n"
                    }
                    Err(e) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": false,
                            "error": format!("wallet error: {e}")
                        })
                        .to_string()
                            + "\n"
                    }
                }
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "0"
                })
                .to_string()
                    + "\n"
            }
        }
        // NUT-13 disaster recovery: re-derive proofs from the wallet seed
        // via the mint's NUT-09 restore endpoint (CDK Wallet::restore —
        // batch 100, gap 3, NUT-07 pruning). Runs against every registered
        // mint; needs the mints configured and reachable.
        "wallet restore" => {
            let wallet_guard = state.wallet.read().await;
            if let Some(ref wallet) = *wallet_guard {
                match wallet.restore_all().await {
                    Ok(results) => {
                        drop(wallet_guard);
                        let mints: Vec<serde_json::Value> = results
                            .iter()
                            .map(|(url, r)| match r {
                                Ok(r) => serde_json::json!({
                                    "url": url,
                                    "unspent_sat": r.unspent_sat,
                                    "spent_sat": r.spent_sat,
                                    "pending_sat": r.pending_sat,
                                }),
                                Err(e) => serde_json::json!({
                                    "url": url,
                                    "error": e,
                                }),
                            })
                            .collect();
                        serde_json::json!({
                            "success": true,
                            "message": serde_json::to_string(&mints)
                                .unwrap_or_default()
                        })
                        .to_string()
                            + "\n"
                    }
                    Err(e) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": false,
                            "error": format!("wallet restore failed: {e}")
                        })
                        .to_string()
                            + "\n"
                    }
                }
            } else {
                serde_json::json!({
                    "success": false,
                    "error": "no wallet configured"
                })
                .to_string()
                    + "\n"
            }
        }
        "wallet drain" => handle_wallet_drain(state).await,
        cmd if cmd.starts_with("wallet fund ") => {
            let token = cmd.strip_prefix("wallet fund ").unwrap().trim();
            handle_wallet_fund(state, token).await
        }
        cmd if cmd.starts_with("wallet ") => {
            serde_json::json!({
                "success": false,
                "error": format!("unknown wallet command: {cmd}")
            })
            .to_string()
                + "\n"
        }
        "health" => handle_health(state).await,
        cmd if cmd.starts_with("config get") => {
            let key = cmd.strip_prefix("config get").unwrap().trim();
            handle_config_get(state, key)
        }
        cmd if cmd.starts_with("config set ") => {
            let rest = cmd.strip_prefix("config set ").unwrap().trim();
            handle_config_set(rest)
        }
        cmd if cmd.starts_with("migrate ") => {
            let tokens_path = cmd.strip_prefix("migrate ").unwrap().trim();
            match run_migration(tokens_path, state).await {
                Ok(report) => {
                    serde_json::json!({
                        "success": true,
                        "message": report
                    })
                    .to_string()
                        + "\n"
                }
                Err(e) => {
                    serde_json::json!({
                        "success": false,
                        "error": format!("migration failed: {e}")
                    })
                    .to_string()
                        + "\n"
                }
            }
        }
        _ => {
            serde_json::json!({
                "success": false,
                "error": format!("unknown command: {cmd}")
            })
            .to_string()
                + "\n"
        }
    }
}

/// Run wallet migration from a tokens.jsonl file.
///
/// Each line is a Cashu V3/V4 token string. For each token, calls
/// `wallet.receive()`. Requires mint connectivity. After all receives,
/// optionally advances keyset counters using keyset_counters.json.
///
/// Returns a JSON report string with imported/failed counts.
async fn run_migration(
    tokens_path: &str,
    state: &AppState,
) -> Result<String, crate::error::CliError> {
    let content = tokio::fs::read_to_string(tokens_path).await.map_err(|e| {
        crate::error::CliError::TokenFileRead {
            path: tokens_path.to_string(),
            reason: e.to_string(),
        }
    })?;

    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();

    let wallet_guard = state.wallet.read().await;
    let wallet = wallet_guard
        .as_ref()
        .ok_or(crate::error::CliError::NoWallet)?;

    let mut imported: u64 = 0;
    let mut failed: u64 = 0;
    let mut errors: Vec<String> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let token = line.trim();
        match wallet.receive(token).await {
            Ok(amount) => {
                imported += 1;
                tracing::info!(token_idx = i, amount, "migrated token");
            }
            Err(e) => {
                failed += 1;
                let err = format!("token {i}: {e}");
                tracing::warn!(error = %err, "migration token failed");
                errors.push(err);
            }
        }
    }

    drop(wallet_guard);

    let report = serde_json::json!({
        "total": total,
        "imported": imported,
        "failed": failed,
        "errors": errors.iter().take(10).collect::<Vec<_>>(),
    });

    Ok(serde_json::to_string(&report).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::identity::MerchantIdentity;
    use crate::portal::{CaptivePortal, NdsPortal};
    use crate::session::SessionManager;
    use crate::wallet::TollWallet;
    use serial_test::serial;

    fn make_test_state() -> Arc<AppState> {
        // No env mutation here: handle_command never reads
        // TOLLGATE_TEST_CONFIG_DIR, and a Once-set global would clobber the
        // value installed by serialized tests in other cli modules.
        let config = Arc::new(Config::new_default());

        let secp = secp256k1::Secp256k1::new();
        let (secret_key, _) = secp.generate_keypair(&mut rand::thread_rng());
        let identity = Arc::new(MerchantIdentity {
            name: "merchant".to_string(),
            secret_key,
        });

        let wallet = Arc::new(tokio::sync::RwLock::new(Some(TollWallet::new(
            [0u8; 64],
            vec![],
            std::path::PathBuf::from("/tmp"),
        ))));
        let sessions = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
        let portal: Arc<dyn CaptivePortal> = Arc::new(NdsPortal::new());
        let verifier = Arc::new(crate::wallet::verify::TokenVerifier::new(vec![]));
        let rate_limiter = Arc::new(crate::rate_limiter::RateLimiter::new(1000));
        let ln_quotes = Arc::new(crate::lightning_quotes::QuoteStore::load(
            std::path::Path::new("/tmp"),
        ));
        Arc::new(AppState {
            config,
            identity,
            wallet,
            sessions,
            portal,
            verifier,
            rate_limiter,
            ln_quotes,
        })
    }

    #[tokio::test]
    async fn version_contains_required_fields() {
        let v = version_string();
        assert!(v.contains("version:"));
        assert!(v.contains("commit:"));
        assert!(v.contains("build_time:"));
        assert!(v.contains("rust_version:"));
        assert!(v.contains("openwrt"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn status_returns_running() {
        let state = make_test_state();
        let resp = handle_command("status", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "running");
        // Issue #32: migration health rides along on status.
        assert!(json["migration"].is_object());
        assert_eq!(json["migration"]["settled"], true);
        assert_eq!(json["migration"]["failed"], 0);
        assert_eq!(json["migration"]["pending"], 0);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn wallet_balance_returns_zero_for_empty_wallet() {
        let state = make_test_state();
        let resp = handle_command("wallet balance", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        // Balance will be "0" since the wallet has no mints registered
        assert_eq!(json["message"], "0");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn wallet_info_returns_json() {
        let state = make_test_state();
        let resp = handle_command("wallet info", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].is_string());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn wallet_fund_journals_intent_and_ambiguous_outcome() {
        // #46: CLI fund entries hit the payment journal — durable intent
        // before receive, ambiguous (never terminal-rejected) on error.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
        let state = make_test_state();
        let resp = handle_command("wallet fund cashuAnotarealtoken", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        let jp = dir.path().join("payment-journal.jsonl");
        let body = std::fs::read_to_string(jp).unwrap();
        assert!(
            body.contains("cli-fund"),
            "fund entries carry the CLI marker"
        );
        assert!(
            body.contains("timeout-unknown"),
            "receive errors are journaled ambiguous, not failed: {body}"
        );
        std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    }

    #[tokio::test]
    async fn wallet_restore_with_no_mints_reports_empty() {
        // No registered mints => restore is a no-op success (the network
        // path itself is CDK's Wallet::restore, exercised upstream and in
        // the field against a live mint).
        let state = make_test_state();
        let resp = handle_command("wallet restore", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].is_string());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn unknown_command_returns_error() {
        let state = make_test_state();
        let resp = handle_command("foobar", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("unknown command"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn migrate_nonexistent_file_returns_error() {
        let state = make_test_state();
        let resp = handle_command("migrate /nonexistent/tokens.jsonl", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("failed to read"));
    }

    #[tokio::test]
    async fn migrate_empty_file_returns_zero_totals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tokens_path = tmp.path().join("tokens.jsonl");
        std::fs::write(&tokens_path, "").unwrap();

        let state = make_test_state();
        let path_str = tokens_path.to_str().unwrap();
        let resp = handle_command(&format!("migrate {path_str}"), &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        let report: serde_json::Value =
            serde_json::from_str(json["message"].as_str().unwrap()).unwrap();
        assert_eq!(report["total"], 0);
        assert_eq!(report["imported"], 0);
        assert_eq!(report["failed"], 0);
    }

    #[tokio::test]
    async fn migrate_invalid_tokens_counted_as_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tokens_path = tmp.path().join("tokens.jsonl");
        // Two invalid token strings — wallet has no mints registered so receive will fail
        std::fs::write(&tokens_path, "not-a-token\ndefinitely-not-a-token\n").unwrap();

        let state = make_test_state();
        let path_str = tokens_path.to_str().unwrap();
        let resp = handle_command(&format!("migrate {path_str}"), &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        let report: serde_json::Value =
            serde_json::from_str(json["message"].as_str().unwrap()).unwrap();
        assert_eq!(report["total"], 2);
        assert_eq!(report["failed"], 2);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn test_health_command_returns_status() {
        let state = make_test_state();
        let resp = handle_command("health", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        let health: serde_json::Value =
            serde_json::from_str(json["message"].as_str().unwrap()).unwrap();
        assert_eq!(health["http_running"], true);
        assert_eq!(health["wallet_loaded"], true);
        assert!(health["mints_reachable"].as_u64().is_some());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn test_config_get_returns_value() {
        let state = make_test_state();
        let resp = handle_command("config get metric", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "bytes");
    }

    #[tokio::test]
    #[serial]
    #[serial_test::serial]
    async fn test_config_set_writes_to_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
        let config = crate::config::Config::new_default();
        crate::config::save_config(&config).unwrap();

        let state = make_test_state();
        let resp = handle_command("config set metric milliseconds", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);

        let reloaded = crate::config::load_config().unwrap().unwrap();
        assert_eq!(reloaded.metric, "milliseconds");

        std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn test_unknown_subcommand_under_wallet_returns_error() {
        let state = make_test_state();
        let resp = handle_command("wallet foobar", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("unknown wallet command"));
    }
}
