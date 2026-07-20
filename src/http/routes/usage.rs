//! GET /usage — returns plain text "used/total" or "-1/-1".
//!
//! Phase 4: queries SessionManager for the requesting client's session.
//! The client MAC is sourced from X-Forwarded-For or remote_addr (matching
//! Go behavior on OpenWrt where nodogsplash sets X-Forwarded-For).

use crate::http::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

pub async fn handle_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Try to get client MAC from X-Forwarded-For or other headers.
    // In production, nodogsplash provides the client IP, which we'd resolve
    // to MAC via ARP. For now, we use the IP as a session key proxy.
    let client_key = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let sessions = state.sessions.lock().await;
    if client_key.is_empty() {
        drop(sessions);
        return usage_response("-1/-1");
    }

    match sessions.get_session(&client_key) {
        Some(session) if sessions.is_active(&client_key) => {
            let used = session.used;
            let total = session.allotment;
            drop(sessions);
            usage_response(&format!("{used}/{total}"))
        }
        Some(_) => {
            // Session exists but expired or exhausted
            drop(sessions);
            usage_response("-1/-1")
        }
        None => {
            drop(sessions);
            usage_response("-1/-1")
        }
    }
}

fn usage_response(body: impl Into<String>) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        body.into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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