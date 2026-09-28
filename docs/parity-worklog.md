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

2026-09-27 12:05 UTC | rust 44c3f3f (+uncommitted build-ipk tar.gz fix) | results/rust-run1 (TOLLGATE_BACKEND=rust-basic, 107 files, RUST_RUN_RC=1)
- OUTCOME: 495 tests: 150 pass / 233 skip / 112 fail+error (86 unique).
  Diff vs go-baseline (results/go-baseline, GO_BASELINE_RC=1, 512 tests:
  215 pass / 243 skip / 54 fail+error): 44 both-fail, 68 rust-fail/go-ok,
  7 rust-ok/go-fail. **run1 is NOT a valid Rust product verdict** — the
  deployment it ran against was broken before pytest started:
- ROOT CAUSE 1 (cascade, ~50 of 68 rust-only failures): clean-VM ipk
  install leaves NO /etc/tollgate/config.json — Go's ConfigManager calls
  EnsureDefaultConfig (config_manager_config.go:347) which WRITES the
  default on first boot; Rust main.rs:30 does
  `load_config().unwrap_or_default()` and never persists. Then
  run-local-tests.sh configure_mint ran `jq ... config.json > /tmp/cfg.json`
  against the missing file → jq errored → `mv` installed an EMPTY
  config.json. Every config-reading fixture (mint_urls, mint_ip_map,
  config, local_502_config, config_guard) then JSONDecodeError'd, and the
  backend (default accepted_mints) rejected every lab token with
  `payment rejected: mint http://10.99.99.2:8383 not accepted`
  (token-verification-failed), incl. /ln-invoice 400.
- ROOT CAUSE 2 (10 restart failures + teardown cascade): router has NO
  curl (both runs; go pr193 shows same `ash: curl: not found`).
  router.py restart_backend health-checks via router-side curl → can
  never return 200 → "Rust backend did not become healthy" even when the
  fallback `setsid /tmp/tollgate` started fine. Also kills pr193
  identity tests on both backends. busybox wget IS present.
- ROOT CAUSE 3: service restart path — /etc/init.d/tollgate-wrt could
  not start the binary on the pre-f820a15 install (path bug, fixed
  committed f820a15; run1's VM did have /usr/bin/tollgate-wrt 0.1.0
  installed, so this contributed only via the pre-run health timeout).
- BOTH-FAIL classes (env, not parity signal): bash_client ×5 (git clone
  of sh1ftred/tollgate-bash-client fails, no route/branch), pr193 ×4
  (curl), LuCI 8080 (Go: 307 redirect; Rust: no LuCI, known gap),
  ssl lifecycle ×2 (443 still listening after remove — Go fails too),
  lightning_backoff ×3 + swap_regression restart invoice (Go: 429
  quote-rate-limited under hammering), keyset_id_versions (test bug:
  compares 8-byte short keyset ID to full V2 ID from /v1/keys).
- GENUINE RUST GAPS isolated so far (must re-verify on fixed deployment
  before fixing more): (a) no EnsureDefaultConfig write — root cause 1;
  (b) uncommitted build-ipk.sh ar→tar.gz ipk format fix is load-bearing
  (opkg 24.10 rejects busybox-ar ipks) and must land; ln-invoice 400 and
  degraded-mode results are unjudgeable under the poisoned config.
- Harness fixes queued: configure_mint must abort when config.json
  missing (never mv an empty file); restart_backend should use wget when
  curl absent; VM prep should install curl (deploy.py TEST_DEPS already
  lists it — local runner skips it).
- Lab note: host did NOT reboot (uptime 45d, contradicting handoff
  notes); OpenWrt 10.99.99.1 SSH-alive (ICMP filtered), Debian .100 up,
  CDK mint :8383 listening.

2026-09-27 13:00 UTC | rust 3054f82 + a7c2f06 + b223437 + 34714f4 / prta 2edf329
- FIXES from run1 triage (each verified on the live VM before the next):
  1. 3054f82 ensure_default_config: clean install now writes
     /etc/tollgate/config.json (2061 B, 0600, defaults) — Go
     EnsureDefaultConfig parity; empty/unparseable file backed up to
     config_backups/. 4 new unit tests; suite 312/312, fmt+clippy clean.
  2. 34714f4 ipk tar.gz format (busybox-ar rejected by opkg 24.10) —
     clean-VM install now works: 46 files, service starts, gonuts-export
     shipped.
  3. a7c2f06 dual-stack [::]:2121 listener — [::1] probes answer (were
     dead on 0.0.0.0-only bind); Go ":2121" parity with 0.0.0.0 fallback.
  4. b223437 get_client_ip to_canonical() — IPv4 peers on the dual-stack
     listener surfaced as ::ffff:10.99.99.100 and never matched
     dhcp.leases/ARP (mac-address-lookup-failed on every payment; found
     in run2's first file before relaunch).
  5. prta 2edf329 harness: configure_mint aborts instead of mv-ing an
     empty config (bootstraps default via restart first); --backend flag;
     ensure_router_curl installs curl (was missing on the VM in BOTH
     runs); restart_backend probes curl-else-wget; health window 20→600
     chars (rust event puts kind at ~char 180).
- VERIFICATION: clean-VM install → service up :::2121, config.json
  created, [::1] answers; run-local --backend=rust-basic
  test_access_denominated.py 3/3 PASSED (run1's price_per_step trio —
  restart + payment + pricing asserts all green).
- Full rust-run2 (107 files) relaunched against b223437 ipk →
  results/rust-run2/. Side-by-side matrix vs go-baseline pending run2
  completion.
- Open framework issue (not fixed, deliberate): 15 test files gate
  skips on `backend.is_rust` which is narrowly type=="rust"
  (tollgate-rs) — under rust-basic those guards never fire. Left as-is
  for now: rust-basic is a Go port, running the assertions is the
  parity signal; revisit per-test if a calibration skip proves needed.

2026-09-27 19:45 UTC | rust b223437 ipk / prta 33eb8a0 | results/rust-run2 + run2b + run2c
- M1 VM parity suite COMPLETE (three legs, contamination documented):
  run2 = full 107 files; run2b = rerun of load-contaminated files;
  run2c = final rerun after lab resurrection. Contamination sources
  hit mid-campaign: (a) sibling lane's cargo loop pinned host load at
  40 for ~2h → 1-vCPU OpenWrt VM starved (port-22 connect timeouts,
  15s curl timeouts); (b) both poc QEMUs died ~16:30 (zero qemu procs,
  no OOM trace — finale lane cycled the lab; PRTA 67c88f9 adds serial
  history); (c) the lab cycle left an ALIEN pre-dispatch binary at
  /usr/bin/tollgate-wrt (md5 8932ecab, binds 0.0.0.0, no CLI strings,
  mtime 16:01) — run2b's later files + the "tollgate health prints
  server log" symptom were THAT binary, not HEAD. Redeployed eb1c1263
  (b223437) before run2c; verified dual-stack + CLI help + md5.
- CONSOLIDATED rust verdict (2c > clean-2b > clean-2; starved→
  inconclusive): 160 PASS / 252 SKIP / 23 BAD / 12 inconclusive.
  Go baseline (Sep 26): 215/243/54. Rust-ok-where-go-fails: 15
  (payment core: payment_regression trio, sentinel duplicate, security
  spent/case-insensitive, swap invoices x4, nds allows-after-payment,
  whoami, mint_url fuzzy x2).
- GENUINE rust gaps isolated (clean evidence, run2c):
  G1 /identity/reveal-seed + friends → 404 (route absent; Go PR #193
     surface) — pr193 x4.
  G2 `tollgate wallet send` CLI missing (supported: balance, info,
     fund, drain) — mint_payout keyset-derivation test.
  G3 NUT-24 payment via X-Cashu header → empty response — nut18.
- Shared/env both-fail (not rust regressions): ssl 443-after-remove x2
  (holds on go too), LuCI 8080 (no LuCI by design), bash_client x5
  (git clone of sh1ftred/tollgate-bash-client fails from this host).
- Framework fixes landed during triage (prta 33eb8a0): go_only/rust_only
  marker gating now covers the backend family — 25+ go_only files
  (degraded_portal, cli_*, try_all_mints, recovery_lifecycle…) had been
  RUNNING under rust-basic because the gate compared backend_type ==
  "rust" literally. They skip now; their earlier rust results were
  either free parity signal (passes) or noise (failures).
- One-offs to re-verify next cycle on a pinned binary:
  nds_deauth_blocks_again (run2), degraded_mode::cli_health (only ever
  observed on the alien binary — expected fixed by a3be466+dispatch,
  cli files' clean rerun was lost to the /tmp junit overwrite).
- Operational lesson recorded: /tmp/local-suite-junit is a SHARED path
  — concurrent runners on this host overwrite each other's per-file
  junit (run2b files 1-3 lost to finale's smoke). Prefix by PID or
  per-run dir before the next multi-lane day.

2026-09-28 00:15 UTC | rust 5a65e3e+390f3d2 / prta 52deacf+8c568a7 | residue + go-baseline2 + G1-G3
- RESIDUE SWEEP (rust-run2d, pinned binary, quiet host): 6/6 files
  PASS incl. the lost cli files and both one-offs (nds_deauth_blocks_again,
  cli_health). 12 inconclusive → all pass. junit paths now PID-suffixed
  with a .latest symlink (prta 52deacf).
- G1 CLOSED (rust 5a65e3e): GET /identity + POST /identity/reveal-seed,
  Go PR #193 parity. NIP-06 (BIP39 seed + BIP32 m/44'/1237'/0'/0/0),
  bech32 npub, HKDF-SHA256 domain hashes for CGNAT IPv4 / per-iface MACs /
  six-word BIP39 passwords. Golden vectors generated from Go's
  identity.DeriveFromMnemonic — byte-identical for two standard mnemonics
  (caught a real bug: RFC 5869 expand counter byte was missing; first
  draft derived wrong IPv4 for every key). VM-verified: pr193 8/8 via
  PRTA. Isolated musl cost +114,688 B (+1.1%, parent-commit worktree
  build measured).
- G2 CLOSED (rust 390f3d2): not a missing feature — Go has no
  `wallet send` either; Go's cobra prints wallet HELP to stdout (exit 0)
  for unknown/missing subcommand, so PRTA skips. Rust client errored to
  stderr with empty stdout → {'raw': ''} → fail. Now mirrors Go's help
  output; VM-verified: mint_payout takes the identical skip branch.
- G3 CLOSED (prta 8c568a7): neither backend implements NUT-24 /pay;
  go-baseline "passes" for the nut18 file were curl-missing skips. The
  one failing test lacked its siblings' _nut24_supported gate — gated
  now. VM-verified: 1 pass + 4 skips, matching Go's shape.
- GO BASELINE2 (clean harness: curl installed, guarded configure_mint):
  231 pass / 244 skip / 37 bad (baseline1: 215/243/54; 26 fixed, 9
  newly-exposed). Go's failures now real: the documented wallet.db
  mint-state class ("TollGate is initializing. No reachable mints" —
  price_per_step trio, concurrent single-token 0/5, case-insensitive
  mint), ln-invoice 429 rate-limit family x6, degraded-mode lifecycle
  x7, ssl-443 x2, LuCI 307, bash_client x5 (env), https-asserts x2
  (local http lab mint).
- FINAL SIDE-BY-SIDE (rust run2..2d + rust-verify-g123 vs go-baseline2,
  446 common): rust 170 pass / 265 skip / 15 bad vs go 231/244/37.
  Rust PASSES 11 where Go fails (payment core: price trio, concurrent
  single-token, ln-invoice family x4, case-insensitive mints, wallet
  balance, block-all-mints-stays-up). Go passes 1 where rust fails
  (backend_no_500_during_degraded). Rust's remaining bad = 8 shared/env
  (LuCI, bash_client x5, keyset test bug, ssl x2) + 6 degraded-mode
  lifecycle family — of which Go fails 5 too. NEXT MILESTONE TARGET:
  degraded-mode enter/advertise/recover lifecycle parity (per-test
  disentangle of product gap vs lab-mint-blocking harness behavior).
- Note: rust skips are ~21 higher than go's solely because 25+ go_only
  files now correctly skip under the rust family (conftest 33eb8a0).

2026-09-28 01:20 UTC | rate-limiter restart report (estate-relay finding) | issue #27
- FINDING CONFIRMED with a sharper root cause: the 21023 rate limiter
  (src/rate_limiter.rs) is clean — in-memory Mutex<HashMap<IpAddr,
  Vec<Instant>>>, per-boot, no disk. Keyed per-IP ONLY (not
  per-IP-per-mint; allow(ip) at pay.rs:151) — spec question flagged in
  the issue.
- The "state persists across restarts" is PROCESS survival, not state
  persistence: a MANUALLY-started tollgate-wrt (setsid, /tmp fallback,
  debug session) is immune to `service tollgate-wrt restart|stop` —
  the init.d `stop()` override (ported verbatim from the GO package,
  which is the origin of the bug) only logs; rc.common restart =
  stop+start never sweeps non-procd processes. The manual instance
  keeps :2121 forever while procd's fresh instance AddrInUse-loops
  (respawn 3 5 0). Reproduced: 65 hammers → 429; TWO service restarts
  → still 429 from the same manual process.
- Clean procd path verified healthy: restart DOES recycle the instance
  and the limiter resets (429 → restart → 200), but with a
  term_timeout=5s SIGTERM race where the OLD process can still answer
  with stale state — a restart-then-immediately-pay harness can see
  either stale 429s or 000s.
- PRTA-side relevance: router.py restart_backend's `setsid /tmp/tollgate`
  fallback is a manual-start factory; every invocation leaves an
  unmanageable process behind. (Our clean-VM runs today re-deployed via
  opkg, so run2/2b/2c/2d results are unaffected.)
- Filed: Amperstrand/tollgate-module-basic-rust#27 (includes repro,
  shared-blame note for the Go packaging, fix suggestions: drop the
  stop() override + sweep strays; startup guard when :2121 is held by a
  non-child). Test pin deferred to the fix PR.
- VM restored to a single procd-managed instance (verified fresh
  listener + payment allowed) after the repro.
