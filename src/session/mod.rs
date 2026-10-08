//! CustomerSession and SessionManager — session tracking with disk persistence.
//!
//! Sessions are persisted to `sessions.json` in the config directory on every
//! mutation, matching tollgate-module-basic-go's behavior. On startup the
//! SessionManager loads existing sessions from disk so that sessions survive
//! process restarts.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// Wired here in the S13 NEW-lane audit: the file existed since #54 but
// was never declared, so none of its tests compiled or ran.
#[cfg(test)]
mod tests;

const SAVE_DEBOUNCE_MS: u64 = 5000;

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

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
    /// Durable idempotency key for the last quoted grant applied to this
    /// session (e.g. `ln:<quote-id>`). Lets settlement prove an allotment
    /// already reached `sessions.json` after a crash between the session
    /// flush and the quote-marker write — the two files cannot be written
    /// atomically together, so the key lives in the session record.
    /// Absent on non-Lightning sessions (identical bytes to before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_grant_id: Option<String>,
}

/// Session manager with disk persistence to `sessions.json`.
pub struct SessionManager {
    pub sessions: HashMap<String, CustomerSession>,
    dirty: AtomicBool,
    /// `Mutex<u64>` rather than `AtomicU64`: mips32 has no native 64-bit
    /// atomics, and epoch-millis overflows 32-bit `AtomicUsize`.
    last_save_ms: Mutex<u64>,
    /// MACs whose session was observed to expire (Go #541 parity: the
    /// bounded history that lets `/session-state` keep answering `expired`
    /// after the record is gone). Process-memory like the sessions — a
    /// restart forgets it and answers `none`, same as the session itself.
    expired_history: std::sync::Mutex<HashMap<String, u64>>,
}

/// Go parity (merchant.go): 24h history TTL, 4096-entry cap.
const EXPIRED_HISTORY_TTL_SECS: u64 = 24 * 60 * 60;
const EXPIRED_HISTORY_MAX_ENTRIES: usize = 4096;

/// Machine-readable session tri-state (Go #541, /session-state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    None,
    Active,
    Expired,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::None => "none",
            SessionState::Active => "active",
            SessionState::Expired => "expired",
        }
    }
}

impl SessionManager {
    /// Create a new empty SessionManager.
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
            dirty: AtomicBool::new(false),
            last_save_ms: Mutex::new(0),
            expired_history: std::sync::Mutex::new(HashMap::new()),
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
            last_grant_id: None,
        };
        self.sessions.insert(mac.to_string(), session.clone());
        session
    }

    /// Add allotment to an existing session, or create a new one if none
    /// exists. Returns `true` if an existing session was extended, `false`
    /// Add allotment to an existing session, or create a new one if none
    /// exists. Returns `true` if an existing session was extended, `false`
    /// if a new session was created. Go parity (`AddAllotment`): the
    /// existing `used` counter is preserved — only the allotment grows and
    /// `granted_at`/`expiry` refresh (the monitor re-syncs `used` from the
    /// gate's own counters). `grant_id`, when set, is the durable
    /// idempotency key checked by `has_grant`.
    pub fn add_allotment(
        &mut self,
        mac: &str,
        metric: &str,
        amount: u64,
        duration_secs: u64,
        grant_id: Option<&str>,
    ) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self.sessions.get_mut(mac) {
            Some(session) => {
                session.allotment += amount;
                session.granted_at = now;
                session.expiry = now + duration_secs;
                if let Some(id) = grant_id {
                    session.last_grant_id = Some(id.to_string());
                }
                true
            }
            None => {
                self.create_session(mac, amount, metric, duration_secs);
                if let Some(id) = grant_id {
                    if let Some(session) = self.sessions.get_mut(mac) {
                        session.last_grant_id = Some(id.to_string());
                    }
                }
                false
            }
        }
    }

    /// Whether the session for `mac` already carries this exact grant —
    /// i.e. the allotment is durable in `sessions.json` and must not be
    /// applied a second time.
    pub fn has_grant(&self, mac: &str, grant_id: &str) -> bool {
        self.sessions
            .get(mac)
            .and_then(|s| s.last_grant_id.as_deref())
            .is_some_and(|id| id == grant_id)
    }

    /// Grant a session unless this exact grant id is already applied and
    /// durable — the single-slot form of grant idempotency (Codex P1 on
    /// #58, round 2): after a crash between the reconciled grant and its
    /// terminal journal append, a restart recognizes the applied grant
    /// from `sessions.json` instead of re-creating the session (which
    /// would reset `used` and replenish expiry for one token, once per
    /// restart). Sufficient for one undecided payment per MAC; the
    /// multi-payment durable grant ledger is #63.
    pub fn apply_grant_once(
        &mut self,
        mac: &str,
        allotment: u64,
        metric: &str,
        duration_secs: u64,
        grant_id: &str,
    ) -> bool {
        if self.has_grant(mac, grant_id) {
            return false;
        }
        self.create_session(mac, allotment, metric, duration_secs);
        if let Some(session) = self.sessions.get_mut(mac) {
            session.last_grant_id = Some(grant_id.to_string());
        }
        true
    }

    /// Forget a grant id whose session save FAILED: in-memory state must
    /// not claim idempotency for a grant that is not durable — otherwise
    /// later passes skip re-granting while nothing reached disk.
    pub fn clear_grant(&mut self, mac: &str, grant_id: &str) {
        if let Some(session) = self.sessions.get_mut(mac) {
            if session.last_grant_id.as_deref() == Some(grant_id) {
                session.last_grant_id = None;
            }
        }
    }

    /// Restore a previously snapshotted session for `mac` (gate-open
    /// rollback), or remove the session when the snapshot says there was
    /// none.
    pub fn rollback_session(&mut self, mac: &str, snapshot: Option<CustomerSession>) {
        match snapshot {
            Some(prev) => {
                self.sessions.insert(mac.to_string(), prev);
            }
            None => {
                self.sessions.remove(mac);
            }
        }
    }

    /// Snapshot a session for a later [`rollback_session`].
    pub fn snapshot_session(&self, mac: &str) -> Option<CustomerSession> {
        self.sessions.get(mac).cloned()
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

    /// Record an observed expiry (Go rememberExpiredSessionLocked parity):
    /// TTL-sweeps old entries, caps the map, then records `mac` at `now`.
    fn remember_expired(&self, mac: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut hist = self
            .expired_history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let cutoff = now.saturating_sub(EXPIRED_HISTORY_TTL_SECS);
        hist.retain(|_, when| *when >= cutoff);
        while hist.len() >= EXPIRED_HISTORY_MAX_ENTRIES {
            // Evict the oldest entry (bounded map, Go parity).
            if let Some(oldest) = hist
                .iter()
                .min_by_key(|(_, when)| **when)
                .map(|(k, _)| k.clone())
            {
                hist.remove(&oldest);
            } else {
                break;
            }
        }
        hist.insert(mac.to_string(), now);
    }

    /// The machine-readable tri-state (Go #541): `none` (never had a
    /// session), `active` (session with allotment left), `expired` (had a
    /// session that is used up — remembered past the record's removal).
    pub fn session_state(&self, mac: &str) -> SessionState {
        if self.is_active(mac) {
            return SessionState::Active;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(EXPIRED_HISTORY_TTL_SECS);
        let hist = self
            .expired_history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Codex P2 on #54: the TTL sweep in remember_expired only runs when
        // another session expires — check the stored timestamp here too, or
        // a quiet router answers `expired` for a MAC that expired days ago.
        match hist.get(mac) {
            Some(when) if *when >= cutoff => SessionState::Expired,
            _ => SessionState::None,
        }
    }

    /// Remove a session by MAC. No-op if the MAC has no session.
    pub fn revoke_session(&mut self, mac: &str) {
        if self.sessions.remove(mac).is_some() {
            self.remember_expired(mac);
        }
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
            self.remember_expired(mac);
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
    /// Debounced: if called again within `SAVE_DEBOUNCE_MS` of the last
    /// successful write, marks dirty and returns `Ok(())` without writing.
    /// The monitor's periodic `flush_if_dirty` call ensures the final state
    /// is eventually persisted. This reduces flash write amplification on
    /// OpenWrt routers.
    ///
    /// Expired sessions are filtered out before writing. The write is atomic:
    /// data goes to a `.tmp` file first, then is renamed into place.
    pub fn save_to_disk(&self, dir: &Path) -> io::Result<()> {
        self.dirty.store(true, Ordering::Release);
        let now = epoch_ms();
        let last = self.last_save_epoch_ms();
        if now.saturating_sub(last) < SAVE_DEBOUNCE_MS {
            return Ok(());
        }
        self.do_save(dir)
    }

    /// Force an immediate durable write, bypassing the debounce. Used by
    /// payment-grant paths where the caller must not advance any marker
    /// before the session is durably recoverable (a debounced
    /// `save_to_disk` returns `Ok(())` without writing).
    pub fn save_now(&self, dir: &Path) -> io::Result<()> {
        self.do_save(dir)
    }

    /// Force a disk write if there are unsaved changes. Called by the
    /// monitor on each tick to ensure throttled saves eventually persist.
    pub fn flush_if_dirty(&self, dir: &Path) -> io::Result<()> {
        if self.dirty.load(Ordering::Acquire) {
            self.do_save(dir)
        } else {
            Ok(())
        }
    }

    fn last_save_epoch_ms(&self) -> u64 {
        // Poison can only mean a panic mid-save; the stale value is still
        // safe for debouncing.
        *self
            .last_save_ms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn do_save(&self, dir: &Path) -> io::Result<()> {
        use std::io::Write;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = dir.join("sessions.json");
        let data: Vec<&CustomerSession> =
            self.sessions.values().filter(|s| s.expiry > now).collect();
        let json = serde_json::to_string_pretty(&data)?;
        let tmp = dir.join("sessions.json.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        *self.last_save_ms.lock().unwrap_or_else(|p| p.into_inner()) = epoch_ms();
        self.dirty.store(false, Ordering::Release);
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
