use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
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
    headers: HeaderMap,
    axum::extract::ConnectInfo(remote_addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
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

    // The MAC is resolved at creation time and stored with the quote: the
    // monitor that later grants the session (possibly after a restart)
    // cannot see the original HTTP client anymore.
    let client_ip = crate::mac_resolver::get_client_ip(&headers, Some(remote_addr));
    let mac = match crate::mac_resolver::get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            return json_response(
                StatusCode::BAD_REQUEST,
                LightningInvoiceResponse {
                    status: 0,
                    quote: String::new(),
                    invoice: String::new(),
                    mint_url: String::new(),
                    amount: req.amount,
                    expiry: 0,
                    state: "error".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: "mac-address-lookup-failed".to_string(),
                },
            );
        }
    };

    let wallet_guard = state.wallet.read().await;
    let wallet = match wallet_guard.as_ref() {
        Some(w) => w,
        None => {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                LightningInvoiceResponse {
                    status: 0,
                    quote: String::new(),
                    invoice: String::new(),
                    mint_url,
                    amount: req.amount,
                    expiry: 0,
                    state: "error".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: "wallet not initialized".to_string(),
                },
            );
        }
    };

    match wallet.request_mint_quote(&mint_url, req.amount).await {
        Ok(info) => {
            let now = crate::lightning_quotes::now_secs();
            let record = crate::lightning_quotes::LightningQuoteRecord {
                quote: info.id.clone(),
                mint_url: mint_url.clone(),
                mac: mac.clone(),
                amount_sat: req.amount,
                created_at: now,
                expiry: info.expiry,
                minted: false,
                allotment_added: false,
                session_granted: false,
            };
            state.ln_quotes.upsert(record).await;

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
                StatusCode::BAD_GATEWAY,
                LightningInvoiceResponse {
                    status: 0,
                    quote: String::new(),
                    invoice: String::new(),
                    mint_url: mint_url.clone(),
                    amount: req.amount,
                    expiry: 0,
                    state: "error".to_string(),
                    access_granted: false,
                    allotment: 0,
                    metric: String::new(),
                    error: format!("mint quote failed: {e}"),
                },
            )
        }
    }
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> Response {
    let stored = state.ln_quotes.get(&q.quote).await;

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

    if stored.session_granted {
        let price_per_step = state
            .config
            .accepted_mints
            .iter()
            .find(|m| m.url.trim_end_matches('/') == stored.mint_url.trim_end_matches('/'))
            .map(|m| m.price_per_step.max(1))
            .unwrap_or(1);
        let allotment = (stored.amount_sat / price_per_step) * state.config.step_size;
        return json_response(
            StatusCode::OK,
            LightningInvoiceResponse {
                status: 1,
                quote: stored.quote,
                invoice: String::new(),
                mint_url: stored.mint_url,
                amount: stored.amount_sat,
                expiry: stored.expiry,
                state: "paid".to_string(),
                access_granted: true,
                allotment,
                metric: state.config.metric.clone(),
                error: String::new(),
            },
        );
    }

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
            quote: stored.quote,
            invoice: String::new(),
            mint_url: stored.mint_url,
            amount: stored.amount_sat,
            expiry,
            state: state_str,
            access_granted: false,
            allotment: 0,
            metric: String::new(),
            error: String::new(),
        },
    )
}
