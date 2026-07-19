//! GET /balance — wallet balance in Go schema.

use crate::http::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct BalanceResponse {
    balance: u64,
    #[serde(rename = "mintBalances")]
    mint_balances: Vec<MintBalance>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MintBalance {
    url: String,
    balance: u64,
}

pub async fn handle_balance(State(_state): State<AppState>) -> impl IntoResponse {
    // Phase 1 stub: zero balance. Phase 3 wires to CDK.
    let resp = BalanceResponse {
        balance: 0,
        mint_balances: vec![],
    };
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
