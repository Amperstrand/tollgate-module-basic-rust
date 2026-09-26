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
| 2 | Daemon binary path | pkg /usr/bin/tollgate-wrt | build-ipk.sh installs /usr/bin/tollgate | — | — | — | pending | FAIL | init.d/tollgate-wrt execs /usr/bin/tollgate-wrt — never installed | install binary as /usr/bin/tollgate-wrt |
| 3 | Operator CLI client (`tollgate status/version/health/wallet/config…`) | /usr/bin/tollgate (cobra) | src/cli/mod.rs socket server only; no client, no arg dispatch in main.rs | yes | partial | partial | pending | FAIL | binary always runs server; socket protocol plain-text vs Go JSON CLIMessage | add client mode + subcommands |
| 4 | gonuts-export migration helper shipped | n/a (Go native) | tools/gonuts-export (not packaged) | — | — | — | pending | FAIL | main.rs requires /usr/bin/gonuts-export at first boot | build+ship in ipk |
| 5 | Packaging payload parity | 5 binaries, hotplug, 3×nft, man8 pages | 1 binary, 1×nft, no hotplug/ssl/first-login | — | — | — | pending | PARTIAL | see payload diff in worklog 2026-09-26 | port missing files |

(Matrix continues as verification proceeds — see docs/parity-worklog.md for
the append-only evidence log.)
