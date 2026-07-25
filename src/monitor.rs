//! Background usage monitoring — polls active sessions every 2 seconds and
//! revokes access (`close_gate` + `revoke_session`) when the allotment is
//! exhausted.
//!
//! Mirrors Go `merchant.go:StartDataUsageMonitoring`. For "bytes" sessions
//! the actual bandwidth is queried via `ndsctl json <mac>`; for time-based
//! sessions the elapsed time since `granted_at` is used.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::session::SessionManager;

pub struct Monitor {
    sessions: Arc<Mutex<SessionManager>>,
    valve_mutex: Arc<Mutex<()>>,
    interval_secs: u64,
    ndsctl_bin: String,
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

fn parse_ndsctl_json_usage(stdout: &str) -> u64 {
    let v: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let downloaded_kb = v.get("downloaded").and_then(|n| n.as_u64()).unwrap_or(0);
    let uploaded_kb = v.get("uploaded").and_then(|n| n.as_u64()).unwrap_or(0);
    downloaded_kb.saturating_add(uploaded_kb) * 1024
}

impl Monitor {
    pub fn new(sessions: Arc<Mutex<SessionManager>>) -> Self {
        Monitor {
            sessions,
            valve_mutex: Arc::new(Mutex::new(())),
            interval_secs: 2,
            ndsctl_bin: std::env::var("NDSCTL_BIN").unwrap_or_else(|_| "ndsctl".to_string()),
        }
    }

    pub fn with_interval(mut self, secs: u64) -> Self {
        self.interval_secs = secs;
        self
    }

    #[cfg(test)]
    fn with_ndsctl_bin(mut self, bin: &str) -> Self {
        self.ndsctl_bin = bin.to_string();
        self
    }

    pub fn start(self) -> JoinHandle<()> {
        let interval = Duration::from_secs(self.interval_secs.max(1));
        let sessions = self.sessions;
        let valve_mutex = self.valve_mutex;
        let ndsctl_bin = self.ndsctl_bin;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                run_tick(&sessions, &valve_mutex, &ndsctl_bin).await;
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

async fn run_tick(
    sessions: &Arc<Mutex<SessionManager>>,
    valve_mutex: &Arc<Mutex<()>>,
    ndsctl_bin: &str,
) {
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
            let usage = query_ndsctl_usage(valve_mutex, ndsctl_bin, &snap.mac).await;
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
    }

    for mac in &to_revoke {
        tracing::warn!(mac = %mac, "allotment reached, closing gate");
        if let Err(e) = crate::valve::close_gate(mac).await {
            tracing::warn!(mac = %mac, error = %e, "failed to close gate");
        }
    }
}

async fn query_ndsctl_usage(valve_mutex: &Arc<Mutex<()>>, bin: &str, mac: &str) -> u64 {
    let _lock = valve_mutex.lock().await;

    let output = match Command::new(bin).args(["json", mac]).output().await {
        Ok(o) => o,
        Err(_) => {
            tracing::debug!(mac = %mac, "ndsctl not available, skipping bandwidth check");
            return 0;
        }
    };

    if !output.status.success() {
        tracing::debug!(mac = %mac, "ndsctl json returned non-zero, skipping");
        return 0;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ndsctl_json_usage(&stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write mock script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat script").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod script");
        }
        path
    }

    #[tokio::test]
    async fn test_monitor_creates_and_starts() {
        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        let monitor = Monitor::new(sessions).with_interval(1);
        let handle = monitor.start();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_finished(), "monitor task should be running");
        handle.abort();
    }

    #[tokio::test]
    async fn test_bytes_session_expires_on_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\n\
if [ \"$1\" = \"json\" ]; then\n\
  echo '{\"downloaded\": 1000000, \"uploaded\": 500000}'\n\
  exit 0\n\
fi\n\
exit 0\n";
        let bin = write_script(dir.path(), "ndsctl", script);

        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:02", 1024, "bytes", 3600);
        }

        let monitor = Monitor::new(sessions.clone())
            .with_interval(1)
            .with_ndsctl_bin(bin.to_str().unwrap());
        let handle = monitor.start();

        tokio::time::sleep(Duration::from_secs(3)).await;
        handle.abort();

        let mgr = sessions.lock().await;
        assert!(
            mgr.get_session("aa:bb:cc:dd:ee:02").is_none(),
            "bytes session should be revoked after usage exceeds allotment"
        );
    }

    #[tokio::test]
    async fn test_ms_session_expires_on_time() {
        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:03", 1, "milliseconds", 3600);
        }

        let monitor = Monitor::new(sessions.clone()).with_interval(1);
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
    async fn test_ndsctl_unavailable_doesnt_crash() {
        let sessions = Arc::new(Mutex::new(SessionManager::new()));
        {
            let mut mgr = sessions.lock().await;
            mgr.create_session("aa:bb:cc:dd:ee:01", 1_000_000, "bytes", 3600);
        }

        let monitor = Monitor::new(sessions.clone())
            .with_interval(1)
            .with_ndsctl_bin("/nonexistent/ndsctl-test-path");
        let handle = monitor.start();

        tokio::time::sleep(Duration::from_secs(3)).await;

        assert!(
            !handle.is_finished(),
            "monitor should still be running when ndsctl unavailable"
        );

        {
            let mgr = sessions.lock().await;
            assert!(
                mgr.get_session("aa:bb:cc:dd:ee:01").is_some(),
                "session should persist when ndsctl unavailable"
            );
        }

        handle.abort();
    }

    #[test]
    fn test_parse_ndsctl_json_usage() {
        let json = r#"{"downloaded": 1000, "uploaded": 500}"#;
        assert_eq!(parse_ndsctl_json_usage(json), (1000 + 500) * 1024);
    }

    #[test]
    fn test_parse_ndsctl_json_missing_fields() {
        assert_eq!(
            parse_ndsctl_json_usage(r#"{"downloaded": 1000}"#),
            1000 * 1024
        );
        assert_eq!(parse_ndsctl_json_usage(r#"{"uploaded": 500}"#), 500 * 1024);
        assert_eq!(parse_ndsctl_json_usage("{}"), 0);
    }

    #[test]
    fn test_parse_ndsctl_json_invalid() {
        assert_eq!(parse_ndsctl_json_usage(""), 0);
        assert_eq!(parse_ndsctl_json_usage("not json"), 0);
    }
}
