use super::CaptivePortal;
use crate::error::AppError;
use crate::valve;
use async_trait::async_trait;

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

#[async_trait]
impl CaptivePortal for NdsPortal {
    async fn grant_access(&self, mac: &str) -> Result<(), AppError> {
        valve::open_gate(mac).await.map_err(AppError::from)
    }

    async fn revoke_access(&self, mac: &str) -> Result<(), AppError> {
        valve::close_gate(mac).await.map_err(AppError::from)
    }

    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), AppError> {
        crate::metering::poll_usage(mac)
            .await
            .map_err(AppError::from)
    }

    async fn is_authenticated(&self, _mac: &str) -> bool {
        false
    }
}
