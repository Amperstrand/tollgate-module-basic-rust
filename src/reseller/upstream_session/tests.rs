use super::*;

const SAMPLE_SESSION_GRANT: &str = r#"{"kind":1022,"pubkey":"abcdef","content":"","tags":[["metric","bytes"],["step_size","22020096"],["allotment","22020096"],["expiry","1784900000"]]}"#;

const SAMPLE_REJECTION: &str =
    r#"{"kind":21023,"pubkey":"abcdef","content":"insufficient funds","tags":[]}"#;

#[test]
fn parse_session_grant_extracts_allotment() {
    let result = UpstreamSession::parse_session_grant(SAMPLE_SESSION_GRANT);
    assert!(result.success);
    assert_eq!(result.allotment, 22020096);
}

#[test]
fn parse_session_grant_extracts_metric() {
    let result = UpstreamSession::parse_session_grant(SAMPLE_SESSION_GRANT);
    assert_eq!(result.metric, "bytes");
}

#[test]
fn parse_session_grant_extracts_step_size() {
    let result = UpstreamSession::parse_session_grant(SAMPLE_SESSION_GRANT);
    assert_eq!(result.step_size, 22020096);
}

#[test]
fn parse_session_grant_extracts_expiry() {
    let result = UpstreamSession::parse_session_grant(SAMPLE_SESSION_GRANT);
    assert_eq!(result.expiry, 1784900000);
}

#[test]
fn parse_session_grant_rejects_wrong_kind() {
    let result = UpstreamSession::parse_session_grant(SAMPLE_REJECTION);
    assert!(!result.success);
    assert!(result.error.unwrap().contains("1022"));
}

#[test]
fn parse_session_grant_rejects_invalid_json() {
    let result = UpstreamSession::parse_session_grant("not json");
    assert!(!result.success);
    assert!(result.error.unwrap().contains("parse"));
}

#[test]
fn parse_session_grant_rejects_missing_allotment() {
    let grant = r#"{"kind":1022,"tags":[["metric","bytes"]]}"#;
    let result = UpstreamSession::parse_session_grant(grant);
    assert!(!result.success);
    assert!(result.error.unwrap().contains("allotment"));
}

#[test]
fn needs_renewal_below_threshold() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.used = 500;
    assert!(!session.needs_renewal());
}

#[test]
fn needs_renewal_at_threshold() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.used = 800;
    assert!(session.needs_renewal());
}

#[test]
fn needs_renewal_above_threshold() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.used = 950;
    assert!(session.needs_renewal());
}

#[test]
fn needs_renewal_zero_allotment() {
    let session = UpstreamSession::new("10.0.0.1", "wlan0");
    assert!(!session.needs_renewal());
}

#[test]
fn is_expired_checks_expiry_time() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.expiry = 1;
    assert!(session.is_expired());

    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    session.expiry = future;
    assert!(!session.is_expired());
}

#[test]
fn remaining_calculates_correctly() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.used = 300;
    assert_eq!(session.remaining(), 700);

    session.used = 1000;
    assert_eq!(session.remaining(), 0);

    session.used = 1500;
    assert_eq!(session.remaining(), 0);
}

#[test]
fn update_usage_returns_renewal_flag() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.renewal_threshold = 0.8;

    assert!(!session.update_usage(500));
    assert!(session.update_usage(800));
    assert!(session.update_usage(900));
}

#[test]
fn apply_payment_updates_session() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.used = 500;

    let result = UpstreamPaymentResult {
        success: true,
        allotment: 22020096,
        metric: "bytes".to_string(),
        step_size: 22020096,
        expiry: 1784900000,
        error: None,
    };

    session.apply_payment(&result);
    assert_eq!(session.allotment, 22020096);
    assert_eq!(session.used, 0);
    assert_eq!(session.expiry, 1784900000);
}

#[test]
fn apply_payment_ignores_failed_result() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.allotment = 1000;
    session.used = 500;

    let result = UpstreamPaymentResult {
        success: false,
        allotment: 0,
        metric: String::new(),
        step_size: 0,
        expiry: 0,
        error: Some("payment failed".to_string()),
    };

    session.apply_payment(&result);
    assert_eq!(session.allotment, 1000);
    assert_eq!(session.used, 500);
}

#[test]
fn is_active_checks_both_expiry_and_usage() {
    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");

    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;

    session.allotment = 1000;
    session.used = 500;
    session.expiry = future;
    assert!(session.is_active());

    session.used = 1000;
    assert!(!session.is_active());

    session.used = 500;
    session.expiry = 1;
    assert!(!session.is_active());
}

/// Codex P1 on #55: Go's kind-1022 (and this repo's, for wire parity)
/// carries no `expiry` tag. The parser must DERIVE one — left at zero,
/// `is_expired()` (0 ⇒ expired) makes the monitor discard the paid
/// session on its first tick and force a re-purchase.
#[test]
fn parse_derives_expiry_for_time_metered_grant_without_expiry_tag() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = format!(
        r#"{{"kind":1022,"tags":[["allotment","3600000"],["metric","milliseconds"],["start-time","{now}"]]}}"#
    );
    let result = UpstreamSession::parse_session_grant(&grant);
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.expiry,
        now + 3600,
        "milliseconds allotment IS the duration"
    );

    let mut session = UpstreamSession::new("10.0.0.1", "wlan0");
    session.apply_payment(&result);
    assert!(
        !session.is_expired(),
        "a just-paid derived session must survive the monitor's first tick"
    );
}

#[test]
fn parse_derives_default_expiry_for_non_time_grant_without_expiry_tag() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = format!(
        r#"{{"kind":1022,"tags":[["allotment","1000000000"],["metric","bytes"],["start-time","{now}"]]}}"#
    );
    let result = UpstreamSession::parse_session_grant(&grant);
    assert!(result.success);
    assert_eq!(result.expiry, now + 3600);
}

#[test]
fn parse_keeps_explicit_expiry_tag_over_derivation() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = format!(
        r#"{{"kind":1022,"tags":[["allotment","3600000"],["metric","milliseconds"],["expiry","{}"]]}}"#,
        now + 7200
    );
    let result = UpstreamSession::parse_session_grant(&grant);
    assert_eq!(result.expiry, now + 7200, "explicit tag must win");
}
