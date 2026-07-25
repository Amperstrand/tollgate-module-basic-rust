//! WiFi connector — manages STA interfaces and connections via UCI commands.

use super::types::{Gateway, StaSection};
use std::process::Command;

pub struct Connector {
    pub dhcp_timeout_secs: u64,
}

impl Connector {
    pub fn new() -> Self {
        Connector {
            dhcp_timeout_secs: 180,
        }
    }

    /// Execute a UCI command and return stdout.
    /// Handles "Entry not found" gracefully for delete/get operations.
    pub fn execute_uci(args: &[&str]) -> Result<String, String> {
        let output = Command::new("uci")
            .args(args)
            .output()
            .map_err(|e| format!("execute uci: {e}"))?;

        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            if (args.first() == Some(&"delete") || args.first() == Some(&"get"))
                && stderr.contains("Entry not found")
            {
                if args.first() == Some(&"delete") {
                    return Ok(String::new());
                }
                return Err("uci: Entry not found".to_string());
            }
            return Err(format!("uci {} failed: {}", args.join(" "), stderr.trim()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Reload WiFi configuration.
    pub fn reload_wifi() -> Result<(), String> {
        Command::new("wifi")
            .arg("reload")
            .status()
            .map_err(|e| format!("wifi reload: {e}"))?
            .success()
            .then_some(())
            .ok_or_else(|| "wifi reload failed".to_string())
    }

    /// Connect to a gateway by configuring a STA interface.
    pub fn connect(&self, gateway: &Gateway, password: &str) -> Result<(), String> {
        let sta = self.find_available_sta_interface()?;

        self.disable_other_sta_interfaces(&sta)?;

        Self::execute_uci(&["set", "network.wwan=interface"])?;
        Self::execute_uci(&["set", "network.wwan.proto=dhcp"])?;
        Self::execute_uci(&["set", &format!("{sta}.network=wwan")])?;
        Self::execute_uci(&["set", &format!("{sta}.ssid={}", gateway.ssid)])?;

        if gateway.encryption != "none" && !gateway.encryption.is_empty() {
            let enc_type = uci_encryption_type(&gateway.encryption);
            Self::execute_uci(&["set", &format!("{sta}.encryption={enc_type}")])?;
            if !password.is_empty() {
                Self::execute_uci(&["set", &format!("{sta}.key={password}")])?;
            }
        } else {
            Self::execute_uci(&["set", &format!("{sta}.encryption=none")])?;
            let _ = Self::execute_uci(&["delete", &format!("{sta}.key")]);
        }

        Self::execute_uci(&["set", &format!("{sta}.disabled=0")])?;
        Self::execute_uci(&["commit", "wireless"])?;
        Self::execute_uci(&["commit", "network"])?;
        Self::reload_wifi()?;

        Ok(())
    }

    /// Disconnect by disabling all STA interfaces.
    pub fn disconnect() -> Result<(), String> {
        let stas = Self::get_sta_sections()?;
        for sta in &stas {
            Self::execute_uci(&["set", &format!("{}.disabled=1", sta.name)])?;
        }
        Self::execute_uci(&["commit", "wireless"])?;
        Self::reload_wifi()?;
        Ok(())
    }

    /// Get the currently connected SSID from `iw dev <iface> link`.
    pub fn get_connected_ssid(iface: &str) -> Result<String, String> {
        let output = Command::new("iw")
            .args(["dev", iface, "link"])
            .output()
            .map_err(|e| format!("iw dev link: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::parse_connected_ssid(&stdout)
    }

    /// Parse SSID from `iw dev <iface> link` output.
    pub fn parse_connected_ssid(output: &str) -> Result<String, String> {
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("SSID:") {
                let ssid = trimmed
                    .strip_prefix("SSID:")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                if !ssid.is_empty() {
                    return Ok(ssid.to_string());
                }
            }
        }
        Err("no SSID found in iw link output".to_string())
    }

    /// Parse signal strength (dBm) from `iw dev <iface> link` output.
    pub fn parse_signal_dbm(output: &str) -> Option<i32> {
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.contains("signal:") {
                let signal_part = trimmed.split("signal:").nth(1)?;
                let dbm_str = signal_part.split_whitespace().next()?;
                return dbm_str.trim_end_matches("dBm").parse().ok();
            }
        }
        None
    }

    /// Get current signal strength for an interface.
    pub fn get_signal(iface: &str) -> Option<i32> {
        let output = Command::new("iw")
            .args(["dev", iface, "link"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::parse_signal_dbm(&stdout)
    }

    fn find_available_sta_interface(&self) -> Result<String, String> {
        let sections = Self::get_sta_sections()?;
        for sta in &sections {
            if sta.disabled {
                return Ok(sta.name.clone());
            }
        }
        if let Some(sta) = sections.first() {
            return Ok(sta.name.clone());
        }
        Self::create_sta_interface()
    }

    fn disable_other_sta_interfaces(&self, active: &str) -> Result<(), String> {
        let sections = Self::get_sta_sections()?;
        for sta in &sections {
            if sta.name != active {
                Self::execute_uci(&["set", &format!("{}.disabled=1", sta.name)])?;
            }
        }
        Ok(())
    }

    fn create_sta_interface() -> Result<String, String> {
        let output = Self::execute_uci(&["add", "wireless", "wifi-iface"])?;
        let name = output.trim().to_string();
        if name.is_empty() {
            return Err("failed to create STA interface".to_string());
        }

        let suffix = format!("{:04x}", rand::random::<u16>());
        let network = format!("wgt{suffix}");

        Self::execute_uci(&["set", &format!("{name}.network={network}")])?;
        Self::execute_uci(&["set", &format!("{name}.mode=sta")])?;
        Self::execute_uci(&["set", &format!("{name}.disabled=1")])?;

        Ok(name)
    }

    /// Get all STA interface sections from UCI wireless config.
    pub fn get_sta_sections() -> Result<Vec<super::types::StaSection>, String> {
        let output = Self::execute_uci(&["show", "wireless"])?;
        Ok(Self::parse_sta_sections(&output))
    }

    /// Parse UCI wireless config to find STA sections.
    pub fn parse_sta_sections(uci_output: &str) -> Vec<super::types::StaSection> {
        let mut sections: Vec<super::types::StaSection> = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current = StaSection {
            name: String::new(),
            ssid: String::new(),
            device: String::new(),
            encryption: String::new(),
            disabled: false,
        };

        for line in uci_output.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("wireless.") && trimmed.contains("=wifi-iface") {
                if let Some(_name) = &current_name {
                    sections.push(current.clone());
                }
                let name = trimmed
                    .strip_prefix("wireless.")
                    .unwrap_or("")
                    .split('=')
                    .next()
                    .unwrap_or("")
                    .to_string();
                current = StaSection {
                    name: name.clone(),
                    ssid: String::new(),
                    device: String::new(),
                    encryption: String::new(),
                    disabled: false,
                };
                current_name = Some(name);
            } else if let Some(name) = &current_name {
                let prefix = format!("wireless.{name}.");
                if let Some(rest) = trimmed.strip_prefix(&prefix) {
                    if let Some((key, value)) = rest.split_once('=') {
                        match key {
                            "ssid" => current.ssid = value.trim_matches('\'').to_string(),
                            "device" => current.device = value.trim_matches('\'').to_string(),
                            "encryption" => {
                                current.encryption = value.trim_matches('\'').to_string();
                            }
                            "mode" => {
                                if value.trim_matches('\'') != "sta" {
                                    current_name = None;
                                }
                            }
                            "disabled" => current.disabled = value.trim_matches('\'') == "1",
                            _ => {}
                        }
                    }
                }
            }
        }

        if current_name.is_some() && (current.encryption.is_empty() || !current.ssid.is_empty()) {
            sections.push(current);
        }

        sections
    }
}

/// Map encryption string to UCI encryption type.
pub fn uci_encryption_type(encryption: &str) -> &'static str {
    let lower = encryption.to_lowercase();
    if lower.contains("wpa3") {
        "sae-mixed"
    } else if lower.contains("wpa2") {
        "psk2"
    } else if lower.contains("wpa") {
        "psk"
    } else {
        "none"
    }
}

impl Default for Connector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
