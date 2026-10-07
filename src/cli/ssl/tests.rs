//! Tests for the client-side `ssl` commands. Router integration (uci, px5g,
//! init scripts) is exercised through PATH-stubbed executables so the whole
//! apply/covers flow runs on the build host.

use super::*;

const GOOD_CERT_PEM: &str = include_str!("../../../tests/fixtures/ssl/good.crt");
const GOOD_KEY_PEM: &str = include_str!("../../../tests/fixtures/ssl/good.key");
const NOSAN_CERT_PEM: &str = include_str!("../../../tests/fixtures/ssl/nosan.crt");
const NOSAN_KEY_PEM: &str = include_str!("../../../tests/fixtures/ssl/nosan.key");

fn uci_available() -> bool {
    std::process::Command::new("which")
        .arg("sh")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A stateful `uci` stub: `set`/`add_list` persist values under
/// `$UCI_STATE/<key>`, `-q get` prints them back, everything else no-ops.
fn write_uci_stub(bin_dir: &Path) {
    let script = r#"#!/bin/sh
echo "$@" >> "$UCI_LOG"
case "$1" in
  -q)
    case "$2" in
      get) key="$3"; if [ -f "$UCI_STATE/$key" ]; then cat "$UCI_STATE/$key"; fi; exit 0;;
      delete|del_list) exit 0;;
    esac
    ;;
  set) k="${2%%=*}"; v="${2#*=}"; printf '%s' "$v" > "$UCI_STATE/$k"; exit 0;;
  add_list) k="${2%%=*}"; v="${2#*=}"; printf '%s\n' "$v" >> "$UCI_STATE/$k"; exit 0;;
  show) exit 0;;
esac
exit 0
"#;
    std::fs::write(bin_dir.join("uci"), script).unwrap();
    make_executable(bin_dir.join("uci"));
}

fn write_px5g_stub(bin_dir: &Path) {
    let script = r#"#!/bin/sh
out=""; keyout=""; prev=""
for a in "$@"; do
  case "$prev" in
    -out) out="$a" ;;
    -keyout) keyout="$a" ;;
  esac
  prev="$a"
done
cp "$PX5G_FIXTURE_CERT" "$out"
cp "$PX5G_FIXTURE_KEY" "$keyout"
"#;
    std::fs::write(bin_dir.join("px5g"), script).unwrap();
    make_executable(bin_dir.join("px5g"));
}

#[cfg(unix)]
fn make_executable(path: PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn make_executable(_path: PathBuf) {}

struct StubEnv {
    old_path: String,
    bin_dir: PathBuf,
}

impl StubEnv {
    fn install() -> Self {
        let bin_dir =
            std::env::temp_dir().join(format!("tollgate-ssl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bin_dir);
        std::fs::create_dir_all(&bin_dir).unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), old_path));
        StubEnv { old_path, bin_dir }
    }

    fn with_uci() -> Self {
        let env = StubEnv::install();
        write_uci_stub(&env.bin_dir);
        env
    }
}

impl Drop for StubEnv {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.old_path);
        let _ = std::fs::remove_dir_all(&self.bin_dir);
    }
}

fn good_summary() -> CertSummary {
    let der = x509::pem_decode(GOOD_CERT_PEM, "CERTIFICATE").unwrap();
    x509::parse_certificate(&der).unwrap()
}

#[test]
fn parse_lan_ip_strips_cidr_and_rejects_garbage() {
    assert_eq!(
        parse_lan_ip("192.168.1.1/24"),
        Some("192.168.1.1".parse::<IpAddr>().unwrap())
    );
    assert_eq!(
        parse_lan_ip(" 10.0.0.5 "),
        Some("10.0.0.5".parse().unwrap())
    );
    assert_eq!(parse_lan_ip("not-an-ip"), None);
    assert_eq!(parse_lan_ip(""), None);
}

#[test]
fn cert_coverage_sans_cover_host_and_ip() {
    let c = good_summary();
    let hosts = vec!["tollgate-test".to_string(), "tollgate-test.lan".to_string()];
    let ips = vec!["192.0.2.10".to_string()];

    let (ok, why) = cert_coverage(&c, &hosts, &ips, c.not_before + 1);
    assert!(ok, "{why}");
    assert!(why.contains("SANs cover"));

    let (ok, why) = cert_coverage(&c, &["other.lan".to_string()], &[], c.not_before + 1);
    assert!(!ok);
    assert!(why.contains("cover none of this router's names"), "{why}");
    assert!(why.contains("DNS:tollgate-test.lan"));
}

#[test]
fn cert_coverage_expired_and_no_names() {
    let c = good_summary();
    let (ok, why) = cert_coverage(&c, &["tollgate-test.lan".to_string()], &[], c.not_after + 1);
    assert!(!ok);
    assert!(why.starts_with("certificate expired on"));

    let (ok, why) = cert_coverage(&c, &[], &[], c.not_before + 1);
    assert!(!ok);
    assert!(why.contains("no hostname or LAN IP"));
}

#[test]
fn cert_coverage_cn_without_san_is_not_coverage() {
    let der = x509::pem_decode(NOSAN_CERT_PEM, "CERTIFICATE").unwrap();
    let c = x509::parse_certificate(&der).unwrap();
    let (ok, why) = cert_coverage(&c, &["plain.example".to_string()], &[], c.not_before + 1);
    assert!(!ok);
    assert!(why.contains("CN:plain.example (no SAN extension)"), "{why}");
}

#[test]
fn pem_encode_roundtrips() {
    let der = x509::pem_decode(GOOD_CERT_PEM, "CERTIFICATE").unwrap();
    let pem = pem_encode(&der);
    let back = x509::pem_decode(
        &format!("-----BEGIN CERTIFICATE-----\n{pem}-----END CERTIFICATE-----"),
        "CERTIFICATE",
    )
    .unwrap();
    assert_eq!(der, back);
}

#[tokio::test]
#[serial_test::serial]
async fn covers_uses_live_uci_names_and_exit_code() {
    if !uci_available() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let cert_path = tmp.path().join("server.crt");
    std::fs::write(&cert_path, GOOD_CERT_PEM).unwrap();
    let nosan_path = tmp.path().join("nosan.crt");
    std::fs::write(&nosan_path, NOSAN_CERT_PEM).unwrap();

    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("network.lan.ipaddr"), "192.0.2.10").unwrap();
    std::fs::write(state.join("system.@system[0].hostname"), "tollgate-test").unwrap();
    std::env::set_var("UCI_STATE", &state);
    std::env::set_var("UCI_LOG", tmp.path().join("uci.log"));

    let stub = StubEnv::with_uci();

    let code = covers(Some(cert_path.to_str().unwrap()), false).await;
    assert_eq!(code, 0, "fixture SANs cover tollgate-test.lan + 192.0.2.10");

    let code = covers(Some(nosan_path.to_str().unwrap()), false).await;
    assert_eq!(code, 1, "CN without SAN is not coverage");

    let code = covers(
        Some(tmp.path().join("missing.crt").to_str().unwrap()),
        false,
    )
    .await;
    assert_eq!(code, 1, "missing cert fails closed");

    drop(stub);
    std::env::remove_var("UCI_STATE");
    std::env::remove_var("UCI_LOG");
}

#[tokio::test]
#[serial_test::serial]
async fn apply_self_signed_via_px5g_stub_installs_identity() {
    if !uci_available() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &cfg_dir);
    std::env::set_var("UCI_STATE", &state);
    std::env::set_var("UCI_LOG", tmp.path().join("uci.log"));
    std::env::set_var("PX5G_FIXTURE_CERT", tmp.path().join("g.crt"));
    std::env::set_var("PX5G_FIXTURE_KEY", tmp.path().join("g.key"));
    std::fs::write(tmp.path().join("g.crt"), GOOD_CERT_PEM).unwrap();
    std::fs::write(tmp.path().join("g.key"), GOOD_KEY_PEM).unwrap();
    std::fs::write(state.join("network.lan.ipaddr"), "192.0.2.10").unwrap();
    std::fs::write(state.join("system.@system[0].hostname"), "tollgate-test").unwrap();

    let stub = StubEnv::with_uci();
    write_px5g_stub(&stub.bin_dir);

    apply(&[], true, true)
        .await
        .expect("stubbed apply must succeed");

    assert!(cfg_dir.join("ssl/server.crt").exists());
    assert!(cfg_dir.join("ssl/server.key").exists());
    assert!(cfg_dir.join("ssl/backup/ssl.mode").exists());
    let mode = std::fs::read_to_string(cfg_dir.join("ssl/backup/ssl.mode")).unwrap();
    assert_eq!(mode.trim(), "self-signed");

    let cert_val = std::fs::read_to_string(state.join("uhttpd.main.cert")).unwrap();
    assert_eq!(
        cert_val.trim(),
        cfg_dir.join("ssl/server.crt").display().to_string()
    );
    let redirect = std::fs::read_to_string(state.join("uhttpd.main.redirect_https")).unwrap();
    assert_eq!(
        redirect.trim(),
        "1",
        "generated cert covers tollgate-test.lan"
    );
    let listen = std::fs::read_to_string(state.join("uhttpd.main.listen_https")).unwrap();
    assert!(listen.contains("0.0.0.0:443"));
    assert!(listen.contains("[::]:443"));

    let log = std::fs::read_to_string(tmp.path().join("uci.log")).unwrap();
    assert!(log.contains("commit uhttpd"));
    assert!(log.contains("commit nodogsplash"));

    drop(stub);
    for var in [
        "UCI_STATE",
        "UCI_LOG",
        "PX5G_FIXTURE_CERT",
        "PX5G_FIXTURE_KEY",
    ] {
        std::env::remove_var(var);
    }
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn remove_without_backup_fails() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", tmp.path());
    let err = remove(true).await.expect_err("no backup => error");
    assert!(err.contains("no SSL backup found"), "{err}");
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

#[tokio::test]
#[serial_test::serial]
async fn status_not_configured_reports_uhttpd_cert() {
    if !uci_available() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &cfg_dir);
    std::env::set_var("UCI_STATE", &state);
    std::env::set_var("UCI_LOG", tmp.path().join("uci.log"));
    std::fs::write(state.join("uhttpd.main.cert"), "/etc/uhttpd.crt").unwrap();
    std::fs::write(state.join("system.@system[0].hostname"), "tollgate-test").unwrap();
    std::fs::write(state.join("network.lan.ipaddr"), "192.0.2.10").unwrap();

    let stub = StubEnv::with_uci();

    status().await.expect("status must not fail");

    drop(stub);
    std::env::remove_var("UCI_STATE");
    std::env::remove_var("UCI_LOG");
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
}

/// Codex P2 on #54: `ssl apply <cert> <key>` must reject an unparseable or
/// mismatched pair BEFORE installing anything or touching uhttpd — a pair
/// uhttpd cannot start with leaves HTTPS dead (silently with --no-restart).
#[tokio::test]
#[serial_test::serial]
async fn apply_real_cert_rejects_mismatched_pair_before_install() {
    if !uci_available() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &cfg_dir);
    std::env::set_var("UCI_STATE", &state);
    std::env::set_var("UCI_LOG", tmp.path().join("uci.log"));
    std::fs::write(state.join("network.lan.ipaddr"), "192.0.2.10").unwrap();
    std::fs::write(state.join("system.@system[0].hostname"), "tollgate-test").unwrap();

    let cert_path = tmp.path().join("server.crt");
    std::fs::write(&cert_path, GOOD_CERT_PEM).unwrap();
    // nosan.key is a valid RSA key for a DIFFERENT certificate.
    let key_path = tmp.path().join("other.key");
    std::fs::write(&key_path, NOSAN_KEY_PEM).unwrap();
    let garbage_key = tmp.path().join("garbage.key");
    std::fs::write(&garbage_key, "not a pem key at all").unwrap();

    let stub = StubEnv::with_uci();

    let err = apply(
        &[
            cert_path.to_string_lossy().into(),
            key_path.to_string_lossy().into(),
        ],
        true,
        true,
    )
    .await
    .expect_err("mismatched pair must be rejected");
    assert!(
        err.contains("certificate/key pair check failed"),
        "expected the pairing error, got: {err}"
    );

    let err = apply(
        &[
            cert_path.to_string_lossy().into(),
            garbage_key.to_string_lossy().into(),
        ],
        true,
        true,
    )
    .await
    .expect_err("garbage key must be rejected");
    assert!(
        err.contains("not a valid PEM private key"),
        "expected the PEM error, got: {err}"
    );

    assert!(
        !cfg_dir.join("ssl/server.crt").exists(),
        "nothing may be installed for a rejected pair"
    );
    assert!(
        !cfg_dir.join("ssl/server.key").exists(),
        "nothing may be installed for a rejected pair"
    );

    drop(stub);
    std::env::remove_var("UCI_STATE");
    std::env::remove_var("UCI_LOG");
}
