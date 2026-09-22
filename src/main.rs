//! tollgate-module-basic-rust — main entry point.

use std::sync::Arc;
use tollgate_module_basic_rust::{
    cli, config, http, identity, lightning_quotes, migration, monitor,
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

    // First-boot gonuts → CDK migration (value-retaining, #12)
    let migration = migration::FirstBootMigration::new(&db_dir);
    if migration.should_run() {
        tracing::info!("detected gonuts bbolt wallet, attempting auto-migration");
        let export_tool = std::env::var("GONUTS_EXPORT_PATH")
            .unwrap_or_else(|_| "/usr/bin/gonuts-export".to_string());
        let exported = tokio::process::Command::new(&export_tool)
            .arg(&migration.old_db)
            .arg(&migration.tokens_file)
            .output()
            .await;
        match exported {
            Ok(o) if o.status.success() => {
                tracing::info!(tokens_file = %migration.tokens_file.display(), "gonuts-export completed")
            }
            Ok(o) => tracing::error!(
                stderr = String::from_utf8_lossy(&o.stderr).to_string(),
                "gonuts-export failed; migration will retry next boot (wallet.db retained)"
            ),
            Err(e) => {
                tracing::error!(error = %e, export_tool = %export_tool, "gonuts-export not found; manual: gonuts-export wallet.db tokens.jsonl — migration will retry next boot (wallet.db retained)")
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

    if migration.should_run() && migration.tokens_file.exists() {
        match migration.import_tokens(&toll_wallet).await {
            Ok(summary) => {
                tracing::info!(
                    imported_sat = summary.imported,
                    failed = summary.failed,
                    skipped_already_imported = summary.skipped_already_imported,
                    "migration import pass complete"
                );
                match migration.finish(summary) {
                    Ok(migration::MigrationFinish::Complete) => tracing::info!(
                        "migration complete: all tokens imported, wallet.db renamed to wallet.db.pre-migration"
                    ),
                    Ok(migration::MigrationFinish::Partial) => tracing::error!(
                        "migration PARTIAL: failed token(s) retained in {}; wallet.db kept as recovery source; retry happens on next boot",
                        migration.journal.display()
                    ),
                    Err(e) => tracing::error!(error = %e, "migration finalize failed; will retry next boot"),
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "migration import pass failed; will retry next boot")
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

    let ln_quotes = Arc::new(lightning_quotes::QuoteStore::load(&config::config_dir()));
    let state = Arc::new(http::AppState {
        config: Arc::new(config_obj),
        identity: Arc::new(identity),
        wallet: Arc::new(tokio::sync::RwLock::new(Some(toll_wallet))),
        sessions: Arc::new(tokio::sync::Mutex::new(sessions)),
        portal: portal.clone(),
        verifier,
        rate_limiter,
        ln_quotes: ln_quotes.clone(),
    });

    {
        let state = state.clone();
        tokio::spawn(async move {
            lightning_quotes::run_monitor(state, ln_quotes, std::time::Duration::from_secs(5)).await
        });
    }

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
