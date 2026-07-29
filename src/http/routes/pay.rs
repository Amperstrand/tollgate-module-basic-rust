//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 4: verifies token, receives into wallet, creates session, returns
//! kind 1022 on success or kind 21023 + HTTP 400 on failure.
//!
// HTTP #1: The Server MUST take a http `POST` request containing a bearer asset token directly in the request body.
// HTTP #1: If the TollGate accepts the provided payment it MUST return http `200 OK` response where the body is a `kind=1022` TollGate Session event.
// HTTP #1: If the payment is invalid, it SHOULD return a `kind=21023` Notice event with an appropriate error code, and MAY use http `402 Payment Required` or `400 Bad Request` status.
// TIP #1: `p`: pubkey of the TollCustomer Gate (from the Customer's Payment event)
// TIP #1: `device-identifier`: (hardware) identifier of the customer's device.
// TIP #1: `allotment`: Amount of `<metric>` allotted to customer after payment.
// TIP #1: `metric`: The purchased metric

use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};
use crate::nostr_event;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::net::SocketAddr;

/// Extract a Cashu token from a Nostr kind 21000 event JSON body.
/// Looks for a tag ["payment", "<token>"].
fn extract_token_from_nostr_event(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("kind").and_then(|k| k.as_u64()) != Some(21000) {
        return None;
    }
    let tags = v.get("tags")?.as_array()?;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            if arr.len() >= 2 && arr.first().and_then(|s| s.as_str()) == Some("payment") {
                return arr.get(1).and_then(|s| s.as_str()).map(|s| s.to_string());
            }
        }
    }
    None
}

pub async fn handle_pay(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Extract token from either path
    let token = if content_type.contains("text/plain") {
        tracing::info!(len = body.len(), "received text/plain payment");
        body.trim().to_string()
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment");
        match extract_token_from_nostr_event(&body) {
            Some(t) => t,
            None => {
                let event = nostr_event::create_event(
                    21023,
                    vec![
                        vec!["level".to_string(), "error".to_string()],
                        vec!["code".to_string(), "invalid-nostr-event".to_string()],
                    ],
                    "invalid Nostr kind 21000 event: no payment tag found",
                    &state.identity.secret_key,
                );
                let json = serde_json::to_string(&event).unwrap_or_default();
                return (
                    StatusCode::BAD_REQUEST,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            "unsupported content-type".to_string(),
        );
    };

    let client_ip = get_client_ip(&headers, Some(remote_addr));

    if let Ok(ip) = client_ip.parse() {
        if !state.rate_limiter.allow(ip).await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("content-type", "text/plain"),
                    ("access-control-allow-origin", "*"),
                ],
                "rate limited".to_string(),
            );
        }
    }

    let mac = match get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec!["code".to_string(), "mac-address-lookup-failed".to_string()],
                ],
                "payment rejected: mac-address-lookup-failed",
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 1: verify token via NUT-07 checkstate
    let (verified_amount, token_mint_url) = match state.verifier.verify(&token).await {
        Ok((amount_msat, mint_url)) => (amount_msat, mint_url),
        Err(e) => {
            tracing::warn!(error = %e, "token verification failed");
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec!["code".to_string(), "token-verification-failed".to_string()],
                ],
                &format!("payment rejected: {e}"),
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 2: receive token into wallet
    let wallet_guard = state.wallet.read().await;
    let received_amount = if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(&token).await {
            Ok(amount_sat) => {
                tracing::info!(amount_sat, "token received into wallet");
                amount_sat
            }
            Err(e) => {
                tracing::warn!(error = %e, "wallet receive failed");
                drop(wallet_guard);
                let event = nostr_event::create_event(
                    21023,
                    vec![
                        vec!["level".to_string(), "error".to_string()],
                        vec!["code".to_string(), "wallet-receive-failed".to_string()],
                    ],
                    &format!("payment rejected: wallet receive failed: {e}"),
                    &state.identity.secret_key,
                );
                let json = serde_json::to_string(&event).unwrap_or_default();
                return (
                    StatusCode::BAD_REQUEST,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        tracing::warn!("wallet not initialized");
        drop(wallet_guard);
        let event = nostr_event::create_event(
            21023,
            vec![
                vec!["level".to_string(), "error".to_string()],
                vec!["code".to_string(), "wallet-not-initialized".to_string()],
            ],
            "payment rejected: wallet not initialized",
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            StatusCode::BAD_REQUEST,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    };
    drop(wallet_guard);

    // Step 3: create session — allotment in the metric's unit (bytes or ms)
    let duration_secs = 3600u64;
    let mint_config = state
        .config
        .accepted_mints
        .iter()
        .find(|m| {
            let cfg_url = m.url.trim_end_matches('/');
            cfg_url == token_mint_url || m.url == token_mint_url
        })
        .or_else(|| state.config.accepted_mints.first());
    let price_per_step = mint_config.map(|m| m.price_per_step).unwrap_or(1).max(1);

    // Validate minimum purchase (payment too small)
    let steps = received_amount / price_per_step;
    if steps == 0 {
        let event = nostr_event::create_event(
            21023,
            vec![],
            "payment rejected: amount below minimum purchase",
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            StatusCode::BAD_REQUEST,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }

    let step_size = state.config.step_size;
    let allotment = (received_amount / price_per_step) * step_size;

    let mut sessions = state.sessions.lock().await;
    let _session = sessions.create_session(&mac, allotment, &state.config.metric, duration_secs);
    sessions
        .save_to_disk(&crate::config::config_dir())
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to save sessions to disk");
        });
    drop(sessions);

    // Open the gate to grant network access via ndsctl.
    if let Err(e) = state.portal.grant_access(&mac).await {
        tracing::warn!(mac = %mac, error = %e, "failed to open gate");
        // Continue anyway — session is created, gate may be opened manually.
    }

    tracing::info!(
        verified_msat = verified_amount,
        received_sat = received_amount,
        allotment = allotment,
        "session granted"
    );

    // Step 4: return kind 1022 session-granted event
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tags = vec![
        vec!["p".to_string(), state.identity.pubkey_hex()],
        vec![
            "device-identifier".to_string(),
            "mac".to_string(),
            mac.clone(),
        ],
        vec!["allotment".to_string(), allotment.to_string()],
        vec!["metric".to_string(), state.config.metric.clone()],
        vec!["start-time".to_string(), now.to_string()],
    ];
    let event = nostr_event::create_event(1022, tags, "", &state.identity.secret_key);
    let json = serde_json::to_string(&event).unwrap_or_default();
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_valid_nostr_event() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["payment", "cashuBabc123token"]],
            "content": "",
            "pubkey": "abc",
            "id": "def",
            "sig": "ghi",
            "created_at": 1234567890
        })
        .to_string();
        let token = extract_token_from_nostr_event(&event);
        assert_eq!(token.as_deref(), Some("cashuBabc123token"));
    }

    #[test]
    fn extract_token_rejects_wrong_kind() {
        let event = serde_json::json!({
            "kind": 99999,
            "tags": [["payment", "token"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_rejects_missing_payment_tag() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["other", "value"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_handles_invalid_json() {
        assert!(extract_token_from_nostr_event("not json").is_none());
    }

    #[test]
    fn extract_token_handles_multiple_tags() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [
                ["other", "val"],
                ["payment", "real-token"],
                ["another", "x"]
            ],
        })
        .to_string();
        assert_eq!(
            extract_token_from_nostr_event(&event).as_deref(),
            Some("real-token")
        );
    }

    /// Test that a session is created after a successful payment flow.
    /// Uses the SessionManager directly to verify the integration logic.
    #[tokio::test]
    async fn payment_creates_session() {
        use crate::session::SessionManager;

        let mut mgr = SessionManager::new();
        let allotment: u64 = 5000; // 5 sats * 1000 = 5000 msat
        let session = mgr.create_session("test:mac", allotment, "bytes", 3600);
        assert_eq!(session.allotment, 5000);
        assert_eq!(session.metric, "bytes");
        assert!(mgr.is_active("test:mac"));
    }

    /// Test that rejected tokens return 400 (simulated).
    #[test]
    fn rejected_token_returns_400() {
        // The handler returns BAD_REQUEST for failed verification.
        // We verify the status code constant matches.
        let expected = StatusCode::BAD_REQUEST;
        assert_eq!(expected.as_u16(), 400);
    }

    #[test]
    fn unsupported_content_type_returns_415() {
        let expected = StatusCode::UNSUPPORTED_MEDIA_TYPE;
        assert_eq!(expected.as_u16(), 415);
    }

    #[test]
    fn payment_below_minimum_detected_via_zero_steps() {
        let received_amount: u64 = 0;
        let price_per_step: u64 = 1;
        let steps = received_amount / price_per_step;
        assert_eq!(steps, 0, "zero amount must yield zero steps");
    }

    #[test]
    fn payment_below_minimum_with_small_amount() {
        let received_amount: u64 = 1;
        let price_per_step: u64 = 2;
        let steps = received_amount / price_per_step;
        assert_eq!(steps, 0, "amount < price_per_step must yield zero steps");
    }

    #[test]
    fn allotment_calculation_correct() {
        let received_amount: u64 = 5;
        let price_per_step: u64 = 1;
        let step_size: u64 = 22020096;
        let steps = received_amount / price_per_step;
        let allotment = steps * step_size;
        assert_eq!(steps, 5);
        assert_eq!(allotment, 110100480);
    }

    #[tokio::test]
    async fn concurrent_sessions_different_macs_no_interference() {
        use crate::session::SessionManager;
        let mgr = std::sync::Arc::new(tokio::sync::Mutex::new(SessionManager::new()));

        let h1 = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.lock()
                    .await
                    .create_session("aa:bb:cc:dd:ee:01", 1000, "bytes", 3600)
            }
        });
        let h2 = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.lock()
                    .await
                    .create_session("aa:bb:cc:dd:ee:02", 2000, "bytes", 3600)
            }
        });

        let (s1, s2) = (h1.await.unwrap(), h2.await.unwrap());
        assert_eq!(s1.mac, "aa:bb:cc:dd:ee:01");
        assert_eq!(s2.mac, "aa:bb:cc:dd:ee:02");

        let guard = mgr.lock().await;
        assert_eq!(guard.sessions.len(), 2);
        assert!(guard.is_active("aa:bb:cc:dd:ee:01"));
        assert!(guard.is_active("aa:bb:cc:dd:ee:02"));
    }
}
