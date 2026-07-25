//! POST /ln-invoice — create LN invoice
//! GET /ln-invoice?quote=<id> — poll invoice status

use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

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

#[derive(Debug, Serialize)]
struct InvoiceError {
    error: String,
}

#[derive(Debug, Clone)]
struct QuoteRecord {
    created_at: u64,
    amount: u64,
    mint_url: String,
    expiry: u64,
}

type QuoteStore = Mutex<std::collections::HashMap<String, QuoteRecord>>;

lazy_static::lazy_static! {
    static ref QUOTE_STORE: QuoteStore = Mutex::new(std::collections::HashMap::new());
}

type JsonResponse = (
    StatusCode,
    [(&'static str, &'static str); 2],
    axum::Json<serde_json::Value>,
);

fn error_response(status: StatusCode, message: &str) -> JsonResponse {
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        axum::Json(serde_json::json!({ "error": message })),
    )
}

fn json_response<T: Serialize>(status: StatusCode, data: T) -> JsonResponse {
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        axum::Json(serde_json::to_value(data).unwrap_or_default()),
    )
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    if req.amount == 0 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "amount must be greater than 0",
        );
    }

    let mint_url = match state.config.accepted_mints.first() {
        Some(mint) => mint.url.clone(),
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "no mint configured",
            );
        }
    };

    let wallet = state.wallet.lock().await;
    let wallet_ref = match wallet.as_ref() {
        Some(w) => w,
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "wallet not initialized",
            );
        }
    };

    let quote_info = match wallet_ref.request_mint_quote(&mint_url, req.amount).await {
        Ok(info) => info,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to request mint quote: {}", e),
            );
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    {
        let mut store = QUOTE_STORE.lock().unwrap();
        store.insert(
            quote_info.id.clone(),
            QuoteRecord {
                created_at: now,
                amount: quote_info.amount,
                mint_url: mint_url.clone(),
                expiry: quote_info.expiry,
            },
        );

        store.retain(|_, rec| now - rec.created_at < 1800);
    }

    let resp = InvoiceResponse {
        quote: quote_info.id,
        request: quote_info.request,
        pubkey: mint_url,
    };
    json_response(StatusCode::OK, resp)
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> Response {
    let quote_record = QUOTE_STORE.lock().unwrap().get(&q.quote).cloned();
    let quote_record = match quote_record {
        Some(record) => record,
        None => {
            return error_response(StatusCode::NOT_FOUND, "quote not found").into_response();
        }
    };

    let wallet_guard = state.wallet.lock().await;
    let (state_str, check_state_str) = match wallet_guard.as_ref() {
        Some(wallet) => match wallet.check_mint_quote(&quote_record.mint_url, &q.quote).await {
            Ok(raw_state) => {
                let lower = raw_state.to_lowercase();
                let (s, cs) = match lower.as_str() {
                    "paid" | "issued" => ("paid", "PAID"),
                    "pending" => ("pending", "PENDING"),
                    _ => ("unpaid", "UNPAID"),
                };
                (s.to_string(), cs.to_string())
            }
            Err(e) => {
                tracing::warn!(
                    quote = %q.quote,
                    error = %e,
                    "failed to check mint quote status — defaulting to unpaid"
                );
                ("unpaid".to_string(), "UNPAID".to_string())
            }
        },
        None => ("unpaid".to_string(), "UNPAID".to_string()),
    };

    json_response(
        StatusCode::OK,
        InvoiceStatus {
            quote: q.quote,
            state: state_str,
            check_state: check_state_str,
            expiry: quote_record.expiry,
        },
    )
    .into_response()
}
