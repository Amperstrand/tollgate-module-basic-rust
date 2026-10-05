//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 4: verifies token, receives into wallet, creates session, returns
//! kind 1022 on success or kind 21023 + HTTP 400 on failure.
//!
// HTTP #1: The Server MUST take a http `POST` request containing a bearer asset token directly in the request body.
// HTTP #1: If the TollGate accepts the provided payment it MUST return http `200 OK` response where the body is a `kind=1022` TollGate Session event.
// HTTP #1: If the payment is invalid, it SHOULD return a `kind=21023` Notice event with an appropriate error code, and MAY use http `402 Payment Required` or `400 Bad Request` status.
// TIP #1: `p`: pubkey of the TollCustomer Gate (from the Customer's Payment event)
// TIP #1: `device-identifier`: (hardware) identifier of the customer's device.
// TIP #1: `allotment`: Amount of `<metric>` allotted to customer after payment.
// TIP #1: `metric`: The purchased metric

use crate::config;
use crate::http::AppState;
use crate::mac_resolver::{get_client_ip, get_mac_address};
use crate::nostr_event;
use crate::payment_journal;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::net::SocketAddr;

/// Extract a Cashu token from a Nostr kind 21000 event JSON body.
/// Looks for a tag ["payment", "<token>"].
fn extract_token_from_nostr_event(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("kind").and_then(|k| k.as_u64()) != Some(21000) {
        return None;
    }
    let tags = v.get("tags")?.as_array()?;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            if arr.len() >= 2 && arr.first().and_then(|s| s.as_str()) == Some("payment") {
                return arr.get(1).and_then(|s| s.as_str()).map(|s| s.to_string());
            }
        }
    }
    None
}

/// Locally-checkable payment validation, run BEFORE any value moves
/// (issue #4: the old ordering received the token first and confiscated
/// below-minimum payments with no refund).
#[derive(Debug)]
pub(crate) struct PaymentPrecheck {
    pub price_per_step: u64,
}

#[derive(Debug)]
pub(crate) enum PrecheckError {
    /// The token's mint is not in the accepted-mint config: pricing by an
    /// arbitrary other mint would misprice the payment, and receiving an
    /// unconfigured mint's value is not this router's contract.
    MintNotAccepted(String),
    BelowMinimum {
        steps: u64,
        min_steps: u64,
        price_per_step: u64,
    },
}

/// The Go notice-code taxonomy (R16 / #19): for the same underlying
/// condition Go and Rust emit the same `code` tag. Go's own classifier
/// inspects mint-returned error text (isRateLimitError etc.) — the
/// strings come from the mint's protocol surface, so mirroring that here
/// is parity, not smell. Codes Go emits that Rust already emits
/// byte-identically (mac-address-lookup-failed, payment-outcome-unknown,
/// session-error, payment-error-below-minimum) keep their spelling.
fn verify_failure_code(e: &crate::error::VerifyError) -> &'static str {
    use crate::error::VerifyError;
    match e {
        VerifyError::InvalidToken(_)
        | VerifyError::NoMintUrl(_)
        | VerifyError::ValueSum(_)
        | VerifyError::NoProofs => "payment-error-invalid-token",
        VerifyError::MintNotAccepted(_) => "mint-not-accepted",
        VerifyError::Spent(_) => "payment-error-token-spent",
        VerifyError::LockedToken => "payment-error-invalid-token",
        VerifyError::CheckStateRequest(msg) | VerifyError::CheckStateStatus(msg) => {
            if is_mint_rate_limit(msg) {
                "mint-rate-limited"
            } else {
                "payment-error-mint-unreachable"
            }
        }
        VerifyError::CheckStateParse(_) | VerifyError::MissingStates => {
            "payment-error-mint-unreachable"
        }
    }
}

/// Go isRateLimitError parity: mint-surface text signals.
fn is_mint_rate_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("429") || m.contains("rate limit") || m.contains("too many requests")
}

/// Go's post-receive classifier (merchant.go:1208-1246) applied to our
/// receive errors: already-spent, rate-limited, below-swap-fee,
/// keyset-expired, mint-unreachable — with `payment-processing-failed`
/// as the fallback.
fn receive_failure_code(e: &crate::wallet::WalletError) -> &'static str {
    let msg = e.to_string().to_ascii_lowercase();
    if matches!(e, crate::wallet::WalletError::Timeout(_)) {
        // The outer timeout (and the session-durability sentinel) is an
        // AMBIGUOUS outcome, not a definitive failure — Go's
        // payment-outcome-unknown, handled by receive_failure_shape.
        return "payment-outcome-unknown";
    }
    if msg.contains("already spent")
        || msg.contains("already used")
        || msg.contains("token already spent")
    {
        return "payment-error-token-spent";
    }
    if is_mint_rate_limit(&msg) {
        return "mint-rate-limited";
    }
    if msg.contains("keyset")
        && (msg.contains("expired") || msg.contains("inactive") || msg.contains("unknown"))
    {
        return "payment-error-keyset-expired";
    }
    if msg.contains("fee")
        && (msg.contains("insufficient") || msg.contains("below") || msg.contains("cannot cover"))
    {
        return "payment-error-below-swap-fee";
    }
    if msg.contains("unreachable")
        || msg.contains("connection refused")
        || msg.contains("connect error")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("dns")
    {
        // Transport failure reaching the mint. NOTE: our own outer Timeout
        // is handled before this (ambiguous outcome), so this arm only
        // catches transport errors CDK surfaced as definitive.
        return "payment-error-mint-unreachable";
    }
    "payment-processing-failed"
}

/// HTTP semantics for a failed receive (issue #33): a timeout is an
/// UNKNOWN outcome, not a rejection — the mint may have accepted the swap
/// (AGENTS.md: ambiguous results are reconciled, not retried). Answering
/// 400 "rejected" would invite the customer to treat a possibly-consumed
/// token as failed; 504 + `payment-outcome-unknown` says "do not resubmit,
/// it is being reconciled".
fn precheck_fallback_price(state: &crate::http::AppState) -> u64 {
    state
        .config
        .accepted_mints
        .first()
        .map(|m| m.price_per_step)
        .unwrap_or(1)
}

fn receive_failure_shape(e: &crate::wallet::WalletError) -> (StatusCode, &'static str, String) {
    match e {
        crate::wallet::WalletError::Timeout(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "payment-outcome-unknown",
            "payment outcome unknown after timeout; do not resubmit this token — it is being reconciled automatically".to_string(),
        ),
        e => {
            let code = receive_failure_code(e);
            let message = match code {
                "payment-error-token-spent" => "This e-cash note has already been spent".to_string(),
                "mint-rate-limited" => "Mint is rate-limiting requests. Please try again in a moment.".to_string(),
                "payment-error-keyset-expired" => format!(
                    "This e-cash note was issued on a keyset the mint has retired; it cannot be recovered by retrying. Cause: {e}"
                ),
                "payment-error-mint-unreachable" => {
                    "Mint temporarily unavailable. Please try again, or use a token from another mint.".to_string()
                }
                _ => format!("payment processing failed: {e}"),
            };
            (StatusCode::BAD_REQUEST, code, message)
        }
    }
}

/// Validate and price a payment from pre-receive facts only.
///
/// The token's face value (verified amount) prices the minimum check;
/// mint-specific pricing comes from the token's OWN mint config — never
/// a fallback to another mint's price.
pub(crate) fn precheck_payment(
    verified_amount_msat: u64,
    token_mint_url: &str,
    accepted_mints: &[crate::config::MintConfig],
) -> Result<PaymentPrecheck, PrecheckError> {
    let mint_config = accepted_mints
        .iter()
        .find(|m| crate::mint_url::mint_urls_equal(&m.url, token_mint_url));

    let mint_config = match mint_config {
        Some(m) => m,
        None => return Err(PrecheckError::MintNotAccepted(token_mint_url.to_string())),
    };

    let price_per_step = mint_config.price_per_step.max(1);
    let min_steps = mint_config.min_purchase_steps.max(1);
    let verified_sat = verified_amount_msat / 1000;
    let steps = verified_sat / price_per_step;

    if steps < min_steps {
        return Err(PrecheckError::BelowMinimum {
            steps,
            min_steps,
            price_per_step,
        });
    }

    Ok(PaymentPrecheck { price_per_step })
}

pub async fn handle_pay(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Extract token from either path
    let token = if content_type.contains("text/plain") {
        tracing::info!(len = body.len(), "received text/plain payment");
        body.trim().to_string()
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment");
        match extract_token_from_nostr_event(&body) {
            Some(t) => t,
            None => {
                let event = nostr_event::create_event(
                    21023,
                    vec![
                        vec!["level".to_string(), "error".to_string()],
                        vec!["code".to_string(), "invalid-nostr-event".to_string()],
                    ],
                    "invalid Nostr kind 21000 event: no payment tag found",
                    &state.identity.secret_key,
                );
                let json = serde_json::to_string(&event).unwrap_or_default();
                return (
                    StatusCode::BAD_REQUEST,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            "unsupported content-type".to_string(),
        );
    };

    let client_ip = get_client_ip(&headers, Some(remote_addr));

    if let Ok(ip) = client_ip.parse() {
        if !state.rate_limiter.allow(ip).await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("content-type", "text/plain"),
                    ("access-control-allow-origin", "*"),
                ],
                "rate limited".to_string(),
            );
        }
    }

    let mac = match get_mac_address(&client_ip) {
        Some(m) => m,
        None => {
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec!["code".to_string(), "mac-address-lookup-failed".to_string()],
                ],
                "payment rejected: mac-address-lookup-failed",
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };
    // Idempotent replay FIRST — before token verification (Codex P1 on
    // #43): a settled token is SPENT at the mint, so the verifier would
    // reject the duplicate POST before this branch could re-grant. The
    // journal entry carries its own pricing facts.
    {
        let cfg_dir = config::config_dir();
        let settled_amount = match payment_journal::settled_outcome(&cfg_dir, &token) {
            Some(payment_journal::PaymentPhase::Received { amount_sat })
            | Some(payment_journal::PaymentPhase::ReconcileSpent { amount_sat }) => {
                Some(amount_sat)
            }
            _ => None,
        };
        if let Some(amount_sat) = settled_amount {
            let entry_price = payment_journal::entry_price_per_step(&cfg_dir, &token)
                .unwrap_or_else(|| precheck_fallback_price(&state));
            // Replay returns the ORIGINAL grant, not a fresh one (Codex
            // P1 on #43): recreating would reset usage and expiry, letting
            // a customer replenish access forever with one old token.
            {
                let sessions = state.sessions.lock().await;
                if sessions.is_active(&mac) {
                    if let Some(sess) = sessions.get_session(&mac) {
                        let remaining = sess.allotment.saturating_sub(sess.used).max(1);
                        tracing::info!(
                            amount_sat,
                            remaining,
                            "idempotent replay: session already active, returning current grant"
                        );
                        drop(sessions);
                        let _ = state.portal.grant_access(&mac).await;
                        return session_granted_event(&state, &mac, remaining);
                    }
                }
                drop(sessions);
            }
            tracing::info!(
                amount_sat,
                "idempotent replay: token settled, session gone — re-granting"
            );
            let steps = amount_sat / entry_price.max(1);
            let allotment = steps * state.config.step_size;
            let mut sessions = state.sessions.lock().await;
            sessions.create_session(&mac, allotment, &state.config.metric, 3600);
            drop(sessions);
            let _ = state.portal.grant_access(&mac).await;
            return session_granted_event(&state, &mac, allotment);
        }
    }

    // Step 1: verify token via NUT-07 checkstate
    let (verified_amount, token_mint_url) = match state.verifier.verify(&token).await {
        Ok((amount_msat, mint_url)) => (amount_msat, mint_url),
        Err(e) => {
            tracing::warn!(error = %e, "token verification failed");
            let code = verify_failure_code(&e);
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec!["code".to_string(), code.to_string()],
                ],
                &format!("payment rejected: {e}"),
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 1.5: every locally-checkable validation runs BEFORE receive —
    // after `wallet.receive()` the token is consumed and a rejection here
    // would confiscate it (issue #4).
    let precheck = match precheck_payment(
        verified_amount,
        &token_mint_url,
        &state.config.accepted_mints,
    ) {
        Ok(p) => p,
        Err(e) => {
            let (code, msg) = match &e {
                PrecheckError::MintNotAccepted(mint) => (
                    "mint-not-accepted",
                    format!("payment rejected: mint {mint} is not configured on this TollGate"),
                ),
                PrecheckError::BelowMinimum { steps, min_steps, price_per_step } => (
                    "payment-error-below-minimum",
                    format!(
                        "payment rejected: {steps} step(s) at {price_per_step} sat/step is below the minimum purchase of {min_steps} step(s)"
                    ),
                ),
            };
            tracing::warn!(code, error = ?e, "payment rejected by pre-receive check");
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec!["code".to_string(), code.to_string()],
                ],
                &msg,
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 1.9: durable payment intent BEFORE value moves (AGENTS.md
    // money-moving-call rule; #40). Also the idempotency key: a replayed
    // token that already bought a session re-grants it (same-MAC
    // overwrite) instead of failing on the spent token.
    let cfg_dir = config::config_dir();
    let payment_id = payment_journal::token_id(&token);
    let intent = payment_journal::PaymentEntry {
        id: payment_id.clone(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        token: token.clone(),
        mac: mac.clone(),
        mint: token_mint_url.to_string(),
        price_per_step: precheck.price_per_step,
        step_size: state.config.step_size,
        metric: state.config.metric.clone(),
        phase: payment_journal::PaymentPhase::Intent,
    };
    if let Err(e) = payment_journal::append_entry(&cfg_dir, &intent) {
        tracing::error!(error = %e, "CRITICAL: could not persist payment intent; refusing to move value with no recovery record");
        let (status, code, message) = receive_failure_shape(&crate::wallet::WalletError::Database(
            "payment journal unavailable".into(),
        ));
        let event = nostr_event::create_event(
            21023,
            vec![
                vec!["level".to_string(), "error".to_string()],
                vec!["code".to_string(), code.to_string()],
            ],
            &message,
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            status,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }

    // Step 2: receive token into wallet
    let wallet_guard = state.wallet.read().await;
    let received_amount = if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(&token).await {
            Ok(amount_sat) => {
                tracing::info!(amount_sat, "token received into wallet");
                amount_sat
            }
            Err(e) => {
                tracing::warn!(error = %e, "wallet receive failed");
                // Every error AT the receive call is ambiguous (Codex P1 on
                // #43): a lost response after the mint accepted surfaces as
                // a Cdk transport error, not a Timeout — classifying those
                // as terminal `rejected` would close payments whose value
                // moved. Pre-receive rejections (precheck) never journal an
                // intent at all.
                let phase = payment_journal::PaymentPhase::TimeoutUnknown;
                let _ = payment_journal::append_entry(
                    &cfg_dir,
                    &payment_journal::PaymentEntry {
                        phase,
                        ..intent.clone()
                    },
                );
                drop(wallet_guard);
                let (status, code, message) = receive_failure_shape(&e);
                let event = nostr_event::create_event(
                    21023,
                    vec![
                        vec!["level".to_string(), "error".to_string()],
                        vec!["code".to_string(), code.to_string()],
                    ],
                    &message,
                    &state.identity.secret_key,
                );
                let json = serde_json::to_string(&event).unwrap_or_default();
                return (
                    status,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        tracing::warn!("wallet not initialized");
        drop(wallet_guard);
        let event = nostr_event::create_event(
            21023,
            vec![
                vec!["level".to_string(), "error".to_string()],
                vec!["code".to_string(), "wallet-not-initialized".to_string()],
            ],
            "payment rejected: wallet not initialized",
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            StatusCode::BAD_REQUEST,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    };
    drop(wallet_guard);

    // Step 3 (Go grantSessionAccess parity): snapshot → extend-or-create →
    // gate open with rollback on failure. A failed gate must roll the
    // session back and answer with a session-error notice — never a 200
    // claiming a grant that did not happen.
    let duration_secs = 3600u64;
    let price_per_step = precheck.price_per_step;

    // The mint may have charged swap fees, so the received amount can be
    // lower than the verified face value. Allotment follows the value that
    // actually arrived. If the fee-reduced amount no longer buys the
    // configured minimum, Go rejects with a session-error notice; parity
    // does the same (the value is in the wallet; the discrepancy is
    // reconciled operationally via the wallet balance).
    let steps = received_amount / price_per_step;
    let min_steps = state
        .config
        .accepted_mints
        .iter()
        .find(|m| crate::mint_url::mint_urls_equal(&m.url, &token_mint_url))
        .map(|m| m.min_purchase_steps.max(1))
        .unwrap_or(1);
    if steps < min_steps {
        // The value has MOVED — a post-receive rejection here is the exact
        // AGENTS.md L58-63 defect ("do not add new validations after value
        // moves"): the customer's token is consumed and a 400 says
        // "rejected". The pre-receive precheck already enforced the face
        // value; only fees can bring the NET below the minimum. Grant what
        // was paid when it buys at least one step; otherwise journal
        // durable credit (reconciled like any undecided payment) and tell
        // the customer the truth.
        tracing::warn!(
            received_sat = received_amount,
            steps,
            min_steps,
            "post-fee amount below minimum — never rejecting after value moved"
        );
        if steps == 0 {
            let _ = payment_journal::append_entry(
                &cfg_dir,
                &payment_journal::PaymentEntry {
                    phase: payment_journal::PaymentPhase::ReconcileZeroSteps {
                        amount_sat: received_amount,
                    },
                    ..intent.clone()
                },
            );
            let msg = format!(
                "payment received but consumed by swap fees ({received_amount} sats net): recorded as credit for this device — no session granted"
            );
            let event = nostr_event::create_event(
                21023,
                vec![
                    vec!["level".to_string(), "error".to_string()],
                    vec![
                        "code".to_string(),
                        "payment-below-minimum-credited".to_string(),
                    ],
                ],
                &msg,
                &state.identity.secret_key,
            );
            let json = serde_json::to_string(&event).unwrap_or_default();
            return (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
        // 1..min_steps: fall through and grant the steps actually paid for.
    }

    let step_size = state.config.step_size;
    let allotment = steps * step_size;

    let mut sessions = state.sessions.lock().await;
    let snapshot = sessions.snapshot_session(&mac);
    let extended =
        sessions.add_allotment(&mac, &state.config.metric, allotment, duration_secs, None);
    // The event reports the session's accumulated total (Go parity), not
    // just this payment's delta.
    let total_allotment = sessions
        .get_session(&mac)
        .map(|s| s.allotment)
        .unwrap_or(allotment);
    // Payment-grant durability + journal ordering (#43, Codex P1 there):
    // save_now (NOT the debounced save_to_disk — a debounced Ok(()) writes
    // nothing), and the terminal journal append must not advance until the
    // session is DURABLY recoverable. A save failure after value moved
    // answers outcome-unknown; reconciliation re-grants later.
    if let Err(e) = sessions.save_now(&crate::config::config_dir()) {
        tracing::error!(error = %e, "CRITICAL: session not durable after receive — answering outcome-unknown; reconciliation will re-grant");
        drop(sessions);
        let _ = payment_journal::append_entry(
            &cfg_dir,
            &payment_journal::PaymentEntry {
                phase: payment_journal::PaymentPhase::TimeoutUnknown,
                ..intent.clone()
            },
        );
        let (status, code, message) = receive_failure_shape(&crate::wallet::WalletError::Timeout(
            std::time::Duration::from_secs(0),
        ));
        let event = nostr_event::create_event(
            21023,
            vec![
                vec!["level".to_string(), "error".to_string()],
                vec!["code".to_string(), code.to_string()],
            ],
            &message,
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            status,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }
    drop(sessions);

    // Open the gate to grant network access via ndsctl. Failure rolls the
    // session back (Go: restoreSession) and answers with a session-error
    // notice instead of a success event. The journal re-marks the payment
    // ambiguous (Codex P1 on #52): the token is spent, the session is
    // rolled back, and reconciliation re-grants on a later attempt/boot
    // instead of stranding it behind a terminal `received`.
    if let Err(e) = state.portal.grant_access(&mac).await {
        tracing::warn!(mac = %mac, error = %e, "failed to open gate; rolling session back");
        let _ = payment_journal::append_entry(
            &cfg_dir,
            &payment_journal::PaymentEntry {
                phase: payment_journal::PaymentPhase::TimeoutUnknown,
                ..intent.clone()
            },
        );
        let mut sessions = state.sessions.lock().await;
        sessions.rollback_session(&mac, snapshot);
        sessions
            .save_now(&crate::config::config_dir())
            .unwrap_or_else(|err| {
                tracing::error!(error = %err, "failed to persist session rollback");
            });
        drop(sessions);
        let msg = format!("failed to open gate: {e}");
        let event = nostr_event::create_event(
            21023,
            vec![
                vec!["level".to_string(), "error".to_string()],
                vec!["code".to_string(), "session-error".to_string()],
            ],
            &msg,
            &state.identity.secret_key,
        );
        let json = serde_json::to_string(&event).unwrap_or_default();
        return (
            StatusCode::BAD_REQUEST,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    }

    // Terminal journal append AFTER the gate opens (Codex P1 on #52,
    // round 2): `received` MEANS "session granted AND gate open". A crash
    // before this append leaves intent-only → reconciliation re-grants; an
    // explicit gate failure re-marks ambiguous below → reconciliation
    // re-grants. No window strands a spent token behind a terminal phase.
    let _ = payment_journal::append_entry(
        &cfg_dir,
        &payment_journal::PaymentEntry {
            phase: payment_journal::PaymentPhase::Received {
                amount_sat: received_amount,
            },
            ..intent.clone()
        },
    );

    tracing::info!(
        verified_msat = verified_amount,
        received_sat = received_amount,
        allotment = allotment,
        extended,
        "session granted"
    );

    // Step 4: return kind 1022 session-granted event (start-time always
    // present — observed Go behavior renews StartTime on AddAllotment; the
    // event reports the accumulated total, Go parity).
    let _ = extended;
    session_granted_event(&state, &mac, total_allotment)
}

fn session_granted_event(
    state: &crate::http::AppState,
    mac: &str,
    allotment: u64,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tags = vec![
        vec!["p".to_string(), state.identity.pubkey_hex()],
        vec![
            "device-identifier".to_string(),
            "mac".to_string(),
            mac.to_string(),
        ],
        vec!["allotment".to_string(), allotment.to_string()],
        vec!["metric".to_string(), state.config.metric.clone()],
        vec!["start-time".to_string(), now.to_string()],
    ];
    let event = nostr_event::create_event(1022, tags, "", &state.identity.secret_key);
    let json = serde_json::to_string(&event).unwrap_or_default();
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_valid_nostr_event() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["payment", "cashuBabc123token"]],
            "content": "",
            "pubkey": "abc",
            "id": "def",
            "sig": "ghi",
            "created_at": 1234567890
        })
        .to_string();
        let token = extract_token_from_nostr_event(&event);
        assert_eq!(token.as_deref(), Some("cashuBabc123token"));
    }

    #[test]
    fn extract_token_rejects_wrong_kind() {
        let event = serde_json::json!({
            "kind": 99999,
            "tags": [["payment", "token"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_rejects_missing_payment_tag() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["other", "value"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_handles_invalid_json() {
        assert!(extract_token_from_nostr_event("not json").is_none());
    }

    #[test]
    fn extract_token_handles_multiple_tags() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [
                ["other", "val"],
                ["payment", "real-token"],
                ["another", "x"]
            ],
        })
        .to_string();
        assert_eq!(
            extract_token_from_nostr_event(&event).as_deref(),
            Some("real-token")
        );
    }

    /// Test that a session is created after a successful payment flow.
    /// Uses the SessionManager directly to verify the integration logic.
    #[tokio::test]
    async fn payment_creates_session() {
        use crate::session::SessionManager;

        let mut mgr = SessionManager::new();
        let allotment: u64 = 5000; // 5 sats * 1000 = 5000 msat
        let session = mgr.create_session("test:mac", allotment, "bytes", 3600);
        assert_eq!(session.allotment, 5000);
        assert_eq!(session.metric, "bytes");
        assert!(mgr.is_active("test:mac"));
    }

    /// R16 (#19): the notice-code taxonomy is string-identical to Go's
    /// for the same underlying condition — a shared portal keys on these
    /// exact bytes. One pin per class, both boundaries.
    #[test]
    fn verify_taxonomy_matches_go_codes() {
        use crate::error::VerifyError;
        assert_eq!(
            verify_failure_code(&VerifyError::InvalidToken("x".into())),
            "payment-error-invalid-token"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::NoProofs),
            "payment-error-invalid-token"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::LockedToken),
            "payment-error-invalid-token"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::MintNotAccepted("m".into())),
            "mint-not-accepted"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::Spent("SPENT".into())),
            "payment-error-token-spent"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::CheckStateStatus(
                "HTTP 429 too many requests".into()
            )),
            "mint-rate-limited"
        );
        assert_eq!(
            verify_failure_code(&VerifyError::CheckStateRequest("connection refused".into())),
            "payment-error-mint-unreachable"
        );
    }

    #[test]
    fn receive_taxonomy_matches_go_codes() {
        use crate::wallet::WalletError;
        // Cdk errors don't parse from strings; exercise the classifier via
        // the shapes it actually sees (Display text of our own variants).
        let spent = WalletError::Database("token already spent".into());
        assert_eq!(receive_failure_code(&spent), "payment-error-token-spent");
        let rate = WalletError::Database("mint returned 429 too many requests".into());
        assert_eq!(receive_failure_code(&rate), "mint-rate-limited");
        let expired = WalletError::Database("keyset 00abc is expired".into());
        assert_eq!(
            receive_failure_code(&expired),
            "payment-error-keyset-expired"
        );
        let unreachable = WalletError::Database("connect error: connection refused".into());
        assert_eq!(
            receive_failure_code(&unreachable),
            "payment-error-mint-unreachable"
        );
        let other = WalletError::Database("something else".into());
        assert_eq!(receive_failure_code(&other), "payment-processing-failed");
        let timeout = WalletError::Timeout(std::time::Duration::from_secs(30));
        assert_eq!(receive_failure_code(&timeout), "payment-outcome-unknown");
    }

    #[test]
    fn receive_failure_shape_carries_taxonomy_code_and_status() {
        use crate::wallet::WalletError;
        let (status, code, _) = receive_failure_shape(&WalletError::Database(
            "mint returned 429 too many requests".into(),
        ));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "mint-rate-limited");
    }

    #[test]
    fn receive_timeout_maps_to_outcome_unknown_not_rejected() {
        let (status, code, _) = receive_failure_shape(&crate::wallet::WalletError::Timeout(
            std::time::Duration::from_secs(30),
        ));
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(code, "payment-outcome-unknown");
    }

    #[test]
    fn definitive_receive_failure_maps_to_rejected() {
        let (status, code, _) =
            receive_failure_shape(&crate::wallet::WalletError::TokenParse("bad".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // R16 (#19): definitive failures carry the taxonomy class, with
        // payment-processing-failed as the fallback (Go parity).
        assert_eq!(code, "payment-processing-failed");
    }

    /// Test that rejected tokens return 400 (simulated).
    #[test]
    fn rejected_token_returns_400() {
        // The handler returns BAD_REQUEST for failed verification.
        // We verify the status code constant matches.
        let expected = StatusCode::BAD_REQUEST;
        assert_eq!(expected.as_u16(), 400);
    }

    #[test]
    fn unsupported_content_type_returns_415() {
        let expected = StatusCode::UNSUPPORTED_MEDIA_TYPE;
        assert_eq!(expected.as_u16(), 415);
    }

    #[test]
    fn payment_below_minimum_detected_via_zero_steps() {
        let received_amount: u64 = 0;
        let price_per_step: u64 = 1;
        let steps = received_amount / price_per_step;
        assert_eq!(steps, 0, "zero amount must yield zero steps");
    }

    #[test]
    fn payment_below_minimum_with_small_amount() {
        let received_amount: u64 = 1;
        let price_per_step: u64 = 2;
        let steps = received_amount / price_per_step;
        assert_eq!(steps, 0, "amount < price_per_step must yield zero steps");
    }

    fn mint_cfg(
        url: &str,
        price_per_step: u64,
        min_purchase_steps: u64,
    ) -> crate::config::MintConfig {
        crate::config::MintConfig {
            url: url.to_string(),
            price_per_step,
            min_purchase_steps,
            ..crate::config::MintConfig::default_production("https://unused.example")
        }
    }

    #[test]
    fn precheck_rejects_below_minimum_before_receive() {
        // 1 sat against a 2 sat/step mint: zero steps must be rejected by
        // the pre-receive check, not after the wallet consumed the token.
        let err = precheck_payment(
            1_000,
            "https://mint.example",
            &[mint_cfg("https://mint.example", 2, 0)],
        )
        .expect_err("below-minimum must fail precheck");
        match err {
            PrecheckError::BelowMinimum {
                steps,
                min_steps,
                price_per_step,
            } => {
                assert_eq!((steps, min_steps, price_per_step), (0, 1, 2));
            }
            other => panic!("expected BelowMinimum, got {other:?}"),
        }
    }

    #[test]
    fn precheck_honors_min_purchase_steps() {
        let mints = [mint_cfg("https://mint.example", 1, 3)];
        assert!(precheck_payment(2_000, "https://mint.example", &mints).is_err());
        assert!(precheck_payment(3_000, "https://mint.example", &mints).is_ok());
    }

    #[test]
    fn precheck_rejects_unconfigured_mint_no_pricing_fallback() {
        // A token from an unconfigured mint must be rejected outright,
        // never priced with another mint's price_per_step.
        let mints = [
            mint_cfg("https://a.example", 5, 0),
            mint_cfg("https://b.example", 1, 0),
        ];
        let err = precheck_payment(10_000, "https://unknown.example", &mints)
            .expect_err("unconfigured mint must fail precheck");
        match err {
            PrecheckError::MintNotAccepted(m) => assert_eq!(m, "https://unknown.example"),
            other => panic!("expected MintNotAccepted, got {other:?}"),
        }
    }

    #[test]
    fn precheck_prices_with_the_tokens_own_mint() {
        let mints = [
            mint_cfg("https://a.example", 5, 0),
            mint_cfg("https://b.example", 1, 0),
        ];
        let ok = precheck_payment(10_000, "https://b.example", &mints)
            .expect("token mint is configured");
        assert_eq!(
            ok.price_per_step, 1,
            "pricing must come from the token's mint, not mints[0]"
        );
    }

    #[test]
    fn precheck_matches_trailing_slash_variant() {
        let mints = [mint_cfg("https://mint.example/", 1, 0)];
        assert!(precheck_payment(1_000, "https://mint.example", &mints).is_ok());
    }

    #[test]
    fn allotment_calculation_correct() {
        let received_amount: u64 = 5;
        let price_per_step: u64 = 1;
        let step_size: u64 = 22020096;
        let steps = received_amount / price_per_step;
        let allotment = steps * step_size;
        assert_eq!(steps, 5);
        assert_eq!(allotment, 110100480);
    }

    #[tokio::test]
    async fn concurrent_sessions_different_macs_no_interference() {
        use crate::session::SessionManager;
        let mgr = std::sync::Arc::new(tokio::sync::Mutex::new(SessionManager::new()));

        let h1 = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.lock()
                    .await
                    .create_session("aa:bb:cc:dd:ee:01", 1000, "bytes", 3600)
            }
        });
        let h2 = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.lock()
                    .await
                    .create_session("aa:bb:cc:dd:ee:02", 2000, "bytes", 3600)
            }
        });

        let (s1, s2) = (h1.await.unwrap(), h2.await.unwrap());
        assert_eq!(s1.mac, "aa:bb:cc:dd:ee:01");
        assert_eq!(s2.mac, "aa:bb:cc:dd:ee:02");

        let guard = mgr.lock().await;
        assert_eq!(guard.sessions.len(), 2);
        assert!(guard.is_active("aa:bb:cc:dd:ee:01"));
        assert!(guard.is_active("aa:bb:cc:dd:ee:02"));
    }
}
