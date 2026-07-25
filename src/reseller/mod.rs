//! Reseller mode — upstream session manager.
//!
//! When a router discovers an upstream TollGate (via the upstream detector),
//! this module connects to it, sends Cashu payments, and manages the session
//! lifecycle (renewal, usage tracking, disconnect).

pub mod upstream_session;

pub use upstream_session::{UpstreamPaymentResult, UpstreamSession};
