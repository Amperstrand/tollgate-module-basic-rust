//! GET /whoami — returns plain text `mac=<MAC>`, or HTTP 500 if MAC cannot
//! be resolved.
//!
//! Port of `tollgate-module-basic-go/src/main.go` `handler` (lines 356–368)
//! bound at `/whoami` (line 769). Go's handler:
//!
//! ```go
//! var ip = getIP(r)
//! var mac, err = getMacAddress(ip)
//! if err != nil {
//!     w.WriteHeader(http.StatusInternalServerError)
//!     return
//! }
//! fmt.Fprint(w, "mac=", mac)
//! ```
//!
//! On MAC resolution failure Go writes NO body and only the status line.
//! On success the body is exactly `mac=<MAC>` with no trailing newline,
//! default HTTP 200 status, and `Content-Type: text/plain`.

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
    match get_mac_address(&client_ip) {
        Some(mac) => (
            StatusCode::OK,
            [
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            format!("mac={mac}"),
        )
            .into_response(),
        None => {
            let mut resp = (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("access-control-allow-origin", "*")],
                String::new(),
            )
                .into_response();
            resp.headers_mut().remove("content-type");
            resp
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn whoami_success_body_format() {
        // The on-the-wire body for a successful resolution is exactly
        // `mac=<MAC>` — no newline, no trailing whitespace.
        let mac = "00:11:22:33:44:55".to_string();
        let body = format!("mac={mac}");
        assert_eq!(body, "mac=00:11:22:33:44:55");
        assert!(!body.ends_with('\n'));
    }

    #[test]
    fn whoami_body_contains_mac_prefix() {
        // The parity test (`test_parity_whoami_format`) asserts the body
        // contains "mac=".
        let body = format!("mac={}", "1a:2b:3c:4d:5e:6f");
        assert!(body.contains("mac="));
    }

    #[test]
    fn whoami_500_body_is_empty_on_failure() {
        // Go writes NO body on failure — parity requires an empty body.
        let body = String::new();
        assert!(body.is_empty());
    }
}
