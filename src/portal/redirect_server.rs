use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

use crate::http::AppState;
use crate::mac_resolver::get_mac_address;

pub async fn handle_port80(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    _uri: Uri,
) -> Response {
    let client_ip = crate::mac_resolver::get_client_ip(&headers, Some(remote_addr));

    if let Some(mac) = get_mac_address(&client_ip) {
        if state.portal.is_authenticated(&mac).await {
            return StatusCode::NO_CONTENT.into_response();
        }
    }

    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("gateway");

    let gateway = host.split(':').next().unwrap_or("gateway");
    let redirect_url = format!("http://{gateway}:2121/");

    (StatusCode::FOUND, [("location", redirect_url.as_str())]).into_response()
}

pub fn create_redirect_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route(
            "/",
            axum::routing::any(handle_port80).fallback(handle_port80),
        )
        .fallback(handle_port80)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn redirect_url_format() {
        let host = "192.168.1.1:80";
        let gateway = host.split(':').next().unwrap_or("gateway");
        let url = format!("http://{gateway}:2121/");
        assert_eq!(url, "http://192.168.1.1:2121/");
    }

    #[test]
    fn redirect_url_no_port_in_host() {
        let host = "tollgate.local";
        let gateway = host.split(':').next().unwrap_or("gateway");
        let url = format!("http://{gateway}:2121/");
        assert_eq!(url, "http://tollgate.local:2121/");
    }
}
