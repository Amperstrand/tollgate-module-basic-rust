//! Unix socket CLI server.
//!
//! Listens on /var/run/tollgate.sock (or TOLLGATE_TEST_CONFIG_DIR/tollgate.sock).
//! Mode 0660. Line-delimited JSON request/response.
//!
//! Commands: version, status, "wallet info", "wallet balance"

use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::http::AppState;

/// Socket path — honors TOLLGATE_TEST_CONFIG_DIR for tests.
pub fn socket_path() -> PathBuf {
    std::env::var("TOLLGATE_TEST_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("tollgate.sock"))
        .unwrap_or_else(|_| PathBuf::from("/var/run/tollgate.sock"))
}

/// Version info returned by the `version` command.
pub fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_COMMIT").unwrap_or("0000000");
    let build_time = option_env!("BUILD_TIME").unwrap_or("unknown");
    let rust_version = option_env!("RUSTC_VERSION").unwrap_or("unknown");

    format!(
        "version: {version}\n\
         commit: {commit}\n\
         build_time: {build_time}\n\
         rust_version: {rust_version}\n\
         openwrt: target={arch}\n",
        arch = std::env::consts::ARCH
    )
}

/// Start the Unix socket CLI server with shared AppState.
pub async fn serve(state: Arc<AppState>) -> std::io::Result<()> {
    let path = socket_path();

    // Remove stale socket
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    // Create parent dir if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&path)?;

    // Set mode 0660
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    }

    tracing::info!(socket = %path.display(), "CLI Unix socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(handle_connection(stream, state));
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed on CLI socket");
            }
        }
    }
}

async fn handle_connection(stream: tokio::net::UnixStream, state: Arc<AppState>) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let cmd = line.trim();
                let response = handle_command(cmd, &state).await;
                if let Err(e) = writer.write_all(response.as_bytes()).await {
                    tracing::warn!(error = %e, "write failed on CLI socket");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "read failed on CLI socket");
                break;
            }
        }
    }
}

async fn handle_command(cmd: &str, state: &AppState) -> String {
    match cmd {
        "version" => version_string(),
        "status" => {
            serde_json::json!({
                "success": true,
                "message": "running"
            })
            .to_string()
                + "\n"
        }
        "wallet info" => {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                let balances = wallet.get_balance_by_mint().await.unwrap_or_default();
                let mints: Vec<serde_json::Value> = balances
                    .iter()
                    .map(|(url, bal)| {
                        serde_json::json!({"url": url, "balance": bal})
                    })
                    .collect();
                drop(wallet_guard);
                serde_json::json!({
                    "success": true,
                    "message": serde_json::to_string(&mints).unwrap_or_default()
                })
                .to_string()
                    + "\n"
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "no wallet configured"
                })
                .to_string()
                    + "\n"
            }
        }
        "wallet balance" => {
            let wallet_guard = state.wallet.lock().await;
            if let Some(ref wallet) = *wallet_guard {
                match wallet.get_balance().await {
                    Ok(balance) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": true,
                            "message": balance.to_string()
                        })
                        .to_string()
                            + "\n"
                    }
                    Err(e) => {
                        drop(wallet_guard);
                        serde_json::json!({
                            "success": false,
                            "error": format!("wallet error: {e}")
                        })
                        .to_string()
                            + "\n"
                    }
                }
            } else {
                serde_json::json!({
                    "success": true,
                    "message": "0"
                })
                .to_string()
                    + "\n"
            }
        }
        _ => {
            serde_json::json!({
                "success": false,
                "error": format!("unknown command: {cmd}")
            })
            .to_string()
                + "\n"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::identity::MerchantIdentity;
    use crate::session::SessionManager;
    use crate::wallet::TollWallet;

    fn make_test_state() -> Arc<AppState> {
        let config = Arc::new(Config::new_default());
        let identity = Arc::new(MerchantIdentity::load_or_generate().unwrap());
        let wallet = Arc::new(tokio::sync::Mutex::new(Some(
            TollWallet::new([0u8; 64], vec![], std::path::PathBuf::from("/tmp")),
        )));
        let sessions = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
        Arc::new(AppState {
            config,
            identity,
            wallet,
            sessions,
        })
    }

    #[tokio::test]
    async fn version_contains_required_fields() {
        let v = version_string();
        assert!(v.contains("version:"));
        assert!(v.contains("commit:"));
        assert!(v.contains("build_time:"));
        assert!(v.contains("rust_version:"));
        assert!(v.contains("openwrt"));
    }

    #[tokio::test]
    async fn status_returns_running() {
        let state = make_test_state();
        let resp = handle_command("status", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "running");
    }

    #[tokio::test]
    async fn wallet_balance_returns_zero_for_empty_wallet() {
        let state = make_test_state();
        let resp = handle_command("wallet balance", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        // Balance will be "0" since the wallet has no mints registered
        assert_eq!(json["message"], "0");
    }

    #[tokio::test]
    async fn wallet_info_returns_json() {
        let state = make_test_state();
        let resp = handle_command("wallet info", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].is_string());
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let state = make_test_state();
        let resp = handle_command("foobar", &state).await;
        let json: serde_json::Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("unknown command"));
    }
}