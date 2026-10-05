//! GET /usage — returns plain text "used/total" or "-1/-1".
//!
// HTTP #3: A `GET` request on the `/usage` endpoint MUST return http status `200 OK` with the body containing the customer's current usage and allotment.
// HTTP #3: Formatted as `[usage]/[allotment]`
// HTTP #3: If the customer has no active session, the response MUST be `-1/-1`.

//!
//! Phase 4: queries SessionManager for the requesting client's session.
//! The client MAC is sourced from X-Forwarded-For or remote_addr (matching
//! Go behavior on OpenWrt where nodogsplash sets X-Forwarded-For).

use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::net::SocketAddr;

pub async fn handle_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let client_ip = get_client_ip(&headers, Some(remote_addr));
    let mac = match get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            return (
                StatusCode::OK,
                [
                    ("content-type", "text/plain"),
                    ("access-control-allow-origin", "*"),
                ],
                "-1/-1".to_string(),
            );
        }
    };

    let sessions = state.sessions.lock().await;
    match sessions.get_session(&mac) {
        Some(session) if sessions.is_active(&mac) => {
            let used = session.used;
            let total = session.allotment;
            drop(sessions);
            (
                StatusCode::OK,
                [
                    ("content-type", "text/plain"),
                    ("access-control-allow-origin", "*"),
                ],
                format!("{used}/{total}"),
            )
        }
        _ => {
            drop(sessions);
            (
                StatusCode::OK,
                [
                    ("content-type", "text/plain"),
                    ("access-control-allow-origin", "*"),
                ],
                "-1/-1".to_string(),
            )
        }
    }
}

/// GET /session-state — the machine-readable tri-state (Go #541 parity):
/// `none` (never had a session), `active`, `expired` (had a session that
/// ran out; remembered past the record's removal). `/usage`'s `-1/-1`
/// cannot distinguish a first-time visitor from an expiring customer;
/// this can, so a portal can offer renewal.
///
/// Identity from the socket only (Go parity: the `?mac=` this route once
/// accepted let one client read another's state). An unidentifiable
/// client answers `none` with an empty mac field — the portal polls this
/// while rendering, so it must not error.
pub async fn handle_session_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let client_ip = get_client_ip(&headers, Some(remote_addr));
    let (mac, state_str) = match get_mac_address(&client_ip) {
        Some(mac) => {
            let sessions = state.sessions.lock().await;
            let state_str = sessions.session_state(&mac).as_str().to_string();
            (mac, state_str)
        }
        None => {
            tracing::warn!("MAC lookup failed for /session-state; answering none");
            (String::new(), "none".to_string())
        }
    };
    let body = serde_json::json!({
        "status": 1,
        "mac": mac,
        "state": state_str,
    });
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        body.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use crate::session::SessionManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn usage_returns_neg1_without_session() {
        let mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
        let session = mgr.lock().await;
        assert!(session.get_session("any").is_none());
        drop(session);
        // Without a session, the response would be "-1/-1"
        let expected = "-1/-1";
        assert_eq!(expected, "-1/-1");
    }

    #[tokio::test]
    async fn usage_returns_used_total_with_session() {
        let mut mgr = SessionManager::new();
        mgr.create_session("192.168.1.100", 5000, "bytes", 3600);
        {
            let s = mgr.sessions.get_mut("192.168.1.100").unwrap();
            s.used = 1500;
        }
        let session = mgr.get_session("192.168.1.100").unwrap();
        assert!(mgr.is_active("192.168.1.100"));
        let expected = format!("{}/{}", session.used, session.allotment);
        assert_eq!(expected, "1500/5000");
    }

    #[tokio::test]
    async fn usage_returns_neg1_for_expired_session() {
        let mut mgr = SessionManager::new();
        mgr.create_session("192.168.1.100", 5000, "bytes", 0);
        // Force expiry
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let s = mgr.sessions.get_mut("192.168.1.100").unwrap();
            s.expiry = now - 1;
        }
        assert!(!mgr.is_active("192.168.1.100"));
    }
}
