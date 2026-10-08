# S13 Codex Review-Finding Audit — Campaign Summary

**Campaign:** S13 batch audit of merged-PR Codex review threads.
**Audited base:** `0da326a` (PRs #25, #29, #37, #39, #42, #43, #54, #55, #57 lineage).
**Lanes:** mig (PRs #25/#29/#37), jrn (PRs #39/#42/#43), new (PRs #54/#55/#57).
**Branches:** `audit/s13-mig` (4 commits, HEAD `7cac96b`), `audit/s13-jrn`
(2 commits, HEAD `0a1d02b`), `audit/s13-new` (9 commits, HEAD `ca39357`),
all based on `0da326a`. This branch (`audit/s13-codex-feedback`) carries the
campaign deliverables only. Nothing is pushed; the fix branches await human
review before any PR.

## Totals

| | rows | unique threads |
|---|---|---|
| dispositioned | 31 (incl. 4 cross-PR FIX-TRACKED-BY-JRN rows) | 27 |
| ALREADY-FIXED (verified in diff, sha cited) | 4 | 4115698770, 4126981658, 4128285052 (mig); 4152466921 (jrn) |
| WRONG (not reproducing, evidence given) | 1 | 4186082594 (new; pinned by regression tests anyway) |
| STILL-ACTIONABLE, fixed in this audit (red-proven) | 15 rows / 14 threads | see fix index |
| STILL-ACTIONABLE, issue draft filed | 10 rows / 8 threads | see draft index (7 drafts) |

Verification: per lane `cargo fmt --check` clean,
`cargo clippy --all-targets -- -D warnings` clean, full suites green —
mig 364 (default) / 382 (embedded); jrn 362 (default, incl. 2 bin tests);
new 399 (default) / 417 (embedded). Red-test discipline per campaign rules:
fix+test committed first, then each test spliced into the parent `0da326a`
files and shown failing (or the red-kind exemption stated: compile-shaped
for jrn's terminal-append seam, test-only for new's WRONG pinning).

Caveat for future runners: this machine shares `CARGO_TARGET_DIR`
(`/home/ubuntu/.cargo-target`, via cargo config) across worktrees of the
same package — stale bin-test binaries can silently report `0 tests`. Force
recompile (touch) or use an isolated `CARGO_TARGET_DIR` when verifying.

## Fix index (commits on the lane branches)

| thread | PR | lane | commit | fix |
|---|---|---|---|---|
| 4126981653 | 29 | mig | `9c2dec2` | completion marker written atomically (tmp+fsync+rename) — torn empty marker no longer read as manual `touch` |
| 4128285045 | 37 | mig | `7cac96b` | MIGRATION.md NUT-13 recovery: stop service + clear generated identity before placing mnemonic |
| 4128285056 | 37 | mig | `46e373b` | identity dir open/sync failures propagate (unproven rename durability aborts first boot) |
| 4128285062 | 37 | mig | `e368128` | legacy marker `failed=N` folds into `summarize_state` (no more false `settled: true`) |
| 4141963797 | 43 | jrn | `74e187d` | reconciler retries `ensure_mint` for undecided entries' persisted mints; MintNotAccepted surfaced loudly |
| 4141963821 | 43 | jrn | `0a1d02b` | terminal-append failure actionable: append-only retry, no session reset loop, CRITICAL log |
| 4186780240 | 57 | new | `10acd24` | keyset-rotation self-swap runs the same `recover_before_op` preflight as other money wrappers |
| 4186780245 | 57 | new | `7e6fa15` | expiry warnings intersect expiring keysets with held unspent proofs |
| 4186082594 | 55 | new | `25bab1f` | (test-only, WRONG disposition) IPv6 authority stability + idempotence pinned |
| 4181569963 | 54 | new | `1b42095` | expired-history TTL enforced on `/session-state` reads (+ wired an orphaned test module: 26 dead tests revived) |
| 4181569976 | 54 | new | `f8304d7` | `ssl apply` validates key parse + cert↔key pairing before any mutation (no new deps) |
| 4186082566 | 55 | new | `01cdb00` | upstream grant expiry derived at parse when the event carries none (Go wire format carries no expiry — verified) |
| 4181569959 / 4186082546 | 54/55 | new | `97284c5` | definitive send failures (WalletNotFound, InsufficientFunds) journal `Failed`, not blocking-Ambiguous |
| 4181569967 | 54 | new | `a88a754` | signal polling resolves netifd `l3_device` (ubus) instead of the UCI section name |

## Issue-draft index (READY-TO-PASTE, full text in the lane verdicts)

| draft | lane | threads | theme |
|---|---|---|---|
| DRAFT-1 | mig | 4115698776 | canonicalized mint URL strands legacy per-mint DB files (probe/adopt old spellings) |
| DRAFT-2 | mig | 4115698778 | single-slot `last_grant_id` — Lightning double-apply crash window (additive grant set) |
| DRAFT-3 | mig | 4126981643 | terminal `Spent` without proof-of-recovery at migration finalization |
| Draft A | jrn | 4140359052 | per-mint lock wait fused into 30s op timeout → false "unknown, don't resubmit" |
| Draft B | jrn | 4141963803 | reconciled same-MAC grants overwrite (additive + durable per-payment grant id) — shared root cause with mig DRAFT-2 |
| Draft C | jrn | 4141963812, 4181569956, 4186082579 | gate-pending family: reconcile path terminalizes after gate failure; replay path ignores gate failures |
| DRAFT-A | new | 4181569949, 4186082529 | reseller upstream `Delivered` journaled but grant in-memory only → double purchase after restart |

## Lane verdicts

Full disposition tables, per-thread notes, crash-window tables, red-proof
outcomes and reproduction sketches live next to this file:

- `docs/audit-s13/verdict-mig.md` — 10 rows, PRs #25/#29/#37
- `docs/audit-s13/verdict-jrn.md` — 8 rows, PRs #39/#42/#43 (+ #54/#55 family)
- `docs/audit-s13/verdict-new.md` — 13 rows, PRs #54/#55/#57

## Post-implementation review (added after lane completion)

A 5-lane review (goal / QA-execution / code-quality / security / context)
ran over all three branches: see `REVIEW.md`. Formal verdict: FAILED on
the branches as-reviewed (one MAJOR: x509 SEC1 long-form DER misparse),
with the decisive context that the campaign had already landed as PR #58
(origin/main `274f001`, superset of these branches) and the MAJOR is
already fixed in the in-flight PR #68 (`82316ce`). Two MEDIUM residuals
(InsufficientFunds-definitive provenance; tmp-path O_NOFOLLOW) remain
documented for the #68 stream. This branch is the durable campaign
record; the three fix branches are pre-merge lineages of #58.
