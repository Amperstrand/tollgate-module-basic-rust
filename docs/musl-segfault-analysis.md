# Technical Findings: musl-static Binary Segfaults

## Date: 2026-07-27
## Status: Root cause identified, documented for production mitigation

---

## 1. Summary

Our Rust binary (`tollgate-module-basic-rust`) segfaults immediately when compiled
as a musl-static binary (`x86_64-unknown-linux-musl`) and run on GCP VMs or inside
QEMU TCG (software CPU emulation). The same binary works perfectly on our local
development machine and on real OpenWrt hardware.

A glibc-dynamic build (`cargo build --release` without musl target) works everywhere.

---

## 2. Root Cause Chain

### 2.1 Static-PIE + musl-gcc incompatibility

Since Rust 1.59 (PR [#70740](https://github.com/rust-lang/rust/pull/70740)), the
`x86_64-unknown-linux-musl` target defaults to **static-PIE** (Position Independent
Executable). This produces `static-pie linked` ELF binaries.

The `musl-gcc` wrapper (from Debian/Ubuntu's `musl-tools` package) does NOT support
`-static-pie`. The wrapper passes `-dynamic-linker /lib/ld-musl-x86_64.so.1` and
`-static` to the linker, which creates a **dynamically linked** binary that claims
to be static. This mismatch causes a segfault in musl's `_start_c()` during
initialization.

**Source**: [rust-lang/rust#95926](https://github.com/rust-lang/rust/issues/95926)

### 2.2 The segfault location

```
gdb backtrace:
#0  _start_c () at ../src_musl/crt/../ldso/dlstart.c:141
#1  _start ()
```

The segfault happens before `main()` is called — during musl's C runtime
initialization. The dynamic linker tries to resolve relocations that point to
unmapped memory because the musl-gcc wrapper's specs file creates an invalid
combination of static + dynamic-linker flags.

### 2.3 Why it works locally but not on GCP

Our local development machine has a patched musl-gcc (Fedora-style patch that
adds static-pie support to the specs file). GCP VMs use stock Ubuntu musl-tools
which lacks this patch.

**Fedora patch**:
[`musl-1.2.0-Support-static-pie-with-musl-gcc-specs.patch`](https://src.fedoraproject.org/rpms/musl/blob/rawhide/f/musl-1.2.0-Support-static-pie-with-musl-gcc-specs.patch)

### 2.4 Why simple C programs work but Rust+ring doesn't

A simple `printf("hello\n")` compiled with `musl-gcc` directly works because
it doesn't trigger the PIE code paths that the linker misconfigures. But Rust's
default `static-pie` + `ring` crate's assembly code (BoringSSL) triggers the
broken relocation handling.

### 2.5 ring crate's role

The `ring` crate (used by `rustls` for TLS) compiles BoringSSL C/assembly code
via `cc-rs`. This C code uses position-independent patterns that interact with
the broken static-PIE relocations. Additionally, `ring` uses the `getrandom`
crate for entropy, which on musl targets calls `libc::getrandom` — this syscall
wrapper can also be affected by the linking issues.

**Source**: [briansmith/ring#713](https://github.com/briansmith/ring/issues/713)

---

## 3. Known Workarounds

### 3.1 Fix: `-C relocation-model=static` (RECOMMENDED for musl)

```bash
RUSTFLAGS="-C relocation-model=static" cargo build --release --target x86_64-unknown-linux-musl
```

This disables PIE, producing a truly static binary that doesn't trigger the
musl-gcc wrapper bug. The binary is slightly less secure (no ASLR for the
text segment) but works correctly everywhere.

**Trade-off**: Minor security reduction (no PIE/ASLR for code segment).
**Binary size**: Minimal change.

### 3.2 Alternative: Use `link-self-contained` + `rust-lld`

```bash
RUSTFLAGS="-Clink-self-contained=yes -Clinker=rust-lld" cargo build --release --target x86_64-unknown-linux-musl
```

This bypasses the musl-gcc wrapper entirely by using Rust's self-contained
linker. Requires Rust 1.67+ (LLVM 15+).

**Trade-off**: May require additional configuration for C dependencies.

### 3.3 Alternative: Patch musl-gcc specs (Fedora patch)

Apply Fedora's patch to the musl-gcc specs file:
```
sudo patch /usr/lib/x86_64-linux-musl/musl-gcc.specs < fedora-static-pie.patch
```

**Trade-off**: System-specific, must be applied on every build machine.

### 3.4 For GCP testing: Use glibc build

```bash
cargo build --release  # No --target, uses host glibc
```

Works on GCP but produces dynamically linked binary (not suitable for OpenWrt).

---

## 4. Impact on OpenWrt Production

### 4.1 Does this affect real OpenWrt hardware?

**No.** OpenWrt uses its own musl toolchain (`musl-cross-make`) which does not
use the `musl-gcc` wrapper. The OpenWrt SDK links binaries correctly with
static-PIE support. The segfault only occurs with Debian/Ubuntu's `musl-tools`
package.

### 4.2 Does this affect our CI pipeline?

**Yes.** Our CI uses GitHub Actions on Ubuntu runners which install `musl-tools`
via `apt-get install musl-tools`. Cross-compiled musl binaries from this setup
may segfault on certain platforms.

**Fix for CI**: Add `RUSTFLAGS="-C relocation-model=static"` to the musl build
step in `.github/workflows/cross-compile.yml`.

### 4.3 Does this affect GCP cloud testing?

**Yes.** GCP VMs run Ubuntu with the stock musl-tools package. Musl-static
binaries built on GCP or deployed to GCP will segfault.

**Fix for GCP**: Use glibc builds for GCP testing, or apply
`RUSTFLAGS="-C relocation-model=static"` during build.

---

## 5. Recommended Fix for Cargo.toml

Add to `.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "relocation-model=static"]
```

This ensures ALL musl builds use static relocation model, avoiding the PIE bug.
Or set it via environment variable in CI:

```yaml
env:
  RUSTFLAGS: "-C relocation-model=static"
```

---

## 6. References

1. [rust-lang/rust#95926](https://github.com/rust-lang/rust/issues/95926) — Root cause: static-pie + musl-gcc wrapper
2. [rust-lang/rust#73661](https://github.com/rust-lang/rust/issues/73661) — relocation-model=static segfault (opt-level=0 only)
3. [rust-lang/rust#86712](https://github.com/rust-lang/rust/issues/86712) — LLD 12 musl segfault (fixed in LLVM 13+)
4. [rust-lang/rust#85543](https://github.com/rust-lang/rust/issues/85543) — Debug musl hello world segfault
5. [briansmith/ring#713](https://github.com/briansmith/ring/issues/713) — ring musl support and PIE verification
6. [rust-lang/rust#108878](https://github.com/rust-lang/rust/issues/108878) — Mixed static/dynamic linking on musl
7. [rust-lang/rust#154439](https://github.com/rust-lang/rust/issues/154439) — gettid + LTO + musl weak linkage segfault
8. [habitat-sh/habitat#8135](https://github.com/habitat-sh/habitat/pull/8135) — Habitat's fix: relocation-model=static
