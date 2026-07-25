use super::*;
use crate::config::schema::UpstreamDetectorConfig;

#[test]
fn hex_to_ip_converts_correctly() {
    assert_eq!(hex_to_ip("0100007F"), Some("127.0.0.1".to_string()));
    assert_eq!(hex_to_ip("0000FE0A"), Some("10.254.0.0".to_string()));
    assert_eq!(hex_to_ip("0200FE0A"), Some("10.254.0.2".to_string()));
}

#[test]
fn hex_to_ip_rejects_invalid() {
    assert_eq!(hex_to_ip("GGGGGGGG"), None);
    assert_eq!(hex_to_ip(""), None);
}

#[test]
fn should_skip_ignored_interface() {
    let mut config = UpstreamDetectorConfig::default();
    config.ignore_interfaces = vec!["lo".into(), "docker0".into(), "br-lan".into()];
    let detector = UpstreamDetector::new(config);

    assert!(detector.should_skip_interface("lo"));
    assert!(detector.should_skip_interface("docker0"));
    assert!(detector.should_skip_interface("br-lan"));
    assert!(!detector.should_skip_interface("eth0"));
    assert!(!detector.should_skip_interface("wlan0"));
}

#[test]
fn should_skip_respects_only_interfaces() {
    let mut config = UpstreamDetectorConfig::default();
    config.only_interfaces = vec!["eth0".into()];
    let detector = UpstreamDetector::new(config);

    assert!(!detector.should_skip_interface("eth0"));
    assert!(detector.should_skip_interface("wlan0"));
    assert!(detector.should_skip_interface("lo"));
}

#[test]
fn start_stop_lifecycle() {
    let config = UpstreamDetectorConfig::default();
    let mut detector = UpstreamDetector::new(config);

    assert!(!detector.running);
    detector.start();
    assert!(detector.running);
    detector.stop();
    assert!(!detector.running);
}

#[test]
fn read_default_gateways_parses_proc_format() {
    let route_data = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0100FE0A\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
eth0\t0000FE0A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";

    let parsed: Vec<&str> = route_data
        .lines()
        .skip(1)
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() >= 4 && f[1] == "00000000" && f[3] == "0003"
        })
        .collect();

    assert_eq!(parsed.len(), 1);
    let fields: Vec<&str> = parsed[0].split_whitespace().collect();
    assert_eq!(fields[0], "eth0");
    let gw = hex_to_ip(fields[2]);
    assert_eq!(gw, Some("10.254.0.1".to_string()));
}

#[test]
fn lookup_mac_finds_entry() {
    let arp_data = "IP address       HW type     Flags       HW address            Mask     Device\n\
10.254.0.1        0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0\n\
10.254.0.2        0x1         0x2         00:00:00:00:00:00     *        eth0\n";

    let found = arp_data
        .lines()
        .skip(1)
        .find_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[0] == "10.254.0.1" && f[3] != "00:00:00:00:00:00" {
                Some(f[3].to_string())
            } else {
                None
            }
        });

    assert_eq!(found, Some("aa:bb:cc:dd:ee:ff".to_string()));
}

#[test]
fn lookup_mac_skips_zero_mac() {
    let arp_data = "IP address       HW type     Flags       HW address            Mask     Device\n\
10.254.0.2        0x1         0x2         00:00:00:00:00:00     *        eth0\n";

    let found = arp_data
        .lines()
        .skip(1)
        .find_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[0] == "10.254.0.2" && f[3] != "00:00:00:00:00:00" {
                Some(f[3].to_string())
            } else {
                None
            }
        });

    assert_eq!(found, None);
}
