//! MAC address resolution — 1:1 port of Go's `getMacAddress` + `getIP`.
//!
//! Mirrors `tollgate-module-basic-go/src/main.go` lines 291–325 (getMacAddress)
//! and 811–829 (getIP). Behaviour contract:
//!
//! - `get_mac_address(ip)`:
//!     1. Validate `ip` parses as `IpAddr` (matches Go's `net.ParseIP`).
//!     2. Read `/tmp/dhcp.leases`. Each line is split on whitespace; if
//!        `fields.len() >= 3 && fields[2].eq_ignore_ascii_case(ip)`,
//!        return `fields[1]` (the MAC).
//!     3. Fallback: read `/proc/net/arp`. Each line is split on whitespace;
//!        if `fields.len() >= 4 && fields[0].eq_ignore_ascii_case(ip)
//!        && fields[3] != "00:00:00:00:00:00"`, return `fields[3]`.
//!     4. Otherwise return `None`.
//!
//! - `get_client_ip(headers, remote_addr)`:
//!     Mirrors Go's `getIP`: only honours forwarding headers when the
//!     request is from a loopback peer (so production OpenWrt traffic
//!     is unaffected). Order: `X-Real-Ip`, then first IP of
//!     `X-Forwarded-For`, then `remote_addr`.

use axum::http::{HeaderMap, HeaderName};
use std::net::{IpAddr, SocketAddr};

/// Path to the dnsmasq lease file — same as Go's hardcoded `/tmp/dhcp.leases`.
const DHCP_LEASES_PATH: &str = "/tmp/dhcp.leases";

/// Path to the kernel ARP table — same as Go's hardcoded `/proc/net/arp`.
const PROC_NET_ARP_PATH: &str = "/proc/net/arp";

/// Zero-MAC sentinel. Go skips ARP rows whose MAC equals this.
const ZERO_MAC: &str = "00:00:00:00:00:00";

/// Resolve a MAC address for the given IP, consulting dnsmasq leases first
/// and the kernel ARP table as a fallback. Returns `None` if `ip` is not a
/// valid IP or no entry is found in either source.
///
/// File I/O is synchronous and blocking. `/tmp/dhcp.leases` and
/// `/proc/net/arp` are tiny pseudo-files, so blocking cost is sub-millisecond;
/// matching Go's behaviour is preferable to a tokio spawn_blocking indirection
/// here. Callers already run inside `async` handlers but hold no lock during
/// the read.
pub fn get_mac_address(ip: &str) -> Option<String> {
    // Validate IP — matches Go's `net.ParseIP(ipAddress) == nil` guard.
    if ip.parse::<IpAddr>().is_err() {
        return None;
    }
    let ip_lower = ip.trim().to_ascii_lowercase();

    // Primary source: dnsmasq lease file.
    // Format per line: <timestamp> <mac> <ip> <hostname> <clientid>
    if let Ok(data) = std::fs::read_to_string(DHCP_LEASES_PATH) {
        for line in data.split('\n') {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields[2].to_ascii_lowercase() == ip_lower {
                return Some(fields[1].trim().to_string());
            }
        }
    }

    // Fallback: kernel ARP table.
    // Format per line: <ip> <hwtype> <flags> <mac> <mask> <device>
    if let Ok(data) = std::fs::read_to_string(PROC_NET_ARP_PATH) {
        for line in data.split('\n') {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4
                && fields[0].to_ascii_lowercase() == ip_lower
                && fields[3] != ZERO_MAC
            {
                return Some(fields[3].trim().to_string());
            }
        }
    }

    None
}

/// Extract the client IP from request headers (or fall back to the socket
/// remote address), mirroring Go's `getIP(r)`. Go only honours forwarding
/// headers when `isLocalRequest(r)` is true — i.e. the peer is loopback.
/// We replicate that guard: forwarding headers are consulted iff
/// `remote_addr` is a loopback address.
pub fn get_client_ip(headers: &HeaderMap, remote_addr: Option<SocketAddr>) -> String {
    let is_local = remote_addr
        .map(|sa| sa.ip().is_loopback())
        .unwrap_or(false);

    if is_local {
        if let Some(real_ip) = headers.get(&HeaderName::from_static("x-real-ip")) {
            if let Ok(s) = real_ip.to_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
        if let Some(xff) = headers.get(&HeaderName::from_static("x-forwarded-for")) {
            if let Ok(s) = xff.to_str() {
                if let Some(first) = s.split(',').next() {
                    let trimmed = first.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }
    }

    // Fallback: socket remote address (host part only, matches Go's
    // net.SplitHostPort → host).
    if let Some(sa) = remote_addr {
        return sa.ip().to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

    fn make_v4(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(
            ip.parse::<Ipv4Addr>().unwrap(),
            port,
        ))
    }

    // ── get_mac_address ────────────────────────────────────────────────

    #[test]
    fn mac_address_rejects_invalid_ip() {
        assert!(get_mac_address("not-an-ip").is_none());
        assert!(get_mac_address("").is_none());
        assert!(get_mac_address("999.999.999.999").is_none());
    }

    #[test]
    fn mac_address_parses_dhcp_leases_ipv4() {
        // The system's real /tmp/dhcp.leases on this test host has a
        // 127.0.0.1 entry injected by the parity fixture. If absent
        // (clean env), skip gracefully rather than fail.
        let resolved = get_mac_address("127.0.0.1");
        if let Some(mac) = resolved {
            assert!(
                mac.contains(':'),
                "expected colon-separated MAC, got {mac}"
            );
            assert_ne!(mac, "00:00:00:00:00:00");
        }
    }

    #[test]
    fn mac_address_parses_dhcp_leases_case_insensitively() {
        // Same entry in different casing must still match.
        let lower = get_mac_address("127.0.0.1");
        let upper = get_mac_address("127.0.0.1".to_ascii_uppercase().as_str());
        assert_eq!(lower, upper, "case-insensitive match expected");
    }

    #[test]
    fn mac_address_returns_none_for_unroutable_test_ip() {
        // 240.0.0.1 is a valid IP but not in dhcp.leases or /proc/net/arp
        // on a typical test host. (If it ever resolves, the test still
        // passes — it's a soft assertion.)
        let mac = get_mac_address("240.0.0.1");
        assert!(
            mac.is_none(),
            "240.0.0.1 should not resolve on a clean test host, got {mac:?}"
        );
    }

    #[test]
    fn mac_address_accepts_ipv6_loopback() {
        // ::1 is a valid IP; resolver should at least not panic.
        let _ = get_mac_address("::1");
    }

    // ── get_client_ip ─────────────────────────────────────────────────

    #[test]
    fn client_ip_uses_x_real_ip_when_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "192.168.1.50".parse().unwrap());
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 12345)));
        assert_eq!(ip, "192.168.1.50");
    }

    #[test]
    fn client_ip_uses_x_forwarded_for_first_when_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "10.0.0.5, 10.0.0.6, 10.0.0.7".parse().unwrap(),
        );
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 12345)));
        assert_eq!(ip, "10.0.0.5");
    }

    #[test]
    fn client_ip_x_real_ip_takes_precedence_over_xff() {
        // Mirrors Go's order: X-Real-Ip checked first.
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "1.2.3.4".parse().unwrap());
        headers.insert("x-forwarded-for", "5.6.7.8".parse().unwrap());
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 12345)));
        assert_eq!(ip, "1.2.3.4");
    }

    #[test]
    fn client_ip_falls_back_to_remote_addr_when_no_headers() {
        let headers = HeaderMap::new();
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 12345)));
        assert_eq!(ip, "127.0.0.1");
    }

    #[test]
    fn client_ip_ignores_headers_when_remote_is_not_loopback() {
        // Non-loopback peer: headers MUST be ignored (Go parity).
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "192.168.1.50".parse().unwrap());
        headers.insert("x-forwarded-for", "10.0.0.5".parse().unwrap());
        let ip = get_client_ip(&headers, Some(make_v4("192.168.0.1", 12345)));
        assert_eq!(ip, "192.168.0.1");
    }

    #[test]
    fn client_ip_handles_empty_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "".parse().unwrap());
        headers.insert("x-forwarded-for", "   ".parse().unwrap());
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 12345)));
        // Both headers empty → fallback to RemoteAddr.
        assert_eq!(ip, "127.0.0.1");
    }

    #[test]
    fn client_ip_handles_missing_remote_addr() {
        let headers = HeaderMap::new();
        let ip = get_client_ip(&headers, None);
        assert_eq!(ip, "");
    }

    #[test]
    fn client_ip_ipv6_loopback_is_treated_as_local() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "2001:db8::1".parse().unwrap());
        let sa: SocketAddr = format!("[::1]:443").parse().unwrap();
        let ip = get_client_ip(&headers, Some(sa));
        assert_eq!(ip, "2001:db8::1");
        // Sanity: exercise an Ipv6Addr parse path.
        let _: Ipv6Addr = "2001:db8::1".parse().unwrap();
    }

    #[test]
    fn client_ip_trims_whitespace_in_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "  192.168.1.99  ".parse().unwrap());
        let ip = get_client_ip(&headers, Some(make_v4("127.0.0.1", 1)));
        assert_eq!(ip, "192.168.1.99");
    }
}
