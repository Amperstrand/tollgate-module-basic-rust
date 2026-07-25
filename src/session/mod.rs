//! CustomerSession and SessionManager — session tracking with disk persistence.
//!
//! Sessions are persisted to `sessions.json` in the config directory on every
//! mutation, matching tollgate-module-basic-go's behavior. On startup the
//! SessionManager loads existing sessions from disk so that sessions survive
//! process restarts.

use std::collections::HashMap;
use std::io;
use std::path::Path;

/// A single customer session keyed by MAC address.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CustomerSession {
    /// Client MAC address — the primary key.
    pub mac: String,
    /// Total allotment in millisatoshis (time metric) or bytes (bytes metric).
    pub allotment: u64,
    /// How much has been consumed so far.
    pub used: u64,
    /// Metric type: "bytes" or "time".
    pub metric: String,
    /// Unix timestamp when the session expires.
    pub expiry: u64,
    /// Unix timestamp when the session was granted.
    pub granted_at: u64,
}

/// Session manager with disk persistence to `sessions.json`.
pub struct SessionManager {
    pub sessions: HashMap<String, CustomerSession>,
}

impl SessionManager {
    /// Create a new empty SessionManager.
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
        }
    }

    /// Create and store a new session for the given MAC.
    /// Overwrites any existing session for the same MAC.
    pub fn create_session(
        &mut self,
        mac: &str,
        allotment: u64,
        metric: &str,
        duration_secs: u64,
    ) -> CustomerSession {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let session = CustomerSession {
            mac: mac.to_string(),
            allotment,
            used: 0,
            metric: metric.to_string(),
            expiry: now + duration_secs,
            granted_at: now,
        };
        self.sessions.insert(mac.to_string(), session.clone());
        session
    }

    /// Add allotment to an existing session, or create a new one if none
    /// exists. Returns `true` if an existing session was extended, `false`
    /// if a new session was created. Extending resets `used` to 0 and
    /// refreshes `granted_at` / `expiry`.
    pub fn add_allotment(
        &mut self,
        mac: &str,
        metric: &str,
        amount: u64,
        duration_secs: u64,
    ) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self.sessions.get_mut(mac) {
            Some(session) => {
                session.allotment += amount;
                session.granted_at = now;
                session.used = 0;
                session.expiry = now + duration_secs;
                true
            }
            None => {
                self.create_session(mac, amount, metric, duration_secs);
                false
            }
        }
    }

    /// Look up a session by MAC address.
    pub fn get_session(&self, mac: &str) -> Option<&CustomerSession> {
        self.sessions.get(mac)
    }

    /// Check whether the session for `mac` is active (not expired, usage
    /// under allotment). Returns false if no session exists.
    pub fn is_active(&self, mac: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self.sessions.get(mac) {
            Some(s) => s.expiry > now && s.used < s.allotment,
            None => false,
        }
    }

    /// Remove a session by MAC. No-op if the MAC has no session.
    pub fn revoke_session(&mut self, mac: &str) {
        self.sessions.remove(mac);
    }

    /// Remove all expired sessions. Returns the number removed.
    pub fn cleanup_expired(&mut self) -> usize {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let expired_macs: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.expiry <= now)
            .map(|(mac, _)| mac.clone())
            .collect();
        let count = expired_macs.len();
        for mac in &expired_macs {
            self.sessions.remove(mac);
        }
        count
    }

    /// Update the `used` field for a session. No-op if session doesn't exist.
    pub fn update_usage(&mut self, mac: &str, used: u64) {
        if let Some(s) = self.sessions.get_mut(mac) {
            s.used = used;
        }
    }

    /// Save all active sessions to disk as JSON (`sessions.json`).
    ///
    /// Expired sessions are filtered out before writing so the file does not
    /// grow unbounded. The write is atomic: data goes to a `.tmp` file first,
    /// then is renamed into place.
    pub fn save_to_disk(&self, dir: &Path) -> io::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = dir.join("sessions.json");
        let data: Vec<&CustomerSession> =
            self.sessions.values().filter(|s| s.expiry > now).collect();
        let json = serde_json::to_string_pretty(&data)?;
        let tmp = dir.join("sessions.json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load sessions from disk. Returns an empty manager if the file does not
    /// exist or cannot be parsed (a warning is logged in the latter case).
    pub fn load_from_disk(dir: &Path) -> Self {
        let path = dir.join("sessions.json");
        match std::fs::read_to_string(&path) {
            Ok(json) => match serde_json::from_str::<Vec<CustomerSession>>(&json) {
                Ok(sessions) => {
                    let mut mgr = SessionManager::new();
                    for s in sessions {
                        mgr.sessions.insert(s.mac.clone(), s);
                    }
                    mgr
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse sessions.json, starting fresh");
                    SessionManager::new()
                }
            },
            Err(_) => SessionManager::new(),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
