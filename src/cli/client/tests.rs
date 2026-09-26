//! Tests for client mode: argument parsing, the exact socket-failure
//! message, and end-to-end runs against a fake unix-socket listener.

use super::*;

fn argv(items: &[&str]) -> Vec<String> {
    std::iter::once("tollgate".to_string())
        .chain(items.iter().map(|s| s.to_string()))
        .collect()
}

#[test]
fn parse_args_collects_flags_anywhere() {
    let p = parse_args(&[
        "-j".to_string(),
        "wallet".to_string(),
        "drain".to_string(),
        "-y".to_string(),
        "cashu".to_string(),
    ])
    .unwrap();
    assert!(p.json);
    assert!(p.yes);
    assert_eq!(p.words, vec!["wallet", "drain", "cashu"]);
}

#[test]
fn parse_args_rejects_unknown_flags_and_bad_tail() {
    assert_eq!(
        parse_args(&["--frobnicate".to_string()]).unwrap_err(),
        ParseError::UnknownFlag("--frobnicate".to_string())
    );
    assert_eq!(
        parse_args(&["logs".to_string(), "-n".to_string(), "x".to_string()]).unwrap_err(),
        ParseError::BadTailValue("x".to_string())
    );
}

#[test]
fn parse_args_help_and_version_short_circuit() {
    assert_eq!(
        parse_args(&["-h".to_string()]).unwrap_err(),
        ParseError::Help
    );
    assert_eq!(
        parse_args(&["--version".to_string(), "-j".to_string()]).unwrap_err(),
        ParseError::Version
    );
}

#[test]
fn socket_failure_message_matches_go_exactly() {
    let e = std::io::Error::from_raw_os_error(2); // ENOENT
    assert_eq!(
        socket_failure_message(std::path::Path::new("/var/run/tollgate.sock"), &e),
        "failed to communicate with TollGate service: \
         failed to connect to TollGate service: dial unix /var/run/tollgate.sock: connect: no such file or directory"
    );
    let e = std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "no response from service",
    );
    // Eof responses surface as the Go "no response" dial text inside send().
    assert!(go_dial_error(std::path::Path::new("/x"), &e).contains("no response from service"));
}

#[test]
fn local_timestamp_format() {
    assert_eq!(format_local_timestamp(0), "1970-01-01_00-00-00");
    // 2026-09-26T15:04:05Z
    assert_eq!(format_local_timestamp(1790435045), "2026-09-26_15-04-05");
}

#[tokio::test]
async fn version_flag_prints_and_exits_zero() {
    assert_eq!(run(&argv(&["--version"])).await, 0);
    assert_eq!(run(&argv(&["-v"])).await, 0);
}

#[tokio::test]
async fn unknown_command_exits_nonzero() {
    assert_eq!(run(&argv(&["frobnicate"])).await, 1);
    assert_eq!(run(&argv(&["-j", "frobnicate"])).await, 1);
}

/// Serve one canned CLIResponse line, capture the request the client sent.
async fn fake_socket(
    dir: &std::path::Path,
    response_line: &str,
) -> tokio::task::JoinHandle<serde_json::Value> {
    let socket_path = dir.join("tollgate.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind fake socket");
    let response = response_line.to_string();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut request = String::new();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        reader.read_line(&mut request).await.expect("read request");
        write_half
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        serde_json::from_str(request.trim()).expect("request must be JSON")
    })
}

#[tokio::test]
#[serial_test::serial]
async fn client_json_command_roundtrips_against_fake_socket() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    let server = fake_socket(
        dir.path(),
        r#"{"success":true,"message":"Total wallet balance: 12 sats","data":{"balance_sats":12},"timestamp":1.0}"#,
    )
    .await;

    let code = run(&argv(&["wallet", "balance", "-j"])).await;
    assert_eq!(code, 0);

    let request = server.await.unwrap();
    assert_eq!(request["command"], "wallet");
    assert_eq!(request["args"], serde_json::json!(["balance"]));
    assert!(request["timestamp"].as_f64().is_some());

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn client_failure_response_exits_one() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    let server = fake_socket(
        dir.path(),
        r#"{"success":false,"error":"Unknown command: nope","timestamp":1.0}"#,
    )
    .await;

    let code = run(&argv(&["status"])).await;
    assert_eq!(code, 1);
    let request = server.await.unwrap();
    assert_eq!(request["command"], "status");

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn client_progress_lines_are_skipped_before_final_response() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    let socket_path = dir.path().join("tollgate.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut request = String::new();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        reader.read_line(&mut request).await.unwrap();
        write_half
            .write_all(b"{\"progress\":\"[1/7] Enabling radios...\",\"timestamp\":1.0}\n")
            .await
            .unwrap();
        write_half
            .write_all(b"{\"success\":true,\"message\":\"Connected to 'x'\",\"timestamp\":1.0}\n")
            .await
            .unwrap();
        serde_json::from_str::<serde_json::Value>(request.trim()).unwrap()
    });

    let code = run(&argv(&["upstream", "connect", "some-ssid"])).await;
    assert_eq!(code, 0);
    let request = server.await.unwrap();
    assert_eq!(request["args"], serde_json::json!(["connect", "some-ssid"]));

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn client_without_socket_exits_one() {
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    assert_eq!(run(&argv(&["status"])).await, 1);
    assert_eq!(run(&argv(&["-j", "health"])).await, 1);

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn drain_without_yes_and_declined_confirmation_fails() {
    // No stdin available in tests => read fails => confirmation declined.
    let dir = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

    let code = run(&argv(&["wallet", "drain", "cashu"])).await;
    assert_eq!(code, 1, "declined drain must exit non-zero");

    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}
