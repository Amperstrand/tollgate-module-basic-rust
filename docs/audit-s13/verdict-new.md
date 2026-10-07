# NEW verdicts

Branch `audit/s13-new`, base `0da326a`, worktree `/tmp/opencode/wt-new`. 13 rows (11 owned + 2 jrn-owned classify-only).

| thread_id | PR | path:line@HEAD | sev | theme | disposition | evidence / commit | issue-draft |
|---|---|---|---|---|---|---|---|
| 4181569949 | 54 | src/wireless/upstream_manager/mod.rs:249 (Delivered append), :429 (in-memory apply) | P1 | Delivered journaled but UpstreamSession only in memory → restart/crash loses paid session → double purchase | STILL-ACTIONABLE (design gap) | reproduces at HEAD: `Delivered` appended before `apply_payment`; `current_session` never persisted; `undelivered_tokens` excludes Delivered → next scan re-purchases | DRAFT-A (below) |
| 4181569956 | 54 | src/http/routes/pay.rs:678 (+ main.rs:58,67) | P1 | gate failure during reconcile terminalizes | FIX-TRACKED-BY-JRN | gate-failure family (jrn's 4141963812): pay.rs:678 grant_access failure → TimeoutUnknown (:683); reconciler main.rs:58 warns-and-ignores gate failure then appends terminal ReconcileSpent (:67-74) — both at HEAD, jrn lane owns pay.rs | — (jrn) |
| 4181569959 | 54 | src/wireless/upstream_manager/mod.rs:285 (Ambiguous for ALL send errors) | P2 | all wallet.send errors journaled Ambiguous; definitive failures wedge undelivered_tokens forever | ADDRESSED | 97284c5 — WalletNotFound + Cdk(InsufficientFunds) → Failed{reason} (not blocking); verified from cdk source: InsufficientFunds escapes the purely-local proof-selection in send prepare; red-proven | — |
| 4181569963 | 54 | src/session/mod.rs:250 (contains_key) | P2 | 24h expired-history TTL only enforced on other expiries | ADDRESSED | 1b42095 — session_state checks the stored timestamp against the same cutoff; red-proven | — |
| 4181569967 | 54 | src/wireless/upstream_manager/mod.rs:491 (iw dev <UCI section>) | P2 | stores UCI wifi-iface section as the wireless device → signal polling dead | ADDRESSED | a88a754 — do_monitor resolves netifd l3_device via existing resolve_l3_device (ubus; #52 r4180935919), caches it, falls back to section name; PATH-stubbed red-proven | — |
| 4181569976 | 54 | src/cli/ssl.rs:495 (cert parsed; key existence only) | P2 | ssl apply never validates key or cert↔key match | ADDRESSED | f8304d7 — structural DER pairing check in the hand-rolled x509 module (EC point / RSA n+e; PKCS#8, SEC1, PKCS#1; no new deps, musl-safe) before any file/UCI mutation; unit + end-to-end apply red-proven | — |
| 4186082529 | 55 | src/wireless/upstream_manager/mod.rs:249 | P1 | same as 4181569949 (cross-dupe) | STILL-ACTIONABLE (design gap) | same evidence as 4181569949 | DRAFT-A (one draft covers both) |
| 4186082546 | 55 | src/wireless/upstream_manager/mod.rs:285 | P1 | same as 4181569959 (cross-dupe) | ADDRESSED | 97284c5 (same commit; the #55 body's "no code ever advances it after saga recovery" is also answered: definitive class now Failed/retryable) | — |
| 4186082566 | 55 | src/reseller/upstream_session/mod.rs:149 (parse) / pay.rs:746-765 (emit) | P1 | Rust→Rust grant event lacks expiry → parsed 0 → monitor discards PAID session | ADDRESSED | 01cdb00 — Go's kind-1022 carries NO expiry (verified: p/device-identifier/allotment/metric/start-time only), so emit-side would diverge the wire format; parse derives: metric=milliseconds → start-time+allotment/1000, else start-time+3600 (mirrors pay.rs create_session 3600); explicit tag wins; red-proven | — |
| 4186082579 | 55 | src/http/routes/pay.rs:683 / main.rs:58,67 | P1 | reconciler ignores gate failure, appends terminal ReconcileSpent | FIX-TRACKED-BY-JRN | same gate-failure family as 4181569956 (jrn's 4141963812); both anchors verified at HEAD | — (jrn) |
| 4186082594 | 55 | src/mint_url.rs:26 (rsplit_once(':')) | P2 | IPv6 authority mangled, non-idempotent | WRONG | claimed `http://[::1]` → `http://[:::1]` does NOT reproduce: host:port reassembly is provably identity and the `]` keeps the default-port drop from firing (empirical probe over 9 IPv6 forms, all idempotent; file unchanged since finding commit 9131d102; CDK MintUrl::format_url treats bracketed authorities equivalently — lowercase whole authority, trim slashes). 25bab1f pins the invariant | — |
| 4186780240 | 57 | src/wallet/wallet.rs:650 (rotate_inactive_keyset_proofs) | P2 | rotation swap lacks recover_before_op preflight | ADDRESSED | 10acd24 — same preflight as every money-moving wrapper (`Self::recover_before_op(&w, "rotate", false)`; withdrawal=false: self-swap preserves value); red-proven | — |
| 4186780245 | 57 | src/wallet/wallet.rs:621 (soonest_expiry_keysets collect) | P2 | expiry collector warns on keysets with NO proofs held | ADDRESSED | 7e6fa15 — expiring keyset IDs intersected with keyset IDs present in unspent proofs; red-proven | — |

## Per-thread notes

- **4181569949 / 4186082529**: Real. `journalled_purchase` appends `Delivered` (mod.rs:249 at base) and the grant lives only in `current_session` (applied at :429/`:554` at base); nothing persists it. After a restart the scan loop buys again — paid-once is lost. Persisting grant terms + startup restore is a distributed-state-machine change (what to restore, how it interacts with the #38 journal and gateway switching) → issue draft, not an audit-lane fix.
- **4181569956 / 4186082579**: jrn-owned classify-only. Both reproduce at HEAD at the cited anchors (pay.rs gate-failure → TimeoutUnknown; main.rs reconciler warn-and-ignore then terminal ReconcileSpent). The family is jrn's thread 4141963812 — the JRN lane runs in parallel and owns the pay.rs gate-failure theme.
- **4181569959 / 4186082546**: Fixed in 97284c5. Definitive set is deliberately tiny (WalletNotFound, Cdk(InsufficientFunds)) — both verified to fail before any network call/saga submission. Timeouts/transport/saga-refusals stay Ambiguous (token may exist). Cross-dupe row covered by the same commit.
- **4181569963**: Fixed in 1b42095. Bonus discovery: `src/session/tests.rs` was an ORPHAN module (added in #54 1824720, never declared in mod.rs — none of its 26 tests compiled or ran; #54's "unit tests pin the tri-state lifecycle" claim was never actually executed). The fix commit wires `#[cfg(test)] mod tests;` — flagged as an ownership-region deviation (one wiring line outside the ~251 region, required for the fix's regression test to run at all).
- **4181569967**: Fixed in a88a754. The connector API at HEAD does NOT return a device, but `resolve_l3_device` (ubus network.interface.wwan status → l3_device, from #52) already existed for the gateway-IP path — reused + cached. PATH-stubbed test proves `iw` receives `wwan0`, never the section name.
- **4181569976**: Fixed in f8304d7. No rustls/pki key-parsing API was directly usable without new deps; the hand-rolled structural comparison matches Go's behavior (x509.ParseECPrivateKey also rejects keys without the embedded public point). End-to-end apply test through the existing PATH-stubbed uci harness.
- **4186082566**: Fixed in 01cdb00, at the parse side in `src/reseller/upstream_session` — an ownership deviation (file not on my list, not on the forbidden list either). Chosen because (a) the brief's own hint names "derive at parse" as the sanctioned alternative, and (b) the emit side is pay.rs (forbidden, jrn's) AND would diverge from Go's wire format (Go emits no expiry — verified via OpenTollGate/tollgate-module-basic-go docs/source). step_size left as-is: renewal sizing already falls back to one step at 0.
- **4186082594**: WRONG. The reviewer's mental model ("rsplit treats the final component as a port") is true but harmless: the code reassembles `host:port` verbatim unless a default port matches, and brackets put `]` in the port substring so the match can't fire. Empirical probe (9 forms incl. `http://[::1]`, `[::1]:8338`, `[::1]:80`, zone IDs, paths) — all identity + idempotent; CDK cross-check equivalent for IPv6. 25bab1f pins the invariant so a rewrite can't regress it (test-only; by construction it also passes on the parent — that IS the WRONG evidence).
- **4186780240** (inherited diff): The previous agent's diff was ~90% correct but did not compile — the closure's error type changed to WalletError while the outer match still wrapped `WalletError::Cdk(e)` (E0308). One-line completion + kept its test verbatim (it mirrors `receive_aborts_when_recovery_skips_a_saga`). Completed as 10acd24; nothing of the inherited work was discarded.
- **4186780245**: Fixed in 7e6fa15. Test plants two equally-expiring keysets with self-consistent v1 IDs derived from distinct curve points (add_keys verifies id↔keys, so arbitrary IDs are rejected).

## Crash-window tables (money-code fixes)

### 4186780240 — rotation preflight (10acd24, also in commit body)
| death point | durable state | consequence |
|---|---|---|
| before swap | none added | nothing moved |
| during swap (timeout cancels mid-saga) | CDK saga persists, inputs Reserved | spawn_saga_recovery + next op/boot settles; preflight refuses until settled |
| preflight refuses (recovery failed/skipped) | prior saga intact | no value moved; retry after reconciliation |

### 4181569959 family — send-failure classification (97284c5, also in commit body)
| death point | journal before | after |
|---|---|---|
| send fails definitively (WalletNotFound / InsufficientFunds) | Ambiguous — wedges undelivered_tokens forever | Failed{reason} — nothing moved, repurchase unblocked |
| send fails ambiguously (timeout/transport) | Ambiguous | Ambiguous — still blocks until the send saga reconciles |
| between send and token append | token-created refusal path | unchanged |

### 4186082566 — grant expiry derivation (01cdb00, also in commit body)
Parse-time derivation only; no money moves in the parser. The purchase itself stays under the #38 journalled-purchase contract (intent → token-created(durable) → delivered/failed/ambiguous), unchanged.

## Test-run summaries

- Baseline (inherited wallet.rs diff + one-line compile fix in place, stated): `cargo test` → 307+9+18+7 = **341 passed, 0 failed**.
- Final: `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test` → 345+9+18+7+17+3 = **399 passed, 0 failed**; `cargo test --features embedded-portal` → **417 passed, 0 failed**.
- Net new tests: rotation preflight refusal, expiry-warning intersect, mint_url IPv6/idempotence ×2, session TTL (+26 revived from the orphaned module), ssl pairing ×4 + end-to-end apply rejection, grant-expiry derivation ×3, definitive-send classification, monitor l3_device polling.

## Red-proof outcomes (each: fix committed first, then test spliced into parent 0da326a, run, restored)

| thread | test | parent result |
|---|---|---|
| 4186780240 | rotation_refuses_when_saga_recovery_fails | RED — `Cdk(HttpError(...))` instead of SagaRecovery refusal |
| 4186780245 | expiry_warning_only_names_keysets_with_held_proofs | RED — both keysets flagged (`left: [2 ids]`, `right: [held]`) |
| 4181569963 | session_state_respects_history_ttl_without_later_expiry | RED — stale entry answers `Expired` (test spliced together with the module wiring — test infrastructure the parent lacked; the only behavioral delta is the session_state fix) |
| 4181569976 | apply_real_cert_rejects_mismatched_pair_before_install (behavioral) | RED — parent prints "Done. HTTPS enabled" and installs the mismatched pair; the 4 x509 unit tests are compile-shaped red on the parent (`private_key_matches_certificate` does not exist there) |
| 4186082566 | parse_derives_expiry_… ×2 | RED — expiry stays 0 on both |
| 4181569959 family | definitive_send_failure_does_not_block_repurchase | RED — parent journals tokenless `Ambiguous`, blocking undelivered_tokens |
| 4181569967 | monitor_polls_signal_on_the_resolved_l3_device | RED — `iw` receives `dev wgt0a1b link` |
| 4186082594 | (test-only hardening) | N/A by design — the tests pass on the parent, which is the WRONG evidence itself |

## Inherited diff disposition

The uncommitted `src/wallet/wallet.rs` (+114/-5) was the previous agent's 4186780240 fix: correct approach (mirrors the other wrappers' preflight), correct test (mirrors `receive_aborts_when_recovery_skips_a_saga`), but ONE compile error (`Err(WalletError::Cdk(e))` with `e: WalletError` after the closure's error type change). Verified by `cargo check` (E0308), fixed with a one-line edit, test run green, committed as 10acd24 with the crash-window table. Nothing discarded.

## Deviations from the brief

1. **session/mod.rs wiring line** (1b42095): `#[cfg(test)] mod tests;` sits at the top of the file, outside the ~251 region I own. Required — the regression test (and 26 pre-existing tests) were dead without it; flagged in the commit and here.
2. **reseller/upstream_session parse fix** (01cdb00): file not on my ownership list (also not on the forbidden list). The brief's per-thread hint explicitly offers "OR derive at parse" as the fix shape; the emit-side alternative is in forbidden pay.rs and would diverge from Go's wire format. Flagged.
3. **style(session) ca39357**: two-line fmt of pre-existing tests in the newly-wired module (the lines only became fmt-visible once the module compiled).

## READY-TO-PASTE issue drafts

### DRAFT-A — Delivered-persistence family (4181569949 + 4186082529)

**Title:** Reseller upstream: persist granted session terms before `Delivered` becomes terminal, restore at startup

**Severity:** P1 (fund safety — double purchase after restart)

**Where (at 0da326a):**
- `Delivered` append: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/wireless/upstream_manager/mod.rs#L249
- in-memory-only apply: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/wireless/upstream_manager/mod.rs#L429 (renewal path: #L554)
- Codex threads: https://github.com/Amperstrand/tollgate-module-basic-rust/pull/54#discussion_r4181569949 and https://github.com/Amperstrand/tollgate-module-basic-rust/pull/55#discussion_r4186082529

**Reproduction sketch:**
1. Enable `reseller_mode`; manager connects to a gateway and completes `journalled_purchase` — journal reaches `Delivered`, `apply_payment` fills `current_session` (in memory only).
2. Restart the process (or crash between the `Delivered` append and `apply_payment`).
3. Next scan cycle: `undelivered_tokens` excludes `Delivered`, no persisted grant exists → the manager purchases a NEW session. The router paid twice for one access grant.

**Why not fixed in the audit lane:** restoring a grant is a distributed-state-machine change — it must decide what is restored (gateway identity? expired grants?), how restoration interacts with the #38 journal's `Delivered` terminal, gateway switching/blacklisting, and the renewal sizing — exactly the "no large money-flow redesigns" class the audit contract excludes.

**Suggested acceptance criteria:**
- Grant terms (gateway id, allotment, metric, step_size, expiry, used) are durable BEFORE the `Delivered` journal append is treated as terminal (write-before-terminal, same pattern as token-created-before-delivery).
- Startup restores a still-valid granted session from the durable record and skips re-purchase for it; an expired/restored-and-consumed grant is discarded exactly once.
- Crash-window table in the fixing commit covers: after send before delivery, during delivery, after delivered before apply, after apply before persist.
- Test kills/restarts the process between the `Delivered` append and `apply_payment` and asserts no second `wallet.send` occurs on the next cycle (PRTA `--backend rust-basic` lane).
