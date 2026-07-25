use super::*;
use crate::wireless::types::{Gateway, NetworkInfo, UpstreamWifiConfig};

fn test_config() -> UpstreamWifiConfig {
    UpstreamWifiConfig {
        scan_interval_seconds: 1,
        fast_check_seconds: 1,
        lost_threshold: 2,
        hysteresis_db: 12,
        signal_floor: -85,
        blacklist_ttl_minutes: 60,
        emergency_penalty: 20,
        max_consecutive_failures: 3,
        switch_cooldown_minutes: 1,
        startup_grace_seconds: 1,
        post_switch_wait_seconds: 1,
        dhcp_timeout_seconds: 10,
        manual_pause_seconds: 10,
    }
}

fn test_network(bssid: &str, ssid: &str, signal: i32) -> NetworkInfo {
    NetworkInfo {
        bssid: bssid.to_string(),
        ssid: ssid.to_string(),
        signal,
        encryption: "WPA2 PSK".to_string(),
        radio: "radio0".to_string(),
    }
}

#[test]
fn select_best_gateway_picks_strongest_signal() {
    let manager = UpstreamManager::new(test_config());
    let networks = vec![
        test_network("AA:BB:CC:DD:EE:01", "WeakAP", -80),
        test_network("AA:BB:CC:DD:EE:02", "StrongAP", -50),
        test_network("AA:BB:CC:DD:EE:03", "MediumAP", -65),
    ];

    let best = manager.select_best_gateway(&networks).unwrap();
    assert_eq!(best.ssid, "StrongAP");
    assert_eq!(best.signal, -50);
}

#[test]
fn select_best_gateway_filters_below_signal_floor() {
    let manager = UpstreamManager::new(test_config());
    let networks = vec![
        test_network("AA:BB:CC:DD:EE:01", "TooWeak", -100),
        test_network("AA:BB:CC:DD:EE:02", "OK", -70),
    ];

    let best = manager.select_best_gateway(&networks).unwrap();
    assert_eq!(best.ssid, "OK");
}

#[test]
fn select_best_gateway_returns_none_if_all_filtered() {
    let manager = UpstreamManager::new(test_config());
    let networks = vec![
        test_network("AA:BB:CC:DD:EE:01", "Weak1", -100),
        test_network("AA:BB:CC:DD:EE:02", "Weak2", -95),
    ];

    assert!(manager.select_best_gateway(&networks).is_none());
}

#[test]
fn select_best_gateway_excludes_blacklisted() {
    let mut manager = UpstreamManager::new(test_config());
    manager.blacklist_gateway("AA:BB:CC:DD:EE:02");

    let networks = vec![
        test_network("AA:BB:CC:DD:EE:01", "WeakAP", -75),
        test_network("AA:BB:CC:DD:EE:02", "StrongButBlacklisted", -50),
    ];

    let best = manager.select_best_gateway(&networks).unwrap();
    assert_eq!(best.ssid, "WeakAP");
    assert!(manager.is_blacklisted("AA:BB:CC:DD:EE:02"));
}

#[test]
fn blacklist_gateway_adds_entry() {
    let mut manager = UpstreamManager::new(test_config());
    assert!(!manager.is_blacklisted("AA:BB:CC:DD:EE:FF"));

    manager.blacklist_gateway("AA:BB:CC:DD:EE:FF");
    assert!(manager.is_blacklisted("AA:BB:CC:DD:EE:FF"));
}

#[test]
fn blacklist_cleanup_removes_expired() {
    let mut manager = UpstreamManager::new(test_config());
    manager.blacklist_gateway("AA:BB:CC:DD:EE:01");

    // Manually expire the entry
    manager.blacklist[0].expires_at = std::time::Instant::now() - Duration::from_secs(1);

    manager.cleanup_blacklist();
    assert!(!manager.is_blacklisted("AA:BB:CC:DD:EE:01"));
    assert_eq!(manager.blacklist.len(), 0);
}

#[test]
fn pause_sets_manual_pause_state() {
    let mut manager = UpstreamManager::new(test_config());
    manager.pause();
    assert_eq!(manager.state, ManagerState::ManualPause);
}

#[test]
fn resume_restores_idle_state() {
    let mut manager = UpstreamManager::new(test_config());
    manager.pause();
    manager.resume();
    assert_eq!(manager.state, ManagerState::Idle);
}

#[test]
fn resume_ignores_non_paused_state() {
    let mut manager = UpstreamManager::new(test_config());
    manager.state = ManagerState::Connected;
    manager.resume();
    assert_eq!(manager.state, ManagerState::Connected);
}

#[test]
fn force_scan_sets_scanning_state() {
    let mut manager = UpstreamManager::new(test_config());
    manager.force_scan();
    assert_eq!(manager.state, ManagerState::Scanning);
}

#[test]
fn switch_cooldown_elapsed_true_when_no_previous_switch() {
    let manager = UpstreamManager::new(test_config());
    assert!(manager.switch_cooldown_elapsed());
}

#[test]
fn consecutive_failures_increments() {
    let mut manager = UpstreamManager::new(test_config());
    assert_eq!(manager.consecutive_failures, 0);
    manager.consecutive_failures += 1;
    manager.consecutive_failures += 1;
    assert_eq!(manager.consecutive_failures, 2);
}

#[test]
fn get_status_returns_current_state() {
    let mut manager = UpstreamManager::new(test_config());
    manager.state = ManagerState::Connected;
    manager.current_gateway = Some(Gateway {
        bssid: "AA:BB:CC:DD:EE:FF".to_string(),
        ssid: "TestAP".to_string(),
        signal: -60,
        encryption: "WPA2".to_string(),
        radio: "radio0".to_string(),
    });

    let status = manager.get_status();
    assert_eq!(status.state, "Connected");
    assert_eq!(status.connected_ssid, Some("TestAP".to_string()));
    assert_eq!(status.connected_signal, Some(-60));
}

#[test]
fn emergency_penalty_extends_blacklist() {
    let mut manager = UpstreamManager::new(test_config());
    manager.consecutive_failures = 3; // at max

    let normal_ttl = manager.config.blacklist_ttl_minutes * 60;
    manager.blacklist_gateway("AA:BB:CC:DD:EE:FF");

    // The blacklist entry should have a longer TTL than normal due to emergency penalty
    let entry = &manager.blacklist[0];
    let now = std::time::Instant::now();
    let ttl = entry.expires_at.duration_since(now).as_secs();

    // Should be longer than just the normal TTL
    assert!(ttl > normal_ttl, "emergency blacklist should be longer than {normal_ttl}s, got {ttl}s");
}
