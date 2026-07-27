# Changelog

## [v0.0.2-alpha] - 2026-07-27

MIPS compilation blocker resolved. All 5 OpenWrt cross-compile targets
now build successfully in CI (mips, mipsel, aarch64, armv7, x86_64).

### Fixed

- **MIPS AtomicU64 blocker**: cdk-common 0.17.3 from crates.io used
  `AtomicU64` in test modules, unavailable on 32-bit MIPS. Created
  [Amperstrand/cdk-common](https://github.com/Amperstrand/cdk-common) —
  exact clone of 0.17.3 with only `AtomicU64 → AtomicUsize`. Applied via
  `[patch.crates-io]` in Cargo.toml. Remove when cashubtc/cdk publishes
  0.17.4+ to crates.io.
- **Trailing-slash mint URL rejection**: `TokenVerifier::new()` now
  normalizes accepted mint URLs by trimming trailing slashes. Previously,
  a config entry like `https://mint.example.com/` would reject valid
  tokens from that mint.
- **Flaky `test_config_set_writes_to_disk`**: process-global
  `TOLLGATE_TEST_CONFIG_DIR` env var raced under parallel test execution.
  All 9 env-var-touching tests annotated with `#[serial]` via `serial_test`
  crate.

### Added

- 16 edge case tests (210 total, up from 194):
  - Token verification: empty/whitespace/garbage tokens, wrong mint error
    detail, trailing-slash URL matching, multiple accepted mints
  - Session lifecycle: `used == allotment` boundary, zero allotment,
    concurrent same-MAC race, re-payment overwrite, cleanup preserves active
  - Payment flow: 415 unsupported content type, zero-steps below minimum,
    allotment calculation, concurrent different-MAC sessions
- PRTA integration suite: 18/21 pass locally (3 skipped due to testnut
  keyset drift — environmental, not code)

### Changed

- Removed broken `.ipk` packaging step from `cross-compile.yml` (MIPS
  requires nightly + build-std, not the stable `cross` toolchain)
- Updated TESTING.md with cdk-common fork documentation

### Binary Sizes (stripped, musl static)

| Target | Size |
|--------|------|
| armv7-unknown-linux-musleabihf | 7.9 MB |
| aarch64-unknown-linux-musl | 8.5 MB |
| x86_64-unknown-linux-musl | 9.6 MB |
| mipsel-unknown-linux-musl | 10.7 MB |
| mips-unknown-linux-musl | 10.6 MB |

### Known Limitations

- NOT deployed on physical hardware (all testing in QEMU/GCP/local)
- CDK fork dependency (Amperstrand/cdk-common) until cashubtc/cdk
  publishes 0.17.4+ to crates.io
- 3 PRTA payment tests skip due to testnut keyset rotation

### Dependencies

- cdk-common 0.17.3 (via Amperstrand/cdk-common fork, was Amperstrand/cdk)
- serial_test 3 (dev-dependency for env-var-touching test serialization)

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
