//! Upstream WiFi manager — orchestrates scanning, connecting, monitoring, and switching.
//!
//! This is the "brain" that coordinates the Scanner (find networks), Connector
//! (UCI commands to connect), and UpstreamSession (payment + usage tracking).
//! It runs as a background tokio task with periodic checks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use super::connector::Connector;
use super::scanner::Scanner;
use super::types::{Gateway, NetworkInfo, UpstreamWifiConfig};
use crate::reseller::upstream_session::UpstreamSession;

#[derive(Debug, Clone, PartialEq)]
enum ManagerState {
    Idle,
    Scanning,
    Connecting(String),
    Connected,
    Monitoring,
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
    pub async fn tick(&mut self, wallet_token: Option<&str>) -> ManagerAction {
        self.cleanup_blacklist();

        match &self.state {
            ManagerState::Idle | ManagerState::Scanning => {
                if self.should_scan() {
                    self.do_scan_and_connect(wallet_token).await
                } else {
                    ManagerAction::NoAction
                }
            }

            ManagerState::Connected | ManagerState::Monitoring => {
                self.do_monitor(wallet_token).await
            }

            ManagerState::Connecting(_) => {
                ManagerAction::NoAction
            }

            ManagerState::Switching(_) => {
                if self.switch_cooldown_elapsed() {
                    self.do_scan_and_connect(wallet_token).await
                } else {
                    ManagerAction::NoAction
                }
            }

            ManagerState::ManualPause => {
                ManagerAction::NoAction
            }
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

    async fn do_scan_and_connect(&mut self, wallet_token: Option<&str>) -> ManagerAction {
        self.state = ManagerState::Scanning;
        tracing::info!("scanning for upstream gateways...");

        let networks = Scanner::scan_all();
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

        let connector = Connector::new();
        self.state = ManagerState::Connecting(gateway.ssid.clone());

        match connector.connect(&gateway, "") {
            Ok(()) => {
                tracing::info!(ssid = %gateway.ssid, signal = gateway.signal, "connected to gateway");

                let mut session = UpstreamSession::new("gateway", &gateway.radio);
                if let Some(token) = wallet_token {
                    let client = reqwest::Client::new();
                    let result = session.send_payment(token, &client).await;
                    if result.success {
                        session.apply_payment(&result);
                        tracing::info!(
                            allotment = session.allotment,
                            metric = %session.metric,
                            "payment successful, session active"
                        );
                    } else {
                        tracing::warn!(error = ?result.error, "payment failed");
                        self.blacklist_gateway(&gateway.bssid);
                        self.state = ManagerState::Idle;
                        return ManagerAction::PaymentFailed(result.error.unwrap_or_default());
                    }
                }

                self.current_session = Some(session);
                self.current_gateway = Some(gateway.clone());
                self.consecutive_failures = 0;
                self.last_switch = Some(Instant::now());
                self.state = ManagerState::Connected;
                ManagerAction::Connected(gateway)
            }
            Err(e) => {
                tracing::warn!(error = %e, ssid = %gateway.ssid, "connection failed");
                self.blacklist_gateway(&gateway.bssid);
                self.consecutive_failures += 1;
                self.state = ManagerState::Idle;
                ManagerAction::ConnectionFailed(e)
            }
        }
    }

    async fn do_monitor(&mut self, wallet_token: Option<&str>) -> ManagerAction {
        let gateway = match &self.current_gateway {
            Some(g) => g.clone(),
            None => {
                self.state = ManagerState::Idle;
                return ManagerAction::NoAction;
            }
        };

        let iface = self.sta_interface.as_deref().unwrap_or("wlan0");
        let signal = Connector::get_signal(iface);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if let Some(sig) = signal {
            if sig < self.config.signal_floor {
                tracing::warn!(signal = sig, floor = self.config.signal_floor, "signal below floor");
                self.blacklist_gateway(&gateway.bssid);
                self.state = ManagerState::Idle;
                self.current_gateway = None;
                self.current_session = None;
                return ManagerAction::SignalLost(gateway.bssid);
            }
        }

        if let Some(session) = &mut self.current_session {
            if session.is_expired() {
                tracing::info!("session expired, scanning for new gateway");
                self.state = ManagerState::Idle;
                self.current_gateway = None;
                self.current_session = None;
                return ManagerAction::SessionExpired;
            }

            if session.needs_renewal() {
                if let Some(token) = wallet_token {
                    let client = reqwest::Client::new();
                    let result = session.send_payment(token, &client).await;
                    if result.success {
                        session.apply_payment(&result);
                        tracing::info!("renewal payment successful");
                        return ManagerAction::Renewed(session.allotment);
                    } else {
                        tracing::warn!(error = ?result.error, "renewal payment failed");
                        return ManagerAction::PaymentFailed(result.error.unwrap_or_default());
                    }
                }
            }
        }

        self.state = ManagerState::Monitoring;
        ManagerAction::Monitoring {
            signal,
            remaining: self.current_session.as_ref().map(|s| s.remaining()).unwrap_or(0),
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
        self.blacklist.iter().any(|e| e.bssid == bssid && e.expires_at > now)
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
            remaining: self.current_session.as_ref().map(|s| s.remaining()).unwrap_or(0),
            consecutive_failures: self.consecutive_failures,
            blacklist_count: self.blacklist.len(),
        }
    }
}

#[derive(Debug, Clone)]
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
