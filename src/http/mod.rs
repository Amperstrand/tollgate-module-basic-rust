//! HTTP server module — axum router with all tollgate routes.

pub mod routes;

use axum::extract::DefaultBodyLimit;
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

/// Shared application state passed to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<crate::config::Config>,
    pub identity: Arc<crate::identity::MerchantIdentity>,
    pub wallet: Arc<tokio::sync::RwLock<Option<crate::wallet::wallet::TollWallet>>>,
    pub sessions: Arc<tokio::sync::Mutex<crate::session::SessionManager>>,
    pub portal: Arc<dyn crate::portal::CaptivePortal>,
    /// Shared token verifier — reuses the internal reqwest::Client across
    /// all payment requests instead of constructing one per POST /.
    pub verifier: Arc<crate::wallet::verify::TokenVerifier>,
    pub rate_limiter: Arc<crate::rate_limiter::RateLimiter>,
}

/// Build the main HTTP router with all routes.
pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route(
            "/",
            axum::routing::get(routes::discovery::handle_discovery).post(routes::pay::handle_pay),
        )
        .route("/whoami", axum::routing::get(routes::whoami::handle_whoami))
        .route("/usage", axum::routing::get(routes::usage::handle_usage))
        .route(
            "/balance",
            axum::routing::get(routes::balance::handle_balance),
        )
        .route(
            "/ln-invoice",
            axum::routing::post(routes::ln_invoice::handle_create_ln_invoice)
                .get(routes::ln_invoice::handle_get_ln_invoice),
        )
        .layer(cors)
        .layer(DefaultBodyLimit::max(1_000_000))
        .with_state(state)
}
