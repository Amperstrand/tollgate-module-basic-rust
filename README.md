# tollgate-module-basic-rust

> **Status: WIP — Phase 4 (session management + metering + payment wiring).**
> Not production-ready yet. The Go original at
> [`tollgate-module-basic-go`](https://github.com/OpenTollGate/tollgate-module-basic-go)
> remains the production binary until this repo reaches feature parity.

Rust clone of `tollgate-module-basic-go` — a drop-in replacement that uses
[CDK](https://github.com/cashubtc/cdk) (Cashu Dev Kit) instead of
[gonuts](https://github.com/Origami74/gonuts-tollgate) for the Cashu wallet.

## Why

`gonuts-tollgate` v0.7.1–v0.7.3 had an unrecoverable swap-counter race: a
transient mint `/swap` failure left the wallet's internal counter advanced
past the highest stored proof, producing error `10002 "blinded message
already signed"` on every subsequent operation. The wallet was permanently
bricked — only a manual DB edit could recover it.

CDK is the maintained Rust implementation of Cashu. This migration is the
strategic fix, not a cosmetic rewrite.

## Goal

A single Rust binary that is a **drop-in replacement** for
`tollgate-module-basic-go`:

- Same CLI (`--json version|status|wallet info|wallet balance`).
- Same HTTP+Unix-socket surface.
- Same config files at `/etc/tollgate/config.json`.
- Same Nostr event shapes (kinds 10021, 1022, 21000, 21023).
- Same on-disk persistence model (SQLite via `cdk-sqlite`).
- Cross-compiled for OpenWrt musl targets: `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`.

## Tech Stack

- Rust edition 2021, MSRV 1.85 (OpenWrt musl cross-build toolchains lag).
- `tokio 1` + `axum 0.8` HTTP server.
- `cdk 0.17` (umbrella, wallet feature) + `cdk-sqlite 0.17` for state.
- `rustls` (no OpenSSL — musl-incompatible).
- `serde` / `serde_json` for config/quotes persistence.
- `secp256k1` for Nostr identity signing.

## Build

```bash
cargo build --release
cargo build --release --target x86_64-unknown-linux-musl
```

See [`docs/binary-size-baseline.md`](docs/binary-size-baseline.md) for the
size comparison against the Go binary.

## License

MIT — see [`LICENSE-MIT`](LICENSE-MIT).
