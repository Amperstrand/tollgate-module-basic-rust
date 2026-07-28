use async_trait::async_trait;

use crate::error::AppError;

pub mod embedded;
pub mod nds;
pub mod redirect_server;

#[cfg(feature = "embedded-portal")]
pub mod nft_manager;

pub use nds::NdsPortal;

#[async_trait]
pub trait CaptivePortal: Send + Sync {
    async fn grant_access(&self, mac: &str) -> Result<(), AppError>;
    async fn revoke_access(&self, mac: &str) -> Result<(), AppError>;
    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), AppError>;
    async fn is_authenticated(&self, mac: &str) -> bool;
}
