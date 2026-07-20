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

pub async fn handle_balance(State(state): State<AppState>) -> impl IntoResponse {
    let wallet_guard = state.wallet.lock().await;
    let resp = if let Some(ref wallet) = *wallet_guard {
        match wallet.get_balance().await {
            Ok(balance) => {
                let mint_balances = wallet
                    .get_balance_by_mint()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(url, bal)| MintBalance { url, balance: bal })
                    .collect();
                BalanceResponse {
                    balance,
                    mint_balances,
                }
            }
            Err(e) => {
                tracing::error!("balance query failed: {e}");
                BalanceResponse {
                    balance: 0,
                    mint_balances: vec![],
                }
            }
        }
    } else {
        // Wallet not initialized
        BalanceResponse {
            balance: 0,
            mint_balances: vec![],
        }
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
