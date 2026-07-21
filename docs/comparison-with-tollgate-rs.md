# Comparison: tollgate-module-basic-rust vs. tollgate-rs

## TL;DR

- **tollgate-module-basic-rust** is a production-near drop-in Rust rewrite of the Go captive-portal binary. It exists to fix a wallet-bricking race condition in the Go Cashu library. 58 unit tests pass; Phase 7 (hardware validation) is in progress.
- **tollgate-rs** is a clean-sheet protocol redesign: resource-agnostic, `no_std` core library, CBOR wire protocol, Spilman payment channels, mesh-first. Core and protocol crates compile; `tollgate-net` has a working binary with Docker integration tests and a `v1-compat` feature, but the project is still in early implementation.
- They are **separate projects with different upstreams**, not forks. One replaces Go today; the other rethinks the protocol for tomorrow.

---

## Side-by-Side Comparison

### 1. Intent / Philosophy

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Goal | Drop-in Rust replacement for `tollgate-module-basic-go`. Fix the swap-counter race that bricks wallets. | Clean-sheet protocol implementation. Resource-agnostic payment layer for any metered resource delivery. |
| Motivation | The Go Cashu library (gonuts v0.7.1-v0.7.3) has a non-atomic swap operation that can permanently brick the wallet. CDK's saga pattern eliminates this. | The v1 design is limited to tree-topology, single-price, per-session token payment on OpenWrt. A new protocol is needed for mesh, per-peer pricing, streaming payment channels, and resource-agnostic operation. |
| Design constraint | Must be API-compatible with the Go binary. Same config files, same HTTP routes, same Nostr event shapes, same CLI commands. | No compatibility constraint. Protocol-first design; v1 compatibility is an optional feature flag (`v1-compat`), not the default path. |
| Relationship to Go | Direct replacement. The README at line 8 states: "Rust rewrite of tollgate-module-basic-go". | Prior work. The intro doc at line 262-271 explicitly lists differences: mesh vs tree, Spilman vs tokens, device-to-device vs human-to-device, network-agnostic vs OpenWrt-only, per-peer vs single price. |
| Non-goal | Resource-agnostic design. Mesh support. Payment channels. These would break Go compatibility. | Captive portal UI. Routing decisions. Wallet implementation details. Anonymity guarantees. The intro doc at lines 118-125 lists these explicitly. |

### 2. Scope

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Crate structure | Single crate with a lib and a bin. One `Cargo.toml`, one binary target (`src/main.rs`), one library (`src/lib.rs`). | Cargo workspace with three crates: `tollgate-protocol` (wire types, `no_std`), `tollgate-core` (state machine, `no_std`), `tollgate-net` (binary with I/O). |
| Repository | `felixfelix-bot/tollgate-module-basic-rust` (per `Cargo.toml` line 8) | `OpenTollGate/tollgate-rs` (per `Cargo.toml` line 14) |
| Rust edition | 2021 (per `Cargo.toml` line 4) | 2024 workspace-wide (per `Cargo.toml` line 10) |
| MSRV | 1.85 (per `Cargo.toml` line 5) | 1.85.0 workspace-wide (per `Cargo.toml` line 11) |
| Source modules (basic-rust) | config, http (routes: discovery, pay, whoami, usage, balance, ln_invoice), wallet (wallet, verify), session, metering, cli, identity, nostr_event, tracing_setup | N/A |
| Source modules (tollgate-rs, protocol) | N/A | codec, message, product |
| Source modules (tollgate-rs, core) | N/A | access, action, event, metering, peer, pricing, session, time |
| Source modules (tollgate-rs, net) | N/A | adapter, client, config, control_server, control, driver, server, spilman (mod, service, wallet), status, v1_compat (adapter, client, crowsnest, handlers, http_client, ln_quotes, mac_resolver, merchant, mod, nostr, pricing, recovery, session_manager, usage_tracker, wallet), openwrt (uci_ops, wifi_scanner, wifi_connector, network_monitor), bin |
| Approximate LOC | ~2,500 across `src/` | ~7,000+ across `crates/` (including ~3,000 LOC in the v1-compat adapter layer) |
| Scope of ambition | One binary, one use case: OpenWrt captive portal selling WiFi access via Nodogsplash. | A protocol library plus multiple deployment binaries (net for IP/FIPS, ESP32 for constrained devices, future resource types like electricity or fluid delivery). |

### 3. Architecture

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Pattern | Monolithic binary. All logic in a single process: HTTP server, CLI server, wallet, session manager, metering, config loading, Nostr signing. | Layered: `tollgate-protocol` (wire types, `no_std`) feeds into `tollgate-core` (sans-IO state machine, `no_std`), which is driven by `tollgate-net` (host binary with I/O, async). |
| I/O model | Direct async I/O via tokio. HTTP server (axum), wallet operations, ndsctl subprocess calls, Unix socket CLI all run as tokio tasks. `WalletHandle = Arc<Mutex<Wallet>>` serializes wallet access. | `tollgate-core` is explicitly a "pure, synchronous state machine" (per `crates/tollgate-core/src/lib.rs` lines 1-20) that performs no I/O and depends on no async runtime. The host drives it in a loop: translate real-world events into `Event`s, call `Session::handle`, execute returned `Action`s. This is the sans-IO pattern. `tollgate-net` uses tokio for the host loop. |
| Trait boundaries | None. Modules call each other directly. Wallet is a concrete struct (`TollWallet`), not a trait. The metering module calls `tokio::process::Command` directly to spawn ndsctl. | `Wallet` trait (10 async methods defined in the design docs: receive_token, create_token, fund_channel, verify_funding, sign_balance_update, verify_balance_update, settle_channel, mint_reachable, balance, compute_channel_secret). `ResourceAdapter` trait (subscribe_meter, peer_metrics, enforce_access). The core defines these; the host implements them. This is what makes the core platform-independent. |
| Reusability | Not reusable outside its specific use case. The wallet code couples to CDK directly; the metering code couples to ndsctl; the HTTP routes couple to the session manager. | `tollgate-core` and `tollgate-protocol` are `no_std` + `alloc` and compile for ESP32. Any deployment provides its own wallet and resource adapter. The README at line 29 states: "A constrained-device variant (tollgate-net-esp32) lives in a separate project and consumes the same tollgate-core." |
| Thread model | tokio multi-thread runtime. HTTP server task (axum accept loop), CLI server task (Unix socket accept loop), signal handler task, background session cleanup task. | Similar tokio host loop in `tollgate-net`, plus a TUI monitoring tool (`tolltop` using ratatui 0.29). Core is single-threaded (synchronous state machine, no concurrency concerns). |
| Error handling | `thiserror` derive macro for error types. `WalletError` enum with variants: Cdk, Database, Timeout, MintNotAccepted, Io, WalletNotFound, TokenParse (per `src/wallet/wallet.rs` lines 42-57). `MeteringError` enum with variants: NotFound, ExecutionFailed, ParseError (per `src/metering/mod.rs` lines 11-18). | `anyhow` for the binary, `thiserror 2.0` for library crates. The workspace lints enforce `unsafe_code = "deny"`. Core action/event types carry typed errors rather than strings. |

### 4. Wire / Protocol Format

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Encoding | JSON over HTTP. Nostr events are JSON arrays serialized per NIP-01. Config files are JSON. CLI responses are JSON. | CBOR (RFC 8949) for peer-to-peer protocol messages. YAML for configuration. JSON only when the `v1-compat` feature is active (for Go-compatible HTTP endpoints). |
| Why this encoding | Compatibility with the Go binary, which uses JSON for everything. NIP-01 defines Nostr events as JSON. | CBOR was chosen (per `tollgate-protocol.md` lines 15-28) because: self-describing format avoids custom parsers per transport, variable-length fields (mint URLs, product lists) are natural in CBOR, well-supported in Rust (minicbor), compact enough for ESP32, and more compact than JSON. JSON was rejected because of larger wire size. FIPS-style binary was rejected because variable-length strings are awkward. |
| Message structure | No formal message protocol. HTTP request/response with JSON bodies. Nostr events (kinds 10021, 1022, 21000, 21023) carry pricing, session, and payment data as NIP-01 tagged arrays: `[0, pubkey, created_at, kind, tags, content]`. | 15 CBOR message types (Announce, PriceSheet, Accept, ChannelReady, MeteringReport, BalanceUpdate, BalanceAck, BootstrapToken, BootstrapAck, RolloverInit, RolloverReady, ChannelClose, CloseAck, Reject, Disconnect). Each is a CBOR map with integer field keys (key 0 is always the message type). Size estimates: Announce ~40 bytes, PriceSheet ~120 bytes, MeteringReport ~50 bytes, BalanceUpdate ~110 bytes (per `tollgate-protocol.md` lines 515-531). |
| Discovery | Nostr kind 10021 event returned by `GET /`. Contains tags: metric (e.g. "bytes"), step_size (e.g. "22020096"), price (e.g. "1"), unit (e.g. "sats"), mint (e.g. "https://mint.example.com"), purchase_min_steps (e.g. "0"). | Announce message (type 0x00): protocol version (u8, current: 1), compressed secp256k1 pubkey (bytes(33)), unit string (text, e.g. "bytes"), capability bitfield (u32, bit 0x01 = SPILMAN). Then PriceSheet message (type 0x01): array of products, each with product_id (bytes(32)), extensions (bytes), pricing_scale (u32), array of mint options. |
| Payment encoding | Cashu token (NUT-00 format, base64-encoded) sent as `text/plain` body, or wrapped in a Nostr kind 21000 event sent as `application/json` body. The token contains proofs with Y-values that are extracted and verified via NUT-07 checkstate. | BootstrapToken message (type 0x07): raw Cashu token bytes in a CBOR envelope. Spilman balance updates: BalanceUpdate message (type 0x05) with channel_id (bytes(32)), cumulative_balance (u64), balance_signature (bytes(64), Schnorr), net_amount (u64). |
| Transport framing | Standard HTTP request/response. No custom framing. | HTTP polling: `POST /tollgate/v1/exchange` with `Content-Type: application/cbor`. Bodies contain zero or more CBOR messages, each prefixed with a 2-byte little-endian length. WebSocket: `GET /tollgate/v1/ws` with binary frames, one CBOR message per frame. |
| Versioning | Implicit: same version as the Go binary (config_version "v0.0.8"). | Explicit: Announce carries a protocol_version u8 field. Both peers must support the same version; mismatch triggers Reject with reason code 0x09. |

### 5. HTTP Surface

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Port | `127.0.0.1:2121` (hardcoded). All routes set `Access-Control-Allow-Origin: *`. | `0.0.0.0:4747` (default; configurable). Listens on all interfaces for peer connectivity. |
| Endpoints (native) | `GET /` (discovery, kind 10021), `POST /` (payment, kind 1022/21023), `GET /whoami` (resolves MAC from /tmp/dhcp.leases + /proc/net/arp, HTTP 500 on failure), `GET /usage` (used/total plain text), `GET /balance` (JSON: {status, session_active, usage, allotment, remaining}), `POST /ln-invoice` (stub), `GET /ln-invoice` (stub) | `POST /tollgate/v1/exchange` (CBOR polling transport), `GET /tollgate/v1/ws` (WebSocket transport). These are the two v2 transports. |
| Endpoints (v1-compat) | N/A (this IS the v1 API) | When `--features v1-compat` is enabled, `build_v1_router()` at `v1_compat/mod.rs:42` exposes the Go-compatible surface: `GET /`, `POST /`, `GET /whoami`, `GET /usage`, `GET /balance`. Implemented in `v1_compat/handlers.rs` (824 lines). |
| Authentication | None on HTTP layer. Identity comes from the Nostr keypair in `identities.json`. The `GET /whoami` endpoint is meant to return the caller's MAC address for identification. | None on HTTP layer. Peers are authenticated out-of-band (FIPS Noise IK, WireGuard, etc.) before TollGate messages flow. The Announce message carries the pubkey, and the server tracks per-pubkey state. |
| CORS | `Access-Control-Allow-Origin: *` on all routes (for captive-portal compatibility). | Not specified in the protocol docs. Likely not needed for peer-to-peer communication. |
| Client identification | Via `X-Forwarded-For` or `X-Real-IP` headers (for `/usage`). Via MAC address in the captive-portal flow. | Via the Announce pubkey. Sessions are keyed by `PeerId` (a wrapper around the 33-byte compressed pubkey). |

### 6. CLI Surface

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Interface | Unix domain socket at `/var/run/tollgate.sock` (mode `0660`). Line-delimited text protocol: one command per line, JSON response. Overridable with `TOLLGATE_TEST_CONFIG_DIR` for testing. | `clap`-based CLI with subcommands (`tollgated`). Also has a control socket (status queries) and a TUI monitoring tool (`tolltop` using ratatui). |
| Commands | `version` (multi-line text: version, commit, build_time, rust_version, openwrt target), `status` (JSON), `wallet info` (JSON array of mint URLs + balances), `wallet balance` (JSON total sats), `migrate <path>` (JSON migration report). Unknown commands return `{"success": false, "error": "unknown command: <cmd>"}`. | CLI subcommands for node operation (start, stop, status, configure, etc. -- exact subcommands not enumerated in the source read, but `clap` with derive is used). Control socket exposes a serializable status snapshot (`status.rs`, 5 unit tests). |
| Usage example | `echo "wallet balance" \| socat - UNIX-CONNECT:/var/run/tollgate.sock` | TUI: `tolltop` (ratatui-based real-time monitoring). |
| Discovery mechanism | Client sends a command string. Server reads one line, parses the command, dispatches to a handler, writes the response plus newline. | Clients discover the node via the protocol itself (Announce + PriceSheet), or via static peer configuration in `tollgate.yaml` under the `peers:` section with optional `endpoint:` fields for IP peering. |

### 7. Persistence Model

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Wallet storage | SQLite via `cdk-sqlite`. One `wallet_<mint>.sqlite` file per accepted mint (filename derived by sanitizing the mint URL). Tables: proofs, keysets, mint_quotes, melt_quotes, keyset_counter, wallet_sagas (per `architecture.md` lines 283-289). | No persistence in `tollgate-core` (it is `no_std`). `tollgate-net`'s wallet implementation uses CDK (with cdk-spilman for channel operations when the `spilman` feature is enabled). Persistence backend is an implementation choice of the concrete Wallet impl, not the core. |
| Session storage | In-memory `HashMap<String, CustomerSession>` keyed by client MAC address (per `session/mod.rs` lines 27-29). Sessions do not survive process restart (matches Go behavior, documented at `session/mod.rs` lines 3-4). | In-memory `BTreeMap<PeerId, PeerSession>` in the core state machine (per `session.rs`). Same ephemeral model. Reboot/state loss is handled by the protocol design: the online peer can share back channel state via a proposed ChannelSync message (future work, per `tollgate-payment-channels.md` lines 236-242), or the rebooted peer falls back to a fresh session with bounded exposure equal to channel capacity. |
| Config storage | JSON files at `/etc/tollgate/` (overridable via `TOLLGATE_TEST_CONFIG_DIR`): `config.json` (Go-compatible schema), `identities.json` (Nostr keypairs), `install.json` (package metadata). | YAML files. Search path (lowest to highest priority): `/etc/tollgate/tollgate.yaml` (system), `~/.config/tollgate/tollgate.yaml` (user, XDG), `./tollgate.yaml` (deployment-specific). All found files are merged in priority order. When `-c` is specified, only that file is loaded (per `tollgate-configuration.md` lines 15-35). |
| Runtime config changes | None. Config is loaded once at startup. Changing `config.json` requires a restart. | Pricing and peer overrides are hot-reloadable (per `tollgate-configuration.md` lines 336-346). The implementation watches the config file for changes and applies them without interrupting active sessions. Channel parameters, metering interval, identity, and accepted mints require restart. |
| Seed/key storage | 64-byte seed at `/etc/tollgate/wallet_seed.bin` (mode `0600`, auto-generated on first boot). Merchant keypair in `identities.json` (auto-generated secp256k1 keypair if none exists, saved with mode `0600`). | Secret key file at configurable path (default `/etc/tollgate/identity.key`). No seed file; the identity key is the root. The pubkey derived from this key is used in Announce messages, Spilman channel creation, and peer identification. |
| SQLite rationale | Inspectability (`sqlite3 wallet.sqlite` via SSH on any router), portability (`.dump` produces text backups), universality (every Linux distro ships sqlite3), and migration compatibility (SQL migrations are diff-able). Explicitly documented at `architecture.md` lines 295-303. | Not documented for tollgate-rs, as persistence is an implementation detail of the wallet, not a protocol concern. |

### 8. Configuration Format

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Format | JSON. Exact same schema as the Go binary. Field names, casing, and `omitempty` behavior must match for drop-in compatibility. The schema structs at `config/schema.rs` line 1 state: "1:1 mirror of Go config_manager package." | YAML. Follows FIPS configuration patterns (per `tollgate-configuration.md`). Every parameter has a sensible default. Comments are supported. |
| Pricing config | `price_per_step: u64` (sats per step, default 1), `step_size: u64` (bytes per step, default 22020096 = ~21 MiB), `price_unit: "sats"`. Single price for the entire node. The `metric` field (default "bytes") determines the unit of measurement. | `price_per_second: i64`, `price_per_unit: i64` (both signed, scaled integers divided by `pricing_scale`). Per-mint pricing (each MintPricing entry ties a price to a specific mint URL and unit). Per-peer overrides via `price_multiplier` in the `peers:` section. Dynamic pricing via `pricing.formula` expressions evaluated against opaque metrics from the ResourceAdapter. |
| Product model | No product abstraction. One global price. The allotment for a payment is simply `received_amount * step_size`. | Products are first-class: each has a `ProductId` (SHA256 of a canonical byte layout with domain-separation tag "tollgate/product-id/v1", per `tollgate-pricing.md` lines 83-99), a `pricing_scale` (u32 divisor, default 1000), per-mint prices, and opaque extension bytes. The product ID includes ALL pricing-relevant fields so any change produces a new ID detectable with one hash comparison. |
| Multi-mint | `accepted_mints` is a list of mint URLs, each with `min_balance`, `balance_tolerance_percent`, `payout_interval_seconds`, `min_payout_amount`, `price_per_step`, `min_purchase_steps`. Pricing is not per-mint (same price_per_step applies to all mints). | `mints` list with `url` and `mint_units` (e.g. `["sat", "msat"]`). Pricing is always per-mint (each MintPricing entry ties a price to a specific mint URL and unit). Operators are encouraged to maintain overlapping channels across at least two mints for resilience (per `tollgate-payment-channels.md` line 247). |
| Profit sharing | `profit_share` list of `{factor, identity}` pairs that must sum to 1.0 (+/-1e-6 tolerance). If they don't sum to 1.0, a warning is emitted and the residual remains in the wallet each payout cycle (per `schema.rs` lines 94-107). | Not a protocol concern. Operator profit management is outside the scope of tollgate-rs (the operator earns the margin between what they charge and what they pay their peers). |
| Upstream config | Extensive upstream configuration: `upstream_detector` (probe timeout/retry, interface filtering, discovery timeout), `upstream_session_manager` (max prices, trust policy, session increments, data monitoring interval), `upstream_wifi` (scan intervals, signal thresholds, blacklist TTL, cooldowns). These are carried over from the Go schema for compatibility. | Not present in the same form. Upstream relationships are handled by the protocol itself: each peer is both a provider and a consumer, and the Spilman channel pair handles the economic relationship. No separate "upstream detector" or "upstream session manager" is needed. |

### 9. Payment Model

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Mechanism | Single Cashu token per payment. Client sends a token (text/plain or wrapped in Nostr kind 21000), provider receives it via CDK `Wallet::receive()`, session is created with an allotment computed as `amount * step_size`. No payment channels, no mid-session top-up. | Two modes: (1) Bootstrap tokens (regular Cashu tokens for initial connection or bootstrap-only clients, who cannot run Spilman channels), (2) Spilman unidirectional payment channels for streaming micropayments. Two channels per peer pair (one per direction) enable bidirectional payment with netting. |
| Settlement frequency | Once per session (token received at start, allotment granted, metering drains it). No mid-session top-up. When the allotment is exhausted, the session ends and the client must pay again. | Every metering interval (default 5 seconds, configurable range 3-10 seconds). The net debtor signs a single balance update for only the net amount owed. One signature per interval. |
| Offline resilience | None. Token verification requires live mint connectivity (CDK receive contacts the mint for a swap operation). If the mint is down, payment fails with HTTP 400 and the error content in a kind 21023 Nostr event. | Balance updates are signed between peers without mint involvement. Payment continues during mint outages. Channel funding, rollover, and settlement require mint connectivity, but the protocol handles queuing and retry. If a channel exhausts during a mint outage and rollover is blocked, delivery pauses; after a configurable timeout (default 60s), the session is closed (per `tollgate-payment-channels.md` lines 194-195). |
| Channel lifecycle | N/A (no channels). | Full lifecycle: Funding (2-of-2 multisig with NUT-11 conditions, ECDH-derived channel secret) -> Active (metering and balance updates) -> RollingOver (new channel alongside exhausting one, at 80% threshold) -> Settling (receiver submits to mint) -> Closed. Channel TTL (default 1 hour, configurable). Capacity starts small (default 10 sats) and grows by a configurable factor (default 2.0x) after each successful rollover. |
| Netting | N/A (unidirectional payment only: client pays provider). | At each interval, both sides compute what they owe each other independently. Only the net delta moves on one channel. This dramatically extends channel life when both sides deliver resources to each other. Example from design docs: if A charges 10 sat/MB and B charges 3 sat/MB, and A delivers 1 MB while B delivers 1 MB, only the net 7 sats moves on A's channel (per `tollgate-intro.md` lines 97-109). |
| Bootstrap | N/A (every payment is a "bootstrap" in the v1 model). | Bootstrap tokens solve the chicken-and-egg problem: a peer needs to pay to get online, but Spilman channels need mint connectivity. A bootstrap token grants enough metered access to reach a mint, then the peer can open Spilman channels. Clients can also remain bootstrap-only for the entire session if they cannot run Spilman (no ECDH, no balance signing capability). |
| Exposure | The entire payment is upfront. If the provider disappears after receiving the token, the client loses the full payment amount. | Bounded by channel capacity. A new peer starts with a small channel (default 10 sats). The maximum exposure per metering interval is 5 seconds of delivery. The time-locked refund path ensures the sender can reclaim funds if the receiver disappears. |

### 10. CDK / Cashu Dependency

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| CDK version | `cdk 0.17` (published on crates.io, per `Cargo.toml` line 29), `cdk-sqlite 0.17` (published, line 30), `cashu 0.17` (published, line 33). Wallet feature only (no mint feature). | `cdk` and `cdk-sqlite` pinned to git tag `v0.16.0` from `https://github.com/cashubtc/cdk.git` (not published on crates.io, per workspace `Cargo.toml` lines 57-58). `cashu` also from the same git tag with `mint` feature enabled (line 37). `cdk-spilman` from `https://github.com/SatsAndSports/cashu_spilman_channels` branch `main` (line 52, unpublished). |
| CDK usage | Wallet only: `receive()` for token intake, `total_balance()` for aggregate balance, per-mint `total_balance()`. Send and melt are not used in v1 (the binary is a merchant, not a spender). The `Wallet::new` call at `wallet.rs` line ~100 creates a CDK wallet for each accepted mint backed by `WalletSqliteDatabase`. | Wallet for bootstrap tokens (via CDK receive). `cdk-spilman` for Spilman channel operations: `fund_channel()` creates 2-of-2 multisig, `sign_balance_update()` signs incremental balances, `settle_channel()` submits to mint. The `Wallet` trait in core abstracts over the concrete implementation, so the core never touches CDK directly. |
| Cashu primitives | `cashu 0.17` for token parsing (NUT-00 format), Y-value extraction from proofs, and NUT types. The verify module at `wallet/verify.rs` parses tokens and checks mint acceptance. | `cashu` from the same CDK git tag, with the `mint` feature enabled (for protocol-primitives layer: token parsing, proof types, mint HTTP client). Used by the protocol layer and the v1-compat wallet adapter. |
| Feature gating | No feature flags. All dependencies are unconditional. The binary always includes CDK, cdk-sqlite, cashu, axum, tokio, secp256k1, reqwest, etc. | `v1-compat` feature gates: `nostr 0.44`, `cdk` (git v0.16.0), `cdk-sqlite` (git v0.16.0), `url`, `base64` (per `tollgate-net/Cargo.toml` line 64). `spilman` feature gates: `cdk-spilman` with `wallet` and `configurable-host-reqwest` features (lines 74-78). `openwrt` feature gates: `rtnetlink`, `ipnetwork`, `netlink-packet-route`, `netlink-packet-core`, `futures`, `async-trait` (lines 65-73). |
| Version mismatch risk | Low. Uses published crates.io versions. Can update independently. | Higher. Pinned to an unpublished git tag. Updating requires coordinating across cdk, cdk-sqlite, cashu, and cdk-spilman. The CDK v0.16 -> v0.17 gap between the two projects means wallet databases are not compatible. |

### 11. Maturity / Status

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Phase | Phase 7 of 8. Phases 0-6 complete: scaffolding, HTTP routes, token verification, CDK wallet, session management, OpenWrt packaging, wallet migration. Phase 7 (test parity on physical hardware) is in progress. | Design phase transitioning to implementation. Protocol and core design documents are complete and detailed (9 design docs totaling ~2,500 lines). `tollgate-protocol` and `tollgate-core` crates compile and have unit tests. `tollgate-net` has a working binary with Docker integration tests and PRTA cloud VM tests. The ROADMAP tracks FIPS integration as Phase 1. |
| What works | 58 unit tests pass. HTTP server (axum on 127.0.0.1:2121), CLI socket (Unix domain), config loading (Go-compatible JSON), Nostr event signing (BIP-340), CDK wallet receive/balance, session management (create, expiry, usage, revoke, cleanup), ndsctl output parsing (6 edge cases), migration from Go wallets (gonuts-export to CDK receive). | 155 unit tests across 24 files. Docker integration tests: detect (mutual Announce between gateway and client containers), bootstrap (fake-mint payment verification via NUT-07 check-state stub), protocol-disconnect (orderly teardown without crash), protocol-reject (rejection handling without crash). PRTA test suite runs via `scripts/shc-prta-test.py` against a cloud VM (Sparrow Hosting Cloud) with `TOLLGATE_BACKEND=rust`. |
| What does not work | `/ln-invoice` endpoints are stubs (hardcoded responses). No end-to-end payment flow tested against a live mint. No physical hardware deployment yet. Kind 1022 Nostr events use placeholder `id` and empty `sig` (Phase 7 task). | Full Spilman channel flow is not tested end-to-end (the `spilman/` module exists with service.rs and wallet.rs but integration tests are not in the testing/ directory). FIPS integration (the primary deployment target per ROADMAP) is planned but not started (Phase 1 items are unchecked). Test directories exist for future milestones (`sandbox/`, `metering/`, `exhaust/`, `drift/`, `traffic/`) but their scripts may not be implemented yet. |
| Known issues | The Go-compatible config schema carries a lot of upstream-related fields (`upstream_detector`, `upstream_session_manager`, `upstream_wifi`) that the Rust binary does not actively use. These exist for JSON compatibility only. | CDK version is pinned to v0.16.0 (git), while the Cashu ecosystem has moved to v0.17+. The `cdk-spilman` dependency is from an unpublished fork. The `v1-compat` and `spilman` features are mutually exclusive with each other in some code paths (they both provide wallet functionality). |

### 12. Deployment Targets

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Primary target | OpenWrt routers (captive portal via Nodogsplash). Cross-compiled for x86_64, aarch64, and armv7 musl targets. Static PIE binaries, zero runtime dependencies. | Linux, macOS, Windows (IP peering), OpenWrt (via feature flags), FIPS mesh network (primary goal per ROADMAP Phase 1). |
| Secondary targets | Any Linux host (for development and testing). No Windows or macOS support needed. | ESP32 via a separate `tollgate-net-esp32` project that consumes `tollgate-core` (per README line 20). This project does not exist yet. |
| Constrained devices | Not designed for. Requires tokio async runtime, SQLite (via cdk-sqlite), file I/O for config and wallet, and subprocess spawning for ndsctl. Even with musl static linking, the memory footprint is too high for microcontrollers. | `tollgate-core` and `tollgate-protocol` are `#![no_std]` with `extern crate alloc` (per `tollgate-protocol/src/lib.rs` line 7, `tollgate-core/src/lib.rs` line 21). They compile for ESP32. The host binary (`tollgate-net-esp32`) would use ESP-IDF runtime and a custom wallet/resource adapter with different constraints. |
| Binary size | ~1.5 MB (stripped, musl, Phase 0 smoke test). Expected 3-5 MB with full CDK integration. Release profile at `Cargo.toml` lines 60-66: `panic = "abort"` (no unwinding tables), `strip = true` (no debug symbols), `opt-level = "z"` (optimize for size), `lto = true` (cross-crate link-time optimization), `codegen-units = 1` (maximum optimization opportunity). This produces a ~3-4x size reduction vs the ~12 MB Go binary. | Not measured yet. No release profile tuning in the workspace Cargo.toml. The `tollgate-net` binary pulls in ratatui (TUI), clap (CLI), serde_yaml, and other dependencies that would increase size significantly. For ESP32, the `no_std` core avoids pulling in these dependencies. |
| Cross-compilation | Documented in README lines 110-126. Uses `rustup target add` for musl targets and `.cargo/config.toml` for zig cc linker wrappers on ARM. Three targets: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, armv7-unknown-linux-musleabihf. | Not yet documented. The `openwrt` feature flag exists but no cross-compilation instructions are in the repository. |

### 13. Feature Flags / Conditional Compilation

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Feature flags | None. The binary is a single compilation unit with no conditional features. All dependencies are always included. | Three feature groups on `tollgate-net`: |
| | | (1) `v1-compat` (default off): enables the Go-compatible HTTP adapter. Pulls in `nostr 0.44`, `cdk` (git v0.16.0), `cdk-sqlite` (git v0.16.0), `url`, `base64`. Without this feature, the node only speaks the native CBOR protocol. |
| | | (2) `spilman` (default off): enables Spilman channel support. Pulls in `cdk-spilman` from GitHub with `wallet` and `configurable-host-reqwest` features. This is the planned primary payment mechanism for the native protocol. |
| | | (3) `openwrt` (default off): enables OpenWrt-specific resource adapters. Pulls in `rtnetlink`, `ipnetwork`, `netlink-packet-route`, `netlink-packet-core`, `futures`, `async-trait`. Enables `tokio/process` for subprocess calls. |
| no_std | No. Requires `std` (tokio, SQLite, file I/O, subprocess calls, environment variables). | `tollgate-protocol` and `tollgate-core` are `#![no_std]` with `extern crate alloc`. They use `alloc::vec::Vec`, `alloc::collections::BTreeMap`, and `alloc::string::String` but no heap allocator configuration, no floating point, no OS-specific code. `tollgate-net` is `std` only. |
| unsafe code | Not explicitly denied at the project level. The codebase does not appear to use unsafe. | Workspace-level lint at `Cargo.toml` lines 17-20: `unsafe_code = "deny"`. The comment states: "Core and protocol are no_std and have no reason to reach for unsafe. tollgate-net only drives I/O; keep the whole tree safe by default." |
| Conditional compilation in code | None visible. All modules are always compiled. | `#[cfg(test)]` modules for unit tests. `#![allow(dead_code)]` on the v1-compat module (per `v1_compat/mod.rs` line 10) because "many functions and types are not yet wired into the active code paths." |

### 14. Testing Infrastructure

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Unit tests | 58 tests across 8 test modules (36 `#[test]` annotations found across 5 source files). Config (7): round-trip Go config/identities/install JSON, defaults, missing/empty files. CLI (7): version string, status, wallet balance, wallet info, unknown command, migrate (nonexistent/empty/invalid tokens). Session (9): create, get, is_active, expiry, usage exhaustion, revoke, cleanup, overwrite. Metering (6): ndsctl output parsing (download+upload sum, missing fields, empty, garbage, non-numeric, whitespace). Wallet (14): open/close cycle, mint acceptance, seed roundtrip, balance, per-mint balance, db path sanitization, receive errors, concurrency, timeout, plus 6 token verification tests (Y-value extraction, mint filtering, invalid tokens, milli-unit scaling). HTTP routes (9): Nostr event token extraction (valid/wrong kind/missing tag/invalid JSON/multiple tags), session creation, 402 status, usage (no session, active, expired). | 155 tests across 24 files (155 `#[test]` annotations found). Coverage by module: protocol codec (6 tests), protocol messages (17), protocol product IDs (5), core pricing (8), core metering (1), core session state machine (9), core access control (2), net config (10), net adapter (7), net OpenWrt UCI operations (21), net WiFi scanner (10), net WiFi connector (8), net network monitor (7), v1-compat pricing (12), v1-compat wallet (3), v1-compat MAC resolver (6), v1-compat adapter (4), v1-compat handlers (2), client (2), status (5), control (1). |
| Integration tests | None in the repository. End-to-end testing is deferred to Phase 7 (physical hardware). The README at line 333 states: "End-to-end payment flow against a live mint (requires network)" and "ndsctl integration on real OpenWrt" are not tested. | Docker-based integration harness in `testing/`. Multi-stage Docker build produces `tollgate-test:latest` (per `testing/README.md`). Test topologies: `detect/` (mutual Announce between gateway and client containers), `bootstrap/` (fake-mint payment verification via NUT-07 stub that reports every proof UNSPENT), `protocol-disconnect/` (client sends Disconnect 0x0E, gateway handles teardown without crashing), `protocol-reject/` (client sends Reject 0x0D with reason code, gateway handles rejection). The bootstrap test uses a Python fake-mint (`testing/bootstrap/fake-mint.py`) running on `python:3-slim`. Protocol tests use `lib/tg_client.py` (minimal Python CBOR client, stdlib only). |
| External test frameworks | Planned: physical-router-test-automation (PRTA) for hardware validation. This is Phase 7. | Active: PRTA test suite runs via `scripts/shc-prta-test.py` which deploys to a Sparrow Hosting Cloud VM, runs `test_rust_v1_api.py` with `TOLLGATE_BACKEND=rust` against port 2121, and captures results. Also `scripts/shc-extensive-test.py` for broader scenario coverage (adapted from PRTA). |
| Test tooling | `tempfile 3` as dev-dependency for wallet DB tests. No mocking framework. | `mockall 0.13` and `mockito 1` as dev-dependencies for mocking wallet and HTTP interactions. `fake-mint.py` (minimal NUT-07 check-state stub, 52 lines). `lib/tg_client.py` (Python CBOR client for protocol-level tests). |
| Future test plans | `testing/` directories planned but not present: metering, exhaust, drift, traffic, sandbox. | Test directories exist for future milestones: `sandbox/`, `metering/`, `exhaust/`, `drift/`, `traffic/`. The README states "the detect harness is the seed; later milestones (bootstrap payment, metering, suspension) extend the same parent-child shape." |

### 15. OpenWrt Packaging Approach

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Makefile | `openwrt/Makefile` (140 lines) follows the prebuilt-binary pattern. CI cross-compiles the Rust binary for each target arch, stages it into the package directory as `tollgate-module-basic-rust`, and the OpenWrt SDK only packages the artifact (no compilation on the router). Package name: `tollgate-wrt`. Depends on `+nodogsplash +jq`. Provides `nodogsplash-files` to claim the captive-portal htdocs path. | No OpenWrt Makefile in the repository. OpenWrt support is planned via the `openwrt` feature flag on `tollgate-net`, which gates netlink and UCI dependencies (`rtnetlink`, `ipnetwork`, `netlink-packet-route`, `netlink-packet-core`). The ROADMAP lists OpenWrt as a target but the packaging artifacts do not exist yet. |
| Init script | `openwrt/files/etc/init.d/tollgate-wrt` (50 lines): procd init script. Binds `127.0.0.1:2121` (HTTP) and `/var/run/tollgate.sock` (CLI). Runs as root (needs ndsctl access). Logs via `logread -e tollgate` (stdout 1, stderr 1). Respawns up to 3 times with 5 second delay. `START=95` (late init, after network). `USE_PROCD=1`. Service name `tollgate-wrt` reuses the existing init.d slot from the Go predecessor for upgrade compatibility. | Not yet created. The design notes in the README at line 156 state that OpenWrt differences are "config paths (UCI vs. XDG), packaging (ipk vs. deb/brew), and resource constraints" but these are not implemented. |
| Captive portal | Ships `openwrt/files/tollgate-captive-portal-site/` with `welcome.html` and `404.html`. The Makefile postinst script creates a symlink `/etc/nodogsplash/htdocs -> /etc/tollgate/tollgate-captive-portal-site`, replacing any existing Nodogsplash portal. If the existing path is a directory (not a symlink), it moves it to `htdocs.backup` first. | TollGate is explicitly device-to-device (per `tollgate-intro.md` line 123: "Captive portal / user interface is built on top, not inside"). The v1-compat layer can serve as a captive-portal backend when the `v1-compat` feature is enabled, but this is an adapter, not the primary design. |
| Upgrade preservation | `openwrt/files/lib/upgrade/keep.d/tollgate` lists files to preserve across firmware upgrades: `/usr/bin/tollgate-module-basic-rust`, `/etc/init.d/tollgate-wrt`, `/etc/tollgate/tollgate-captive-portal-site/*`. This ensures the binary, init script, and portal site survive sysupgrade. | Not yet addressed. No keep-list file, no upgrade preservation strategy documented. |
| Pre/post install scripts | preinst: creates `/etc/tollgate/` directory, stamps `install_time` in `install.json` (using `jq`). postinst: creates captive-portal symlink, enables and starts the service. | Not applicable (no packaging yet). |

---

## Narrative Analysis

tollgate-module-basic-rust exists because of a specific, acute bug: the Go Cashu library's non-atomic swap operation can permanently brick a wallet. The swap-counter race means a transient network failure between persisting the keyset counter and saving the resulting proofs leaves the counter advanced past the highest stored proof index. Every subsequent operation fails with error 10002 "blinded message already signed". The only recovery is a manual database edit. This is not a theoretical concern; it has been observed in production. CDK's saga pattern fixes this by making wallet operations atomic: either the full receive/send/melt completes and persists, or no state changes at all. The Rust rewrite is a vehicle for CDK adoption, and every design decision is constrained by the need to be a drop-in replacement for the Go binary. Same HTTP routes, same config files, same Nostr event shapes, same CLI commands. The benefit is immediate: operators can swap the binary without changing anything else. The cost is that the project inherits all the limitations of the v1 design.

tollgate-rs starts from a different question entirely. The v1 design works for its original use case (a single router selling WiFi access via a captive portal), but it does not scale to mesh networks, per-peer pricing, streaming micropayments, or non-network resources. The protocol is resource-agnostic: the same core library can power a Linux router, an ESP32 microcontroller, or a hypothetical electricity meter. The architecture enforces this via the sans-IO pattern: tollgate-core is a pure synchronous state machine that performs no I/O, and tollgate-protocol is a `no_std` CBOR message library. The host provides a Wallet implementation and a ResourceAdapter implementation, and the core drives the payment lifecycle through Events and Actions. This separation is what allows the same protocol logic to compile for both a tokio-powered Linux binary and an ESP-IDF microcontroller. The tradeoff is complexity and immaturity. The design documents are thorough (nine core documents totaling approximately 2,500 lines covering pricing, metering, access control, payment channels, protocol messages, configuration, bootstrap, and FIPS integration), but the implementation is still catching up.

The payment model difference is the most consequential architectural divergence between the two projects. tollgate-module-basic-rust uses a single Cashu token per session: the client pays upfront, gets a fixed allotment, and when it is exhausted, the session ends. This is simple but wasteful: if the client uses only half their allotment, the remaining balance is stuck until the session expires. It also requires mint connectivity for every payment, meaning a mint outage blocks all new sessions. tollgate-rs uses Spilman unidirectional payment channels: the client locks ecash in a 2-of-2 multisig and signs incremental balance updates every 5 seconds. Payment is streaming, not upfront. The mint is only needed for channel lifecycle transitions (funding, rollover, settlement), not for ongoing payment. This enables offline resilience, fine-grained accounting, and netting (if both sides deliver resources to each other, only the net delta moves). The cost is protocol complexity: 15 message types, channel lifecycle management, rollover logic, expiry handling, and reboot recovery.

The two projects are not on a collision course. tollgate-module-basic-rust solves an immediate production problem (wallet bricking) with a constrained, compatibility-first approach. tollgate-rs designs the protocol for the next decade of TollGate deployments. The v1-compat feature in tollgate-rs is a bridge: if it reaches full parity, tollgate-rs could serve as both a v1-compatible drop-in and a v2 protocol node, eventually making tollgate-module-basic-rust redundant. But that convergence is theoretical. Today, they serve different audiences and different time horizons. An operator running a production OpenWrt captive portal today should use tollgate-module-basic-rust (or stay on Go). An engineer building a mesh network with per-peer pricing and Spilman channels should work with tollgate-rs.

---

## Relationship Between the Projects

These are two separate efforts with different upstreams, different goals, and different architectural foundations. They are not forks of each other, and they do not share any code.

**tollgate-module-basic-rust** is a drop-in replacement for `tollgate-module-basic-go`. Its repository is `felixfelix-bot/tollgate-module-basic-rust` (per `Cargo.toml` line 8). It rewrites the Go binary in Rust using CDK instead of gonuts, preserving the exact same HTTP API, CLI interface, config file format, and Nostr event shapes. The motivation is documented at `README.md` lines 33-44: the Go Cashu library has a swap-counter race that permanently bricks wallets, and CDK's saga pattern eliminates this class of bug. Every design decision is constrained by the need to be a drop-in replacement.

**tollgate-rs** is a clean-sheet protocol redesign under the `OpenTollGate` organization. Its repository is `OpenTollGate/tollgate-rs` (per workspace `Cargo.toml` line 14). It starts from the question "what should the TollGate protocol actually be?" and builds a resource-agnostic, mesh-capable, channel-based payment layer. The architecture separates a `no_std` protocol library and a sans-IO core state machine from platform-specific binaries. This is a fundamentally different approach from the v1 tree-topology, single-token, captive-portal model. The intro doc at lines 262-271 explicitly lists five differences from v1: mesh vs tree, Spilman vs tokens, device-to-device vs human-to-device, network-agnostic vs OpenWrt-only, per-peer vs single price.

The two projects share some surface similarities because they both implement TollGate concepts: both use Cashu ecash, both use secp256k1 for signing, both target OpenWrt routers, both serve as merchant nodes that receive payments. But the internals are different enough that they are not interchangeable. tollgate-module-basic-rust talks Nostr events over JSON HTTP; tollgate-rs talks CBOR messages over a polling or WebSocket transport. tollgate-module-basic-rust uses single-token payments; tollgate-rs uses Spilman payment channels. tollgate-module-basic-rust has one global price; tollgate-rs has per-peer, per-product, per-mint pricing with dynamic adjustment. tollgate-module-basic-rust uses ndsctl for metering; tollgate-rs defines a ResourceAdapter trait for metering. tollgate-module-basic-rust uses JSON config; tollgate-rs uses YAML config.

**Convergence is theoretically possible.** tollgate-rs has a `v1-compat` feature that implements the Go v1 HTTP API. The adapter layer in `crates/tollgate-net/src/v1_compat/` (15 modules, including handlers at 824 lines) translates v1 requests into the driver's internal operations. When this feature is compiled in, tollgate-rs exposes the same routes at the same port with the same Nostr event shapes. If the v1-compat layer reaches parity with tollgate-module-basic-rust, tollgate-rs could serve as both a v1 drop-in and a v2 protocol node. The v1-compat pricing module (`v1_compat/pricing.rs`, 337 lines) already implements `select_cheapest_compatible()` which mirrors the Go Chandler's `selectCompatiblePricingOption` logic, and the handlers module covers the same HTTP endpoints.

**The physical-router-test-automation (PRTA) framework tests both projects as separate backend types.** The PRTA suite uses the `TOLLGATE_BACKEND` environment variable to select which binary to deploy and test. For tollgate-rs, the script at `scripts/shc-prta-test.py` line 195 sets `TOLLGATE_BACKEND=rust` and runs `test_rust_v1_api.py` against it. For tollgate-module-basic-rust, the value would be `rust-basic` (the PRTA framework distinguishes them as separate backends). This confirms they are treated as distinct implementations by the shared test infrastructure.

---

## When to Choose Which

| Scenario | Choice | Why |
|---|---|---|
| Drop-in Go replacement today | **tollgate-module-basic-rust** | Same config files (`config.json`, `identities.json`, `install.json`), same HTTP API (port 2121, same routes, same response shapes (balance schema aligned to Go in commit d2c27e8)), same CLI commands over the same Unix socket, same Nostr event shapes (kinds 10021, 1022, 21000, 21023). Swap the binary, keep everything else. Migration tooling exists (`MIGRATION.md`). |
| Resource-agnostic protocol library | **tollgate-rs** | `tollgate-core` and `tollgate-protocol` are `no_std` and know nothing about what is being sold. Plug in any Wallet implementation and any ResourceAdapter implementation. Works for network bytes, watt-hours, milliliters, or any metered unit. |
| FIPS mesh integration | **tollgate-rs** | FIPS is the primary deployment target in the ROADMAP (Phase 1). Design docs for FIPS peering (`peering-fips.md`) cover bloom filters, per-link metrics, traffic counters, and lifecycle events. The `openwrt` feature flag gates netlink dependencies needed for FIPS. |
| Production OpenWrt captive portal | **tollgate-module-basic-rust** | Has a working OpenWrt Makefile (prebuilt-binary pattern), procd init script, captive-portal site files, upgrade preservation list, and Nodogsplash integration (provides `nodogsplash-files`, creates htdocs symlink). tollgate-rs has none of these. |
| ESP32 deployment | **tollgate-rs** (planned) | `tollgate-core` and `tollgate-protocol` compile for `no_std` + `alloc`. A separate `tollgate-net-esp32` project is planned that would use ESP-IDF runtime and a custom wallet/resource adapter. tollgate-module-basic-rust cannot run on ESP32 (requires tokio, SQLite, file I/O). |
| Spilman payment channels | **tollgate-rs** | Spilman channels are a core part of the protocol design. The `spilman` feature enables `cdk-spilman`. Full channel lifecycle (funding, active, rollover, settlement) is designed and partially implemented. tollgate-module-basic-rust does not support channels. |
| Nostr-based discovery | **tollgate-module-basic-rust** | Discovery via Nostr kind 10021 events is the primary mechanism. The discovery route at `GET /` returns a signed kind 10021 event with pricing and mint tags. tollgate-rs uses its own Announce + PriceSheet protocol messages; Nostr is only available in the v1-compat layer. |
| Starting fresh greenfield | **tollgate-rs** | Cleaner architecture, separation of concerns, trait-based extensibility, resource-agnostic design. Starting with tollgate-module-basic-rust locks you into the v1 API surface (JSON config, ndsctl metering, single-token payments, Nostr events). |
| Per-peer pricing | **tollgate-rs** | Every peer relationship has its own price, per product, per mint, dynamically adjustable. The `adjust()` method applies demand-based and per-peer multipliers, then clamps to floor/ceiling bounds. tollgate-module-basic-rust has one global `price_per_step` for the entire node. |
| Offline resilience | **tollgate-rs** | Balance updates are signed between peers without mint involvement. Payment continues during mint outages. Channels survive connectivity loss (up to their TTL). Bootstrap tokens allow initial connection without any mint history. tollgate-module-basic-rust requires live mint connectivity for every token receive. |
| Dual pricing (time + usage) | **tollgate-rs** | The `Price` struct has both `per_second` and `per_unit` dimensions. The `cost_scaled()` method combines them. Either dimension can be zero for simpler models. tollgate-module-basic-rust only supports usage-based pricing (`price_per_step` * `step_size`). |
| Negative pricing (attract resources) | **tollgate-rs** | Both `per_second` and `per_unit` are signed i64. A leaf node can set a negative price, paying peers to take its outgoing traffic. The cost_scaled method naturally handles negative costs. tollgate-module-basic-rust uses unsigned u64 for price_per_step. |

---

## Migration Paths

### From tollgate-module-basic-go to tollgate-module-basic-rust

This is the documented, supported migration path. See `MIGRATION.md` at `/home/ubuntu/src/tollgate-module-basic-rust/MIGRATION.md` (438 lines).

**Pre-migration** (MIGRATION.md lines 24-82): back up `/etc/tollgate/` (the entire directory, preserving `wallet.db`, `config.json`, `identities.json`, `install.json`). Note the current wallet balance via `echo "wallet balance" | socat - UNIX-CONNECT:/var/run/tollgate.sock`. Ensure the mint is reachable (CDK `receive()` contacts the mint for a swap). Stop the Go binary. Ensure `gonuts-export` is available (default `/usr/bin/gonuts-export`, overridable via `GONUTS_EXPORT_PATH`).

**Automated first-boot** (MIGRATION.md lines 122-188): when the Rust binary starts and detects `wallet.db` exists, `wallet.sqlite` does not exist, and `.migration_complete` is absent, it runs `gonuts-export /etc/tollgate/wallet.db /etc/tollgate/tokens.jsonl`. This exports all proofs as Cashu token strings (one per line). The operator then runs the migrate CLI command. If `gonuts-export` fails or is not found, a warning is logged and the wallet starts empty. The old `wallet.db` is never modified.

**Manual migration** (MIGRATION.md lines 192-248): run `echo "migrate /etc/tollgate/tokens.jsonl" | socat - UNIX-CONNECT:/var/run/tollgate.sock`. The CLI calls `Wallet::receive(token)` for each line. Each token is imported independently; previously imported tokens are skipped. The response is a JSON report with `total`, `imported`, `failed`, and `errors` fields. Verify the balance matches the pre-migration value. Mark complete with `touch /etc/tollgate/.migration_complete`.

**Rollback** (MIGRATION.md lines 419-437): stop the Rust binary, remove `wallet.sqlite` and `.migration_complete`, restore `wallet.db` from backup, start the Go binary. The Go wallet is never modified by the Rust binary. The migration is fully reversible at any point.

**Bricked wallet handling** (MIGRATION.md lines 253-291): even if the Go wallet's keyset counter is advanced past the highest stored proof index (the bricking condition documented at `docs/brick-detection.md`), the proofs themselves are valid and can be exported as tokens. They import into a fresh CDK wallet with correct counter management. No manual counter fix-up is needed. CDK's HD wallet derives its own indices independent of the gonuts counter.

### From tollgate-module-basic-go to tollgate-rs

This path uses tollgate-rs's `v1-compat` feature. When compiled with `--features v1-compat`, tollgate-rs exposes the Go-compatible HTTP surface (same routes, same Nostr event shapes) via the adapter layer in `crates/tollgate-net/src/v1_compat/` (15 modules totaling approximately 3,000 LOC). The `build_v1_router()` function at `v1_compat/mod.rs:42` constructs an axum Router that handles the Go-compatible endpoints.

The migration of wallet state would follow the same gonuts-export pattern, since both Rust projects use CDK for wallet operations. However, there is a CDK version mismatch: tollgate-rs pins CDK to git tag v0.16.0 while tollgate-module-basic-rust uses published v0.17. The SQLite database schemas may differ between these versions. Wallet database compatibility between the two is not guaranteed.

This path is less tested than the direct Go-to-basic-rust migration. The PRTA test script (`shc-prta-test.py`) deploys tollgate-rs with `TOLLGATE_BACKEND=rust` and runs the standard v1 API test suite, but it does not test the migration of an existing Go wallet database into tollgate-rs.

### From tollgate-module-basic-rust to tollgate-rs

This is a theoretical future migration. No tooling exists for this path. The two projects use different CDK versions (0.17 vs 0.16 git), different config formats (JSON vs YAML with different schemas and different pricing models), and different protocol layers (Nostr events over JSON HTTP vs CBOR messages over polling/WebSocket). A migration would require:

1. **Config conversion**: translating `config.json` to `tollgate.yaml`. The pricing model is different (single `price_per_step` vs dual `per_second`/`per_unit` with `pricing_scale`). The upstream-related fields (`upstream_detector`, `upstream_session_manager`, `upstream_wifi`) have no direct equivalent in tollgate-rs (they are handled by the protocol itself).

2. **Wallet migration**: CDK SQLite from v0.17 to v0.16. This is not a supported migration direction (usually one migrates forward). The alternative is to wait for tollgate-rs to update to CDK v0.17+, or to export tokens from the v0.17 wallet and re-import them into a fresh v0.16 wallet via the bootstrap token path.

3. **Captive-portal adaptation**: the existing HTML portal files would need to work with the new HTTP transport. In v1-compat mode, tollgate-rs serves the same JSON REST API, so the portal might work without changes. In native mode, the portal would need to speak CBOR or the operator would need a different frontend entirely.

4. **Metering replacement**: ndsctl-based metering (subprocess call to `ndsctl state <mac>`) would need to be replaced with the ResourceAdapter trait implementation. The `openwrt` feature flag provides netlink-based monitoring, but it is not a drop-in for ndsctl.

---

## Technical Depth

### Wallet Atomicity

**tollgate-module-basic-rust** uses CDK's saga pattern. The `TollWallet` struct at `/home/ubuntu/src/tollgate-module-basic-rust/src/wallet/wallet.rs:62` wraps `HashMap<String, Arc<Mutex<Wallet>>>` behind a tokio Mutex. The outer Mutex serializes all wallet operations across the async runtime (acceptable because wallet ops are infrequent, one per payment, per the architecture doc at line 356-358). Each CDK `Wallet` instance is backed by `cdk-sqlite::WalletSqliteDatabase`, created at `wallet.rs` approximately line 100 via `Wallet::new(mint_url, unit, localstore, seed)`. When `receive()` is called, CDK begins a saga: it derives secrets from the HD wallet, contacts the mint for a swap (POST /swap), and persists the resulting proofs and the advanced keyset counter in a single SQLite transaction. If the mint call fails or the swap returns an error, the saga rolls back and no state changes at all. This is documented in the architecture diagram at `architecture.md` lines 128-143, which contrasts the gonuts flow (increment counter, then save proofs, with a race window between the two) with the CDK flow (begin saga, derive secrets, POST /swap, persist proofs + advance counter in single txn, commit saga, OR rollback saga with nothing persisted). The `OP_TIMEOUT` at `wallet.rs` line 38 is set to 30 seconds to match the Go binary's timeout behavior.

**tollgate-rs** delegates wallet operations to the `Wallet` trait defined in the design docs at `tollgate-payment-channels.md` lines 360-404. The trait has 10 async methods: `receive_token`, `create_token`, `fund_channel`, `verify_funding`, `sign_balance_update`, `verify_balance_update`, `settle_channel`, `mint_reachable`, `balance`, and `compute_channel_secret`. The concrete implementation for `tollgate-net` uses `cdk-spilman`, which builds on top of CDK's wallet functionality but adds Spilman-specific operations. `fund_channel()` creates a 2-of-2 multisig token with NUT-11 spending conditions: `P2PK: (Sender AND Receiver) OR (Sender after expiry)`. The sender derives a channel secret via ECDH with the receiver's pubkey, constructs deterministic blinded outputs, and sends the funding proofs to the receiver. `sign_balance_update()` signs an incremental balance update without contacting the mint. `settle_channel()` submits the latest signed balance update to the mint for a swap, producing receiver earnings and sender change.

Atomicity in the Spilman model comes from a different mechanism than CDK's saga pattern. The 2-of-2 multisig ensures that the sender cannot reclaim funds until the channel's time-locked refund path activates (the expiry timestamp). The receiver cannot spend until they submit the signed balance update to the mint. Both parties have cryptographic assurances. The channel state is held in memory by both peers, not persisted to disk. The design doc at `tollgate-payment-channels.md` line 234 explicitly states: "Nodes are not expected to persist runtime state between restarts." The reboot recovery model has bounded exposure: worst case is one channel's worth of earned income for the rebooted peer (if they were a receiver and lost their copy of the latest signed balance update).

### Nostr Event Signing

**Both projects** use `secp256k1` BIP-340 Schnorr signatures for Nostr event signing. The implementations are equivalent because BIP-340 is a fixed standard.

**tollgate-module-basic-rust** implements signing directly at `/home/ubuntu/src/tollgate-module-basic-rust/src/nostr_event.rs:24-59`. The `create_event()` function takes a kind, tags, content, and SecretKey. It creates a `Secp256k1::new()` context, derives a Keypair from the secret key, extracts the X-only public key as a hex string (used as the Nostr pubkey, not the full compressed key), gets the current Unix timestamp, serializes the canonical JSON array `[0, pubkey_hex, created_at, kind, tags, content]` via `serde_json`, hashes it with SHA-256 to produce the event ID, and signs the ID hash using `secp256k1::schnorr::sign_schnorr_no_aux_rand()`. The signature is the 64-byte BIP-340 Schnorr signature encoded as hex. The keypair is loaded from `identities.json` at startup (or auto-generated if none exists, per `architecture.md` lines 207-209). The secret key file permissions are 0600.

**tollgate-rs** uses the `nostr 0.44` crate for Nostr operations, but only when the `v1-compat` feature is enabled (pulled in via `tollgate-net/Cargo.toml` line 39). The v1-compat Nostr module at `crates/tollgate-net/src/v1_compat/nostr.rs` handles event creation and signing for the Go-compatible API surface. The v1-compat merchant module handles key loading and event construction. For the native protocol, peer identification does not use Nostr events. The Announce message carries a raw 33-byte compressed secp256k1 public key. Spilman balance updates use 64-byte Schnorr signatures, but these are protocol-level signatures over channel state (channel_id + cumulative_balance), not Nostr event signatures. The `secp256k1` crate version differs: tollgate-module-basic-rust uses `0.29`, tollgate-rs uses `0.30` (per workspace `Cargo.toml` line 53), but both produce BIP-340-compatible signatures.

The two projects sign the same way because BIP-340 is BIP-340, but they sign different things in different contexts. tollgate-module-basic-rust signs Nostr events (kind 10021 discovery events are signed; kind 1022 session events currently use placeholder id and empty sig, a Phase 7 task per the README at line 276). tollgate-rs signs channel balance updates in its native protocol, and Nostr events only in v1-compat mode.

### Metering

**tollgate-module-basic-rust** uses ndsctl, the Nodogsplash control utility, for metering. The module at `/home/ubuntu/src/tollgate-module-basic-rust/src/metering/mod.rs:28-42` defines `parse_ndsctl_output()` which iterates over lines looking for `"download:"` and `"upload:"` prefixes, parses the numeric values, and returns `(download + upload, 0)` as the used bytes. The second element of the tuple is always 0 because ndsctl does not report total allotment (the session manager tracks that in memory). The `poll_usage()` function at line 46 spawns `tokio::process::Command::new("ndsctl").args(["state", mac])` as a subprocess, captures stdout, and feeds it to the parser. If ndsctl is unavailable (returns an error, such as file-not-found on a non-OpenWrt system), the function returns an `MeteringError::NotFound` variant. The caller in the HTTP routes treats any metering error as `(0, 0)`, providing graceful degradation for development environments. The session manager at `session/mod.rs:107-111` has `update_usage(mac, used)` which sets `s.used = used`. There is no bidirectional reporting (the provider does not know how much the client thinks they used), no transit loss reconciliation, no calibration mechanism, and no continuous counter stream. Metering is polled, not pushed.

**tollgate-rs** defines a `Counters` struct in `tollgate-core/src/metering.rs:6-11` with two cumulative u64 fields: `delivered` (units we sent to the peer, outbound, what we charge for) and `received` (units the peer sent to us, inbound). Both fields are cumulative since session start (the ChannelReady baseline), not per-interval deltas. This is a deliberate design choice for self-healing: if a MeteringReport is lost, duplicated, or delivered out of order, the next report still carries the correct totals, and both sides can recompute the delta locally. The `reconcile()` function at line 19 returns the higher of two values: `if local > remote { local } else { remote }`. This implements the "honest-provider-optimistic" rule documented in `tollgate-metering.md` lines 50-58: bill the higher count because the provider expended resources sending them, even if some were lost in transit. If the discrepancy exceeds the configurable tolerance (default 5%), a warning is issued. If it persists for 3+ consecutive intervals, the channel is closed. The `ResourceAdapter` trait provides `subscribe_meter()` which returns a `MeterStream` with `watch::Receiver<u64>` channels for `delivered` and `received`. This is a push model: the implementation pushes counter updates as they change, and the core takes a snapshot at each metering interval.

### Pricing

**tollgate-module-basic-rust** uses a flat per-step pricing model. The `MintConfig` struct at `/home/ubuntu/src/tollgate-module-basic-rust/src/config/schema.rs:112-129` has `price_per_step: u64` (default 1 sat per `step_size` bytes) and the global `Config.step_size: u64` (default 22020096 bytes, approximately 21 MiB, per `schema.rs` line 80). When a client pays N sats, the session allotment is computed as `N * 22020096` bytes. The `metric` field (default "bytes", per `schema.rs` line 47) determines the unit; if set to "time", the allotment would be in milliseconds instead. There is one price for the entire node, regardless of which peer is connecting or which mint they use. The `margin` field (default 0.1, line 81) and the `upstream_session_manager` config (max_price_per_millisecond, max_price_per_byte, trust policies, session increments) exist for upstream purchasing but do not affect the downstream pricing model presented to clients. There is no dynamic pricing, no per-peer differentiation, and no time-based charging component. The `purchase_min_steps` field (default 0) allows setting a minimum purchase amount.

**tollgate-rs** uses a dual-dimension pricing model defined in `tollgate-core/src/pricing.rs:14-18`. The `Price` struct has two signed i64 fields: `per_second` and `per_unit`. Both are scaled integers divided by a `pricing_scale` divisor (u32, default 1000, defined in the protocol crate's `DEFAULT_PRICING_SCALE`). With a scale of 1000, a `per_unit` value of 10 means 0.01 sat per unit. The `cost_scaled()` method at line 26 computes `(elapsed_ms * per_second) / 1000 + units * per_unit` using i128 intermediate arithmetic to avoid overflow, then tries to convert back to i64 (clamping to MAX/MIN on overflow). The method supports negative costs (negative pricing: the node pays the peer to attract resources, a core economic mechanism documented in `tollgate-pricing.md` lines 156-161). A `Product` struct wraps a pricing scale, per-mint `MintPrice` entries, and opaque extension bytes. The product ID is a SHA256 hash over a canonical byte layout with a domain-separation tag ("tollgate/product-id/v1"), ensuring cross-implementation stability (per `tollgate-pricing.md` lines 83-99). The `PriceBounds` struct enforces floor and ceiling on both dimensions. The `adjust()` method at line 86 applies an active-peer factor (demand-based boost: `price * (1 + factor)`) and a per-peer multiplier, then clamps to bounds. Pricing can change at every metering interval by piggybacking new product IDs on the MeteringReport message (fields 4-5 in the protocol). The peer must accept or close the channel.

---

## References

### tollgate-module-basic-rust

| File | Lines | Relevance |
|------|-------|-----------|
| `/home/ubuntu/src/tollgate-module-basic-rust/README.md` | 1-388 | Project overview, phase status, tech stack, HTTP endpoints, CLI, testing, binary size |
| `/home/ubuntu/src/tollgate-module-basic-rust/MIGRATION.md` | 1-438 | Go-to-Rust migration runbook, swap-counter race analysis, bricked wallet detection |
| `/home/ubuntu/src/tollgate-module-basic-rust/Cargo.toml` | 1-66 | Dependencies (cdk 0.17, axum 0.8, secp256k1 0.29, no feature flags), release profile |
| `/home/ubuntu/src/tollgate-module-basic-rust/docs/architecture.md` | 1-359 | Module structure, payment data flow, wallet saga pattern, session management, ndsctl metering, persistence model, binary size, thread model |
| `/home/ubuntu/src/tollgate-module-basic-rust/src/wallet/wallet.rs` | 1-571 | TollWallet struct (line 62), CDK saga pattern, receive/balance operations, OP_TIMEOUT, WalletError enum |
| `/home/ubuntu/src/tollgate-module-basic-rust/src/nostr_event.rs` | 1-60 | NostrEvent struct, create_event() with BIP-340 Schnorr signing, SHA-256 event ID |
| `/home/ubuntu/src/tollgate-module-basic-rust/src/metering/mod.rs` | 1-63 | parse_ndsctl_output() (line 28), poll_usage() subprocess (line 46), MeteringError enum |
| `/home/ubuntu/src/tollgate-module-basic-rust/src/session/mod.rs` | 1-121 | CustomerSession struct, SessionManager with HashMap, create_session, is_active, update_usage, cleanup_expired |
| `/home/ubuntu/src/tollgate-module-basic-rust/src/config/schema.rs` | 1-464 | Config struct, MintConfig with price_per_step (line 123), step_size (line 21), Go-compatible schema |
| `/home/ubuntu/src/tollgate-module-basic-rust/openwrt/Makefile` | 1-140 | OpenWrt package build (prebuilt-binary pattern), preinst/postinst scripts, upgrade keep list |
| `/home/ubuntu/src/tollgate-module-basic-rust/openwrt/files/etc/init.d/tollgate-wrt` | 1-50 | procd init script (port 2121, tollgate.sock, respawn 3/5, START=95) |

### tollgate-rs

| File | Lines | Relevance |
|------|-------|-----------|
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/README.md` | 1-143 | Project overview, architecture diagram, key properties, project structure, prior work |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/ROADMAP.md` | 1-66 | FIPS integration Phase 1, internet exit Phase 2, full mesh Phase 3, mobile Phase 4 |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/Cargo.toml` | 1-59 | Workspace structure (3 crates), CDK v0.16.0 git pin, v1-compat/spilman/openwrt features, unsafe_code deny |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-intro.md` | 1-317 | Goals, architecture (sans-IO, trait boundaries), payment lifecycle, security threat model, prior work v1 comparison |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-pricing.md` | 1-421 | Dual pricing (time + units), pricing_scale, Product struct, product_id canonical layout, dynamic pricing, operator controls |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-protocol.md` | 1-553 | CBOR encoding rationale, 15 message types with CBOR definitions, HTTP polling + WebSocket transports, message sequences, size estimates |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-payment-channels.md` | 1-477 | Spilman channel lifecycle, funding (NUT-11 2-of-2), active metering, rollover mechanics, offline resilience, Wallet trait, netting, channel capacity growth |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-metering.md` | 1-119 | Counters (cumulative since session start), calibration, transit loss resolution, ResourceAdapter trait, MeterStream |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/docs/design/core/tollgate-configuration.md` | 1-363 | YAML config schema, identity, products, dynamic pricing rules, channel parameters, metering, bootstrap, mints, peer overrides, runtime changes |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-protocol/Cargo.toml` | 1-16 | no_std + alloc, minicbor + sha2 dependencies |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-protocol/src/lib.rs` | 1-21 | no_std declaration, exported types (message types, codec, product) |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-core/Cargo.toml` | 1-15 | no_std description, depends only on tollgate-protocol |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-core/src/lib.rs` | 1-40 | no_std + alloc, sans-IO architecture explanation, module list |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-core/src/pricing.rs` | 1-213 | Price struct (line 14), cost_scaled() (line 26), Product (line 38), PriceBounds (line 55), adjust() (line 86), 8 unit tests |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-core/src/metering.rs` | 1-33 | Counters struct (line 6), reconcile() (line 19), 1 unit test |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-core/src/session.rs` | 1-702 | PeerPhase enum (line 19), PeerSession struct (line 35), Session state machine, BTreeMap storage |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-net/Cargo.toml` | 1-79 | Feature flags (v1-compat line 64, spilman line 74, openwrt line 65), ratatui TUI, cdk-spilman git dep |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-net/src/v1_compat/mod.rs` | 1-47 | v1-compat module list (15 modules), build_v1_router() (line 42) |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-net/src/v1_compat/handlers.rs` | 1-824 | Go-compatible HTTP handler implementations, extract_token_amount_and_allotment(), decode_cashu_amount() |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/crates/tollgate-net/src/v1_compat/pricing.rs` | 1-337 | PricingOption struct, select_cheapest_compatible(), PricingError enum, 12 unit tests |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/testing/README.md` | 1-55 | Docker integration test harness, topology descriptions (detect, bootstrap, disconnect, reject) |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/scripts/shc-prta-test.py` | 1-235 | PRTA test runner, TOLLGATE_BACKEND=rust, cloud VM deployment, test_rust_v1_api.py |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/scripts/shc-extensive-test.py` | 1-130+ | Extensive test suite adapted from PRTA |
| `/home/ubuntu/src/tollgate-rs-ai-research-and-experiments/testing/bootstrap/fake-mint.py` | 1-52 | Minimal NUT-07 check-state stub (every proof UNSPENT) |

---

## Dependency Comparison

### Runtime Dependencies (tollgate-module-basic-rust)

| Crate | Version | Source | Purpose |
|-------|---------|--------|---------|
| `tokio` | 1 | crates.io | Async runtime. Features: rt-multi-thread, macros, signal, sync, net, time, io-util, fs, process. |
| `axum` | 0.8 | crates.io | HTTP server. Features: ws (WebSocket support, though not used in v1). |
| `tower` | 0.5 | crates.io | HTTP middleware stack. Features: util. |
| `tower-http` | 0.6 | crates.io | HTTP middleware. Features: cors, timeout, trace. |
| `http-body-util` | 0.1 | crates.io | HTTP body utilities for axum 0.8 / hyper 1.x compatibility. |
| `cdk` | 0.17 | crates.io | Cashu Development Kit. Features: wallet only (no mint). The core dependency for wallet operations. |
| `cdk-sqlite` | 0.17 | crates.io | CDK persistence backend. SQLite storage for wallet state (proofs, keysets, sagas). |
| `cashu` | 0.17 | crates.io | Cashu protocol primitives. Token parsing (NUT-00 format), proof types, NUT type definitions. |
| `reqwest` | 0.12 | crates.io | HTTP client for mint communication. Features: rustls-tls (no OpenSSL for musl compat), json. |
| `secp256k1` | 0.29 | crates.io | BIP-340 Schnorr signing for Nostr events. Features: rand, global-context, serde. |
| `serde` | 1 | crates.io | Serialization framework. Features: derive. |
| `serde_json` | 1 | crates.io | JSON serialization for config, CLI responses, Nostr events. |
| `tracing` | 0.1 | crates.io | Structured logging facade. |
| `tracing-subscriber` | 0.3 | crates.io | Logging output. Features: env-filter for runtime log level control. |
| `thiserror` | 1 | crates.io | Error derive macro for WalletError, MeteringError. |
| `sha2` | 0.10 | crates.io | SHA-256 hashing for Nostr event IDs. |
| `hex` | 0.4 | crates.io | Hex encoding/decoding for event IDs, signatures, keys. |
| `rand` | 0.8 | crates.io | Key and seed generation. |

### Workspace Dependencies (tollgate-rs)

| Crate | Version | Source | Scope | Purpose |
|-------|---------|--------|-------|---------|
| `tollgate-protocol` | 0.1.0 (path) | local | protocol, core, net | CBOR message types, canonical encoding. no_std. |
| `tollgate-core` | 0.1.0 (path) | local | core, net | Resource-agnostic state machine. no_std. |
| `minicbor` | 0.24 | crates.io | protocol | CBOR encoding/decoding. Features: alloc, derive. no_std-compatible. |
| `sha2` | 0.10 | crates.io | protocol | Product ID hashing (no default-features, stays no_std). |
| `cashu` | git v0.16.0 | github.com/cashubtc/cdk | net | Protocol primitives. Features: mint (unlike basic-rust which uses wallet). |
| `cdk` | git v0.16.0 | github.com/cashubtc/cdk | net (v1-compat) | Cashu Dev Kit wallet. Features: wallet (v1-compat only). |
| `cdk-sqlite` | git v0.16.0 | github.com/cashubtc/cdk | net (v1-compat) | CDK persistence (v1-compat only). |
| `cdk-spilman` | git main | github.com/SatsAndSports/cashu_spilman_channels | net (spilman) | Spilman payment channels. Features: wallet, configurable-host-reqwest. |
| `tokio` | 1 | crates.io | net | Async runtime. Same features as basic-rust. |
| `axum` | 0.8 | crates.io | net | HTTP server. Features: ws. |
| `clap` | 4 | crates.io | net | CLI argument parsing. Features: derive. |
| `serde_yaml` | 0.9 | crates.io | net | YAML config parsing. |
| `nostr` | 0.44 | crates.io | net (v1-compat) | Nostr event creation and signing (v1-compat only). |
| `ratatui` | 0.29 | crates.io | net | TUI monitoring tool (tolltop). |
| `secp256k1` | 0.30 | crates.io | net | BIP-340 signing. Features: rand, global-context. |
| `dirs` | 6.0 | crates.io | net | XDG config directory resolution. |
| `reqwest` | 0.12 | crates.io | net | HTTP client. Features: json, rustls-tls. |
| `rtnetlink` | 0.21 | crates.io | net (openwrt) | Netlink socket for traffic control on OpenWrt. |
| `ipnetwork` | 0.20 | crates.io | net (openwrt) | IP address parsing for network adapter. |
| `netlink-packet-route` | 0.30 | crates.io | net (openwrt) | Netlink route packet construction. |
| `mockall` | 0.13 | crates.io | dev (net) | Mocking framework for wallet and adapter tests. |
| `mockito` | 1 | crates.io | dev (net) | HTTP mocking for mint interaction tests. |

### Key Dependency Differences

**CDK version**: basic-rust uses published 0.17, tollgate-rs uses git v0.16.0. The CDK crate had breaking changes between these versions. Wallet databases are not cross-compatible.

**secp256k1 version**: basic-rust uses 0.29, tollgate-rs uses 0.30. Both support BIP-340, but the API surface may differ slightly.

**Cashu crate feature**: basic-rust uses `cashu 0.17` with default features (wallet-oriented). tollgate-rs uses `cashu` from the CDK git repo with the `mint` feature enabled (because the protocol layer needs mint HTTP client capabilities for bootstrap token verification).

**TUI**: tollgate-rs includes ratatui for a monitoring TUI. basic-rust has no TUI.

**Netlink**: tollgate-rs has OpenWrt-specific netlink dependencies gated behind the `openwrt` feature. basic-rust uses ndsctl subprocess calls instead (no netlink dependency).

---

## Nostr Event Shape Comparison

Both projects implement the same four Nostr event kinds from the TollGate v1 specification. The event structures are identical; the difference is in how they are produced and signed.

### Kind 10021 (Discovery / Advertisement)

Both projects produce this event on `GET /`. The event carries the node's pricing and capabilities.

| Field | Value in both projects | Notes |
|-------|----------------------|-------|
| kind | 10021 | TollGate product advertisement |
| tags[0] | ["metric", "bytes"] or ["metric", "time"] | The metered resource type |
| tags[1] | ["step_size", "22020096"] | Bytes per pricing step (basic-rust) |
| tags[2] | ["price", "1"] | Price in sats per step (basic-rust) |
| tags[3] | ["unit", "sats"] | Currency unit |
| tags[4] | ["mint", "https://mint.example.com"] | Accepted mint URL |
| tags[5] | ["purchase_min_steps", "0"] | Minimum purchase |
| content | Empty or JSON | Varies by implementation |

In tollgate-module-basic-rust, this event is created and signed by `create_event()` in `nostr_event.rs`. In tollgate-rs, it is created by the v1-compat merchant module.

### Kind 21000 (Payment)

Client sends this event to `POST /` with `Content-Type: application/json`. It wraps a Cashu token.

| Field | Value | Notes |
|-------|-------|-------|
| kind | 21000 | Payment event |
| tags[0] | ["cashu", "<base64-token>"] | The Cashu token (NUT-00 format) |
| content | Empty | Token is in the tag |

Both projects extract the token from tag[0][1] for verification. tollgate-module-basic-rust's `pay.rs` handler does this extraction with multiple fallback strategies (direct text/plain, JSON with correct kind, JSON with wrong kind, etc.).

### Kind 1022 (Session Granted)

Provider returns this event on successful payment via `POST /` (HTTP 200).

| Field | Value | Notes |
|-------|-------|-------|
| kind | 1022 | Session granted |
| tags[0] | ["allotment", "<amount>"] | Granted data allowance |
| tags[1] | ["metric", "bytes"] | Unit of measurement |
| tags[2] | ["expiry", "<unix-timestamp>"] | When the session expires |

In tollgate-module-basic-rust, the `id` and `sig` fields are currently placeholders (Phase 7 task). In tollgate-rs's v1-compat, the event is signed properly via the nostr crate.

### Kind 21023 (Payment Rejected)

Provider returns this event on failed payment via `POST /` (HTTP 400).

| Field | Value | Notes |
|-------|-------|-------|
| kind | 21023 | Payment rejected |
| content | Error message | Human-readable reason for rejection |

---

## Session Lifecycle Comparison

### tollgate-module-basic-rust Session Flow

```
1. Client connects to captive portal
2. Client sends GET / → receives kind 10021 (pricing)
3. Client obtains Cashu token from their wallet
4. Client sends POST / with token (text/plain or kind 21000)
5. Server parses token, extracts Y-values
6. Server checks: mint in accepted_mints? → if not, reject with kind 21023
7. Server calls NUT-07 checkstate (if mint reachable)
8. Server calls Wallet::receive(token) → CDK contacts mint for swap
9. On success: allotment = received_amount * step_size
10. Server creates CustomerSession { mac, allotment, used=0, metric, expiry }
11. Server returns kind 1022 (session granted)
12. Server calls ndsctl auth <mac> (via Nodogsplash, not in the Rust binary)
13. Periodic: server calls ndsctl state <mac> → parse used bytes
14. If used >= allotment: server calls ndsctl deauth <mac>, removes session
15. If session expired: cleanup_expired() removes it
```

Key properties:
- One payment per session. No mid-session top-up.
- Allotment is computed from the token amount. No negotiation.
- Session keyed by client MAC address.
- No bidirectional reporting. Provider's ndsctl output is authoritative.
- If the process restarts, all sessions are lost.

### tollgate-rs Session Flow (Native Protocol)

```
1. Both peers complete transport-level authentication (FIPS, WireGuard, etc.)
2. A → B: Announce (protocol version, pubkey, capabilities)
3. B → A: Announce
4. A → B: PriceSheet (products, per-mint prices, interval range)
5. B → A: PriceSheet
6. B → A: Accept (chosen product, mint option, funding proofs, interval range)
7. A → B: Accept (chosen product, mint option, funding proofs, interval range)
8. B → A: ChannelReady (B→A channel ID, direction)
9. A → B: ChannelReady (A→B channel ID, direction)
   [Both channels active. Metering baseline established.]
10. At each 5-second interval:
    a. A → B: MeteringReport (cumulative delivered, received, elapsed_ms)
    b. B → A: MeteringReport (cumulative delivered, received, elapsed_ms)
    c. Both compute: cost_A_owes_B and cost_B_owes_A
    d. Net debtor signs BalanceUpdate on their outgoing channel
    e. Net creditor sends BalanceAck
11. If A's outgoing channel at 80%: A sends RolloverInit + new funding
12. B verifies and sends RolloverReady
13. Old channel drains to 100%, new channel continues
14. On close: ChannelClose → CloseAck → settle with mint
15. On disconnect: Disconnect message (or TCP close)
```

Key properties:
- Payment is continuous, not upfront. One signature per 5-second interval.
- Two channels per peer pair enable bidirectional payment with netting.
- Pricing is negotiated (take-it-or-leave-it with per-mint options).
- Both sides report metering. Discrepancies reconciled (higher value used).
- Channel capacity grows with relationship trust.
- Offline: balance updates continue without mint. Channels survive outages.
- If a node restarts: bounded exposure (one channel's capacity).

---

## Security Model Comparison

### Trust Assumptions

| | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Mint trust | Operator must trust accepted mints to not refuse valid tokens. The config lists accepted mint URLs. | Same, but the protocol adds multi-mint resilience: operators should maintain overlapping channels across 2-3 mints so a single mint outage does not block all operations. |
| Peer trust | Not applicable (single-hop, client-server model). The client trusts the provider to grant the purchased allotment. | Both peers know each other's payment identities (they share Spilman channels). Privacy comes from Cashu's blind signatures: the mint cannot link payments to identities. |
| Transport trust | HTTP on localhost (127.0.0.1:2121). Nodogsplash handles client-facing HTTP. | Peers authenticated out-of-band before TollGate messages flow (FIPS Noise IK, WireGuard, etc.). TollGate does not handle transport authentication. |
| Client identity | Identified by MAC address (from captive portal) or IP address (from X-Forwarded-For / X-Real-IP headers). | Identified by compressed secp256k1 public key (33 bytes) in the Announce message. |

### Threat Mitigation

| Threat | tollgate-module-basic-rust | tollgate-rs |
|---|---|---|
| Freeloading (use without paying) | Token must be verified before session is granted. NUT-07 checkstate confirms proofs are unspent. | Access control gates delivery per peer based on payment status. Unpaid peers can only send data to the local node (for payment negotiation). |
| Overpayment (metering disagreement) | Not addressed. Provider's ndsctl output is authoritative. | Transit loss tolerance (default 5%). Reconciliation uses higher of two reported values. Persistent over-tolerance triggers channel close. |
| Rugpull (provider takes payment, cuts service) | Client already paid. Exposure is the full payment amount upfront. | Maximum exposure is one metering interval (5 seconds) of delivery. Short intervals limit loss. |
| Rugpull (client stops paying, keeps service) | Access control via Nodogsplash (ndsctl deauth) when session expires or usage is exhausted. | Access control in core: delivery stops when payment stops. Suspended state blocks transit resources. |
| Mint outage | All new payments fail. Existing sessions continue until they expire or exhaust. | Balance updates continue (no mint needed). Channel funding/rollover/settlement queued. If a channel exhausts during outage and rollover is blocked, delivery pauses. Safety margin before channel expiry triggers emergency rollover. |
| Wallet bricking | Fixed by CDK saga pattern. The entire reason for this project's existence. | Fixed by design: no keyset counter persisted separately from proofs. Spilman channel state is in-memory (no persistence to corrupt). |
| Double-spend | Client sends a token that was already spent. NUT-07 checkstate catches this. | Bootstrap tokens verified with mint before service is granted. If mint unreachable, token is rejected outright (no trust-before-verification). |

---

## v1-Compat Layer Deep Dive

The v1-compat feature in tollgate-rs is an adapter that translates Go v1 API requests into the tollgate-rs driver model. It lives entirely in `crates/tollgate-net/src/v1_compat/` and consists of 15 modules:

| Module | Lines (approx.) | Purpose |
|--------|------|---------|
| `mod.rs` | 47 | Module declarations and `build_v1_router()` entry point |
| `handlers.rs` | 824 | axum route handlers mirroring the Go HTTP surface |
| `pricing.rs` | 337 | V1 PricingOption struct, `select_cheapest_compatible()`, budget validation |
| `merchant.rs` | ~200 | V1ServerConfig, merchant identity, allotment calculation |
| `wallet.rs` | ~150 | CDK wallet operations for v1 token receive/balance |
| `nostr.rs` | ~100 | Nostr event creation and signing for v1 API |
| `adapter.rs` | ~200 | Translates between v1 concepts and the driver model |
| `client.rs` | ~100 | V1 client mode (purchasing from upstream) |
| `session_manager.rs` | ~200 | V1-style session tracking (MAC-keyed, like basic-rust) |
| `usage_tracker.rs` | ~150 | V1-style usage polling |
| `mac_resolver.rs` | ~200 | MAC address resolution for v1 client identification |
| `ln_quotes.rs` | ~100 | Lightning invoice quotes (stub, matching basic-rust's stubs) |
| `crowsnest.rs` | ~150 | Upstream discovery (Nostr-based, from Go Chandler) |
| `recovery.rs` | ~100 | Wallet recovery and migration helpers |
| `http_client.rs` | ~150 | HTTP client for upstream communication |

The handler at `handlers.rs` implements the same five routes as tollgate-module-basic-rust:

- `GET /`: Returns a Nostr kind 10021 event with pricing tags, using the merchant module for identity and the adapter for pricing.
- `POST /`: Extracts token amount, computes allotment via `merchant::calculate_allotment()`, calls the driver for session creation, returns kind 1022 or kind 21023.
- `GET /whoami`: Returns the client's MAC address (resolved via `mac_resolver`).
- `GET /usage`: Returns `used/total` for the client's session (tracked by `usage_tracker`).
- `GET /balance`: Returns the wallet balance JSON.

The `pricing.rs` module implements `PricingOption` (line 11-22) which mirrors the Go Chandler's pricing concept: `asset_type`, `price_per_step`, `unit`, `mint_url`, `min_steps`. The `select_cheapest_compatible()` function (line 48) finds the cheapest option that matches one of the node's accepted mints and satisfies budget constraints (max price per unit, sufficient funds).

The adapter module bridges the v1 API surface (which expects a simple "receive token, create session" flow) with the tollgate-rs driver (which expects a session-based peer relationship with Announce/PriceSheet/Accept/ChannelReady lifecycle). The v1-compat path shortcuts most of this: it receives the token via CDK (like basic-rust does), creates a session in the v1 session manager, and returns the v1-style response.

---

## Future Directions

### tollgate-module-basic-rust

The immediate roadmap is completing Phase 7: test parity on physical OpenWrt hardware. This means:

1. Deploying to real GL-MT6000/MT3000 routers.
2. Running end-to-end payment flows against a live Cashu mint.
3. Validating ndsctl integration (poll_usage subprocess calls).
4. Migrating production wallets under live network conditions.
5. Completing the kind 1022 Nostr event signing (currently uses placeholder id and sig).
6. Wiring `/whoami` to actual ARP/remote_addr lookup.
7. Deciding whether to implement `/ln-invoice` endpoints or leave them as stubs.

Beyond Phase 7, the project could consider: adding Spilman channels (a major undertaking that would break v1 compatibility), supporting per-peer pricing, or adding dynamic pricing. However, any of these would move it away from being a drop-in replacement. The right approach for those features is tollgate-rs.

### tollgate-rs

The ROADMAP tracks four phases:

1. **Phase 1: FIPS Integration** (in progress). Implementing FIPS control socket features (per-peer forwarding, bloom filters, traffic counters, lifecycle events). Docker integration tests. QEMU-based multi-node test topologies. Physical router lab validation.

2. **Phase 2: FIPS Internet Exit / Tunneling**. GRE tunnel PoC, TUN/TAP interface for FIPS-to-internet bridging. TollGate pricing on tunnel interfaces. Docker/QEMU/physical router testing.

3. **Phase 3: Full FIPS-Only Network**. Remove IP entirely from the internal mesh. Multiple exit nodes with reputation-based pricing. DNS resolution through FIPS. Per-FIPS-instance pricing for heterogeneous peers (LoRa vs fiber).

4. **Phase 4: Native Mobile Integration**. Android app via `cargo-ndk` (not Tauri/WebView). Kotlin/Jetpack Compose UI via UniFFI. FIPS-based push notifications (no FCM/APNS, direct Nostr events to foreground service). WiFi Direct for phone-to-router connections.

Additional future work mentioned across the design docs: microFIPS (ESP32 as a build target in the main FIPS repo), TTL/ping proximity proof for pairing and DoS protection, payment-aware routing (well-paying peers get favorable routing), 802.11s mesh backbone for router-to-router links, automated inter-mint fund movement for multi-mint resilience, and proof-of-delivery mechanisms for stronger transit loss guarantees.

---

## Topology Model Comparison

### tollgate-module-basic-rust: Tree Topology

The v1 model (inherited from the Go binary) is a strict tree. Every node has exactly one upstream provider (its "parent") and can serve multiple downstream clients (its "children"). Payment flows upward only: clients pay the node, the node pays its parent.

```
Internet
  |
  Gateway (tollgate-module-basic-go/rust)
  |
  Relay (tollgate-module-basic-go/rust)
  / \
Client  Client
```

In this model:
- Each node has one pricing relationship (with its parent).
- The node's margin is the spread between what it charges downstream and what it pays upstream (configured via `margin`, `upstream_session_manager`, etc.).
- Clients do not pay each other. A client only interacts with its immediate access point.
- Discovery of upstream nodes uses Nostr (the "crowsnest" concept) or manual configuration (`upstream_detector` with probe/retry logic, `upstream_wifi` for WiFi scanning).
- No mesh routing. Traffic flows up the tree to the gateway and out to the internet.

The `upstream_detector` config in `schema.rs` implements this tree discovery: it probes for upstream nodes, filters by interface and signature, and manages a list of known upstreams. The `upstream_session_manager` handles the economic relationship with the upstream (max price per millisecond/byte, trust policies, session increments).

### tollgate-rs: Mesh Topology

The v2 model is a mesh. Every peer is an independent economic actor that can buy from and sell to any other peer. There is no parent-child relationship; every connection is a peer-to-peer commercial relationship.

```
  Node A ----[10 sat/MB]----> Node B ----[3 sat/MB]----> Gateway ----> Internet
    ^                          |
    |                          |
    +--------[5 sat/MB]---------+
    
  Relay margin: 10 - 5 = 5 sat/MB profit (selling to B while buying from A)
  Client doesn't know Gateway exists. Gateway doesn't know Client exists.
```

In this model:
- Each peer has N pricing relationships (one per connected peer).
- The operator's margin is the spread between what they charge for delivery and what they pay their peers, on a per-peer basis.
- Every hop is independently priced. A relay can charge Client A 10 sat/MB while paying Gateway 3 sat/MB, earning 7 sat/MB on relayed traffic from A.
- Clients don't need path knowledge. Operators earn the margin between buy-price and sell-price.
- FIPS provides the mesh routing layer. TollGate provides the payment layer. They are orthogonal.
- A peer can simultaneously buy connectivity from multiple upstreams and sell to multiple downstreams, optimizing for price and quality.

This fundamental difference in topology drives many of the other architectural differences: per-peer pricing (each hop has its own terms), bidirectional payment (both sides may deliver resources), and hop-by-hop settlement (no end-to-end billing relationship).

---

## Token Verification Flow Comparison

### tollgate-module-basic-rust

The verification flow is implemented in `wallet/verify.rs` and called from `http/routes/pay.rs` before the CDK receive.

```
1. POST / receives body (text/plain or application/json)
2. If application/json: parse as Nostr event, check kind == 21000, extract token from tags[0][1]
3. If text/plain: use the raw body as the token string
4. Parse the Cashu token (cashu 0.17 Token::parse)
5. Extract Y-values from all proofs in the token
6. Check: is the token's mint URL in the accepted_mints list?
   - If not: reject with kind 21023 + HTTP 400
7. Optionally: call NUT-07 checkstate (POST /v1/checkstate to the mint with Y-values)
   - Verifies each proof is UNSPENT
   - If mint unreachable: the behavior depends on implementation (could accept or reject)
8. Call Wallet::receive(token) -> CDK contacts mint for swap
   - The mint verifies proofs again (CDK always checks with the mint)
   - On success: proofs are stored in wallet.sqlite, balance increases
   - On failure: HTTP 400 to client
```

The verify module has 6 unit tests covering: valid token parsing, Y-value extraction, mint filtering (reject token from unknown mint), invalid tokens (wrong format), and milli-unit scaling.

### tollgate-rs (Bootstrap Path)

The verification flow for bootstrap tokens is defined in `tollgate-bootstrap.md` and implemented in the testing harness.

```
1. Client sends BootstrapToken message (type 0x07) with raw Cashu token bytes
2. Provider extracts the token
3. Provider calls wallet.verify_funding() or wallet.receive_token() for bootstrap
4. Provider contacts the mint for NUT-07 checkstate
   - POST /v1/checkstate with the proof Y-values
   - Mint returns state for each proof (UNSPENT or SPENT)
   - If any proof is SPENT: reject with BootstrapAck (status=1, reason="already spent")
   - If mint unreachable: reject with BootstrapAck (status=1, reason="mint unreachable")
   - The design explicitly states: "there is no pending / trust-on-faith mode"
5. On success: BootstrapAck (status=0), session state transitions to Active
   - Provider grants metered access sufficient to reach a mint (bootstrap allotment)
6. Client can now open Spilman channels (if it has SPILMAN capability)
```

The fake-mint.py integration test stub at `testing/bootstrap/fake-mint.py` implements only step 4: it returns `{"states": [{"Y": y, "state": "UNSPENT"}]}` for every Y-value. This exercises the provider's real verification code path without a full Cashu mint.

---

## Binary Size and Build Comparison

### Build Profiles

**tollgate-module-basic-rust** has an aggressively optimized release profile at `Cargo.toml` lines 60-66:

```toml
[profile.release]
panic = "abort"      # Removes unwinding tables (~100-200 KB)
strip = true          # Removes debug symbols
opt-level = "z"       # Optimize for binary size (not speed)
lto = true            # Link-time optimization across all crates
codegen-units = 1     # Single codegen unit = maximum optimization opportunity
```

This profile is designed for resource-constrained OpenWrt routers. The combination of `panic = "abort"` (eliminates unwinding tables and landing pads), `opt-level = "z"` (optimizes for size over speed), and `lto = true` with `codegen-units = 1` (maximum cross-crate optimization at the cost of build time) produces the smallest possible binary. The tradeoff is slower compilation and potentially slower runtime performance, but for a captive-portal payment node that handles one transaction per client session, this is acceptable.

**tollgate-rs** does not have a custom release profile in its workspace `Cargo.toml`. The default release profile applies: `opt-level = 3`, no LTO, no strip, `panic = "unwind"`. The binary would be significantly larger with default settings. For OpenWrt deployment, tollgate-rs would need an equivalent release profile, but this has not been done yet.

### Dependency Weight

**tollgate-module-basic-rust** has a relatively lean dependency tree. The heaviest dependencies are CDK/cdk-sqlite/cashu (the Cashu wallet stack) and axum (the HTTP framework). The `reqwest` client uses `rustls-tls` instead of OpenSSL, which is critical for musl static linking. The binary has no TUI, no CLI framework (clap), no YAML parser, no netlink libraries.

**tollgate-rs** pulls in significantly more dependencies for `tollgate-net`: ratatui (TUI), clap (CLI), serde_yaml (YAML config), nostr (v1-compat), dirs (XDG paths), plus the netlink stack for OpenWrt (rtnetlink, ipnetwork, netlink-packet-route, netlink-packet-core). Many of these are conditional on feature flags, but a full-features build (v1-compat + spilman + openwrt) would be substantially larger. The ESP32 build avoids all of these by using only `tollgate-core` and `tollgate-protocol`.

### Cross-Compilation

**tollgate-module-basic-rust** has a complete cross-compilation setup. The `.cargo/config.toml` configures zig cc wrappers for ARM musl targets. Three targets are documented in the README: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, armv7-unknown-linux-musleabihf. The Makefile uses a prebuilt-binary pattern where CI builds the binary and stages it, and the OpenWrt SDK only packages the artifact. The init script references the binary at `/usr/bin/tollgate-module-basic-rust`.

**tollgate-rs** does not have documented cross-compilation instructions. The `openwrt` feature flag exists for OS-specific code paths (netlink, UCI), but the build process for producing OpenWrt binaries (IPK packaging, cross-toolchain setup, prebuilt-binary staging) is not documented or implemented. The protocol and core crates compile for `no_std` targets (which is how ESP32 support would work), but the net binary requires standard library features and has not been tested on musl targets.

---

## Community and Ecosystem Position

Both projects exist within the broader OpenTollGate ecosystem, but they serve different roles and have different organizational homes.

**The Go predecessor** (`tollgate-module-basic-go` under the OpenTollGate organization) is the production deployment today. It runs on OpenWrt routers selling WiFi access via Nodogsplash captive portals. It has the swap-counter race bug that motivated the Rust rewrite.

**tollgate-module-basic-rust** (`felixfelix-bot/tollgate-module-basic-rust`) is the immediate fix. It addresses the wallet-bricking issue by switching to CDK while preserving the entire v1 API surface. It is the path of least resistance for operators who need a working system today. Its repository is under a personal GitHub account, not the OpenTollGate organization, suggesting it may be a temporary effort until tollgate-rs matures.

**tollgate-rs** (`OpenTollGate/tollgate-rs`) is the strategic direction. It lives under the OpenTollGate organization and represents the long-term vision for the protocol. The clean-sheet design with `no_std` core, resource-agnostic pricing, and mesh-first architecture positions it as the foundation for all future TollGate deployments, not just routers. The reference directory in the repo includes the Go predecessor source, FIPS source, and the Spilman channel architecture, indicating it is meant to subsume and extend the prior work.

**physical-router-test-automation (PRTA)** is the shared test infrastructure. It tests both Rust backends but treats them as distinct (`TOLLGATE_BACKEND=rust` for tollgate-rs, `TOLLGATE_BACKEND=rust-basic` for tollgate-module-basic-rust). This shared test infrastructure provides a common quality bar and ensures that neither project regresses on the v1 API contract. The test suite (`test_rust_v1_api.py`) covers the standard v1 API: discovery (GET / returns valid kind 10021), payment (POST / with a valid token returns kind 1022 with correct allotment), usage tracking (GET /usage reflects session state), and balance reporting (GET /balance returns wallet state).

**Cashu ecosystem alignment**: tollgate-module-basic-rust uses published CDK 0.17 from crates.io, aligning with the current Cashu ecosystem version. tollgate-rs uses an unpublished CDK v0.16.0 from git plus an unpublished `cdk-spilman` fork from a separate repository. As the Cashu ecosystem evolves and Spilman channels are standardized (potentially as a NUT), tollgate-rs will need to update its CDK dependency to keep pace. The v0.16 vs v0.17 gap is a technical debt that will need to be addressed before tollgate-rs can reach production maturity. The `cashu` crate usage also differs: basic-rust uses default features (wallet-oriented), while tollgate-rs enables the `mint` feature (for protocol-level mint client operations).

---

## Appendix: CBOR Protocol Message Reference (tollgate-rs)

This appendix lists the 15 CBOR message types defined in `tollgate-protocol.md`. These form the tollgate-rs peer-to-peer protocol. tollgate-module-basic-rust implements none of these -- it uses the v1 HTTP/Nostr API instead. The table below is a quick-reference summary; full field-level CBOR schemas are in the protocol specification.

| Type | Name | Direction | Purpose |
|------|------|-----------|---------|
| 0x00 | Announce | Bidirectional | Peer identification: protocol version, compressed secp256k1 pubkey, resource unit, capability bitfield |
| 0x01 | PriceSheet | Bidirectional | Product catalog with per-mint pricing options and metering interval ranges |
| 0x02 | Accept | Bidirectional | Accept a specific product and mint option by hashed ID; includes Spilman funding proofs |
| 0x03 | ChannelReady | Bidirectional | Confirm Spilman channel is funded and active; carries channel ID and direction |
| 0x04 | MeteringReport | Bidirectional | Unsigned cumulative resource counters (bytes sent/received, time elapsed) |
| 0x05 | BalanceUpdate | Debtor to creditor | Signed Spilman balance update for net amount owed this interval |
| 0x06 | BalanceAck | Creditor to debtor | Acknowledge acceptance of a BalanceUpdate |
| 0x07 | BootstrapToken | Peer to provider | Raw Cashu token for pre-channel bootstrap funding |
| 0x08 | BootstrapAck | Provider to peer | Accept or reject a bootstrap token with reason code |
| 0x09 | RolloverInit | Sender to receiver | Initiate a new channel before the current one exhausts |
| 0x0A | RolloverReady | Receiver to sender | Confirm new channel is funded and ready |
| 0x0B | ChannelClose | Either direction | Request cooperative channel close |
| 0x0C | CloseAck | Either direction | Acknowledge channel close request |
| 0x0D | Reject | Either direction | Reject any proposal with a reason string |
| 0x0E | Disconnect | Either direction | Orderly session teardown |

The protocol is transport-agnostic. Messages are CBOR-encoded (RFC 8949) and can travel over HTTP polling (`POST /tollgate/v1/exchange` with length-prefixed CBOR bodies), WebSocket, or any bidirectional channel. The default port is 4747. Field keys are small integers (not strings) for wire compactness. There is no handshake -- peers are authenticated out-of-band by FIPS Noise IK, WireGuard, or similar mechanisms.

The capability bitfield in Announce (field 4) currently defines one bit: `0x01` for SPILMAN. A peer without this bit is bootstrap-only and cannot fund Spilman channels; it pays via BootstrapToken (0x07) for each session. This maps to the capability negotiation described in the Protocol Design dimension above.

Key structural points that differentiate this from the v1 HTTP API used by tollgate-module-basic-rust: (a) every message is CBOR, not JSON; (b) the protocol is peer-to-peer and bidirectional, not client-server; (c) metering is push-based (MeteringReport every 5 seconds), not pull-based (GET /usage); (d) payment channels (Spilman) replace per-session token swaps; (e) there is no Nostr event model -- identity is the secp256k1 pubkey in Announce, not a Nostr keypair with kind-tagged events.