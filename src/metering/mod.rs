//! Metering — ndsctl integration for usage polling.
//!
//! Calls `ndsctl state <mac>` to get current download/upload bytes for a
//! client. Falls back to (0, 0) if ndsctl is unavailable (graceful
//! degradation for non-OpenWrt dev environments).

use thiserror::Error;

/// Errors from metering operations.
#[derive(Debug, Error)]
pub enum MeteringError {
    #[error("ndsctl not found: {0}")]
    NotFound(String),
    #[error("ndsctl failed: {0}")]
    ExecutionFailed(String),
    #[error("parse error: {0}")]
    ParseError(String),
}

/// Parse ndsctl output to extract used (download + upload) bytes.
/// Expected format lines like:
/// ```text
/// download: 1234567
/// upload: 7654321
/// ```
/// Returns (used, total) where `used` = download + upload and
/// `total` = 0 (ndsctl doesn't report total allotment).
pub fn parse_ndsctl_output(output: &str) -> Result<(u64, u64), MeteringError> {
    let mut download: u64 = 0;
    let mut upload: u64 = 0;

    for line in output.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("download:") {
            download = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("upload:") {
            upload = val.trim().parse().unwrap_or(0);
        }
    }

    Ok((download + upload, 0))
}

/// Poll usage for a client MAC via `ndsctl state <mac>`.
/// Returns (used_bytes, total_bytes). On any error, returns (0, 0).
pub async fn poll_usage(mac: &str) -> Result<(u64, u64), MeteringError> {
    let output = tokio::process::Command::new("ndsctl")
        .args(["state", mac])
        .output()
        .await
        .map_err(|e| MeteringError::NotFound(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MeteringError::ExecutionFailed(stderr.to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ndsctl_output(&stdout)
}

#[cfg(test)]
mod tests;
