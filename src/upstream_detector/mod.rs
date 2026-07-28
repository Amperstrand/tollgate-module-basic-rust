//! Upstream TollGate detector — discovers upstream TollGates on WAN interfaces.
//!
//! When a router is behind another TollGate (reseller mode), this module
//! detects the upstream gateway, probes it to confirm it's a TollGate,
//! and reports it to the upstream session manager.

pub mod gateway_prober;
pub mod types;

use crate::config::schema::UpstreamDetectorConfig;
use gateway_prober::GatewayProber;

pub struct UpstreamDetector {
    config: UpstreamDetectorConfig,
    prober: GatewayProber,
    running: bool,
}

impl UpstreamDetector {
    pub fn new(config: UpstreamDetectorConfig) -> Self {
        UpstreamDetector {
            config,
            prober: GatewayProber::new(),
            running: false,
        }
    }

    pub fn start(&mut self) {
        self.running = true;
        tracing::info!("upstream detector started");
    }

    pub fn stop(&mut self) {
        self.running = false;
        tracing::info!("upstream detector stopped");
    }

    /// Scan WAN interfaces for upstream TollGates.
    /// Reads /proc/net/route to find the default gateway, then probes it.
    pub async fn scan(&self) -> Vec<DiscoveredGateway> {
        let gateways = match read_default_gateways() {
            Ok(gws) => gws,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read default gateways");
                return Vec::new();
            }
        };

        let mut discovered = Vec::new();
        for gw in &gateways {
            if self.should_skip_interface(&gw.interface) {
                continue;
            }

            match self.prober.probe(&gw.ip).await {
                Ok(info) => {
                    tracing::info!(
                        gateway = %gw.ip,
                        interface = %gw.interface,
                        metric = %info.metric,
                        "TollGate discovered"
                    );
                    discovered.push(DiscoveredGateway {
                        ip: gw.ip.clone(),
                        interface: gw.interface.clone(),
                        mac: gw.mac.clone(),
                        metric: info.metric,
                        step_size: info.step_size,
                        price_per_step: info.price_per_step,
                        mint_url: info.mint_url,
                    });
                }
                Err(e) => {
                    tracing::debug!(
                        gateway = %gw.ip,
                        error = %e,
                        "gateway is not a TollGate"
                    );
                }
            }
        }

        discovered
    }

    fn should_skip_interface(&self, iface: &str) -> bool {
        if self.config.only_interfaces.iter().any(|i| i == iface) {
            return false;
        }
        if !self.config.only_interfaces.is_empty() {
            return true;
        }
        self.config.ignore_interfaces.iter().any(|i| i == iface)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredGateway {
    pub ip: String,
    pub interface: String,
    pub mac: String,
    pub metric: String,
    pub step_size: u64,
    pub price_per_step: u64,
    pub mint_url: String,
}

struct RawGateway {
    ip: String,
    interface: String,
    mac: String,
}

/// Read default gateway(s) from /proc/net/route + /proc/net/arp.
fn read_default_gateways() -> Result<Vec<RawGateway>, crate::error::DetectorError> {
    let route_data = std::fs::read_to_string("/proc/net/route")
        .map_err(|e| crate::error::DetectorError::RouteRead(e.to_string()))?;

    let mut gateways = Vec::new();
    for line in route_data.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }

        let interface = fields[0];
        let dest = fields[1];
        let gateway_hex = fields[2];
        let flags = fields[3];

        // Destination 00000000 = default route
        if dest != "00000000" {
            continue;
        }

        // Flag 0003 = RTF_UP | RTF_GATEWAY
        if flags != "0003" {
            continue;
        }

        let ip = hex_to_ip(gateway_hex).unwrap_or_else(|| format!("unparseable:{gateway_hex}"));

        let mac = lookup_mac(&ip).unwrap_or_default();

        gateways.push(RawGateway {
            ip,
            interface: interface.to_string(),
            mac,
        });
    }

    Ok(gateways)
}

/// Convert a hex IP (little-endian, from /proc/net/route) to dotted decimal.
fn hex_to_ip(hex: &str) -> Option<String> {
    let val = u32::from_str_radix(hex, 16).ok()?;
    let a = (val & 0xFF) as u8;
    let b = ((val >> 8) & 0xFF) as u8;
    let c = ((val >> 16) & 0xFF) as u8;
    let d = ((val >> 24) & 0xFF) as u8;
    Some(format!("{a}.{b}.{c}.{d}"))
}

/// Look up MAC address for an IP from /proc/net/arp.
fn lookup_mac(ip: &str) -> Option<String> {
    let arp = std::fs::read_to_string("/proc/net/arp").ok()?;
    for line in arp.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 4 && fields[0] == ip {
            let mac = fields[3];
            if mac != "00:00:00:00:00:00" {
                return Some(mac.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
