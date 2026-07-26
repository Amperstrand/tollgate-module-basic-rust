use super::CaptivePortal;
use async_trait::async_trait;
use std::net::IpAddr;

use crate::mac_resolver::resolve_ip_from_mac;

#[cfg(feature = "embedded-portal")]
use super::nft_manager::NftManager;

#[cfg(feature = "embedded-portal")]
pub struct EmbeddedPortal {
    nft: NftManager,
}

#[cfg(feature = "embedded-portal")]
impl EmbeddedPortal {
    pub fn new() -> Self {
        EmbeddedPortal {
            nft: NftManager::new(),
        }
    }

    pub fn with_nft(nft: NftManager) -> Self {
        EmbeddedPortal { nft }
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

#[cfg(feature = "embedded-portal")]
#[async_trait]
impl CaptivePortal for EmbeddedPortal {
    async fn grant_access(&self, mac: &str) -> Result<(), String> {
        let ip = resolve_or_err(mac)?;
        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || {
            nft.add_client(ip).map_err(|e| e.to_string())?;
            nft.create_counter(ip).map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn revoke_access(&self, mac: &str) -> Result<(), String> {
        let ip = resolve_or_err(mac)?;
        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || {
            nft.remove_client(ip).map_err(|e| e.to_string())?;
            nft.delete_counter(&ip).map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), String> {
        let ip = resolve_or_err(mac)?;
        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || nft.poll_counter(&ip).map_err(|e| e.to_string()))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn is_authenticated(&self, mac: &str) -> bool {
        resolve_ip_from_mac(mac).is_some()
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
}
