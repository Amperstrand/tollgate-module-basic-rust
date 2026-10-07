# JRN verdicts

Worktree: `/tmp/opencode/wt-jrn` (branch `audit/s13-jrn`, base `0da326a`)
Commits: `74e187d` (wallet-init retry), `0a1d02b` (terminal-append failure).

| thread_id | PR | path:line@HEAD | sev | theme | disposition | evidence / commit | issue-draft |
|---|---|---|---|---|---|---|---|
| 4140359052 | 39 | src/wallet/wallet.rs:328-329 (also 915-916, 1037-1038, 1070-1071) | P1 | per-mint lock wait fused into receive()'s 30s op timeout → false "unknown, don't resubmit" | STILL-ACTIONABLE | verified at HEAD: `wallet.lock().await` inside `timeout(OP_TIMEOUT)` in receive/send/mint_tokens/melt; `RECOVERY_TIMEOUT`=120s (L82) > `OP_TIMEOUT`=30s (L75) | draft A (lock-wait vs op-timeout) |
| 4152466921 | 42 | (covered at call sites) src/cli/mod.rs:118-146, src/cli/jsonproto.rs:283-340, src/payout.rs:569-620, src/wireless/upstream_manager/mod.rs:200-245 | P1 | timed-out send/melt retried without reconciling earlier withdrawal | ALREADY-FIXED | `08ebb00` (#51): drain consults `mint_has_unresolved_send` + payout-journal intent before re-issuing (src/cli/mod.rs:130-146), jsonproto drain journals intent/TokenCreated; corroborated by `5e8ebb1` (#45: payout melts journal Ambiguous, never re-melt same invoice) and `df1dbd7` (#38: reseller send → TokenCreated durable before delivery) — every send/melt call site at HEAD checks a durable intent/outcome first | none (residual note below) |
| 4141963797 | 43 | src/main.rs:6-52 (was main.rs:20@1090dca6) | P1 | boot ensure_mint failure → reconciler WalletNotFound forever | STILL-ACTIONABLE → FIXED THIS AUDIT | `74e187d`: pass retries `ensure_mint` for every undecided entry's persisted mint (bounded 1/pass) before proofs are queried; MintNotAccepted surfaced loudly. Partial pre-existing mitigation: 30s mint-retry loop (`df1dbd7`) covers only currently-configured mints and skips its first tick to t+30s | none |
| 4141963803 | 43 | src/main.rs:138 `create_session` in apply_reconciled_grant (was main.rs:29@1090dca6) | P1 | two undecided same-MAC payments in one pass → create_session overwrite loses all but last grant | STILL-ACTIONABLE | overwrite semantics verified at HEAD (both entries still terminalize ReconcileSpent; only last hash-map-ordered grant survives); also clobbers live-path add_allotment accumulations | draft B (additive grants + durable grant id) |
| 4141963812 | 43 | live path FIXED at src/http/routes/pay.rs:678-729; residual: src/main.rs:148-150+terminal append, pay.rs:340/355, pay.rs:721 | P1 | terminal phase appended before/without gate success → paid customer, no access, skipped as settled | STILL-ACTIONABLE (family design gap) | live-path ordering fixed by `df1dbd7` (#52 round 2, verified in diff: gate failure → rollback + TimeoutUnknown re-mark; terminal `Received` only after gate opens). Remaining: reconcile path terminalizes ReconcileSpent after a warn-only gate failure; replay path ignores gate failure and answers 200 | draft C (gate-pending family) — covers 4181569956, 4186082579 |
| 4141963821 | 43 | src/main.rs:106-184 apply_reconciled_grant (was main.rs:52@1090dca6) | P1 | terminal ReconcileSpent append failure discarded → 60s loop recreates session forever | STILL-ACTIONABLE → FIXED THIS AUDIT | `0a1d02b`: append failure recorded in process-lifetime `granted_pending_terminal`; later passes retry ONLY the append (no session reset, no gate re-open), CRITICAL logged; CLI-fund branch failure surfaced too. Residual: pay.rs:721 live `Received` append suppression (pay.rs outside this thread's lane scope) — noted in draft C | none (residual in draft C) |
| 4181569956 | 54 | family: src/main.rs:148-150 + terminal append | P1 | gate failure in reconcile terminalizes | FIX-TRACKED-BY-JRN | family verdict = thread 4141963812 (draft C): reconcile path still appends terminal ReconcileSpent after a failed grant_access at HEAD | draft C |
| 4186082579 | 55 | family: src/main.rs:148-150 + terminal append; pay.rs:340/355 | P1 | gate failure before/within reconcile → terminal + replay answers success ignoring gate | FIX-TRACKED-BY-JRN | family verdict = thread 4141963812 (draft C): same terminalize-after-gate-failure gap plus the replay branch's ignored grant_access | draft C |

## Per-thread notes

**4140359052 (#39, wallet.rs:691@3ad94cd4 → 328-329@HEAD).** Re-located at
HEAD: `receive()` still acquires the per-mint mutex inside
`timeout(OP_TIMEOUT)` (src/wallet/wallet.rs:328-329); the same fusion exists
in `send` (915-916), `mint_tokens` (1037-1038) and `melt` (1070-1071).
`spawn_saga_recovery` holds the mutex up to `RECOVERY_TIMEOUT` = 120s
(L82, L1294-1296) while `OP_TIMEOUT` = 30s (L75), so a payment queued behind
a recovery can burn its entire 30s budget on the lock wait and surface
`WalletError::Timeout` without CDK or the mint ever being contacted;
`handle_pay` then journals TimeoutUnknown and answers 504
payment-outcome-unknown "do not resubmit" for a token that is still
spendable and has no saga. NOT fixed here: the retry-safe semantics need a
new `WalletError` variant (src/error.rs) plus a `receive_failure_code`/
`receive_failure_shape` arm (pay.rs classifier) — both files outside this
lane's ownership; a stringly-typed reuse of `Database` would game the
substring classifier. Draft A filed.

**4152466921 (#42, wallet.rs:297@54423067).** Diffed `54423067..HEAD`. The
finding demanded: persist the withdrawal intent/outcome and check that
record before allowing another attempt. At HEAD every send/melt call site
does exactly that: CLI drain consults `mint_has_unresolved_send` (added in
`08ebb00`, verified via `git show 08ebb00 -- src/wallet/wallet.rs`) plus a
payout-journal Intent and defers while a saga is settling
(src/cli/mod.rs:118-146); the JSON-protocol drain journals
Intent/TokenCreated (src/cli/jsonproto.rs:283-340); payout melts journal
Ambiguous on timeout/error and never re-melt the same invoice
(src/payout.rs:569-620, `5e8ebb1`); the reseller send makes the token
durable before delivery (`df1dbd7`, upstream_manager/mod.rs:200-245). The
wrapper-level `recover_before_op` refusal (wallet.rs:300-305) additionally
blocks immediate replays after same-call recovery. Residual (note, not a
gap at HEAD): the discipline is per-caller, not enforced by TollWallet — a
future caller without its own journal could still double-withdraw; the
existing wrapper guard only catches same-call recovery activity.

**4141963797 (#43, main.rs:20@1090dca6).** Verified at HEAD: the wallet
object is always inserted into AppState, but a failed boot `ensure_mint`
leaves the per-mint wallet unregistered, so `reconcile` → `token_spent`
returned WalletNotFound → undecided forever. Partial mitigation landed in
`df1dbd7` (30s mint-retry loop) but only for mints in the CURRENT config,
and its first retry tick fires at t+30s while the startup reconcile pass
runs at t≈0; entries whose mint left the config after intent time were
never covered (silent WalletNotFound each pass). Fixed in `74e187d`: the
pass retries `ensure_mint` for every undecided entry's persisted mint
(bounded one attempt per pass; idempotent map lookup once registered) and
surfaces MintNotAccepted loudly as an operator-decision signal. Red test:
`reconcile_pass_registers_wallet_for_undecided_entries_mints` (genuine red
on parent — see red-proof outcomes).

**4141963803 (#43, main.rs:29@1090dca6).** Verified at HEAD: the reconcile
grant loop still applies `create_session` (overwrite) per entry
(src/main.rs:138 after this audit's refactor; main.rs:46 at base). Two
same-MAC undecided payments decided in one pass: both terminalize
ReconcileSpent, only the last grant survives — the other payment's value is
permanently lost. Additional harm: a reconcile re-grant REPLACES a
live-path `add_allotment`-accumulated session (live renewals are additive
since `df1dbd7`), shrinking the customer's standing. Fix requires additive
grants keyed by a durable per-payment grant id; the session module exposes
only a single-slot `last_grant_id`/`has_grant` (src/session/mod.rs:130-175)
— the multi-grant ledger is the shared root cause with the mig cluster's
session grant-id draft, and session files are outside this lane. Draft B
filed; no code change.

**4141963812 (#43, pay.rs:520@1090dca6).** The live-path half was fixed
after the finding by `df1dbd7` (#52 round 2; verified in
`git show df1dbd7 -- src/http/routes/pay.rs`): gate failure now re-marks
TimeoutUnknown, rolls the session back and answers 400 session-error; the
terminal `Received` append moved AFTER the gate opens
(pay.rs:678-729@HEAD). The family design gap remains on (a) the reconcile
path — src/main.rs:148-150 logs a gate failure as a warn and still appends
terminal ReconcileSpent, so a spent token's owner can end up with a session
but no network access, never retried (exactly what #54/#55 report against
main.rs), and (b) the idempotent-replay path — pay.rs:340/355 ignore
`grant_access` failures (`let _ =`) while answering a success event. Also
adjacent: pay.rs:721 still suppresses the live `Received` append error
(the live twin of what `0a1d02b` fixed on the reconcile path). Per the
campaign rules for design gaps: Draft C filed, no money-flow redesign here.

**4141963821 (#43, main.rs:52@1090dca6).** Verified at HEAD (base): the
terminal append's error was discarded (`let _ =`, main.rs:69@base) — with
the journal stuck at Intent/TimeoutUnknown, every 60s pass re-ran
create_session (resetting usage/expiry indefinitely) while the failure
stayed silent; the CLI-fund branch discarded its append error the same way.
Fixed in `0a1d02b`: grant application extracted into `apply_reconciled_grant`
with a process-lifetime `granted_pending_terminal` set — on append failure
the entry is recorded and later passes retry ONLY the append (session
usage/expiry preserved, gate not re-opened) until it lands, logging
CRITICAL per failure. The durable per-payment grant id (full fix) is Draft
B's scope. Red test:
`terminal_append_failure_retries_append_without_resetting_session`
(compile-shaped red — see below).

**4181569956 (#54, pay.rs:590@510dacbb) / 4186082579 (#55,
pay.rs:684@9131d102).** Cross-PR repeats of the 4141963812 family. Both
verified at HEAD against the current code: the reconcile path still
terminalizes after a failed `grant_access` (main.rs:148-150 + terminal
append), and the replay branch still answers success while ignoring a gate
failure (pay.rs:340/355). Disposition FIX-TRACKED-BY-JRN; see Draft C.

## Crash-window tables (money-code fixes)

### `74e187d` — reconciler wallet-init retry (no value moves in this path)

| death point | durable state | on restart |
|---|---|---|
| before the wallet-init retry | journal undecided | next pass retries wallet init |
| wallet registered, mid-reconcile | journal undecided | wallet re-registered idempotently |
| proofs queried, before grant | journal undecided | next pass re-decides (unchanged) |
| after terminal ReconcileSpent | journal terminal | nothing owed (unchanged) |

### `0a1d02b` — terminal-append failure handling (no value moves in this path)

| death point | durable state | on restart |
|---|---|---|
| after session save_now, before gate | journal undecided | next pass re-grants (overwrite idempotent) |
| after gate, before terminal append | journal undecided, session durable | one idempotent re-grant, append retried |
| append fails (persisting) | journal undecided, session durable | set is process-local: ONE re-grant per boot, then append-only retries, CRITICAL logged |
| terminal append Ok | journal terminal | nothing owed |

## Test-run summaries

- Authoritative baseline, pristine worktree at `0da326a`
  (`cargo test`): lib 306 passed / 0 failed; bin 0; integration 9+18+7+17+3
  = 54 passed / 0 failed. Total 360 / 0. (An early background run executed
  under the session's default cwd — another lane's worktree — and showed an
  unrelated keyset-hygiene flake; it never reproduced in this worktree nor
  in the pristine base run.)
- Branch HEAD `0a1d02b` (`cargo test`): lib 306 passed / 0 failed; bin
  **2 passed / 0 failed** (the two new regression tests); integration 54
  passed / 0 failed. Total **362 / 0**.
- `cargo fmt --check`: clean. `cargo clippy --all-targets -- -D warnings`:
  clean.

## Red-proof outcomes

- `reconcile_pass_registers_wallet_for_undecided_entries_mints`
  (thread 4141963797): **genuine red on parent** — parent `src/main.rs`
  restored from `git show 0da326a:src/main.rs`, test module spliced in,
  `cargo test --bin tollgate-module-basic-rust reconcile_pass_registers`
  → FAILED ("got []" — no wallet registered); restored via
  `git checkout HEAD -- src/main.rs`; green on the fix commit.
- `terminal_append_failure_retries_append_without_resetting_session`
  (thread 4141963821): **compile-shaped red** — splicing the test into the
  parent fails to compile (`error[E0425]: cannot find function
  'apply_reconciled_grant'`, demonstrated and restored). Reason: the parent
  discards the append error with `let _ =`, leaving no seam through which a
  once-failing append can be observed; the regression test must drive the
  extracted helper. The failure injection itself is real (journal chmod
  0444 for one pass, uid != 0), and the test proves the healed behavior:
  usage preserved across the append retry, gate opened exactly once,
  terminal phase landed, pending set cleared.

## READY-TO-PASTE issue drafts

### Draft A — Distinguish per-mint lock-wait expiry from an ambiguous receive (Codex P1 on #39, thread 4140359052)

**Severity:** P1 (customer told "do not resubmit" for a token that was never
touched)

**Where:** `src/wallet/wallet.rs:328-329` (receive), same fusion in send
(915-916), mint_tokens (1037-1038), melt (1070-1071); `RECOVERY_TIMEOUT`=120s
at L82 vs `OP_TIMEOUT`=30s at L75.
Permalink: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/wallet/wallet.rs#L328-L334

**Reproduction sketch:**
1. An op on mint M times out; `spawn_saga_recovery` grabs M's wallet mutex
   for up to 120s (wallet.rs:1294-1296).
2. A customer POSTs a token for M. `receive()` enters
   `timeout(OP_TIMEOUT=30s)` (L328) and parks on `wallet.lock().await`
   (L329).
3. 30s elapse with zero CDK/mint contact → `WalletError::Timeout`.
4. `handle_pay` journals `TimeoutUnknown` and answers 504
   `payment-outcome-unknown` "do not resubmit this token" (pay.rs:161-167,
   484-520) — but the token is still spendable and no saga exists; the
   journal entry later reconciles to unspent (nothing owed). The payment
   was refused under false ambiguity.

**Fix direction:** acquire the per-mint lock OUTSIDE `timeout(OP_TIMEOUT)`
under its own bounded wait; on lock-wait expiry return a NEW
`WalletError` variant (e.g. `MintBusy`) — requires adding the variant in
`src/error.rs` and a `receive_failure_code`/`receive_failure_shape` arm in
`src/http/routes/pay.rs` mapping it to a retry-safe 400/503 notice code
("mint busy — safe to resubmit", Go-parity taxonomy addition). Applying the
same separation to send/melt/mint_tokens can land in the same change or a
tracked follow-up.

**Acceptance criteria:**
1. With M's mutex held longer than the lock-wait bound, `receive` returns
   the busy variant in ~the lock-wait bound (not OP_TIMEOUT), with no CDK
   or mint contact.
2. `handle_pay` answers a retry-safe code (not `payment-outcome-unknown`)
   and the same token resubmits successfully once the lock frees.
3. Regression test: hold the mutex, assert variant + latency; assert
   OP_TIMEOUT still bounds the operation itself.
4. No change to ambiguous-outcome handling for timeouts that fire AFTER
   the operation started (saga recovery path unchanged).

### Draft B — Reconciled same-MAC grants must be additive with durable per-payment grant ids (Codex P1 on #43, thread 4141963803)

**Severity:** P1 (silent loss of a paid grant)

**Where:** reconcile grant loop applies `create_session` (overwrite) per
entry — `src/main.rs:46` at 0da326a (`src/main.rs:138`,
`apply_reconciled_grant`, after the JRN audit's refactor).
Permalink: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/main.rs#L44-L75

**Reproduction sketch:**
1. Two payments for the same MAC end undecided (crash/timeout mid-receive);
   both tokens are spent at the mint.
2. One reconciliation pass decides both: `reconcile` returns both entries;
   the loop calls `create_session(mac, …)` twice — the second overwrites
   the first — then appends terminal `ReconcileSpent` for BOTH.
3. Only the last hash-map-ordered grant survives: the other payment's
   allotment is permanently lost while its journal says settled.
Variant harm: a reconcile re-grant REPLACES a live-path
`add_allotment`-accumulated session (live renewals are additive since
df1bdd7), shrinking a standing customer's allotment to the single
reconciled grant.

**Shared root cause (coordinate with the mig cluster's session grant-id
draft):** `src/session/mod.rs` tracks only a single-slot `last_grant_id`
(`has_grant`, session/mod.rs:167) — there is no per-payment durable grant
ledger, so reconcile-side re-runs rely on overwrite idempotency instead of
grant-id dedup.

**Fix direction:** grants applied additively (`add_allotment`) keyed by a
durable per-payment grant id (the payment-journal entry id), with
`has_grant`-style dedup so crash re-runs never double-apply; requires
extending the session module's grant tracking beyond the single slot.

**Acceptance criteria:**
1. Two same-MAC undecided payments decided in one pass both contribute
   allotment (total = sum, order-independent).
2. Re-running reconciliation after a crash between grant and terminal
   append does not double-grant (grant-id dedup).
3. A reconciled grant ADDS to an active accumulated session instead of
   replacing it.
4. Crash-injection regression tests for both scenarios.

### Draft C — Keep payments gate-pending until access actually opens: reconcile + replay paths (Codex P1 on #43/#54/#55, threads 4141963812, 4181569956, 4186082579)

**Severity:** P1 (spent token, persisted session, no network access,
never retried)

**Where (at HEAD):**
- LIVE path is FIXED (df1dbd7, #52 round 2): pay.rs:678-729 — gate failure
  re-marks TimeoutUnknown, rolls the session back, answers 400
  session-error; terminal `Received` only after the gate opens.
- Remaining gap 1 — reconcile path: `src/main.rs:57-59`@0da326a
  (src/main.rs:148-150 after the JRN audit's refactor): a `grant_access`
  failure is a warn; the loop still appends terminal `ReconcileSpent`.
  Permalinks: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/main.rs#L57-L75
- Remaining gap 2 — replay path: pay.rs:340/355@0da326a ignore
  `grant_access` failures (`let _ =`) while answering a success event.
  Permalink: https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/http/routes/pay.rs#L329-L357
- Adjacent (same terminalization family): pay.rs:721 suppresses the live
  `Received` append error — the live-path twin of the reconcile-side
  append-failure handling the JRN audit fixed in `0a1d02b`.

**Crash/failure-window table (reconcile path at HEAD):**

| death / failure point | durable state | customer effect |
|---|---|---|
| token confirmed spent, before session save | journal undecided | re-decided next pass — OK |
| session save_now ok, crash before grant_access | journal undecided, session durable | re-grant next pass (overwrite) + gate — OK |
| grant_access FAILS (ndsctl down) | journal terminal `ReconcileSpent` | session + NO access, never retried — GAP |
| crash after grant_access, before terminal append | journal undecided | idempotent re-grant — OK |
| replay POST while gate is down | settled outcome, HTTP 200 | success answered, gate never opened — GAP |

**Fix direction:** a recoverable gate-pending phase — e.g. `ReconcileSpent`
is only appended after `grant_access` succeeds; a `GatePending` phase is
retried by the 60s reconciler (grant_access only, mirroring the
append-only retry the JRN audit introduced for terminal appends) and by
the replay path; replay must surface a gate failure instead of `let _ =`,
and the live `Received` append error at pay.rs:721 gets the same
actionable treatment the reconcile path now has.

**Acceptance criteria:**
1. A gate failure during reconciliation leaves the entry retryable; later
   passes retry `grant_access` until it opens, then terminalize.
2. Terminal `ReconcileSpent` implies the gate opened (or an
   operator-visible compensating state exists).
3. The replay path answers success only when access is actually open (or
   successfully re-opened); gate failures surface as session-error.
4. Crash-injection tests at each row of the table above.
