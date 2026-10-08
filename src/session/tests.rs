//! Tests for CustomerSession and SessionManager.

use super::*;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[test]
fn create_session_stores_and_returns_clone() {
    let mut mgr = SessionManager::new();
    let s = mgr.create_session("aa:bb:cc:dd:ee:ff", 1_000_000, "bytes", 3600);
    assert_eq!(s.mac, "aa:bb:cc:dd:ee:ff");
    assert_eq!(s.allotment, 1_000_000);
    assert_eq!(s.used, 0);
    assert_eq!(s.metric, "bytes");
    assert_eq!(s.expiry, s.granted_at + 3600);
    // Stored in map
    let got = mgr
        .get_session("aa:bb:cc:dd:ee:ff")
        .expect("session exists");
    assert_eq!(got.allotment, 1_000_000);
}

#[test]
fn get_session_returns_none_for_unknown_mac() {
    let mgr = SessionManager::new();
    assert!(mgr.get_session("00:00:00:00:00:00").is_none());
}

#[test]
fn is_active_true_for_fresh_session() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1_000_000, "bytes", 3600);
    assert!(mgr.is_active("aa:bb:cc:dd:ee:ff"));
}

#[test]
fn is_active_false_when_expired() {
    let mut mgr = SessionManager::new();
    // Duration of 0 means it expires immediately
    let s = mgr.create_session("aa:bb:cc:dd:ee:ff", 1_000_000, "bytes", 0);
    // Force expiry into the past
    {
        let stored = mgr.sessions.get_mut("aa:bb:cc:dd:ee:ff").unwrap();
        stored.expiry = now() - 1;
    }
    let _ = s;
    assert!(!mgr.is_active("aa:bb:cc:dd:ee:ff"));
}

#[test]
fn is_active_false_when_usage_exceeds_allotment() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    {
        let stored = mgr.sessions.get_mut("aa:bb:cc:dd:ee:ff").unwrap();
        stored.used = 1001;
    }
    assert!(!mgr.is_active("aa:bb:cc:dd:ee:ff"));
}

#[test]
fn revoke_session_removes_from_map() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    assert!(mgr.get_session("aa:bb:cc:dd:ee:ff").is_some());
    mgr.revoke_session("aa:bb:cc:dd:ee:ff");
    assert!(mgr.get_session("aa:bb:cc:dd:ee:ff").is_none());
}

#[test]
fn revoke_unknown_mac_is_noop() {
    let mut mgr = SessionManager::new();
    mgr.revoke_session("00:00:00:00:00:00"); // should not panic
}

#[test]
fn cleanup_expired_removes_only_expired() {
    let mut mgr = SessionManager::new();
    // Session 1: expires now (duration 0)
    mgr.create_session("aa:bb:cc:dd:ee:f0", 1000, "bytes", 0);
    // Force expiry to past
    {
        let s = mgr.sessions.get_mut("aa:bb:cc:dd:ee:f0").unwrap();
        s.expiry = now() - 10;
    }
    // Session 2: valid for 1 hour
    mgr.create_session("aa:bb:cc:dd:ee:f1", 1000, "bytes", 3600);

    let removed = mgr.cleanup_expired();
    assert_eq!(removed, 1);
    assert!(mgr.get_session("aa:bb:cc:dd:ee:f0").is_none());
    assert!(mgr.get_session("aa:bb:cc:dd:ee:f1").is_some());
}

#[test]
fn create_session_overwrites_existing() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    mgr.create_session("aa:bb:cc:dd:ee:ff", 5000, "time", 7200);
    let s = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(s.allotment, 5000);
    assert_eq!(s.metric, "time");
}

#[test]
fn test_add_allotment_creates_new_session() {
    let mut mgr = SessionManager::new();
    let extended = mgr.add_allotment("aa:bb:cc:dd:ee:ff", "bytes", 1000, 3600, None);
    assert!(!extended, "should return false for newly created session");
    let s = mgr
        .get_session("aa:bb:cc:dd:ee:ff")
        .expect("session exists");
    assert_eq!(s.allotment, 1000);
    assert_eq!(s.metric, "bytes");
    assert_eq!(s.used, 0);
}

#[test]
fn test_add_allotment_extends_existing_session() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    let extended = mgr.add_allotment("aa:bb:cc:dd:ee:ff", "bytes", 500, 3600, None);
    assert!(extended, "should return true for extended session");
    let s = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(s.allotment, 1500, "allotment should be 1000 + 500");
}

#[test]
fn test_add_allotment_preserves_used() {
    // Go AddAllotment parity: the consumed counter is NOT reset on
    // extension — resetting it would refund already-consumed bytes for
    // free on the bytes metric.
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    mgr.update_usage("aa:bb:cc:dd:ee:ff", 500);
    let extended = mgr.add_allotment("aa:bb:cc:dd:ee:ff", "bytes", 500, 3600, None);
    assert!(extended);
    let s = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(
        s.used, 500,
        "used must be preserved on extension (Go parity)"
    );
    assert_eq!(s.allotment, 1500);
}

#[test]
fn test_rollback_session_restores_snapshot() {
    let mut mgr = SessionManager::new();
    let original = mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    let snapshot = mgr.snapshot_session("aa:bb:cc:dd:ee:ff");
    mgr.add_allotment("aa:bb:cc:dd:ee:ff", "bytes", 500, 3600, None);
    mgr.rollback_session("aa:bb:cc:dd:ee:ff", snapshot);
    let s = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(
        s.allotment, original.allotment,
        "rollback restores allotment"
    );

    let snapshot2 = mgr.snapshot_session("aa:bb:cc:dd:ee:ff");
    mgr.rollback_session("aa:bb:cc:dd:ee:ff", None);
    assert!(mgr.get_session("aa:bb:cc:dd:ee:ff").is_none());
    let _ = snapshot2;
}

#[test]
fn test_save_and_load_roundtrip() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");

    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:01", 1_000_000, "bytes", 3600);
    mgr.create_session("aa:bb:cc:dd:ee:02", 2_000_000, "time", 7200);
    mgr.create_session("aa:bb:cc:dd:ee:03", 3_000_000, "bytes", 1800);
    mgr.update_usage("aa:bb:cc:dd:ee:01", 500_000);

    mgr.save_to_disk(tmp.path()).expect("save should succeed");

    let loaded = SessionManager::load_from_disk(tmp.path());

    assert_eq!(loaded.sessions.len(), 3);
    let s1 = loaded.get_session("aa:bb:cc:dd:ee:01").unwrap();
    assert_eq!(s1.allotment, 1_000_000);
    assert_eq!(s1.used, 500_000);
    assert_eq!(s1.metric, "bytes");
    let s2 = loaded.get_session("aa:bb:cc:dd:ee:02").unwrap();
    assert_eq!(s2.allotment, 2_000_000);
    assert_eq!(s2.metric, "time");
    let s3 = loaded.get_session("aa:bb:cc:dd:ee:03").unwrap();
    assert_eq!(s3.allotment, 3_000_000);
}

#[test]
fn test_load_missing_file_returns_empty() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let loaded = SessionManager::load_from_disk(tmp.path());
    assert_eq!(loaded.sessions.len(), 0);
}

#[test]
fn test_load_corrupt_json_returns_empty() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let path = tmp.path().join("sessions.json");
    std::fs::write(&path, "this is not valid json {{{{").expect("write failed");

    let loaded = SessionManager::load_from_disk(tmp.path());
    assert_eq!(loaded.sessions.len(), 0);
}

#[test]
fn test_save_filters_expired_sessions() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");

    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:01", 1_000_000, "bytes", 3600);
    mgr.create_session("aa:bb:cc:dd:ee:02", 2_000_000, "bytes", 3600);
    {
        let s = mgr.sessions.get_mut("aa:bb:cc:dd:ee:02").unwrap();
        s.expiry = now() - 10;
    }

    mgr.save_to_disk(tmp.path()).expect("save should succeed");

    let loaded = SessionManager::load_from_disk(tmp.path());
    assert_eq!(loaded.sessions.len(), 1);
    assert!(loaded.get_session("aa:bb:cc:dd:ee:01").is_some());
    assert!(loaded.get_session("aa:bb:cc:dd:ee:02").is_none());
}

#[test]
fn is_active_false_when_used_equals_allotment() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    mgr.update_usage("aa:bb:cc:dd:ee:ff", 1000);
    assert!(
        !mgr.is_active("aa:bb:cc:dd:ee:ff"),
        "used == allotment should be inactive"
    );
}

#[test]
fn create_session_with_zero_allotment() {
    let mut mgr = SessionManager::new();
    let s = mgr.create_session("aa:bb:cc:dd:ee:ff", 0, "bytes", 3600);
    assert_eq!(s.allotment, 0);
    assert!(
        !mgr.is_active("aa:bb:cc:dd:ee:ff"),
        "zero-allotment session should be inactive"
    );
}

#[tokio::test]
async fn concurrent_create_session_same_mac_no_panic() {
    use std::sync::Arc;
    let mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
    let mac = "aa:bb:cc:dd:ee:ff";

    let h1 = tokio::spawn({
        let mgr = mgr.clone();
        let mac = mac.to_string();
        async move { mgr.lock().await.create_session(&mac, 1000, "bytes", 3600) }
    });
    let h2 = tokio::spawn({
        let mgr = mgr.clone();
        let mac = mac.to_string();
        async move { mgr.lock().await.create_session(&mac, 2000, "bytes", 3600) }
    });

    let (r1, r2) = (h1.await.unwrap(), h2.await.unwrap());
    assert!(r1.allotment == 1000 || r1.allotment == 2000);
    assert!(r2.allotment == 1000 || r2.allotment == 2000);
    let guard = mgr.lock().await;
    assert_eq!(guard.sessions.len(), 1, "same MAC should not duplicate");
    let final_allotment = guard.get_session(mac).unwrap().allotment;
    assert!(
        final_allotment == 1000 || final_allotment == 2000,
        "last writer wins: {final_allotment}"
    );
}

#[test]
fn re_payment_overwrites_existing_session() {
    let mut mgr = SessionManager::new();
    mgr.create_session("aa:bb:cc:dd:ee:ff", 1000, "bytes", 3600);
    mgr.update_usage("aa:bb:cc:dd:ee:ff", 800);
    mgr.create_session("aa:bb:cc:dd:ee:ff", 5000, "bytes", 3600);
    let s = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
    assert_eq!(s.allotment, 5000);
    assert_eq!(s.used, 0, "used should be reset on overwrite");
}

#[test]
fn cleanup_expired_preserves_active_sessions() {
    let mut mgr = SessionManager::new();
    for i in 0..10 {
        mgr.create_session(&format!("aa:bb:cc:dd:ee:{i:02x}"), 1000, "bytes", 3600);
    }
    {
        let s = mgr.sessions.get_mut("aa:bb:cc:dd:ee:05").unwrap();
        s.expiry = now() - 1;
    }
    let removed = mgr.cleanup_expired();
    assert_eq!(removed, 1);
    assert_eq!(mgr.sessions.len(), 9);
    assert!(mgr.get_session("aa:bb:cc:dd:ee:05").is_none());
    assert!(mgr.get_session("aa:bb:cc:dd:ee:00").is_some());
}

#[test]
fn session_state_tri_state_lifecycle() {
    let mut mgr = SessionManager::new();
    assert_eq!(mgr.session_state("aa:bb:cc:00:00:01"), SessionState::None);

    mgr.create_session("aa:bb:cc:00:00:01", 1000, "milliseconds", 3600);
    assert_eq!(mgr.session_state("aa:bb:cc:00:00:01"), SessionState::Active);

    // Revoke (the monitor's expiry path): record gone, history answers expired.
    mgr.revoke_session("aa:bb:cc:00:00:01");
    assert_eq!(
        mgr.session_state("aa:bb:cc:00:00:01"),
        SessionState::Expired
    );

    // Re-payment returns to active.
    mgr.create_session("aa:bb:cc:00:00:01", 500, "milliseconds", 3600);
    assert_eq!(mgr.session_state("aa:bb:cc:00:00:01"), SessionState::Active);
}

#[test]
fn cleanup_expired_records_history_for_session_state() {
    let mut mgr = SessionManager::new();
    let past = now() - 10;
    mgr.sessions.insert(
        "aa:bb:cc:00:00:02".into(),
        CustomerSession {
            mac: "aa:bb:cc:00:00:02".into(),
            allotment: 100,
            used: 0,
            metric: "milliseconds".into(),
            expiry: past,
            granted_at: past - 60,
            applied_grants: std::collections::HashSet::new(),
            last_grant_id: None,
        },
    );
    assert_eq!(mgr.cleanup_expired(), 1);
    assert!(!mgr.sessions.contains_key("aa:bb:cc:00:00:02"));
    assert_eq!(
        mgr.session_state("aa:bb:cc:00:00:02"),
        SessionState::Expired
    );
}

/// Codex P2 on #54: the 24h expired-history TTL must hold even when no
/// later expiry re-runs the sweep — a stale `expired` answer is wrong
/// days after the fact. The manager takes no clock, so the entry's
/// timestamp is backdated directly.
#[test]
fn session_state_respects_history_ttl_without_later_expiry() {
    let mgr = SessionManager::new();
    let stale = now() - EXPIRED_HISTORY_TTL_SECS - 1;
    let fresh = now() - 60;
    *mgr.expired_history
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = [
        ("aa:bb:cc:00:00:aa".into(), stale),
        ("aa:bb:cc:00:00:bb".into(), fresh),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        mgr.session_state("aa:bb:cc:00:00:aa"),
        SessionState::None,
        "entry older than the 24h TTL must not answer expired"
    );
    assert_eq!(
        mgr.session_state("aa:bb:cc:00:00:bb"),
        SessionState::Expired,
        "entry inside the TTL still answers expired"
    );
}
