use super::Scanner;
use super::super::types::*;

const SAMPLE_IWINFO_OUTPUT: &str = "\
Cell 01 - Address: AA:BB:CC:DD:EE:FF
          ESSID: \"TollGate-AP\"
          Quality: 40/70  Signal: -70 dBm  Noise: -95 dBm
          Channel: 6 (2.4 GHz)
          Encryption: WPA2 PSK (CCMP)

Cell 02 - Address: 11:22:33:44:55:66
          ESSID: \"FreeWiFi\"
          Quality: 60/70  Signal: -50 dBm  Noise: -95 dBm
          Channel: 11 (2.4 GHz)
          Encryption: none

Cell 03 - Address: DE:AD:BE:EF:00:01
          ESSID: \"AnotherTollGate\"
          Quality: 20/70  Signal: -85 dBm  Noise: -95 dBm
          Channel: 149 (5 GHz)
          Encryption: WPA3 SAE (CCMP)
";

#[test]
fn parse_scan_output_extracts_all_cells() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    assert_eq!(networks.len(), 3);
}

#[test]
fn parse_scan_output_extracts_bssid() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    assert_eq!(networks[0].bssid, "AA:BB:CC:DD:EE:FF");
    assert_eq!(networks[1].bssid, "11:22:33:44:55:66");
    assert_eq!(networks[2].bssid, "DE:AD:BE:EF:00:01");
}

#[test]
fn parse_scan_output_extracts_ssid() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    assert_eq!(networks[0].ssid, "TollGate-AP");
    assert_eq!(networks[1].ssid, "FreeWiFi");
    assert_eq!(networks[2].ssid, "AnotherTollGate");
}

#[test]
fn parse_scan_output_extracts_signal() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    assert_eq!(networks[0].signal, -70);
    assert_eq!(networks[1].signal, -50);
    assert_eq!(networks[2].signal, -85);
}

#[test]
fn parse_scan_output_extracts_encryption() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    assert!(networks[0].encryption.contains("WPA2"));
    assert_eq!(networks[1].encryption, "none");
    assert!(networks[2].encryption.contains("WPA3"));
}

#[test]
fn parse_scan_output_assigns_radio() {
    let networks = Scanner::parse_scan_output(SAMPLE_IWINFO_OUTPUT, "radio0");
    for net in &networks {
        assert_eq!(net.radio, "radio0");
    }
}

#[test]
fn parse_scan_output_handles_empty() {
    let networks = Scanner::parse_scan_output("", "radio0");
    assert!(networks.is_empty());
}

#[test]
fn parse_scan_output_handles_no_essid() {
    let input = "Cell 01 - Address: AA:BB:CC:DD:EE:FF\n          Quality: 40/70  Signal: -70 dBm\n";
    let networks = Scanner::parse_scan_output(input, "radio0");
    assert!(networks.is_empty());
}

#[test]
fn parse_scan_output_skips_empty_essid() {
    let input = "\
Cell 01 - Address: AA:BB:CC:DD:EE:FF
          ESSID: \"\"
          Quality: 40/70  Signal: -70 dBm
";
    let networks = Scanner::parse_scan_output(input, "radio0");
    assert!(networks.is_empty());
}

#[test]
fn gateway_from_network_info_preserves_fields() {
    let net = NetworkInfo {
        bssid: "AA:BB:CC:DD:EE:FF".to_string(),
        ssid: "TestAP".to_string(),
        signal: -65,
        encryption: "WPA2 PSK".to_string(),
        radio: "radio0".to_string(),
    };
    let gw: Gateway = net.into();
    assert_eq!(gw.bssid, "AA:BB:CC:DD:EE:FF");
    assert_eq!(gw.ssid, "TestAP");
    assert_eq!(gw.signal, -65);
}

#[test]
fn upstream_wifi_config_defaults_are_sensible() {
    let config = UpstreamWifiConfig::default();
    assert_eq!(config.scan_interval_seconds, 300);
    assert_eq!(config.signal_floor, -85);
    assert_eq!(config.lost_threshold, 2);
    assert_eq!(config.max_consecutive_failures, 3);
    assert!(config.dhcp_timeout_seconds > 0);
}
