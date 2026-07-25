use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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

#[derive(Debug, Clone)]
struct QuoteRecord {
    created_at: u64,
    amount: u64,
    mint_url: String,
    expiry: u64,
}

type QuoteStore = std::sync::Mutex<std::collections::HashMap<String, QuoteRecord>>;

static QUOTE_STORE: std::sync::OnceLock<QuoteStore> = std::sync::OnceLock::new();

fn quote_store() -> &'static QuoteStore {
    QUOTE_STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
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
                serde_json::json!({"error": "no mint configured"}),
            );
        }
    };

    let wallet_guard = state.wallet.lock().await;
    let quote_info = match wallet_guard.as_ref() {
        Some(wallet) => match wallet.request_mint_quote(&mint_url, req.amount).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "failed to create mint quote");
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": format!("failed to create mint quote: {e}")}),
                );
            }
        },
        None => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"error": "wallet not initialized"}),
            );
        }
    };
    drop(wallet_guard);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    {
        let mut store = quote_store().lock().unwrap();
        store.insert(
            quote_info.id.clone(),
            QuoteRecord {
                created_at: now,
                amount: req.amount,
                mint_url: mint_url.clone(),
                expiry: quote_info.expiry,
            },
        );
        store.retain(|_, rec| now - rec.created_at < 1800);
    }

    json_response(
        StatusCode::OK,
        InvoiceResponse {
            quote: quote_info.id,
            request: quote_info.request,
            pubkey: mint_url,
        },
    )
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> Response {
    let record = {
        let store = quote_store().lock().unwrap();
        store.get(&q.quote).cloned()
    };

    let record = match record {
        Some(r) => r,
        None => {
            return json_response(
                StatusCode::NOT_FOUND,
                serde_json::json!({"error": "quote not found"}),
            );
        }
    };

    let (state_str, check_state_str) = {
        let wallet_guard = state.wallet.lock().await;
        match wallet_guard.as_ref() {
            Some(wallet) => {
                match wallet.check_mint_quote(&record.mint_url, &q.quote).await {
                    Ok(raw) => {
                        let lower = raw.to_lowercase();
                        let s = if lower.contains("paid") || lower.contains("issued") { "paid" } else { "unpaid" };
                        let cs = if lower.contains("paid") || lower.contains("issued") { "PAID" } else { "UNPAID" };
                        (s.to_string(), cs.to_string())
                    }
                    Err(_) => ("unpaid".to_string(), "UNPAID".to_string()),
                }
            }
            None => ("unpaid".to_string(), "UNPAID".to_string()),
        }
    };

    json_response(
        StatusCode::OK,
        InvoiceStatus {
            quote: q.quote,
            state: state_str,
            check_state: check_state_str,
            expiry: record.expiry,
        },
    )
}
