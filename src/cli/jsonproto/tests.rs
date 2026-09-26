//! Tests for the Go-compatible JSON command protocol.

use super::*;
use crate::config::Config;
use crate::identity::MerchantIdentity;
use crate::portal::{CaptivePortal, NdsPortal};
use crate::session::SessionManager;
use crate::wallet::TollWallet;
use std::sync::Arc;

fn make_test_state() -> Arc<AppState> {
    let config = Arc::new(Config::new_default());

    let secp = secp256k1::Secp256k1::new();
    let (secret_key, _) = secp.generate_keypair(&mut rand::thread_rng());
    let identity = Arc::new(MerchantIdentity {
        name: "merchant".to_string(),
        secret_key,
    });

    let wallet = Arc::new(tokio::sync::RwLock::new(Some(TollWallet::new(
        [0u8; 64],
        vec![],
        std::path::PathBuf::from("/tmp"),
    ))));
    let sessions = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
    let portal: Arc<dyn CaptivePortal> = Arc::new(NdsPortal::new());
    let verifier = Arc::new(crate::wallet::verify::TokenVerifier::new(vec![]));
    let rate_limiter = Arc::new(crate::rate_limiter::RateLimiter::new(1000));
    let ln_quotes = Arc::new(crate::lightning_quotes::QuoteStore::load(
        std::path::Path::new("/tmp"),
    ));
    Arc::new(AppState {
        config,
        identity,
        wallet,
        sessions,
        portal,
        verifier,
        rate_limiter,
        ln_quotes,
    })
}

async fn dispatch_line(line: &str, state: &AppState) -> serde_json::Value {
    let out = handle_json_line(line, state).await;
    assert!(
        out.ends_with('\n'),
        "response must be one newline-terminated line"
    );
    serde_json::from_str(out.trim()).expect("response must be JSON")
}

fn uci_absent() -> bool {
    which_absent("uci")
}

fn which_absent(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).all(|dir| !dir.join(bin).exists()))
        .unwrap_or(true)
}

#[tokio::test]
async fn version_returns_go_shape() {
    let state = make_test_state();
    let resp = dispatch_line(
        r#"{"command":"version","args":[],"timestamp":"2026-01-01T00:00:00Z"}"#,
        &state,
    )
    .await;
    assert_eq!(resp["success"], true);
    assert!(resp["message"].as_str().unwrap().contains("version:"));
    assert_eq!(resp["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(resp["data"]["rust_version"].is_string());
    assert!(resp["data"]["openwrt_version"].is_string());
    assert!(resp["timestamp"].as_f64().is_some());
}

#[tokio::test]
async fn status_returns_service_status_fields() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"status"}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["message"], "Service status retrieved");
    assert_eq!(resp["data"]["running"], true);
    assert_eq!(resp["data"]["config_ok"], true);
    assert_eq!(resp["data"]["wallet_ok"], true);
    assert_eq!(
        resp["data"]["version"],
        format!("TollGate {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(resp["data"]["uptime"]
        .as_str()
        .is_some_and(|u| u.ends_with('s')));
}

#[tokio::test]
async fn health_returns_go_shape() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"health"}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["message"], "healthy");
    assert_eq!(resp["data"]["status"], "ok");
    assert_eq!(resp["data"]["wallet_ok"], true);
}

#[tokio::test]
async fn wallet_balance_returns_balance_sats() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"wallet","args":["balance"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["balance_sats"], 0);
    assert_eq!(resp["message"], "Total wallet balance: 0 sats");
}

#[tokio::test]
async fn wallet_info_returns_total_and_mints() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"wallet","args":["info"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["total_balance"], 0);
    assert_eq!(resp["data"]["mint_count"], 0);
    assert_eq!(resp["data"]["mint_balances"], serde_json::json!({}));
}

#[tokio::test]
async fn wallet_subcommand_errors_match_go() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"wallet"}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Wallet command requires an action (drain, balance, info, fund)"
    );

    let resp = dispatch_line(r#"{"command":"wallet","args":["frobnicate"]}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Unknown wallet action: frobnicate (supported: drain, balance, info, fund)"
    );

    let resp = dispatch_line(r#"{"command":"wallet","args":["drain"]}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Drain command requires a type: 'cashu' (lightning not yet supported)"
    );

    let resp = dispatch_line(
        r#"{"command":"wallet","args":["drain","lightning"]}"#,
        &state,
    )
    .await;
    assert_eq!(resp["error"], "Lightning drain not yet implemented");
}

#[tokio::test]
async fn unknown_command_matches_go_error() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"frobnicate"}"#, &state).await;
    assert_eq!(resp["success"], false);
    assert_eq!(resp["error"], "Unknown command: frobnicate");
}

#[tokio::test]
async fn invalid_json_line_returns_error_response() {
    let state = make_test_state();
    let out = handle_json_line("{not json", &state).await;
    let resp: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().starts_with("Invalid JSON"));
}

#[tokio::test]
async fn config_get_returns_config_and_identities() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"config","args":["get"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["message"], "Configuration retrieved");
    assert_eq!(resp["data"]["config"]["metric"], "bytes");
    assert!(resp["data"]["identities"].is_object());
}

#[tokio::test]
async fn config_get_key_returns_key_value_data() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"config","args":["get","metric"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"], serde_json::json!({ "metric": "bytes" }));

    let resp = dispatch_line(r#"{"command":"config","args":["get","nope"]}"#, &state).await;
    assert_eq!(resp["success"], false);
    assert_eq!(resp["error"], "unknown config key: nope");
}

#[tokio::test]
async fn config_schema_returns_field_tables() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"config","args":["schema"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["message"], "Configuration schema");
    let config_schema = resp["data"]["config"].as_array().unwrap();
    let json_keys: Vec<&str> = config_schema
        .iter()
        .filter_map(|f| f["json_key"].as_str())
        .collect();
    for expected in [
        "config_version",
        "metric",
        "step_size",
        "accepted_mints",
        "profit_share",
        "upstream_wifi",
    ] {
        assert!(json_keys.contains(&expected), "missing {expected}");
    }
    let identities = resp["data"]["identities"].as_array().unwrap();
    assert!(identities
        .iter()
        .any(|f| f["json_key"] == "owned_identities"));
}

#[tokio::test]
#[serial_test::serial]
async fn config_set_persists_and_validates() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
    let state = make_test_state();

    let resp = dispatch_line(
        r#"{"command":"config","args":["set","metric","milliseconds"]}"#,
        &state,
    )
    .await;
    assert_eq!(resp["success"], true);
    let reloaded = crate::config::load_config().unwrap().unwrap();
    assert_eq!(reloaded.metric, "milliseconds");

    let resp = dispatch_line(
        r#"{"command":"config","args":["set","metric","bogus"]}"#,
        &state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().contains("metric must be"));

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

fn minimal_valid_config_json() -> String {
    serde_json::json!({
        "config_version": "v0.0.8",
        "metric": "bytes",
        "step_size": 1024,
        "accepted_mints": [{
            "url": "https://mint.example",
            "price_unit": "sats",
        }],
        "profit_share": [{ "factor": 1.0, "identity": "owner" }],
    })
    .to_string()
}

#[tokio::test]
#[serial_test::serial]
async fn config_save_validates_required_fields_and_persists() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
    let state = make_test_state();

    let payload = serde_json::json!({
        "command": "config",
        "args": ["save", &minimal_valid_config_json()],
    })
    .to_string();
    let resp = dispatch_line(&payload, &state).await;
    assert_eq!(resp["success"], true, "{resp}");
    assert_eq!(
        resp["message"],
        "Configuration saved (restart tollgate-wrt to apply)"
    );
    let saved = crate::config::load_config().unwrap().unwrap();
    assert_eq!(saved.step_size, 1024);

    let bad = r#"{"command":"config","args":["save","{\"metric\":\"bytes\"}"]}"#;
    let resp = dispatch_line(bad, &state).await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .starts_with("Missing required fields"));

    let mut broken =
        serde_json::from_str::<serde_json::Value>(&minimal_valid_config_json()).unwrap();
    broken["profit_share"] = serde_json::json!([{ "factor": 0.5, "identity": "owner" }]);
    let line = serde_json::json!({
        "command": "config",
        "args": ["save", broken.to_string()],
    })
    .to_string();
    let resp = dispatch_line(&line, &state).await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("Invalid profit_share"));

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn config_save_identities_persists_atomically() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());
    let state = make_test_state();

    let identities = serde_json::json!({
        "config_version": "v0.0.1",
        "owned_identities": [],
        "public_identities": [{ "name": "owner", "lightning_address": "owner@ln.example" }],
    });
    let line = serde_json::json!({
        "command": "config",
        "args": ["save-identities", identities.to_string()],
    })
    .to_string();
    let resp = dispatch_line(&line, &state).await;
    assert_eq!(resp["success"], true, "{resp}");
    let saved = crate::config::load_identities().unwrap().unwrap();
    assert_eq!(saved.public_identities.len(), 1);

    let bad = r#"{"command":"config","args":["save-identities","not json"]}"#;
    let resp = dispatch_line(bad, &state).await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"].as_str().unwrap().starts_with("Invalid JSON"));

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
async fn upstream_subcommand_errors_match_go() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"upstream"}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Upstream command requires a subcommand (scan, connect, list, remove)"
    );

    let resp = dispatch_line(r#"{"command":"upstream","args":["frob"]}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Unknown upstream subcommand: frob (supported: scan, connect, list-upstream, remove-upstream, known)"
    );

    let resp = dispatch_line(r#"{"command":"upstream","args":["connect"]}"#, &state).await;
    assert_eq!(resp["error"], "connect requires an SSID argument");
}

#[tokio::test]
async fn upstream_connect_reports_not_supported() {
    let state = make_test_state();
    let resp = dispatch_line(
        r#"{"command":"upstream","args":["connect","some-ssid"]}"#,
        &state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("not supported on this build"));
}

#[tokio::test]
async fn network_subcommand_errors_match_go() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"network"}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Network command requires a subcommand (private)"
    );

    let resp = dispatch_line(r#"{"command":"network","args":["public"]}"#, &state).await;
    assert_eq!(
        resp["error"],
        "Unknown network subcommand: public (supported: private)"
    );

    let resp = dispatch_line(r#"{"command":"network","args":["private"]}"#, &state).await;
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .starts_with("Private network command requires an action"));
}

#[tokio::test]
async fn network_private_status_fails_cleanly_without_uci() {
    if !uci_absent() {
        return;
    }
    let state = make_test_state();
    let resp = dispatch_line(
        r#"{"command":"network","args":["private","status"]}"#,
        &state,
    )
    .await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .starts_with("Failed to get private network SSID"));
}

#[tokio::test]
async fn upstream_list_fails_cleanly_without_uci() {
    if !uci_absent() {
        return;
    }
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"upstream","args":["list-upstream"]}"#, &state).await;
    assert_eq!(resp["success"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .starts_with("Failed to list upstreams"));
}

#[tokio::test]
async fn upstream_known_returns_empty_history() {
    let state = make_test_state();
    let resp = dispatch_line(r#"{"command":"upstream","args":["known"]}"#, &state).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"], serde_json::json!([]));
    assert_eq!(resp["message"], "0 TollGates discovered across 0 scans");
}

#[test]
fn random_password_shape_matches_go() {
    for _ in 0..32 {
        let pw = generate_random_password();
        let parts: Vec<&str> = pw.split('-').collect();
        assert_eq!(parts.len(), 4, "{pw}");
        assert!(parts[0].starts_with(char::is_uppercase), "{pw}");
        let num: u32 = parts[3].parse().unwrap();
        assert!(num < 100);
        assert!((8..=63).contains(&pw.len()), "{pw}");
    }
}

#[test]
fn go_duration_formatting() {
    assert_eq!(format_go_duration(0), "0s");
    assert_eq!(format_go_duration(42), "42s");
    assert_eq!(format_go_duration(65), "1m5s");
    assert_eq!(format_go_duration(3600), "1h0m0s");
    assert_eq!(format_go_duration(7_233), "2h0m33s");
}

/// Real-socket round trip through `cli::serve`: one connection speaking the
/// JSON CLIMessage protocol, one speaking the historical plain-text
/// protocol, on the same socket — both must keep working.
#[tokio::test]
#[serial_test::serial]
async fn serve_speaks_both_json_and_plain_text_protocols() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    let state = make_test_state();
    let server = tokio::spawn(async move {
        let _ = crate::cli::serve(state).await;
    });

    let sock = crate::cli::socket_path();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(sock.exists(), "server must create the socket");

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut conn = tokio::net::UnixStream::connect(&sock)
        .await
        .expect("connect to CLI socket");

    conn.write_all(b"{\"command\":\"status\"}\n").await.unwrap();
    let mut reader = tokio::io::BufReader::new(conn);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["success"], true);
    assert_eq!(resp["message"], "Service status retrieved");
    assert_eq!(resp["data"]["running"], true);
    assert!(resp["timestamp"].as_f64().is_some());
    // Plain-text protocol on a fresh connection must be untouched.
    let mut conn2 = tokio::net::UnixStream::connect(&sock).await.unwrap();
    conn2.write_all(b"status\n").await.unwrap();
    let mut reader2 = tokio::io::BufReader::new(conn2);
    let mut line2 = String::new();
    reader2.read_line(&mut line2).await.unwrap();
    let resp2: serde_json::Value = serde_json::from_str(line2.trim()).unwrap();
    assert_eq!(resp2["success"], true);
    assert_eq!(resp2["message"], "running");

    server.abort();
    let _ = tokio::fs::remove_file(crate::cli::socket_path()).await;
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}
