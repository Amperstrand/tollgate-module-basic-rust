//! tollgate-module-basic-rust — main entry point.

use std::sync::Arc;
use tollgate_module_basic_rust::{
    cli, config, http, identity, monitor,
    portal::{self, CaptivePortal},
    session, tracing_setup, wallet, wireless,
};

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
    let db_dir = config::config_dir();
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
    let tokens_file_exists = old_db.exists() && !new_db.exists() && !migration_marker.exists();

    if tokens_file_exists {
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
                tracing::info!(tokens_file = %tokens_file.display(), "gonuts-export completed");
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
                    "gonuts-export not found, starting with empty wallet. Manual: gonuts-export wallet.db tokens.jsonl"
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
    let verifier = Arc::new(wallet::verify::TokenVerifier::new(mint_urls.clone()));
    let rate_limiter = Arc::new(tollgate_module_basic_rust::rate_limiter::RateLimiter::from_env());
    let mut toll_wallet = wallet::TollWallet::new(seed, mint_urls, db_dir.clone());
    for mint in &config_obj.accepted_mints {
        match toll_wallet.ensure_mint(&mint.url).await {
            Ok(()) => tracing::info!(mint = %mint.url, "wallet registered for mint"),
            Err(e) => tracing::warn!(mint = %mint.url, error = %e, "failed to register mint"),
        }
    }

    if tokens_file_exists {
        tracing::info!("importing tokens from gonuts migration");
        let tokens_path = db_dir.join("tokens.jsonl");
        match std::fs::read_to_string(&tokens_path) {
            Ok(content) => {
                let mut imported = 0u64;
                let mut failed = 0u64;
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    match toll_wallet.receive(line).await {
                        Ok(amount) => {
                            tracing::info!(amount, "token imported");
                            imported += amount;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to import token");
                            failed += 1;
                        }
                    }
                }
                tracing::info!(imported, failed, "migration import complete");

                let _ = std::fs::write(
                    &migration_marker,
                    format!(
                        "imported={imported}\nfailed={failed}\ndate={}\n",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                    ),
                );

                if let Err(e) = std::fs::rename(&old_db, db_dir.join("wallet.db.pre-migration")) {
                    tracing::warn!(error = %e, "failed to rename old wallet.db");
                }
                tracing::info!("migration complete: marker written, old wallet.db renamed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to read tokens.jsonl for import");
            }
        }
    }

    // Load persisted sessions from disk (sessions.json) so sessions survive restarts
    let sessions = session::SessionManager::load_from_disk(&config::config_dir());
    tracing::info!(count = sessions.sessions.len(), "sessions loaded from disk");

    #[cfg(not(feature = "embedded-portal"))]
    let portal: Arc<dyn CaptivePortal> = Arc::new(portal::NdsPortal::new());

    #[cfg(feature = "embedded-portal")]
    let portal: Arc<dyn CaptivePortal> = {
        let embedded = portal::embedded::EmbeddedPortal::new();
        if let Err(e) = embedded.install() {
            tracing::warn!(error = %e, "nftables install failed");
        }
        Arc::new(embedded)
    };

    let state = Arc::new(http::AppState {
        config: Arc::new(config_obj),
        identity: Arc::new(identity),
        wallet: Arc::new(tokio::sync::RwLock::new(Some(toll_wallet))),
        sessions: Arc::new(tokio::sync::Mutex::new(sessions)),
        portal: portal.clone(),
        verifier,
        rate_limiter,
    });

    let monitor_handle = {
        let sessions = state.sessions.clone();
        let portal = state.portal.clone();
        monitor::Monitor::new(sessions, portal).start()
    };

    let upstream_handle = {
        let upstream_config = wireless::UpstreamWifiConfig::default();
        let mut mgr = wireless::UpstreamManager::new(upstream_config);
        let wallet_arc = state.wallet.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                wireless::UpstreamWifiConfig::default().scan_interval_seconds,
            ));
            interval.tick().await;
            loop {
                interval.tick().await;
                let token: Option<String> = {
                    let w = wallet_arc.read().await;
                    if let Some(wallet) = w.as_ref() {
                        match wallet.get_balance().await {
                            Ok(0) => None,
                            Ok(balance) => {
                                tracing::debug!(balance, "wallet has balance for upstream payment");
                                None
                            }
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                };
                let action = mgr.tick(token.as_deref()).await;
                if action != wireless::ManagerAction::NoAction {
                    tracing::info!(action = ?action, "upstream manager action");
                }
            }
        })
    };

    // Start HTTP server + CLI socket
    let http_state = state.clone();
    let http_handle = tokio::spawn(async move {
        let app = http::create_router((*http_state).clone());
        let listener = match tokio::net::TcpListener::bind("0.0.0.0:2121").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                tracing::error!(
                    "Port 2121 already in use. Another TollGate process is likely running.\n\
                     Find and kill it:\n\
                     sudo ss -tlnp sport = :2121\n\
                     sudo kill -9 <PID>\n\
                     Or: fuser -k 2121/tcp"
                );
                std::process::exit(1);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to bind 0.0.0.0:2121");
                std::process::exit(1);
            }
        };
        tracing::info!("HTTP server listening on 0.0.0.0:2121");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("HTTP server error");
    });

    let cli_state = state.clone();
    let cli_handle = tokio::spawn(async move {
        if let Err(e) = cli::serve(cli_state).await {
            tracing::error!(error = %e, "CLI socket server error");
        }
    });

    #[cfg(feature = "embedded-portal")]
    let redirect_handle = {
        let redirect_state = state.clone();
        tokio::spawn(async move {
            let app = portal::redirect_server::create_redirect_router((*redirect_state).clone());
            match tokio::net::TcpListener::bind("0.0.0.0:80").await {
                Ok(listener) => {
                    tracing::info!("Port-80 redirect server listening on 0.0.0.0:80");
                    if let Err(e) = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .await
                    {
                        tracing::error!(error = %e, "redirect server error");
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to bind port 80 — redirect server disabled (requires root)"
                    );
                }
            }
        })
    };

    #[cfg(feature = "embedded-portal")]
    let watchdog_handle = {
        let nft = portal::nft_manager::NftManager::new();
        tokio::spawn(async move {
            let mut consecutive_failures = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                match tokio::net::TcpStream::connect("127.0.0.1:2121").await {
                    Ok(_) => {
                        if consecutive_failures > 0 {
                            tracing::info!("HTTP recovered after {consecutive_failures} failures");
                        }
                        consecutive_failures = 0;
                    }
                    Err(_) => {
                        consecutive_failures += 1;
                        tracing::warn!("HTTP health check failed ({consecutive_failures}/3)");
                        if consecutive_failures >= 3 {
                            tracing::error!("HTTP unresponsive 90s — removing nftables table to prevent lockout");
                            let _ = nft.teardown();
                            consecutive_failures = 0;
                        }
                    }
                }
            }
        })
    };

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
    monitor_handle.abort();
    upstream_handle.abort();
    #[cfg(feature = "embedded-portal")]
    redirect_handle.abort();
    #[cfg(feature = "embedded-portal")]
    watchdog_handle.abort();
    tracing::info!("shutdown complete");
}
