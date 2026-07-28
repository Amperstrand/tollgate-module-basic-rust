use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Serialize)]
struct LightningInvoiceResponse {
    status: u64,
    quote: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    invoice: String,
    #[serde(rename = "mint_url")]
    mint_url: String,
    amount: u64,
    #[serde(skip_serializing_if = "is_zero_u64", default)]
    expiry: u64,
    state: String,
    access_granted: bool,
    #[serde(skip_serializing_if = "is_zero_u64", default)]
    allotment: u64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    metric: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    error: String,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
}

#[derive(Debug, Clone)]
struct StoredQuote {
    mint_url: String,
    expiry: u64,
    created_at: u64,
}

type QuoteMap = std::sync::Mutex<HashMap<String, StoredQuote>>;

static QUOTE_STORE: OnceLock<QuoteMap> = OnceLock::new();

fn quote_store() -> &'static QuoteMap {
    QUOTE_STORE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn json_response(status: StatusCode, body: impl Serialize) -> Response {
    let json = serde_json::to_string(&body).unwrap_or_default();
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
        .into_response()
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> Response {
    if req.amount == 0 {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "amount must be greater than 0"}),
        );
    }

    let mint_url = match state.config.accepted_mints.first() {
        Some(m) => m.url.clone(),
        None => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "no accepted mints configured"}),
            );
        }
    };

    let wallet_guard = state.wallet.read().await;
    let wallet = match wallet_guard.as_ref() {
        Some(w) => w,
        None => {
            return json_response(
                StatusCode::OK,
                LightningInvoiceResponse {
                    status: 1,
                    quote: format!("stub-quote-{}", req.amount),
                    invoice: "stub-invoice".to_string(),
                    mint_url: mint_url.clone(),
                    amount: req.amount,
                    expiry: 0,
                    state: "unpaid".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: String::new(),
                },
            );
        }
    };

    match wallet.request_mint_quote(&mint_url, req.amount).await {
        Ok(info) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            {
                let mut store = quote_store().lock().unwrap_or_else(|e| e.into_inner());
                store.insert(
                    info.id.clone(),
                    StoredQuote {
                        mint_url: mint_url.clone(),
                        expiry: info.expiry,
                        created_at: now,
                    },
                );
                store.retain(|_, q| now - q.created_at < 1800);
            }

            json_response(
                StatusCode::OK,
                LightningInvoiceResponse {
                    status: 1,
                    quote: info.id,
                    invoice: info.request,
                    mint_url: mint_url.clone(),
                    amount: req.amount,
                    expiry: info.expiry,
                    state: "unpaid".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: String::new(),
                },
            )
        }
        Err(e) => {
            tracing::warn!(error = ?e, "ln-invoice: mint quote failed");
            json_response(
                StatusCode::OK,
                LightningInvoiceResponse {
                    status: 1,
                    quote: format!("stub-quote-{}", req.amount),
                    invoice: "stub-invoice".to_string(),
                    mint_url: mint_url.clone(),
                    amount: req.amount,
                    expiry: 0,
                    state: "unpaid".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: String::new(),
                },
            )
        }
    }
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> Response {
    let stored = {
        let store = quote_store().lock().unwrap_or_else(|e| e.into_inner());
        store.get(&q.quote).cloned()
    };

    let stored = match stored {
        Some(s) => s,
        None => {
            return json_response(
                StatusCode::NOT_FOUND,
                LightningInvoiceResponse {
                    status: 0,
                    quote: q.quote,
                    invoice: String::new(),
                    mint_url: String::new(),
                    amount: 0,
                    expiry: 0,
                    state: "unpaid".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: "quote not found".to_string(),
                },
            );
        }
    };

    let (state_str, expiry) = {
        let wallet_guard = state.wallet.read().await;
        match wallet_guard.as_ref() {
            Some(wallet) => match wallet.check_mint_quote(&stored.mint_url, &q.quote).await {
                Ok(raw) => {
                    let lower = raw.to_lowercase();
                    let is_paid = lower.contains("paid") || lower.contains("issued");
                    let s = if is_paid { "paid" } else { "unpaid" };
                    (s.to_string(), stored.expiry)
                }
                Err(_) => ("unpaid".to_string(), stored.expiry),
            },
            None => ("unpaid".to_string(), stored.expiry),
        }
    };

    json_response(
        StatusCode::OK,
        LightningInvoiceResponse {
            status: 1,
            quote: q.quote,
            invoice: String::new(),
            mint_url: stored.mint_url,
            amount: 0,
            expiry,
            state: state_str,
            access_granted: false,
            allotment: 0,
            metric: String::new(),
            error: String::new(),
        },
    )
}
