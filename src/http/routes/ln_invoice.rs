//! POST /ln-invoice — create LN invoice (stub)
//! GET /ln-invoice?quote=<id> — poll invoice status (stub)

use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
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

#[derive(Debug, Clone)]
struct QuoteRecord {
    created_at: u64,
    amount: u64,
}

type QuoteStore = std::sync::Mutex<std::collections::HashMap<String, QuoteRecord>>;

static QUOTE_STORE: std::sync::OnceLock<QuoteStore> = std::sync::OnceLock::new();

fn quote_store() -> &'static QuoteStore {
    QUOTE_STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub async fn handle_create_ln_invoice(
    State(_state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let quote_id = format!("stub-quote-{}", req.amount);

    // Insert new quote and clean up old ones
    {
        let mut store = quote_store().lock().unwrap();
        store.insert(
            quote_id.clone(),
            QuoteRecord {
                created_at: now,
                amount: req.amount,
            },
        );

        // Remove quotes older than 30 minutes (1800 seconds)
        store.retain(|_, rec| now - rec.created_at < 1800);
    }

    let resp = InvoiceResponse {
        quote: quote_id,
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
