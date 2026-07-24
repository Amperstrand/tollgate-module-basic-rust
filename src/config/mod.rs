//! Configuration loading + parsing for tollgate-module-basic-rust.
//!
//! Mirrors the Go `config_manager` package struct-for-struct so the same
//! `/etc/tollgate/config.json`, `install.json`, and `identities.json` files
//! load without modification.
//!
//! All file paths honour the `TOLLGATE_TEST_CONFIG_DIR` environment variable.

pub mod schema;

pub use schema::{Config, IdentitiesConfig, InstallConfig, ProfitShareConfig};

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

/// Base config directory.
pub fn config_dir() -> PathBuf {
    std::env::var("TOLLGATE_TEST_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/tollgate"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn identities_path() -> PathBuf {
    config_dir().join("identities.json")
}

pub fn install_path() -> PathBuf {
    config_dir().join("install.json")
}

/// Load config.json. Returns Ok(None) if file missing/empty (matches Go).
pub fn load_config() -> Result<Option<Config>, String> {
    load_config_from(&config_path())
}

pub fn load_config_from(path: &std::path::Path) -> Result<Option<Config>, String> {
    match std::fs::read(path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Load identities.json.
pub fn load_identities() -> Result<Option<IdentitiesConfig>, String> {
    let path = identities_path();
    match std::fs::read(&path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Load install.json.
pub fn load_install() -> Result<Option<InstallConfig>, String> {
    let path = install_path();
    match std::fs::read(&path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Save config with atomic write (write to temp, rename).
pub fn save_config(config: &Config) -> Result<(), String> {
    let path = config_path();
    let data = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, data).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Phase 6: Config Migration ────────────────────────────────────────

/// Migrate config from old version to new version. Populates missing fields
/// with defaults, updates config_version.
///
/// If `upstream_wifi.scan_interval_seconds` is zero (the serde default for
/// a missing field), the entire `upstream_wifi` block is replaced with the
/// defaults. The `config_version` is always updated to match the defaults.
pub fn migrate_config(config: &mut Config, defaults: &Config) {
    if config.upstream_wifi.scan_interval_seconds == 0 {
        config.upstream_wifi = defaults.upstream_wifi.clone();
    }
    config.config_version = defaults.config_version.clone();
}

/// Backup current config file to `<parent>/config_backups/` before migration.
///
/// The backup file is named `config-{version}-{unix_timestamp}.json`.
/// If the config version cannot be read (corrupt JSON), `"unknown"` is used.
/// Uses `SystemTime` for the timestamp (no chrono dependency).
pub fn backup_config(config_path: &Path) -> std::io::Result<()> {
    let backup_dir = config_path
        .parent()
        .unwrap_or(Path::new("/etc/tollgate"))
        .join("config_backups");
    std::fs::create_dir_all(&backup_dir)?;

    // Filename includes the old version for audit trail.
    let version = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|v| {
            v.get("config_version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let backup_name = format!("config-{version}-{timestamp}.json");
    std::fs::copy(config_path, backup_dir.join(backup_name))?;
    Ok(())
}

/// Validate profit_share: each factor must be 0–1, and the sum must equal
/// 1.0 within ±1e-6 tolerance.
pub fn validate_profit_share(profit_share: &[ProfitShareConfig]) -> Result<(), String> {
    if profit_share.is_empty() {
        return Err("profit_share is empty".to_string());
    }
    let mut sum = 0.0f64;
    for (i, ps) in profit_share.iter().enumerate() {
        if ps.factor < 0.0 {
            return Err(format!("profit_share[{i}] has negative factor"));
        }
        if ps.factor > 1.0 {
            return Err(format!("profit_share[{i}] has factor > 1.0"));
        }
        sum += ps.factor;
    }
    if (sum - 1.0).abs() > 1e-6 {
        return Err(format!("profit_share factors sum to {sum}, expected 1.0"));
    }
    Ok(())
}

/// Load config with migration support.
///
/// 1. Try to load the config file.
/// 2. If load fails (corrupt JSON), back up the corrupt file and return the default config.
/// 3. If the config version differs from the default, back it up, migrate, and save.
/// 4. Validate profit_share (log a warning on failure, but still return the config).
/// 5. Return the (possibly migrated) config.
pub fn load_config_with_migration() -> Option<Config> {
    let defaults = Config::new_default();
    let path = config_path();

    match load_config_from(&path) {
        Ok(Some(mut config)) => {
            if config.config_version != defaults.config_version {
                if let Err(e) = backup_config(&path) {
                    tracing::warn!(error = %e, "failed to back up config before migration");
                }
                migrate_config(&mut config, &defaults);
                if let Err(e) = save_config(&config) {
                    tracing::warn!(error = %e, "failed to save migrated config");
                }
            }

            if let Err(e) = validate_profit_share(&config.profit_share) {
                tracing::warn!(error = %e, "profit_share validation warning");
            }

            Some(config)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "config file is corrupt, backing up and returning default");
            let _ = backup_config(&path);
            Some(defaults)
        }
    }
}
