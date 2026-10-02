//! tollgate-module-basic-rust — main entry point.

use std::sync::Arc;
use tollgate_module_basic_rust::{
    cli, config, http, identity, lightning_quotes, migration, monitor, payment_journal,
    portal::{self, CaptivePortal},
    session, tracing_setup, wallet, wireless,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One payment-journal reconciliation pass: decide undecided payments and
/// grant what customers are owed. Shared by the startup run and the
/// periodic re-check.
async fn reconcile_payments_once(state: &Arc<http::AppState>) {
    let cfg_dir = config::config_dir();
    let (report, grants) = {
        let w = state.wallet.read().await;
        let Some(wallet) = w.as_ref() else { return };
        payment_journal::reconcile(&cfg_dir, wallet).await
    };
    for (entry, amount_sat) in grants {
        // CLI fund entries (#46) owe no session/gate: the value lands in
        // the wallet via the same saga recovery; the operator sees the
        // balance. Advance the journal and continue.
        if entry.mac == payment_journal::CLI_FUND_MAC {
            let _ = payment_journal::append_entry(
                &cfg_dir,
                &payment_journal::PaymentEntry {
                    phase: payment_journal::PaymentPhase::ReconcileSpent { amount_sat },
                    ..entry.clone()
                },
            );
            tracing::info!(
                amount_sat,
                "reconciled CLI fund: value recovered into the wallet (no session owed)"
            );
            continue;
        }
        // Grant from the pricing facts frozen at intent time (Codex P2 on
        // #43), not current config.
        let steps = amount_sat / entry.price_per_step.max(1);
        let allotment = steps * entry.step_size.max(1);
        {
            let mut sessions = state.sessions.lock().await;
            sessions.create_session(&entry.mac, allotment, &entry.metric, 3600);
            // save_now: durable before the terminal journal append may
            // advance — debounced save returns Ok without writing (Codex
            // P1 on #43). On failure the entry stays undecided and is
            // retried on the next pass.
            if let Err(e) = sessions.save_now(&config::config_dir()) {
                tracing::error!(mac = %entry.mac, error = %e, "CRITICAL: reconciled session not durable — leaving payment undecided for retry");
                drop(sessions);
                continue;
            }
        }
        if let Err(e) = state.portal.grant_access(&entry.mac).await {
            tracing::warn!(mac = %entry.mac, error = %e, "gate open after payment reconcile failed");
        }
        tracing::info!(
            mac = %entry.mac,
            allotment,
            amount_sat,
            "reconciled payment: session granted for a previously-undecided outcome"
        );
        // Terminal append AFTER the granted session is durable (Codex P1
        // on #43): a crash before this leaves the entry reconcilable again
        // — the same-MAC session overwrite makes the re-grant idempotent.
        let _ = payment_journal::append_entry(
            &cfg_dir,
            &payment_journal::PaymentEntry {
                phase: payment_journal::PaymentPhase::ReconcileSpent { amount_sat },
                ..entry.clone()
            },
        );
    }
    if report.granted_sessions + report.closed_unspent + report.zero_steps + report.undecided > 0 {
        tracing::info!(
            granted = report.granted_sessions,
            closed_unspent = report.closed_unspent,
            zero_steps = report.zero_steps,
            undecided = report.undecided,
            "payment journal reconciled"
        );
    }
}

#[tokio::main]
async fn main() {
    // Multi-call dispatch: no argv args => run the server (this binary is
    // installed as /usr/bin/tollgate-wrt); any args => behave as the
    // `tollgate` operator CLI (a /usr/bin/tollgate symlink points here).
    // Must run before tracing/server init so client mode never touches the
    // socket, the wallet, or the log subscriber.
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() > 1 {
        std::process::exit(cli::client::run(&argv).await);
    }

    // Initialize tracing — must happen before anything else
    tracing_setup::init();

    tracing::info!("RunInitialProbe: tollgate-module-basic-rust v{VERSION} starting");

    // Load config (Go parity: a missing/empty/broken config.json is created
    // from defaults on first boot — EnsureDefaultConfig).
    let config_obj = config::ensure_default_config().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config ensure failed, using built-in defaults");
        config::Config::new_default()
    });
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
    let mut export_outcome = migration::ExportOutcome::NotRun;
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
                tracing::info!(tokens_file = %migration.tokens_file.display(), "gonuts-export completed");
                export_outcome = migration::ExportOutcome::Succeeded;
            }
            Ok(o) => {
                tracing::error!(
                    stderr = String::from_utf8_lossy(&o.stderr).to_string(),
                    "gonuts-export failed; tokens not imported this boot (wallet.db retained)"
                );
                export_outcome = migration::ExportOutcome::Failed;
            }
            Err(e) => {
                // Spawn failure = nothing touched tokens.jsonl this boot, so a
                // manually exported file (MIGRATION.md) stays importable.
                tracing::error!(error = %e, export_tool = %export_tool, "gonuts-export not found; manual: gonuts-export wallet.db tokens.jsonl — migration will retry next boot (wallet.db retained)");
            }
        }
    }

    let seed = match wallet::TollWallet::load_or_create_seed(&seed_path).await {
        Ok(seed) => seed,
        Err(e) => {
            // Remediation guidance is in the error log emitted by
            // load_or_create_seed (seed/mnemonic mismatch or corruption).
            tracing::error!(error = %e, "refusing to start with an unsafe wallet seed state");
            std::process::exit(1);
        }
    };

    // Build wallet with accepted mints from config
    let mint_urls: Vec<String> = config_obj
        .accepted_mints
        .iter()
        .map(|m| m.url.clone())
        .collect();
    let verifier = Arc::new(wallet::verify::TokenVerifier::new(mint_urls.clone()));
    let mint_urls_for_retry = mint_urls.clone();
    let rate_limiter = Arc::new(tollgate_module_basic_rust::rate_limiter::RateLimiter::from_env());
    let mut toll_wallet = wallet::TollWallet::new(seed, mint_urls, db_dir.clone());
    for mint in &config_obj.accepted_mints {
        match toll_wallet.ensure_mint(&mint.url).await {
            Ok(()) => tracing::info!(mint = %mint.url, "wallet registered for mint"),
            Err(e) => tracing::warn!(mint = %mint.url, error = %e, "failed to register mint"),
        }
    }

    if migration::should_import_tokens(
        migration.should_run(),
        migration.tokens_file.exists(),
        export_outcome,
    ) {
        match migration.import_tokens(&toll_wallet).await {
            Ok(summary) => {
                tracing::info!(
                    imported_sat = summary.imported,
                    failed = summary.failed,
                    skipped_already_imported = summary.skipped_already_imported,
                    pending = summary.pending,
                    spent = summary.spent,
                    spent_sat = summary.spent_sat,
                    partially_spent = summary.partially_spent,
                    partially_spent_unspent_sat = summary.partially_spent_unspent_sat,
                    remainder_recovered_sat = summary.remainder_recovered_sat,
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

    // Payment-journal reconciliation (#40): decide every payment left
    // intent-only (crash mid-receive) or timeout-unknown, by asking the
    // mint (NUT-07). Spent => the customer paid => grant the session they
    // are owed; unspent => nothing owed. Runs at startup AND periodically
    // (Codex P1 on #43: a long-running router must not hold recovered
    // value indefinitely without granting — the 504 promised automatic
    // reconciliation). Boot is never blocked; undecided entries retry.
    {
        let state = state.clone();
        tokio::spawn(async move {
            reconcile_payments_once(&state).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // fires immediately; the pass above already ran
            loop {
                tick.tick().await;
                if payment_journal::summarize(&config::config_dir()).needs_reconciliation == 0 {
                    continue;
                }
                reconcile_payments_once(&state).await;
            }
        });
    }

    let monitor_handle = {
        let sessions = state.sessions.clone();
        let portal = state.portal.clone();
        monitor::Monitor::new(sessions, portal).start()
    };

    // Re-register configured mints that failed at boot (mint/WAN down when
    // the process started). ensure_mint is idempotent, so retrying the full
    // configured set until every mint is live heals without a restart —
    // the in-process counterpart of Go's hotplug one-shot service restart.
    let mint_retry_wallet = state.wallet.clone();
    let mint_retry_mints: Vec<String> = mint_urls_for_retry.clone();
    let _mint_retry_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // first tick fires immediately; skip it
        loop {
            interval.tick().await;
            let mut all_live = true;
            for mint in &mint_retry_mints {
                let mut w = mint_retry_wallet.write().await;
                if let Some(wallet) = w.as_mut() {
                    match wallet.ensure_mint(mint).await {
                        Ok(()) => {}
                        Err(e) => {
                            all_live = false;
                            tracing::warn!(mint = %mint, error = %e, "mint registration retry failed");
                        }
                    }
                }
            }
            if all_live {
                tracing::info!("all configured mints registered");
                return;
            }
        }
    });

    // Reseller gate: the upstream manager is the only task that moves
    // wallet funds outbound (upstream purchases paid via wallet.send()).
    // `reseller_mode` defaults to false — ordinary installations must
    // never spawn the money-moving loop.
    let upstream_handle = if state.config.reseller_mode {
        tracing::info!("reseller_mode enabled — starting upstream WiFi manager");
        let upstream_config = wireless::UpstreamWifiConfig::default();
        let mut mgr = wireless::UpstreamManager::new(upstream_config);
        let wallet_arc = state.wallet.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                wireless::UpstreamWifiConfig::default().scan_interval_seconds,
            ));
            interval.tick().await;
            loop {
                interval.tick().await;
                let action = {
                    let w = wallet_arc.read().await;
                    mgr.tick(w.as_ref()).await
                };
                if action != wireless::ManagerAction::NoAction {
                    tracing::info!(action = ?action, "upstream manager action");
                }
            }
        }))
    } else {
        None
    };

    // Start HTTP server + CLI socket
    let http_state = state.clone();
    let http_handle = tokio::spawn(async move {
        let app = http::create_router((*http_state).clone());
        // Go binds ":2121" — dual-stack, so [::1]:2121 answers too (PRTA's
        // backend_url probes the IPv6 loopback). Mirror that: [::] first
        // (dual-stack on Linux), fall back to IPv4-only when IPv6 is off.
        let listener = match tokio::net::TcpListener::bind("[::]:2121").await {
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
                tracing::warn!(error = %e, "IPv6 bind failed, falling back to 0.0.0.0");
                match tokio::net::TcpListener::bind("0.0.0.0:2121").await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to bind 0.0.0.0:2121");
                        std::process::exit(1);
                    }
                }
            }
        };
        tracing::info!("HTTP server listening on :2121 (dual-stack)");
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
    _mint_retry_handle.abort();
    monitor_handle.abort();
    if let Some(upstream_handle) = upstream_handle {
        upstream_handle.abort();
    }
    #[cfg(feature = "embedded-portal")]
    redirect_handle.abort();
    #[cfg(feature = "embedded-portal")]
    watchdog_handle.abort();
    tracing::info!("shutdown complete");
}
