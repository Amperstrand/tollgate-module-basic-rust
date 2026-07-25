//! GET / — Nostr kind 10021 discovery event.

use crate::http::AppState;
use crate::nostr_event;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_discovery(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    let identity = &state.identity;

    let first_mint = config.accepted_mints.first();
    let price = first_mint
        .map(|m| m.price_per_step.to_string())
        .unwrap_or_default();
    let unit = first_mint.map(|m| m.price_unit.clone()).unwrap_or_default();
    let url = first_mint.map(|m| m.url.clone()).unwrap_or_default();
    let min_steps = first_mint
        .map(|m| m.min_purchase_steps.to_string())
        .unwrap_or_default();

    // Tag layout MUST match the Go binary's CreateAdvertisement
    // (merchant.go:CreateAdvertisement). See PARITY test
    // test_parity_discovery_tag_names.
    let tags = vec![
        vec!["metric".to_string(), config.metric.clone()],
        vec!["step_size".to_string(), config.step_size.to_string()],
        vec!["tips".to_string(), "1".to_string(), "2".to_string()],
        vec![
            "price_per_step".to_string(),
            "cashu".to_string(),
            price,
            unit,
            url,
            min_steps,
        ],
    ];

    let event = nostr_event::create_event(10021, tags, "", &identity.secret_key);
    let json = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}
