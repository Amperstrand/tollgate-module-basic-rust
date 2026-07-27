# Changelog

## [v0.0.1-alpha] - 2026-07-26

First alpha release of tollgate-module-basic-rust — a Rust rewrite of
tollgate-module-basic-go using CDK (Cashu Dev Kit) instead of gonuts.

### Added

- Full HTTP API parity with Go binary (6 endpoints)
- CDK wallet integration with SQLite persistence
- Automatic wallet migration from gonuts bbolt → CDK SQLite
- Config validation with `validate()`, `ensure_defaults()`, migration
- Upstream TollGate detector (gateway discovery + HTTP probing)
- Wireless scanner (iwinfo parsing) + connector (UCI commands)
- UpstreamManager orchestration with blacklist and signal monitoring
- Reseller mode (UpstreamSession with Cashu payment flow)
- OpenWrt packaging (Makefile, procd init, uci-defaults)
- 194 unit tests, 0 failures
- Cross-compilation for all 5 OpenWrt targets (including MIPS)
- QEMU boot verification (wallet init, HTTP server, graceful shutdown)
- CLI commands: version, status, wallet info/balance, config get/set, migrate, health

### Known Limitations

- NOT deployed on physical hardware (all testing in QEMU/GCP)
- gonuts-export binary required for auto-migration
- CDK fork dependency (Amperstrand/cdk) until upstream merges AtomicU64 fix
- Wireless orchestration event loop not wired into main.rs
- No integration tests against live mints

### Dependencies

- cdk 0.17.3 (via Amperstrand/cdk fork)
- axum 0.8, tokio 1, secp256k1 0.29, reqwest 0.12
