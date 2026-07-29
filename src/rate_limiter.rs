use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const WINDOW: Duration = Duration::from_secs(60);

pub struct RateLimiter {
    requests: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    max_per_minute: usize,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize) -> Self {
        Self {
            requests: Mutex::new(HashMap::new()),
            max_per_minute: max_per_minute.max(1),
        }
    }

    pub fn from_env() -> Self {
        let rpm = std::env::var("TOLLGATE_RATE_LIMIT_RPM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        Self::new(rpm)
    }

    pub async fn allow(&self, ip: IpAddr) -> bool {
        let mut map = self.requests.lock().await;
        let now = Instant::now();
        let timestamps = map.entry(ip).or_default();
        timestamps.retain(|t| now.duration_since(*t) < WINDOW);
        if timestamps.len() >= self.max_per_minute {
            return false;
        }
        timestamps.push(now);
        true
    }
}
