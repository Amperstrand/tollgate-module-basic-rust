# Parity worklog (append-only)

Format: timestamp | commit | test command | failure | root cause | change | retest

---

2026-09-26 14:30 UTC | rust 6c68e21 / go 1377a87 / prta-fork 15eab12 (+upstream f01a2a5)
- Environment recorded (see PARITY.md).
- Rust baseline: `cargo fmt --check` PASS; `cargo clippy --all-targets --all-features -- -D warnings` PASS;
  `cargo test --all-features` 245/245 PASS; `cargo build --release` PASS (10,668,832 B);
  `cargo build --release --target x86_64-unknown-linux-musl` PASS (10,235,144 B static).
- Dependency audit: duplicate cashu (0.17 direct + 0.18 via cdk-common). 95
  stale transitive pins. → bump cashu to 0.18 + `cargo update`
  (commit 16b1284). Retest: all green, binary −87,168 B.
- cdk-common 0.18.1: no AtomicU64 (registry grep) — mipsel patch contingency inactive.

2026-09-26 14:40 UTC | prta f4ccafc
- lib/backend.py:55 mapped rust-basic/rust-embedded → felixfelix-bot/tollgate-module-basic-rust.
  README table too. Upstream PRTA main has the same staleness. Fixed to
  Amperstrand/tollgate-module-basic-rust. Merged upstream/main (162 commits)
  into parity/rust-basic-amperstrand — clean merge.
- Note: PRTA `workflow` property for rust backends is "Build and Package";
  Amperstrand repo workflows are named "CI"/"Cross-compile + Package"/"Rust
  Basic CI" — CI-artifact download path for rust-basic is broken-by-name
  (matters only for `--tollgate-branch` deploys; milestone 1 uses local ipks).

2026-09-26 14:50 UTC | go 1377a87
- Built Go x86_64 ipk: `ARCH=x86_64 PKG_VERSION=v0.6.1-parity-baseline
  TG_ALLOW_GO_MISMATCH=1 bash packaging/local-build-ipk.sh` →
  tollgate-wrt_v0.6.1-parity-baseline_x86_64.ipk (8,353,734 B).
- Go payload manifest (packaging parity reference): /usr/bin/{tollgate-wrt,
  tollgate, tollgate-apply-ssl, tollgate-remove-ssl, check_package_path},
  /usr/local/bin/first-login-setup, /etc/init.d/tollgate-wrt,
  /etc/hotplug.d/iface/95-tollgate-restart,
  /etc/nftables.d/{20-nds-enforce,30-backend-firewall,31-admin-board-not-guest-reachable}.nft,
  /etc/uci-defaults/{90-tollgate-captive-portal-symlink,99-tollgate-setup},
  /lib/upgrade/keep.d/tollgate, portal site, man8 pages, LICENSE.
- Go control: Package tollgate-wrt; Depends libc, nodogsplash, jq;
  Provides nodogsplash-files; Replaces base-files.
- Go init.d: execs /usr/bin/tollgate-wrt, starts cron, respawn 3 5 0,
  depends nodogsplash, TOLLGATE_DEBUG=1, log /tmp/tollgate-debug.log.

2026-09-26 14:55 UTC | rust 6c68e21
- Built Rust x86_64 ipk via packaging/build-ipk.sh → tollgate-rs_0.1.0_x86_64.ipk.
- CONFIRMED GAP (init script): packaging/files/etc/init.d/tollgate-wrt execs
  /usr/bin/tollgate-wrt, but build-ipk.sh installs the binary as
  /usr/bin/tollgate. openwrt/Makefile installs /usr/bin/tollgate-module-basic-rust
  with the SAME broken init script. Service can never start from either path.
- CONFIRMED GAP (CLI): main.rs has no argv dispatch — `tollgate --version`
  starts the server. Socket server supports only version/status/
  "wallet info"/"wallet balance"/migrate. Go CLI surface is much larger.
- CONFIRMED GAP (migration): main.rs first-boot migration shells out to
  /usr/bin/gonuts-export — tool exists in tools/ (source + prebuilt) but is
  not shipped in the ipk.
- build-ipk.sh broke under global CARGO_TARGET_DIR redirect (looks for
  ./target). Fixed to resolve via `cargo metadata` (uncommitted yet).
- Rust payload vs Go payload missing: /usr/bin/tollgate-wrt name collision
  aside — no hotplug restart, no 30-backend-firewall.nft, no
  31-admin-board-not-guest-reachable.nft, no ssl helpers, no
  first-login-setup, no check_package_path, no man pages.
- Rust control: Package tollgate-rs (Go: tollgate-wrt); Provides/Conflicts/
  Replaces tollgate-wrt — replace semantics OK.

2026-09-26 15:00 UTC | lab
- virtual-lab doctor --host localhost: PASS (KVM ok, all required commands).
- start-poc initially raced a stale qemu (pid 882723) holding
  tollgate-poc.qcow2; cleaned via stop-poc; fresh start-poc OK:
  OpenWrt 10.99.99.1 (24.10.1), Debian 10.99.99.100, host 10.99.99.2.
- VM prior state: Go tollgate-wrt v0.6.1-post-merge-14 running; /etc/tollgate
  has mixed Go (wallet.db) + Rust (wallet_seed.bin, per-mint sqlite) state
  from earlier experiments — will reset for clean baselines.
