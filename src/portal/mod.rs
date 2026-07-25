pub mod nds;

pub use nds::NdsPortal;

pub trait CaptivePortal: Send + Sync {
    fn grant_access(&self, mac: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
    fn revoke_access(&self, mac: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
    fn poll_usage(&self, mac: &str) -> impl std::future::Future<Output = Result<(u64, u64), String>> + Send;
    fn is_authenticated(&self, mac: &str) -> impl std::future::Future<Output = bool> + Send;
}
