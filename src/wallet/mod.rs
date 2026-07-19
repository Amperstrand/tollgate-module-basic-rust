//! Token verifier — Cashu token parsing + NUT-07 checkstate.
//!
//! Ported from tollgate-rs/crates/tollgate-net/src/wallet.rs.
//! Read-only path: parse token, verify proofs unspent at mint, return amount.

pub mod verify;
