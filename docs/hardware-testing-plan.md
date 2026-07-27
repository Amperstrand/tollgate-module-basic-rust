# Hardware Testing Plan — tollgate-module-basic-rust

> **Status**: Planning document for Phase 14 (OpenWrt hardware testing)
> **Prerequisites**: Phases 0-13 complete (194 + 212 cargo tests pass, CI green)

---

## 1. Scope

Validate the tollgate-module-basic-rust binary on physical OpenWrt routers,
covering: payment flow, captive portal enforcement, session lifecycle,
performance, stability, and cross-architecture compatibility.

---

## 2. Hardware Matrix

### Minimum Requirements
- **RAM**: 512MB (Rust runtime ~50MB + CDK wallet ~50MB + system ~100MB)
- **Flash**: 32MB (binary ~10MB + CDK SQLite + config)
- **Architecture**: x86_64, aarch64, or armv7 (MIPS requires nightly + build-std)

### Recommended Test Devices

| Device | Arch | RAM | Flash | WiFi | Status |
|--------|------|-----|-------|------|--------|
| GL.iNet GL-MT6000 | aarch64 | 512MB | 32MB | WiFi 6 | In OpenWrt labs |
| GL.iNet GL-MT3000 | aarch64 | 512MB | 32MB | WiFi 6 | PRTA test target |
| OpenWrt One | aarch64 | 1GB | — | WiFi 6 | Official test device |
| x86_64 mini PC | x86_64 | 4GB+ | 64GB+ | — | CI/QEMU baseline |
| GL.iNet AR150 | mips | 64MB | 16MB | WiFi N | Legacy target (if feasible) |

### Architecture Build Matrix

| Target | Cross-compile | Binary Size | Notes |
|--------|--------------|-------------|-------|
| x86_64-unknown-linux-musl | ✅ stable | ~10MB | CI verified, works on QEMU KVM |
| aarch64-unknown-linux-musl | ✅ stable | ~10MB | GL.iNet MT3000/MT6000 |
| armv7-unknown-linux-musleabihf | ✅ stable | ~10MB | Needs testing |
| mips-unknown-linux-musl | ⚠️ nightly | ~10MB | CI verified, needs hardware test |
| mipsel-unknown-linux-musl | ⚠️ nightly | ~10MB | CI verified, needs hardware test |

---

## 3. Known Issues and Mitigations

### 3.1 musl-static segfault on GCP/cloud VMs

**Root cause**: Rust's `static-pie` default + Debian/Ubuntu `musl-gcc` wrapper
produces binaries that segfault in `_start_c()` on certain kernels.

**Impact**: Binary segfaults on GCP VMs and QEMU TCG (software emulation).
Does NOT affect real OpenWrt hardware (OpenWrt's own toolchain handles this).

**Fix applied**: `.cargo/config.toml` sets `relocation-model=static` for musl.
See `docs/musl-segfault-analysis.md` for full analysis.

**Additional finding**: `getrandom` crate's `dlsym()` usage on musl static binaries
can cause segfaults on kernels with non-standard vDSO. getrandom v0.4.3+ fixes
this by using static linking for musl targets. Consider upgrading if segfaults
persist on specific hardware.

### 3.2 NDS blocks SSH by default

**Issue**: NoDogSplash blocks all traffic to the router except allowed ports.

**Fix**: Add port 22 to NDS `users_to_router` config:
```
list users_to_router 'allow tcp port 22'
```

### 3.3 ndsctl auth fails without client traffic

**Issue**: `ndsctl auth <mac>` fails if NDS hasn't detected the client yet.

**Fix**: Client must attempt internet access (e.g., `curl http://1.1.1.1`)
before payment to trigger NDS detection.

### 3.4 OpenWrt VM RAM requirement

**Issue**: QEMU VM with 256MB RAM crashes when running NDS + tollgate + CDK wallet.

**Fix**: Use 512MB+ RAM for QEMU VMs. Physical routers with 512MB+ are unaffected.

---

## 4. Test Phases

### Phase A: Pre-flight (on dev machine)

```
A1. cargo test                                          — 194 tests pass
A2. cargo test --features embedded-portal                — 212 tests pass
A3. cargo clippy --all-targets -- -D warnings            — 0 errors
A4. cargo build --release --target <arch>-musl           — binary builds
A5. Binary --version runs without segfault               — EXIT=0
```

### Phase B: Deployment to physical router

```
B1. SCP binary to router                                — binary transferred
B2. Install NDS + dependencies via opkg                  — packages installed
B3. Deploy nftables enforcement include (.nft)           — fw4 reload succeeds
B4. Deploy captive portal site assets                    — splash.html accessible
B5. Configure NDS UCI (gateway port, allowed ports)      — NDS starts
B6. Start tollgate binary                                — HTTP :2121 responds
B7. Verify: /balance returns valid JSON                  — HTTP 200
B8. Verify: /discovery returns kind=10021                — HTTP 200
B9. Verify: /whoami returns client MAC                   — HTTP 200
B10. Verify: ndsctl status shows running                 — Version, uptime
```

### Phase C: Captive portal enforcement

```
C1. Client connects to WiFi                              — gets DHCP lease
C2. Client curls external URL                            — HTTP 302 redirect to portal
C3. Client loads splash page on :80                      — HTML renders
C4. Client enters Cashu token + clicks Pay              — POST to :2121
C5. ndsctl auth succeeds                                 — state=Authenticated
C6. Client can reach internet                            — HTTP 200 from external
C7. Monitor tracks session usage                         — /balance shows active
C8. Session expires after allotment                      — /balance shows inactive
C9. ndsctl deauth succeeds                               — state=Preauthenticated
C10. Client blocked again                                — HTTP 302 redirect
```

### Phase D: Payment flow (with real Cashu tokens)

```
D1. Mint token from testnut.cashu.exchange               — token generated
D2. POST token to /                                      — kind=1022 accepted
D3. Verify allotment calculation                         — correct ms/bytes
D4. Monitor tracks elapsed time                          — usage increments
D5. Session expires at correct time                      — within ±2s of allotment
D6. Wallet balance reflects received tokens              — CLI wallet balance
D7. Multiple sequential payments                         — each creates new session
D8. Payment with insufficient token                      — kind=21023 rejected
D9. Payment with invalid token                           — kind=21023 rejected
D10. Payment with expired token                          — kind=21023 rejected
```

### Phase E: Performance and stability

```
E1. iperf3 throughput under load                         — >80% of wire speed
E2. Memory usage after 1h                                — <70% of total RAM
E3. CPU usage during payment                             — <50% peak
E4. 24h stability test (automated payments every 60s)    — no crashes
E5. Concurrent client test (5+ simultaneous)             — all authenticated
E6. Recovery from mint outage (degraded mode)            — graceful fallback
E7. Recovery from NDS restart                             — tollgate continues
E8. Recovery from network reconnect                       — sessions persist
```

### Phase F: Embedded portal (--features embedded-portal)

```
F1. Deploy embedded-portal binary                         — starts without NDS
F2. nftables table inet tollgate installed                — priority -1, policy accept
F3. nftables coexists with fw4                            — no VM crash
F4. Client blocked by nft forward chain                   — traffic rejected
F5. Payment adds client to authenticated_v4 set           — set membership changes
F6. Per-client named counter created                      — c-<ip> in nft list
F7. Counter increments with traffic                       — bytes > 0
F8. Session expiry removes from set                       — set empty after expiry
F9. Watchdog removes table on crash                       — table deleted after 90s
F10. Emergency-clear.nft works                            — manual recovery
```

### Phase G: Cross-architecture validation

```
G1. Build for aarch64-unknown-linux-musl                  — binary builds
G2. Deploy to GL.iNet MT3000 (ARM64)                      — binary runs
G3. Full captive portal flow on ARM64                     — all Phase C steps pass
G4. Build for mips-unknown-linux-musl (nightly)           — binary builds
G5. Deploy to MIPS router (if available)                   — binary runs
G6. Full captive portal flow on MIPS                      — all Phase C steps pass
```

---

## 5. Test Infrastructure

### Tools Available on OpenWrt

| Tool | Package | Purpose |
|------|---------|---------|
| curl | curl | HTTP API testing |
| ndsctl | nodogsplash | Captive portal state inspection |
| nft | nftables | Firewall ruleset verification |
| iperf3 | iperf3 | Network throughput benchmarking |
| tc + netem | kmod-sched-core | Network emulation (delay, loss) |
| logread | logread | System log inspection |
| uci | uci | Configuration management |

### PRTA Integration

The Physical Router Test Automation (PRTA) framework at
`/home/ubuntu/src/physical-router-test-automation/` provides:

- **Local KVM mode**: QEMU VMs on dev machine (proven working)
- **GCP cloud mode**: Nested VMs using `nested-ubuntu` image (needs `enable-vmx`)
- **Physical mode**: Direct SSH to real routers via `TOLLGATE_SSH_HOST`
- **Mock mode**: Local binary testing without VMs (`TOLLGATE_MOCK=1`)

### GCP Cloud Testing Setup

```bash
# Create VM from nested-ubuntu image (has enable-vmx license for KVM)
gcloud compute instances create tollgate-test \
    --zone=us-central1-a \
    --machine-type=n1-standard-8 \
    --image=nested-ubuntu \
    --boot-disk-size=50GB

# SSH in, setup environment, run start-poc
gcloud compute ssh tollgate-test --zone=us-central1-a
sudo apt-get install -y qemu-system-x86 qemu-utils sshpass python3-pip
git clone https://github.com/OpenTollGate/physical-router-test-automation.git
cd physical-router-test-automation
PYTHONPATH=. python3 scripts/virtual-lab.py start-poc --host localhost
```

**Important**: Use `nested-ubuntu` image (NOT default Ubuntu). It has the
`enable-vmx` GCP license that provides `/dev/kvm` for nested virtualization.
Default Ubuntu images do NOT have KVM support.

### OpenWrt Labs Integration

The official OpenWrt testing framework (`openwrt-tests` by aparcar) uses
Labgrid for device control. GL.iNet MT1300 and MT6000 are available in
distributed labs:

```bash
# Access OpenWrt lab devices
export LG_PLACE=aparcar-glinet_gl-mt6000
export LG_PROXY=labgrid-aparcar
uv run labgrid-client lock
uv run labgrid-client power cycle
pytest tests/
uv run labgrid-client unlock
```

---

## 6. Safety Mechanisms to Validate

| Mechanism | How to Test | Expected Result |
|-----------|-------------|-----------------|
| Watchdog (60s) | Kill tollgate binary, wait 90s | nft table auto-removed |
| Emergency-clear | `nft -f /etc/tollgate/emergency-clear.nft` | All traffic flows again |
| Graceful degradation | Block mint URL in firewall | API returns degraded notice |
| Session persistence | Restart binary mid-session | Sessions.json restored |
| Config atomicity | Kill during config write | Config not corrupted |

---

## 7. Binary Size Targets

| Target | Stripped Size | Notes |
|--------|--------------|-------|
| x86_64-musl | ~10MB | panic=unwind, relocation-model=static |
| aarch64-musl | ~10MB | For GL.iNet MT3000/MT6000 |
| mips-musl | ~10MB | Nightly + build-std required |

**Flash requirement**: 32MB minimum (binary + CDK SQLite + captive portal site).

---

## 8. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| musl segfault on specific hardware | Low | Critical | Test binary first; carry glibc fallback |
| nftables conflict with fw4 | Medium | High | Use priority -1, policy accept |
| NDS incompatibility | Low | Medium | Test with NDS 5.0.2+ |
| RAM exhaustion | Medium | High | Monitor with `free`; require 512MB+ |
| CDK wallet corruption | Low | Critical | SQLite WAL mode; backup wallet.sqlite |
| WiFi driver issues | Medium | Medium | Test on multiple router models |
| MIPS build toolchain | High | Medium | Nightly + build-std; test on real MIPS |

---

## 9. Success Criteria

Phase 14 is complete when:

1. ✅ Binary runs on at least 2 physical router models (1 ARM64 + 1 x86_64)
2. ✅ Full captive portal E2E: blocked → pay → internet → expire → blocked
3. ✅ 24h stability test passes (no crashes, memory leaks)
4. ✅ Performance: <20% throughput degradation vs baseline
5. ✅ Recovery: watchdog + emergency-clear validated
6. ✅ Migration: Go wallet.db successfully migrated on real hardware
7. ✅ NDS + nftables enforcement working with fw4 coexistence

---

## 10. References

- [musl-segfault-analysis.md](musl-segfault-analysis.md) — Root cause of segfaults
- [full-parity-plan.md](full-parity-plan.md) — Original 14-phase plan
- [embedded-portal-plan.md](embedded-portal-plan.md) — Portal safety requirements
- [spec-compliance-report.md](spec-compliance-report.md) — TollGate spec tests
- [openwrt-tests](https://github.com/aparcar/openwrt-tests) — Official testing framework
- [PRTA](https://github.com/OpenTollGate/physical-router-test-automation) — Our test automation
