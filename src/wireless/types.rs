//! Data types for the wireless gateway manager.

use serde::{Deserialize, Serialize};

/// A discovered WiFi network from a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub bssid: String,
    pub ssid: String,
    pub signal: i32,
    pub encryption: String,
    pub radio: String,
}

/// A gateway with pricing info — a TollGate access point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gateway {
    pub bssid: String,
    pub ssid: String,
    pub signal: i32,
    pub encryption: String,
    pub radio: String,
}

impl From<NetworkInfo> for Gateway {
    fn from(net: NetworkInfo) -> Self {
        Gateway {
            bssid: net.bssid,
            ssid: net.ssid,
            signal: net.signal,
            encryption: net.encryption,
            radio: net.radio,
        }
    }
}

/// A UCI STA (Station) interface section.
#[derive(Debug, Clone)]
pub struct StaSection {
    pub name: String,
    pub ssid: String,
    pub device: String,
    pub encryption: String,
    pub disabled: bool,
}

/// Configuration for the upstream WiFi manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamWifiConfig {
    pub scan_interval_seconds: u64,
    pub fast_check_seconds: u64,
    pub lost_threshold: u32,
    pub hysteresis_db: i32,
    pub signal_floor: i32,
    pub blacklist_ttl_minutes: u64,
    pub emergency_penalty: i32,
    pub max_consecutive_failures: u32,
    pub switch_cooldown_minutes: u64,
    pub startup_grace_seconds: u64,
    pub post_switch_wait_seconds: u64,
    pub dhcp_timeout_seconds: u64,
    pub manual_pause_seconds: u64,
}

impl Default for UpstreamWifiConfig {
    fn default() -> Self {
        UpstreamWifiConfig {
            scan_interval_seconds: 300,
            fast_check_seconds: 30,
            lost_threshold: 2,
            hysteresis_db: 12,
            signal_floor: -85,
            blacklist_ttl_minutes: 60,
            emergency_penalty: 20,
            max_consecutive_failures: 3,
            switch_cooldown_minutes: 10,
            startup_grace_seconds: 90,
            post_switch_wait_seconds: 5,
            dhcp_timeout_seconds: 180,
            manual_pause_seconds: 120,
        }
    }
}
