use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

const MAX_LIGHTNING_INVOICE_SATS: u64 = 1_000_000;
const DEVICE_UNRESOLVED_MESSAGE: &str = "We could not identify your device on the network. Reconnect to the TollGate Wi-Fi and try again.";
// The PRTA venue-detection contract keys on this exact string
// (loopback clients have no resolvable MAC → tests skip): renaming it
// broke the whole ln_invoice lane. Keep it stable.
const DEVICE_UNRESOLVED_CODE: &str = "mac-address-lookup-failed";

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default, alias = "mint")]
    pub mint_url: Option<String>,
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
    #[serde(skip_serializing_if = "String::is_empty", default)]
    code: String,
    #[serde(skip_serializing_if = "is_zero_u64", default)]
    retry_after: u64,
}

impl LightningInvoiceResponse {
    fn refusal(error: &str) -> LightningInvoiceResponse {
        LightningInvoiceResponse {
            status: 0,
            quote: String::new(),
            invoice: String::new(),
            mint_url: String::new(),
            amount: 0,
            expiry: 0,
            state: String::new(),
            access_granted: false,
            allotment: 0,
            metric: String::new(),
            error: error.to_string(),
            code: String::new(),
            retry_after: 0,
        }
    }

    fn with_code(mut self, code: &str) -> Self {
        self.code = code.to_string();
        self
    }
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
    // mint_url stays OPTIONAL (main's tested API contract: the PRTA suite
    // posts {"amount": N} and expects the first accepted mint — requiring
    // it broke backward compat). An explicit mint_url must match an
    // accepted mint; absent means the first accepted mint.
    let explicit_mint = req
        .mint_url
        .clone()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());

    if req.amount == 0 {
        return json_response(
            StatusCode::BAD_REQUEST,
            LightningInvoiceResponse::refusal("amount must be greater than 0"),
        );
    }

    let mint_cfg = match &explicit_mint {
        Some(url) => match state
            .config
            .accepted_mints
            .iter()
            .find(|m| crate::mint_url::mint_urls_equal(&m.url, url))
        {
            Some(m) => m.clone(),
            None => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    LightningInvoiceResponse::refusal("mint not accepted"),
                );
            }
        },
        None => match state.config.accepted_mints.first() {
            Some(m) => m.clone(),
            None => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    LightningInvoiceResponse::refusal("no accepted mints configured"),
                );
            }
        },
    };
    let mint_url = crate::wallet::canonical_mint_url(&mint_cfg.url);

    if req.amount > MAX_LIGHTNING_INVOICE_SATS {
        return json_response(
            StatusCode::BAD_REQUEST,
            LightningInvoiceResponse::refusal(
                "Amount too large: this TollGate accepts at most 1000000 sats per invoice.",
            )
            .with_code("amount-too-large"),
        );
    }

    // Store the canonical identity: a quote surviving a restart must match
    // the wallet map and config even if the configured spelling changes to
    // an equivalent alias (AGENTS.md mint-URL canonicalization).
    let mint_url = crate::wallet::canonical_mint_url(&mint_url);

    // AGENTS.md: validations possible locally run before any value moves.
    // An invoice below price_per_step/min_purchase_steps would settle to a
    // zero-step session after the mint already moved the paid value
    // (mirrors precheck_payment in pay.rs). Pricing comes from the
    // accepted-mint config matching the requested mint; a request naming
    // an unaccepted mint finds no config here and is refused later by the
    // wallet lookup, before any invoice exists.
    let accepted = state
        .config
        .accepted_mints
        .iter()
        .find(|m| crate::mint_url::mint_urls_equal(&m.url, &mint_url));
    let steps = if let Some(mint) = accepted {
        let price_per_step = mint.price_per_step.max(1);
        let min_steps = mint.min_purchase_steps.max(1);
        let steps = req.amount / price_per_step;
        if steps < min_steps {
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": format!(
                        "amount below minimum purchase: {steps} step(s) at {price_per_step} sat/step, minimum is {min_steps} step(s)"
                    ),
                    "steps": steps,
                    "min_steps": min_steps,
                    "price_per_step": price_per_step,
                }),
            );
        }
        steps
    } else {
        // No matching accepted-mint config: the wallet lookup below refuses
        // the request before any invoice exists.
        0
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
                LightningInvoiceResponse::refusal(DEVICE_UNRESOLVED_MESSAGE)
                    .with_code(DEVICE_UNRESOLVED_CODE),
            );
        }
    };

    let wallet_guard = state.wallet.read().await;
    let wallet = match wallet_guard.as_ref() {
        Some(w) => w,
        None => {
            return json_response(
                StatusCode::BAD_REQUEST,
                LightningInvoiceResponse::refusal("failed to create lightning invoice"),
            );
        }
    };

    match wallet.request_mint_quote(&mint_url, req.amount).await {
        Ok(info) => {
            let now = crate::lightning_quotes::now_secs();
            // Terms frozen at creation (0 = derive at settlement, used when
            // the mint carried no matching accepted-mint config).
            let allotment = steps * state.config.step_size;
            let record = crate::lightning_quotes::LightningQuoteRecord {
                quote: info.id.clone(),
                mint_url: mint_url.clone(),
                mac: mac.clone(),
                amount_sat: req.amount,
                created_at: now,
                expiry: info.expiry,
                minted: false,
                allotment,
                metric: state.config.metric.clone(),
                allotment_added: false,
                session_granted: false,
            };
            // AGENTS.md: never return a payable invoice whose record is
            // not durable — a restart would make it unsettleable.
            if let Err(e) = state.ln_quotes.upsert(record).await {
                tracing::error!(error = %e, "ln-invoice: quote record not durable; invoice withheld");
                let _ = state.ln_quotes.remove(&info.id).await;
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    LightningInvoiceResponse::refusal("failed to create lightning invoice"),
                );
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
                    code: String::new(),
                    retry_after: 0,
                },
            )
        }
        Err(e) => {
            tracing::warn!(error = ?e, "ln-invoice: mint quote failed");
            json_response(
                StatusCode::BAD_REQUEST,
                LightningInvoiceResponse::refusal("failed to create lightning invoice"),
            )
        }
    }
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::ConnectInfo(remote_addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Query(q): Query<InvoiceQuery>,
) -> Response {
    let quote_id = q.quote.trim().to_string();
    if quote_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            LightningInvoiceResponse::refusal("quote is required"),
        );
    }

    // The quote is bound to the device MAC at creation time; polling is only
    // answered for that same device (identity from the socket, never a query
    // parameter).
    let client_ip = crate::mac_resolver::get_client_ip(&headers, Some(remote_addr));
    let mac = match crate::mac_resolver::get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            return json_response(
                StatusCode::BAD_REQUEST,
                LightningInvoiceResponse::refusal(DEVICE_UNRESOLVED_MESSAGE)
                    .with_code(DEVICE_UNRESOLVED_CODE),
            );
        }
    };

    let stored = match state.ln_quotes.get(&quote_id).await {
        Some(s) if s.mac == mac => s,
        _ => {
            return json_response(
                StatusCode::BAD_REQUEST,
                LightningInvoiceResponse::refusal("failed to fetch invoice status"),
            );
        }
    };

    if stored.session_granted {
        // Terms frozen at creation; legacy records (allotment == 0) fall
        // back to current config pricing.
        let allotment = if stored.allotment > 0 {
            stored.allotment
        } else {
            let price_per_step =
                crate::lightning_quotes::find_mint_config(&state.config, &stored.mint_url)
                    .map(|m| m.price_per_step.max(1))
                    .unwrap_or(1);
            (stored.amount_sat / price_per_step) * state.config.step_size
        };
        let metric = if stored.metric.is_empty() {
            state.config.metric.clone()
        } else {
            stored.metric.clone()
        };
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
                metric,
                error: String::new(),
                code: String::new(),
                retry_after: 0,
            },
        );
    }

    let (state_str, expiry) = {
        let wallet_guard = state.wallet.read().await;
        match wallet_guard.as_ref() {
            Some(wallet) => match wallet
                .check_mint_quote_state(&stored.mint_url, &quote_id)
                .await
            {
                Ok(raw) => (
                    crate::lightning_quotes::quote_state_display(raw).to_string(),
                    stored.expiry,
                ),
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
            code: String::new(),
            retry_after: 0,
        },
    )
}
