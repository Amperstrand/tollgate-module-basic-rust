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
    assert!(
        ttl > normal_ttl,
        "emergency blacklist should be longer than {normal_ttl}s, got {ttl}s"
    );
}

/// Codex P2 on #54 / P1 on #55: a DEFINITIVE wallet.send failure (mint's
/// wallet not registered — fails before CDK, no saga, no value moved) must
/// not journal the blocking Ambiguous phase: no token exists to recover,
/// and one such entry wedges every future reseller purchase behind
/// undelivered_tokens forever.
#[tokio::test]
#[serial_test::serial]
async fn definitive_send_failure_does_not_block_repurchase() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", tmp.path());
    let cfg_dir = tmp.path().to_path_buf();

    // No mint registered: wallet.send fails with WalletNotFound without
    // touching the network or creating any saga.
    let mut seed = [0u8; 64];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut seed);
    let wallet = TollWallet::new(seed, vec![], tmp.path().join("db"));
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");

    let first =
        UpstreamManager::journalled_purchase(&wallet, "http://127.0.0.1:1", 1000, &mut session)
            .await;
    assert!(first.is_none(), "send must fail (wallet not registered)");

    use crate::payout_journal as pj;
    let undelivered = pj::undelivered_tokens(&cfg_dir, "reseller-upstream");
    assert!(
        undelivered.is_empty(),
        "a definitive failure has no token to recover and must not block: {undelivered:?}"
    );

    // And the duplicate-attempt gate lets a second purchase attempt run
    // (it fails the same way, but is not BLOCKED by the first failure).
    let second =
        UpstreamManager::journalled_purchase(&wallet, "http://127.0.0.1:1", 1000, &mut session)
            .await;
    assert!(second.is_none());
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

/// Codex P2 on #54: `sta_interface` holds the UCI wifi-iface SECTION name
/// — polling `iw dev <section> link` always fails on normal OpenWrt, so
/// signal monitoring is dead. do_monitor must resolve the netifd
/// l3_device (ubus) and pass THAT to `iw`. Verified end-to-end with
/// PATH-stubbed `ubus` + `iw` binaries.
#[tokio::test]
#[serial_test::serial]
async fn monitor_polls_signal_on_the_resolved_l3_device() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // ubus advertises the wwan netifd interface's l3_device.
    let ubus = bin.join("ubus");
    std::fs::write(&ubus, "#!/bin/sh\nprintf '{\"l3_device\":\"wwan0\"}'\n").unwrap();

    // iw records the device it was asked about.
    let log = tmp.path().join("iw.log");
    let iw = bin.join("iw");
    std::fs::write(
        &iw,
        format!("#!/bin/sh\nprintf '%s ' \"$@\" >> {}\n", log.display()),
    )
    .unwrap();
    make_executable(ubus);
    make_executable(iw);

    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin.display(), old_path));

    let mut manager = UpstreamManager::new(test_config());
    // What Connector::connect actually returns: a UCI section name.
    manager.sta_interface = Some("wgt0a1b".to_string());
    manager.current_gateway = Some(test_network("AA:BB:CC:DD:EE:01", "AP", -50).into());

    let _ = manager.do_monitor(None).await;

    let polled = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        polled.contains("dev wwan0 link"),
        "signal poll must target the resolved l3_device, got: {polled:?}"
    );
    assert!(
        !polled.contains("wgt0a1b"),
        "the UCI section name is not a kernel device"
    );

    std::env::set_var("PATH", old_path);
}

/// Codex P2 on #58: the l3_device cache must not outlive the STA section
/// it was resolved for. Multi-radio routers can switch sections on
/// reconnect; netifd then binds a different l3_device, and a stale cache
/// would poll a dead device forever.
#[tokio::test]
async fn monitor_re_resolves_device_after_sta_section_switch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // resolve_l3_device always queries `network.interface.wwan` — what
    // changes on a section switch is netifd's ANSWER (it rebinds the
    // l3_device). The stub is therefore stateful: first call answers
    // wwan0 (radio0's binding), every later call wlan1 (radio1's).
    let counter = tmp.path().join("ubus.calls");
    let ubus = bin.join("ubus");
    std::fs::write(
        &ubus,
        format!(
            "#!/bin/sh\nn=$(cat {c} 2>/dev/null || echo 0)\nn=$((n+1))\n\
             echo $n > {c}\n\
             if [ $n -le 1 ]; then printf '{{\"l3_device\":\"wwan0\"}}'\n\
             else printf '{{\"l3_device\":\"wlan1\"}}'\nfi\n",
            c = counter.display()
        ),
    )
    .unwrap();

    let log = tmp.path().join("iw.log");
    let iw = bin.join("iw");
    std::fs::write(
        &iw,
        format!("#!/bin/sh\nprintf '%s ' \"$@\" >> {}\n", log.display()),
    )
    .unwrap();
    make_executable(ubus);
    make_executable(iw);

    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin.display(), old_path));

    let mut manager = UpstreamManager::new(test_config());
    manager.sta_interface = Some("wgt0a1b".to_string());
    manager.current_gateway = Some(test_network("AA:BB:CC:DD:EE:01", "AP", -50).into());
    let _ = manager.do_monitor(None).await;
    assert!(
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("dev wwan0 link"),
        "first poll resolves radio0's l3_device"
    );

    // Reconnect selects radio1's section — the cache must invalidate
    // (production does this in do_scan_and_connect; simulated directly).
    manager.sta_interface = Some("wgt1a1b".to_string());
    manager.sta_device = None;
    let _ = manager.do_monitor(None).await;
    assert!(
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("dev wlan1 link"),
        "after a section switch the poll must target the NEW l3_device"
    );

    std::env::set_var("PATH", old_path);
}

fn make_executable(path: std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
}
