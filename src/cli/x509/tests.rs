//! Tests for the minimal x509 parser, using static self-signed fixtures
//! (CN/SAN/IP combinations + a wildcard SAN) generated once with openssl.

use super::*;

const GOOD_CERT: &str = include_str!("../../../tests/fixtures/ssl/good.crt");
const GOOD_KEY: &str = include_str!("../../../tests/fixtures/ssl/good.key");
const NOSAN_CERT: &str = include_str!("../../../tests/fixtures/ssl/nosan.crt");
const WILD_CERT: &str = include_str!("../../../tests/fixtures/ssl/wild.crt");

fn parse(pem: &str) -> CertSummary {
    let der = pem_decode(pem, "CERTIFICATE").expect("fixture must decode");
    parse_certificate(&der).expect("fixture must parse")
}

#[test]
fn parse_extracts_sans_cn_and_validity() {
    let s = parse(GOOD_CERT);
    assert_eq!(
        s.dns_sans,
        vec!["tollgate-test.lan".to_string(), "tollgate-test".to_string()]
    );
    assert_eq!(s.ip_sans, vec!["192.0.2.10".parse::<IpAddr>().unwrap()]);
    assert_eq!(s.subject_cn, "tollgate-test.lan");
    assert!(s.not_before < s.not_after);
    // 2026-09-26T15:23:51Z .. 2036-09-23T15:23:51Z
    assert_eq!(s.not_before, 1790436231);
    assert_eq!(s.not_after, 2105796231);
}

#[test]
fn parse_cert_without_san_keeps_cn_only() {
    let s = parse(NOSAN_CERT);
    assert!(s.dns_sans.is_empty());
    assert!(s.ip_sans.is_empty());
    assert_eq!(s.subject_cn, "plain.example");
}

#[test]
fn parse_wildcard_san() {
    let s = parse(WILD_CERT);
    assert_eq!(s.dns_sans, vec!["*.wild.example".to_string()]);
}

#[test]
fn host_matches_exact_case_insensitive() {
    assert!(host_matches("TollGate-Test.LAN", "tollgate-test.lan"));
    assert!(host_matches("tollgate-test.lan.", "tollgate-test.lan"));
    assert!(!host_matches("tollgate-test.lan", "othertest.lan"));
}

#[test]
fn host_matches_wildcard_one_label_only() {
    assert!(host_matches("*.wild.example", "foo.wild.example"));
    assert!(!host_matches("*.wild.example", "wild.example"));
    assert!(!host_matches("*.wild.example", "a.b.wild.example"));
}

#[test]
fn civil_from_days_roundtrips_epoch() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2026, 9, 26), 20722);
    let (y, m, d) = civil_from_days(20722);
    assert_eq!((y, m, d), (2026, 9, 26));
}

#[test]
fn base64_decodes_standard_alphabet() {
    assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
    assert_eq!(base64_decode("AAA=").unwrap(), [0, 0]);
    assert!(base64_decode("!!!!").is_none());
}

#[test]
fn pem_decode_finds_first_block_of_type() {
    let combined = format!("{GOOD_CERT}\n{GOOD_KEY}");
    let der = pem_decode(&combined, "CERTIFICATE").unwrap();
    assert!(!der.is_empty());
    let key = pem_decode(&combined, "PRIVATE KEY").unwrap();
    assert!(!key.is_empty());
    assert!(pem_decode("not pem at all", "CERTIFICATE").is_none());
}

#[test]
fn parse_garbage_der_errors() {
    assert!(parse_certificate(b"").is_err());
    assert!(parse_certificate(&[0x30, 0x02, 0x00, 0x00]).is_err());
}
