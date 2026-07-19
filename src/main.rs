//! tollgate-module-basic-rust — Phase 0 scaffolding.
//!
//! Forces full CDK dependency tree into the release build for binary-size
//! measurement. Exercises: Amount arithmetic, Proof serde, Wallet type.

use cdk::amount::Amount;
use cdk::nuts::Proof;
use cdk::wallet::Wallet;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUSTC_VERSION: &str = match option_env!("RUSTC_VERSION") {
    Some(v) => v,
    None => "unknown",
};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = VERSION,
        rustc = RUSTC_VERSION,
        target = std::env::consts::ARCH,
        "tollgate-module-basic-rust phase-0 scaffolding (smoke test)"
    );

    // Force CDK amount arithmetic code into the binary.
    let amt = Amount::from(1000_u64);
    let doubled = amt + Amount::from(500_u64);
    tracing::info!("CDK smoke: {doubled}");

    // Force CDK serde/Proof code path. This drags in all the NUT types,
    // serde derives, and the cdk_common protocol layer.
    let proof_json = r#"{"amount":1,"id":"00","secret":"0000000000000000000000000000000000000000000000000000000000000000","C":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
    match serde_json::from_str::<Proof>(proof_json) {
        Ok(proof) => {
            let reser = serde_json::to_string(&proof).unwrap_or_default();
            tracing::info!("CDK smoke: proof reser length = {}", reser.len());
        }
        Err(e) => {
            // Parse failure is fine — the serde code is compiled regardless.
            tracing::warn!("proof parse failed (ok for smoke test): {e}");
        }
    }

    // Keep types alive so LTO doesn't strip the dependency.
    let _ = std::any::TypeId::of::<Wallet>();
    let _ = std::any::TypeId::of::<axum::body::Body>();

    println!("tollgate-module-basic-rust v{VERSION} — phase 0 smoke build");
    println!("rustc: {RUSTC_VERSION}");
    println!("target: {}", std::env::consts::ARCH);
}
