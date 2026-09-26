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

2026-09-26 17:50 UTC | rust fa0d7a2 / prta 9d44c20+2735590
- Differential parity suite (host, lab fakewallet mint, both binaries):
  30 passed, 1 xfailed (CLI protocol xfail — fix in flight).
- Go baseline binary initially built from a non-main branch: gonuts dev
  build injected unreachable testnut mint (kind 21023 poisoning). Rebuilt
  with production ldflags (config_manager.GitBranch=main, cli.Version) —
  the VM ipk must be rebuilt the same way for the Go baseline. NOTE for
  VM baseline: use ipk built AFTER this discovery; the earlier
  tollgate-wrt_v0.6.1-parity-baseline_x86_64.ipk embeds the branch name.
- Host-side Go payment needs a stub /usr/local/bin/ndsctl (exit 0) —
  gate-open blocks otherwise. Installed (sudo) on the lab host.
- Shared-host hazard: sibling omarchy-cashu testbed leaves orphaned
  (ppid=1) mockgate/rust-gate daemons on :2121. Parity test now kills
  orphaned holders / skips on actively-owned ones
  (_free_port_2121_or_skip). Manual cleanup may still be needed
  (`ss -tlnp | grep 2121` + kill by pid).
- /tmp/dhcp.leases on lab hosts is a symlink to root-owned
  /var/lib/misc/dnsmasq.leases — rust_basic_server fixture now swaps it
  for a writable file and restores.
- FUND-SAFETY RESULT: below-minimum purchase rejected on BOTH backends
  with token UNSPENT at the mint (NUT-07) — the historical
  "receive-before-validate" defect is NOT present in current rust main.
- Go concurrent-receive defect (reference behavior): 5 parallel distinct
  token payments → Go grants 2/5 (gonuts "Duplicate outputs" +
  keyset-counter db race). Rust/CDK grants 5/5. Documented as deliberate
  Rust improvement in the parity test docstring.
- Go baseline VM suite: progressing (54+/107 files at time of writing).
