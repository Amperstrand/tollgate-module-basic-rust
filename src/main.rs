//! tollgate-module-basic-rust — main entry point.

use std::path::PathBuf;
use std::sync::Arc;
use tollgate_module_basic_rust::{cli, config, http, identity, session, tracing_setup, wallet};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    // Initialize tracing — must happen before anything else
    tracing_setup::init();

    tracing::info!("RunInitialProbe: tollgate-module-basic-rust v{VERSION} starting");

    // Load config
    let config_obj = config::load_config().unwrap_or(None).unwrap_or_default();
    tracing::info!(
        metric = %config_obj.metric,
        mints = config_obj.accepted_mints.len(),
        "config loaded"
    );

    // Load or generate merchant identity
    let identity = identity::MerchantIdentity::load_or_generate()
        .expect("failed to load/generate merchant identity");
    tracing::info!(pubkey = %identity.pubkey_hex(), "merchant identity loaded");

    // Load or generate wallet seed
    let db_dir = PathBuf::from("/etc/tollgate");
    let seed_path = db_dir.join("wallet_seed.bin");
    if let Some(parent) = seed_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // First-boot auto-migration: if gonuts bbolt wallet.db exists AND CDK
    // wallet.sqlite does NOT exist AND migration marker is absent:
    // 1. Run gonuts-export → tokens.jsonl
    // 2. Import tokens via wallet.receive()
    // 3. Write .migration_complete marker
    // 4. Rename wallet.db → wallet.db.pre-migration
    let old_db = db_dir.join("wallet.db");
    let new_db = db_dir.join("wallet.sqlite");
    let migration_marker = db_dir.join(".migration_complete");

    if old_db.exists() && !new_db.exists() && !migration_marker.exists() {
        tracing::info!("detected gonuts bbolt wallet, attempting auto-migration");
        let export_tool = std::env::var("GONUTS_EXPORT_PATH")
            .unwrap_or_else(|_| "/usr/bin/gonuts-export".to_string());
        let tokens_file = db_dir.join("tokens.jsonl");

        let export_result = tokio::process::Command::new(&export_tool)
            .arg(&old_db)
            .arg(&tokens_file)
            .output()
            .await;

        match export_result {
            Ok(output) if output.status.success() => {
                tracing::info!("gonuts-export completed, importing tokens");
                // Tokens will be imported on next CLI `migrate` command
                // or when mint connectivity allows wallet.receive()
                tracing::info!(tokens_file = %tokens_file.display(), "tokens exported, run 'migrate <path>' via CLI to import");
            }
            Ok(output) => {
                tracing::warn!(
                    stderr = String::from_utf8_lossy(&output.stderr).to_string(),
                    "gonuts-export failed, starting with empty wallet"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    export_tool = %export_tool,
                    "gonuts-export not found or failed, starting with empty wallet. \
                     Operator can run migration manually: gonuts-export wallet.db tokens.jsonl && \
                     echo 'migrate /etc/tollgate/tokens.jsonl' | nc -U /var/run/tollgate.sock"
                );
            }
        }
    }

    let seed = wallet::TollWallet::load_or_create_seed(&seed_path)
        .await
        .expect("failed to load/create wallet seed");

    // Build wallet with accepted mints from config
    let mint_urls: Vec<String> = config_obj
        .accepted_mints
        .iter()
        .map(|m| m.url.clone())
        .collect();
    let mut toll_wallet = wallet::TollWallet::new(seed, mint_urls, db_dir.clone());
    for mint in &config_obj.accepted_mints {
        match toll_wallet.ensure_mint(&mint.url).await {
            Ok(()) => tracing::info!(mint = %mint.url, "wallet registered for mint"),
            Err(e) => tracing::warn!(mint = %mint.url, error = %e, "failed to register mint"),
        }
    }

    // Build app state
    let state = Arc::new(http::AppState {
        config: Arc::new(config_obj),
        identity: Arc::new(identity),
        wallet: Arc::new(tokio::sync::Mutex::new(Some(toll_wallet))),
        sessions: Arc::new(tokio::sync::Mutex::new(session::SessionManager::new())),
    });

    // Start HTTP server + CLI socket
    let http_state = state.clone();
    let http_handle = tokio::spawn(async move {
        let app = http::create_router((*http_state).clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:2121")
            .await
            .expect("failed to bind 127.0.0.1:2121");
        tracing::info!("HTTP server listening on 127.0.0.1:2121");
        axum::serve(listener, app).await.expect("HTTP server error");
    });

    let cli_state = state.clone();
    let cli_handle = tokio::spawn(async move {
        if let Err(e) = cli::serve(cli_state).await {
            tracing::error!(error = %e, "CLI socket server error");
        }
    });

    // Wait for shutdown signal
    let shutdown_int = tokio::signal::ctrl_c();

    tokio::pin!(shutdown_int);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");

    tokio::select! {
        _ = shutdown_int => {
            tracing::info!("SIGINT received, shutting down");
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received, shutting down");
        }
    }

    // Cleanup
    let socket_path = cli::socket_path();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    http_handle.abort();
    cli_handle.abort();
    tracing::info!("shutdown complete");
}
