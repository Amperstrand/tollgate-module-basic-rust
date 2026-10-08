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
    // Codex P1 on #43 (thread 4141963797): `ensure_mint` can fail at boot
    // (e.g. saga recovery contacts an unavailable mint), leaving no wallet
    // registered for the mint — undecided payments for it would then hit
    // `WalletNotFound` on every pass and stay undecided forever. The 30s
    // mint-retry loop only covers mints in the CURRENT config and only
    // while some configured mint is failing; retry wallet init here for
    // every undecided entry's PERSISTED mint before its proofs are queried
    // (bounded: one attempt per pass — `ensure_mint` is idempotent and a
    // map lookup once registered).
    {
        let mut undecided_mints: Vec<String> =
            payment_journal::fold_last(&payment_journal::read_journal(&cfg_dir))
                .values()
                .filter(|e| e.phase.needs_reconciliation())
                .map(|e| e.mint.clone())
                .collect();
        undecided_mints.sort();
        undecided_mints.dedup();
        if !undecided_mints.is_empty() {
            let mut w = state.wallet.write().await;
            if let Some(wallet) = w.as_mut() {
                for mint in &undecided_mints {
                    // Failure logs and leaves the entries undecided — the
                    // next pass retries. MintNotAccepted (the entry's mint
                    // left the config after intent time) surfaces here
                    // loudly: that payment needs an operator decision.
                    if let Err(e) = wallet.ensure_mint(mint).await {
                        tracing::warn!(
                            mint,
                            error = %e,
                            "payment reconciliation: wallet init retry failed for an undecided entry's mint"
                        );
                    }
                }
            }
        }
    }
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
            match payment_journal::append_entry(
                &cfg_dir,
                &payment_journal::PaymentEntry {
                    phase: payment_journal::PaymentPhase::ReconcileSpent { amount_sat },
                    ..entry.clone()
                },
            ) {
                Ok(()) => tracing::info!(
                    amount_sat,
                    "reconciled CLI fund: value recovered into the wallet (no session owed)"
                ),
                Err(e) => tracing::error!(
                    error = %e,
                    "CRITICAL: terminal journal append failed for a reconciled CLI fund — entry stays undecided and re-reconciles next pass"
                ),
            }
            continue;
        }
        apply_reconciled_grant(
            state,
            &cfg_dir,
            &entry,
            amount_sat,
            granted_pending_terminal(),
        )
        .await;
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

/// Entry ids whose reconciled grant (session + gate) was applied but whose
/// terminal `ReconcileSpent` append failed (Codex P1 on #43, thread
/// 4141963821): later passes re-run ONLY the append — re-granting would
/// reset the session's usage/expiry on every pass while the append keeps
/// failing. Process-lifetime, not durable: after a restart at most one
/// idempotent re-grant happens before the set repopulates; the journal
/// stays the authority.
fn granted_pending_terminal() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Apply one reconciled grant: durable session, gate open, then the
/// terminal `ReconcileSpent` append — treating an append failure as
/// actionable instead of discarding it (Codex P1 on #43, thread
/// 4141963821). A discarded append error would leave the entry undecided
/// forever while the 60s loop keeps re-granting, resetting the session's
/// usage/expiry each pass. The first failure records the entry in
/// `granted_pending_terminal`; later passes skip the session/gate
/// re-grant and retry ONLY the append until it lands.
async fn apply_reconciled_grant(
    state: &Arc<http::AppState>,
    cfg_dir: &std::path::Path,
    entry: &payment_journal::PaymentEntry,
    amount_sat: u64,
    granted_pending_terminal: &std::sync::Mutex<std::collections::HashSet<String>>,
) {
    // Grant from the pricing facts frozen at intent time (Codex P2 on
    // #43), not current config.
    let steps = amount_sat / entry.price_per_step.max(1);
    let allotment = steps * entry.step_size.max(1);
    let already_granted = granted_pending_terminal.lock().unwrap().contains(&entry.id);
    if !already_granted {
        let grant_applied_durable;
        {
            let mut sessions = state.sessions.lock().await;
            // Snapshot BEFORE granting: on a failed durable save the
            // ENTIRE prior state is restored (Codex P1 on #68) — leaving
            // the granted session in memory without its idempotency key
            // would let the monitor's next usage-tick persist it keyless,
            // and a restart would replenish it again.
            let prior = sessions.snapshot_session(&entry.mac);
            // apply_grant_once (Codex P1 on #58, round 2): the grant id
            // is durable in sessions.json, so a restart between the
            // grant and the terminal append recognizes the applied
            // grant instead of re-creating the session (used=0, fresh
            // expiry — repeated restarts replenished one token's
            // access). Boundary: multiple undecided payments for one MAC
            // need the durable multi-grant ledger (#63).
            grant_applied_durable =
                sessions.apply_grant_once(&entry.mac, allotment, &entry.metric, 3600, &entry.id);
            if grant_applied_durable {
                // save_now: durable before the terminal journal append may
                // advance — debounced save returns Ok without writing (Codex
                // P1 on #43).
                if let Err(e) = sessions.save_now(cfg_dir) {
                    sessions.rollback_session(&entry.mac, prior);
                    tracing::error!(mac = %entry.mac, error = %e, "CRITICAL: reconciled session not durable — rolled back, leaving payment undecided for retry");
                    return;
                }
            }
        }
        // Gate handling differs by which side of a restart we are on:
        // - fresh grant this pass → open the gate (normal path);
        // - grant already durable (restart case) → the session must be
        //   preserved untouched, but the captive-portal/nftables state
        //   died with the old process and nothing re-opens it for
        //   loaded sessions — re-open idempotently (Codex P1 on #68)
        //   BEFORE the terminal append closes recovery.
        // (already_granted from the in-memory set is the same-boot
        // append-only retry: the gate is known open in THIS process.)
        if let Err(e) = state.portal.grant_access(&entry.mac).await {
            // A failed gate (re-)open must NOT terminalize: the paid
            // customer would stay blocked with recovery closed. Return
            // undecided — the durable grant id makes the next pass safe
            // (session preserved, only the gate retried). This is the
            // reconcile-path half of the gate-pending family (#64) and
            // the live path's own contract (gate failure → re-mark,
            // never settle).
            tracing::warn!(mac = %entry.mac, error = %e, "gate open after payment reconcile failed — staying undecided for retry");
            return;
        }
        if grant_applied_durable {
            tracing::info!(
                mac = %entry.mac,
                allotment,
                amount_sat,
                "reconciled payment: session granted for a previously-undecided outcome"
            );
        }
    }
    // Terminal append AFTER the granted session is durable (Codex P1 on
    // #43): a crash before this leaves the entry reconcilable again —
    // the same-MAC session overwrite makes the re-grant idempotent.
    match payment_journal::append_entry(
        cfg_dir,
        &payment_journal::PaymentEntry {
            phase: payment_journal::PaymentPhase::ReconcileSpent { amount_sat },
            ..entry.clone()
        },
    ) {
        Ok(()) => {
            granted_pending_terminal.lock().unwrap().remove(&entry.id);
        }
        Err(e) => {
            granted_pending_terminal
                .lock()
                .unwrap()
                .insert(entry.id.clone());
            tracing::error!(
                id = %entry.id,
                mac = %entry.mac,
                error = %e,
                "CRITICAL: terminal journal append failed after the reconciled grant was applied — retrying the append only (no session reset) on the next pass"
            );
        }
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

    // Keyset hygiene sweep (R10/#13): every 6h per mint, refresh keysets
    // and (1) warn when held keysets expire within 30 days, (2) rotate
    // unspent proofs off keysets the mint retired (same-mint self-swap —
    // value never leaves the mint). Errors log and retry next sweep.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
            interval.tick().await; // immediate first tick: run once at boot
            loop {
                let cfg = state.config.clone();
                let mints: Vec<String> = cfg.accepted_mints.iter().map(|m| m.url.clone()).collect();
                for mint in &mints {
                    let wallet = state.wallet.read().await;
                    if let Some(w) = wallet.as_ref() {
                        match w.keyset_hygiene(mint, 30).await {
                            Ok(h) => {
                                if h.proofs_on_inactive_keysets > 0 {
                                    tracing::warn!(
                                        mint = %mint,
                                        proofs = h.proofs_on_inactive_keysets,
                                        "keyset hygiene: proofs on retired keysets — rotating (same-mint swap)"
                                    );
                                    // rotate takes &self (the mint's wallet
                                    // serializes internally) — keep the READ
                                    // guard so maintenance never blocks
                                    // payments or Lightning (Codex P2 on #57).
                                    match w.rotate_inactive_keyset_proofs(mint).await {
                                        Ok(n) => {
                                            tracing::info!(mint = %mint, rotated = n, "keyset hygiene: rotated proofs to active keyset")
                                        }
                                        Err(e) => {
                                            tracing::warn!(mint = %mint, error = %e, "keyset rotation failed; retrying next sweep")
                                        }
                                    }
                                    continue;
                                }
                                if !h.soonest_expiry_keysets.is_empty() {
                                    tracing::warn!(
                                        mint = %mint,
                                        keysets = ?h.soonest_expiry_keysets,
                                        "keyset hygiene: held keysets expire within 30 days — plan rotation"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::debug!(mint = %mint, error = %e, "keyset hygiene check unavailable (mint unreachable?)")
                            }
                        }
                    }
                }
                interval.tick().await;
            }
        });
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Offline gate double: counts grant_access calls, never fails.
    struct FakePortal {
        grants: std::sync::Mutex<u32>,
        grant_fails: std::sync::atomic::AtomicBool,
    }

    impl FakePortal {
        fn counting() -> Self {
            Self {
                grants: std::sync::Mutex::new(0),
                grant_fails: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl portal::CaptivePortal for FakePortal {
        async fn grant_access(
            &self,
            _mac: &str,
        ) -> Result<(), tollgate_module_basic_rust::error::AppError> {
            if self.grant_fails.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(tollgate_module_basic_rust::error::AppError::Internal(
                    "injected gate failure".to_string(),
                ));
            }
            *self.grants.lock().unwrap() += 1;
            Ok(())
        }
        async fn revoke_access(
            &self,
            _mac: &str,
        ) -> Result<(), tollgate_module_basic_rust::error::AppError> {
            Ok(())
        }
        async fn poll_usage(
            &self,
            _mac: &str,
        ) -> Result<(u64, u64), tollgate_module_basic_rust::error::AppError> {
            Ok((0, 0))
        }
        async fn is_authenticated(&self, _mac: &str) -> bool {
            true
        }
    }

    fn test_state(
        dir: &Path,
        wallet: Option<wallet::TollWallet>,
    ) -> (Arc<http::AppState>, Arc<FakePortal>) {
        let portal = Arc::new(FakePortal::counting());
        let identity =
            Arc::new(identity::MerchantIdentity::from_privkey_hex(&"01".repeat(32)).unwrap());
        let state = Arc::new(http::AppState {
            config: Arc::new(config::Config::new_default()),
            identity,
            wallet: Arc::new(tokio::sync::RwLock::new(wallet)),
            sessions: Arc::new(tokio::sync::Mutex::new(session::SessionManager::new())),
            portal: portal.clone(),
            verifier: Arc::new(wallet::verify::TokenVerifier::new(vec![])),
            rate_limiter: Arc::new(tollgate_module_basic_rust::rate_limiter::RateLimiter::new(
                1000,
            )),
            ln_quotes: Arc::new(lightning_quotes::QuoteStore::load(dir)),
        });
        (state, portal)
    }

    fn undecided_entry(
        id: &str,
        phase: payment_journal::PaymentPhase,
    ) -> payment_journal::PaymentEntry {
        payment_journal::PaymentEntry {
            id: id.to_string(),
            ts: 1,
            token: "not-a-cashu-token".to_string(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            mint: "https://test-mint.example".to_string(),
            price_per_step: 1,
            step_size: 1000,
            metric: "bytes".to_string(),
            phase,
        }
    }

    /// Codex P1 on #43 (thread 4141963797): when boot's `ensure_mint`
    /// failed for an undecided payment's mint, the reconciliation pass
    /// must retry wallet init for that persisted mint instead of hitting
    /// `WalletNotFound` forever. The unparseable token keeps the entry
    /// undecided via a LOCAL parse error (no network), so the wallet
    /// registration is the pass's only assertable effect.
    #[tokio::test]
    #[serial_test::serial]
    async fn reconcile_pass_registers_wallet_for_undecided_entries_mints() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
        let w = wallet::TollWallet::new(
            [7u8; 64],
            vec!["https://test-mint.example".to_string()],
            dir.clone(),
        );
        let (state, _portal) = test_state(&dir, Some(w));
        payment_journal::append_entry(
            &dir,
            &undecided_entry("reg-retry-test", payment_journal::PaymentPhase::Intent),
        )
        .unwrap();

        reconcile_payments_once(&state).await;

        let guard = state.wallet.read().await;
        let registered = guard.as_ref().unwrap().get_balance_by_mint().await.unwrap();
        std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
        assert!(
            registered.iter().any(|(m, _)| m == "https://test-mint.example"),
            "reconciler must (re-)initialize the wallet for an undecided entry's persisted mint, got {registered:?}"
        );
    }

    /// Codex P1 on #43 (thread 4141963821): a terminal-append failure
    /// after the grant was applied must not be discarded — while the
    /// append keeps failing, later passes retry ONLY the append; the
    /// session (usage/expiry) is never reset and the gate is not
    /// re-opened. Once the append lands, the entry terminalizes and the
    /// pending tracking clears. The journal file is made read-only for
    /// one pass (uid != 0) to inject exactly one append failure.
    #[tokio::test]
    async fn terminal_append_failure_retries_append_without_resetting_session() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let (state, portal) = test_state(&dir, None);
        let entry = undecided_entry(
            "append-retry-test",
            payment_journal::PaymentPhase::TimeoutUnknown,
        );
        payment_journal::append_entry(&dir, &entry).unwrap();
        let journal_file = dir.join(payment_journal::PAYMENT_JOURNAL_NAME);
        let pending = std::sync::Mutex::new(std::collections::HashSet::new());

        // Pass 1: the terminal append fails (journal read-only), after the
        // session + gate were applied.
        std::fs::set_permissions(&journal_file, std::fs::Permissions::from_mode(0o444)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        assert!(pending.lock().unwrap().contains("append-retry-test"));
        assert_eq!(*portal.grants.lock().unwrap(), 1);
        {
            let mut sessions = state.sessions.lock().await;
            assert!(sessions.get_session("aa:bb:cc:dd:ee:ff").is_some());
            sessions.update_usage("aa:bb:cc:dd:ee:ff", 400);
        }

        // Pass 2: the append heals — ONLY the append re-runs.
        std::fs::set_permissions(&journal_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        {
            let sessions = state.sessions.lock().await;
            assert_eq!(
                sessions.get_session("aa:bb:cc:dd:ee:ff").unwrap().used,
                400,
                "a terminal-append retry must not recreate (reset) the session"
            );
        }
        assert_eq!(
            *portal.grants.lock().unwrap(),
            1,
            "the gate is not re-opened on an append-only retry"
        );
        assert!(pending.lock().unwrap().is_empty());
        let journal = payment_journal::read_journal(&dir);
        let folded = payment_journal::fold_last(&journal);
        assert!(matches!(
            folded["append-retry-test"].phase,
            payment_journal::PaymentPhase::ReconcileSpent { .. }
        ));
    }
    /// Codex P1 on #58 (round 2): the pending-terminal set is
    /// process-lifetime state — a restart between a failed terminal
    /// append and its retry loses it while the journal still says the
    /// payment needs reconciliation. The durable grant id in
    /// sessions.json must carry the idempotency across the restart:
    /// the session is NOT recreated (used/expiry preserved) and the
    /// gate is not re-opened; only the append is retried.
    #[tokio::test]
    async fn restart_after_failed_terminal_append_does_not_replenish() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let (state, portal) = test_state(&dir, None);
        let entry = undecided_entry(
            "restart-replenish-test",
            payment_journal::PaymentPhase::TimeoutUnknown,
        );
        payment_journal::append_entry(&dir, &entry).unwrap();
        let journal_file = dir.join(payment_journal::PAYMENT_JOURNAL_NAME);
        let pending = std::sync::Mutex::new(std::collections::HashSet::new());

        // Pass 1: the grant applies; the terminal append fails.
        std::fs::set_permissions(&journal_file, std::fs::Permissions::from_mode(0o444)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        {
            let mut sessions = state.sessions.lock().await;
            sessions.update_usage("aa:bb:cc:dd:ee:ff", 400);
        }
        assert_eq!(*portal.grants.lock().unwrap(), 1);

        // "Restart": the process-lifetime set is gone; the journal is
        // still failing. The durable grant id must answer.
        let fresh_pending_after_restart = std::sync::Mutex::new(std::collections::HashSet::new());
        apply_reconciled_grant(&state, &dir, &entry, 5, &fresh_pending_after_restart).await;
        {
            let sessions = state.sessions.lock().await;
            assert_eq!(
                sessions.get_session("aa:bb:cc:dd:ee:ff").unwrap().used,
                400,
                "a restart must not recreate (reset) the session for an already-applied grant"
            );
            assert!(
                sessions.has_grant("aa:bb:cc:dd:ee:ff", "restart-replenish-test"),
                "the applied grant id stays recorded across the restart"
            );
        }
        assert_eq!(
            *portal.grants.lock().unwrap(),
            2,
            "the session is preserved but the gate is idempotently RE-OPENED \
             after a restart (portal state died with the old process — \
             Codex P1 on #68)"
        );

        // Heal: the append lands and terminalizes.
        std::fs::set_permissions(&journal_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &fresh_pending_after_restart).await;
        let journal = payment_journal::read_journal(&dir);
        let folded = payment_journal::fold_last(&journal);
        assert!(matches!(
            folded["restart-replenish-test"].phase,
            payment_journal::PaymentPhase::ReconcileSpent { .. }
        ));
    }
    /// Codex P1 on #68: when the durable session save fails, the ENTIRE
    /// prior state is restored — a granted session left in memory
    /// without its idempotency key could be persisted keyless by the
    /// monitor's next usage tick, and a restart would replenish it.
    #[tokio::test]
    async fn failed_session_save_rolls_the_grant_back_entirely() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let (state, portal) = test_state(&dir, None);
        let entry = undecided_entry(
            "save-rollback-test",
            payment_journal::PaymentPhase::TimeoutUnknown,
        );
        payment_journal::append_entry(&dir, &entry).unwrap();
        let pending = std::sync::Mutex::new(std::collections::HashSet::new());

        // The config dir is read-only: the grant applies in memory, the
        // durable save fails, the whole grant must roll back.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        {
            let sessions = state.sessions.lock().await;
            assert!(
                sessions.get_session("aa:bb:cc:dd:ee:ff").is_none(),
                "the granted session must not survive a failed durable save"
            );
        }
        assert_eq!(*portal.grants.lock().unwrap(), 0);
        assert!(pending.lock().unwrap().is_empty());
        let journal = payment_journal::read_journal(&dir);
        let folded = payment_journal::fold_last(&journal);
        assert!(
            !matches!(
                folded["save-rollback-test"].phase,
                payment_journal::PaymentPhase::ReconcileSpent { .. }
            ),
            "the entry must stay undecided while its grant is not durable"
        );

        // Heal: the next pass grants fresh and lands everything.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        assert_eq!(*portal.grants.lock().unwrap(), 1);
        let sessions = state.sessions.lock().await;
        assert!(sessions.has_grant("aa:bb:cc:dd:ee:ff", "save-rollback-test"));
    }
    /// Codex P1 on #68 (round 4): a FAILED gate (re-)open must not
    /// terminalize the payment — the paid customer would stay blocked
    /// with recovery closed. The entry stays undecided; the next pass
    /// preserves the durable session (grant id) and retries only the
    /// gate, then terminalizes once it opens.
    #[tokio::test]
    async fn failed_gate_open_keeps_the_payment_reconcilable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let (state, portal) = test_state(&dir, None);
        let entry = undecided_entry(
            "gate-retry-test",
            payment_journal::PaymentPhase::TimeoutUnknown,
        );
        payment_journal::append_entry(&dir, &entry).unwrap();
        let pending = std::sync::Mutex::new(std::collections::HashSet::new());

        // Pass 1: the gate is down — the grant must not terminalize.
        portal
            .grant_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        {
            let sessions = state.sessions.lock().await;
            assert!(sessions.has_grant("aa:bb:cc:dd:ee:ff", "gate-retry-test"));
        }
        let journal = payment_journal::read_journal(&dir);
        let folded = payment_journal::fold_last(&journal);
        assert!(
            !matches!(
                folded["gate-retry-test"].phase,
                payment_journal::PaymentPhase::ReconcileSpent { .. }
            ),
            "a failed gate must keep the payment undecided"
        );

        // Pass 2: the gate heals — session preserved, gate retried,
        // terminal append lands.
        portal
            .grant_fails
            .store(false, std::sync::atomic::Ordering::SeqCst);
        apply_reconciled_grant(&state, &dir, &entry, 5, &pending).await;
        assert_eq!(*portal.grants.lock().unwrap(), 1);
        {
            let sessions = state.sessions.lock().await;
            assert!(
                sessions.has_grant("aa:bb:cc:dd:ee:ff", "gate-retry-test"),
                "the session survives the gate retry untouched"
            );
        }
        let journal = payment_journal::read_journal(&dir);
        let folded = payment_journal::fold_last(&journal);
        assert!(matches!(
            folded["gate-retry-test"].phase,
            payment_journal::PaymentPhase::ReconcileSpent { .. }
        ));
    }
}
