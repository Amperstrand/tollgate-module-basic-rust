use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::portal::CaptivePortal;
use crate::session::SessionManager;

pub struct Monitor {
    sessions: Arc<Mutex<SessionManager>>,
    portal: Arc<dyn CaptivePortal>,
    interval_secs: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_time_metric(metric: &str) -> bool {
    metric == "milliseconds" || metric == "time"
}

impl Monitor {
    pub fn new(sessions: Arc<Mutex<SessionManager>>, portal: Arc<dyn CaptivePortal>) -> Self {
        Monitor {
            sessions,
            portal,
            interval_secs: 2,
        }
    }

    pub fn with_interval(mut self, secs: u64) -> Self {
        self.interval_secs = secs;
        self
    }

    pub fn start(self) -> JoinHandle<()> {
        let interval = Duration::from_secs(self.interval_secs.max(1));
        let sessions = self.sessions;
        let portal = self.portal;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                run_tick(&sessions, &portal).await;
            }
        })
    }
}

struct SessionSnapshot {
    mac: String,
    metric: String,
    allotment: u64,
    granted_at: u64,
}

async fn run_tick(sessions: &Arc<Mutex<SessionManager>>, portal: &Arc<dyn CaptivePortal>) {
    let now = now_secs();

    let snapshots: Vec<SessionSnapshot> = {
        let mgr = sessions.lock().await;
        mgr.sessions
            .iter()
            .filter(|(_, s)| s.expiry > now)
            .map(|(mac, s)| SessionSnapshot {
                mac: mac.clone(),
                metric: s.metric.clone(),
                allotment: s.allotment,
                granted_at: s.granted_at,
            })
            .collect()
    };

    let mut updates: Vec<(String, u64)> = Vec::new();
    let mut to_revoke: Vec<String> = Vec::new();

    for snap in &snapshots {
        if snap.metric == "bytes" {
            let usage = match portal.poll_usage(&snap.mac).await {
                Ok((used, _total)) => used,
                Err(e) => {
                    tracing::debug!(
                        mac = %snap.mac,
                        error = %e,
                        "portal.poll_usage failed, treating as zero"
                    );
                    0
                }
            };
            updates.push((snap.mac.clone(), usage));
            if usage >= snap.allotment {
                to_revoke.push(snap.mac.clone());
            }
        } else if is_time_metric(&snap.metric) {
            let elapsed_ms = now.saturating_sub(snap.granted_at) * 1000;
            updates.push((snap.mac.clone(), elapsed_ms));
            if elapsed_ms >= snap.allotment {
                to_revoke.push(snap.mac.clone());
            }
        }
    }

    {
        let mut mgr = sessions.lock().await;
        for (mac, used) in &updates {
            mgr.update_usage(mac, *used);
        }
        for mac in &to_revoke {
            mgr.revoke_session(mac);
        }
        mgr.cleanup_expired();
        if !updates.is_empty() || !to_revoke.is_empty() {
            if let Err(e) = mgr.save_to_disk(&crate::config::config_dir()) {
                tracing::warn!(error = %e, "failed to persist session updates to disk");
            }
        }
    }

    for mac in &to_revoke {
        tracing::warn!(mac = %mac, "allotment reached, closing gate");
        if let Err(e) = portal.revoke_access(mac).await {
            tracing::warn!(mac = %mac, error = %e, "failed to close gate");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct MockPortal {
        usage_map: StdMutex<HashMap<String, u64>>,
        revoked: StdMutex<Vec<String>>,
        granted: StdMutex<Vec<String>>,
        fail_poll: StdMutex<bool>,
    }

    impl MockPortal {
        fn new() -> Self {
            MockPortal {
                usage_map: StdMutex::new(HashMap::new()),
                revoked: StdMutex::new(Vec::new()),
                granted: StdMutex::new(Vec::new()),
                fail_poll: StdMutex::new(false),
            }
        }

        fn set_usage(&self, mac: &str, bytes: u64) {
            self.usage_map
                .lock()
                .unwrap()
                .insert(mac.to_string(), bytes);
        }

        fn set_fail_poll(&self, fail: bool) {
            *self.fail_poll.lock().unwrap() = fail;
        }

        fn was_revoked(&self, mac: &str) -> bool {
            self.revoked.lock().unwrap().iter().any(|m| m == mac)
        }

        #[allow(dead_code)]
        fn was_granted(&self, mac: &str) -> bool {
            self.granted.lock().unwrap().iter().any(|m| m == mac)
        }
    }

    #[async_trait]
    impl CaptivePortal for MockPortal {
        async fn grant_access(&self, mac: &str) -> Result<(), String> {
            self.granted.lock().unwrap().push(mac.to_string());
            Ok(())
        }

        async fn revoke_access(&self, mac: &str) -> Result<(), String> {
            self.revoked.lock().unwrap().push(mac.to_string());
            Ok(())
        }

        async fn poll_usage(&self, mac: &str) -> Result<(u64, u64), String> {
            if *self.fail_poll.lock().unwrap() {
                return Err("mock poll failure".to_string());
            }
            let usage = self
                .usage_map
                .lock()
                .unwrap()
                .get(mac)
                .copied()
                .unwrap_or(0);
            Ok((usage, 0))
        }

        async fn is_authenticated(&self, _mac: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn test_monitor_creates_and_starts() {
        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        let portal: Arc<dyn CaptivePortal> = Arc::new(MockPortal::new());
        let monitor = Monitor::new(sessions, portal).with_interval(1);
        let handle = monitor.start();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_finished(), "monitor task should be running");
        handle.abort();
    }

    #[tokio::test]
    async fn test_bytes_session_expires_on_usage() {
        let mock = Arc::new(MockPortal::new());
        mock.set_usage("aa:bb:cc:dd:ee:02", 2_000_000);

        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:02", 1024, "bytes", 3600);
        }

        let portal: Arc<dyn CaptivePortal> = mock.clone();
        let monitor = Monitor::new(sessions.clone(), portal).with_interval(1);
        let handle = monitor.start();

        tokio::time::sleep(Duration::from_secs(3)).await;
        handle.abort();

        let mgr = sessions.lock().await;
        assert!(
            mgr.get_session("aa:bb:cc:dd:ee:02").is_none(),
            "bytes session should be revoked after usage exceeds allotment"
        );
        assert!(
            mock.was_revoked("aa:bb:cc:dd:ee:02"),
            "portal.revoke_access should be called for expired session"
        );
    }

    #[tokio::test]
    async fn test_ms_session_expires_on_time() {
        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        let portal: Arc<dyn CaptivePortal> = Arc::new(MockPortal::new());
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:03", 1, "milliseconds", 3600);
        }

        let monitor = Monitor::new(sessions.clone(), portal).with_interval(1);
        let handle = monitor.start();

        tokio::time::sleep(Duration::from_secs(3)).await;
        handle.abort();

        let mgr = sessions.lock().await;
        assert!(
            mgr.get_session("aa:bb:cc:dd:ee:03").is_none(),
            "time session should be revoked after allotment exceeded"
        );
    }

    #[tokio::test]
    async fn test_portal_poll_error_doesnt_crash() {
        let mock = Arc::new(MockPortal::new());
        mock.set_fail_poll(true);

        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:01", 1_000_000, "bytes", 3600);
        }

        let portal: Arc<dyn CaptivePortal> = mock.clone();
        let monitor = Monitor::new(sessions.clone(), portal).with_interval(1);
        let handle = monitor.start();

        tokio::time::sleep(Duration::from_secs(3)).await;

        assert!(
            !handle.is_finished(),
            "monitor should still be running when portal.poll_usage errors"
        );

        {
            let mgr = sessions.lock().await;
            assert!(
                mgr.get_session("aa:bb:cc:dd:ee:01").is_some(),
                "session should persist when portal.poll_usage errors"
            );
        }

        handle.abort();
    }
}
