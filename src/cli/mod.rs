//! Unix socket CLI server.
//!
//! Listens on /var/run/tollgate.sock (or TOLLGATE_TEST_CONFIG_DIR/tollgate.sock).
//! Mode 0660. Line-delimited JSON request/response.
//!
//! Commands: version, status, "wallet info", "wallet balance"

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Socket path — honors TOLLGATE_TEST_CONFIG_DIR for tests.
pub fn socket_path() -> PathBuf {
    std::env::var("TOLLGATE_TEST_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("tollgate.sock"))
        .unwrap_or_else(|_| PathBuf::from("/var/run/tollgate.sock"))
}

/// Version info returned by the `version` command.
pub fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_COMMIT").unwrap_or("0000000");
    let build_time = option_env!("BUILD_TIME").unwrap_or("unknown");
    let rust_version = option_env!("RUSTC_VERSION").unwrap_or("unknown");

    format!(
        "version: {version}\n\
         commit: {commit}\n\
         build_time: {build_time}\n\
         rust_version: {rust_version}\n\
         openwrt: target={arch}\n",
        arch = std::env::consts::ARCH
    )
}

/// Start the Unix socket CLI server.
pub async fn serve() -> std::io::Result<()> {
    let path = socket_path();

    // Remove stale socket
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    // Create parent dir if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&path)?;

    // Set mode 0660
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    }

    tracing::info!(socket = %path.display(), "CLI Unix socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(stream));
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed on CLI socket");
            }
        }
    }
}

async fn handle_connection(stream: tokio::net::UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let cmd = line.trim();
                let response = handle_command(cmd);
                if let Err(e) = writer.write_all(response.as_bytes()).await {
                    tracing::warn!(error = %e, "write failed on CLI socket");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "read failed on CLI socket");
                break;
            }
        }
    }
}

fn handle_command(cmd: &str) -> String {
    match cmd {
        "version" => version_string(),
        "status" => {
            serde_json::json!({
                "success": true,
                "message": "running"
            })
            .to_string()
                + "\n"
        }
        "wallet info" => {
            serde_json::json!({
                "success": true,
                "message": "no wallet configured yet"
            })
            .to_string()
                + "\n"
        }
        "wallet balance" => {
            serde_json::json!({
                "success": true,
                "message": "0"
            })
            .to_string()
                + "\n"
        }
        _ => {
            serde_json::json!({
                "success": false,
                "error": format!("unknown command: {cmd}")
            })
            .to_string()
                + "\n"
        }
    }
}
