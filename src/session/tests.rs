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
