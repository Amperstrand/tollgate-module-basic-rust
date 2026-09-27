# PARITY.md — tollgate-module-basic-rust vs tollgate-module-basic-go

Living document. Reference: Go `main` @ `1377a87` (behavioral spec unless
documented as bug). Rust under test: branch `parity/m1-vm-parity`.
PRTA: Amperstrand fork @ `15eab12` + upstream merge (`f01a2a5`), branch
`parity/rust-basic-amperstrand`.

Environment (recorded 2026-09-26): Ubuntu 24.04.4, kernel 7.0.0-28-generic,
x86_64, 16 vCPU, KVM available (`/dev/kvm`, kvm_amd nested=1), rustc/cargo
1.98.1, QEMU 8.2.2, Python 3.12.3, Go 1.25.8 (pinned CI: 1.26.8 — local build
used TG_ALLOW_GO_MISMATCH=1).

Repo SHAs at start:
- tollgate-module-basic-rust: `6c68e210a8e465f22cc2203893e6fdfc6fe6fcb0`
- tollgate-module-basic-go: `1377a8766787365bc20a148794c8fe27647dbd5d`
- physical-router-test-automation (fork): `15eab1244706baa8ac71243b2e2aad3c6d12770f`
- physical-router-test-automation (upstream parent): `f01a2a572f2e3a2e5cec09f864a2ed3ea57a2d16`

OpenWrt VM: 24.10.1 r28597-0425664679 (x86_64, QEMU, KVM).

## Dependency audit (2026-09-26)

| Crate | Before | After | Note |
|---|---|---|---|
| cashu (direct) | 0.17.3 | 0.18.1 | dedupe — was 0.17 direct + 0.18 via cdk |
| cdk / cdk-sqlite | 0.18.0 | 0.18.1 | matches PRTA lab mint (0.18.0 line) |
| tokio | 1.53.1 | (semver-refreshed) | |
| axum | 0.8.9 | — | current |
| thiserror | 1.0.69 | 1.0.69 | v2 exists; deferred (no functional gain) |
| rand | 0.8.7 | 0.8.7 | 0.9 exists; secp256k1 0.29 pins 0.8 API |
| secp256k1 | 0.29.1 | 0.29.1 | current |

Binary size: 10,668,832 → 10,581,664 B (x86_64-gnu, −0.8%);
x86_64-musl static: 10,235,144 B. Go ipk: 8,353,734 B (8.0 MB, daemon+CLI+assets).

cdk-common 0.18.1 contains no `AtomicU64` — the mipsel 32-bit-atomics
`[patch.crates-io]` contingency remains inactive (comment in Cargo.toml).
Cross-lanes: x86_64-musl verified locally; aarch64/armv7 spot-check pending
(docker+cross available); mips/mipsel CI-only (nightly build-std).

## Feature matrix

Status legend: PASS / PARTIAL / FAIL / NOT TESTED / BLOCKED.

| # | Feature | Go ref location | Rust location | Go tests | Rust tests | PRTA | VM validated? | Status | Known semantic differences | Required work |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | PRTA rust-basic repo mapping | — | PRTA lib/backend.py:55 | — | — | yes | n/a | PASS | now Amperstrand (fork commit) | — |
| 2 | Daemon binary path | pkg /usr/bin/tollgate-wrt | build-ipk.sh installs /usr/bin/tollgate-wrt | — | — | — | pending | PASS (fixed) | init.d/tollgate-wrt + openwrt Makefile aligned (commit f820a15) | VM install test |
| 3 | Operator CLI client (`tollgate status/version/health/wallet/config…`) | /usr/bin/tollgate (cobra) | multi-call dispatch + JSON CLIMessage protocol (in flight) | yes | partial | partial | pending | PARTIAL | implementation underway | land + VM test |
| 4 | gonuts-export migration helper shipped | n/a (Go native) | tools/gonuts-export | — | — | — | pending | PASS (fixed) | built+shipped in ipk (commit f820a15) | VM migration test |
| 5 | Packaging payload parity | 5 binaries, hotplug, 3×nft, man8 | ported all but man pages + /usr/bin/tollgate symlink | — | — | — | pending | PARTIAL | man8 pages P3-deferred; CLI symlink lands with #3 | VM install/upgrade test |
| 6 | GET /whoami semantics | main.go handler: 200 `mac=` always | whoami.rs: 200 `mac=` always | yes | yes | parity test | host | PASS (fixed fa0d7a2) | | |
| 7 | POST/GET /ln-invoice semantics | main.go handleLightningInvoicePost/Get | ln_invoice.rs | yes | yes | parity test | host | PASS (fixed fa0d7a2) | code/retry_after fields, device-unresolved, quote-MAC binding, amount+mint_url required, 1M sats ceiling | VM: real quote lifecycle |
| 8 | Payment: valid token | merchant.PurchaseSession | pay.rs | yes | yes | parity payment matrix | host | PASS | | |
| 9 | Payment: double-spend rejection | merchant | pay.rs | yes | yes | parity payment matrix | host | PASS | | |
| 10 | Payment: malformed/unknown-mint | main.go | pay.rs | yes | yes | parity payment matrix | host | PASS | | |
| 11 | Payment: below-minimum fund safety | merchant (validate before receive) | pay.rs | yes | yes | parity payment matrix + NUT-07 | host | PASS | Rust does NOT consume the below-min token (old defect fixed in main) | |
| 12 | Payment: concurrent receives | gonuts (races: duplicate outputs) | CDK atomic counters | yes | yes | parity payment matrix | host | PASS (Rust better) | Go 2/5 grants under concurrency (gonuts counter race); Rust 5/5 — documented deliberate improvement | Go-side known issue |
| 13 | Payment: trailing-slash mint config | canonicalization | wallet | yes | yes | parity payment matrix | host | PASS | | |
| 14 | Nostr discovery (kind 10021 tags) | main.go handler | routes/discovery.rs | yes | yes | parity test | host | PASS | | |
| 15 | First-boot default config.json on disk | config_manager.go:347 EnsureDefaultConfig (writes default when missing/empty/invalid, backs up unusable file) | config::ensure_default_config (3054f82) | yes | yes (4 unit tests) | config fixtures | VM (run2: file created after clean install) | PASS | empty/unparseable backed up under config_backups/, profit_share reset, ensure_defaults normalize | — |
| 16 | Listener address family | main.go ":2121" dual-stack | main.rs binds [::]:2121, 0.0.0.0 fallback (a7c2f06) | implicit | — | restart health probes | VM ([::1] + 127.0.0.1 both answer) | PASS | was 0.0.0.0-only → every [::1] probe failed | — |
| 17 | Client IP normalization for mapped v6 peers | Go net reports ::ffff:a.b.c.d as IPv4 | mac_resolver get_client_ip to_canonical() (b223437) | implicit | yes (2 unit tests) | payments via Debian VM | VM (price_per_step trio 3/3 — run1's failure class) | PASS | lease/ARP lookups + XFF loopback trust match Go | — |
| 18 | ipk container format | Go packaging (tar.gz ipk) | build-ipk.sh tar.gz (34714f4) | — | — | opkg 24.10 install | VM (clean install OK) | PASS | busybox-ar ipks rejected by opkg 24.10 | version pinning for upgrade-in-place |
