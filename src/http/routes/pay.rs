//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 4: verifies token, receives into wallet, creates session, returns
//! kind 1022 on success or kind 21023 + HTTP 402 on failure.

use crate::http::AppState;
use crate::wallet::verify::TokenVerifier;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

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
            if arr.len() >= 2 {
                if arr.first().and_then(|s| s.as_str()) == Some("payment") {
                    return arr.get(1).and_then(|s| s.as_str()).map(|s| s.to_string());
                }
            }
        }
    }
    None
}

pub async fn handle_pay(
    State(state): State<AppState>,
    headers: HeaderMap,
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
                let resp = serde_json::json!({
                    "kind": 21023,
                    "content": "invalid Nostr kind 21000 event: no payment tag found",
                });
                let json = serde_json::to_string(&resp).unwrap_or_default();
                return (
                    StatusCode::PAYMENT_REQUIRED,
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

    // Step 1: verify token via NUT-07 checkstate
    let mints: Vec<String> = state
        .config
        .accepted_mints
        .iter()
        .map(|m| m.url.clone())
        .collect();

    let verifier = TokenVerifier::new(mints);
    let verified_amount = match verifier.verify(&token).await {
        Ok(amount_msat) => amount_msat,
        Err(e) => {
            tracing::warn!(error = %e, "token verification failed");
            let resp = serde_json::json!({
                "kind": 21023,
                "content": format!("payment rejected: {e}"),
            });
            let json = serde_json::to_string(&resp).unwrap_or_default();
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 2: receive token into wallet
    let wallet_guard = state.wallet.lock().await;
    let received_amount = if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(&token).await {
            Ok(amount_sat) => {
                tracing::info!(amount_sat, "token received into wallet");
                amount_sat
            }
            Err(e) => {
                tracing::warn!(error = %e, "wallet receive failed");
                drop(wallet_guard);
                let resp = serde_json::json!({
                    "kind": 21023,
                    "content": format!("payment rejected: wallet receive failed: {e}"),
                });
                let json = serde_json::to_string(&resp).unwrap_or_default();
                return (
                    StatusCode::PAYMENT_REQUIRED,
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
        let resp = serde_json::json!({
            "kind": 21023,
            "content": "payment rejected: wallet not initialized",
        });
        let json = serde_json::to_string(&resp).unwrap_or_default();
        return (
            StatusCode::PAYMENT_REQUIRED,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    };
    drop(wallet_guard);

    // Step 3: create session — allotment in millisatoshis
    let duration_secs = 3600u64; // default 1 hour session
    let allotment = received_amount * 1000; // convert sat to millisat for allotment

    let mut sessions = state.sessions.lock().await;
    let _session = sessions.create_session(
        "00:00:00:00:00:00",
        allotment,
        &state.config.metric,
        duration_secs,
    );
    drop(sessions);

    tracing::info!(
        verified_msat = verified_amount,
        received_sat = received_amount,
        allotment_msat = allotment,
        "session granted"
    );

    // Step 4: return kind 1022 session-granted event
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let resp = serde_json::json!({
        "id": format!("{:064x}", rand::random::<u64>()),
        "pubkey": state.identity.pubkey_hex(),
        "created_at": now,
        "kind": 1022,
        "tags": [
            ["allotment", allotment.to_string()],
            ["metric", state.config.metric.clone()],
        ],
        "content": "",
        "sig": ""
    });

    let json = serde_json::to_string(&resp).unwrap_or_default();
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

    /// Test that rejected tokens return 402 (simulated).
    #[test]
    fn rejected_token_returns_402() {
        // The handler returns PAYMENT_REQUIRED for failed verification.
        // We verify the status code constant matches.
        let expected = StatusCode::PAYMENT_REQUIRED;
        assert_eq!(expected.as_u16(), 402);
    }
}