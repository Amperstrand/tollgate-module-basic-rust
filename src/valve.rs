//! Valve / gate-control module — calls ndsctl to authorize and deauthorize
//! client MACs. This is the captive-portal integration that grants and
//! revokes network access, mirroring the Go `valve` package.
//!
//! ndsctl is not thread-safe (NoDogSplash can deadlock under concurrent
//! access — issue #387), so all ndsctl invocations are serialized through a
//! global async mutex. The auth operation is idempotent and retried because
//! NoDogSplash may not have registered the client session yet at the moment
//! of the first call (notably in the two-router reseller flow).
//!
//! # Wire behavior
//! - `open_gate(mac)`  → `ndsctl auth <mac>`   (5 attempts, 400 ms backoff)
//! - `close_gate(mac)` → `ndsctl deauth <mac>` (3 attempts, 200 ms backoff)
//!
//! A non-zero exit whose stderr contains "already" or "not found" is treated
//! as success: re-authorizing an authed client and deauthing an absent one
//! are both no-ops from the portal's perspective.

use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Serializes all ndsctl invocations. Held across the full retry sequence
/// (subprocess + backoff sleep) so no two ndsctl calls ever overlap.
///
/// This is `tokio::sync::Mutex` (not `std::sync::Mutex`) because the guard
/// must be held across `.await` points inside an async fn. A `std::sync`
/// guard is `!Send`, which would make the future of `open_gate` `!Send` and
/// break compilation of the axum handler that awaits it.
static NDSCTL_MUTEX: Mutex<()> = Mutex::const_new(());

/// Maximum number of authorize attempts. NoDogSplash may not have a client
/// session registered the instant we try to authorize it; the auth operation
/// is idempotent, so a bounded retry is safe.
const AUTH_MAX_ATTEMPTS: u32 = 5;
/// Backoff between authorize retries.
const AUTH_RETRY_DELAY_MS: u64 = 400;

/// Number of deauthorize attempts and its backoff.
const DEAUTH_MAX_ATTEMPTS: u32 = 3;
const DEAUTH_RETRY_DELAY_MS: u64 = 200;

/// Resolve the ndsctl binary path. Defaults to "ndsctl" on PATH but can be
/// overridden via the `NDSCTL_BIN` environment variable — useful for tests
/// and for deployments where ndsctl lives outside the default PATH.
fn ndsctl_bin() -> String {
    std::env::var("NDSCTL_BIN").unwrap_or_else(|_| "ndsctl".to_string())
}

/// Open the gate: authorize `mac` via `ndsctl auth`, granting internet
/// access. Retries because NoDogSplash may not have registered the client
/// session at the moment of the first attempt.
pub async fn open_gate(mac: &str) -> Result<(), String> {
    let bin = ndsctl_bin();
    run_ndsctl_with_retry(&bin, "auth", mac, AUTH_MAX_ATTEMPTS, AUTH_RETRY_DELAY_MS).await
}

/// Close the gate: deauthorize `mac` via `ndsctl deauth`, revoking internet
/// access.
pub async fn close_gate(mac: &str) -> Result<(), String> {
    let bin = ndsctl_bin();
    run_ndsctl_with_retry(
        &bin,
        "deauth",
        mac,
        DEAUTH_MAX_ATTEMPTS,
        DEAUTH_RETRY_DELAY_MS,
    )
    .await
}

/// Core retry loop around a single ndsctl action. The `bin` argument is the
/// resolved binary path (kept out of the public signature so the public API
/// matches the spec while tests can inject a mock binary path).
///
/// The global `NDSCTL_MUTEX` is held for the entire retry sequence so that a
/// long backoff sleep cannot be interrupted by a concurrent ndsctl call.
async fn run_ndsctl_with_retry(
    bin: &str,
    action: &str,
    mac: &str,
    max_retries: u32,
    delay_ms: u64,
) -> Result<(), String> {
    // Held until the function returns — see NDSCTL_MUTEX docs.
    let _lock = NDSCTL_MUTEX.lock().await;

    for attempt in 0..max_retries {
        let output = Command::new(bin)
            .args([action, mac])
            .output()
            .await
            .map_err(|e| format!("ndsctl {action} failed to start: {e}"))?;

        if output.status.success() {
            tracing::debug!(
                action, mac, attempt = attempt + 1, max_retries,
                "ndsctl succeeded"
            );
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        // ndsctl returns non-zero if the client is already in the requested
        // state or is not present — these are benign for our purposes.
        if stderr.contains("already") || stderr.contains("not found") {
            tracing::debug!(
                action,
                mac,
                attempt = attempt + 1,
                stderr = %stderr.trim(),
                "ndsctl reported benign non-zero state, treating as success"
            );
            return Ok(());
        }

        if attempt + 1 < max_retries {
            tracing::debug!(
                action,
                mac,
                attempt = attempt + 1,
                stderr = %stderr.trim(),
                "ndsctl failed, retrying"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        } else {
            tracing::warn!(
                action,
                mac,
                attempt = attempt + 1,
                stderr = %stderr.trim(),
                "ndsctl failed on final attempt"
            );
        }
    }

    Err(format!(
        "ndsctl {action} {mac} failed after {max_retries} attempts"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Write an executable shell script into `dir` and return its path.
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write mock script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat script").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod script");
        }
        path
    }

    // S1 — module compiles and public API is wired. Exercises the FULL public
    // path (env-var binary resolution -> open_gate -> ndsctl auth) with a mock
    // script that records the action argument. Serialized with the close_gate
    // test below because both touch the global NDSCTL_BIN env var.
    #[tokio::test]
    async fn open_gate_invokes_auth_via_env_bin() {
        let _guard = ACTION_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\necho \"$1\" > \"$(dirname \"$0\")/action\"\nexit 0\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        std::env::set_var("NDSCTL_BIN", bin.to_str().unwrap());

        let res = open_gate("aa:bb:cc:dd:ee:ff").await;
        std::env::remove_var("NDSCTL_BIN");
        assert!(res.is_ok(), "open_gate should succeed, got {:?}", res);

        let action = std::fs::read_to_string(dir.path().join("action"))
            .expect("read action")
            .trim()
            .to_string();
        assert_eq!(action, "auth", "open_gate must invoke ndsctl auth");
    }

    // S2 — happy path: mock ndsctl exits 0 on first try.

    #[tokio::test]
    async fn success_on_first_attempt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = write_script(dir.path(), "ndsctl", "#!/bin/sh\nexit 0\n");
        let res = run_ndsctl_with_retry(bin.to_str().unwrap(), "auth", "aa:bb:cc:dd:ee:ff", 5, 1)
            .await;
        assert!(res.is_ok(), "expected Ok, got {:?}", res);
    }

    // S2b — retry then succeed: mock fails twice, then exits 0. Asserts the
    // loop retried exactly three times by reading the script's own counter.

    #[tokio::test]
    async fn retries_then_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Counter file lives next to the script so it's unique per test.
        let script = "#!/bin/sh\n\
CNT=\"$(dirname \"$0\")/count\"\n\
n=$(cat \"$CNT\" 2>/dev/null || echo 0)\n\
n=$((n + 1))\n\
echo \"$n\" > \"$CNT\"\n\
if [ \"$n\" -lt 3 ]; then\n\
  echo \"transient failure\" >&2\n\
  exit 1\n\
fi\n\
exit 0\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        let res = run_ndsctl_with_retry(bin.to_str().unwrap(), "auth", "11:22:33:44:55:66", 5, 1)
            .await;
        assert!(res.is_ok(), "expected Ok after retry, got {:?}", res);

        let count: u32 = std::fs::read_to_string(dir.path().join("count"))
            .expect("read counter")
            .trim()
            .parse()
            .expect("parse counter");
        assert_eq!(count, 3, "should have invoked ndsctl exactly 3 times");
    }

    // S3 — "already authed" stderr is benign -> Ok.

    #[tokio::test]
    async fn already_authed_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\necho 'Client already authorized' >&2\nexit 1\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        let res = run_ndsctl_with_retry(bin.to_str().unwrap(), "auth", "aa:bb:cc:dd:ee:ff", 3, 1)
            .await;
        assert!(res.is_ok(), "expected Ok for already-authed, got {:?}", res);
    }

    // S4 — "not found" stderr is benign -> Ok (covers deauth of a client
    // that has already left the client table).

    #[tokio::test]
    async fn not_found_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\necho 'Client not found' >&2\nexit 1\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        let res = run_ndsctl_with_retry(bin.to_str().unwrap(), "deauth", "aa:bb:cc:dd:ee:ff", 3, 1)
            .await;
        assert!(res.is_ok(), "expected Ok for not-found, got {:?}", res);
    }

    // S5 — generic persistent failure exhausts retries -> Err with message.

    #[tokio::test]
    async fn exhausts_retries_on_persistent_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\necho 'permission denied' >&2\nexit 1\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        let res = run_ndsctl_with_retry(bin.to_str().unwrap(), "auth", "aa:bb:cc:dd:ee:ff", 3, 1)
            .await;
        assert!(
            res.is_err(),
            "expected Err after exhausting retries, got {:?}",
            res
        );
        let err = res.unwrap_err();
        assert!(
            err.contains("failed after 3 attempts"),
            "error should mention retry exhaustion, got: {err}"
        );
    }

    // S6 — ndsctl binary does not exist -> Err "failed to start".

    #[tokio::test]
    async fn missing_binary_returns_error() {
        let res = run_ndsctl_with_retry(
            "/nonexistent/path/ndsctl-does-not-exist",
            "auth",
            "aa:bb:cc:dd:ee:ff",
            2,
            1,
        )
        .await;
        assert!(res.is_err(), "expected Err for missing binary, got {:?}", res);
        let err = res.unwrap_err();
        assert!(
            err.contains("failed to start"),
            "error should mention start failure, got: {err}"
        );
    }

    // S7 — close_gate exercises the deauth action through the public API
    // (fast: mock exits 0 immediately, backoff never reached).

    #[tokio::test]
    async fn close_gate_uses_deauth_action() {
        let _guard = ACTION_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let script = "#!/bin/sh\necho \"$1\" > \"$(dirname \"$0\")/action\"\nexit 0\n";
        let bin = write_script(dir.path(), "ndsctl", script);
        std::env::set_var("NDSCTL_BIN", bin.to_str().unwrap());

        let res = close_gate("aa:bb:cc:dd:ee:ff").await;
        std::env::remove_var("NDSCTL_BIN");
        assert!(res.is_ok(), "close_gate should succeed, got {:?}", res);

        let action = std::fs::read_to_string(dir.path().join("action"))
            .expect("read action")
            .trim()
            .to_string();
        assert_eq!(action, "deauth", "close_gate must invoke ndsctl deauth");
    }

    // Serialize the two tests that touch the global NDSCTL_BIN env var.
    static ACTION_TEST_LOCK: Mutex<()> = Mutex::const_new(());
}
