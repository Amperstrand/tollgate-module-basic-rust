# mig verdicts

Cluster MIG — threads on PRs #25, #29, #37 (10 threads).
Worktree `/tmp/opencode/wt-mig`, branch `audit/s13-mig`, audited base `0da326a`,
branch HEAD `7cac96b`. All evidence re-located at HEAD; finding line numbers
were not trusted.

| thread_id | PR | path:line@HEAD | sev | theme | disposition | evidence / commit | issue-draft |
|---|---|---|---|---|---|---|---|
| 4115698770 | 25 | src/lightning_quotes.rs:259-300 | P1 | quote marked minted on NUT-04 `Issued` without local saga recovery | ALREADY-FIXED | 2dc87f5 (#39) + bef76f0 (#42) | — |
| 4115698776 | 25 | src/wallet/wallet.rs:196-252 | P1 | canonicalized mint URL ⇒ new DB filename, legacy file stranded | STILL-ACTIONABLE | reproduces at HEAD; no legacy-file probe in `ensure_mint` | DRAFT-1 |
| 4115698778 | 25 | src/session/mod.rs:45,167-172 | P1 | single `last_grant_id` overwritten by later grant ⇒ double-apply | STILL-ACTIONABLE | reproduces at HEAD; #43 journal covers token payments only | DRAFT-2 |
| 4126981643 | 29 | src/migration.rs:423 (+ finish:625) | P1 | no-unresolved-saga ≠ value recovered — terminal `Spent` without proof outputs | STILL-ACTIONABLE | reproduces at HEAD; no wallet-proof-possession check or durable credit | DRAFT-3 |
| 4126981653 | 29 | src/migration.rs:668 (was finish:660) | P1 | torn zero-byte completion marker read as manual completion | STILL-ACTIONABLE | **fixed here** — 9c2dec2 | — |
| 4126981658 | 29 | src/wallet/wallet.rs:362-431 | P1 | NUT-07 `Pending` collapsed into spendable ⇒ re-receive same inputs | ALREADY-FIXED | bef76f0 (#42) + 87d7df9 (#43) | — |
| 4128285045 | 37 | MIGRATION.md:534-553 | P1 | recovery doc: package install autostarts service before mnemonic placed | STILL-ACTIONABLE | **fixed here** — 7cac96b (doc) | — |
| 4128285052 | 37 | src/wallet/wallet.rs:1209-1247 | P1 | identity temp file 0666&umask until post-write chmod | ALREADY-FIXED | 2dc87f5 (#39) | — |
| 4128285056 | 37 | src/wallet/wallet.rs:1240-1257 | P1 | dir open/sync failures discarded ⇒ rename not known durable | STILL-ACTIONABLE | **fixed here** — 46e373b | — |
| 4128285062 | 37 | src/migration.rs:755-771 | P2 | legacy `failed=N` marker counts ignored ⇒ `settled: true` lie | STILL-ACTIONABLE | **fixed here** — e368128 | — |

Counts: **10 dispositioned, 4 fixed, 3 issue-drafts.**

## Per-thread notes

**4115698770 (ALREADY-FIXED).** The finding's scenario: `mint_tokens` times
out after the mint accepted; `check_mint_quote_state` sees `Issued`; quote
marked minted "without ever retrying recovery until a restart". At 841b7192
`mint_tokens` had neither recovery trigger (verified via `git show
841b7192:src/wallet/wallet.rs`). At HEAD, `mint_tokens`
(src/wallet/wallet.rs:1029-1057) (a) runs `recover_before_op` before `mint`
(bef76f0, #42) and (b) fires `spawn_saga_recovery` on the timeout branch
(2dc87f5, #39) — in-session recovery on the exact path the finding
describes; boot-time `ensure_mint` remains the backstop. The settle path
still advances `minted=true` on mint-authoritative `Issued` without a
synchronous proof-possession check — acceptable: the saga is durable in
SQLite, recovery is retried in-session and at boot, and the grant depends on
mint-side commitment, not local proof possession. Residual noted, not
actionable.

**4115698776 (STILL-ACTIONABLE → DRAFT-1).** `ensure_mint` canonicalizes the
mint URL and derives the DB filename from it (wallet.rs:196-252);
`db_path_for_mint` has no probe/migration of files named from pre-#25 raw
spellings (`git log 841b7192..HEAD -S db_path_for_mint` shows no such
change; #42's SDK alignment did not add one). Old spelling
`https://Mint.Example/` → `https___Mint.Example_.sqlite`; canonical →
`https___mint.example.sqlite` — different file, stranded proofs, fresh
derivation counters under the same seed. Design gap; issue draft, no hasty
migration code in this audit.

**4115698778 (STILL-ACTIONABLE → DRAFT-2).** `last_grant_id` is still a
single slot (session/mod.rs:45; `add_allotment` overwrites at :148/:156;
`has_grant` compares only the last at :167-172). #43's payment journal made
*token* payment grants durable/idempotent, but the ln-quote settle path
(lightning_quotes.rs:321-347) still relies on the single slot. The Codex
A/B double-apply crash sequence reproduces at HEAD (crash-window table in
DRAFT-2). Ownership: grant-id theme = issue draft only, no code.

**4126981643 (STILL-ACTIONABLE → DRAFT-3).** The import loop's `AllSpent`
arm (migration.rs:423-439) journals terminal `Spent` whenever NUT-07 says
all-spent and `mint_has_unresolved_receive` is false. But CDK deletes a
receive saga when its outcome is persisted — including the documented
value-less closed path — so saga absence is not proof of recovered value.
`finish()` renames `wallet.db` when failed==0 && pending==0. No
wallet-proof-possession check and no durable credit exist at HEAD. #50's
`PartiallySpent`/`RemainderImported` machinery covers partial tokens only,
not this. Design gap; issue draft.

**4126981653 (STILL-ACTIONABLE — fixed in 9c2dec2).** Verified at HEAD:
`finish()` used `File::create(&self.marker)` in place (0da326a
migration.rs:660) — truncate-then-write; `marker_is_complete` (:787-789)
honors an empty marker as the operator's manual `touch`. A crash between
create and write ⇒ migration skipped forever. Fix: body written to
`.migration_complete.tmp`, fsynced, renamed, parent dir fsynced
(best-effort, deliberately — see commit body). Crash-window table below.
Red-proof: `finish_replaces_marker_atomically_never_in_place` — red on
parent (Ok + prior marker destroyed), green on fix.

**4126981658 (ALREADY-FIXED).** At 51b05c14 `token_spent` collapsed every
non-`Spent` state into `Ok(None)`. At HEAD the surface is tri-state:
`token_check_state` (wallet.rs:377-431) returns `Pending` for any
Pending/Reserved proof; `token_spent` (:362-374) maps `Pending` to `Err`
("outcome still ambiguous", citing Codex P1 on #43); the import loop's
batched `check_tokens_spent` + `classify_spend_state` (:95-143, :443-567)
classify pending proofs as `TokenSpendState::Indeterminate`, and the loop
defers those (migration.rs:541-550, `git log -S Indeterminate` → bef76f0
#42). The import loop can no longer treat Pending as spendable.

**4128285045 (STILL-ACTIONABLE — fixed in 7cac96b, doc).** Verified the
mechanism: openwrt/Makefile post-install runs `/etc/init.d/tollgate-wrt
enable/start`, first start generates fresh `wallet_seed.bin` +
`wallet_mnemonic.txt`, and `load_or_create_seed` (wallet.rs:1165-1173)
refuses to start on seed/mnemonic mismatch — so following the old doc
procedure bricks the next start instead of recovering. Doc-only fix: step 1
now stops the service and removes the generated identity before the
mnemonic is placed. Red-test exemption: the defect lives in operator
procedure, not code; no asserterable unit (verified against Makefile +
`load_or_create_seed` mismatch refusal).

**4128285052 (ALREADY-FIXED).** `write_file_private` opens the tmp file via
`tokio::fs::OpenOptions` with `.mode(0o600)` (wallet.rs:1213-1219) and
retains the explicit chmod only for stale leftovers (:1232-1238) — exactly
the fix the finding requested. `git log a10cc34c..HEAD -S "mode(0o600)"`
→ 2dc87f5 (#39). Diff verified, not assumed.

**4128285056 (STILL-ACTIONABLE — fixed in 46e373b).** Verified at HEAD: the
dir open result was discarded (`if let Ok(dir)`) and the sync result
dropped (`let _ =`). Fix: both propagate; bare relative filenames
(parent `""`) normalize to `.` so cwd writes keep working. Crash-window
table below. Red-proof: `identity_dir_sync_failure_propagates` (0o333 dir
admits create/rename, refuses dir open — isolates the discarded branch);
red on parent, green on fix (guard-skips under root, where permission bits
are not enforced).

**4128285062 (STILL-ACTIONABLE — fixed in e368128).** Verified at HEAD:
`summarize_state` read the marker only for its `state` string; `failed`
came solely from the journal, so a legacy marker with `failed=2` and no
journal reported `failed=0` / `settled=true`, contradicting
`marker_is_complete` (which treats legacy `failed>0` as incomplete and
retries). Fix: fold the marker's `failed=N` into `state.failed` when (and
only when) the marker is legacy; current `state=` markers stay
journal-backed (their counts snapshot the journal). Red-proof:
`legacy_marker_failure_counts_keep_state_unsettled` red on parent
(failed==0, settled==true); double-count guard pinned by
`current_marker_failures_are_not_double_counted` (green on parent by
design — non-regression pin).

## Crash-window tables (money-adjacent fixes)

**9c2dec2 — completion-marker write (`finish`):**

| death point | durable state | next boot |
|---|---|---|
| before rename | old marker intact, tmp fsynced | re-run; journal `Imported`/`Spent` dedupes |
| after rename, before dir fsync | new complete/partial marker (dir entry maybe unproven) | correct state; worst case marker lost → re-run converges via journal |
| torn in-place write (old code) | EMPTY marker | `should_run()==false` forever — the defect |

**46e373b — wallet-identity dir sync (`write_file_private`):**

| death point | durable state | consequence |
|---|---|---|
| before rename | tmp fsynced 0600 | nothing moved, no identity |
| after rename, dir open/sync fails | renamed file, unproven dir entry | first boot now FAILS loudly (was: silent Ok) — identity is never used before its rename is durably committed |
| after dir fsync | fully durable | normal |

Deliberate contrast: the marker's post-rename dir fsync stays best-effort
(lost rename converges via the journal); the wallet identity has no
reconvergence path, so unproven durability aborts.

## Test-run output summaries

All commands run on branch HEAD `7cac96b` in `/tmp/opencode/wt-mig`:

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean (0 warnings).
- `cargo test` — **310 passed, 0 failed** (lib) + 54 integration
  (9+18+7+17+3), 0 failed.
- `cargo test --features embedded-portal` — **328 passed, 0 failed** (lib)
  + 54 integration, 0 failed.
- Baseline at 0da326a before edits: 306 lib tests, 0 failed.

Red-test discipline (fix+test committed together first; parent files
checked out, only the test spliced in, run, restored via
`git checkout HEAD --`):

| test | on parent 0da326a | on fix |
|---|---|---|
| `identity_dir_sync_failure_propagates` | FAILED (panicked: "unproven durability must fail loudly") | pass |
| `legacy_marker_failure_counts_keep_state_unsettled` | FAILED (assertion `state.failed == 2`) | pass |
| `finish_replaces_marker_atomically_never_in_place` | FAILED (panicked: "an unwritable tmp must abort the finish") | pass |

Commits (local only; nothing pushed):

- `46e373b` fix(wallet): propagate identity dir sync failures (Codex P1 on #37)
- `e368128` fix(migration): count legacy marker failures in summarize_state (Codex P2 on #37)
- `9c2dec2` fix(migration): write the completion marker atomically (Codex P1 on #29)
- `7cac96b` docs(migration): stop service and clear generated identity before mnemonic recovery (Codex P1 on #37)

---

## Issue drafts (READY-TO-PASTE)

### DRAFT-1 — Stranded legacy per-mint wallet DBs after mint-URL canonicalization (Codex 4115698776, PR #25)

**Severity:** P1 (fund safety — silent balance loss on upgrade).
**Where:** `src/wallet/wallet.rs:196-252` (`ensure_mint` + `db_path_for_mint`)
at `0da326a`.
**Permalink:**
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/wallet/wallet.rs#L196-L252

**Reproduction sketch:**

1. Run a pre-#25 binary with config mint `https://Mint.Example/` — DB file
   `https___Mint.Example_.sqlite` is created (filename sanitized from the
   raw spelling minus trailing slash).
2. Upgrade to current: `ensure_mint` canonicalizes to
   `https://mint.example` and opens `https___mint.example.sqlite` — a
   blank CDK database under the same seed. No probe, rename, or adoption
   of the old spelling exists anywhere at HEAD.
3. Result: existing proofs stranded in the old file; derivation counters
   restart from zero for the same mint/keysets (AGENTS.md: "never reuse
   deterministic derivation outputs" is threatened by the counter reset);
   balance silently drops.

**Suggested acceptance criteria:**

- On `ensure_mint`, if the canonical DB path does not exist, probe the
  plausible legacy spellings of the same mint (raw config string,
  trailing-slash-trimmed, case-preserved host/scheme) and adopt (rename)
  the first existing match into the canonical filename — before opening.
- Adoption must be atomic (rename), logged, and idempotent across reboots.
- A wallet registered under an alias spelling must never open a second
  DB for the same canonical mint (one wallet per canonical URL, per
  AGENTS.md).
- Tests: config spelling `https://Mint.Example/` with a pre-existing
  legacy-named DB → canonical file adopted, balance preserved, no second
  file opened; repeated boots converge (no double-adopt).

### DRAFT-2 — Single-slot `last_grant_id` lets a later Lightning grant erase an earlier crash-recovery witness (Codex 4115698778, PR #25)

**Severity:** P1 (fund safety — double-grant of paid allotment).
**Where:** `src/session/mod.rs:45` (`last_grant_id`), `:167-172`
(`has_grant`), settle consumer `src/lightning_quotes.rs:321-347`.
**Permalinks:**
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/session/mod.rs#L45
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/session/mod.rs#L167-L172
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/lightning_quotes.rs#L321-L347

**Crash-window table (the double-apply sequence):**

| step | durable state | note |
|---|---|---|
| quote A settle: `add_allotment(mac, "ln:A")` + `save_now` | sessions.json carries `last_grant_id="ln:A"` + A's allotment | A's quote-store marker `allotment_added` NOT yet written |
| CRASH | as above | |
| reboot; pending quote B settles FIRST | sessions.json `last_grant_id="ln:B"` (slot overwritten), B's allotment added | B's marker written |
| quote A retries: `has_grant(mac,"ln:A")` → false | A's allotment added a SECOND time | customer double-granted |

**Reproduction sketch:** two paid quotes A, B for one MAC; kill the process
between A's session flush and A's quote-store upsert; restart with B sorted
first in `ungranted()`; observe A's allotment applied twice (compare
allotment sum vs. paid total).

**Suggested acceptance criteria:**

- Applied grant ids persist as a set (or bounded journal) per session, not
  a single slot; `has_grant` membership must survive later grants.
- Serde migration: existing `last_grant_id` values seed the set; wire
  format stays backward compatible for non-Lightning sessions
  (`skip_serializing_if` on empty).
- Regression test: simulate the table above (A durable, B settles, A
  retries) and assert each allotment applied exactly once.
- Consider folding ln-quote grants into the #43 payment-journal
  reconciliation so Lightning grants get the same durable idempotency
  token payments already have.

### DRAFT-3 — Migration terminalizes `Spent` without proving recovered outputs (Codex 4126981643, PR #29)

**Severity:** P1 (fund safety — value silently dropped at migration
finalization).
**Where:** `src/migration.rs:423-439` (`AllSpent` arm) and `:625`
(`finish` rename gate); related helper
`src/wallet/wallet.rs:746-816` (`mint_has_unresolved_receive`).
**Permalinks:**
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/migration.rs#L423-L439
https://github.com/Amperstrand/tollgate-module-basic-rust/blob/0da326a/src/migration.rs#L625

**Reproduction sketch:** a token whose earlier receive saga was deleted
without recovering outputs (CDK closes value-less sagas with only a logged
warning), or that was spent elsewhere: NUT-07 answers all-spent,
`mint_has_unresolved_receive` is false ⇒ terminal `Spent` journaled ⇒
`finish()` sees failed==0 && pending==0 ⇒ `wallet.db` renamed away and the
marker records clean, although no value provably reached this wallet.
Absence of an incomplete saga is CDK's "operation decided" signal, not a
"value recovered" proof.

**Suggested acceptance criteria:**

- Before journaling terminal `Spent`, verify the wallet actually holds
  outputs for the token (e.g. per-Y unspent-proof lookup in the local
  store for the token's Ys — the inverse of `mint_has_unresolved_receive`),
  OR record a durable credit/audit entry when it does not.
- `wallet.db` rename must additionally require zero unproven-`Spent`
  tokens (extend the `finish` gate with a third counter).
- Crash-consistency: the verification is a local SQLite read (works with
  the mint down); its outcome must be journaled, not recomputed per boot
  against moving state.
- Tests: all-spent token with (a) outputs present → terminal Spent, clean
  finish; (b) outputs absent → NOT clean-finished (or credited + surfaced
  in `summarize_state`), `wallet.db` retained/flagged accordingly.
