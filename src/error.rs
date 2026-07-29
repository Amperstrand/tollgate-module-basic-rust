//! Unified error hierarchy for tollgate-module-basic-rust.
//!
//! Every module has its own strongly-typed error enum. The top-level
//! [`AppError`] umbrella collects them all via `#[from]` so that `?`
//! auto-converts at call boundaries. [`AppError`] also implements
//! [`axum::response::IntoResponse`] for direct use in HTTP handlers.
//!
//! # Design
//!
//! - Per-module enums keep context close to the source.
//! - `#[error(transparent)]` on umbrella variants passes the inner
//!   `Display` through unchanged, so log/output messages stay identical
//!   to the pre-refactor `String` messages.
//! - Existing enums (`WalletError`, `NftError`, `MeteringError`) are
//!   defined here so `error.rs` is the single import point.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::time::Duration;

// ---------------------------------------------------------------------------
// WalletError (moved from wallet/wallet.rs)
// ---------------------------------------------------------------------------

/// Errors returned by [`crate::wallet::TollWallet`] operations.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("CDK error: {0}")]
    Cdk(#[from] cdk::Error),
    #[error("database error: {0}")]
    Database(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("mint {0} not in accepted mints list")]
    MintNotAccepted(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet not initialized for mint {0}")]
    WalletNotFound(String),
    #[error("token parse error: {0}")]
    TokenParse(String),
}

// ---------------------------------------------------------------------------
// ConfigError — config loading/saving + identity management
// ---------------------------------------------------------------------------

/// Errors from configuration loading, saving, and validation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid merchant private key: {0}")]
    InvalidKey(String),
    #[error("{0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// WirelessError — WiFi scanner + connector (UCI/iw/iwinfo)
// ---------------------------------------------------------------------------

/// Errors from WiFi scanning and connection management.
#[derive(Debug, thiserror::Error)]
pub enum WirelessError {
    #[error("execute uci: {0}")]
    UciSpawn(String),
    #[error("uci: Entry not found")]
    UciEntryNotFound,
    #[error("uci {args} failed: {stderr}")]
    UciFailed { args: String, stderr: String },
    #[error("wifi reload: {0}")]
    WifiReloadSpawn(String),
    #[error("wifi reload failed")]
    WifiReloadFailed,
    #[error("iw dev link: {0}")]
    IwSpawn(String),
    #[error("no SSID found in iw link output")]
    NoSsid,
    #[error("failed to create STA interface")]
    StaCreateFailed,
    #[error("execute iwinfo: {0}")]
    ScanSpawn(String),
    #[error("iwinfo scan failed: {0}")]
    ScanFailed(String),
}

// ---------------------------------------------------------------------------
// SessionError — valve (gate control) + portal access errors
// ---------------------------------------------------------------------------

/// Errors from session gate-control (ndsctl) and captive-portal operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("ndsctl {action} failed to start: {reason}")]
    GateSpawn { action: String, reason: String },
    #[error("ndsctl {action} {mac} failed after {attempts} attempts")]
    GateExhausted {
        action: String,
        mac: String,
        attempts: u32,
    },
}

// ---------------------------------------------------------------------------
// VerifyError — token verification (NUT-07 checkstate)
// ---------------------------------------------------------------------------

/// Errors from Cashu token parsing and proof verification.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("invalid Cashu token: {0}")]
    InvalidToken(String),
    #[error("token has no mint URL: {0}")]
    NoMintUrl(String),
    #[error("mint {0} not accepted")]
    MintNotAccepted(String),
    #[error("could not sum token value: {0}")]
    ValueSum(String),
    #[error("token contains no proofs")]
    NoProofs,
    #[error("mint check-state request failed: {0}")]
    CheckStateRequest(String),
    #[error("mint returned error: {0}")]
    CheckStateStatus(String),
    #[error("mint response not JSON: {0}")]
    CheckStateParse(String),
    #[error("mint response missing 'states'")]
    MissingStates,
    #[error("one or more proofs already spent (state: {0})")]
    Spent(String),
    #[error("token has spending conditions (P2PK/HTLC) and cannot be spent by the gateway")]
    LockedToken,
}

// ---------------------------------------------------------------------------
// CliError — CLI command errors
// ---------------------------------------------------------------------------

/// Errors from CLI command processing.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("failed to read {path}: {reason}")]
    TokenFileRead { path: String, reason: String },
    #[error("no wallet configured")]
    NoWallet,
}

// ---------------------------------------------------------------------------
// PayoutError — Lightning payout (LNURL/melt) errors
// ---------------------------------------------------------------------------

/// Errors from Lightning payouts via LNURL and wallet melt.
#[derive(Debug, thiserror::Error)]
pub enum PayoutError {
    #[error("wallet not available")]
    NoWallet,
    #[error("invalid lightning address")]
    InvalidAddress,
    #[error("HTTP client build failed: {0}")]
    HttpClientBuild(String),
    #[error("LNURL fetch failed: {0}")]
    LnurlFetch(String),
    #[error("LNURL parse failed: {0}")]
    LnurlParse(String),
    #[error("no callback in LNURL response")]
    NoCallback,
    #[error("invoice fetch failed: {0}")]
    InvoiceFetch(String),
    #[error("invoice parse failed: {0}")]
    InvoiceParse(String),
    #[error("no BOLT11 invoice in response")]
    NoInvoice,
    #[error("melt failed: {0}")]
    Melt(String),
}

// ---------------------------------------------------------------------------
// DetectorError — upstream gateway detection errors
// ---------------------------------------------------------------------------

/// Errors from upstream TollGate gateway probing and route reading.
#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error("read /proc/net/route: {0}")]
    RouteRead(String),
    #[error("probe {url}: {reason}")]
    ProbeRequest { url: String, reason: String },
    #[error("probe {url}: HTTP {status}")]
    ProbeStatus { url: String, status: String },
    #[error("parse discovery event: {0}")]
    ProbeParse(String),
    #[error("not a TollGate (kind={kind}, expected 10021)")]
    NotTollGate { kind: u64 },
    #[error("no mint URL in discovery event")]
    NoMintUrl,
}

// ---------------------------------------------------------------------------
// AppError — top-level umbrella
// ---------------------------------------------------------------------------

/// Top-level error that unifies all module-specific errors.
///
/// Each variant uses `#[from]` so that `?` in a function returning
/// `Result<T, AppError>` auto-converts from any module error.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Wallet(#[from] WalletError),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error(transparent)]
    Wireless(#[from] WirelessError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Payout(#[from] PayoutError),
    #[error(transparent)]
    Detector(#[from] DetectorError),
    #[error(transparent)]
    Metering(#[from] crate::metering::MeteringError),
    #[cfg(feature = "embedded-portal")]
    #[error(transparent)]
    Nft(#[from] crate::portal::nft_manager::NftError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Internal(String),
}

// Re-export existing error types so `crate::error::X` works for all.
pub use crate::metering::MeteringError;
#[cfg(feature = "embedded-portal")]
pub use crate::portal::nft_manager::NftError;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            // Bad request — client sent invalid data.
            Self::Verify(VerifyError::InvalidToken(_))
            | Self::Verify(VerifyError::NoProofs)
            | Self::Verify(VerifyError::MintNotAccepted(_))
            | Self::Verify(VerifyError::Spent(_))
            | Self::Verify(VerifyError::LockedToken) => StatusCode::BAD_REQUEST,

            // Config validation errors are client-side fixable.
            Self::Config(ConfigError::Validation(_)) | Self::Config(ConfigError::InvalidKey(_)) => {
                StatusCode::BAD_REQUEST
            }

            // Wallet not initialised — service is not ready yet.
            Self::Wallet(WalletError::WalletNotFound(_))
            | Self::Wallet(WalletError::MintNotAccepted(_)) => StatusCode::SERVICE_UNAVAILABLE,

            // Session/gate-control failures — upstream NDS unavailable.
            Self::Session(_) => StatusCode::SERVICE_UNAVAILABLE,

            // Not-found class.
            Self::Metering(MeteringError::NotFound(_)) => StatusCode::NOT_FOUND,
            #[cfg(feature = "embedded-portal")]
            Self::Nft(NftError::CounterNotFound(_)) => StatusCode::NOT_FOUND,

            // Everything else is an internal server error.
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (status, self.to_string()).into_response()
    }
}
