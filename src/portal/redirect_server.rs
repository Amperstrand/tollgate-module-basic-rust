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

    let gateway = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<title>TollGate Portal</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:480px;margin:40px auto;padding:0 16px;color:#1a1a1a}}
h1{{font-size:1.5rem}}
textarea{{width:100%;height:80px;box-sizing:border-box;margin:8px 0}}
button{{padding:12px 24px;font-size:1rem;cursor:pointer;border:none;border-radius:8px;background:#0066cc;color:#fff}}
.status{{margin-top:16px;padding:12px;border-radius:8px;display:none}}
.ok{{background:#d4edda;color:#155724}}
.err{{background:#f8d7da;color:#721c24}}
</style>
</head>
<body>
<h1>TollGate Portal</h1>
<p>Internet access requires payment. Paste a Cashu token below.</p>
<textarea id="token" placeholder="cashuB..."></textarea>
<br/>
<button onclick="pay()">Pay &amp; Connect</button>
<div id="status" class="status"></div>
<script>
async function pay(){{
  const t=document.getElementById('token').value.trim();
  if(!t){{alert('Paste a Cashu token first');return;}}
  const s=document.getElementById('status');
  s.style.display='block';s.className='status';s.textContent='Processing...';
  try{{
    const r=await fetch('http://{gateway}:2121/',{{method:'POST',headers:{{'Content-Type':'text/plain'}},body:t}});
    if(r.status===200){{s.className='status ok';s.textContent='Payment accepted! You are online.';setTimeout(()=>window.location.reload(),2000);}}
    else{{const j=await r.json();s.className='status err';s.textContent='Payment failed: '+(j.content||r.status);}}
  }}catch(e){{s.className='status err';s.textContent='Error: '+e.message;}}
}}
</script>
</body>
</html>"#
    );

    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("access-control-allow-origin", "*"),
            ("cache-control", "no-cache, no-store, must-revalidate"),
        ],
        html,
    )
        .into_response()
}

pub fn create_redirect_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::any(handle_port80))
        .fallback(handle_port80)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn gateway_extraction_with_port() {
        let host = "192.168.1.1:80";
        let gateway = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        assert_eq!(gateway, "192.168.1.1");
    }

    #[test]
    fn gateway_extraction_without_port() {
        let host = "tollgate.local";
        let gateway = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        assert_eq!(gateway, "tollgate.local");
    }
}
