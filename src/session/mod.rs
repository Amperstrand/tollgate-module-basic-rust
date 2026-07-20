//! CustomerSession and SessionManager — in-memory session tracking.
//!
//! Sessions are in-memory only, matching Go behavior. Process restart
//! loses all sessions. No persistence is attempted.

use std::collections::HashMap;

/// A single customer session keyed by MAC address.
#[derive(Debug, Clone)]
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

/// In-memory session manager. No persistence — matches Go behavior.
/// Process restart loses all sessions.
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
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;