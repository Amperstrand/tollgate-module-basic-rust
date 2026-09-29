//! GET /identity and POST /identity/reveal-seed — PR #193 identity surface.
//!
//! Go parity (main.go:1450-1456, handleIdentityRevealSeed at :1598):
//! - GET /identity returns the merchant key's public attributes
//!   `{npub, ipv4, macs}` — reachable from any interface, no secrets.
//! - POST /identity/reveal-seed is a loopback-only derivation oracle: the
//!   raw (non-JSON) body is a 12-word BIP39 mnemonic, the response is the
//!   full NIP-06 identity derived from it. It does NOT reveal the router's
//!   stored seed. Empty/invalid mnemonic → 400 "invalid mnemonic";
//!   non-POST → 405; non-loopback → 403.

use crate::http::AppState;
use crate::identity;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

pub async fn handle_get_identity(State(state): State<AppState>) -> Response {
    let priv_hex = state.identity.secret_key.display_secret().to_string();
    match identity::derive_public(&priv_hex) {
        Ok(public) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string(&public).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "text/plain")],
            format!("identity derivation failed: {e}"),
        )
            .into_response(),
    }
}

pub async fn handle_reveal_seed(
    State(_state): State<AppState>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    if !remote_addr.ip().to_canonical().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "text/plain")],
            "forbidden: loopback only",
        )
            .into_response();
    }
    reveal_seed_inner(&body).await
}

async fn reveal_seed_inner(body: &[u8]) -> Response {
    // Go reads at most 1024 bytes of body before trimming (LimitReader).
    let body_text = String::from_utf8_lossy(body);
    let mnemonic = body_text
        .chars()
        .take(1024)
        .collect::<String>()
        .trim()
        .to_string();

    match identity::derive_full_from_mnemonic(&mnemonic) {
        Ok(full) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string(&full).unwrap_or_default(),
        )
            .into_response(),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            [("content-type", "text/plain")],
            "invalid mnemonic",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    // Handler-level integration (loopback gate, 405, JSON shape) runs against
    // the real router in the VM via PRTA test_pr193_identity_endpoints.py;
    // AppState here carries wallet/session machinery not worth faking.
    // What is unit-testable offline is the derivation itself — see
    // identity.rs golden-vector tests generated from the Go implementation.
    use super::*;

    #[tokio::test]
    async fn reveal_seed_invalid_mnemonic_maps_to_bad_request() {
        let resp = reveal_seed_inner(b"not a valid mnemonic at all here").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"invalid mnemonic");
    }

    #[test]
    fn loopback_guard_matches_mapped_and_plain_addresses() {
        fn is_loopback(addr: std::net::SocketAddr) -> bool {
            addr.ip().to_canonical().is_loopback()
        }
        assert!(is_loopback("127.0.0.1:1".parse().unwrap()));
        assert!(is_loopback("[::1]:1".parse().unwrap()));
        assert!(is_loopback("[::ffff:127.0.0.1]:1".parse().unwrap()));
        assert!(!is_loopback("10.99.99.100:1".parse().unwrap()));
        assert!(!is_loopback("[::ffff:10.99.99.100]:1".parse().unwrap()));
    }
}
