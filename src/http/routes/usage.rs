//! GET /usage — returns plain text "used/total" or "-1/-1".

use crate::http::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_usage(State(_state): State<AppState>) -> impl IntoResponse {
    // Phase 1: no session state. Phase 4 wires to ndsctl.
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        "-1/-1",
    )
}
