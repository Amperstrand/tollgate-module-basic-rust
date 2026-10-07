//! Client-side `ssl` commands — port of Go `src/cmd/tollgate-cli/ssl.go`.
//!
//! The Go CLI generates its self-signed certificate in-process with the Go
//! crypto stack; this port delegates generation to OpenWrt's `px5g` (the
//! platform's own generator, same one uhttpd's init script uses) and keeps
//! everything else — backup/revert bookkeeping, uhttpd/dnsmasq/nodogsplash
//! uci plumbing, the SAN-coverage verdict behind `ssl covers`/redirect_https,
//! and the operator opt-out marker — behaviorally identical.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::cli::x509::{self, CertSummary};

const UHTTPD_CERT_DEFAULT: &str = "/etc/uhttpd.crt";
const UHTTPD_INIT: &str = "/etc/init.d/uhttpd";
const DNSMASQ_INIT: &str = "/etc/init.d/dnsmasq";
const NODOGSPLASH_INIT: &str = "/etc/init.d/nodogsplash";

pub const ERR_CERT_DOES_NOT_COVER_ROUTER: &str = "certificate does not cover this router";

fn ssl_dir() -> PathBuf {
    crate::config::config_dir().join("ssl")
}
fn backup_dir() -> PathBuf {
    ssl_dir().join("backup")
}
fn cert_dest() -> PathBuf {
    ssl_dir().join("server.crt")
}
fn key_dest() -> PathBuf {
    ssl_dir().join("server.key")
}
fn opt_out_file() -> PathBuf {
    ssl_dir().join("tls-identity-removed")
}

// ── uci / init.d plumbing ────────────────────────────────────────────

async fn run_uci(args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("uci")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("uci {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "uci {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn uci_get(key: &str) -> Result<String, String> {
    run_uci(&["-q", "get", key])
        .await
        .map(|s| s.trim().to_string())
}

async fn uci_get_or_empty(key: &str) -> String {
    uci_get(key).await.unwrap_or_default()
}

async fn uci_get_list(key: &str) -> Vec<String> {
    uci_get_or_empty(key)
        .await
        .split_whitespace()
        .map(|f| f.trim_matches('\'').to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

fn assert_no_control_chars(key: &str, value: &str) -> Result<(), String> {
    let bad = |s: &str| s.contains(['\n', '\r', '\0']);
    if bad(key) || bad(value) {
        return Err("invalid UCI value: contains control characters".to_string());
    }
    Ok(())
}

async fn uci_set(key: &str, value: &str) -> Result<(), String> {
    assert_no_control_chars(key, value)?;
    run_uci(&["set", &format!("{key}={value}")])
        .await
        .map(|_| ())
}

async fn uci_add_list(key_value: &str) -> Result<(), String> {
    run_uci(&["add_list", key_value]).await.map(|_| ())
}

async fn uci_del_list(key_value: &str) -> Result<(), String> {
    run_uci(&["-q", "del_list", key_value]).await.map(|_| ())
}

async fn uci_delete(key: &str) -> Result<(), String> {
    run_uci(&["delete", key]).await.map(|_| ())
}

async fn uci_delete_if_exists(key: &str) -> Result<(), String> {
    match run_uci(&["-q", "delete", key]).await {
        Ok(_) => Ok(()),
        Err(e) if e.contains("Entry not found") => Ok(()),
        Err(e) => Err(e),
    }
}

async fn uci_commit(config: &str) -> Result<(), String> {
    run_uci(&["commit", config]).await.map(|_| ())
}

async fn run_init_script(path: &str, action: &str) -> Result<(), String> {
    let output = tokio::process::Command::new(path)
        .arg(action)
        .output()
        .await
        .map_err(|e| format!("{path} {action}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{path} {action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ── pure helpers ─────────────────────────────────────────────────────

/// `network.lan.ipaddr` values may carry a CIDR suffix (measured on a
/// 25.12 install: `192.168.1.1/24`); strip it and validate.
pub fn parse_lan_ip(value: &str) -> Option<IpAddr> {
    let addr = value.trim();
    let addr = addr.split('/').next().unwrap_or("").trim();
    addr.parse().ok()
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn format_date(epoch: i64) -> String {
    let (y, m, d) = x509::civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn san_summary(c: &CertSummary) -> String {
    let parts: Vec<String> = c
        .dns_sans
        .iter()
        .map(|n| format!("DNS:{n}"))
        .chain(c.ip_sans.iter().map(|i| format!("IP:{i}")))
        .collect();
    if !parts.is_empty() {
        return parts.join(",");
    }
    if !c.subject_cn.trim().is_empty() {
        return format!("CN:{} (no SAN extension)", c.subject_cn.trim());
    }
    "none".to_string()
}

/// A certificate covers this router when its SANs validate the configured
/// hostname, the `<hostname>.lan` alias dnsmasq serves, or the LAN IP.
/// A CommonName with no SAN extension is deliberately not coverage.
pub fn cert_coverage(
    c: &CertSummary,
    hosts: &[String],
    ips: &[String],
    now_epoch: i64,
) -> (bool, String) {
    if now_epoch > c.not_after {
        return (
            false,
            format!("certificate expired on {}", format_date(c.not_after)),
        );
    }
    let names: Vec<&str> = hosts
        .iter()
        .chain(ips.iter())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return (
            false,
            "this router has no hostname or LAN IP to check the certificate against".to_string(),
        );
    }
    for name in &names {
        let covered = match name.parse::<IpAddr>() {
            Ok(ip) => c.ip_sans.contains(&ip),
            Err(_) => c.dns_sans.iter().any(|p| x509::host_matches(p, name)),
        };
        if covered {
            return (true, format!("SANs cover {name}"));
        }
    }
    (
        false,
        format!(
            "SANs ({}) cover none of this router's names ({})",
            san_summary(c),
            names.join(", ")
        ),
    )
}

async fn router_tls_names() -> (Vec<String>, Vec<String>) {
    let mut hostname = uci_get_or_empty("system.@system[0].hostname").await;
    hostname = hostname.trim().to_string();
    if hostname.is_empty() {
        if let Ok(h) = tokio::fs::read_to_string("/proc/sys/kernel/hostname").await {
            hostname = h.trim().to_string();
        }
    }
    let mut hosts = Vec::new();
    if !hostname.is_empty() {
        hosts.push(hostname.clone());
        hosts.push(format!("{hostname}.lan"));
    }
    let mut ips = Vec::new();
    if let Some(ip) = parse_lan_ip(&uci_get_or_empty("network.lan.ipaddr").await) {
        ips.push(ip.to_string());
    }
    (hosts, ips)
}

/// The certificate uhttpd.main is configured to present — the one a browser
/// is offered, and the one the derived redirect depends on.
async fn uhttpd_cert_path() -> String {
    let path = uci_get_or_empty("uhttpd.main.cert").await;
    let path = path.trim();
    if path.is_empty() {
        UHTTPD_CERT_DEFAULT.to_string()
    } else {
        path.to_string()
    }
}

pub async fn cert_covers_router(cert_path: &str) -> (bool, String) {
    if cert_path.trim().is_empty() {
        return (false, "no certificate path configured".to_string());
    }
    let data = match tokio::fs::read(cert_path).await {
        Ok(d) => d,
        Err(e) => return (false, format!("cannot read {cert_path}: {e}")),
    };
    if String::from_utf8_lossy(&data).trim().is_empty() {
        return (false, format!("{cert_path} is empty"));
    }
    let der = match x509::pem_decode(&String::from_utf8_lossy(&data), "CERTIFICATE") {
        Some(d) => d,
        None => return (false, format!("{cert_path} is not a PEM certificate")),
    };
    let cert = match x509::parse_certificate(&der) {
        Ok(c) => c,
        Err(e) => return (false, format!("cannot parse {cert_path}: {e}")),
    };
    let (hosts, ips) = router_tls_names().await;
    cert_coverage(&cert, &hosts, &ips, epoch_now())
}

async fn lan_ip_from_uci() -> Result<String, String> {
    let raw = uci_get("network.lan.ipaddr").await;
    let raw = match raw {
        Ok(v) => v,
        Err(_) => return Err("cannot determine LAN IP (network.lan.ipaddr)".to_string()),
    };
    match parse_lan_ip(&raw) {
        Some(ip) => Ok(ip.to_string()),
        None => Err(format!(
            "cannot determine LAN IP (network.lan.ipaddr={raw:?})"
        )),
    }
}

// ── apply ────────────────────────────────────────────────────────────

/// `ssl apply [cert [key]]`: without arguments generate a self-signed
/// certificate (via px5g) for the router's hostname; with files install a
/// real certificate.
pub async fn apply(args: &[String], yes: bool, no_restart: bool) -> Result<(), String> {
    let lan_ip = lan_ip_from_uci().await?;

    if backup_dir().exists() {
        println!("WARNING: SSL backup already exists (SSL may already be applied).");
        println!("  Run 'tollgate ssl remove' first to cleanly revert.");
        if !yes && !ask_confirmation("Overwrite backup and re-apply?") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let apply_result = if args.is_empty() {
        apply_self_signed(&lan_ip, yes, no_restart).await
    } else {
        apply_real_cert(args, &lan_ip, yes, no_restart).await
    };
    apply_result?;
    if !identity_installed() {
        return Ok(());
    }
    clear_opt_out()
}

fn identity_installed() -> bool {
    fn nonempty_file(path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    }
    nonempty_file(&cert_dest()) && nonempty_file(&key_dest())
}

fn ask_confirmation(message: &str) -> bool {
    print!("{} (y/N): ", message);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let r = line.trim().to_lowercase();
    r == "y" || r == "yes"
}

async fn apply_self_signed(lan_ip: &str, yes: bool, no_restart: bool) -> Result<(), String> {
    let mut hostname = uci_get_or_empty("system.@system[0].hostname")
        .await
        .trim()
        .to_string();
    if hostname.is_empty() {
        hostname = "TollGate".to_string();
    }
    let domain = if hostname.ends_with(".lan") {
        hostname.clone()
    } else {
        format!("{hostname}.lan")
    };

    println!("Generating self-signed certificate for {domain}...");

    let work_dir = std::env::temp_dir().join(format!("tollgate-ssl-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("failed to create temp dir: {e}"))?;
    let result = generate_self_signed(&work_dir, &hostname, &domain, lan_ip).await;
    let cert_file = work_dir.join("cert.pem");
    let key_file = work_dir.join("key.pem");
    let outcome = match result {
        Ok(()) => {
            install_self_signed(
                &cert_file, &key_file, &hostname, &domain, lan_ip, yes, no_restart,
            )
            .await
        }
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&work_dir);
    outcome
}

/// px5g (OpenWrt's own generator) produces the certificate; SANs cover the
/// hostname, its .lan alias and the LAN IP, matching Go's template. The
/// subjectAltName -addext flags must stay consecutive and trailing — px5g's
/// argument loop chains one SAN list entry per flag and mis-orders otherwise.
async fn generate_self_signed(
    work_dir: &Path,
    hostname: &str,
    domain: &str,
    lan_ip: &str,
) -> Result<(), String> {
    let cert_path = work_dir.join("cert.pem");
    let key_path = work_dir.join("key.pem");
    let output = tokio::process::Command::new("px5g")
        .args([
            "selfsigned",
            "-days",
            "3650",
            "-newkey",
            "rsa:2048",
            "-keyout",
        ])
        .arg(&key_path)
        .arg("-out")
        .arg(&cert_path)
        .args(["-subj", &format!("/CN={domain}")])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .arg("-addext")
        .arg(format!("subjectAltName=DNS:{hostname}"))
        .arg("-addext")
        .arg(format!("subjectAltName=DNS:{domain}"))
        .arg("-addext")
        .arg(format!("subjectAltName=IP:{lan_ip}"))
        .output()
        .await
        .map_err(|e| format!("px5g not available (required to generate the certificate): {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "px5g selfsigned failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if !cert_path.exists() || !key_path.exists() {
        return Err("px5g did not produce cert/key files".to_string());
    }
    Ok(())
}

async fn install_self_signed(
    cert_file: &Path,
    key_file: &Path,
    hostname: &str,
    domain: &str,
    lan_ip: &str,
    yes: bool,
    no_restart: bool,
) -> Result<(), String> {
    println!();
    println!("Certificate details:");
    println!("  Domain : {domain} (self-signed)");
    println!("  Expires: 10 years");
    println!("  SANs   : {hostname}, {domain}, {lan_ip}");
    println!("  LAN IP : {lan_ip}");
    println!();
    println!("  NOTE: Self-signed certs are NOT trusted by browsers or RFC 8908 clients.");
    println!("  The captive portal will continue using HTTP interception.");
    println!("  LuCI admin will be accessible via HTTPS with a browser warning.");
    println!();
    println!("Changes to apply:");
    println!(
        "  [1] Install self-signed cert+key to {}/",
        ssl_dir().display()
    );
    println!(
        "  [2] uhttpd: cert='{}' key='{}'",
        cert_dest().display(),
        key_dest().display()
    );
    println!("  [3] nodogsplash: allow tcp port 443 so clients can reach uhttpd HTTPS");
    println!();
    if !yes && !ask_confirmation("Apply all?") {
        println!("Aborted.");
        return Ok(());
    }

    ssl_backup("self-signed", domain, lan_ip).await?;
    install_certs(cert_file, key_file)?;
    println!("[1] Self-signed certificate installed.");
    configure_uhttpd().await?;
    println!("[2] uhttpd configured.");
    allow_port_443().await?;
    println!("[3] nodogsplash firewall updated.");
    reload_after_apply(false, no_restart).await?;
    apply_redirect_https().await?;
    println!();
    println!("Done. Self-signed HTTPS enabled for {domain}");
    println!();
    println!("  Portal URL: http://{domain}/ (NoDogSplash, HTTP only)");
    println!("  LuCI URL:   https://{domain}/ (uhttpd, HTTPS, self-signed)");
    println!();
    println!("To revert: tollgate ssl remove");
    Ok(())
}

async fn apply_real_cert(
    args: &[String],
    lan_ip: &str,
    yes: bool,
    no_restart: bool,
) -> Result<(), String> {
    let cert_file = PathBuf::from(&args[0]);
    let key_file = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        split_combined_pem(&cert_file).await?
    };

    for f in [&cert_file, &key_file] {
        if !f.exists() {
            return Err(format!("cert file not found: {}", f.display()));
        }
    }

    let pem = tokio::fs::read_to_string(&cert_file)
        .await
        .map_err(|e| format!("cannot read cert file: {e}"))?;
    let der = x509::pem_decode(&pem, "CERTIFICATE")
        .ok_or_else(|| format!("not a valid PEM certificate: {}", cert_file.display()))?;
    let cert =
        x509::parse_certificate(&der).map_err(|e| format!("failed to parse certificate: {e}"))?;

    // Codex P2 on #54: reject an unparseable or mismatched key pair
    // BEFORE any file or UCI mutation — uhttpd would otherwise be left
    // pointing at a pair it cannot start with (especially with
    // --no-restart, where nothing surfaces the breakage).
    let key_pem = tokio::fs::read_to_string(&key_file)
        .await
        .map_err(|e| format!("cannot read key file: {e}"))?;
    let key_der = ["PRIVATE KEY", "EC PRIVATE KEY", "RSA PRIVATE KEY"]
        .iter()
        .find_map(|t| x509::pem_decode(&key_pem, t))
        .ok_or_else(|| format!("not a valid PEM private key: {}", key_file.display()))?;
    x509::private_key_matches_certificate(&der, &key_der)
        .map_err(|e| format!("certificate/key pair check failed: {e}"))?;

    if epoch_now() > cert.not_after {
        println!("WARNING: certificate has expired!");
        println!(
            "  Continuing anyway \u{2014} the cert will be installed but browsers will reject it."
        );
    }

    let domain = cert
        .dns_sans
        .first()
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cert.subject_cn.trim().to_string());
    if domain.is_empty() {
        return Err("could not extract domain from certificate (no SAN or CN found)".to_string());
    }

    println!();
    println!("Certificate details:");
    println!("  Domain : {domain}");
    println!("  Expires: {}", format_date(cert.not_after));
    println!("  SAN    : {}", cert.dns_sans.join(", "));
    println!("  LAN IP : {lan_ip}");
    println!();
    println!("Changes to apply:");
    println!("  [1] Install cert+key to {}/", ssl_dir().display());
    println!(
        "  [2] uhttpd: cert='{}' key='{}'",
        cert_dest().display(),
        key_dest().display()
    );
    println!("  [3] dnsmasq: resolve {domain} -> {lan_ip}");
    println!("  [4] nodogsplash: allow tcp port 443 so clients can reach uhttpd HTTPS");
    println!();
    if !yes && !ask_confirmation("Apply all?") {
        println!("Aborted.");
        return Ok(());
    }

    ssl_backup("real-cert", &domain, lan_ip).await?;
    install_certs(&cert_file, &key_file)?;
    println!("[1] Certificate installed.");
    configure_uhttpd().await?;
    println!("[2] uhttpd configured.");
    configure_dnsmasq(&domain, lan_ip).await?;
    println!("[3] dnsmasq configured: {domain} -> {lan_ip}");
    allow_port_443().await?;
    println!("[4] nodogsplash firewall updated.");
    reload_after_apply(true, no_restart).await?;
    apply_redirect_https().await?;
    println!();
    println!("Done. HTTPS enabled for {domain}");
    println!();
    println!("  Portal URL: http://{domain}/ (NoDogSplash, HTTP only)");
    println!("  LuCI URL:   https://{domain}/ (uhttpd, HTTPS)");
    println!();
    println!("To revert: tollgate ssl remove");
    Ok(())
}

/// Split a combined cert+key PEM into separate temp files.
async fn split_combined_pem(input: &Path) -> Result<PathBuf, String> {
    let data = tokio::fs::read_to_string(input)
        .await
        .map_err(|e| format!("cannot read file: {e}"))?;
    let mut cert = String::new();
    let mut key = String::new();
    for (block_type, der) in x509::pem_blocks(&data) {
        let body = pem_encode(&der);
        match block_type.as_str() {
            "CERTIFICATE" => {
                cert.push_str("-----BEGIN CERTIFICATE-----\n");
                cert.push_str(&body);
                cert.push_str("-----END CERTIFICATE-----\n");
            }
            "PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY" => {
                key.push_str(&format!("-----BEGIN {block_type}-----\n"));
                key.push_str(&body);
                key.push_str(&format!("-----END {block_type}-----\n"));
            }
            _ => {}
        }
    }
    if cert.is_empty() && key.is_empty() {
        return Err(format!(
            "no PEM certificate or key blocks found in: {}",
            input.display()
        ));
    }
    if cert.is_empty() {
        return Err(format!(
            "private key found but no certificate in: {}\n  Provide a cert file as the first argument",
            input.display()
        ));
    }
    if key.is_empty() {
        return Err(format!(
            "certificate found but no private key in: {}\n  Provide a key file as the second argument",
            input.display()
        ));
    }

    let work_dir = std::env::temp_dir().join(format!("tollgate-ssl-split-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("failed to create temp dir: {e}"))?;
    let cert_path = work_dir.join("cert.pem");
    let key_path = work_dir.join("key.pem");
    std::fs::write(&cert_path, cert).map_err(|e| e.to_string())?;
    std::fs::write(&key_path, key).map_err(|e| e.to_string())?;
    Ok(key_path)
}

fn pem_encode(der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut chars = String::new();
    for chunk in der.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        chars.push(ALPHABET[(n >> 18) as usize & 63] as char);
        chars.push(ALPHABET[(n >> 12) as usize & 63] as char);
        chars.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        chars.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    let mut out = String::new();
    for (i, c) in chars.chars().enumerate() {
        if i > 0 && i % 64 == 0 {
            out.push('\n');
        }
        out.push(c);
    }
    out.push('\n');
    out
}

async fn reload_after_apply(real_cert: bool, no_restart: bool) -> Result<(), String> {
    if no_restart {
        println!(
            "Skipping service reload (--no-restart): the caller converges uhttpd and nodogsplash."
        );
        return Ok(());
    }
    reload_services(real_cert).await
}

async fn reload_services(real_cert: bool) -> Result<(), String> {
    run_init_script(UHTTPD_INIT, "reload")
        .await
        .map_err(|e| format!("failed to reload uhttpd: {e}"))?;
    if real_cert {
        run_init_script(DNSMASQ_INIT, "reload")
            .await
            .map_err(|e| format!("failed to reload dnsmasq: {e}"))?;
    }
    run_init_script(NODOGSPLASH_INIT, "restart")
        .await
        .map_err(|e| format!("failed to restart nodogsplash: {e}"))?;
    Ok(())
}

// ── backup / install ─────────────────────────────────────────────────

async fn ssl_backup(mode: &str, domain: &str, lan_ip: &str) -> Result<(), String> {
    let dir = backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create backup dir: {e}"))?;

    write_backup_file(
        &dir.join("uhttpd.cert"),
        &uci_get_or_empty("uhttpd.main.cert").await,
    );
    write_backup_file(
        &dir.join("uhttpd.key"),
        &uci_get_or_empty("uhttpd.main.key").await,
    );
    write_backup_file(&dir.join("ssl.domain"), domain);
    write_backup_file(&dir.join("ssl.lan_ip"), lan_ip);
    write_backup_file(&dir.join("ssl.mode"), mode);

    if let Ok(show) = run_uci(&["show", "dhcp"]).await {
        let domains: Vec<&str> = show
            .lines()
            .filter(|l| l.contains("=domain"))
            .collect::<Vec<_>>();
        write_backup_file(&dir.join("dnsmasq.domains"), &domains.join("\n"));
    }

    println!("Backup saved to {}/", dir.display());
    Ok(())
}

fn write_backup_file(path: &Path, content: &str) {
    if !content.is_empty() {
        let _ = std::fs::write(path, format!("{content}\n"));
    }
}

fn file_read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn install_certs(cert_file: &Path, key_file: &Path) -> Result<(), String> {
    let dir = ssl_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create SSL dir: {e}"))?;
    std::fs::copy(cert_file, cert_dest()).map_err(|e| format!("failed to install cert: {e}"))?;
    std::fs::copy(key_file, key_dest()).map_err(|e| format!("failed to install key: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_dest(), std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(cert_dest(), std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

async fn configure_uhttpd() -> Result<(), String> {
    uci_set("uhttpd.main.cert", &cert_dest().display().to_string()).await?;
    uci_set("uhttpd.main.key", &key_dest().display().to_string()).await?;

    let listen_https = uci_get_list("uhttpd.main.listen_https").await;
    if !listen_https.iter().any(|v| v == "0.0.0.0:443") {
        uci_add_list("uhttpd.main.listen_https=0.0.0.0:443").await?;
    }
    if !listen_https.iter().any(|v| v == "[::]:443") {
        uci_add_list("uhttpd.main.listen_https=[::]:443").await?;
    }
    uci_commit("uhttpd").await
}

async fn configure_dnsmasq(domain: &str, lan_ip: &str) -> Result<(), String> {
    remove_dnsmasq_domain_if_exists(domain).await?;
    run_uci(&["add", "dhcp", "domain"]).await?;
    uci_set("dhcp.@domain[-1].name", domain).await?;
    uci_set("dhcp.@domain[-1].ip", lan_ip).await?;
    uci_commit("dhcp").await
}

async fn allow_port_443() -> Result<(), String> {
    let nds_users = uci_get_list("nodogsplash.@nodogsplash[0].users_to_router").await;
    if !nds_users.iter().any(|v| v == "allow tcp port 443") {
        uci_add_list("nodogsplash.@nodogsplash[0].users_to_router=allow tcp port 443").await?;
    }
    uci_commit("nodogsplash").await
}

async fn remove_port_443_allow() -> Result<(), String> {
    uci_del_list("nodogsplash.@nodogsplash[0].users_to_router=allow tcp port 443").await
}

async fn remove_dnsmasq_domain(domain: &str) -> Result<(), String> {
    remove_dnsmasq_domain_if_exists(domain).await?;
    uci_commit("dhcp").await
}

async fn remove_dnsmasq_domain_if_exists(domain: &str) -> Result<(), String> {
    let show = run_uci(&["show", "dhcp"]).await.unwrap_or_default();
    for line in show.lines().filter(|l| l.contains("=domain")) {
        let section = match line.split_once('.') {
            Some((_, rest)) => match rest.split_once('=') {
                Some((idx, _)) => idx,
                None => continue,
            },
            None => continue,
        };
        let name = uci_get_or_empty(&format!("dhcp.{section}.name")).await;
        if name == domain {
            uci_delete(&format!("dhcp.{section}")).await?;
        }
    }
    Ok(())
}

async fn restore_uhttpd() -> Result<(), String> {
    let dir = backup_dir();
    let prev_cert = file_read(&dir.join("uhttpd.cert"));
    let prev_key = file_read(&dir.join("uhttpd.key"));
    let mut restored = false;

    if !prev_cert.is_empty() && Path::new(&prev_cert).exists() {
        uci_set("uhttpd.main.cert", &prev_cert).await?;
        restored = true;
    }
    if !prev_key.is_empty() && Path::new(&prev_key).exists() {
        uci_set("uhttpd.main.key", &prev_key).await?;
        restored = true;
    }

    if !restored {
        if Path::new("/etc/uhttpd.crt").exists() && Path::new("/etc/uhttpd.key").exists() {
            uci_set("uhttpd.main.cert", "/etc/uhttpd.crt").await?;
            uci_set("uhttpd.main.key", "/etc/uhttpd.key").await?;
        } else {
            uci_delete("uhttpd.main.cert").await?;
            uci_delete("uhttpd.main.key").await?;
            uci_delete("uhttpd.main.listen_https").await?;
        }
    }
    Ok(())
}

/// Re-derive uhttpd.main.redirect_https from the certificate uhttpd.main
/// serves: '1' only while that certificate covers this router.
pub async fn apply_redirect_https() -> Result<(), String> {
    let path = uhttpd_cert_path().await;
    let (covers, reason) = cert_covers_router(&path).await;
    let value = if covers { "1" } else { "0" };
    if covers {
        println!("  redirect_https=1 ({path} covers this router)");
    } else {
        println!("  redirect_https=0 ({path} does not: {reason})");
    }
    uci_set("uhttpd.main.redirect_https", value).await?;
    uci_commit("uhttpd").await
}

// ── the operator's opt-out ───────────────────────────────────────────

fn ssl_opted_out() -> bool {
    std::fs::metadata(opt_out_file())
        .map(|m| !m.is_dir())
        .unwrap_or(false)
}

fn mark_opt_out(summary: &str) -> Result<(), String> {
    let dir = ssl_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create SSL dir: {e}"))?;
    let body = format!(
        "TLS identity removed by an operator: {summary}\n{}\n\
         Written by `tollgate ssl remove`.\n\
         The setup path (uci-defaults 99-tollgate-setup) will not provision a TLS\n\
         identity while this file exists. Run `tollgate ssl apply` to end the opt-out.\n",
        "-".repeat(70)
    );
    std::fs::write(opt_out_file(), body)
        .map_err(|e| format!("failed to record the TLS opt-out: {e}"))
}

fn clear_opt_out() -> Result<(), String> {
    if !ssl_opted_out() {
        return Ok(());
    }
    std::fs::remove_file(opt_out_file())
        .map_err(|e| format!("failed to clear the TLS opt-out: {e}"))?;
    println!(
        "Cleared the TLS opt-out marker ({}): this router will keep its identity across installs again.",
        opt_out_file().display()
    );
    Ok(())
}

// ── remove ───────────────────────────────────────────────────────────

pub async fn remove(yes: bool) -> Result<(), String> {
    if !backup_dir().exists() {
        return Err(format!(
            "no SSL backup found at {}/\n  Either SSL was never applied, or the backup was deleted",
            backup_dir().display()
        ));
    }

    let domain = file_read(&backup_dir().join("ssl.domain"));
    let mode = file_read(&backup_dir().join("ssl.mode"));

    if mode == "self-signed" {
        remove_self_signed(&domain, yes).await
    } else {
        remove_real_cert(&domain, yes).await
    }
}

async fn remove_self_signed(domain: &str, yes: bool) -> Result<(), String> {
    println!("Reverting self-signed SSL configuration for: {domain}");
    println!();
    println!("Changes to revert:");
    println!(
        "  [1] Remove self-signed cert+key from {}/",
        ssl_dir().display()
    );
    println!("  [2] uhttpd: restore previous cert configuration");
    println!("  [3] nodogsplash: remove port 443 allow rule");
    println!();
    if !yes && !ask_confirmation("Revert all?") {
        println!("Aborted.");
        return Ok(());
    }

    let _ = tokio::fs::remove_file(cert_dest()).await;
    let _ = tokio::fs::remove_file(key_dest()).await;
    restore_uhttpd().await?;
    println!("[1] uhttpd cert reverted.");
    remove_port_443_allow().await?;
    println!("[2] nodogsplash firewall updated.");
    uci_commit("uhttpd").await?;
    uci_commit("nodogsplash").await?;
    apply_redirect_https().await?;
    reload_services(false).await?;
    let _ = tokio::fs::remove_dir_all(backup_dir()).await;

    if let Err(e) = mark_opt_out(&format!("self-signed identity for {domain}")) {
        eprintln!("WARNING: {e}");
        eprintln!("  The identity is removed, but a later install may provision a new one.");
    }

    let portal_name = uci_get_or_empty("system.@system[0].hostname").await;
    let portal_name = if portal_name.is_empty() {
        "tollgate".to_string()
    } else {
        portal_name
    };
    println!();
    println!("Done. Self-signed HTTPS removed.");
    println!("  Portal URL: http://{portal_name}.lan/");
    Ok(())
}

async fn remove_real_cert(domain: &str, yes: bool) -> Result<(), String> {
    println!("Reverting SSL configuration for: {domain}");
    println!();
    println!("Changes to revert:");
    println!("  [1] Remove cert+key from {}/", ssl_dir().display());
    println!("  [2] uhttpd: restore previous cert configuration");
    println!("  [3] dnsmasq: remove DNS entry for {domain}");
    println!("  [4] nodogsplash: remove port 443 allow");
    println!();
    if !yes && !ask_confirmation("Revert all?") {
        println!("Aborted.");
        return Ok(());
    }

    let _ = tokio::fs::remove_file(cert_dest()).await;
    let _ = tokio::fs::remove_file(key_dest()).await;
    restore_uhttpd().await?;
    println!("[1] uhttpd cert reverted.");

    remove_dnsmasq_domain(domain).await?;
    println!("[2] Removed dnsmasq entry for {domain}");

    uci_delete_if_exists("nodogsplash.@nodogsplash[0].gatewaydomainname").await?;
    remove_port_443_allow().await?;
    println!("[3] nodogsplash cleaned up (gatewaydomainname removed, port 443 allow removed)");

    uci_commit("uhttpd").await?;
    uci_commit("dhcp").await?;
    uci_commit("nodogsplash").await?;
    apply_redirect_https().await?;
    reload_services(true).await?;
    let _ = tokio::fs::remove_dir_all(backup_dir()).await;

    if let Err(e) = mark_opt_out(&format!("real certificate for {domain}")) {
        eprintln!("WARNING: {e}");
        eprintln!("  The identity is removed, but a later install may provision a new one.");
    }

    println!();
    println!("Done. HTTPS removed. Portal now served over HTTP.");
    Ok(())
}

// ── status / covers ──────────────────────────────────────────────────

pub async fn status() -> Result<(), String> {
    let mode = file_read(&backup_dir().join("ssl.mode"));
    let domain = file_read(&backup_dir().join("ssl.domain"));

    if !cert_dest().exists() {
        println!("SSL: not configured");
        println!("  Run 'tollgate ssl apply' to generate a self-signed certificate");
        println!("  Run 'tollgate ssl apply <cert> [key]' to install a real certificate");
        let path = uhttpd_cert_path().await;
        let (_, reason) = cert_covers_router(&path).await;
        println!("  uhttpd serves: {path}");
        println!("  Coverage      : {reason}");
        if ssl_opted_out() {
            println!(
                "  TLS identity  : removed by the operator ({})",
                opt_out_file().display()
            );
            println!(
                "                  the setup path will not provision one while that marker exists"
            );
            println!("                  run 'tollgate ssl apply' to end the opt-out");
        }
        return Ok(());
    }

    println!("SSL: configured");
    println!("  Mode   : {mode}");
    println!("  Domain : {domain}");
    println!("  Cert   : {}", cert_dest().display());
    println!("  Key    : {}", key_dest().display());
    let (covers, reason) = cert_covers_router(&cert_dest().display().to_string()).await;
    if covers {
        println!("  Coverage: this router ({reason})");
    } else {
        println!("  Coverage: NOT this router ({reason})");
    }

    if let Ok(pem) = tokio::fs::read_to_string(cert_dest()).await {
        if let Some(der) = x509::pem_decode(&pem, "CERTIFICATE") {
            if let Ok(cert) = x509::parse_certificate(&der) {
                println!("  Subject: CN={}", cert.subject_cn);
                println!("  Issuer : CN={}", cert.subject_cn);
                println!("  NotBefore: {}", format_date(cert.not_before));
                println!("  NotAfter : {}", format_date(cert.not_after));
                let now = epoch_now();
                if now > cert.not_after {
                    println!("  WARNING: certificate has EXPIRED");
                } else {
                    println!(
                        "  Days remaining: {}",
                        (cert.not_after - now).div_euclid(86_400)
                    );
                }
                if !cert.dns_sans.is_empty() {
                    println!("  SAN    : {}", cert.dns_sans.join(", "));
                }
            }
        }
    }
    Ok(())
}

/// Exit-code contract: 0 when the certificate covers this router, 1 when it
/// does not — the uci-defaults setup path branches on exactly this.
pub async fn covers(cert_arg: Option<&str>, json: bool) -> i32 {
    let cert_path = match cert_arg {
        Some(p) => p.to_string(),
        None => uhttpd_cert_path().await,
    };
    let (covers, reason) = cert_covers_router(&cert_path).await;

    if json {
        let mut payload = serde_json::json!({
            "success": covers,
            "command": "ssl covers",
            "cert": cert_path,
            "covers": covers,
            "reason": reason,
            "timestamp": epoch_now(),
        });
        if !covers {
            payload["error"] = serde_json::json!(ERR_CERT_DOES_NOT_COVER_ROUTER);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return if covers { 0 } else { 1 };
    }

    if covers {
        println!("covers: yes \u{2014} {reason}");
        0
    } else {
        println!("covers: no \u{2014} {reason}");
        1
    }
}

#[cfg(test)]
mod tests;
