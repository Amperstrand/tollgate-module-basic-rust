//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 2: text/plain path verifies token via NUT-07 checkstate.
//! JSON path returns 501 (Phase 3).

use crate::http::AppState;
use crate::wallet::verify::TokenVerifier;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

pub async fn handle_pay(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("text/plain") {
        tracing::info!(len = body.len(), "received text/plain payment");

        // Build accepted-mints list from config
        let mints: Vec<String> = state
            .config
            .accepted_mints
            .iter()
            .map(|m| m.url.clone())
            .collect();

        let verifier = TokenVerifier::new(mints);
        match verifier.verify(body.trim()).await {
            Ok(amount_msat) => {
                tracing::info!(amount_msat, "token verified — session granted");

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                // Nostr kind 1022 = session granted
                let resp = serde_json::json!({
                    "id": format!("{:064x}", rand::random::<u64>()),
                    "pubkey": state.identity.pubkey_hex(),
                    "created_at": now,
                    "kind": 1022,
                    "tags": [
                        ["allotment", amount_msat.to_string()],
                        ["metric", state.config.metric.clone()],
                    ],
                    "content": "",
                    "sig": ""
                });

                let json = serde_json::to_string(&resp).unwrap_or_default();
                return (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "token verification failed");

                // Nostr kind 21023 = notice/rejection
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
        }
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment (Phase 3 stub)");
        // Phase 3: parse Nostr kind 21000 event, extract cashu token from tags
        return (
            StatusCode::NOT_IMPLEMENTED,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            r#"{"error":"json payment path not yet implemented"}"#.to_string(),
        );
    }

    (
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        "unsupported content-type".to_string(),
    )
}
