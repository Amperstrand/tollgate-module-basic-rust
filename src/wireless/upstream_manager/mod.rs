//! Upstream WiFi manager — orchestrates scanning, connecting, monitoring, and switching.
//!
//! This is the "brain" that coordinates the Scanner (find networks), Connector
//! (UCI commands to connect), and UpstreamSession (payment + usage tracking).
//! It runs as a background tokio task with periodic checks.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::connector::Connector;
use super::scanner::Scanner;
use super::types::{Gateway, NetworkInfo, UpstreamWifiConfig};
use crate::reseller::upstream_session::{UpstreamPaymentResult, UpstreamSession};
use crate::upstream_detector::gateway_prober::GatewayProber;
use crate::wallet::TollWallet;

/// Best-effort L3 gateway of a station interface (BusyBox `ip route`).
fn sta_gateway_ip(sta_interface: Option<&str>) -> Option<String> {
    let iface = resolve_l3_device(sta_interface?)?;
    let out = std::process::Command::new("ip")
        .args(["route", "show", "dev", &iface])
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix("default via ") {
            let gw = rest.split_whitespace().next()?.to_string();
            return Some(gw);
        }
    }
    None
}

/// A UCI wifi-iface section name (e.g. `wgt0a1b`) is NOT an L3 network
/// device — `ip route show dev` needs the netifd interface's resolved
/// `l3_device` (e.g. `wwan0`). Query ubus for it, falling back to the
/// section name for simple setups where they coincide (Codex P1 on #52).
fn resolve_l3_device(uci_section: &str) -> Option<String> {
    let out = std::process::Command::new("ubus")
        .args(["call", "network.interface.wwan", "status"])
        .output()
        .ok()?;
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let l3 = status.get("l3_device")?.as_str()?.to_string();
    if l3.is_empty() { None } else { Some(l3) }.or_else(|| {
        tracing::warn!(
            uci_section,
            "wwan l3_device unresolved; trying section name"
        );
        Some(uci_section.to_string())
    })
}

#[derive(Debug, Clone, PartialEq)]
enum ManagerState {
    Idle,
    Scanning,
    Connecting(String),
    Connected,
    Monitoring,
    #[allow(dead_code)]
    Switching(String),
    ManualPause,
}

struct BlacklistEntry {
    bssid: String,
    expires_at: Instant,
}

pub struct UpstreamManager {
    config: UpstreamWifiConfig,
    state: ManagerState,
    current_session: Option<UpstreamSession>,
    current_gateway: Option<Gateway>,
    blacklist: Vec<BlacklistEntry>,
    consecutive_failures: u32,
    last_switch: Option<Instant>,
    sta_interface: Option<String>,
}

impl UpstreamManager {
    pub fn new(config: UpstreamWifiConfig) -> Self {
        UpstreamManager {
            config,
            state: ManagerState::Idle,
            current_session: None,
            current_gateway: None,
            blacklist: Vec::new(),
            consecutive_failures: 0,
            last_switch: None,
            sta_interface: None,
        }
    }

    /// Run one tick of the management loop. Returns the action taken.
    ///
    /// `wallet` supplies the outbound Cashu value: when a payment is due the
    /// manager sizes the purchase from the gateway's advertised pricing and
    /// creates a fresh token from the wallet (Go reseller semantics — the
    /// token is generated at payment time, never pre-minted).
    pub async fn tick(&mut self, wallet: Option<&TollWallet>) -> ManagerAction {
        self.cleanup_blacklist();

        match &self.state {
            ManagerState::Idle | ManagerState::Scanning => {
                if self.should_scan() {
                    self.do_scan_and_connect(wallet).await
                } else {
                    ManagerAction::NoAction
                }
            }

            ManagerState::Connected | ManagerState::Monitoring => self.do_monitor(wallet).await,

            ManagerState::Connecting(_) => ManagerAction::NoAction,

            ManagerState::Switching(_) => {
                if self.switch_cooldown_elapsed() {
                    self.do_scan_and_connect(wallet).await
                } else {
                    ManagerAction::NoAction
                }
            }

            ManagerState::ManualPause => ManagerAction::NoAction,
        }
    }

    /// Pause the manager manually (e.g., user-initiated).
    pub fn pause(&mut self) {
        self.state = ManagerState::ManualPause;
        tracing::info!("upstream manager paused");
    }

    /// Resume from manual pause.
    pub fn resume(&mut self) {
        if self.state == ManagerState::ManualPause {
            self.state = ManagerState::Idle;
            tracing::info!("upstream manager resumed");
        }
    }

    /// Force an immediate scan.
    pub fn force_scan(&mut self) {
        self.state = ManagerState::Scanning;
    }

    fn should_scan(&self) -> bool {
        if self.consecutive_failures >= self.config.max_consecutive_failures {
            return false;
        }
        true
    }

    fn switch_cooldown_elapsed(&self) -> bool {
        if let Some(last) = self.last_switch {
            last.elapsed() >= Duration::from_secs(self.config.switch_cooldown_minutes * 60)
        } else {
            true
        }
    }

    /// #38 durable-record contract for the reseller purchase path
    /// (mint_payment_token → wallet.send → send_payment → apply_payment).
    ///
    /// Crash-window table:
    /// | death point | durable journal | consequence |
    /// |---|---|---|
    /// | before send | intent | nothing moved |
    /// | after send, before delivery | `token-created` + TOKEN STRING | value recoverable — re-deliver or operator redeems; NEVER re-mint |
    /// | during delivery | `token-created` (or delivery-ambiguous) | re-deliver SAME token (bearer value consumed once upstream) |
    /// | after delivery, before apply | `delivered` | session lost but paid-once; next cycle is a NEW purchase |
    ///
    /// Duplicate attempts consult the journal first: an undelivered token
    /// for this identity blocks minting fresh value.
    async fn journalled_purchase(
        wallet: &TollWallet,
        mint_url: &str,
        amount_sat: u64,
        session: &mut UpstreamSession,
    ) -> Option<UpstreamPaymentResult> {
        use crate::payout_journal as pj;

        let cfg_dir = crate::config::config_dir();
        const IDENTITY: &str = "reseller-upstream";

        // Duplicate-attempt gate (#38): never mint fresh value while an
        // earlier purchase's token is undelivered — surface instead.
        let undelivered = pj::undelivered_tokens(&cfg_dir, IDENTITY);
        if !undelivered.is_empty() {
            tracing::error!(
                count = undelivered.len(),
                total_sat = undelivered.iter().map(|e| e.amount_sat).sum::<u64>(),
                "reseller purchase BLOCKED: undelivered payment token(s) from an earlier attempt exist in {} — recover them (re-deliver or redeem) before purchasing again",
                pj::journal_path(&cfg_dir).display()
            );
            return None;
        }

        let op = format!(
            "upstream-{mint_url}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        let intent = pj::PayoutEntry {
            id: pj::entry_id(mint_url, IDENTITY, &op),
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            token: None,
            mint: String::new(),
            identity: IDENTITY.to_string(),
            invoice: op,
            amount_sat,
            literal_invoice: false,
            phase: pj::PayoutPhase::Intent,
        };
        if let Err(e) = pj::append_entry(&cfg_dir, &intent) {
            tracing::error!(error = %e, "CRITICAL: payout journal unavailable — refusing to mint payment value with no recovery record");
            return None;
        }

        match wallet.send(mint_url, amount_sat, false).await {
            Ok(token) => {
                // THE fund-safety record: the bearer token is durable
                // BEFORE any delivery attempt — a crash here loses nothing
                // (Codex P1 on #52: if this append FAILS, delivery must be
                // refused — proceeding would create withdrawable value
                // with no recoverable token record).
                if let Err(e) = pj::append_entry(
                    &cfg_dir,
                    &pj::PayoutEntry {
                        token: Some(token.clone()),
                        phase: pj::PayoutPhase::TokenCreated,
                        ..intent.clone()
                    },
                ) {
                    tracing::error!(error = %e, "CRITICAL: send succeeded but the token could not be made durable — refusing delivery; the wallet's send saga is the recovery record; do not re-purchase until reconciled");
                    return None;
                }
                let client = reqwest::Client::new();
                let result = session.send_payment(&token, &client).await;
                // Every post-send phase carries the token (Codex P1 on
                // #52): last-write-wins must never hide the recovery
                // surface from undelivered_tokens.
                let phase = if result.success {
                    pj::PayoutPhase::Delivered
                } else {
                    let ambiguous = result
                        .error
                        .as_deref()
                        .map(|e| e.contains("request failed"))
                        .unwrap_or(false);
                    if ambiguous {
                        pj::PayoutPhase::DeliveryAmbiguous
                    } else {
                        pj::PayoutPhase::DeliveryFailed {
                            reason: result.error.clone().unwrap_or_default(),
                        }
                    }
                };
                let _ = pj::append_entry(
                    &cfg_dir,
                    &pj::PayoutEntry {
                        token: Some(token),
                        phase,
                        ..intent.clone()
                    },
                );
                Some(result)
            }
            Err(e) => {
                // Codex P2 on #54 / P1 on #55: only genuinely ambiguous
                // outcomes may enter the blocking Ambiguous phase. A
                // definitive local failure moved no value and holds no
                // token to recover — journaling it Ambiguous would wedge
                // undelivered_tokens (and thus every future purchase)
                // forever. Definitive set: the mint's wallet is not
                // registered (fails before CDK, no saga exists) and
                // CDK's InsufficientFunds (escapes the purely local
                // proof-selection in send's prepare — nothing was signed
                // or submitted). Everything else (timeouts, transport,
                // saga-recovery refusals) stays Ambiguous: the token may
                // exist undelivered.
                let definitive = matches!(&e, crate::error::WalletError::WalletNotFound(_))
                    || matches!(
                        &e,
                        crate::error::WalletError::Cdk(cdk::Error::InsufficientFunds)
                    );
                let phase = if definitive {
                    tracing::warn!(
                        error = %e,
                        "upstream wallet.send failed definitively (no value moved) — journaled retryable-failed, repurchase unblocked"
                    );
                    pj::PayoutPhase::Failed {
                        reason: e.to_string(),
                    }
                } else {
                    // Ambiguous (the token may exist undelivered): journal
                    // it — but we have no token string, so it surfaces via
                    // the saga + the warning below rather than as a
                    // recoverable token.
                    tracing::error!(
                        error = %e,
                        "upstream wallet.send errored — outcome ambiguous; the send saga reconciles at next recovery; no token string was captured"
                    );
                    pj::PayoutPhase::Ambiguous
                };
                let _ = pj::append_entry(
                    &cfg_dir,
                    &pj::PayoutEntry {
                        phase,
                        ..intent.clone()
                    },
                );
                None
            }
        }
    }

    /// Price an upstream purchase from the gateway's live advertisement
    /// (kind 10021); renewal sizes the new purchase like the session it
    /// replaces (Go uses a configured preferred increment — approximation
    /// documented in PARITY.md). Returns (mint_url, amount_sat), or None
    /// when pricing is impossible. The wallet.send + delivery live in
    /// journalled_purchase (#38).
    async fn mint_payment_token(
        &self,
        wallet: Option<&TollWallet>,
        renew_sizing: Option<(u64, u64)>,
    ) -> Option<(String, u64)> {
        let wallet = wallet?;

        // No guessed gateway: pricing and paying whatever answers at an
        // invented address is a wrong-party payment risk. If the STA
        // route lookup fails, skip this purchase attempt entirely.
        let gateway_ip = match sta_gateway_ip(self.sta_interface.as_deref()) {
            Some(ip) => ip,
            None => {
                tracing::warn!(
                    sta_interface = ?self.sta_interface,
                    "cannot price upstream purchase: no default route on STA interface — skipping"
                );
                return None;
            }
        };

        let prober = GatewayProber::new();
        let info = match prober.probe(&gateway_ip).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, gateway_ip = %gateway_ip, "cannot price upstream purchase: advertisement probe failed");
                return None;
            }
        };

        let steps = match renew_sizing {
            Some((step_size, allotment)) if step_size > 0 => (allotment / step_size).max(1),
            _ => 1,
        };
        let amount_sat = steps.saturating_mul(info.price_per_step.max(1));

        // The send itself lives in journalled_purchase (#38 durable
        // contract); this fn only prices.
        let _ = wallet;
        tracing::debug!(
            mint = %info.mint_url,
            amount_sat,
            steps,
            "priced upstream purchase"
        );
        Some((info.mint_url, amount_sat))
    }

    async fn do_scan_and_connect(&mut self, wallet: Option<&TollWallet>) -> ManagerAction {
        self.state = ManagerState::Scanning;
        tracing::info!("scanning for upstream gateways...");

        let networks = tokio::task::spawn_blocking(Scanner::scan_all)
            .await
            .unwrap_or_default();
        if networks.is_empty() {
            self.consecutive_failures += 1;
            tracing::warn!(
                failures = self.consecutive_failures,
                "no networks found during scan"
            );
            self.state = ManagerState::Idle;
            return ManagerAction::ScanFailed("no networks".to_string());
        }

        let gateway = self.select_best_gateway(&networks);
        let gateway = match gateway {
            Some(g) => g,
            None => {
                self.state = ManagerState::Idle;
                return ManagerAction::ScanFailed("no suitable gateway".to_string());
            }
        };

        self.state = ManagerState::Connecting(gateway.ssid.clone());

        let gateway_for_connect = gateway.clone();
        let connect_outcome = tokio::task::spawn_blocking(move || {
            let connector = Connector::new();
            connector.connect(&gateway_for_connect, "")
        })
        .await;

        match connect_outcome {
            Ok(Ok(sta_iface)) => {
                tracing::info!(ssid = %gateway.ssid, signal = gateway.signal, "connected to gateway");

                // Codex P1 on #52: capture the STA interface the connector
                // actually used — pricing route lookups depend on it and it
                // was previously never assigned (None → every purchase
                // silently skipped).
                self.sta_interface = Some(sta_iface);

                // Codex P1 on #52: the gateway IP must come from the STA
                // route, not the literal host "gateway" (no DNS on normal
                // OpenWrt networks — delivery would always fail).
                let gateway_ip = sta_gateway_ip(self.sta_interface.as_deref());
                let mut session = match gateway_ip {
                    Some(ip) => UpstreamSession::new(&ip, &gateway.radio),
                    None => {
                        // No route yet (DHCP still settling) — treat like a
                        // failed probe: retry next tick, never pay a guess.
                        tracing::warn!(
                            "no default route on STA after connect — deferring purchase"
                        );
                        self.current_session = Some(UpstreamSession::new("", &gateway.radio));
                        self.current_gateway = Some(gateway.clone());
                        self.consecutive_failures = 0;
                        self.state = ManagerState::Connected;
                        return ManagerAction::NoAction;
                    }
                };
                if let Some(wallet_ref) = wallet {
                    // #38: price, then purchase + deliver under the durable
                    // contract (intent → token-created(token durable) →
                    // delivered/failed/ambiguous; duplicates consult the
                    // journal and never re-mint over undelivered value).
                    if let Some((mint_url, amount_sat)) =
                        self.mint_payment_token(wallet, None).await
                    {
                        match Self::journalled_purchase(
                            wallet_ref,
                            &mint_url,
                            amount_sat,
                            &mut session,
                        )
                        .await
                        {
                            Some(result) if result.success => {
                                session.apply_payment(&result);
                                tracing::info!(
                                    allotment = session.allotment,
                                    metric = %session.metric,
                                    "payment successful, session active"
                                );
                            }
                            Some(result) => {
                                tracing::warn!(error = ?result.error, "payment failed");
                                self.blacklist_gateway(&gateway.bssid);
                                self.state = ManagerState::Idle;
                                return ManagerAction::PaymentFailed(
                                    result.error.unwrap_or_default(),
                                );
                            }
                            None => {
                                // Undelivered earlier token or journal/send
                                // failure — surfaced by journalled_purchase.
                                self.state = ManagerState::Idle;
                                return ManagerAction::PaymentFailed(
                                    "purchase blocked or errored (see logs)".to_string(),
                                );
                            }
                        }
                    }
                }

                self.current_session = Some(session);
                self.current_gateway = Some(gateway.clone());
                self.consecutive_failures = 0;
                self.last_switch = Some(Instant::now());
                self.state = ManagerState::Connected;
                ManagerAction::Connected(gateway)
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                tracing::warn!(error = %msg, ssid = %gateway.ssid, "connection failed");
                self.blacklist_gateway(&gateway.bssid);
                self.consecutive_failures += 1;
                self.state = ManagerState::Idle;
                ManagerAction::ConnectionFailed(msg)
            }
            Err(e) => {
                let msg = format!("spawn_blocking panicked: {e}");
                tracing::warn!(error = %msg, ssid = %gateway.ssid, "connection task failed");
                self.blacklist_gateway(&gateway.bssid);
                self.consecutive_failures += 1;
                self.state = ManagerState::Idle;
                ManagerAction::ConnectionFailed(msg)
            }
        }
    }

    async fn do_monitor(&mut self, wallet: Option<&TollWallet>) -> ManagerAction {
        let gateway = match &self.current_gateway {
            Some(g) => g.clone(),
            None => {
                self.state = ManagerState::Idle;
                return ManagerAction::NoAction;
            }
        };

        let iface = self.sta_interface.as_deref().unwrap_or("wlan0").to_string();
        let signal = tokio::task::spawn_blocking(move || Connector::get_signal(&iface))
            .await
            .unwrap_or(None);
        let _now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if let Some(sig) = signal {
            if sig < self.config.signal_floor {
                tracing::warn!(
                    signal = sig,
                    floor = self.config.signal_floor,
                    "signal below floor"
                );
                self.blacklist_gateway(&gateway.bssid);
                self.state = ManagerState::Idle;
                self.current_gateway = None;
                self.current_session = None;
                return ManagerAction::SignalLost(gateway.bssid);
            }
        }

        if self
            .current_session
            .as_ref()
            .is_some_and(|s| s.is_expired())
        {
            tracing::info!("session expired, scanning for new gateway");
            self.state = ManagerState::Idle;
            self.current_gateway = None;
            self.current_session = None;
            return ManagerAction::SessionExpired;
        }

        let renewal = self
            .current_session
            .as_ref()
            .filter(|s| s.needs_renewal())
            .map(|s| (s.step_size, s.allotment));
        if let Some(sizing) = renewal {
            if let Some((mint_url, amount_sat)) =
                self.mint_payment_token(wallet, Some(sizing)).await
            {
                let result = {
                    let wallet_ref = match wallet {
                        Some(w) => w,
                        None => return ManagerAction::NoAction,
                    };
                    let session = match self.current_session.as_mut() {
                        Some(s) => s,
                        None => return ManagerAction::NoAction,
                    };
                    match Self::journalled_purchase(wallet_ref, &mint_url, amount_sat, session)
                        .await
                    {
                        Some(r) => r,
                        None => return ManagerAction::NoAction,
                    }
                };
                if result.success {
                    if let Some(session) = self.current_session.as_mut() {
                        session.apply_payment(&result);
                    }
                    tracing::info!("renewal payment successful");
                    let allotment = self
                        .current_session
                        .as_ref()
                        .map(|s| s.allotment)
                        .unwrap_or(0);
                    return ManagerAction::Renewed(allotment);
                } else {
                    tracing::warn!(error = ?result.error, "renewal payment failed");
                    return ManagerAction::PaymentFailed(result.error.unwrap_or_default());
                }
            }
        }

        self.state = ManagerState::Monitoring;
        ManagerAction::Monitoring {
            signal,
            remaining: self
                .current_session
                .as_ref()
                .map(|s| s.remaining())
                .unwrap_or(0),
        }
    }

    fn select_best_gateway(&self, networks: &[NetworkInfo]) -> Option<Gateway> {
        networks
            .iter()
            .filter(|n| !self.is_blacklisted(&n.bssid))
            .filter(|n| n.signal >= self.config.signal_floor)
            .max_by_key(|n| n.signal)
            .map(|n| Gateway::from(n.clone()))
    }

    fn is_blacklisted(&self, bssid: &str) -> bool {
        let now = Instant::now();
        self.blacklist
            .iter()
            .any(|e| e.bssid == bssid && e.expires_at > now)
    }

    fn blacklist_gateway(&mut self, bssid: &str) {
        let ttl = Duration::from_secs(self.config.blacklist_ttl_minutes * 60);
        let penalty = if self.consecutive_failures >= self.config.max_consecutive_failures {
            ttl + Duration::from_secs(self.config.emergency_penalty as u64 * 60)
        } else {
            ttl
        };

        tracing::info!(bssid = %bssid, ttl_secs = penalty.as_secs(), "blacklisting gateway");
        self.blacklist.push(BlacklistEntry {
            bssid: bssid.to_string(),
            expires_at: Instant::now() + penalty,
        });
    }

    fn cleanup_blacklist(&mut self) {
        let now = Instant::now();
        self.blacklist.retain(|e| e.expires_at > now);
    }

    pub fn get_status(&self) -> ManagerStatus {
        ManagerStatus {
            state: format!("{:?}", self.state),
            connected_ssid: self.current_gateway.as_ref().map(|g| g.ssid.clone()),
            connected_signal: self.current_gateway.as_ref().map(|g| g.signal),
            remaining: self
                .current_session
                .as_ref()
                .map(|s| s.remaining())
                .unwrap_or(0),
            consecutive_failures: self.consecutive_failures,
            blacklist_count: self.blacklist.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ManagerAction {
    NoAction,
    Connected(Gateway),
    ScanFailed(String),
    ConnectionFailed(String),
    PaymentFailed(String),
    SignalLost(String),
    SessionExpired,
    Renewed(u64),
    Monitoring { signal: Option<i32>, remaining: u64 },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagerStatus {
    pub state: String,
    pub connected_ssid: Option<String>,
    pub connected_signal: Option<i32>,
    pub remaining: u64,
    pub consecutive_failures: u32,
    pub blacklist_count: usize,
}

#[cfg(test)]
mod tests;
