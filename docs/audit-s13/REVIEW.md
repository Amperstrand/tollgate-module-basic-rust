# S13 Campaign — Post-Implementation Review (5-lane)

Reviewed by the review-work orchestrator after all three lanes finished.
Review surface: branches `audit/s13-mig` (@7cac96b), `audit/s13-jrn`
(@0a1d02b), `audit/s13-new` (@ca39357) vs base `0da326a` — 15 commits,
combined diff ~80 KB / 20 files.

## Overall verdict: FAILED on the audited branches as-reviewed — sole MAJOR already remediated upstream in PR #68

Formal lane verdicts:

| # | Review area | Agent | Verdict | Confidence |
|---|---|---|---|---|
| 1 | Goal & constraint verification | Oracle | PASS | MEDIUM |
| 2 | QA by execution | autonomous | PASS | HIGH |
| 3 | Code quality | Oracle | **FAIL** | HIGH |
| 4 | Security (supplementary) | Oracle | PASS (severity MEDIUM) | — |
| 5 | Context mining | autonomous | PASS (with decisive discovery) | HIGH |

## Decisive context (lane 5)

The campaign **already landed** while the session ran: `origin/main` @
`274f001` is PR #58 ("disposition all Codex review debt from merged PRs
#25-#57 — 14 findings fixed, 9 design gaps filed"), parent exactly
`0da326a`, content a superset of all three audit branches plus
`audit/s13-fixes`. A follow-up PR **#68** is in flight
(`audit/s13-followup`, Codex round 8) implementing the drafted design
gaps. All four ALREADY-FIXED sha claims verified in history
(`2dc87f5`, `bef76f0`, `87d7df9`, `08ebb00`); the WRONG IPv6 argument
verified against byte-identical code at `9131d102` and `0da326a`;
`audit/keyset-hygiene-v2` confirmed as the content-identical pre-squash
lineage of merged #57. No landing conflicts are possible — the branches'
content is upstream.

## Findings vs landed/in-flight state

| finding | lane / sev | audited branches | landed #58 | in-flight #68 |
|---|---|---|---|---|
| x509 `sec1_public_key` hardcoded 1-byte DER length (`raw[2..]`) — uncompressed P-521 SEC1 keys misparse → valid pairs false-rejected with misleading "not a valid PEM private key" | quality MAJOR | present | present (KNOWN LIMITATION note there covers a different aspect: embedded-point trust) | **FIXED** in `82316ce` — `read_tlv_raw(content)` on the [1] content + P-521 fixtures/tests (exactly the suggested fix) |
| `Cdk(InsufficientFunds)` classified definitive/Failed — safe only if CDK raises it pre-submission; "escapes local prepare" places it in the post-submission window | security MEDIUM | present | present with rationale comment | not addressed — documented residual; verify CDK error provenance or gate on check_proof_state before unblocking repurchase |
| Predictable tmp paths (`File::create`, no O_NOFOLLOW/O_EXCL) on `.migration_complete.tmp` / identity `.tmp` — symlink truncation if a non-root principal can write the dir | security MEDIUM | present | present | not addressed — defense-in-depth on root-only `/etc/tollgate`; recommend O_NOFOLLOW or tempfile crate |
| `terminal_append_failure…` test chmod 0444 injection lacks the root-guard its sibling test has | quality MINOR | present | present | — |
| "not a valid PEM private key" also returned for structurally-valid-but-unextractable keys (misleading diagnostics) | quality MINOR | present | present | — |
| Reconciler folds the journal twice per pass (prelude + reconcile) | quality MINOR | present | present | — |
| Legacy marker + journal coexistence can double-count failures (conservative direction) | goal WARN | present | present | — |
| Test privkey `"01"*32`, env test-hook in prod `config_dir()` (pre-existing) | security LOW | present | present | — |

## QA evidence (lane 2, independently executed with isolated CARGO_TARGET_DIR)

- fmt clean ×3; clippy `--all-targets -D warnings` exit 0 ×3.
- Suites: mig 364 default / 382 embedded; jrn 362 (lib 306 + **bin 2**
  + integration 54) / 380 embedded; new 399 (lib 345) / 417 embedded —
  0 failures, exact expected counts.
- All named regression filters pass (17 filter runs).
- **All three red-proofs replicated on parent `0da326a`** with the exact
  predicted failure shapes: mig `an unwritable tmp must abort the finish`;
  jrn `got []`; new `got Cdk(HttpError(None, "Connection refused…"))`
  — exit 101 each; files restored, worktrees clean before removal.
- Binary surface: `--help` exit 0; `-v` prints `tollgate version 0.1.0`;
  `version` subcommand requires the service socket (SKIP, recorded).
- The shared `CARGO_TARGET_DIR` footgun was confirmed real (stale bin test
  binary reporting 0 tests) and neutralized per-worktree.

## Close-out

- The three audit branches are pre-merge lineages of landed PR #58; keep
  them as local history or delete — their content is upstream at
  `274f001`. This docs branch (`audit/s13-codex-feedback`) is the
  campaign's durable record: verdicts, this review, and the summary.
- Residual actionable items live upstream now: the two MEDIUMs above
  should ride the #68 review stream (one-line pointers suffice).
- Campaign counts stand: 31 rows / 27 unique threads dispositioned,
  14 threads fixed (landed in #58), 7 issue drafts (design gaps being
  implemented in #68 rounds 3–8: gate re-open/rollback, durable
  applied-grant set, grant tombstones, terminal cleanup).
