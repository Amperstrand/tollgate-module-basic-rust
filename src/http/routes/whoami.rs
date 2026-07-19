//! GET /whoami — returns plain text `mac=AA:BB:CC:DD:EE:FF`.

use crate::http::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_whoami(State(_state): State<AppState>) -> impl IntoResponse {
    // Phase 1: placeholder MAC. Phase 4 sources from ARP/remote_addr.
    let mac = "00:00:00:00:00:00";
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        format!("mac={mac}"),
    )
}
