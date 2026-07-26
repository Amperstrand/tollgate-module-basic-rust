use async_trait::async_trait;

pub mod nds;

pub use nds::NdsPortal;

#[async_trait]
pub trait CaptivePortal: Send + Sync {
    async fn grant_access(&self, mac: &str) -> Result<(), String>;
    async fn revoke_access(&self, mac: &str) -> Result<(), String>;
    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), String>;
    async fn is_authenticated(&self, mac: &str) -> bool;
}
