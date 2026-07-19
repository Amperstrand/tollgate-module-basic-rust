//! GET / — Nostr kind 10021 discovery event.

use crate::http::AppState;
use crate::nostr_event;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_discovery(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    let identity = &state.identity;

    let metric = if config.metric == "bytes" {
        "data"
    } else {
        "time"
    };
    let first_mint = config.accepted_mints.first();
    let price = first_mint
        .map(|m| m.price_per_step.to_string())
        .unwrap_or_default();
    let unit = first_mint.map(|m| m.price_unit.clone()).unwrap_or_default();
    let url = first_mint.map(|m| m.url.clone()).unwrap_or_default();
    let min_steps = first_mint
        .map(|m| m.min_purchase_steps.to_string())
        .unwrap_or_default();

    let tags = vec![
        vec!["metric".to_string(), metric.to_string()],
        vec!["step_size".to_string(), config.step_size.to_string()],
        vec!["price_per_step".to_string(), price.clone()],
        vec![
            "block".to_string(),
            "cashu".to_string(),
            price,
            unit,
            url,
            min_steps,
        ],
        vec!["tips".to_string()],
    ];

    let event = nostr_event::create_event(10021, tags, "", &identity.secret_key);
    let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}
