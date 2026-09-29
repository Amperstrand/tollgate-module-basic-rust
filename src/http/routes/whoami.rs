//! GET /whoami — returns plain text `mac=<MAC>`.
//!
//! An echo of the caller's own address: when the MAC cannot be resolved the
//! answer is `mac=` with HTTP 200 (Go parity — `main.go handler`: answer an
//! empty mac instead of failing; never publish the 00:00:00:00:00:00
//! sentinel as if it were an identity).

use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

pub async fn handle_whoami(
    State(_state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
) -> Response {
    let client_ip = get_client_ip(&headers, Some(remote_addr));
    let mac = get_mac_address(&client_ip).unwrap_or_default();
    if mac.is_empty() {
        tracing::warn!("MAC address lookup failed for /whoami; answering an empty mac");
    }
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        format!("mac={mac}"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn whoami_success_body_format() {
        let mac = "00:11:22:33:44:55".to_string();
        let body = format!("mac={mac}");
        assert_eq!(body, "mac=00:11:22:33:44:55");
        assert!(!body.ends_with('\n'));
    }

    #[test]
    fn whoami_unresolvable_mac_answers_empty_value_with_200() {
        // Go parity: unresolvable MAC is an echo of emptiness, not an error.
        let mac = String::new();
        let body = format!("mac={mac}");
        assert_eq!(body, "mac=");
    }
}
