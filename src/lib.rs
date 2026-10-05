// clippy 1.99's double_must_use fires on every #[async_trait] trait
// (the macro pins boxed must_use futures; the lint is a toolchain-roll
// artifact, not a code defect). Remove if the lint learns async_trait.
#![allow(clippy::double_must_use)]

//! Library root for tollgate-module-basic-rust.

pub mod cli;
pub mod config;
pub mod degraded;
pub mod error;
pub mod http;
pub mod identity;
pub mod lightning_quotes;
pub mod mac_resolver;
pub mod metering;
pub mod migration;
pub mod mint_health;
pub mod mint_url;
pub mod monitor;
pub mod nostr_event;
pub mod payment_journal;
pub mod payout;
pub mod payout_journal;
pub mod portal;
pub mod rate_limiter;
pub mod reseller;
pub mod session;
pub mod tracing_setup;
pub mod upstream_detector;
pub mod valve;
pub mod wallet;
pub mod wireless;
