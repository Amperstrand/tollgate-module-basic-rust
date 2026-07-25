use super::CaptivePortal;
use crate::valve;

pub struct NdsPortal;

impl Default for NdsPortal {
    fn default() -> Self {
        Self::new()
    }
}

impl NdsPortal {
    pub fn new() -> Self {
        NdsPortal
    }
}

impl CaptivePortal for NdsPortal {
    async fn grant_access(&self, mac: &str) -> Result<(), String> {
        valve::open_gate(mac).await
    }

    async fn revoke_access(&self, mac: &str) -> Result<(), String> {
        valve::close_gate(mac).await
    }

    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), String> {
        crate::metering::poll_usage(mac)
            .await
            .map_err(|e| e.to_string())
    }

    async fn is_authenticated(&self, _mac: &str) -> bool {
        false
    }
}
