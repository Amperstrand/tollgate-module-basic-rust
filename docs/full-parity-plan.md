# Comprehensive Implementation Plan: Full Go Parity + NDS Replacement

> **Goal**: Make tollgate-module-basic-rust a complete, production-ready
> replacement for tollgate-module-basic-go with NO nodogsplash dependency.
>
> **Estimated total effort**: 10-14 weeks (single experienced Rust developer)
>
> **Current state**: HTTP API surface parity achieved (7 fixes committed).
> Missing: operational features + captive portal functionality.

## Current State Assessment

### Already done (committed)
- [x] HTTP endpoint shapes match Go (balance, discovery, payment, usage, whoami)
- [x] Nostr event signing (kinds 10021, 1022, 21023)
- [x] Correct allotment formula `(received / price_per_step) * step_size`
- [x] Valve gate control (`src/valve.rs` — ndsctl auth/deauth with retry)
- [x] Session disk persistence (`sessions.json`)
- [x] MAC resolution (`src/mac_resolver.rs` — dhcp.leases + arp)
- [x] Config file compatibility (config.json, identities.json, install.json)
- [x] Bind on all interfaces (`0.0.0.0:2121`)
- [x] CDK wallet integration (receive, send, balance, melt)
- [x] Comparison report + parity test suite (13 tests)

### Missing (30 items from 155-property audit)

## Part A: Go Feature Parity (~5-6 weeks)

### Phase 1: Background Monitoring + Real Usage Tracking (1 week)

**Problem**: Sessions never expire from actual usage. The `session.used` field
is static — nothing updates it. Go has a 2-second background loop that queries
ndsctl for real per-client data consumption and closes gates when allotment is
reached.

**Implementation**:

```
src/monitor.rs (new)
├── struct Monitor { interval, sessions, valve, ndsctl_mutex }
├── async fn start() — spawns tokio task
├── async fn poll_all_sessions() — iterate active sessions
├── async fn check_session_expiry(mac) — bytes vs time logic
└── async fn revoke_expired(mac) — close gate + delete session
```

**Algorithm** (from Go `merchant.go:StartDataUsageMonitoring`):
```
every 2 seconds:
  for each active session:
    if metric == "bytes":
      usage = valve.get_data_usage_since_baseline(mac)
      if usage >= session.allotment:
        valve.close_gate(mac)
        sessions.revoke(mac)
    if metric == "milliseconds":
      elapsed = now - session.granted_at
      if elapsed * 1000 >= session.allotment:
        valve.close_gate(mac)
        sessions.revoke(mac)
```

**Data baseline tracking** (from Go `valve/customer_data_tracker.go`):
- `SetDataBaseline(mac)`: Snapshot current ndsctl counters when session starts
- `GetDataUsageSinceBaseline(mac)`: Current counters - baseline = usage delta
- Store baselines in `HashMap<String, DataBaseline>`

**Wire into main.rs**: Start monitor after wallet initialization.

**Files**: `src/monitor.rs` (new), `src/main.rs` (spawn task), `src/lib.rs` (register)

**Effort**: M (1 week)

---

### Phase 2: Session Lifecycle Completion (4 days)

**Problem**: Rust `create_session` always overwrites. Go `AddAllotment`
extends existing sessions (adds to allotment, resets start time). Also, Rust
doesn't set data baselines when creating sessions.

**Implementation**:

In `src/session/mod.rs`:
```rust
pub fn add_allotment(&mut self, mac: &str, metric: &str, amount: u64, duration_secs: u64) {
    match self.sessions.get_mut(mac) {
        Some(session) => {
            session.allotment += amount;
            session.granted_at = now();
            session.used = 0;
        }
        None => {
            self.create_session(mac, amount, metric, duration_secs);
        }
    }
}
```

In `src/http/routes/pay.rs`: Change `create_session` → `add_allotment`.

**Effort**: S (4 days including testing)

---

### Phase 3: Lightning Invoice Support (1.5 weeks)

**Problem**: `/ln-invoice` endpoints return hardcoded stubs. Go has full
implementation: mint quote → background monitor → auto-session-grant.

**Implementation**:

```
src/lightning.rs (new)
├── struct LightningQuote { quote_id, invoice, mint_url, amount, expiry, state }
├── struct LightningQuoteRecord { bolt11, mac, mint_url, amount, expiry, ... }
├── async fn request_invoice(mac, mint_url, amount) -> LightningQuote
├── async fn monitor_quote(quote_id) — background task with backoff
├── async fn grant_access_if_paid(quote_id) — mint tokens + create session
├── fn persist_quotes() / fn load_quotes() — survive restarts
└── async fn get_quote_status(quote_id, mac) -> LightningQuoteStatus
```

**Flow** (from Go `merchant/lightning.go`):
1. POST /ln-invoice → CDK `request_mint_quote` → store quote record → start monitor
2. Monitor polls CDK `check_mint_quote` every 5s (backoff to 30s)
3. When paid: CDK `mint_quote_tokens` → calculate allotment → grant session
4. GET /ln-invoice?quote=X → return quote state + access_granted flag

**Quote persistence**: Save to `/etc/tollgate/lightning_quotes.json`, reload on startup.

**Wire into**: `src/http/routes/ln_invoice.rs` (replace stubs)

**Effort**: L (1.5 weeks)

---

### Phase 4: Payout Routine (1 week)

**Problem**: No profit sharing. Go has multi-phase payout: probe recipients →
pay owner first → pay maintainers, via LNURL + Cashu melt.

**Implementation**:

```
src/payout.rs (new)
├── struct Recipient { identity, amount, lightning, is_owner }
├── async fn start_payout_routine() — one tokio task per mint
├── async fn process_payout(mint_config) — 3-phase algorithm
├── async fn payout_share(mint, amount, ln_address) — melt to lightning
├── async fn fetch_invoice_with_retry(ln_address, amount) — LNURL
└── fn build_recipients(config, identities) -> Vec<Recipient>
```

**Algorithm** (from Go `merchant.go:processPayout`):
```
every payout_interval_seconds (default 60s):
  balance = wallet.get_balance_by_mint(mint_url)
  if balance < min_payout_amount: skip
  if balance <= min_balance: skip

  aimed = balance - min_balance
  recipients = config.profit_share.map(factor => aimed * factor)

  Phase 1: Probe all recipients for LNURL reachability
  Phase 2: Pay owner first (abort all if owner unreachable)
  Phase 3: Pay remaining reachable maintainers
```

**Effort**: L (1 week — LNURL integration is the complexity)

---

### Phase 5: Degraded Mode + Mint Health (1 week)

**Problem**: Binary crashes or hangs when all mints are unreachable. Go starts
in degraded mode and auto-upgrades when mints recover.

**Implementation**:

```
src/mint_health.rs (new)
├── struct MintHealthTracker { reachable_mints, consecutive_successes, ... }
├── async fn run_initial_probe() — check all mints on startup
├── async fn run_proactive_checks() — every 5 minutes
├── async fn run_aggressive_retry() — every 15s for first 5 minutes
├── async fn probe_mint(url) -> bool — GET /v1/info
├── fn is_reachable(url) -> bool
└── fn on_reachable_set_changed(callback)

src/degraded.rs (new)
├── struct DegradedMerchant — implements payment/advertisement with errors
├── fn upgrade_to_full() — swap merchant instance
└── fn load_offline_wallet() — try loading existing wallet.db
```

**State machine**:
```
Startup → probe all mints
  ├── ≥1 reachable → FullMerchant (normal operation)
  └── 0 reachable  → DegradedMerchant
                      ├── all payments return "service-unavailable"
                      ├── advertisement returns "initializing" notice
                      └── background aggressive retry (15s intervals)
                           └── first mint reachable → upgrade to FullMerchant
```

**Effort**: M (1 week)

---

### Phase 6: Config Migration + Validation (2 days)

**Problem**: No config version migration, no backup on parse failure, incomplete
profit_share validation.

**Implementation** in `src/config/mod.rs`:
- `migrate_config(old_version, new_version)` — populate missing fields
- `backup_config(path)` — copy to `/etc/tollgate/config_backups/`
- `validate_profit_share()` — check each factor 0-1 + sum to 1.0

**Effort**: S (2 days)

---

### Phase 7: CLI Parity (1 week)

**Problem**: Rust CLI uses plain text protocol with 5 commands. Go uses JSON
protocol with 12+ commands.

**Decision**: Rather than matching Go's JSON CLI protocol (which would break
existing Rust CLI tests), implement the MISSING commands in the existing
plain-text protocol:

**New commands to add** (in `src/cli/mod.rs`):
- `wallet drain [mint_url]` — extract all mints to Cashu tokens
- `wallet fund <token>` — receive token into wallet
- `health` — return service health JSON
- `config get [key]` — read config value
- `config set <key> <value>` — update config value

**NOT implementing** (require wireless hardware, not applicable to Rust binary):
- `network` — WiFi configuration (Go-only, requires hostapd)
- `upstream scan/connect/list/remove` — WiFi STA management (Go-only)

**Effort**: M (1 week)

---

## Part B: Embedded Portal — Replace NDS (~5-6 weeks)

### Phase 8: CaptivePortal Trait Extraction (3 days)

Extract a `CaptivePortal` trait that abstracts the portal operations.
Both `NdsPortal` (current, calls ndsctl) and `EmbeddedPortal` (new, uses
nftables) implement this trait.

```
src/portal/mod.rs
├── #[async_trait] trait CaptivePortal {
│     async fn grant_access(mac, ip)
│     async fn revoke_access(mac)
│     async fn poll_usage(mac) -> (used, total)
│     async fn is_authenticated(mac) -> bool
│     async fn install()   // setup firewall rules
│     async fn teardown()  // cleanup firewall rules
│   }
├── #[cfg(not(feature = "embedded-portal"))]
│   pub use nds::NdsPortal as PortalBackend;
└── #[cfg(feature = "embedded-portal")]
    pub use embedded::EmbeddedPortal as PortalBackend;
```

Wire `Arc<dyn CaptivePortal>` into `AppState`. All existing code calls through
the trait. Zero behavior change.

**Effort**: S (3 days)

---

### Phase 9: nftables Rule Management (1 week)

Create the nftables table/chains/sets that form the captive portal ruleset.

```
src/portal/nft_manager.rs
├── struct NftManager { table_name: "tollgate" }
├── fn install() — create table + chains + sets (idempotent)
├── fn teardown() — remove table (RAII guard)
├── fn add_client(ip) — add to authenticated_v4 set
├── fn remove_client(ip) — remove from set
├── fn create_counter(ip) — named counter per client
├── fn delete_counter(ip)
├── fn poll_counter(ip) -> (packets, bytes)
└── fn list_counters() -> HashMap<IpAddr, (u64, u64)>
```

**Ruleset** (nftables `inet` family for IPv4+IPv6):
```
table inet tollgate {
  set authenticated_v4 { type ipv4_addr }
  set authenticated_v6 { type ipv6_addr }

  chain prerouting {
    type nat hook prerouting priority dstnat
    tcp dport 80 ip saddr != @authenticated_v4 redirect to :80
    tcp dport 80 ip6 saddr != @authenticated_v6 redirect to :80
  }

  chain forward {
    type filter hook forward priority 0
    ip saddr @authenticated_v4 accept
    ip6 saddr @authenticated_v6 accept
    udp dport 53 accept
    tcp dport 53 accept
    tcp dport 80 accept
    drop
  }
}
```

**Dependencies**: `nftables = { version = "0.6", optional = true }`

**Safety**: Watchdog task removes tollgate table if HTTP unresponsive >60s.
Emergency-clear ruleset at `/etc/tollgate/emergency-clear.nft`.

**Effort**: M (1 week)

---

### Phase 10: Client Auth/Revoke via nftables (3 days)

Wire `grant_access`/`revoke_access` to nftables set membership.

```
impl CaptivePortal for EmbeddedPortal {
    async fn grant_access(mac, ip) {
        self.nft.add_client(ip);
        self.nft.create_counter(ip);
        self.set_data_baseline(mac);
    }
    async fn revoke_access(mac) {
        self.nft.remove_client(ip);
        self.nft.delete_counter(ip);
        // No ndsctl deauth needed — nftables set removal is instant
    }
}
```

Requires MAC → IP resolution (already in `mac_resolver.rs`). For IPv6, extend
with NDP lookup (`ip -6 neigh show`).

**Effort**: S (3 days)

---

### Phase 11: Per-Client Bandwidth Counters (1 week)

Replace ndsctl-based usage tracking with nftables named counters.

```
impl CaptivePortal for EmbeddedPortal {
    async fn poll_usage(mac) -> (u64, u64) {
        let ip = resolve_mac_to_ip(mac);
        let (packets, bytes) = self.nft.poll_counter(ip);
        (bytes, session.allotment)
    }
}
```

**Batch polling**: Single `nft -j list counters` returns ALL counters. Parse
JSON via `nftables::helper::get_current_ruleset()`.

**Counter overflow**: nftables uses u64 internally; the Rust crate uses u32.
For high traffic, parse raw JSON output and extract u64 values manually if
needed.

**Effort**: M (1 week)

---

### Phase 12: Port-80 Redirect Server (3 days)

New axum listener on port 80 for unauthenticated client redirect.

```
src/portal/redirect_server.rs
├── async fn handle_port80(State(state), ConnectInfo(addr), headers) {
│     mac = resolve_mac(addr.ip())
│     if portal.is_authenticated(mac):
│       return StatusCode::NO_CONTENT  // passthrough
│     else:
│       return Redirect::to("http://<gateway>:2121/")
│   }
```

**Splash page**: Serve static HTML from `openwrt/files/tollgate-captive-portal-site/`
for clients that don't auto-redirect.

**Effort**: S (3 days)

---

### Phase 13: IPv6 Dual-Stack (2 weeks)

**Challenges**:
- SLAAC privacy addresses (client IPv6 changes frequently)
- Multiple IPv6 addresses per client
- No ARP — use NDP (`ip -6 neigh show`)
- nftables `inet` family handles both IPv4+IPv6 in one table

**Implementation**:
- Extend `mac_resolver.rs`: `resolve_mac_to_ipv6_addrs(mac) -> Vec<Ipv6Addr>`
- `grant_access` adds ALL client IPs (v4 + all v6) to nftables sets
- Periodic refresh of IPv6 address set (addresses change)
- Test with real devices (iOS, Android, macOS, Windows)

**Effort**: L (2 weeks)

---

### Phase 14: OpenWrt Integration + Hardware Testing (1-2 weeks)

- Update `openwrt/Makefile` to build with `--features embedded-portal`
- Update procd init script: remove nodogsplash dependency
- Add `nftables-nojson` package dependency
- Ship emergency-clear ruleset
- Test on real routers: GL.iNet AR150 (AR300M), Xiaomi, x86_64
- Verify: payment → gate open → internet → usage tracking → gate close

**Effort**: L (1-2 weeks)

---

## Summary Timeline

| Phase | Feature | Effort | Dependencies |
|-------|---------|--------|-------------|
| **Part A: Go Parity** | | | |
| 1 | Background monitoring + usage tracking | 1 week | None |
| 2 | Session lifecycle completion | 4 days | Phase 1 |
| 3 | Lightning invoice support | 1.5 weeks | Phase 2 |
| 4 | Payout routine | 1 week | Phase 5 |
| 5 | Degraded mode + mint health | 1 week | None |
| 6 | Config migration + validation | 2 days | None |
| 7 | CLI parity (5 new commands) | 1 week | None |
| **Part A subtotal** | | **~5-6 weeks** | |
| **Part B: Embedded Portal** | | | |
| 8 | CaptivePortal trait extraction | 3 days | None |
| 9 | nftables rule management | 1 week | Phase 8 |
| 10 | Client auth/revoke via nftables | 3 days | Phase 9 |
| 11 | Per-client bandwidth counters | 1 week | Phase 10 |
| 12 | Port-80 redirect server | 3 days | Phase 10 |
| 13 | IPv6 dual-stack | 2 weeks | Phase 10 |
| 14 | OpenWrt hardware testing | 1-2 weeks | Phases 9-13 |
| **Part B subtotal** | | **~5-6 weeks** | |
| **Total** | | **10-14 weeks** | |

## Parallelization Opportunities

- Phases 1+5+6+7 can run in parallel (no dependencies)
- Phase 8 can start while Part A is in progress
- Phases 3+4 depend on Phase 5 (mint health for payout pre-checks)
- Phase 13 (IPv6) can be deferred — ship IPv4-only first

## Dependencies to Add

```toml
[dependencies]
nftables = { version = "0.6", optional = true }
async-trait = { version = "0.1", optional = true }
async-channel = "2"  # for background task communication

[features]
default = []
embedded-portal = ["dep:nftables", "dep:async-trait"]
```

## Testing Strategy

| Level | What | How |
|-------|------|-----|
| Unit | Each module's logic | cargo test (mock ndsctl/nft) |
| Integration | HTTP endpoints + wallet | pytest rust-basic suite |
| Parity | Go vs Rust comparison | pytest parity suite (extend to 30+ tests) |
| nftables | Firewall rules on Linux | network namespaces + real nft binary |
| E2E | Full captive portal flow | QEMU OpenWrt VM + client container |
| Hardware | Real router validation | GL.iNet, x86_64 with real devices |

## Risk Mitigation

1. **nftables ruleset correctness** — watchdog timer + emergency-clear
2. **Counter overflow** — parse raw JSON for u64 if crate u32 overflows
3. **IPv6 address churn** — periodic refresh + MAC-based identity
4. **CDK version drift** — pin to 0.17, test with 0.18 when released
5. **Binary size** — monitor with `--features embedded-portal` (may add ~200KB)
