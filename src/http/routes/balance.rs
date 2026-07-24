//! GET /balance — session-state JSON mirroring Go's `balanceResponse`.
//!
//! Port of `tollgate-module-basic-go/src/main.go` `HandleBalance` (lines
//! 542–640). The Go struct:
//!
//! ```go
//! type balanceResponse struct {
//!     Status        int    `json:"status"`
//!     SessionActive bool   `json:"session_active"`
//!     Metric        string `json:"metric,omitempty"`
//!     Usage         uint64 `json:"usage"`
//!     Allotment     uint64 `json:"allotment"`
//!     Remaining     uint64 `json:"remaining"`
//!     StartTime     int64  `json:"start_time,omitempty"`
//!     Error         string `json:"error,omitempty"`
//! }
//! ```
//!
//! Thus `metric`, `start_time`, and `error` are OMITTED when empty/zero
//! (Go `omitempty`). The remaining fields (`status`, `session_active`,
//! `usage`, `allotment`, `remaining`) are ALWAYS present.
//!
//! Behaviour parity:
//! 1. Resolve client IP (Go `getIP`) → MAC (Go `getMacAddress`).
//! 2. If MAC resolution fails → HTTP 200 with
//!    `{status:1, session_active:false, usage:0, allotment:0, remaining:0}`.
//! 3. If MAC resolved but no active session → same shape as step 2.
//! 4. If MAC resolved and active session found → HTTP 200 with full
//!    session-state JSON including metric/start_time.

use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Serialize;
use std::net::SocketAddr;

#[derive(Debug, Serialize)]
struct BalanceResponse {
    status: i32,
    session_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    metric: Option<String>,
    usage: u64,
    allotment: u64,
    remaining: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl BalanceResponse {
    /// "No active session" shape — matches Go's
    /// `balanceResponse{Status: 1, SessionActive: false}` which, via
    /// `omitempty`, serialises to exactly:
    /// `{"status":1,"session_active":false,"usage":0,"allotment":0,"remaining":0}`.
    fn no_session() -> Self {
        BalanceResponse {
            status: 1,
            session_active: false,
            metric: None,
            usage: 0,
            allotment: 0,
            remaining: 0,
            start_time: None,
            error: None,
        }
    }
}

pub async fn handle_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    // Mirror Go's getIP + getMacAddress chain.
    let client_ip = get_client_ip(&headers, Some(remote_addr));
    let mac = get_mac_address(&client_ip);
    if mac.is_none() {
        // MAC lookup failed → graceful no-session response (HTTP 200),
        // matching Go's "MAC lookup failed" branch.
        return balance_json(StatusCode::OK, BalanceResponse::no_session());
    }
    let mac = mac.expect("checked None above");

    // Look up the session for this MAC.
    let sessions = state.sessions.lock().await;
    if !sessions.is_active(&mac) {
        // No active session → same no-session shape.
        return balance_json(StatusCode::OK, BalanceResponse::no_session());
    }

    let session = sessions
        .get_session(&mac)
        .expect("is_active returned true, session must exist");
    let used = session.used;
    let allotment = session.allotment;
    let remaining = allotment.saturating_sub(used);
    let metric = session.metric.clone();
    let start_time = session.granted_at as i64;
    drop(sessions);

    // Go encodes the active-session response with Status=1 (yes, really —
    // see main.go:631; the active-session branch also uses Status: 1).
    let resp = BalanceResponse {
        status: 1,
        session_active: true,
        metric: Some(metric),
        usage: used,
        allotment,
        remaining,
        start_time: Some(start_time),
        error: None,
    };
    balance_json(StatusCode::OK, resp)
}

/// Serialise `resp` and wrap with the standard headers + status code.
fn balance_json(
    status: StatusCode,
    resp: BalanceResponse,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    let body = serde_json::to_string(&resp).unwrap_or_else(|_| {
        // Last-resort fallback — should never trigger for this struct.
        r#"{"status":0,"session_active":false,"error":"serialization failed"}"#.to_string()
    });
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;

    #[test]
    fn no_session_response_matches_go_omitempty_shape() {
        // Go encodes this as:
        //   {"status":1,"session_active":false,"usage":0,"allotment":0,"remaining":0}
        // metric/start_time/error MUST be absent.
        let resp = BalanceResponse::no_session();
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let obj = parsed.as_object().expect("object");
        assert_eq!(obj.len(), 5, "exactly 5 fields, got {obj:?}");
        assert_eq!(obj["status"], 1);
        assert_eq!(obj["session_active"], false);
        assert_eq!(obj["usage"], 0);
        assert_eq!(obj["allotment"], 0);
        assert_eq!(obj["remaining"], 0);
        assert!(
            !obj.contains_key("metric"),
            "metric must be omitted when empty (Go omitempty parity)"
        );
        assert!(
            !obj.contains_key("start_time"),
            "start_time must be omitted when 0 (Go omitempty parity)"
        );
        assert!(
            !obj.contains_key("error"),
            "error must be omitted when empty (Go omitempty parity)"
        );
    }

    #[test]
    fn active_session_response_includes_metric_and_start_time() {
        let mut mgr = SessionManager::new();
        mgr.create_session("aa:bb:cc:dd:ee:ff", 5000, "milliseconds", 3600);
        {
            let s = mgr.sessions.get_mut("aa:bb:cc:dd:ee:ff").unwrap();
            s.used = 1500;
        }
        let session = mgr.get_session("aa:bb:cc:dd:ee:ff").unwrap();
        let used = session.used;
        let allotment = session.allotment;
        let remaining = allotment.saturating_sub(used);
        let resp = BalanceResponse {
            status: 1,
            session_active: true,
            metric: Some(session.metric.clone()),
            usage: used,
            allotment,
            remaining,
            start_time: Some(session.granted_at as i64),
            error: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let obj = parsed.as_object().expect("object");
        assert_eq!(obj["status"], 1);
        assert_eq!(obj["session_active"], true);
        assert_eq!(obj["metric"], "milliseconds");
        assert_eq!(obj["usage"], 1500);
        assert_eq!(obj["allotment"], 5000);
        assert_eq!(obj["remaining"], 3500);
        assert!(obj["start_time"].as_i64().unwrap_or(0) > 0);
        assert!(!obj.contains_key("error"));
    }

    #[test]
    fn remaining_clamps_to_zero_when_used_exceeds_allotment() {
        // saturating_sub guards against underflow (Go's `if allotment > used`
        // is equivalent to clamping to zero).
        let resp = BalanceResponse {
            status: 1,
            session_active: true,
            metric: Some("bytes".to_string()),
            usage: 8000,
            allotment: 5000,
            remaining: 5000u64.saturating_sub(8000),
            start_time: Some(1234),
            error: None,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["remaining"], 0);
        assert_eq!(parsed["usage"], 8000);
    }
}
