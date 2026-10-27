# Upstream Capability Inventory — OpenTollGate/tollgate-module-basic-rust @ 19ae887 (main)

Purpose: evidence-backed inventory for planning the Amperstrand port as small reviewable PRs.
All paths are relative to the repo root. MAC addresses and bearer tokens are elided per rules.
Where a capability does not exist, the answer is **NOT FOUND** (with the closest evidence).

---

## 1. Session persistence

**Model: SQLite, WAL, `synchronous=FULL`, explicit columns, `PRAGMA user_version` schema stamping, fail-closed.**

- `src/session/store.rs:110-113` — pragmas applied at open, before any write:
  ```rust
  conn.pragma_update(None, "journal_mode", "WAL")?;
  conn.pragma_update(None, "synchronous", "FULL")?;
  ```
- Schema (`store.rs:131-161`): `SCHEMA_VERSION = 1`; tables `sessions(mac PK, allotment, used, metric, expiry, granted_at)`, `closed_usage(mac PK, used, closed_at)`, `quotes(quote_id PK, amount, unit, state, created_at, expires_at, request)`. Unknown `user_version` → `StoreError::UnknownSchema` → refuse to start (`store.rs:124-126`).
- Fail-closed: corrupt/not-a-database → `StoreError::Corrupt`; `src/main.rs:117-123` exits **78 (EX_CONFIG)**:
  ```rust
  // FAIL CLOSED: an unreadable store must never be read as "no sessions"
  std::process::exit(78); // EX_CONFIG
  ```
- **Write relative to payment grant**: `SessionManager::create_session` (`src/session/mod.rs:175-187`) upserts the row synchronously *before* the handler builds the 200/kind-1022 response (`src/http/routes/pay.rs:396-422`). On persist error the session is removed from memory ("a session we could not persist is a session we do not have", `mod.rs:177-186`). ⚠️ Caveat: `handle_pay` ignores the return value (`let _session = ...`, pay.rs:397) and still responds 200 with the original allotment — a persist failure therefore yields value received + no recorded session + success advertised to the client.
- **Restart recovery** (`mod.rs:101-134`): sessions expired while down are closed (deleted, not revived) with final usage recorded to `closed_usage`; live sessions reloaded with `used` intact; unpaid quotes past deadline marked `expired` (never deleted — "a paid one must stay redeemable"). Rationale documented in ADR-0001 (`docs/adr/0001-durable-state-wal-full.md`).
- Usage counter: ADR-0002 batching — flush at 5 s OR 5 % of allotment OR terminal transition (`mod.rs:75-77`, `update_usage` `mod.rs:327-361`); exact flush at close (`store.rs:303-314`, `flush_usage` `mod.rs:373-393`).
- File: `${TOLLGATE_STATE_DB:-/etc/tollgate/tollgate.db}` (`main.rs:103-109`) — same volume as wallet DBs.
- ⚠️ Packaging gap: `openwrt/files/lib/upgrade/keep.d/tollgate` preserves `config.json`, `identities.json`, `wallet_seed.bin`, `install.json`, `ecash/` — but **not** `tollgate.db` or the per-mint wallet `*.sqlite` files that live in `/etc/tollgate/` (see §3). A sysupgrade drops paid-session state and wallet proofs.

## 2. Pay path (`POST /`)

Order in `handle_pay` (`src/http/routes/pay.rs:137-431`):

1. **Body cap**: >1 MiB → 413 + notice (`pay.rs:151-170`). ⚠️ Check runs on the already-extracted `body: String`; the axum extractor is unbounded, so the bytes are buffered before the refusal (the code comment admits this: "The axum `String` extractor is unbounded", pay.rs:150-151).
2. **Content-Type**: absent or `text/plain` → raw token; `application/json` → kind-21000 event, tag `["payment", <token>]` (`pay.rs:178-210`); other types → 415.
3. **Token parse + mint acceptance BEFORE any network call** (A6/A8): `verifier_token_mint` parses the token's mint URL, then `select_mint_for_token` allowlist + price-unit check (`pay.rs:217-238`, `src/http/routes/select.rs:35-70`). Rejection code `payment-error-mint-not-accepted`.
4. **NUT-07 checkstate** via `TokenVerifier::verify` (`src/wallet/verify.rs:35-97`; POST `{mint}/v1/checkstate`, all Ys must be `UNSPENT`). Failure → 402 `payment-error-token-spent`.
5. **`wallet.receive()`** (value moves here) (`pay.rs:270-312`), under the global wallet mutex.
6. **Client MAC resolution** — *after* value moved (`pay.rs:314-341`): socket peer (ADR-0007) → dnsmasq lease → ARP (`src/http/mac/mod.rs:48-93`). Failure → 400 `session-error` with the token already received.
7. **Pricing / minimum purchase** — *after* value moved (`pay.rs:343-389`): `calculate_allotment` = `(amount_sats / price_per_step) * step_size` (`pay.rs:88-104`); a payment under one step → 402 (mis-keyed as `payment-error-token-spent`, pay.rs:356-359). `purchase_min_steps` is parsed (`src/config/schema.rs:144`) but **NOT enforced in /pay** — it is only floored to ≥1 in the discovery advertisement (`src/http/routes/discovery.rs:32-45`).
8. **Session create** (durable, §1), then signed kind-1022 response (TIP-01 tags `device-identifier`, `allotment`, `metric`, `start-time`; `pay.rs:412-422`).

**Validation after value moves: YES** — steps 6 and 7 both run after `wallet.receive()`. A token can be consumed with no session granted (unresolvable MAC; sub-step payment).

**Spending-condition (P2PK/HTLC) rejection: NOT FOUND.** `grep -rni 'p2pk|hctl|spending.condition|witness' src/` returns nothing (only bip39 wordlist noise). Tokens with spending conditions are not screened.

**Duplicate-POST idempotency: NOT FOUND** (no token-hash keying, no payment journal). The only serialization is the wallet mutex around `receive` (pay.rs:270); a replay relies on the mint's NUT-07 spent-check failing at step 4. The `/ln-invoice` lane *does* have an idempotency token (§4) but `/pay` has none.

Quirk worth knowing: `create_session`'s 4th parameter is `duration_secs`, and `/pay` passes `start_time` (unix now) — `pay.rs:391-402` vs `mod.rs:164` — so `expiry ≈ 2 × unix_now`: sessions effectively never time-expire and end only by exhaustion/revoke. `/ln-invoice` grants pass a bounded TTL instead (`ln_invoice.rs:462-473`).

## 3. Wallet integration

- **Construction/keying**: `TollWallet` holds `HashMap<String /*normalized mint URL*/, Arc<Mutex<cdk::Wallet>>>` — one CDK wallet per mint, unit fixed to `CurrencyUnit::Sat` (`src/wallet/wallet.rs:62-67, 109-115`). Not per-unit.
- **Canonicalization**: raw `String` + `trim_end_matches('/')` only (`wallet.rs:95, 159`). **Not** the `MintUrl` type. Inconsistent with `select.rs:30-32`, which also lowercases. Two spellings that differ only by case would select correctly but fail `wallets.get()` at receive (edge case).
- **One-wallet-per-mint**: `ensure_mint` inserts keyed by normalized URL; duplicate calls are no-ops (`wallet.rs:90-132`). Per-mint SQLite file: `db_path_for_mint` sanitizes non-`[A-Za-z0-9.-]` to `_` (`wallet.rs:134-146`), i.e. `/etc/tollgate/https___mint.example.com.sqlite`.
- **Timeouts**: every op (receive/send/melt/quotes) wrapped in `tokio::time::timeout(30s)` (`OP_TIMEOUT`, `wallet.rs:38, 167-171` …). Verifier HTTP client: 30 s, keep-alives disabled (`verify.rs:10-16`).
- **`recover_incomplete_sagas`**: called once per mint at `ensure_mint` — i.e., at startup registration (`wallet.rs:117-127`), logging recovered/compensated/skipped/failed. Not re-run periodically.
- Seed: 64 random bytes in `wallet_seed.bin` mode 0600 (`wallet.rs:393-416`). No mnemonic for the wallet (see §9).

## 4. Lightning quotes (`/ln-invoice`)

- **Implemented, not a stub** — README.md:262-263 is stale; `src/http/routes/ln_invoice.rs` does real `mint_quote` (POST, ln_invoice.rs:206-250), mint-state polling (ln_invoice.rs:302-325), `mint_tokens` + session grant + `ndsctl auth` (ln_invoice.rs:405-505).
- **Quote store is IN-MEMORY in production**: `AppState.quotes` is `Arc<Mutex<MintQuoteRegistry>>` and `MintQuoteRegistry` is a `HashMap` (`src/http/routes/quote.rs:67-106`; wired in `main.rs:125-127`). The durable `quotes` table (§1) exists with `upsert_quote/get_quote/set_quote_state`, but grep shows `upsert_quote` is called only from `src/session/store_test.rs:200` — **no production writer**. The boot-time `expire_unpaid_quotes` therefore sweeps a table nothing populates.
- **Restart mid-quote**: registry empty → `GET /ln-invoice?quote=` → 404 `"unknown quote"` (ln_invoice.rs:283-285). If the customer had paid the BOLT11, the sats sit at the mint unredeemed by this module (mint_quote id lost); there is no reconciliation path. ADR-0001 explicitly names "Quote lost → a paid invoice can never be redeemed … money loss" as the motivation — the durable table was built (P1) but the handler was never switched onto it.
- Idempotency *within* a process lifetime is solid: `poll_grant_decision` (pure, tested; ln_invoice.rs:33-66) + `Issued` mark prevents re-grant (fixed the measured 1000→12000 re-poll bug, ln_invoice.rs:525-533).

## 5. Gonuts migration

- **`MIGRATION.md`** (438 lines): operator runbook — pre-migration backup/balance check, the swap-counter race explanation, automated first-boot export, manual `migrate` CLI, brick detection, troubleshooting.
- **`tools/gonuts-export/main.go`**: Go tool reading bbolt `wallet.db` buckets `keysets`/`proofs`/`seed` (matching gonuts `wallet/storage/bolt.go`) and emitting one Cashu token per keyset batch to `tokens.jsonl` (`main.go:15-46`).
- **First boot** (`main.rs:46-81`): if `wallet.db` exists && `wallet.sqlite` absent && `.migration_complete` absent → run `gonuts-export` (path overridable via `GONUTS_EXPORT_PATH`). On success it only *logs* "run 'migrate <path>' via CLI to import" — import is manual. The comment block at `main.rs:39-41` claims marker write + rename as steps 3-4, but the code does neither; `MIGRATION.md:240-248` makes those explicit **manual operator steps** (`touch .migration_complete`, `mv wallet.db wallet.db.pre-migration`).
- **Failure handling**: export failure/missing binary → warn, start with empty wallet (`main.rs:65-80`). Per-token import failures are **retained**: failed lines stay in `tokens.jsonl`, counted in the report (`{total, imported, failed, errors[≤10]}`, `src/cli/mod.rs:224-249`); nothing is deleted, so re-running `migrate` is possible. Value-retention therefore depends on the operator keeping the file — the module never removes it.

## 6. Payout / split / operator share

**NOT FOUND — no execution path.** `profit_share: Vec<ProfitShareConfig>` and per-mint `payout_interval_seconds`/`min_payout_amount` are parsed and validated (`src/config/schema.rs:19, 137-139, 110-127`; factors must sum to 1.0 ±1e-6) but grep shows no payout loop, no `send()` caller in production code (`TollWallet::send` exists at `wallet.rs:185-216` with zero call sites), and no Nostr split. The residual-fraction warning (`schema.rs:115-119`) even describes money that "will remain in wallet each payout cycle" — a cycle that does not exist in this binary.

## 7. Error / notice semantics

- Failure responses are **signed kind-21023 Nostr notice events (TIP-01)** with `level` (`error|warning|info|debug`, coerced) and machine-readable `code` tags (`src/http/routes/pay.rs:43-76`).
- Notice code vocabulary observed: `payment-error-token-spent`, `payment-error-mint-not-accepted`, `payment-processing-failed`, `session-error` (pay.rs), plus ticket-route codes `device-unresolved`, `no-session`, `ticket-issued`, `ticket-invalid`, `ticket-error`, `attachment-active`, `rebind-error`, `invalid-request` (`session_ticket_route.rs:66-219`). The doc comment (pay.rs:47-49) also mentions `upstream-error-not-connected` as TIP-01 vocabulary; it is not emitted in this codebase.
- Taxonomy caveats: `payment-error-token-spent` is reused for non-spend failures (invalid kind-21000 JSON pay.rs:186-190; sub-step payment pay.rs:356-359), and body-too-large uses `payment-processing-failed` with 413.
- Other lanes use the corpus envelope `{status: 0|1, error|...}` (ln_invoice.rs:127-162) or `{status, code, error}` (ticket routes).
- **Go-parity claims**: extensive and measured — route-by-route hardware probes comparing Rust vs Go responses (PR #4 comments), Go-vector tests for identity derivation (`identity_derive.rs:305-346` "bit-for-bit"), ticket wire "must match Go byte-for-byte" (`session_ticket.rs:8-14`), rebind collapse of ticket failures to Go's single 403 `ticket-invalid` (`session_ticket_route.rs:154-166`).

## 8. PR #4 and PR #6 routes

**PR #4 — "feat: Go-parity routes — session-state, tickets, rebind, identity"** (merged 2026-10-20, +3277/−7, 17 files):

- `GET /session-state` (`src/http/routes/session_state.rs`): `{status: 1, mac, state}` where state ∈ `none|active|expired` — distinguishes "never paid" from "expired", which `/usage`'s `-1/-1` cannot. Unresolvable client → `mac: ""`, `state: "none"` (never a sentinel MAC).
- `POST /session/ticket` (`src/http/routes/session_ticket_route.rs:56-115`): 404 `no-session` if the socket-resolved client has no session; else issues an envelope. **Wire format** (`src/session_ticket.rs:11-21`): `v1 . base64url(JSON{handle,iat,exp}) . base64url(HMAC-SHA256(body))`, key from crypto/rand at startup, **memory-only** (restart invalidates all tickets by design; TTL 12 h, max 4096 attachments).
- `POST /session/rebind` (`session_ticket_route.rs:122-223`): body `{ticket}`; all four ticket failures collapse to 403 `ticket-invalid` (Go parity); success moves the session row (allotment, usage, expiry) from old MAC to the caller's resolved MAC via `SessionManager::move_session` (persisted, `session/mod.rs:205-218`) and answers `{status, mac, usage: "used/total"}`. 409 `attachment-active` if the previous address still has a live session (nodogsplash consult recorded as a hardware follow-up, `session_ticket_route.rs:187-189`).
- `GET /identity` (`src/http/routes/identity.rs:30-52`): derives `npub`, CGNAT `ipv4`, per-interface `macs` from `identities.json` owned key; routes 404-disabled if key absent.

**PR #6 — "feat: NIP-06 mnemonic recovery for /identity/reveal-seed"** (merged 2026-10-20, +237/−14, 3 files):

- `POST /identity/reveal-seed` (`identity.rs:57-109`): **loopback-only** (403 otherwise). Body = 12-word BIP39 mnemonic (or 64-hex key). NIP-06 derivation BIP39→BIP32 `m/44'/1237'/0'/0/0` (`src/identity_derive.rs:216-241`, PBKDF2-HMAC-SHA512 2048 iters), Go-vector bit-for-bit test (`identity_derive.rs:305-346`). Response echoes `{npub, ipv4, macs, mnemonic, privatekey, root_password, wifi_password}` — recovery material, hence loopback-only + POST-signals-intent.

## 9. Keyset hygiene, seed backup/restore, withdrawal CLI, reseller path

- **Keyset expiry warnings / rotation: NOT FOUND.** No code touches keyset lifecycle beyond migration export.
- **Seed backup/restore**: wallet seed is a raw 64-byte `wallet_seed.bin` (`wallet.rs:393-416`). **NUT-13: NOT FOUND.** No wallet mnemonic; no `wallet.restore()` call anywhere. The only BIP39/NIP-06 code is for the *identity* key (§8), not the wallet. Consequence: losing `/etc/tollgate/*.sqlite` (e.g. sysupgrade, §1) loses proofs with no restore backstop.
- **Withdrawal / fund / drain CLI ops: NOT FOUND.** CLI is exactly `version`, `status`, `wallet info`, `wallet balance`, `migrate <path>` (`src/cli/mod.rs:101-198`).
- **Reseller / upstream purchase path: NOT FOUND.** `reseller_mode: bool` parsed (`schema.rs:32`) and used nowhere; the large `upstream_detector`/`upstream_session_manager`/`upstream_wifi` config blocks (schema.rs:183-433) are parsed for Go config compatibility but no runtime behavior is attached. (Open PR #7, §15, is the first upstream-purchase machinery.)

## 10. Metering / usage model + gate lifecycle

- **nodogsplash via `ndsctl`** (no nftables anywhere). Usage = `ndsctl json <mac>` flat object, `downloaded+uploaded` (`src/metering/mod.rs:56-114`); `{}` is treated as **absence, not zero** (silent-zero bug class documented in the module header). Measured against nodogsplash 5.0.2 on hardware.
- Counter model: in-memory `used` updated from live readings; persisted batched per ADR-0002 (§1); authoritative source is ndsctl, store is a cache.
- Gate open: `ndsctl auth` with `classify_gate_output` — exit-0 silence is NOT success; `UnknownClient` distinct from `Refused` (`mod.rs:246-312`).
- **Close-on-exhaustion**: 30 s sweep in `main.rs:192-276` — sync live counters in first (STEP 2/3), then `terminal_sessions` (expired OR `used >= allotment`, `session/mod.rs:23-35`), exact final-usage flush, revoke, then `ndsctl deauth` outside the lock. `classify_gate_close` treats "not found" as success (`mod.rs:319-331`).
- **Boot continuity**: at startup, every still-valid session gets `deauth` (clear stale) then `auth` (ADR-0006 §5; `main.rs:152-186`); expired-while-down sessions are closed by `SessionManager::open` and deliberately NOT re-authed. Measured: `conformance/boot-continuity-measured.json` (SIGKILL → restart → gate restored).
- Rate of exhaustion detection: bounded by the 30 s sweep interval (operator-accepted overrun, ADR-0006 §4).

## 11. Rate limiting, body caps, DoS protections

- **Rate limiting: NOT FOUND** (no tower limit layer, no governor; grep for `rate.?limit|governor|tower::limit` empty).
- Body caps: `/pay` 1 MiB ceiling — but enforced *after* unbounded `String` buffering (§2). `/ln-invoice` and `/session/rebind` rely on axum defaults (`axum::Json` ~2 MiB; `Bytes` unbounded for rebind).
- **No global HTTP TimeoutLayer** despite `tower-http` `timeout` feature being enabled in `Cargo.toml:28`; the only timeouts are the 30 s wallet-op wrappers and the verifier client.
- CORS: `allow_origin/methods/headers(Any)` (`src/http/mod.rs:51-54`). Server binds `0.0.0.0:2121` (overridable `TOLLGATE_BIND`, `main.rs:138`). Other DoS surface: `/pay` performs a network NUT-07 round trip per request (bounded 30 s) — cheap-check-first ordering (§2 step 3) at least refuses unknown mints pre-network.

## 12. Build / targets / packaging / CI

- Targets: `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` (musl-gcc, `.cargo/config.toml`), `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf` (via `cross`/Docker — ring's asm needs real musl toolchains; `cross-compile.yml:37-77`). **MIPS: NOT FOUND as a build lane.** Only trace is `PKG_USE_MIPS16:=0` in `openwrt/Makefile:31` and a musl.cc-toolchain explainer for aarch64 (`ci/cross-build-aarch64.sh:1-25`). No 32-bit-atomics workarounds (portable-atomic etc.).
- Size discipline: `[profile.release] panic="abort", strip, opt-level="z", lto, codegen-units=1` (`Cargo.toml:66-72`); baseline doc `docs/binary-size-baseline.md`; CI prints stripped sizes per target.
- Packaging: OpenWrt feed Makefile `openwrt/Makefile` using the **prebuilt-binary pattern** (CI stages the artifact; SDK only packages; installs `/usr/bin/tollgate-module-basic-rust`, `DEPENDS:=+nodogsplash +jq`), procd init `openwrt/files/etc/init.d/tollgate-wrt` (respawn, reuses Go's `tollgate-wrt` service slot), sysupgrade keep-list (see §1 gap), captive-portal site pages. CI does **not** build the ipk — cross-compile workflow uploads staged binaries as artifacts only.
- Workflows: `.github/workflows/ci.yml` (fmt + clippy `-D warnings` + test; gnu+musl builds) and `cross-compile.yml` (same checks + 3-target cross builds + artifact staging).

## 13. Test inventory

- **194 `#[test]`/`#[tokio::test]` functions** in `src/` (grep count; README's "58 unit tests" is stale). Kinds: unit (parsers, decisions), handler/contract tests driving the real router via `oneshot`/`MockConnectInfo` (e.g. `pay_body_test.rs`, `parity_routes_test.rs`, `balance_contract_test.rs`, `ln_invoice_contract_test.rs`), Go-vector parity tests (`identity_derive.rs`), regression tests named after measured bugs. No `tests/` integration directory on main (PR #7 adds one).
- Mutation-checking culture: REPORT.md documents reverting implementations to prove tests fail ("reverting the formula fails 4 tests").
- **Conformance tooling** (`ci/`): `conformance_runner.py` — drives the real binary over HTTP against the external `OpenTollGate/tollgate` corpus manifest; records PASS/FAIL/**UNVERIFIED**/ERROR per fixture; UNVERIFIED is an honest outcome, never coerced (runner docstring; ADR-0003 shells out to `cdk-cli` for V4 CBOR decoding, `nut00.py` implements hash_to_curve for NUT-07 Ys). `mint_probe.py` + `test_mint_probe.py` (live-mint lane), `hardware_roundtrip.py` (one-command bench lane: pay → gate → bytes counted → `/usage`; bench mutex as hard gate; vantage/portal-registration requirements annotated).
- **Verdicts are committed JSON** in `conformance/`: `verdicts.json` (CI lane: 15 PASS / 12 UNVERIFIED / 0 FAIL, 2026-10-20), `verdicts-hardware-a04e.json` (17 PASS / 10 UNVERIFIED on-device), plus per-topic evidence files (`adr-0006-close-measured.json`, `adr-0007-identity-measured.json`, `boot-continuity-measured.json`, `hardware-grant-e2e-a04e.json`, `metering-*-measured.json`).

## 14. Docs / conventions

**ADRs** (`docs/adr/`, binding-until-superseded per README.md there):

| ADR | One-line decision |
|---|---|
| 0001 | Durable state = one SQLite file, WAL + `synchronous=FULL`, explicit columns + `user_version`, fail-closed exit 78 |
| 0002 | Usage counter writes batched (5 s / 5 % / terminal); money-grade fsync reserved for money state |
| 0003 | Conformance runner decodes V4/CBOR by shelling out to `cdk-cli`, not a hand-rolled decoder |
| 0004 | Corpus fixtures must come from a reachable mint and be regenerable by script (KG twins were permanently unverifiable) |
| 0005 | Licence deliberately undecided (Go is GPL-3.0, repo declares MIT with no LICENSE file; open) |
| 0006 | Close the gate: 30 s sweep + deauth + boot-time re-auth continuity (measured on hardware) |
| 0007 | Client identity is the TCP socket, not caller-writable headers; `identity_from_socket` config escape hatch (default true) |

**README/PROGRESS/REPORT claims**: README is stale in places (58 tests, `/ln-invoice` "stub", `whoami` placeholder — code has moved past all three). `PROGRESS.md` is a one-line-per-finding ledger (A1–A9, P1–P7d) naming file touches; `REPORT.md` is the evidence-first Path A report with an explicit "What is NOT verified" section. `docs/architecture.md` still says "Sessions are in-memory only" (line 157) — superseded by ADR-0001/P1.

**Inferable conventions**: test-first (RED→GREEN + mutation checks); every claim measured on named hardware with committed JSON evidence; ADRs for expensive-to-reverse decisions with costs recorded; external corpus as the arbiter of conformance; honest UNVERIFIED over green-washing; bearer-token hygiene (split string literals to dodge the `detect_wallet_secrets` pre-commit gate, `verify.rs:136-150`); bench mutex discipline; small titled PRs (`feat:`/`fix:`/`docs:`/`chore:`); Go-parity wire compatibility is a hard requirement.

## 15. Open PR #7 — "spilman channel payment rail"

`gh pr view 7`: **OPEN**, author felixfelix-bot, branch `pr/spilman-channel-rail`, created 2026-10-21, **+10 550 / −112 across 61 files**, **no formal reviews yet** (0 review records).

- **What it adds**: a Spilman-channel wholesale payment rail so the router buys upstream access **without a mint round trip per purchase** (mint touched only at channel open/settle), behind a config flag defaulting off. Components: `PaymentRail` seam + `NoopRail` (`src/spilman/mod.rs`), exact billing/pricing constants (`billing.rs`, `payment_units.rs`, `pricing.rs` — including a corrected 22 021 bytes/msat parity rate after finding the documented 22 020 overcharges ~1 msat/step), durable CAS ledger (`ledger.rs`), payer lifecycle with crash-recovery (`payer.rs`, two-phase `OpeningFromSwap` persisted before funding swap), payee host/mint-http/registration, plus `src/http/routes/spilman.rs` mounted on the router and session channel-credit/binding support.
- **Dependency blast radius**: moves the whole stack (`cdk`, `cdk-sqlite`, `cashu`) from crates.io to **git tag v0.18.1** to unify with `cdk-spilman` (two-`cashu`-instances type clash; claims all 194 pre-existing tests stay green).
- **Files/areas touched (conflict surface)**: `Cargo.toml`/`Cargo.lock`; `src/http/mod.rs` (route mount); `src/http/routes/{mod,pay,ln_invoice,session_state,identity,balance,usage}.rs`; `src/session/{mod,store}.rs` (+ new `channel_credit.rs`); `src/config/schema.rs`; `src/main.rs`, `src/cli/mod.rs`, `src/lib.rs`, `src/identity_derive.rs`; ~15 new `src/spilman/*` files; 10 new `tests/*.rs` integration tests; docs + examples + scripts. Any incoming PR touching pay path, session store, config schema, or Cargo deps will conflict.
- **Review state**: no GitHub reviews; the author ran a self-commissioned **cross-family (deepseek) cold review** whose verdict was "CHANGES REQUIRED — 8 findings, 3 fixed, 5 open" (PR comments), plus on-router evidence (A04E bench, sha256-verified binaries, 14/14 hardware lane) and an un-claimed determinism question on channel ids (unsound assertion removed).
- **Overlap with payments/payout**: **yes — directly.** It is payment machinery (billing arithmetic, durable ledger, channel open/settle, recovery), but it does *not* implement profit-share/nostr-split payout (§6 remains absent) and it leaves retail `handle_pay` behavior unchanged.

## 16. Review conventions (merged PRs #1–#6)

- **All six approved by the same reviewer: `c03rad0r`** (APPROVED on #1–#6; author felixfelix-bot throughout). No other reviewers, no requested-changes records — approvals are terse; the substantive demands live in PR comment threads and the code/docs themselves.
- **What reviewers demanded (evidenced in threads)**: measured-on-hardware claims rather than "tested locally" (PR #7: "Draft PRs get a lot of 'tested locally' claims. This one ran on the A04E bench router … sha256 verified on the device"); explicit "what does NOT work / NOT verified" sections; removal of unsound assertions (PR #7 determinism claim); divergence tables Rust-vs-Go with fixes landed in-PR (PR #4 comment: rebind wire and identities.json fork "were ours to fix"); cross-family independent review for critical payment code; honest framing ("this does not eliminate the round trip, it amortises it").
- **Typical merged PR size**: #1 initial import (93 files, +16 889) is the outlier; everything after is small and focused — #4 +3 277/17 files, #6 +237/3, #5 +34/2, #3 +6/1, #2 −17/1. House style: single-purpose PRs, ledger updates in PROGRESS.md, ADRs when a decision is expensive to reverse, committed conformance JSON when behavior changes.

## 17. Closing assessment

**Upstream STRONG (Amperstrand should adopt as-is, not port over):**

- **Durable session store design** (ADR-0001/0002): WAL+FULL, fail-closed exit 78, schema versioning, closed-usage audit trail, batched usage flushes — a clean, documented model to converge on.
- **Gate lifecycle** (ADR-0006): measured ndsctl auth/deauth classification, 30 s sweep with live-counter sync, boot continuity re-auth — all hardware-measured with committed evidence.
- **Identity model** (ADR-0007): socket-not-headers identity, lease/ARP resolution, response headers that expose ignored claims; plus the Go-parity `/session-state`, ticket/rebind routes and loopback-only NIP-06 reveal-seed.
- **Evidence infrastructure**: conformance runner against the external corpus with honest UNVERIFIED, one-command hardware round-trip lane, committed verdict JSONs, mutation-check discipline. Adopt the tooling and the culture.
- **Pay-path ordering discipline** (allowlist/unit before network), price-table allotment (A1), signed 1022/21023 events, `poll_grant_decision` idempotency for /ln-invoice, extend-not-replace sessions.
- **Build/packaging baseline**: musl static lanes, size discipline, procd init, corpus-driven wire shapes.

**Upstream THIN (port candidates from Amperstrand):**

- **Payment journal / duplicate-POST idempotency for `/pay`** — no token-hash keying exists at all (§2).
- **Validate-and-price-before-receive**: MAC resolution and minimum-purchase run *after* `wallet.receive()`; `purchase_min_steps` unenforced; persist-failure still returns 200 (§1, §2).
- **Durable ln-quote store wiring**: the durable `quotes` table exists but is unwired; restart mid-quote strands a paid invoice (§4) — Amperstrand's durable quote store + startup reconciliation ports directly.
- **Payout journal / profit-share / nostr split execution**: config-only today (§6).
- **Keyset hygiene (expiry/rotation) and NUT-13 seed-restore backstop**: absent; wallet seed is a raw bin with no restore path (§9) — critical given the keep.d sysupgrade gap (§1).
- **Withdrawal intents / drain CLI / reseller durable contract**: absent; `reseller_mode` and upstream config blocks are dead weight (§9). Note PR #7 will land upstream-purchase machinery — coordinate.
- **Rate limiting + pre-buffer body caps + global HTTP timeout**: absent (§11).
- **P2PK/HTLC spending-condition rejection** on receive: absent (§2).
- **MIPS32 / no-64-bit-atomics lane**: no MIPS target at all (§12) — Amperstrand's fix has no upstream equivalent to conflict with.

**Coordination note**: PR #7's git-source dependency unification (+10.5k lines touching the pay path, session store, and config schema) is the main merge-order hazard for any incoming PR in those areas; landing small PRs before or cleanly rebased on it will decide reviewability.
