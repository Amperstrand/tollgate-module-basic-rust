# Testing tollgate-module-basic-rust

## Quick start

```bash
# Build
cargo build --release

# Start for local testing (kills any existing process on port 2121)
./scripts/tollgate-test.sh

# Run unit tests
cargo test
```

## Test categories

### 1. Unit tests (194 tests, 0 failures)

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Covers: config validation, session management, wallet operations, HTTP routes,
upstream detector, wireless scanner/connector, reseller mode, CLI commands.

### 2. Live payment test (local)

Prerequisites:
- testnut.cashu.exchange reachable (FakeWallet, auto-pays invoices)
- Cashu CLI installed (`pip install cashu`)
- `/tmp/dhcp.leases` writable

```bash
# Start tollgate-rs
./scripts/tollgate-test.sh &

# Mint a Cashu token
cashu -h https://testnut.cashu.exchange -y -t invoice 5
TOKEN=$(cashu -h https://testnut.cashu.exchange -y -t send 4)

# Pay
curl -X POST http://127.0.0.1:2121/ \
  -H "Content-Type: text/plain" \
  -H "X-Forwarded-For: 10.0.0.42" \
  -d "$TOKEN"

# Expected: kind=1022 (session granted)
# Check balance:
curl http://127.0.0.1:2121/balance -H "X-Forwarded-For: 10.0.0.42"
# Expected: {"session_active":true,"allotment":15000,...}
```

### 3. PRTA integration tests (on GCP)

```bash
# Create GCP VM
gcloud compute instances create tollgate-test \
    --zone=us-east1-b --machine-type=n2-standard-4 \
    --image-family=ubuntu-2204-lts --image-project=ubuntu-os-cloud

# SSH in, install deps, run tests
gcloud compute ssh tollgate-test --zone=us-east1-b
sudo apt-get install -y build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
git clone https://github.com/Amperstrand/tollgate-module-basic-rust.git
cd tollgate-module-basic-rust && cargo build --release
git clone https://github.com/OpenTollGate/physical-router-test-automation.git /tmp/prta
pip3 install --user requests pytest pytest-timeout coincurve cashu

export TOLLGATE_BACKEND=rust-basic
export TOLLGATE_BINARY_PATH=$(pwd)/target/release/tollgate-module-basic-rust
cd /tmp/prta
pytest tests/api/test_rust_basic*.py -v --timeout=30
```

### 4. MIPS cross-compilation (CI)

Automatic on every push to main. Checks all 5 targets:
- x86_64-unknown-linux-musl
- aarch64-unknown-linux-musl
- armv7-unknown-linux-musleabihf
- mips-unknown-linux-musl (requires nightly + build-std)
- mipsel-unknown-linux-musl (requires nightly + build-std)

### 5. Wallet migration test

```bash
# 1. Run Go TollGate (creates wallet.db)
TOLLGATE_TEST_CONFIG_DIR=/tmp/tg-go /path/to/tollgate-go &

# 2. Copy Go state
cp -r /tmp/tg-go /tmp/tg-rust

# 3. Run Rust TollGate (auto-migrates)
TOLLGATE_TEST_CONFIG_DIR=/tmp/tg-rust /path/to/tollgate-rs

# 4. Verify:
# - /tmp/tg-rust/.migration_complete exists
# - /tmp/tg-rust/wallet.db.pre-migration exists (renamed)
# - /tmp/tg-rust/wallet.sqlite exists (new CDK database)
# - HTTP discovery responds with preserved config
```

## Common gotchas

### Port 2121 already in use

**Symptom:** Binary panics or another process serves stale responses.

**Cause:** Another TollGate binary is running (e.g., from a previous session).

**Fix:**
```bash
sudo ss -tlnp sport = :2121    # Find the PID
sudo kill -9 <PID>              # Kill it
# Or just: fuser -k 2121/tcp
```

**Prevention:** Use `./scripts/tollgate-test.sh` which kills existing processes first.

### MAC address resolution fails

**Symptom:** `{"error":"mac-address-lookup-failed"}` on POST /.

**Cause:** The binary resolves client MAC from `/tmp/dhcp.leases` then `/proc/net/arp`. On non-OpenWrt hosts, neither has entries for test IPs.

**Fix:**
```bash
echo "9999999999 02:00:00:00:00:01 10.0.0.42 test-client *" > /tmp/dhcp.leases
# Then send X-Forwarded-For: 10.0.0.42 in payment requests
```

### Cashu CLI marshmallow error

**Symptom:** `AttributeError: module 'marshmallow' has no attribute '__version_info__'`

**Cause:** environs package incompatible with marshmallow version.

**Fix:**
```bash
pip install "marshmallow==3.20.1" --force-reinstall
```

### MIPS build requires nightly

**Cause:** `mips-unknown-linux-musl` has no prebuilt std on stable Rust.

**Fix:** CI uses `dtolnay/rust-toolchain@nightly` with `-Z build-std=std,panic_abort`.

### cdk-common AtomicU64 on 32-bit MIPS

**Cause:** `cdk-common` 0.17.3 (from crates.io) uses `AtomicU64` in test
modules, which is unavailable on 32-bit MIPS (`mips-unknown-linux-musl`).

**Fix:** `Cargo.toml` has a `[patch.crates-io]` entry pointing to
[Amperstrand/cdk-common](https://github.com/Amperstrand/cdk-common) — an
exact clone of crates.io 0.17.3 with only `AtomicU64 → AtomicUsize` in
test modules. Same version, same API, MIPS-safe atomics. Remove the patch
when cashubtc/cdk publishes a compatible release (0.17.4+) to crates.io.

## Test matrix

| Test | Local | GCP | OpenWrt VM | Physical Router |
|------|-------|-----|------------|-----------------|
| Unit tests (194) | ✅ | ✅ | ✅ | ✅ |
| HTTP discovery | ✅ | ✅ | ✅ | ✅ |
| Payment (Cashu) | ✅ | ✅ | ✅ | Pending |
| LN invoice | ✅ | ✅ | ✅ | Pending |
| Config validation | ✅ | ✅ | ✅ | ✅ |
| CLI commands | ✅ | ✅ | ✅ | ✅ |
| ndsctl integration | ❌ | ❌ | Pending | Pending |
| WiFi scanning | ❌ | ❌ | Pending | Pending |
| Wallet migration | ✅ | ✅ | Pending | Pending |
| MIPS compilation | ❌ | ❌ | ✅ (CI) | Pending |
| Physical deployment | ❌ | ❌ | ❌ | Pending |
