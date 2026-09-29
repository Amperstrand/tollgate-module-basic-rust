#!/bin/bash
set -euo pipefail

# Build .ipk package for OpenWrt without requiring the OpenWrt SDK.
# An .ipk is an ar archive containing: debian-binary + control.tar.gz + data.tar.gz
#
# Usage:
#   ./packaging/build-ipk.sh [architecture] [output-dir]
#
# Examples:
#   ./packaging/build-ipk.sh                    # builds x86_64
#   ./packaging/build-ipk.sh aarch64             # builds aarch64
#   ./packaging/build-ipk.sh mips /tmp/output    # error: MIPS needs CI

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
ARCH="${1:-x86_64}"
OUTPUT_DIR="${2:-$REPO_DIR/dist}"
VERSION="$(grep '^version' "$REPO_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"

case "$ARCH" in
    x86_64)   MUSL_TARGET="x86_64-unknown-linux-musl"; GOARCH="amd64" ;;
    aarch64)  MUSL_TARGET="aarch64-unknown-linux-musl"; GOARCH="arm64" ;;
    armv7)    MUSL_TARGET="armv7-unknown-linux-musleabihf"; GOARCH="arm"; GOARM="7" ;;
    mips)    echo "MIPS requires nightly + build-std. Use CI instead." >&2; exit 1 ;;
    mipsel)  echo "MIPSEL requires nightly + build-std. Use CI instead." >&2; exit 1 ;;
    *)       echo "Unknown architecture: $ARCH" >&2; exit 1 ;;
esac

echo "=== Building .ipk for $ARCH ($MUSL_TARGET) v$VERSION ==="

# Step 1: Build binary
echo "--- Building binary ---"
cd "$REPO_DIR"
cargo build --release --target "$MUSL_TARGET"
# Resolve via cargo metadata: CARGO_TARGET_DIR redirects builds away from ./target
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TARGET_DIR" ] || TARGET_DIR="$REPO_DIR/target"
BINARY="$TARGET_DIR/$MUSL_TARGET/release/tollgate-module-basic-rust"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY" >&2
    exit 1
fi

# gonuts-export: first-boot gonuts→CDK wallet migration helper (main.rs
# invokes /usr/bin/gonuts-export when it finds a legacy Go wallet.db).
GONUTS_EXPORT_DIR="$REPO_DIR/tools/gonuts-export"
GONUTS_EXPORT="$SCRIPT_DIR/.gonuts-export-$ARCH"
echo "--- Building gonuts-export ($GOARCH, static) ---"
( cd "$GONUTS_EXPORT_DIR" && CGO_ENABLED=0 GOOS=linux GOARCH="$GOARCH" ${GOARM:+GOARM="$GOARM"} \
    go build -trimpath -ldflags="-s -w" -o "$GONUTS_EXPORT" ./ )

# Step 2: Create staging directory
STAGE=$(mktemp -d)
trap "rm -rf $STAGE" EXIT

echo "--- Staging files ---"
mkdir -p "$STAGE/data/usr/bin"
mkdir -p "$STAGE/data/etc/init.d"
mkdir -p "$STAGE/data/usr/local/bin"
mkdir -p "$STAGE/data/etc/nftables.d"
mkdir -p "$STAGE/data/etc/uci-defaults"
mkdir -p "$STAGE/data/etc/hotplug.d/iface"
mkdir -p "$STAGE/data/etc/tollgate"
mkdir -p "$STAGE/data/lib/upgrade/keep.d"

# Daemon binary — same path as the Go package it replaces
cp "$BINARY" "$STAGE/data/usr/bin/tollgate-wrt"
chmod 755 "$STAGE/data/usr/bin/tollgate-wrt"

# Operator CLI: same binary in client mode (any argv arg => CLI, none => server)
ln -s tollgate-wrt "$STAGE/data/usr/bin/tollgate"

# SSL helper shims (Go parity: thin wrappers over `tollgate ssl …`)
cp "$SCRIPT_DIR/files/usr/bin/tollgate-apply-ssl" "$STAGE/data/usr/bin/"
cp "$SCRIPT_DIR/files/usr/bin/tollgate-remove-ssl" "$STAGE/data/usr/bin/"
chmod 755 "$STAGE/data/usr/bin/tollgate-apply-ssl" "$STAGE/data/usr/bin/tollgate-remove-ssl"

# Self-update helper + first-login hook (Go parity)
cp "$SCRIPT_DIR/files/usr/bin/check_package_path" "$STAGE/data/usr/bin/"
chmod 755 "$STAGE/data/usr/bin/check_package_path"
cp "$SCRIPT_DIR/files/usr/local/bin/first-login-setup" "$STAGE/data/usr/local/bin/"
chmod 755 "$STAGE/data/usr/local/bin/first-login-setup"

# Migration helper
cp "$GONUTS_EXPORT" "$STAGE/data/usr/bin/gonuts-export"
chmod 755 "$STAGE/data/usr/bin/gonuts-export"

# Init script
cp "$SCRIPT_DIR/files/etc/init.d/tollgate-wrt" "$STAGE/data/etc/init.d/"
chmod 755 "$STAGE/data/etc/init.d/"*

# NDS enforcement + backend/admin firewall rules (Go parity)
cp "$SCRIPT_DIR/files/etc/nftables.d/20-nds-enforce.nft" "$STAGE/data/etc/nftables.d/"
cp "$SCRIPT_DIR/files/etc/nftables.d/30-backend-firewall.nft" "$STAGE/data/etc/nftables.d/"
cp "$SCRIPT_DIR/files/etc/nftables.d/31-admin-board-not-guest-reachable.nft" "$STAGE/data/etc/nftables.d/"

# UCI defaults (Go parity)
cp "$SCRIPT_DIR/files/etc/uci-defaults/99-tollgate-setup" "$STAGE/data/etc/uci-defaults/"
cp "$SCRIPT_DIR/files/etc/uci-defaults/90-tollgate-captive-portal-symlink" "$STAGE/data/etc/uci-defaults/"
chmod 755 "$STAGE/data/etc/uci-defaults/"*

# WAN-up service restart (Go parity)
cp "$SCRIPT_DIR/files/etc/hotplug.d/iface/95-tollgate-restart" "$STAGE/data/etc/hotplug.d/iface/"
chmod 755 "$STAGE/data/etc/hotplug.d/iface/"*

# Emergency clear (Rust-specific extra)
cp "$SCRIPT_DIR/files/etc/tollgate/emergency-clear.nft" "$STAGE/data/etc/tollgate/"

# Upgrade keep
cp "$SCRIPT_DIR/files/lib/upgrade/keep.d/tollgate" "$STAGE/data/lib/upgrade/keep.d/"

# Captive portal site
cp -r "$SCRIPT_DIR/files/tollgate-captive-portal-site" "$STAGE/data/etc/tollgate/"

# License
[ -f "$REPO_DIR/LICENSE-MIT" ] && cp "$REPO_DIR/LICENSE-MIT" "$STAGE/data/usr/share/doc/tollgate-wrt/LICENSE" 2>/dev/null \
    || { mkdir -p "$STAGE/data/usr/share/doc/tollgate-wrt"; cp "$REPO_DIR/LICENSE-MIT" "$STAGE/data/usr/share/doc/tollgate-wrt/LICENSE"; }

# Step 3: Create control metadata
echo "--- Creating control ---"
mkdir -p "$STAGE/control"

cat > "$STAGE/control/control" << CTRL
Package: tollgate-wrt
Version: $VERSION
Architecture: $ARCH
Maintainer: TollGate <tollgate@tollgate.me>
Section: net
Priority: optional
Depends: libc, nodogsplash, jq
Provides: nodogsplash-files
Description: TollGate payment gateway for OpenWrt (Rust implementation).
 Powered by Cashu ecash and CDK (Cashu Dev Kit). Drop-in replacement
 for the Go tollgate-wrt package.
CTRL

# Postinst (mirrors Go's opkg behavior: run uci-defaults, restart services)
cat > "$STAGE/control/postinst" << 'POST'
#!/bin/sh
echo "TollGate Rust post-installation..."
for script in /etc/uci-defaults/99-tollgate-setup /etc/uci-defaults/90-tollgate-captive-portal-symlink; do
    [ -x "$script" ] && "$script" || true
done
/etc/init.d/network restart 2>/dev/null || true
/etc/init.d/firewall reload 2>/dev/null || true
/etc/init.d/nodogsplash restart 2>/dev/null || true
if [ -x /etc/init.d/tollgate-wrt ]; then
    /etc/init.d/tollgate-wrt enable 2>/dev/null || true
    /etc/init.d/tollgate-wrt start 2>/dev/null || true
fi
echo "TollGate Rust installed successfully"
exit 0
POST
chmod 755 "$STAGE/control/postinst"

# Preinst (install_time stamp, same as Go)
cat > "$STAGE/control/preinst" << 'PRE'
#!/bin/sh
mkdir -p /etc/tollgate
if [ -f /etc/tollgate/install.json ]; then
    jq ".install_time = $(date +%s)" /etc/tollgate/install.json > /tmp/install.json.tmp && \
    mv /tmp/install.json.tmp /etc/tollgate/install.json
else
    echo "{\"install_time\": $(date +%s)}" > /etc/tollgate/install.json
fi
exit 0
PRE
chmod 755 "$STAGE/control/preinst"

# Step 4: Create .ipk
echo "--- Creating .ipk ---"
mkdir -p "$OUTPUT_DIR"

echo "2.0" > "$STAGE/debian-binary"

( cd "$STAGE/control" && tar czf "$STAGE/control.tar.gz" . )
( cd "$STAGE/data" && tar czf "$STAGE/data.tar.gz" . )

IPK_NAME="tollgate-wrt_${VERSION}_${ARCH}.ipk"
IPK_PATH="$OUTPUT_DIR/$IPK_NAME"

# OpenWrt opkg-build ipk format: a gzipped tar wrapping debian-binary +
# data.tar.gz + control.tar.gz. (opkg also reads the Debian ar format, but
# opkg 24.10 in the field rejects ar-wrapped ipks built by busybox ar, and
# OpenWrt-side tooling assumes the tar.gz layout.)
rm -f "$IPK_PATH"
( cd "$STAGE" && tar --format=gnu --owner=0 --group=0 --numeric-owner \
    -cf - ./debian-binary ./data.tar.gz ./control.tar.gz \
    | gzip -n > "$IPK_PATH" )

echo ""
echo "=== IPK BUILD COMPLETE ==="
ls -lh "$IPK_PATH"
echo ""
echo "Package: tollgate-wrt v$VERSION ($ARCH)"
echo "Files: $(tar tzf "$STAGE/data.tar.gz" | wc -l)"
echo ""
echo "Install on OpenWrt:"
echo "  scp $IPK_PATH root@192.168.1.1:/tmp/"
echo "  ssh root@192.168.1.1 'opkg install /tmp/$IPK_NAME'"
