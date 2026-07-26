use super::CaptivePortal;
use async_trait::async_trait;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use crate::mac_resolver::{resolve_all_ips_from_mac, resolve_ip_from_mac};

#[cfg(feature = "embedded-portal")]
use super::nft_manager::NftManager;

#[cfg(feature = "embedded-portal")]
pub struct EmbeddedPortal {
    nft: NftManager,
    rule_handles: Mutex<HashMap<IpAddr, u32>>,
}

#[cfg(feature = "embedded-portal")]
impl EmbeddedPortal {
    pub fn new() -> Self {
        EmbeddedPortal {
            nft: NftManager::new(),
            rule_handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_nft(nft: NftManager) -> Self {
        EmbeddedPortal {
            nft,
            rule_handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn install(&self) -> Result<(), String> {
        self.nft.install().map_err(|e| e.to_string())
    }

    pub fn teardown(&self) -> Result<(), String> {
        self.nft.teardown().map_err(|e| e.to_string())
    }
}

#[cfg(feature = "embedded-portal")]
impl Default for EmbeddedPortal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(feature = "embedded-portal"), allow(dead_code))]
fn resolve_or_err(mac: &str) -> Result<IpAddr, String> {
    resolve_ip_from_mac(mac).ok_or_else(|| format!("no IP address found for MAC {mac}"))
}

#[cfg_attr(not(feature = "embedded-portal"), allow(dead_code))]
fn resolve_all_or_err(mac: &str) -> Result<Vec<IpAddr>, String> {
    let ips = resolve_all_ips_from_mac(mac);
    if ips.is_empty() {
        Err(format!("no IP address found for MAC {mac}"))
    } else {
        Ok(ips)
    }
}

#[cfg(feature = "embedded-portal")]
#[async_trait]
impl CaptivePortal for EmbeddedPortal {
    async fn grant_access(&self, mac: &str) -> Result<(), String> {
        let ips = resolve_all_or_err(mac)?;
        let nft = self.nft.clone();
        let handles = tokio::task::spawn_blocking(move || -> Result<Vec<(IpAddr, u32)>, String> {
            let mut handles = Vec::new();
            for ip in &ips {
                nft.add_client(*ip).map_err(|e| e.to_string())?;
                nft.create_counter(*ip).map_err(|e| e.to_string())?;
                match nft.add_counter_rule(*ip) {
                    Ok(h) => handles.push((*ip, h)),
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(handles)
        })
        .await
        .map_err(|e| e.to_string())??;

        let mut map = self.rule_handles.lock().unwrap();
        for (ip, handle) in handles {
            map.insert(ip, handle);
        }
        Ok(())
    }

    async fn revoke_access(&self, mac: &str) -> Result<(), String> {
        let ips = resolve_all_or_err(mac)?;
        let handles: Vec<(IpAddr, Option<u32>)> = {
            let mut map = self.rule_handles.lock().unwrap();
            ips.iter().map(|ip| (*ip, map.remove(ip))).collect()
        };

        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            for (ip, handle) in &handles {
                if let Some(h) = handle {
                    let _ = nft.delete_rule(*h);
                }
                let _ = nft.remove_client(*ip);
                let _ = nft.delete_counter(ip);
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), String> {
        let ips = resolve_all_or_err(mac)?;
        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || -> Result<(u64, u64), String> {
            let mut total_packets = 0u64;
            let mut total_bytes = 0u64;
            for ip in &ips {
                if let Ok((p, b)) = nft.poll_counter(ip) {
                    total_packets += p;
                    total_bytes += b;
                }
            }
            Ok((total_bytes, 0))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn is_authenticated(&self, mac: &str) -> bool {
        let ips = resolve_all_ips_from_mac(mac);
        if ips.is_empty() {
            return false;
        }
        let map = self.rule_handles.lock().unwrap();
        ips.iter().any(|ip| map.contains_key(ip))
    }
}

#[cfg(feature = "embedded-portal")]
impl Drop for EmbeddedPortal {
    fn drop(&mut self) {
        if let Err(e) = self.nft.teardown() {
            eprintln!("warning: nftables teardown failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_or_err_returns_error_for_missing_mac() {
        let result = resolve_or_err("aa:bb:cc:dd:ee:ff");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no IP address found"));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn embedded_portal_has_empty_handle_map_on_init() {
        let portal = EmbeddedPortal::new();
        assert!(portal.rule_handles.lock().unwrap().is_empty());
    }
}
