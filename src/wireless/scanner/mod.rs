//! WiFi scanner — parses `iwinfo` scan output to discover networks.

use super::types::NetworkInfo;
use std::process::Command;

pub struct Scanner;

impl Scanner {
    pub fn new() -> Self {
        Scanner
    }

    /// Get list of WiFi radios from `/sys/class/ieee80211/`.
    pub fn get_radios() -> Vec<String> {
        let mut radios = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/class/ieee80211") {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    radios.push(name.to_string());
                }
            }
        }
        radios
    }

    /// Scan all radios for WiFi networks. Returns sorted by signal (strongest first).
    pub fn scan_all() -> Vec<NetworkInfo> {
        let radios = Self::get_radios();
        let mut all_networks = Vec::new();

        for radio in &radios {
            match Self::scan_radio(radio) {
                Ok(nets) => all_networks.extend(nets),
                Err(e) => {
                    tracing::warn!(radio = %radio, error = %e, "scan failed");
                }
            }
        }

        all_networks.sort_by(|a, b| b.signal.cmp(&a.signal));
        all_networks
    }

    /// Scan a single radio using `iwinfo <radio> scan`.
    pub fn scan_radio(radio: &str) -> Result<Vec<NetworkInfo>, String> {
        let output = Command::new("iwinfo")
            .args([radio, "scan"])
            .output()
            .map_err(|e| format!("execute iwinfo: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "iwinfo scan failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Self::parse_scan_output(&stdout, radio))
    }

    /// Parse `iwinfo` scan output into NetworkInfo structs.
    /// Each cell starts with "Cell XX - Address: AA:BB:CC:DD:EE:FF"
    pub fn parse_scan_output(output: &str, radio: &str) -> Vec<NetworkInfo> {
        let mut networks = Vec::new();
        let mut current_bssid: Option<String> = None;
        let mut current_ssid: Option<String> = None;
        let mut current_signal: Option<i32> = None;
        let mut current_encryption: Option<String> = None;

        for line in output.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("Cell") && trimmed.contains("Address:") {
                if let Some(bssid) = &current_bssid {
                    if let Some(ssid) = &current_ssid {
                        networks.push(NetworkInfo {
                            bssid: bssid.clone(),
                            ssid: ssid.clone(),
                            signal: current_signal.unwrap_or(-100),
                            encryption: current_encryption
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                            radio: radio.to_string(),
                        });
                    }
                }
                current_bssid = trimmed
                    .split("Address:")
                    .nth(1)
                    .map(|s| s.trim().to_string());
                current_ssid = None;
                current_signal = None;
                current_encryption = None;
            } else if trimmed.starts_with("ESSID:") {
                let raw = trimmed
                    .strip_prefix("ESSID:")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                if !raw.is_empty() {
                    current_ssid = Some(raw.to_string());
                }
            } else if trimmed.contains("Signal:") {
                if let Some(signal_part) = trimmed.split("Signal:").nth(1) {
                    let signal_str = signal_part.split_whitespace().next().unwrap_or("");
                    current_signal = signal_str
                        .trim_end_matches("dBm")
                        .trim()
                        .parse::<i32>()
                        .ok();
                }
            } else if trimmed.starts_with("Encryption:") {
                let enc = trimmed.strip_prefix("Encryption:").unwrap_or("").trim();
                if !enc.is_empty() && enc != "none" {
                    current_encryption = Some(enc.to_string());
                } else {
                    current_encryption = Some("none".to_string());
                }
            }
        }

        if let Some(bssid) = &current_bssid {
            if let Some(ssid) = &current_ssid {
                networks.push(NetworkInfo {
                    bssid: bssid.clone(),
                    ssid: ssid.clone(),
                    signal: current_signal.unwrap_or(-100),
                    encryption: current_encryption
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    radio: radio.to_string(),
                });
            }
        }

        networks
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
