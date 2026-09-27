//! Client mode — the `tollgate` operator CLI (port of Go
//! `src/cmd/tollgate-cli/main.go`).
//!
//! Entered from `main` whenever argv has any argument: no args runs the
//! server, any args run this. Server-backed commands speak the JSON
//! CLIMessage/CLIResponse protocol over the same unix socket the server
//! listens on; `logs`, `start`/`stop`/`restart` and the `ssl` commands are
//! executed locally (init scripts, logread, px5g/uci) without a socket.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::cli::ssl;
use crate::cli::x509;

const USAGE: &str = "TollGate CLI - Control your TollGate instance

Usage:
  tollgate [flags] <command> [args]

Commands:
  version                          Show version information
  status                           Show service status
  health                           Check service health
  logs [-n N] [-f]                 Show TollGate logs (logread, fallback /tmp/tollgate-debug.log)
  start | stop | restart           Start/stop/restart NoDogSplash and TollGate services
  wallet balance                   Show wallet balance
  wallet info                      Show wallet information
  wallet fund <cashu-token>        Fund wallet with a Cashu token
  wallet drain cashu               Drain wallet to Cashu tokens (asks confirmation; -y to skip)
  config get [key]                 Get configuration (whole config or one key)
  config set <key> <value>         Set a configuration value
  config schema                    Show the configuration schema
  config save <json>               Replace config.json with the given JSON
  config save-identities <json>    Replace identities.json with the given JSON
  network private status           Show private network status
  network private enable|disable   Enable/disable the private WiFi network
  network private rename <name>    Rename the private SSID
  network private set-password [pw] Change the private WiFi password (generates one when omitted)
  upstream scan                    Scan for upstream WiFi networks
  upstream list                    List configured upstream STA interfaces
  upstream known                   Show discovered TollGate APs from scan history
  upstream remove <ssid>           Remove a disabled upstream from config
  upstream connect <ssid> [pass]   Connect to an upstream WiFi network
  ssl apply [cert [key]]           Apply an SSL certificate (self-signed via px5g when omitted)
  ssl remove                       Revert SSL changes made by 'ssl apply'
  ssl status                       Show SSL configuration and coverage
  ssl covers [cert]                Exit 0 when the certificate covers this router

Flags:
  -j, --json     Output results as JSON
  -y, --yes      Assume yes; skip confirmation prompts
  -n, --tail N   Number of log lines to show (default 50)
  -f, --follow   Follow log output
      --no-restart  ssl apply: do not reload uhttpd/nodogsplash
  -v, --version  Show the CLI version
  -h, --help     Show this help";

#[derive(Debug, Default)]
struct Parsed {
    words: Vec<String>,
    json: bool,
    yes: bool,
    no_restart: bool,
    tail: Option<usize>,
    follow: bool,
}

#[derive(Debug, PartialEq)]
enum ParseError {
    UnknownFlag(String),
    BadTailValue(String),
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Parsed, ParseError> {
    let mut parsed = Parsed::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-j" | "--json" => parsed.json = true,
            "-y" | "--yes" => parsed.yes = true,
            "--no-restart" => parsed.no_restart = true,
            "-f" | "--follow" => parsed.follow = true,
            "-h" | "--help" => return Err(ParseError::Help),
            "-v" | "--version" => return Err(ParseError::Version),
            "-n" | "--tail" => match iter.next() {
                Some(v) => match v.parse::<usize>() {
                    Ok(n) => parsed.tail = Some(n),
                    Err(_) => return Err(ParseError::BadTailValue(v.clone())),
                },
                None => return Err(ParseError::BadTailValue(String::new())),
            },
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_string()))
            }
            other => parsed.words.push(other.to_string()),
        }
    }
    Ok(parsed)
}

// ── socket protocol ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct CliMessage {
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    flags: HashMap<String, String>,
    timestamp: f64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct CliResponse {
    // Progress-only responses carry no success field; absent means false.
    #[serde(default)]
    success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<String>,
    timestamp: f64,
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// Strip Rust's " (os error N)" suffix and lowercase the first letter so the
/// message matches Go's strerror rendering byte-for-byte
/// ("no such file or directory", "permission denied").
fn unix_error_string(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = s.split(" (os error").next().unwrap_or("").trim();
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The exact Go client failure text, asserted by tests.
fn socket_failure_message(path: &std::path::Path, e: &std::io::Error) -> String {
    format!(
        "failed to communicate with TollGate service: \
         failed to connect to TollGate service: dial unix {}: connect: {}",
        path.display(),
        unix_error_string(e)
    )
}

/// Send one CLIMessage line, read CLIResponse lines until a non-progress
/// one arrives (the `upstream connect` streaming variant emits progress
/// lines first).
async fn send_command(
    command: &str,
    args: &[String],
    flags: HashMap<String, String>,
) -> Result<CliResponse, std::io::Error> {
    let path = crate::cli::socket_path();
    let stream = tokio::net::UnixStream::connect(&path).await?;
    let (read_half, mut write_half) = stream.into_split();

    let msg = CliMessage {
        command: command.to_string(),
        args: args.to_vec(),
        flags,
        timestamp: now_epoch(),
    };
    let mut line = serde_json::to_string(&msg).unwrap_or_default();
    line.push('\n');
    use tokio::io::AsyncWriteExt;
    write_half.write_all(line.as_bytes()).await?;

    let mut reader = tokio::io::BufReader::new(read_half);
    use tokio::io::AsyncBufReadExt;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "no response from service",
            ));
        }
        let resp: CliResponse = match serde_json::from_str(buf.trim()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(progress) = resp.progress.as_deref() {
            if !progress.is_empty() {
                println!("  {progress}");
                continue;
            }
        }
        return Ok(resp);
    }
}

// ── display ──────────────────────────────────────────────────────────

fn display_response(resp: &CliResponse) {
    if resp.success {
        if let Some(msg) = resp.message.as_deref() {
            if !msg.is_empty() {
                println!("{msg}");
            }
        }
        if let Some(data) = resp.data.as_ref() {
            display_data(data);
        }
    } else {
        eprintln!(
            "Error: {}",
            resp.error.as_deref().unwrap_or("unknown error")
        );
    }
}

fn display_data(data: &serde_json::Value) {
    match data {
        serde_json::Value::Object(map) => {
            if map.contains_key("tokens") {
                display_drain_result(map);
            } else if map.contains_key("ssid") {
                display_private_network(map);
            } else {
                display_map(map, "");
            }
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                println!("No results");
            } else if let Some(first) = items.first().and_then(|i| i.as_object()) {
                if first.contains_key("radio") && first.contains_key("status") {
                    display_sta_list(items);
                } else if first.contains_key("radio") {
                    display_scan_results(items);
                } else {
                    print_pretty(data);
                }
            } else {
                print_pretty(data);
            }
        }
        _ => print_pretty(data),
    }
}

fn print_pretty(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn display_map(map: &serde_json::Map<String, serde_json::Value>, prefix: &str) {
    for key in map.keys() {
        let value = &map[key];
        match value {
            serde_json::Value::String(s) => println!("{prefix}{key}: {s}"),
            serde_json::Value::Number(n) => println!("{prefix}{key}: {n}"),
            serde_json::Value::Bool(b) => println!("{prefix}{key}: {b}"),
            serde_json::Value::Object(inner) => {
                println!("{prefix}{key}:");
                display_map(inner, &format!("{prefix}  "));
            }
            other => println!("{prefix}{key}: {other}"),
        }
    }
}

fn display_private_network(map: &serde_json::Map<String, serde_json::Value>) {
    println!();
    println!("Private Network Configuration");
    println!("=============================");
    if let Some(serde_json::Value::String(ssid)) = map.get("ssid") {
        println!("SSID:     {ssid}");
    }
    if let Some(serde_json::Value::String(pw)) = map.get("password") {
        println!("Password: {pw}");
    }
    if let Some(serde_json::Value::Bool(enabled)) = map.get("enabled") {
        println!(
            "Status:   {}",
            if *enabled { "Enabled" } else { "Disabled" }
        );
    }
    println!();
}

/// Local time "2026-09-26_15-04-05" without a chrono dependency.
fn format_local_timestamp(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs_of_day = epoch.rem_euclid(86_400);
    let (y, m, d) = x509::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn display_drain_result(map: &serde_json::Map<String, serde_json::Value>) {
    println!();
    println!("Wallet Drain Results:");
    println!("====================");
    if let Some(serde_json::Value::Number(total)) = map.get("total_sats") {
        println!("Total drained: {total} sats");
        println!();
    }

    let Some(tokens) = map.get("tokens").and_then(|t| t.as_array()) else {
        return;
    };
    if tokens.is_empty() {
        println!("No tokens created (all balances are zero)");
        return;
    }

    let filename = format!(
        "wallet_drain_{}.txt",
        format_local_timestamp(now_epoch() as i64)
    );
    match save_tokens_to_file(&filename, tokens, map) {
        Ok(()) => println!("Tokens saved to: {filename}"),
        Err(e) => eprintln!("Error saving tokens to file: {e}"),
    }
    println!();

    for (i, token) in tokens.iter().enumerate() {
        println!("Token {}:", i + 1);
        if let Some(m) = token.get("mint_url").and_then(|v| v.as_str()) {
            println!("  Mint: {m}");
        }
        if let Some(b) = token.get("balance_sats").and_then(|v| v.as_u64()) {
            println!("  Balance: {b} sats");
        }
        if let Some(t) = token.get("token").and_then(|v| v.as_str()) {
            println!("  Token: {t}");
        }
        println!();
    }
}

fn save_tokens_to_file(
    filename: &str,
    tokens: &[serde_json::Value],
    map: &serde_json::Map<String, serde_json::Value>,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(filename)?;
    writeln!(file, "# TollGate Wallet Drain")?;
    writeln!(
        file,
        "# Date: {}",
        format_local_timestamp(now_epoch() as i64)
    )?;
    if let Some(total) = map.get("total_sats").and_then(|v| v.as_u64()) {
        writeln!(file, "# Total: {total} sats across {} tokens", tokens.len())?;
    }
    writeln!(file)?;
    for (i, token) in tokens.iter().enumerate() {
        writeln!(file, "## Token {}", i + 1)?;
        if let Some(m) = token.get("mint_url").and_then(|v| v.as_str()) {
            writeln!(file, "Mint: {m}")?;
        }
        if let Some(b) = token.get("balance_sats").and_then(|v| v.as_u64()) {
            writeln!(file, "Balance: {b} sats")?;
        }
        if let Some(t) = token.get("token").and_then(|v| v.as_str()) {
            writeln!(file, "Token: {t}")?;
            writeln!(file)?;
        }
    }
    Ok(())
}

fn display_scan_results(networks: &[serde_json::Value]) {
    println!();
    println!(
        "{:<30}  {:<8}  {:<5}  {:<20}  Radio",
        "SSID", "Signal", "Ch", "Encryption"
    );
    println!("{}", "-".repeat(80));
    for n in networks {
        let Some(m) = n.as_object() else { continue };
        let get = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let signal = m
            .get("signal")
            .and_then(|v| v.as_i64())
            .map(|s| format!("{s} dBm"))
            .unwrap_or_default();
        println!(
            "{:<30}  {:<8}  {:<5}  {:<20}  {}",
            get("ssid"),
            signal,
            get("channel"),
            get("encryption"),
            get("radio")
        );
    }
    println!();
    println!("Total: {} network(s)", networks.len());
}

fn display_sta_list(stas: &[serde_json::Value]) {
    println!();
    println!(
        "{:<20}  {:<10}  {:<10}  ENCRYPTION",
        "SSID", "STATUS", "RADIO"
    );
    println!("{}", "-".repeat(55));
    for s in stas {
        let Some(m) = s.as_object() else { continue };
        let get = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "{:<20}  {:<10}  {:<10}  {}",
            get("ssid"),
            get("status"),
            get("radio"),
            get("encryption")
        );
    }
    println!();
    println!("{} upstream STA(s) configured.", stas.len());
}

fn ask_confirmation(message: &str) -> bool {
    print!("{message} (y/N): ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let r = line.trim().to_lowercase();
    r == "y" || r == "yes"
}

// ── command execution ────────────────────────────────────────────────

/// Entry point from `main` (argv[0] included). Returns the process exit code.
pub async fn run(argv: &[String]) -> i32 {
    let parsed = match parse_args(&argv[1..]) {
        Ok(p) => p,
        Err(ParseError::Help) => {
            println!("{USAGE}");
            return 0;
        }
        Err(ParseError::Version) => {
            println!("tollgate version {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Err(ParseError::UnknownFlag(f)) => {
            eprintln!("Error: unknown flag: {f}");
            eprintln!("Run 'tollgate --help' for usage.");
            return 1;
        }
        Err(ParseError::BadTailValue(v)) => {
            eprintln!("Error: invalid value \"{v}\" for -n: parse error");
            return 1;
        }
    };

    if parsed.words.is_empty() {
        println!("{USAGE}");
        return 0;
    }

    let words: Vec<&str> = parsed.words.iter().map(String::as_str).collect();
    match words.split_first() {
        Some((&"logs", _)) => cmd_logs(&parsed).await,
        Some((&"start", _)) => cmd_service("start", &parsed).await,
        Some((&"stop", _)) => cmd_service("stop", &parsed).await,
        Some((&"restart", _)) => cmd_service("restart", &parsed).await,
        Some((&"ssl", sub)) => cmd_ssl(sub, &parsed).await,
        Some((&"wallet", sub)) => cmd_wallet(sub, &parsed).await,
        Some((&"config", sub)) => cmd_config(sub, &parsed).await,
        Some((&"network", sub)) => cmd_network(sub, &parsed).await,
        Some((&"upstream", sub)) => cmd_upstream(sub, &parsed).await,
        Some((&"version", _)) => send_and_display("version", &[], &parsed).await,
        Some((&"status", _)) => send_and_display("status", &[], &parsed).await,
        Some((&"health", _)) => send_and_display("health", &[], &parsed).await,
        Some((cmd, _)) => {
            eprintln!("Error: unknown command \"{cmd}\" for \"tollgate\"");
            eprintln!("Run 'tollgate --help' for usage.");
            1
        }
        None => {
            println!("{USAGE}");
            0
        }
    }
}

async fn send(command: &str, args: &[&str], parsed: &Parsed) -> Result<CliResponse, i32> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    match send_command(command, &args, HashMap::new()).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            let path = crate::cli::socket_path();
            if parsed.json {
                let resp = CliResponse {
                    success: false,
                    message: None,
                    data: None,
                    error: Some(format!(
                        "Failed to communicate with TollGate service: {}",
                        go_dial_error(&path, &e)
                    )),
                    progress: None,
                    timestamp: now_epoch(),
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                );
            } else {
                eprintln!("{}", socket_failure_message(&path, &e));
                eprintln!("Make sure the TollGate service is running");
            }
            Err(1)
        }
    }
}

/// Go's `net.Dial` error text for the inner layer of the failure message.
fn go_dial_error(path: &std::path::Path, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof => "no response from service".to_string(),
        _ => format!(
            "failed to connect to TollGate service: dial unix {}: connect: {}",
            path.display(),
            unix_error_string(e)
        ),
    }
}

async fn send_and_display(command: &str, args: &[&str], parsed: &Parsed) -> i32 {
    let resp = match send(command, args, parsed).await {
        Ok(r) => r,
        Err(code) => return code,
    };
    if parsed.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
    } else {
        display_response(&resp);
    }
    if resp.success {
        0
    } else {
        1
    }
}

async fn cmd_wallet(sub: &[&str], parsed: &Parsed) -> i32 {
    match sub {
        ["balance"] => send_and_display("wallet", &["balance"], parsed).await,
        ["info"] => send_and_display("wallet", &["info"], parsed).await,
        ["fund"] => {
            if parsed.json {
                eprintln!("Error: fund command requires a cashu token argument when using --json");
                return 1;
            }
            print!("Paste your Cashu token: ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut token = String::new();
            if std::io::stdin().read_line(&mut token).is_err() {
                eprintln!("Error: failed to read token input");
                return 1;
            }
            let token = token.trim();
            if token.is_empty() {
                eprintln!("Error: no token provided");
                return 1;
            }
            send_and_display("wallet", &["fund", token], parsed).await
        }
        ["fund", token] => send_and_display("wallet", &["fund", token], parsed).await,
        ["drain", "cashu"] => {
            if parsed.json {
                return send_and_display("wallet", &["drain", "cashu"], parsed).await;
            }
            println!();
            println!("\u{26a0}\u{fe0f}  WARNING: Draining the wallet will remove ALL funds from the wallet!");
            println!("The funds will be converted to Cashu tokens that will be saved to a file.");
            println!("Once drained, the tokens are OUT of the wallet and must be stored securely.");
            println!();
            if !parsed.yes && !ask_confirmation("Are you sure you want to drain the wallet?") {
                eprintln!(
                    "Error: drain cancelled: no confirmation given (use --yes to run non-interactively)"
                );
                return 1;
            }
            println!();
            send_and_display("wallet", &["drain", "cashu"], parsed).await
        }
        ["drain"] | ["drain", "lightning"] => {
            eprintln!("Error: drain requires a type: 'cashu' (lightning not yet supported)");
            1
        }
        // Go's cobra prints the wallet help to STDOUT (exit 0) for a missing
        // or unknown subcommand — even under --json. Callers (and PRTA's
        // cli_command) parse stdout, so an empty stdout reads as a crash.
        [] | [_, ..] => {
            println!("{WALLET_HELP}");
            0
        }
    }
}

const WALLET_HELP: &str =
    "Manage your TollGate wallet - check balance, drain funds, view information

Usage:
  tollgate wallet [command]

Available Commands:
  balance     Show wallet balance
  drain       Drain wallet funds
  fund        Fund wallet with a Cashu token
  info        Show wallet information

Flags:
  -h, --help   help for wallet

Global Flags:
  -j, --json   Output results as JSON

Use \"tollgate wallet [command] --help\" for more information about a command.";

async fn cmd_config(sub: &[&str], parsed: &Parsed) -> i32 {
    match sub {
        ["get"] => send_and_display("config", &["get"], parsed).await,
        ["get", key] => send_and_display("config", &["get", key], parsed).await,
        ["set", key, value] => send_and_display("config", &["set", key, value], parsed).await,
        ["schema"] => send_and_display("config", &["schema"], parsed).await,
        ["save", json] => send_and_display("config", &["save", json], parsed).await,
        ["save-identities", json] => {
            send_and_display("config", &["save-identities", json], parsed).await
        }
        ["set"] | ["set", _] => {
            eprintln!("Error: config set requires exactly 2 arguments: <key> <value>");
            1
        }
        ["save"] => {
            eprintln!("Error: config save requires a <json-string> argument");
            1
        }
        ["save-identities"] => {
            eprintln!("Error: config save-identities requires a <json-string> argument");
            1
        }
        [other, ..] => {
            eprintln!(
                "Error: unknown config subcommand \"{other}\" (supported: get, set, schema, save, save-identities)"
            );
            1
        }
        [] => {
            eprintln!(
                "Error: config requires a subcommand: get, set, schema, save, save-identities"
            );
            1
        }
    }
}

async fn cmd_network(sub: &[&str], parsed: &Parsed) -> i32 {
    match sub {
        ["private", "status"] => send_and_display("network", &["private", "status"], parsed).await,
        ["private", "enable"] => send_and_display("network", &["private", "enable"], parsed).await,
        ["private", "disable"] => {
            if !parsed.json {
                println!();
                println!("\u{26a0}\u{fe0f}  WARNING: Disabling the private network may lock you out of the router!");
                println!("Make sure you have another way to access the router (e.g., via the public network or physical access).");
                println!();
                if !ask_confirmation("Are you sure you want to disable the private network?") {
                    println!("Operation cancelled.");
                    return 0;
                }
            }
            send_and_display("network", &["private", "disable"], parsed).await
        }
        ["private", "rename", name] => {
            send_and_display("network", &["private", "rename", name], parsed).await
        }
        ["private", "set-password"] => {
            send_and_display("network", &["private", "set-password"], parsed).await
        }
        ["private", "set-password", pw] => {
            send_and_display("network", &["private", "set-password", pw], parsed).await
        }
        ["private", "rename"] => {
            eprintln!("Error: rename requires a new SSID name");
            1
        }
        ["private", other, ..] => {
            eprintln!(
                "Error: unknown private network action \"{other}\" (supported: status, enable, disable, rename, set-password)"
            );
            1
        }
        ["private"] => {
            eprintln!("Error: network private requires an action: status, enable, disable, rename, set-password");
            1
        }
        [other, ..] => {
            eprintln!("Error: unknown network subcommand \"{other}\" (supported: private)");
            1
        }
        [] => {
            eprintln!("Error: network requires a subcommand: private");
            1
        }
    }
}

async fn cmd_upstream(sub: &[&str], parsed: &Parsed) -> i32 {
    match sub {
        ["scan"] => send_and_display("upstream", &["scan"], parsed).await,
        ["known"] => send_and_display("upstream", &["known"], parsed).await,
        ["list"] | ["list-upstream"] => {
            send_and_display("upstream", &["list-upstream"], parsed).await
        }
        ["remove", ssid] | ["remove-upstream", ssid] => {
            send_and_display("upstream", &["remove-upstream", ssid], parsed).await
        }
        ["connect", ssid] => send_and_display("upstream", &["connect", ssid], parsed).await,
        ["connect", ssid, pw] => send_and_display("upstream", &["connect", ssid, pw], parsed).await,
        ["remove"] | ["remove-upstream"] => {
            eprintln!("Error: remove requires an SSID argument");
            1
        }
        ["connect"] => {
            eprintln!("Error: connect requires an SSID argument");
            1
        }
        [other, ..] => {
            eprintln!(
                "Error: unknown upstream subcommand \"{other}\" (supported: scan, connect, list, remove, known)"
            );
            1
        }
        [] => {
            eprintln!("Error: upstream requires a subcommand: scan, connect, list, remove, known");
            1
        }
    }
}

// ── client-side commands (no socket) ─────────────────────────────────

async fn cmd_logs(parsed: &Parsed) -> i32 {
    let tail = parsed.tail.unwrap_or(50);

    if parsed.follow {
        let status = tokio::process::Command::new("logread")
            .args(["-e", "tollgate", "-f"])
            .status()
            .await;
        return match status {
            Ok(s) if s.success() => 0,
            Ok(s) => {
                eprintln!("Error: failed to read logs: logread exited with {s}");
                1
            }
            Err(e) => {
                eprintln!("Error: failed to read logs: {e}");
                1
            }
        };
    }

    let logread = tokio::process::Command::new("logread")
        .output()
        .await
        .and_then(|o| {
            if o.status.success() {
                Ok(o)
            } else {
                Err(std::io::Error::other("logread failed"))
            }
        });

    let lines: Vec<String> = match logread {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.contains("tollgate"))
            .map(str::to_string)
            .collect(),
        Err(_) => match tokio::fs::read_to_string("/tmp/tollgate-debug.log").await {
            Ok(content) => content.lines().map(str::to_string).collect(),
            Err(e) => {
                eprintln!("Error: failed to read logs: {e}");
                return 1;
            }
        },
    };

    let start = lines.len().saturating_sub(tail);
    for line in &lines[start..] {
        println!("{line}");
    }
    0
}

async fn cmd_service(action: &str, parsed: &Parsed) -> i32 {
    let services: [(&str, &str); 2] = match action {
        "start" => [
            ("NoDogSplash", "/etc/init.d/nodogsplash"),
            ("TollGate", "/etc/init.d/tollgate-wrt"),
        ],
        "stop" => [
            ("TollGate", "/etc/init.d/tollgate-wrt"),
            ("NoDogSplash", "/etc/init.d/nodogsplash"),
        ],
        _ => [
            ("NoDogSplash", "/etc/init.d/nodogsplash"),
            ("TollGate", "/etc/init.d/tollgate-wrt"),
        ],
    };

    for (name, init) in services {
        println!("Executing: {name} {action}...");
        let output = tokio::process::Command::new(init)
            .arg(action)
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() => {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if !combined.trim().is_empty() {
                    println!("{combined}");
                }
            }
            Ok(o) => {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if parsed.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "success": false,
                            "error": format!("failed to {action} {name}"),
                            "output": combined,
                        })
                    );
                } else {
                    eprintln!("Failed to {action} {name}");
                    if !combined.trim().is_empty() {
                        eprintln!("Output: {combined}");
                    }
                }
                return 1;
            }
            Err(e) => {
                if parsed.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "success": false,
                            "error": format!("failed to {action} {name}: {e}"),
                        })
                    );
                } else {
                    eprintln!("Failed to {action} {name}: {e}");
                }
                return 1;
            }
        }
    }

    if parsed.json {
        println!(
            "{}",
            serde_json::json!({
                "success": true,
                "message": format!("Successfully {action}ed services"),
            })
        );
    } else {
        println!("Successfully {action}ed services");
    }
    0
}

async fn cmd_ssl(sub: &[&str], parsed: &Parsed) -> i32 {
    let result = match sub {
        ["apply", rest @ ..] => {
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            ssl::apply(&rest, parsed.yes, parsed.no_restart).await
        }
        ["remove"] => ssl::remove(parsed.yes).await,
        ["status"] => ssl::status().await,
        ["covers"] => return ssl::covers(None, parsed.json).await,
        ["covers", cert] => return ssl::covers(Some(cert), parsed.json).await,
        [] => {
            eprintln!("Error: ssl requires a subcommand: apply, remove, status, covers");
            return 1;
        }
        [other, ..] => {
            eprintln!(
                "Error: unknown ssl subcommand \"{other}\" (supported: apply, remove, status, covers)"
            );
            return 1;
        }
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
