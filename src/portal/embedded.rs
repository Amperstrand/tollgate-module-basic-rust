#[cfg(any(test, feature = "embedded-portal"))]
use std::net::IpAddr;

#[cfg(test)]
use crate::mac_resolver::resolve_ip_from_mac;

#[cfg(feature = "embedded-portal")]
use {async_trait::async_trait, std::collections::HashMap, std::sync::Mutex};

#[cfg(feature = "embedded-portal")]
use {super::nft_manager::NftManager, super::CaptivePortal, crate::error::AppError};

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

    pub fn install(&self) -> Result<(), crate::portal::nft_manager::NftError> {
        self.nft.install()
    }

    pub fn teardown(&self) -> Result<(), crate::portal::nft_manager::NftError> {
        self.nft.teardown()
    }
}

#[cfg(feature = "embedded-portal")]
impl Default for EmbeddedPortal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
fn resolve_or_err(mac: &str) -> Result<IpAddr, crate::error::AppError> {
    resolve_ip_from_mac(mac).ok_or_else(|| {
        crate::error::AppError::Internal(format!("no IP address found for MAC {mac}"))
    })
}

#[cfg(feature = "embedded-portal")]
#[async_trait]
impl CaptivePortal for EmbeddedPortal {
    async fn grant_access(&self, mac: &str) -> Result<(), AppError> {
        let mac_owned = mac.to_string();
        let nft = self.nft.clone();
        let handles =
            tokio::task::spawn_blocking(move || -> Result<Vec<(IpAddr, u32)>, AppError> {
                let ips = crate::mac_resolver::resolve_all_ips_from_mac(&mac_owned);
                if ips.is_empty() {
                    return Err(AppError::Internal(format!(
                        "no IP address found for MAC {mac_owned}"
                    )));
                }
                let mut installed = Vec::new();
                for ip in &ips {
                    if let Err(e) = nft.add_client(*ip) {
                        for (done_ip, done_h) in &installed {
                            let _ = nft.delete_rule(*done_h);
                            let _ = nft.remove_client(*done_ip);
                            let _ = nft.delete_counter(done_ip);
                        }
                        return Err(e.into());
                    }
                    if let Err(e) = nft.create_counter(*ip) {
                        let _ = nft.remove_client(*ip);
                        for (done_ip, done_h) in &installed {
                            let _ = nft.delete_rule(*done_h);
                            let _ = nft.remove_client(*done_ip);
                            let _ = nft.delete_counter(done_ip);
                        }
                        return Err(e.into());
                    }
                    match nft.add_counter_rule(*ip) {
                        Ok(h) => installed.push((*ip, h)),
                        Err(e) => {
                            let _ = nft.delete_counter(ip);
                            let _ = nft.remove_client(*ip);
                            for (done_ip, done_h) in &installed {
                                let _ = nft.delete_rule(*done_h);
                                let _ = nft.remove_client(*done_ip);
                                let _ = nft.delete_counter(done_ip);
                            }
                            return Err(e.into());
                        }
                    }
                }
                Ok(installed)
            })
            .await
            .map_err(|e| AppError::Internal(e.to_string()))??;

        let mut map = self.rule_handles.lock().unwrap_or_else(|e| e.into_inner());
        for (ip, handle) in handles {
            map.insert(ip, handle);
        }
        Ok(())
    }

    async fn revoke_access(&self, mac: &str) -> Result<(), AppError> {
        let mac_owned = mac.to_string();
        let nft = self.nft.clone();
        let handles_snapshot: std::collections::HashMap<IpAddr, u32> = {
            let map = self.rule_handles.lock().unwrap_or_else(|e| e.into_inner());
            map.clone()
        };

        let cleaned_ips: Vec<IpAddr> =
            tokio::task::spawn_blocking(move || -> Result<Vec<IpAddr>, AppError> {
                let ips = crate::mac_resolver::resolve_all_ips_from_mac(&mac_owned);
                let mut cleaned = Vec::new();
                for ip in &ips {
                    if let Some(h) = handles_snapshot.get(ip) {
                        let _ = nft.delete_rule(*h);
                    }
                    let _ = nft.remove_client(*ip);
                    let _ = nft.delete_counter(ip);
                    cleaned.push(*ip);
                }
                Ok(cleaned)
            })
            .await
            .map_err(|e| AppError::Internal(e.to_string()))??;

        let mut map = self.rule_handles.lock().unwrap_or_else(|e| e.into_inner());
        for ip in &cleaned_ips {
            map.remove(ip);
        }
        Ok(())
    }

    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), AppError> {
        let mac_owned = mac.to_string();
        let nft = self.nft.clone();
        tokio::task::spawn_blocking(move || -> Result<(u64, u64), AppError> {
            let ips = crate::mac_resolver::resolve_all_ips_from_mac(&mac_owned);
            if ips.is_empty() {
                return Err(AppError::Internal(format!(
                    "no IP address found for MAC {mac_owned}"
                )));
            }
            let mut total_bytes = 0u64;
            for ip in &ips {
                if let Ok((_, bytes)) = nft.poll_counter(ip) {
                    total_bytes += bytes;
                }
            }
            Ok((total_bytes, 0))
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
    }

    async fn is_authenticated(&self, mac: &str) -> bool {
        let mac_owned = mac.to_string();
        let known_ips: Vec<IpAddr> = {
            let map = self.rule_handles.lock().unwrap_or_else(|e| e.into_inner());
            map.keys().cloned().collect()
        };
        tokio::task::spawn_blocking(move || {
            let ips = crate::mac_resolver::resolve_all_ips_from_mac(&mac_owned);
            ips.iter().any(|ip| known_ips.contains(ip))
        })
        .await
        .unwrap_or(false)
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no IP address found"));
    }

    #[cfg(feature = "embedded-portal")]
    #[test]
    fn embedded_portal_has_empty_handle_map_on_init() {
        let portal = EmbeddedPortal::new();
        assert!(portal.rule_handles.lock().unwrap().is_empty());
    }
}
