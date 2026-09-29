//! Per-mint wallet-drain journal.
//!
//! A Cashu swap is irreversible once the mint accepts it (NUT-03): the token
//! returned by a drain is the only spendable representation of those funds.
//! Without a journal, that token exists solely in the aggregate CLI response,
//! so a later per-mint failure (or a crash) destroys access to the funds.
//! Journaling immediately after each success keeps an independent second copy
//! for exactly that case. Mirrors Go `src/cli/drain_journal.go`.
//!
//! Entries are never removed by the service: tokens are bearer instruments
//! and the journal file is 0600. Operators sweep the file once the tokens
//! are secured elsewhere.

use std::io::Write;
use std::path::PathBuf;

fn journal_path() -> PathBuf {
    crate::config::config_dir().join("wallet-drain-journal.jsonl")
}

/// Append one drained-token entry and fsync it before the next mint is
/// attempted. Errors are returned so the caller can stop draining further
/// mints instead of losing the only durable copy of a later token.
pub fn append(mint_url: &str, amount_sats: u64, token: &str) -> std::io::Result<()> {
    let path = journal_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let entry = serde_json::json!({
        "timestamp": now_epoch_secs(),
        "mint_url": mint_url,
        "amount_sats": amount_sats,
        "token": token,
    });

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;

    let mut file = file;
    file.write_all(format!("{entry}\n").as_bytes())?;
    file.sync_all()
}

fn now_epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn append_writes_jsonl_entries_to_test_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", dir.path());

        append("https://mint.example", 100, "token-a").unwrap();
        append("https://other.example", 7, "token-b").unwrap();

        let content =
            std::fs::read_to_string(dir.path().join("wallet-drain-journal.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["mint_url"], "https://mint.example");
        assert_eq!(first["amount_sats"], 100);
        assert_eq!(first["token"], "token-a");
        assert!(first["timestamp"].as_f64().is_some());

        std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    }
}
