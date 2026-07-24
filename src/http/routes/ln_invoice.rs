//! POST /ln-invoice — create LN invoice via CDK mint quote (NUT-04).
//! GET /ln-invoice?quote=<id> — poll invoice status and grant session on payment.
//!
//! Uses a module-level quote store (OnceLock<Mutex<HashMap>>) since the
//! AppState struct cannot be extended without modifying main.rs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::OnceLock;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};

const QUOTE_EXPIRY_SECS: u64 = 30 * 60;

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub mint_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteRecord {
    quote_id: String,
    mac: String,
    mint_url: String,
    amount: u64,
    created_at: u64,
    invoice: String,
    expiry: u64,
}

static QUOTE_STORE: OnceLock<Mutex<HashMap<String, QuoteRecord>>> = OnceLock::new();

fn quote_store() -> &'static Mutex<HashMap<String, QuoteRecord>> {
    QUOTE_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn json_response(
    status: StatusCode,
    body: serde_json::Value,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        body.to_string(),
    )
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    if let Err(msg) = validate_amount(req.amount) {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"status": 0, "error": msg}),
        );
    }

    let mint_url = req.mint_url.clone().unwrap_or_else(|| {
        state
            .config
            .accepted_mints
            .first()
            .map(|m| m.url.clone())
            .unwrap_or_default()
    });
    if mint_url.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"status": 0, "error": "no mint_url provided and no accepted mints configured"}),
        );
    }

    let client_ip = get_client_ip(&headers, Some(remote_addr));
    let mac = match get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"status": 0, "error": "could not resolve client MAC address"}),
            );
        }
    };

    let quote_info = {
        let wallet_guard = state.wallet.lock().await;
        let wallet = match wallet_guard.as_ref() {
            Some(w) => w,
            None => {
                drop(wallet_guard);
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"status": 0, "error": "wallet not initialized"}),
                );
            }
        };
        match wallet.request_mint_quote(&mint_url, req.amount).await {
            Ok(info) => info,
            Err(e) => {
                drop(wallet_guard);
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"status": 0, "error": format!("mint quote failed: {e}")}),
                );
            }
        }
    };

    let record = QuoteRecord {
        quote_id: quote_info.id.clone(),
        mac,
        mint_url: mint_url.clone(),
        amount: req.amount,
        created_at: now_secs(),
        invoice: quote_info.request.clone(),
        expiry: quote_info.expiry,
    };

    quote_store()
        .lock()
        .await
        .insert(quote_info.id.clone(), record);

    let state_clone = state.clone();
    let quote_id = quote_info.id.clone();
    tokio::spawn(async move {
        monitor_quote(state_clone, quote_id).await;
    });

    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": 1,
            "quote": quote_info.id,
            "invoice": quote_info.request,
            "mint_url": mint_url,
            "amount": req.amount,
            "expiry": quote_info.expiry,
            "state": "unpaid",
        }),
    )
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    _headers: HeaderMap,
    _connect: ConnectInfo<SocketAddr>,
    Query(q): Query<InvoiceQuery>,
) -> impl IntoResponse {
    let record = match lookup_quote(&q.quote).await {
        Some(r) => r,
        None => {
            return json_response(
                StatusCode::NOT_FOUND,
                serde_json::json!({"status": 0, "error": "unknown quote id"}),
            );
        }
    };

    let cdk_state = {
        let wallet_guard = state.wallet.lock().await;
        let wallet = match wallet_guard.as_ref() {
            Some(w) => w,
            None => {
                drop(wallet_guard);
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"status": 0, "error": "wallet not initialized"}),
                );
            }
        };
        match wallet.check_mint_quote(&record.mint_url, &q.quote).await {
            Ok(s) => s,
            Err(e) => {
                drop(wallet_guard);
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"status": 0, "error": format!("quote check failed: {e}")}),
                );
            }
        }
    };

    let cdk_state_lower = cdk_state.to_ascii_lowercase();
    let paid = cdk_state_lower.contains("paid");
    let issued = cdk_state_lower.contains("issued");

    if paid || issued {
        if paid {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                if let Err(e) = wallet.mint_tokens(&record.mint_url, &q.quote).await {
                    tracing::warn!(error = %e, "mint_tokens failed for quote {}", q.quote);
                    drop(wallet_guard);
                    return json_response(
                        StatusCode::OK,
                        serde_json::json!({
                            "status": 1,
                            "quote": q.quote,
                            "state": cdk_state,
                            "access_granted": false,
                            "allotment": 0,
                            "metric": state.config.metric,
                        }),
                    );
                }
            }
        }

        let mint = state.config.accepted_mints.first();
        let price_per_step = mint.map(|m| m.price_per_step).unwrap_or(1).max(1);
        let step_size = state.config.step_size;
        let allotment = (record.amount / price_per_step) * step_size;

        let mut sessions = state.sessions.lock().await;
        sessions.create_session(&record.mac, allotment, &state.config.metric, 3600);
        drop(sessions);

        if let Err(e) = crate::valve::open_gate(&record.mac).await {
            tracing::warn!(mac = %record.mac, error = %e, "failed to open gate");
        }

        return json_response(
            StatusCode::OK,
            serde_json::json!({
                "status": 1,
                "quote": q.quote,
                "state": cdk_state,
                "access_granted": true,
                "allotment": allotment,
                "metric": state.config.metric,
            }),
        );
    }

    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": 1,
            "quote": q.quote,
            "state": cdk_state,
            "access_granted": false,
            "allotment": 0,
            "metric": state.config.metric,
        }),
    )
}

async fn monitor_quote(state: AppState, quote_id: String) {
    let mut backoff = 5u64;
    loop {
        let record = match lookup_quote(&quote_id).await {
            Some(r) => r,
            None => return,
        };

        if now_secs().saturating_sub(record.created_at) > QUOTE_EXPIRY_SECS {
            tracing::info!(quote = %quote_id, "quote expired, stopping monitor");
            return;
        }

        let cdk_state = {
            let wallet_guard = state.wallet.lock().await;
            let wallet = match wallet_guard.as_ref() {
                Some(w) => w,
                None => {
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            };
            match wallet.check_mint_quote(&record.mint_url, &quote_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "monitor: quote check failed");
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            }
        };

        let lower = cdk_state.to_ascii_lowercase();
        if lower.contains("paid") {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                if let Err(e) = wallet.mint_tokens(&record.mint_url, &quote_id).await {
                    tracing::warn!(error = %e, "monitor: mint_tokens failed");
                    drop(wallet_guard);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    continue;
                }
            }
            grant_session(&state, &record).await;
            return;
        }
        if lower.contains("issued") {
            grant_session(&state, &record).await;
            return;
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn grant_session(state: &AppState, record: &QuoteRecord) {
    let mint = state.config.accepted_mints.first();
    let price_per_step = mint.map(|m| m.price_per_step).unwrap_or(1).max(1);
    let step_size = state.config.step_size;
    let allotment = (record.amount / price_per_step) * step_size;

    let mut sessions = state.sessions.lock().await;
    sessions.create_session(&record.mac, allotment, &state.config.metric, 3600);
    drop(sessions);

    if let Err(e) = crate::valve::open_gate(&record.mac).await {
        tracing::warn!(mac = %record.mac, error = %e, "monitor: failed to open gate");
    }
    tracing::info!(mac = %record.mac, allotment, "monitor: session granted");
}

async fn lookup_quote(quote_id: &str) -> Option<QuoteRecord> {
    quote_store().lock().await.get(quote_id).cloned()
}

fn validate_amount(amount: u64) -> Result<(), &'static str> {
    if amount == 0 {
        Err("amount must be greater than 0")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ln_invoice_create_validates_amount() {
        assert!(validate_amount(0).is_err());
        assert!(validate_amount(1).is_ok());
        assert!(validate_amount(1000).is_ok());
    }

    #[tokio::test]
    async fn test_ln_invoice_status_unknown_quote() {
        assert!(lookup_quote("nonexistent-quote-id-12345").await.is_none());
    }

    #[tokio::test]
    async fn test_quote_store_roundtrip() {
        let record = QuoteRecord {
            quote_id: "test-quote-roundtrip".into(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            mint_url: "https://mint.example".into(),
            amount: 100,
            created_at: now_secs(),
            invoice: "lnbc1000n...".into(),
            expiry: 900,
        };
        quote_store()
            .lock()
            .await
            .insert("test-quote-roundtrip".into(), record.clone());
        let found = lookup_quote("test-quote-roundtrip").await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().quote_id, "test-quote-roundtrip");
    }
}
