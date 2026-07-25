//! POST /ln-invoice — create LN invoice (stub)
//! GET /ln-invoice?quote=<id> — poll invoice status (stub)

use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Serialize)]
struct InvoiceResponse {
    quote: String,
    request: String,
    pubkey: String,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
}

#[derive(Debug, Serialize)]
struct InvoiceStatus {
    quote: String,
    state: String,
    #[serde(rename = "checkState")]
    check_state: String,
    expiry: u64,
}

pub async fn handle_create_ln_invoice(
    State(_state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    let resp = InvoiceResponse {
        quote: format!("stub-quote-{}", req.amount),
        request: "stub-invoice".to_string(),
        pubkey: "stub-pubkey".to_string(),
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

pub async fn handle_get_ln_invoice(
    State(_state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> impl IntoResponse {
    let resp = InvoiceStatus {
        quote: q.quote,
        state: "unpaid".to_string(),
        check_state: "UNPAID".to_string(),
        expiry: 0,
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
