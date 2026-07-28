//! Configuration loading + parsing for tollgate-module-basic-rust.
//!
//! Mirrors the Go `config_manager` package struct-for-struct so the same
//! `/etc/tollgate/config.json`, `install.json`, and `identities.json` files
//! load without modification.
//!
//! All file paths honour the `TOLLGATE_TEST_CONFIG_DIR` environment variable.

pub mod schema;

pub use schema::{Config, IdentitiesConfig, InstallConfig};

#[cfg(test)]
mod tests;

use crate::error::ConfigError;
use std::path::PathBuf;

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
pub fn load_config() -> Result<Option<Config>, ConfigError> {
    load_config_from(&config_path())
}

pub fn load_config_from(path: &std::path::Path) -> Result<Option<Config>, ConfigError> {
    match std::fs::read(path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(ConfigError::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Load identities.json.
pub fn load_identities() -> Result<Option<IdentitiesConfig>, ConfigError> {
    let path = identities_path();
    match std::fs::read(&path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(ConfigError::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Load install.json.
pub fn load_install() -> Result<Option<InstallConfig>, ConfigError> {
    let path = install_path();
    match std::fs::read(&path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(ConfigError::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// Save config with atomic write (write to temp, rename).
pub fn save_config(config: &Config) -> Result<(), ConfigError> {
    let path = config_path();
    let data = serde_json::to_vec_pretty(config)?;

    // Atomic write: write to temp file, then rename
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp_path, &path)?;

    Ok(())
}
