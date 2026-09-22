# AGENTS.md

<!-- markdownlint-disable MD013 -->

Instructions for AI coding agents working in this repository.

> TollGate-Rust is a drop-in replacement for
> [tollgate-module-basic-go](https://github.com/OpenTollGate/tollgate-module-basic-go),
> using [CDK](https://github.com/cashubtc/cdk) as the Cashu wallet. The Go
> binary remains production until this one proves parity **including crash
> consistency** — feature parity alone is not correctness parity.

## Orientation

- Rust code lives in [src/](src/); HTTP surface in `src/http/routes/`,
  wallet wrapper in `src/wallet/`, sessions in `src/session/`,
  captive portal in `src/portal/`, payout in `src/payout.rs`.
- Build: `cargo build --release`. Test: `cargo test` (and
  `cargo test --features embedded-portal`). Cross-compile lanes live in
  `.github/workflows/cross-compile.yml`; MIPS/MIPSEL need nightly +
  `build-std` and the `Amperstrand/cdk-common` fork (mipsel has no 64-bit
  atomics) — read `Cargo.toml`'s patch note before bumping cdk deps.
- Much of the behavior only exists on a real router (`ndsctl`, nftables,
  Wi-Fi). The Go repo's `tests/cloud-lab/` and PRTA
  (physical-router-test-automation) run against this backend too
  (`--backend rust-basic`) — extend those rather than inventing new
  harnesses.

## Fund safety, crash consistency, and distributed transaction invariants

Any change touching payments, wallets, sessions, gates, mints, payouts,
retries, or persistent identifiers is a **distributed state-machine
change**, not a local edit. The router can lose power, the process can be
killed, the mint can rate-limit (429), time out, return 5xx, restart, or
accept a request and lose the response. Every such change must state:
the point of no return, what durable state is written **before** it,
recovery behavior after a crash at each step, retry and duplicate-
execution behavior, and the compensating action if a later step fails.

Hard rules (each maps to a real failure class — see the parity issue
backlog for details):

- **Never reuse deterministic Cashu derivation outputs.** Once a
  derivation range `[counter, counter+n)` has been exposed to a mint it
  must never be derived again. CDK guarantees this *inside* a wallet
  operation (atomic `keyset_counter` row keyed by keyset id, saga
  persists counter range before the network call). **Do not break the
  guarantee from outside CDK**: never derive messages yourself, never
  retry a dropped wallet future by replaying the same inputs without
  reconciling, and remember `tokio::time::timeout` **cancels** the
  operation mid-saga — the saga survives in SQLite but nothing in this
  process retries it until the next `ensure_mint`.
- **Wallet-level atomicity is NOT application-level atomicity.**
  `wallet.receive()` completing (or being recoverable by CDK) does not
  mean `payment → session → gate → HTTP response` is atomic. CDK
  recovering a swap says nothing about whether the customer got a
  session. Business-level recovery is *this repo's* job.
- **Do not perform irreversible monetary operations before validations
  that can be done locally.** Token parse, mint acceptance,
  spending-condition rejection, minimum-purchase arithmetic, and
  pricing must all run **before** `wallet.receive()`. Today the
  minimum-purchase check runs after receive — that ordering is a known
  fund-loss defect; do not add new validations after value moves.
- **Every irreversible operation followed by fallible work needs either
  durable forward recovery or a compensating action.** If the process
  dies between receive and session-create, the token is spent and the
  customer has nothing. Design for: durable payment records, startup
  reconciliation (CDK sagas **and** TollGate payments), and refunds via
  replacement tokens or durable customer credit when forward recovery
  is impossible.
- **Ambiguous network results must be reconciled, not blindly
  retried.** A 30s timeout on receive/melt is ambiguity, not failure:
  query the mint's quote/proof state (NUT-07 checkstate, NUT-04/05
  quote state) before answering the customer or retrying.
- **Retries must be idempotent or use fresh state.** A duplicate POST
  of the same token, or a customer retrying after a timeout, must never
  double-grant or double-report. Payment idempotency keys belong at the
  TollGate layer (hash of token + mint), not inside CDK.
- **Partial successes must never be discarded.** Payouts that paid one
  recipient and failed the next must record what was paid, durably,
  before attempting the next.
- **Process-memory state must not be authoritative where restart
  correctness matters.** Known in-memory-only state today: the
  `/ln-invoice` quote store (`QUOTE_STORE` static map — lost on
  restart), the wallet map (per-mint wallets registered at boot). Quote
  state, payment state, and pending compensations must be persisted
  (Go persists Lightning quotes to disk; this repo must too).
- **Canonicalize persistent mint identities at every boundary.** CDK
  canonicalizes `MintUrl` (lowercase scheme/host, trailing-slash trim)
  at the type level, but this repo keys wallets, config comparisons and
  accepted-mint sets by raw strings with only a trailing-slash trim.
  Uppercase-host config vs lowercase-host token ⇒ spurious
  `WalletNotFound`/`MintNotAccepted`. Route every persisted/compared
  mint URL through one canonical form (CDK's `MintUrl::from_str`).
- **Migrations must preserve value even when individual items fail.**
  The first-boot gonuts→CDK migration renames the old wallet even when
  token imports failed; failed tokens are currently only counted, not
  retained. Any migration change must keep failed items recoverable.

Before modifying wallet/payment logic, research first — in this order:
the relevant Cashu NUTs (NUT-02 keysets/fees, NUT-03 swap, NUT-04 mint
quotes, NUT-05 melt, NUT-07 checkstate, NUT-19 error semantics,
NUT-08 fees), current CDK behavior **from source**
(`crates/cdk/src/wallet/{swap,send,receive,melt}/saga/`,
`recovery.rs` — do not trust prose descriptions of the saga, including
this repo's README), the Go implementation's behavior
(`OpenTollGate/tollgate-module-basic-go` is the reference for
wire-format and semantics), and existing issues in both repos.

> Do not guess about Cashu or Lightning protocol behavior from local
> wrappers alone. Use web research, z.ai/zread, upstream source,
> specifications, and existing issue history before making
> protocol-sensitive changes.

Tests for payment/wallet changes must kill/restart the process at
transaction boundaries (after receive before session, after session
before gate, after gate before response) and exercise network failure,
429, timeout, mint restart, router restart, mint URL aliases, keyset
rotation, and partial failure. See the shared conformance matrix issue
for the scenario list; PRTA's `--backend rust-basic` lanes are the
execution venue.

## CDK responsibility boundaries — do not duplicate protocol logic

CDK owns: keyset fetching/rotation, deterministic derivation + counters,
swap/send/melt/receive sagas and crash recovery
(`recover_incomplete_sagas`), proof state transitions (Unspent /
Reserved / Pending / Spent), quote lifecycle, DLEQ validation, token
encoding. **Do not reimplement any of that here.** This repo owns:
when to call CDK, what must be durable *around* CDK calls
(payments, sessions, business-level recovery), validation ordering,
idempotency, HTTP semantics, pricing, gate/portal integration, and
migration.

Rules of thumb:

- A money-moving CDK call (`receive`, `send`, `prepare_send().confirm()`,
  `mint_quote`, `prepare_melt().confirm()`) must be wrapped in a
  TollGate durable record created **before** the call, advanced after
  each step, and reconciled at startup. Never fire-and-forget.
- Don't wrap CDK calls in `timeout()` without a recovery story for the
  cancelled future: the saga persists, but only `ensure_mint` re-runs
  `recover_incomplete_sagas`. If you add a timeout, add the
  reconciliation that follows it.
- Don't parse/compare mint URLs as raw strings — use the same
  canonicalization as CDK (`MintUrl`), or a helper that mirrors it,
  everywhere a URL becomes a key (wallet map, accepted-mint set,
  config matching, DB filenames).
- One CDK wallet per (canonical mint URL, unit). Two wallet instances
  over one mint's SQLite file will corrupt the derivation counter
  ownership model — don't open the same mint twice under different
  spellings.

## Persistence model (SQLite on OpenWrt)

- CDK state lives in per-mint SQLite files
  (`<sanitized-mint-url>.sqlite`) — proofs, keysets, counters, quotes,
  saga records. `wallet_seed.bin` is a **random 64-byte seed with no
  mnemonic representation** — losing it loses derivable future
  outputs; do not delete, regenerate, or move it casually, and note the
  backup story is a known gap vs Go's mnemonic.
- TollGate state: `sessions.json` (atomic tmp+rename, but debounced —
  see the session-persistence parity issue), config/identity files.
  Debounced writes are fine for metering, **not** for payment-grant
  durability: a payment response returned before the session write is
  durable is a fund-safety window.
- Flash wear is real: prefer append-only journals + periodic
  consolidation over full-file rewrites on every event; never fsync in
  a hot loop; remember `sessions.json` rewrite-per-payment is O(sessions)
  write amplification.

## Musl/OpenWrt targets

- Targets: `x86_64`, `aarch64`, `armv7` (musl), `mips`/`mipsel`
  (nightly + `build-std`, `panic_abort`, cdk-common fork for
  32-bit-atomics). Bumping `cdk`/`cdk-common`/`cashu` requires
  verifying **all** lanes still cross-compile, and that the
  AtomicU64 port still applies — check
  `.github/workflows/cross-compile.yml` before merging dependency
  bumps.
- `reqwest` uses rustls only (no OpenSSL); keep it that way for musl.
- Binary size is a first-class constraint (opt-level=z, LTO, strip);
  measure before/after for any dependency change.
