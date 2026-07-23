# Embedded Captive Portal — Implementation Plan

> **Status**: Experimental feature flag `embedded-portal`
> **Estimated effort**: 6-10 weeks (single experienced Rust dev)
> **Biggest risk**: nftables ruleset correctness on diverse OpenWrt kernels

## Goal

Replace nodogsplash as a runtime dependency by implementing captive portal
functionality directly in the Rust binary, behind a `#[cfg(feature =
"embedded-portal")]` feature flag. When the flag is off (default), the binary
uses ndsctl (current behavior). When on, it manages nftables rules directly.

## Architecture

```
trait CaptivePortal {
    async fn grant_access(&self, mac: &str) -> Result<()>;
    async fn revoke_access(&self, mac: &str) -> Result<()>;
    async fn poll_usage(&self, mac: &str) -> Result<(u64, u64)>;
    async fn is_authenticated(&self, mac: &str) -> bool;
    async fn install(&self) -> Result<()>;
    async fn teardown(&self) -> Result<()>;
}

// Default (no feature flag): NdsPortal — calls ndsctl
// With embedded-portal: EmbeddedPortal — manages nftables directly
```

## File structure

```
src/portal/
  mod.rs              # CaptivePortal trait + cfg dispatch
  nds.rs              # NdsPortal (wraps existing valve.rs)
  embedded.rs         # EmbeddedPortal (nftables-based)
  nft_manager.rs      # nftables rule lifecycle (table/chain/set/counter)
  redirect_server.rs  # Port-80 HTTP redirect for unauthenticated clients
  usage_tracker.rs    # Per-client bandwidth via nft named counters
```

## Dependencies to add

```toml
[dependencies]
nftables = { version = "0.6", optional = true }     # shells out to `nft` binary
async-trait = { version = "0.1", optional = true }   # trait async fn

[features]
default = []
embedded-portal = ["dep:nftables", "dep:async-trait"]
```

## Phases

### Phase 1: Trait extraction (3 days)
- Extract `CaptivePortal` trait from existing `valve.rs` + `metering/mod.rs`
- Create `NdsPortal` that wraps current ndsctl calls
- Wire `AppState` to hold `Arc<dyn CaptivePortal>`
- All existing behavior unchanged (behind default NdsPortal)
- **Deliverable**: Binary works exactly as before, but portal calls go through trait

### Phase 2: nftables rule management (1 week)
- Create `NftManager` that manages `table inet tollgate`
- `install()`: Create table + prerouting chain + forward chain + authenticated set
- `teardown()`: Remove table (RAII guard for cleanup)
- Idempotent: safe to call multiple times
- Test on Linux dev machine with `nft` installed
- **Deliverable**: Can install/remove captive portal ruleset from Rust

### Phase 3: Client auth/revoke (3 days)
- `grant_access(mac)`: Resolve MAC → IP, add IP to nftables set, create counter
- `revoke_access(mac)`: Remove IP from set, delete counter, call ndsctl deauth (transitional)
- MAC → IP resolution via `/proc/net/arp` + `ip neigh` (extend `mac_resolver.rs`)
- **Deliverable**: Paying client gets internet access; non-paying client is blocked

### Phase 4: Per-client bandwidth accounting (1 week)
- Create nftables named counter per authenticated client IP
- `poll_usage(mac)`: Run `nft -j list counters`, parse JSON, find counter by name
- Replace `metering::poll_usage` with portal abstraction
- Batch polling: single `nft -j list counters` returns ALL counters
- **Deliverable**: Real bandwidth tracking without ndsctl

### Phase 5: Port-80 redirect server (3 days)
- New axum listener on `0.0.0.0:80`
- Handler: check if client MAC is authenticated → 302 redirect to `:2121/` or passthrough
- Splash page HTML served from `openwrt/files/tollgate-captive-portal-site/`
- **Deliverable**: Unauthenticated clients see payment page; authenticated clients browse freely

### Phase 6: Background monitoring loop (3 days)
- tokio task: every 2-5 seconds, poll all session usages
- If usage ≥ allotment: revoke access + mark session expired
- If session expired by time: revoke access
- Replace the documented-but-unimplemented cleanup loop
- **Deliverable**: Sessions auto-terminate when allotment exhausted

### Phase 7: IPv6 dual-stack (2 weeks)
- Extend nftables to `inet` family (handles IPv4 + IPv6 in one table)
- Extend `mac_resolver.rs` with NDP resolver: `ip -6 neigh show`
- Track multiple IPv6 addresses per MAC (SLAAC privacy extensions)
- Test with real devices (iOS, Android, macOS, Windows)
- **Deliverable**: Captive portal works for IPv6-only and dual-stack clients

### Phase 8: OpenWrt integration + hardware testing (1-2 weeks)
- Update OpenWrt Makefile to build with `--features embedded-portal`
- Update procd init script to skip nodogsplash dependency
- Test on real routers (ARM): GL.iNet, Xiaomi, generic x86_64
- Failsafe: watchdog that removes tollgate table if HTTP unresponsive >60s
- Emergency-clear ruleset at `/etc/tollgate/emergency-clear.nft`
- **Deliverable**: Production-ready embedded portal on OpenWrt

## nftables ruleset design

```
table inet tollgate {
  set authenticated_v4 { type ipv4_addr; }
  set authenticated_v6 { type ipv6_addr; }

  chain prerouting {
    type nat hook prerouting priority dstnat; policy accept;
    tcp dport 80 ip saddr != @authenticated_v4 redirect to :80
    tcp dport 80 ip6 saddr != @authenticated_v6 redirect to :80
  }

  chain forward {
    type filter hook forward priority 0; policy accept;
    ip saddr @authenticated_v4 accept
    ip6 saddr @authenticated_v6 accept
    udp dport 53 accept        # DNS for unauth clients
    tcp dport 53 accept
    tcp dport 80 accept        # Gets redirected above
    drop                       # Block everything else
  }

  # Per-client counters (created dynamically)
  counter c_10_0_0_5 { packets 0 bytes 0 }
}
```

## Safety measures

1. **Never use `policy drop`** on forward chain — always explicit `drop` rule
2. **Watchdog timer**: Background task removes tollgate table if HTTP server
   doesn't respond for 60 seconds (prevents network lockout)
3. **Emergency ruleset**: Ship `/etc/tollgate/emergency-clear.nft` that
   operator can apply via SSH: `nft -f /etc/tollgate/emergency-clear.nft`
4. **Dry-run mode**: `nft --check` before applying rulesets
5. **Graceful degradation**: If nftables operations fail, fall back to ndsctl

## Testing strategy

| Level | What | How |
|-------|------|-----|
| Unit | NftManager rule generation | Mock `nft` binary (like valve.rs tests) |
| Integration | nftables ruleset on Linux | Real `nft` binary, network namespaces |
| E2E | Full captive portal flow | QEMU VM with OpenWrt, client container |
| Hardware | Real router validation | GL.iNet AR150/AR300M, x86_64 |

## Migration path

1. Default: `NdsPortal` (no behavior change)
2. Opt-in: Build with `--features embedded-portal` → `EmbeddedPortal`
3. Both paths share the same `CaptivePortal` trait — payment/session/wallet
   layers are completely unaffected
4. Operators can switch back to NdsPortal by rebuilding without the feature
5. Eventually: make `embedded-portal` the default and deprecate NdsPortal
