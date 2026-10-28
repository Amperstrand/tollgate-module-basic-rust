# Consolidating tollgate-module-basic-rust into OpenTollGate

> Status: DRAFT — capability matrix and PR series being finalized.
> Tracking issue: see the GitHub issue linking this document.
> Date: 2026-10-25

## 1. Goal and non-goals

**Goal.** Move every improvement that lives in `Amperstrand/tollgate-module-basic-rust`
to `OpenTollGate/tollgate-module-basic-rust` as a series of small, independently
reviewable PRs, verify them (CI + conformance + ai-legion hardware lanes), and then
sunset the Amperstrand repository so all future work is coordinated on OpenTollGate.

**Non-goals.**

- No rewrite of upstream's architecture. Upstream's decisions (ADRs, SQLite
  sessions, identity-from-socket) stand unless a measured case is made in an ADR
  of our own.
- No "big bang" equivalence PR. If a delta cannot be reviewable, it is split or
  dropped with rationale.
- No fork divergence period: once the PR series lands, Amperstrand's repo is
  frozen (archived), not maintained in parallel.

## 2. Situation (facts, 2026-10-25)

- `OpenTollGate/tollgate-module-basic-rust` was created 2026-10-20 by
  **felixfelix-bot** importing his independently developed implementation
  (PR #1, 93 files, byte-identical to `felixfelix-bot/tollgate-module-basic-rust`
  `main`), followed by PRs #2–#6 (small fixes, Go-parity routes, NIP-06
  reveal-seed). **PR #7 ("spilman" channel payment rail, 61 files, +10.5k
  lines) is open** and actively developed.
- The two codebases have **disjoint git histories**. This is a
  cross-codebase capability port, not a rebase:
  - upstream `src/` ≈ 9.9k LOC vs Amperstrand `src/` ≈ 24.0k LOC
  - shared-but-divergent files: `src/wallet/wallet.rs` (592 vs 2483),
    `src/http/routes/pay.rs` (517 vs 1097), `src/cli/mod.rs`, `src/main.rs`,
    `src/identity.rs`, `src/config/schema.rs`
  - 118 files exist only on our side (payment/payout journals, rate limiter,
    `mint_url.rs`, portal, wireless, reseller, CLI subcommands, ipk packaging,
    5 regression suites); 56 only upstream (ADR docs, contract tests,
    SQLite session store, session tickets, identity derive, conformance
    verdicts)
  - both on cdk 0.18.x (upstream lockfile: 0.18.2; ours: 0.18.1)
- The **Go module already made this move** (fork consolidation, see
  `tollgate-module-basic-go#758` and the `OpenTollGate/.github` migration) —
  we follow that precedent (template in §7).
- The `Amperstrand` GitHub account is a **member of the OpenTollGate org**;
  upstream `main` has no branch protection, no PR/issue templates. Review
  culture (from PRs #1–#6): measured claims, committed evidence artifacts,
  ADRs for binding decisions.
- Upstream is **nodogsplash**-based; our repo has both a captive-portal
  (nftables) path and packaging for OpenWrt (`packaging/files`, 31 files).

## 3. Amperstrand work accounting (everything, and its disposition)

Every commit/branch that exists on the Amperstrand side is listed here with a
disposition. Nothing is silently dropped.

### 3.1 The 17 commits on `origin/main` not yet ported anywhere (2026-09-27 → 2026-10-23)

| Commit | Amperstrand PR | Theme | Size | Disposition |
|---|---|---|---|---|
| `0dbc7e0` | #25 | ln-quote durability, Codex review round | +926/7f | PORT ( Lightning quote durability) |
| `a27bfb3` | #29 | migration hardening, Codex review round | +1502/7f | PORT (migration safety) |
| `f3dd65d` | #37 | NUT-13 seed-restore backstop + migration status | +499/9f | PORT (seed backup story) |
| `2dc87f5` | #39 | in-session saga reconciliation after timeouts | +218/3f | PORT (wallet recovery) |
| `87d7df9` | #43 | durable payment journal + startup session reconciliation | +840/8f | PORT (core fund-safety) |
| `3837911` | #44 | link in-flight gate to tokens per-Y, not per-mint | +61/2f | PORT (gate correctness) |
| `bef76f0` | #42 | cdk 0.18.1 alignment, drop legacy `cashu` crate | +1444/13f | ADOPT-UPSTREAM (already 0.18.2) + cherry-pick deltas |
| `5e8ebb1` | #45 | durable payout journal | +982/6f | PORT (payout safety) — coordinate with PR #7 |
| `ee7ab69` | #50 | PartiallySpent remainder auto-recovery + exporter CI guard | +417/7f | PORT (migration safety) |
| `08ebb00` | #51 | durable withdrawal intents (fund/drain journals) | +221/7f | PORT (CLI ops safety) |
| `df1dbd7` | #38 | reseller durable-record contract (carries #28) | +9266/104f | SPLIT: contract logic vs portal/site bundle; see §5 |
| `ff0513b` | #53 | serialize env-mutating CLI tests | +8/1f | FOLD into whichever CLI PR lands first |
| `1824720` | #54 | `/session-state` route (Go #541) | +161/4f | RECONCILE with upstream PR #4 (already has session-state) |
| `5568298` | #55 | Go-parity notice-code taxonomy (R16) | +175/1f | PORT (error semantics) — verify against upstream's taxonomy |
| `0da326a` | #57 | keyset hygiene: expiry warnings + proactive rotation | +193/2f | PORT (wallet hygiene) |
| `274f001` | #58 | S13 audit disposition: 14 fixes + 9 design gaps filed | +1552/19f | SPLIT: fixes fold into their topical PRs; gap list → upstream issues |
| `9e3fc8b` | #74 | rustls/webpki TLS advisory bumps | +6/1f | UPSTREAM-DEP (check 0.18.2 lockfile; bump if stale) |

### 3.2 Earlier work already on our base (pre-2026-09-27, equally un-ported)

These predate the 17 but are just as much "our improvements" — upstream never
imported any Amperstrand code. Capability-level dispositions in §4 cover:
validate-and-price-before-receive (#4/#20), value-retaining first-boot
migration (#12/#21), durable Lightning quotes that actually grant (#9/#22/#23),
fund-safety AGENTS.md (#3), MIPS32 `Mutex<u64>` fix (#2) + cross-compile lanes
(#1), HTTP 429 retry/backoff + per-IP rate limiting, spending-condition-locked
token rejection (P2PK/HTLC) + 1 MB body cap, `ln_invoice` Go-format parity +
lock-poison recovery, captive portal (embedded portal feature, nftables
watchdog + emergency-clear, IPv6 dual-stack), wireless upstream manager,
`rate_limiter.rs`, `mint_url.rs` canonicalization, `.ipk` packaging CI,
`mac_resolver`, `upstream_detector`, `monitor`, `valve`, audit report docs.

### 3.3 Loose ends

| Item | State | Disposition |
|---|---|---|
| `173d757` (rebased `91f6c03`) AGENTS.md lab SOP | local main, unpushed | Push with this plan; content is Amperstrand-lab-specific — stays out of upstream |
| Open PR #68 `audit/s13-followup` (durable grant idempotency + P-521 SEC1 parse) | open, +1546/−40, 15 files | Finish review on Amperstrand OR port content into the payment-journal PR upstream; decide in Wave 1 |
| Local branch `audit/s13-codex-feedback` (2 docs commits `7b427db`, `d8d89ea`: S13 campaign verdicts, +855 lines) | local only | Push branch to origin as backup; verdict docs inform upstream PR descriptions |
| `fix/initd-stop-stray-sweep` (25 commits vs main, base 2026-09-29) | origin branch, no PR | SUPERSEDED on main (stop-sweep + `initd_stop_test.sh` already landed); verify portal-bundle + reseller_mode bits, then drop |
| Stash backups ×3 (July era WIP) + `stash-backup/*` remote branches | origin | Old WIP on pre-August bases; review-for-value then drop at sunset |
| 21 merged PR branches (`audit/*`, `feat/*`, `fix/*`) on origin | merged (squash) | Delete at sunset |

## 4. Capability delta matrix

Dispositions: **PORT** (we bring it as PR(s)) · **ADOPT-UPSTREAM** (their
solution stands, we drop ours) · **RECONCILE** (both have one; merge semantics)
· **DROP** (not wanted upstream, with rationale). Evidence:
`docs/migration-evidence/amperstrand-capabilities.md` (our side, 35 caps) and
`docs/migration-evidence/upstream-capabilities.md` (their side @ 19ae887).

| Capability | Ours | Upstream | Disposition | Notes |
|---|---|---|---|---|
| Validate-and-price-before-receive | precheck before any wallet access (#20) | MAC resolution + pricing run **after** `receive()`; `purchase_min_steps` unenforced; persist-failure still returns 200 | **PORT** (Wave 0, PR-1) | Closes token-consumed-with-no-session; Go-parity `payment-error-below-minimum` code exists in Go |
| Payment journal + duplicate-POST idempotency + startup reconcile | JSONL journal + NUT-07 reconcile (#43/#44/#58; #68 unmerged) | **NOT FOUND** (no token-hash keying) | **PORT** (Wave 1, PR-2) | Adapt to upstream's SQLite store (ADR-0001) instead of JSONL; fold #68 grant-idempotency/tombstone content |
| In-session saga reconciliation after timeouts; 504 `payment-outcome-unknown`; `recover_before_op` | #39 + #42 | sagas recovered only at boot `ensure_mint` | **PORT** (Wave 1, PR-3) | Timeout ⇒ unknown, not rejected |
| Durable ln-quote lifecycle | durable store + settle-once monitor (#22/#23/#25) | durable `quotes` **table exists but unwired**; restart mid-quote strands a paid invoice | **PORT** (Wave 1, PR-4) | Smallest gap-to-close: switch handler onto existing table + boot settle; Amperstrand crash-window tests port |
| Payout journal + `journalled_melt` decision table | #45 | no payout execution at all (config-only) | **PORT** (Wave 1, PR-5) | Coordinate with PR #7 rail; journal is rail-agnostic |
| Rate limiting + pre-buffer body cap + global HTTP timeout | per-IP limiter + 1MB cap + backoff | **NOT FOUND**; body cap enforced after unbounded buffering | **PORT** (Wave 1, PR-6) | Use `axum` `DefaultBodyLimit` (pre-buffer) + `tower::timeout` |
| Keyset hygiene (expiry warnings + rotation) | #57 | **NOT FOUND** | **PORT** (Wave 2) | Mirrors open Go #417 |
| NUT-13 seed backstop + `wallet restore` | #37 | **NOT FOUND** (raw seed bin, no restore; critical given keep.d gap) | **PORT** (Wave 2) | Independent of their NIP-06 reveal-seed (identity ≠ wallet) |
| P2PK/HTLC spending-condition rejection | via cdk `token_secrets` (#42) | **NOT FOUND** | **PORT** (Wave 2, small) | |
| Mint-URL canonicalization | `mint_url.rs` (open gaps #59/#76 dot-segments) | raw string + trailing-slash; `select.rs` lowercases inconsistently | **RECONCILE** (Wave 2) | Adopt CDK `MintUrl` type on both compare + wallet-map sites |
| Notice-code taxonomy (Go parity) | #55 | partial vocabulary; `payment-error-token-spent` mis-keyed for sub-step/invalid-JSON | **RECONCILE** (Wave 2) | Land with PR-1's below-minimum code; verify against Go corpus |
| Withdrawal intents (fund/drain journals) | #51 | **NOT FOUND** (CLI is 5 commands) | **PORT** (Wave 3) | Drain journal mirrors Go `src/cli/drain_journal.go` |
| Gonuts migration suite (journal, conditional rename, convergent re-runs, remainder auto-recovery, exporter guard) | #21/#29/#42/#44/#50 | manual CLI `migrate` only; import failures retained but no journal/convergence | **PORT** (Wave 3) — **owner decision** | Only if upstream must migrate Go production wallets; upstream already ships MIGRATION.md + exporter |
| Reseller durable-record contract (`journalled_purchase`) | #52 | `reseller_mode` parsed, dead; PR #7 adds different upstream-purchase machinery | **DEFER** (post-#7) | Revisit after #7 lands; contract pattern may inform their payer lifecycle |
| Embedded nftables portal + IPv6 + port-80 redirect | feature-flagged, KVM-tested | nodogsplash-native (ADR-0006 measured) | **DROP** (upstream architecture decision) | Cherry-pick micro-fixes only (Host-header escape if missing) |
| Wireless upstream manager event loop | 706 LOC + config fields | config fields parsed for compat, no runtime | **DROP** (with PR #7 rail as the upstream answer) | |
| Sessions persistence | sessions.json debounced + save_now | SQLite WAL+FULL, fail-closed exit 78 (ADR-0001/0002) | **ADOPT-UPSTREAM** | Strictly better |
| Gate lifecycle / metering | ndsctl via valve + monitor | ADR-0006 measured sweep + boot continuity | **ADOPT-UPSTREAM** | |
| Identity model | mac_resolver + XFF-on-loopback | ADR-0007 identity-from-socket + Go-parity routes (#4) + NIP-06 (#6) | **ADOPT-UPSTREAM** | ADR-0007 fix is explicitly missing on our side (our #73) |
| /session-state | #54 | upstream PR #4 | **ADOPT-UPSTREAM** (verify our 24h-TTL history parity in their impl) | |
| cdk 0.18 alignment | #42 (0.18.1) | 0.18.2 lockfile | **ADOPT-UPSTREAM** | Cherry-pick nothing unless delta found |
| TLS advisory bumps (#74) | rustls 0.23.46 etc. | check lockfile | **UPSTREAM-DEP** | Bump if stale |
| MIPS32 Mutex/no-atomics + 5-arch lanes | #2/#1 | **no MIPS lane** | **OFFER** as separate infra PR (owner decision: is MIPS a target upstream?) | |
| .ipk packaging CI | tar.gz ipk in CI | staged binaries only, no ipk in CI | **OFFER** (small infra PR) | |
| Test serialization, exporter-hygiene CI | #53, #50 | own harness | **DROP/FOLD** where applicable | |
| Conformance corpus runner | PRTA lanes | committed verdict JSONs + honest UNVERIFIED culture | **ADOPT-UPSTREAM** | Their runner is the arbiter |

## 5. PR series (waves)

Per-PR card: scope · source (Amperstrand commits) · conflict surface with
upstream PR #7 (draft, 61 files, pay/session/config/Cargo).

**Wave 0 — landed 2026-10-27/28:**

- [x] Plan committed + pushed (Amperstrand main `5f44731`; branch `audit/s13-codex-feedback` pushed as backup)
- [x] Tracker: Amperstrand/ops#42
- [x] **PR-1 → OpenTollGate#8** `fix(pay): validate and price before receive` — pre-receive price gate (Go-message parity), MAC before receive, no-200-on-persist-failure, below-swap-fee re-key. 201 tests (194→201).
- [x] **keep.d → OpenTollGate#9** sysupgrade keeps `/etc/tollgate` (tollgate.db + per-mint wallet *.sqlite were dropped = silent balance loss).
- [x] **PR-6 → OpenTollGate#10** rate limiting (Go wire parity: 429/Retry-After 6/default 10 rpm) + 2 MiB pre-buffer DefaultBodyLimit + 35 s TimeoutLayer→504. 201 tests (194→201).
- [x] **PR-4 → OpenTollGate#11** durable ln-quotes: creation persists into the quotes table, GET rehydrates from store (restart no longer 404s a paid quote), durable issued-mark, boot value settlement of PAID quotes (single-mint; multi-mint needs schema work). 197 tests (194→197).
- [x] **PR-3 → OpenTollGate#12** timeout = unknown outcome: detached in-session `recover_incomplete_sagas` on every op timeout arm + 504 `payment-outcome-unknown` mapping. 196 tests (194→196).

**Wave 1 — remaining (execution notes for the next session):**

- [x] **PR-2 → OpenTollGate#13** durable payment journal + boot reconciliation + idempotent replay — schema v2 with in-place v1→v2 migration; intent-before-receive with frozen pricing terms; NUT-07 boot reconcile (terminal only after the durable grant); settled-token replay to the original buyer's MAC; persist-failure rollback to intent. 214 tests (194→214). Deliberate follow-up: the per-payment durable grant-id ledger (our unmerged #68 / issue #60 — additive same-MAC grants) needs its own upstream schema discussion.
- [ ] **PR-5 payout journal — OWNER DECISION before porting.** Upstream has no payout execution path at all (config parsed, no loop, no `send()` caller; their open PR #7 adds a different payment rail). Porting our `journalled_melt` decision table means introducing payout execution to a repo that deliberately lacks it — that is a product decision (their reviewers' call), not a mechanical port. Recommended framing when the owner returns: offer the journal + loop behind config as one PR, explicitly coordinated with PR #7's rail, or defer until #7 resolves. Source material if green-lit: Amperstrand `src/payout_journal.rs` (358 LOC, decision table unit-tested without network) + `tests/payout_regression.rs` (14 cases).

**Wave 2 — wallet robustness:**

- [x] P2PK/HTLC spending-condition rejection → **OpenTollGate#14** (parse-time refusal via cashu `token_secrets` + `SpendingConditions`; pinned by real cashuB wire-format fixtures; 198 tests).
- [x] Hermetic test state-db (unplanned, incident-driven) → **OpenTollGate#15** — the CLI test harness derived its store from a hardcoded `/etc/tollgate/tollgate.db`, which opened/migrated the bench host's real state (see the ops#42 incident note); tests now derive it from the redirected config dir.
- [ ] Keyset hygiene (#57 — mirrors open Go #417): expiry warnings + proactive same-mint rotation; boot + periodic sweep.
- [ ] NUT-13 seed backstop + `wallet restore` (#37; independent of their NIP-06 identity work): BIP39 mnemonic beside the seed, crash-safe write ordering, mismatch refusal, restore loop per mint.
- [ ] `MintUrl` canonicalization reconcile: adopt CDK's `MintUrl` at both compare + wallet-map sites (their `select.rs` lowercases inconsistently; our dot-segment gaps #59/#76).
- [ ] Notice-taxonomy tail vs Go corpus (below-minimum/below-swap-fee/outcome-unknown landed with #8/#12; remaining: `mint-rate-limited` vs `payment-error-mint-unreachable` split, `payment-error-keyset-expired`).

**Wave 3 — CLI + migration (owner decisions):**

- [ ] PR-11 withdrawal intents (#51). [ ] PR-12 gonuts migration suite
  (#21/#29/#42/#44/#50) — only if upstream needs Go-wallet migration.

**Wave 4 — offered separately / deferred:** reseller contract (post-#7),
MIPS+ipk infra PRs, portal micro-fixes.

**Sunset (per §7 template), after final parity gate.**

## 5b. Gate result and landing (2026-10-28)

**All eight PRs (#8–#15) MERGED to upstream main at 3550101** after the PRTA-first gate: full rust-basic suite (100 files) in the virtualized venue against integration build e40be61 (237/237 offline). Failure taxonomy: 22 failing tests, zero attributable to the PRs — 16× upstream's silent-`Config::default()`-on-load-failure defect (control-proven on main; new PR candidate: fail-closed config), 3× lightning-backoff gap, 1× framework test bug, 2× NUT-24 product gap, 2× CLI protocol gap. Full report: ops#42 comments; run log preserved at /tmp/opencode/reports/prta-run1.log.

**PR-5 disposition (owner decision):** melt-loop port shelved — ecosystem norm is manual withdrawal; ecash-send distribution removes the LN-melt hazard class by construction. Next: withdrawal-intents port + distribution-design research issue.

## 6. Verification and testing plan

Per-PR gates (device-free, tonight's bar): `cargo fmt --check` +
`cargo clippy --all-targets -- -D warnings` + `cargo test` green; new tests
named after the defect they pin; PR body carries an honest "NOT verified"
section (their culture — PR #7 comment: no "tested locally" claims).

Per-wave gates: upstream `ci/conformance_runner.py` against the
OpenTollGate/tollgate corpus (UNVERIFIED is honest, never coerced) + committed
verdict JSON updates when behavior changes.

Final parity gate (hardware — owned by the two device lanes, not this lane):
PRTA `--backend rust-basic` lanes (payment matrix, ln-invoice family,
session-state) on ai-legion labgrid places (coordinator ai-legion:20408,
verified reachable; place allocation per AGENTS.md lab SOP:
CONWRT_LAB + conwrt-bench registry). PRTA checkout carries no hardcoded
amperstrand/opentollgate rust-basic repo URL (verified by grep) — deployment
is artifact/ref-driven, so no PRTA-side URL migration is required; re-verify
at gate time. Crash-consistency scenarios (kill at transaction boundaries,
mint 429/timeout/restart) run as PRTA lanes per the shared conformance matrix
issue — extend there, don't invent new harnesses.

## 7. Process precedent and sunset mechanics

Template extracted from the Go-module consolidation
(OpenTollGate/tollgate-module-basic-go issue #758, executed 2026-10-07;
PR #759; old fork now archived):

1. Land everything landable upstream first (this plan's waves).
2. Migrate every live fork issue upstream with full bodies preserved +
   headers pointing at fork originals for history; disposition closed ones
   explicitly (superseded-by / migrated-via mappings).
3. Close open fork PRs with superseded-by references (our #68 disposition
   folds into upstream PR-2).
4. Push any remaining local-only branches as backup, review stashes for
   value, delete merged branches.
5. Re-point the repo description banner: "SUPERSEDED — single source of
   truth: OpenTollGate/tollgate-module-basic-rust. Do not open issues or PRs
   here."
6. Archive the fork (GitHub archive flag), after the final parity gate.

## 8. Risks and open questions

- **PR #7 merge order** (draft, 61 files, +10.5k, touches pay path + session
  store + config + Cargo): every Wave-1 PR conflicts. Mitigate: PR-1 is a
  small reorder (lands either side); PR-2+ sequence after #7 or rebase on it.
  Watch: no movement since 2026-10-21.
- **#68 decision** (Wave 1): port content into upstream PR-2 vs finish review
  on the dying repo. Recommendation: port — the Amperstrand review queue is
  the one being sunset.
- **Journal storage substrate**: JSONL (ours) vs SQLite (ADR-0001). Port to
  SQLite — their ADR stands per non-goals; write an upstream ADR only if a
  measured case emerges.
- **Single-reviewer bus factor**: all upstream PRs #1–#6 approved by
  c03rad0r; expect serialization.
- **License**: upstream ADR-0005 undecided (declares MIT, no LICENSE file; Go
  is GPL-3.0). Ours is MIT. Not a blocker for porting our own code; flag in
  PR conversations.
- **`upstream keep.d` gap** (their bug, our finding): sysupgrade drops
  `tollgate.db` + wallet `*.sqlite` — pair the NUT-13 PR (PR-8) with a keep.d
  fix PR; upstream inventory §1.
- **Hardware evidence**: this lane is device-free by charter; every PR opens
  with an explicit NOT-verified-on-hardware section until the device lanes
  measure it.
- **Owner decisions needed**: PR-12 (gonuts migration need), MIPS/ipk infra
  PRs (is MIPS an upstream target?), sunset timing.
