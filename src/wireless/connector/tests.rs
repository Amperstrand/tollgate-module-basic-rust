use super::*;

const SAMPLE_IW_LINK_CONNECTED: &str = "\
Connected to AA:BB:CC:DD:EE:FF (on wlan0)
\tSSID: TollGate-AP
\tfreq: 2437
\tRX: 1234567 bytes (1234 packets)
\tTX: 987654 bytes (989 packets)
\tsignal: -65 dBm
\trx bitrate: 65.0 MBit/s
\ttx bitrate: 65.0 MBit/s
";

const SAMPLE_IW_LINK_DISCONNECTED: &str = "Not connected.\n";

const SAMPLE_UCI_WIRELESS: &str = "\
wireless.radio0=wifi-device
wireless.radio0.channel='6'
wireless.radio0.hwmode='11g'
wireless.radio1=wifi-device
wireless.radio1.channel='36'
wireless.radio1.hwmode='11a'
wireless.default_radio0=wifi-iface
wireless.default_radio0.device='radio0'
wireless.default_radio0.network='lan'
wireless.default_radio0.mode='ap'
wireless.default_radio0.ssid='TollGate'
wireless.wgt0a1b=wifi-iface
wireless.wgt0a1b.device='radio0'
wireless.wgt0a1b.network='wwan'
wireless.wgt0a1b.mode='sta'
wireless.wgt0a1b.ssid='UpstreamAP'
wireless.wgt0a1b.encryption='psk2'
wireless.wgt0a1b.key='secret'
wireless.wgt0a1b.disabled='0'
";

#[test]
fn parse_connected_ssid_extracts_ssid() {
    let ssid = Connector::parse_connected_ssid(SAMPLE_IW_LINK_CONNECTED);
    assert_eq!(ssid.unwrap(), "TollGate-AP");
}

#[test]
fn parse_connected_ssid_returns_err_when_not_connected() {
    let ssid = Connector::parse_connected_ssid(SAMPLE_IW_LINK_DISCONNECTED);
    assert!(ssid.is_err());
}

#[test]
fn parse_signal_dbm_extracts_signal() {
    let signal = Connector::parse_signal_dbm(SAMPLE_IW_LINK_CONNECTED);
    assert_eq!(signal, Some(-65));
}

#[test]
fn parse_signal_dbm_returns_none_when_not_connected() {
    let signal = Connector::parse_signal_dbm(SAMPLE_IW_LINK_DISCONNECTED);
    assert_eq!(signal, None);
}

#[test]
fn uci_encryption_type_maps_wpa2() {
    assert_eq!(uci_encryption_type("WPA2 PSK (CCMP)"), "psk2");
}

#[test]
fn uci_encryption_type_maps_wpa3() {
    assert_eq!(uci_encryption_type("WPA3 SAE (CCMP)"), "sae-mixed");
}

#[test]
fn uci_encryption_type_maps_wpa() {
    assert_eq!(uci_encryption_type("WPA PSK"), "psk");
}

#[test]
fn uci_encryption_type_maps_open() {
    assert_eq!(uci_encryption_type("none"), "none");
    assert_eq!(uci_encryption_type(""), "none");
}

#[test]
fn parse_sta_sections_finds_sta() {
    let sections = Connector::parse_sta_sections(SAMPLE_UCI_WIRELESS);
    let sta = sections.iter().find(|s| s.ssid == "UpstreamAP");
    assert!(sta.is_some());
    let sta = sta.unwrap();
    assert_eq!(sta.name, "wgt0a1b");
    assert_eq!(sta.device, "radio0");
    assert_eq!(sta.encryption, "psk2");
    assert!(!sta.disabled);
}

#[test]
fn parse_sta_sections_excludes_ap_mode() {
    let sections = Connector::parse_sta_sections(SAMPLE_UCI_WIRELESS);
    let ap = sections.iter().find(|s| s.ssid == "TollGate");
    assert!(ap.is_none(), "AP sections should be excluded");
}

#[test]
fn parse_sta_sections_handles_empty() {
    let sections = Connector::parse_sta_sections("");
    assert!(sections.is_empty());
}

#[test]
fn parse_sta_sections_detects_disabled() {
    let uci = "\
wireless.sta1=wifi-iface
wireless.sta1.mode='sta'
wireless.sta1.ssid='TestAP'
wireless.sta1.disabled='1'
";
    let sections = Connector::parse_sta_sections(uci);
    assert_eq!(sections.len(), 1);
    assert!(sections[0].disabled);
}
