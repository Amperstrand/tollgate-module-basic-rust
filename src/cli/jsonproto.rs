//! Go-compatible JSON command protocol for the CLI unix socket.
//!
//! A line starting with `{` is parsed as a `CLIMessage` and answered with a
//! single `CLIResponse` JSON line — the wire format Go's `tollgate` client
//! and `CLIServer` speak (`src/cli/{types,server,config,network}.go` in the
//! Go repo). Every other line keeps the historical plain-text protocol.

use serde::{Deserialize, Serialize};

use crate::http::AppState;

#[derive(Debug, Deserialize)]
pub struct CliMessage {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub flags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct CliResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// Seconds since the Unix epoch. Go marshals RFC3339 here; automation
    /// must not depend on the timestamp format, so a chrono-free epoch
    /// float keeps the field present without a new dependency.
    pub timestamp: f64,
}

impl CliResponse {
    pub fn ok(message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        CliResponse {
            success: true,
            message: Some(message.into()),
            data,
            error: None,
            progress: None,
            timestamp: now_epoch(),
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        CliResponse {
            success: false,
            message: None,
            data: None,
            error: Some(error.into()),
            progress: None,
            timestamp: now_epoch(),
        }
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

fn process_start() -> std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

/// Go `time.Duration.String()` shape, whole seconds: "2h5m30s", "5m3s", "42s".
fn format_go_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn uptime_string() -> String {
    format_go_duration(process_start().elapsed().as_secs())
}

fn version_info() -> String {
    format!("TollGate {}", env!("CARGO_PKG_VERSION"))
}

fn full_version_info() -> serde_json::Value {
    let openwrt_version = std::fs::read_to_string("/etc/openwrt_release")
        .map(|content| {
            content
                .lines()
                .find_map(|l| {
                    l.strip_prefix("DISTRIB_DESCRIPTION=")
                        .map(|v| v.trim_matches(['\'', '"']).to_string())
                })
                .unwrap_or_else(|| "unknown".to_string())
        })
        .unwrap_or_else(|_| "unknown".to_string());

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": option_env!("GIT_COMMIT").unwrap_or("unknown"),
        "build_time": option_env!("BUILD_TIME").unwrap_or("unknown"),
        "rust_version": option_env!("RUSTC_VERSION").unwrap_or("unknown"),
        "openwrt_version": openwrt_version,
    })
}

async fn check_connectivity() -> bool {
    tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", "3", "9.9.9.9"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn handle_version() -> CliResponse {
    CliResponse::ok(super::version_string(), Some(full_version_info()))
}

async fn handle_status(state: &AppState) -> CliResponse {
    let wallet_ok = state.wallet.read().await.is_some();
    let status = serde_json::json!({
        "running": true,
        "version": version_info(),
        "uptime": uptime_string(),
        "config_ok": true,
        "wallet_ok": wallet_ok,
        "network_ok": check_connectivity().await,
    });
    CliResponse::ok("Service status retrieved", Some(status))
}

async fn handle_health(state: &AppState) -> CliResponse {
    let wallet_ok = state.wallet.read().await.is_some();
    let health = serde_json::json!({
        "status": "ok",
        "version": version_info(),
        "config_ok": true,
        "wallet_ok": wallet_ok,
        "uptime": uptime_string(),
    });
    CliResponse::ok("healthy", Some(health))
}

async fn handle_wallet_balance(state: &AppState) -> CliResponse {
    let guard = state.wallet.read().await;
    let Some(wallet) = guard.as_ref() else {
        return CliResponse::error("wallet not initialized");
    };
    match wallet.get_balance().await {
        Ok(balance) => CliResponse::ok(
            format!("Total wallet balance: {balance} sats"),
            Some(serde_json::json!({ "balance_sats": balance })),
        ),
        Err(e) => CliResponse::error(format!("wallet error: {e}")),
    }
}

async fn handle_wallet_info(state: &AppState) -> CliResponse {
    let guard = state.wallet.read().await;
    let Some(wallet) = guard.as_ref() else {
        return CliResponse::error("wallet not initialized");
    };
    let total = match wallet.get_balance().await {
        Ok(b) => b,
        Err(e) => return CliResponse::error(format!("wallet error: {e}")),
    };
    let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
    let mint_balances: serde_json::Map<String, serde_json::Value> = balances
        .iter()
        .filter(|(_, v)| *v > 0)
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();

    CliResponse::ok(
        format!(
            "Wallet info - Total: {total} sats across {} mints",
            mint_balances.len()
        ),
        Some(serde_json::json!({
            "total_balance": total,
            "mint_count": mint_balances.len(),
            "mint_balances": mint_balances,
        })),
    )
}

async fn handle_wallet_fund(state: &AppState, token: &str) -> CliResponse {
    let guard = state.wallet.read().await;
    let Some(wallet) = guard.as_ref() else {
        return CliResponse::error("wallet not initialized");
    };
    // Same durable-intent discipline as the plain-text handler (#51/#46):
    // a crash or timeout after the mint accepted must leave a TollGate
    // record the reconciler can settle (Codex P1 on #52).
    let cfg_dir = crate::config::config_dir();
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
        tracing::error!(error = %e, "CRITICAL: payment journal unavailable — refusing to move value");
        return CliResponse::error("payment journal unavailable; fund refused");
    }
    match wallet.receive(token).await {
        Ok(amount) => {
            let _ = crate::payment_journal::append_entry(
                &cfg_dir,
                &crate::payment_journal::PaymentEntry {
                    phase: crate::payment_journal::PaymentPhase::Received { amount_sat: amount },
                    ..entry
                },
            );
            CliResponse::ok(
                format!("received {amount} sats"),
                Some(serde_json::json!({ "amount_received": amount })),
            )
        }
        Err(e) => {
            // Ambiguous, not failed (#43 semantics).
            let _ = crate::payment_journal::append_entry(
                &cfg_dir,
                &crate::payment_journal::PaymentEntry {
                    phase: crate::payment_journal::PaymentPhase::TimeoutUnknown,
                    ..entry
                },
            );
            CliResponse::error(format!(
                "wallet receive failed (outcome journaled; reconciliation will settle it): {e}"
            ))
        }
    }
}

/// Drain every mint with a non-zero balance into Cashu tokens. The drain is
/// not atomic across mints: each token is journaled durably before the next
/// mint is attempted, per-mint failures are collected, and successfully
/// produced tokens are always reported (Go issue #375 parity).
async fn handle_wallet_drain_cashu(state: &AppState) -> CliResponse {
    let guard = state.wallet.read().await;
    let Some(wallet) = guard.as_ref() else {
        return CliResponse::error("wallet not initialized");
    };
    let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
    if balances.is_empty() {
        return CliResponse::error("No mints found in wallet");
    }

    let mut tokens: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();
    let mut total_drained: u64 = 0;

    for (mint_url, balance) in &balances {
        if *balance == 0 {
            continue;
        }
        // Durable intent BEFORE the send (Codex P1 on #52): a crash between
        // send and the token journal must not strand withdrawn value with
        // no record; ambiguity (timeout) likewise leaves a marker.
        let cfg_dir = crate::config::config_dir();
        let op = format!(
            "drain-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        let pj_entry = crate::payout_journal::PayoutEntry {
            id: crate::payout_journal::entry_id(mint_url, "cli-drain", &op),
            token: None,
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            mint: mint_url.clone(),
            identity: "cli-drain".to_string(),
            invoice: op,
            amount_sat: *balance,
            literal_invoice: false,
            phase: crate::payout_journal::PayoutPhase::Intent,
        };
        if let Err(e) = crate::payout_journal::append_entry(&cfg_dir, &pj_entry) {
            tracing::error!(error = %e, mint = %mint_url, "CRITICAL: payout journal unavailable — refusing to drain");
            errors.push(serde_json::json!({
                "mint_url": mint_url,
                "error": "journal unavailable; drain refused",
            }));
            continue;
        }
        match wallet.send(mint_url, *balance, false).await {
            Ok(token) => {
                // Token durable in the payout journal immediately (the
                // per-mint drain journal remains the audit ledger).
                let _ = crate::payout_journal::append_entry(
                    &cfg_dir,
                    &crate::payout_journal::PayoutEntry {
                        token: Some(token.clone()),
                        phase: crate::payout_journal::PayoutPhase::TokenCreated,
                        ..pj_entry.clone()
                    },
                );
                if let Err(e) = super::drain_journal::append(mint_url, *balance, &token) {
                    tracing::error!(
                        mint = %mint_url,
                        error = %e,
                        "drained mint but failed to journal token; not draining further mints"
                    );
                    errors.push(serde_json::json!({
                        "mint_url": mint_url,
                        "error": format!(
                            "drained {balance} sats but could not make the token durable ({e}); the token is included in this response only"
                        ),
                    }));
                    tokens.push(serde_json::json!({
                        "mint_url": mint_url,
                        "balance_sats": balance,
                        "token": token,
                    }));
                    total_drained += balance;
                    break;
                }
                tokens.push(serde_json::json!({
                    "mint_url": mint_url,
                    "balance_sats": balance,
                    "token": token,
                }));
                total_drained += balance;
            }
            Err(e) => {
                tracing::warn!(mint = %mint_url, error = %e, "failed to drain mint");
                // Ambiguous, not plain-failed: the send may have produced a
                // token after the caller gave up (Codex P1 on #52).
                let _ = crate::payout_journal::append_entry(
                    &cfg_dir,
                    &crate::payout_journal::PayoutEntry {
                        phase: crate::payout_journal::PayoutPhase::Ambiguous,
                        ..pj_entry.clone()
                    },
                );
                errors.push(serde_json::json!({
                    "mint_url": mint_url,
                    "error": e.to_string(),
                }));
            }
        }
    }

    let partial = !tokens.is_empty() && !errors.is_empty();
    let success = errors.is_empty();
    let data = serde_json::json!({
        "success": success,
        "partial": partial,
        "tokens": tokens,
        "errors": errors,
        "total_sats": total_drained,
    });

    if !success {
        let summaries: Vec<String> = data["errors"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let m = e["mint_url"].as_str()?;
                        let r = e["error"].as_str()?;
                        Some(format!("Failed to drain mint {m}: {r}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let message = if tokens.is_empty() {
            format!(
                "Failed to drain {} mint(s); no tokens produced",
                errors.len()
            )
        } else {
            format!(
                "Partially drained {total_drained} sats from {} mint(s); {} mint(s) failed",
                tokens.len(),
                errors.len()
            )
        };
        return CliResponse {
            success: false,
            message: Some(message),
            data: Some(data),
            error: Some(summaries.join("; ")),
            progress: None,
            timestamp: now_epoch(),
        };
    }

    if tokens.is_empty() {
        return CliResponse::ok(
            "No tokens to drain - all mint balances are zero",
            Some(serde_json::json!({
                "success": true,
                "tokens": [],
                "total_sats": 0,
            })),
        );
    }

    CliResponse::ok(
        format!(
            "Successfully drained {total_drained} sats from {} mints",
            tokens.len()
        ),
        Some(data),
    )
}

async fn handle_config_get(state: &AppState, key: Option<&str>) -> CliResponse {
    match key {
        None => {
            let identities = crate::config::load_identities().ok().flatten().unwrap_or(
                crate::config::schema::IdentitiesConfig {
                    config_version: String::new(),
                    owned_identities: Vec::new(),
                    public_identities: Vec::new(),
                },
            );
            CliResponse::ok(
                "Configuration retrieved",
                Some(serde_json::json!({
                    "config": &*state.config,
                    "identities": identities,
                })),
            )
        }
        Some(k) => {
            let value = match k {
                "metric" => state.config.metric.clone(),
                "step_size" => state.config.step_size.to_string(),
                "config_version" => state.config.config_version.clone(),
                "log_level" => state.config.log_level.clone(),
                "show_setup" => state.config.show_setup.to_string(),
                "reseller_mode" => state.config.reseller_mode.to_string(),
                _ => {
                    return CliResponse::error(format!("unknown config key: {k}"));
                }
            };
            CliResponse::ok("", Some(serde_json::json!({ k: value })))
        }
    }
}

async fn handle_config_set(key: &str, value: &str) -> CliResponse {
    // Mirrors the plain-text handler's supported key set.
    let mut current = crate::config::load_config()
        .unwrap_or(None)
        .unwrap_or_default();
    match key {
        "metric" => {
            if value != "bytes" && value != "milliseconds" {
                return CliResponse::error(format!(
                    "metric must be 'bytes' or 'milliseconds', got '{value}'"
                ));
            }
            current.metric = value.to_string();
        }
        "step_size" => match value.parse::<u64>() {
            Ok(n) if n > 0 => current.step_size = n,
            _ => {
                return CliResponse::error(format!(
                    "step_size must be a positive integer, got '{value}'"
                ))
            }
        },
        _ => {
            return CliResponse::error(format!(
                "unsupported config key: {key} (supported: metric, step_size)"
            ))
        }
    }
    match crate::config::save_config(&current) {
        Ok(()) => CliResponse::ok(
            format!("{key} updated to {value} (restart required to take effect)"),
            None,
        ),
        Err(e) => CliResponse::error(format!("failed to save config: {e}")),
    }
}

async fn handle_config_schema() -> CliResponse {
    CliResponse::ok(
        "Configuration schema",
        Some(serde_json::json!({
            "config": crate::config::schema::config_schema(),
            "identities": crate::config::schema::identities_schema(),
        })),
    )
}

async fn handle_config_save(json_str: &str) -> CliResponse {
    let raw: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return CliResponse::error(format!("Invalid JSON: {e}")),
    };

    const REQUIRED: [&str; 5] = [
        "config_version",
        "metric",
        "step_size",
        "accepted_mints",
        "profit_share",
    ];
    let missing: Vec<&str> = REQUIRED
        .iter()
        .filter(|f| raw.get(**f).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        return CliResponse::error(format!("Missing required fields: {missing:?}"));
    }

    let config: crate::config::Config = match serde_json::from_value(raw) {
        Ok(c) => c,
        Err(e) => return CliResponse::error(format!("Invalid JSON: {e}")),
    };
    if let Err(e) = config.validate_profit_share() {
        return CliResponse::error(format!("Invalid profit_share: {e}"));
    }
    if let Err(e) = crate::config::save_config(&config) {
        return CliResponse::error(format!("Failed to save config: {e}"));
    }
    CliResponse::ok("Configuration saved (restart tollgate-wrt to apply)", None)
}

async fn handle_identities_save(json_str: &str) -> CliResponse {
    let identities: crate::config::schema::IdentitiesConfig = match serde_json::from_str(json_str) {
        Ok(i) => i,
        Err(e) => return CliResponse::error(format!("Invalid JSON: {e}")),
    };

    let path = crate::config::identities_path();
    let temp_path = path.with_extension("tmp");
    let data = match serde_json::to_vec_pretty(&identities) {
        Ok(d) => d,
        Err(e) => return CliResponse::error(format!("Failed to save identities: {e}")),
    };
    if let Err(e) = std::fs::write(&temp_path, &data) {
        return CliResponse::error(format!("Failed to save identities: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&temp_path, &path) {
        return CliResponse::error(format!("Failed to save identities: {e}"));
    }
    CliResponse::ok("Identities saved (restart tollgate-wrt to apply)", None)
}

// ── network private (uci) ────────────────────────────────────────────

async fn uci_get(key: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("uci")
        .args(["-q", "get", key])
        .output()
        .await
        .map_err(|e| format!("failed to get UCI value: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to get UCI value: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn uci_set(key: &str, value: &str) -> Result<(), String> {
    if key.contains(['\n', '\r', '\0']) || value.contains(['\n', '\r', '\0']) {
        return Err("invalid UCI value: contains control characters".to_string());
    }
    let output = tokio::process::Command::new("uci")
        .arg("set")
        .arg(format!("{key}={value}"))
        .output()
        .await
        .map_err(|e| format!("failed to set UCI value: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to set UCI value: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn uci_commit(config: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("uci")
        .args(["commit", config])
        .output()
        .await
        .map_err(|e| format!("failed to commit UCI changes: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to commit UCI changes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn reload_wireless() -> Result<(), String> {
    let output = tokio::process::Command::new("wifi")
        .arg("reload")
        .output()
        .await
        .map_err(|e| format!("failed to reload wireless: {e}"))?;
    if !output.status.success() {
        return Err("failed to reload wireless".to_string());
    }
    Ok(())
}

/// Human-readable random password: three NATO words + a number, like Go's
/// `generateRandomPassword`.
pub fn generate_random_password() -> String {
    use rand::seq::SliceRandom;
    const WORDS: [&str; 26] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra",
        "tango", "uniform", "victor", "whiskey", "xray", "yankee", "zulu",
    ];
    let mut rng = rand::thread_rng();
    let capitalize = |s: &str| -> String {
        let mut c = s.chars();
        c.next().map_or_else(String::new, |f| {
            f.to_uppercase().collect::<String>() + c.as_str()
        })
    };
    format!(
        "{}-{}-{}-{:02}",
        capitalize(WORDS.choose(&mut rng).unwrap_or(&"alpha")),
        capitalize(WORDS.choose(&mut rng).unwrap_or(&"bravo")),
        capitalize(WORDS.choose(&mut rng).unwrap_or(&"charlie")),
        rand::Rng::gen_range(&mut rng, 0..100)
    )
}

async fn handle_private_status() -> CliResponse {
    let ssid = match uci_get("wireless.private_radio0.ssid").await {
        Ok(s) => s,
        Err(e) => return CliResponse::error(format!("Failed to get private network SSID: {e}")),
    };
    let password = uci_get("wireless.private_radio0.key")
        .await
        .unwrap_or_else(|_| "(not set)".to_string());
    let disabled = uci_get("wireless.private_radio0.disabled")
        .await
        .unwrap_or_default();
    CliResponse::ok(
        "",
        Some(serde_json::json!({
            "ssid": ssid,
            "password": password,
            "enabled": disabled != "1",
        })),
    )
}

async fn apply_private_toggle(disabled: &str, enable: bool) -> CliResponse {
    let action = if enable { "enable" } else { "disable" };
    if let Err(e) = uci_set("wireless.private_radio0.disabled", disabled).await {
        return CliResponse::error(format!("Failed to {action} 2.4GHz private network: {e}"));
    }
    if let Err(e) = uci_set("wireless.private_radio1.disabled", disabled).await {
        tracing::warn!(error = %e, "failed to {action} 5GHz private network (may not exist)");
    }
    if let Err(e) = uci_commit("wireless").await {
        return CliResponse::error(format!("Failed to commit wireless changes: {e}"));
    }
    if let Err(e) = reload_wireless().await {
        return CliResponse::error(format!("Failed to reload wireless: {e}"));
    }
    CliResponse::ok(
        format!(
            "Private network {} successfully",
            if enable { "enabled" } else { "disabled" }
        ),
        None,
    )
}

async fn apply_private_rename(new_ssid: &str) -> CliResponse {
    if new_ssid.is_empty() {
        return CliResponse::error("SSID cannot be empty");
    }
    if let Err(e) = uci_set("wireless.private_radio0.ssid", new_ssid).await {
        return CliResponse::error(format!("Failed to rename 2.4GHz private network: {e}"));
    }
    if let Err(e) = uci_set("wireless.private_radio1.ssid", new_ssid).await {
        tracing::warn!(error = %e, "failed to rename 5GHz private network (may not exist)");
    }
    if let Err(e) = uci_commit("wireless").await {
        return CliResponse::error(format!("Failed to commit wireless changes: {e}"));
    }
    if let Err(e) = reload_wireless().await {
        return CliResponse::error(format!("Failed to reload wireless: {e}"));
    }
    CliResponse::ok(
        format!("Private network renamed to '{new_ssid}' successfully"),
        None,
    )
}

async fn apply_private_password(new_password: &str) -> CliResponse {
    let new_password = if new_password.is_empty() {
        generate_random_password()
    } else {
        new_password.to_string()
    };
    if new_password.len() < 8 || new_password.len() > 63 {
        return CliResponse::error("Password must be between 8 and 63 characters");
    }
    if let Err(e) = uci_set("wireless.private_radio0.key", &new_password).await {
        return CliResponse::error(format!(
            "Failed to change 2.4GHz private network password: {e}"
        ));
    }
    if let Err(e) = uci_set("wireless.private_radio1.key", &new_password).await {
        tracing::warn!(error = %e, "failed to change 5GHz private network password (may not exist)");
    }
    if let Err(e) = uci_commit("wireless").await {
        return CliResponse::error(format!("Failed to commit wireless changes: {e}"));
    }
    if let Err(e) = reload_wireless().await {
        return CliResponse::error(format!("Failed to reload wireless: {e}"));
    }
    CliResponse::ok(
        "Private network password changed successfully",
        Some(serde_json::json!({ "new_password": new_password })),
    )
}

// ── upstream ─────────────────────────────────────────────────────────

async fn handle_upstream_scan() -> CliResponse {
    let networks = tokio::task::spawn_blocking(crate::wireless::Scanner::scan_all)
        .await
        .unwrap_or_default();
    let result: Vec<serde_json::Value> = networks
        .iter()
        .map(|net| {
            serde_json::json!({
                "ssid": net.ssid,
                "signal": net.signal,
                "channel": "",
                "encryption": net.encryption,
                "bssid": net.bssid,
                "radio": net.radio,
                "band": "unknown",
                "is_tollgate": net.ssid.starts_with("TollGate"),
            })
        })
        .collect();
    CliResponse::ok(
        format!("Found {} network(s)", result.len()),
        Some(serde_json::Value::Array(result)),
    )
}

async fn handle_upstream_list() -> CliResponse {
    let sections = tokio::task::spawn_blocking(crate::wireless::Connector::get_sta_sections).await;
    let sections = match sections {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => return CliResponse::error(format!("Failed to list upstreams: {e}")),
        Err(e) => return CliResponse::error(format!("Failed to list upstreams: {e}")),
    };
    let result: Vec<serde_json::Value> = sections
        .iter()
        .map(|sta| {
            serde_json::json!({
                "ssid": sta.ssid,
                "status": if sta.disabled { "disabled" } else { "ACTIVE" },
                "radio": sta.device,
                "encryption": sta.encryption,
            })
        })
        .collect();
    CliResponse::ok(
        format!("{} upstream STA(s) configured", result.len()),
        Some(serde_json::Value::Array(result)),
    )
}

async fn handle_upstream_remove(ssid: &str) -> CliResponse {
    let sections = tokio::task::spawn_blocking(crate::wireless::Connector::get_sta_sections).await;
    let sections = match sections {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => return CliResponse::error(format!("Failed to remove upstream: {e}")),
        Err(e) => return CliResponse::error(format!("Failed to remove upstream: {e}")),
    };
    let section = sections.iter().find(|s| s.ssid == ssid);
    let Some(section) = section else {
        return CliResponse::error(format!(
            "Failed to remove upstream: no disabled upstream found with SSID '{ssid}'"
        ));
    };
    if !section.disabled {
        return CliResponse::error(format!(
            "Failed to remove upstream: cannot remove active upstream '{ssid}', switch first"
        ));
    }
    let name = section.name.clone();
    let delete = tokio::task::spawn_blocking(move || {
        crate::wireless::Connector::execute_uci(&["delete", &format!("wireless.{name}")]).and_then(
            |_| crate::wireless::Connector::execute_uci(&["commit", "wireless"]).map(|_| ()),
        )
    })
    .await;
    match delete {
        Ok(Ok(())) => CliResponse::ok(format!("Removed upstream '{ssid}'"), None),
        Ok(Err(e)) => CliResponse::error(format!("Failed to remove upstream: {e}")),
        Err(e) => CliResponse::error(format!("Failed to remove upstream: {e}")),
    }
}

// ── dispatch ─────────────────────────────────────────────────────────

/// Entry point from the socket server: parse a `CLIMessage` JSON line and
/// produce the `CLIResponse` JSON line for it.
pub async fn handle_json_line(line: &str, state: &AppState) -> String {
    let msg: CliMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => return response_line(&CliResponse::error(format!("Invalid JSON: {e}"))),
    };
    let resp = dispatch(&msg, state).await;
    response_line(&resp)
}

fn response_line(resp: &CliResponse) -> String {
    serde_json::to_string(resp).unwrap_or_else(|_| {
        "{\"success\":false,\"error\":\"failed to encode response\"}".to_string()
    }) + "\n"
}

pub(crate) async fn dispatch(msg: &CliMessage, state: &AppState) -> CliResponse {
    let args: Vec<&str> = msg.args.iter().map(String::as_str).collect();

    match (msg.command.as_str(), args.as_slice()) {
        ("version", []) => handle_version().await,
        ("status", []) => handle_status(state).await,
        ("health", []) => handle_health(state).await,

        ("wallet", []) => CliResponse::error(
            "Wallet command requires an action (drain, balance, info, fund)",
        ),
        ("wallet", ["balance"]) => handle_wallet_balance(state).await,
        ("wallet", ["info"]) => handle_wallet_info(state).await,
        ("wallet", ["fund"]) => {
            CliResponse::error("Fund command requires a cashu token argument")
        }
        ("wallet", ["fund", token]) => handle_wallet_fund(state, token).await,
        ("wallet", ["drain"]) => CliResponse::error(
            "Drain command requires a type: 'cashu' (lightning not yet supported)",
        ),
        ("wallet", ["drain", "cashu"]) => handle_wallet_drain_cashu(state).await,
        ("wallet", ["drain", "lightning"]) => {
            CliResponse::error("Lightning drain not yet implemented")
        }
        ("wallet", ["drain", other]) => CliResponse::error(format!(
            "Unknown drain type: {other} (supported: cashu)"
        )),
        ("wallet", [action, ..]) => CliResponse::error(format!(
            "Unknown wallet action: {action} (supported: drain, balance, info, fund)"
        )),

        ("network", []) => CliResponse::error("Network command requires a subcommand (private)"),
        ("network", ["private"]) => CliResponse::error(
            "Private network command requires an action (status, enable, disable, rename, password)",
        ),
        ("network", ["private", "status"]) => handle_private_status().await,
        ("network", ["private", "enable"]) => {
            apply_private_toggle("0", true).await
        }
        ("network", ["private", "disable"]) => {
            apply_private_toggle("1", false).await
        }
        ("network", ["private", "rename"]) => {
            CliResponse::error("Rename command requires a new SSID name")
        }
        ("network", ["private", "rename", name]) => apply_private_rename(name).await,
        ("network", ["private", "set-password"]) => apply_private_password("").await,
        ("network", ["private", "set-password", pw]) => apply_private_password(pw).await,
        ("network", ["private", action, ..]) => CliResponse::error(format!(
            "Unknown private network action: {action} (supported: status, enable, disable, rename, set-password)"
        )),
        ("network", [other, ..]) => CliResponse::error(format!(
            "Unknown network subcommand: {other} (supported: private)"
        )),

        ("upstream", []) => CliResponse::error(
            "Upstream command requires a subcommand (scan, connect, list, remove)",
        ),
        ("upstream", ["scan"]) => handle_upstream_scan().await,
        ("upstream", ["list"]) | ("upstream", ["list-upstream"]) => {
            handle_upstream_list().await
        }
        ("upstream", ["known"]) => CliResponse::ok(
            "0 TollGates discovered across 0 scans",
            Some(serde_json::json!([])),
        ),
        ("upstream", ["remove" | "remove-upstream"]) => {
            CliResponse::error("remove-upstream requires an SSID argument")
        }
        ("upstream", ["remove" | "remove-upstream", ssid]) => {
            handle_upstream_remove(ssid).await
        }
        ("upstream", ["connect"]) => {
            CliResponse::error("connect requires an SSID argument")
        }
        ("upstream", ["connect", ..]) => CliResponse::error(
            "upstream connect not supported on this build (no upstream connector wired into the CLI server)",
        ),
        ("upstream", [other, ..]) => CliResponse::error(format!(
            "Unknown upstream subcommand: {other} (supported: scan, connect, list-upstream, remove-upstream, known)"
        )),

        ("config", []) => CliResponse::error(
            "Config command requires a subcommand (get, set, schema, save, save-identities)",
        ),
        ("config", ["get"]) => handle_config_get(state, None).await,
        ("config", ["get", key]) => handle_config_get(state, Some(key)).await,
        ("config", ["set"]) | ("config", ["set", _]) => {
            CliResponse::error("config set requires <key> <value>")
        }
        ("config", ["set", key, value]) => handle_config_set(key, value).await,
        ("config", ["schema"]) => handle_config_schema().await,
        ("config", ["save"]) => CliResponse::error("config save requires <json-string>"),
        ("config", ["save", json]) => handle_config_save(json).await,
        ("config", ["save-identities"]) => {
            CliResponse::error("config save-identities requires <json-string>")
        }
        ("config", ["save-identities", json]) => handle_identities_save(json).await,
        ("config", [other, ..]) => CliResponse::error(format!(
            "Unknown config subcommand: {other} (supported: get, set, schema, save, save-identities)"
        )),

        _ => CliResponse::error(format!("Unknown command: {}", msg.command)),
    }
}

#[cfg(test)]
mod tests;
