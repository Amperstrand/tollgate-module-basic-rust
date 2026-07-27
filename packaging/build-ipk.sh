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
#   ./packaging/build-ipk.sh mips /tmp/output    # builds MIPS, outputs to /tmp/output

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
ARCH="${1:-x86_64}"
OUTPUT_DIR="${2:-$REPO_DIR/dist}"
VERSION="$(grep '^version' "$REPO_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"

case "$ARCH" in
    x86_64)   MUSL_TARGET="x86_64-unknown-linux-musl" ;;
    aarch64)  MUSL_TARGET="aarch64-unknown-linux-musl" ;;
    armv7)    MUSL_TARGET="armv7-unknown-linux-musleabihf" ;;
    mips)    echo "MIPS requires nightly + build-std. Use CI instead." >&2; exit 1 ;;
    mipsel)  echo "MIPSEL requires nightly + build-std. Use CI instead." >&2; exit 1 ;;
    *)       echo "Unknown architecture: $ARCH" >&2; exit 1 ;;
esac

echo "=== Building .ipk for $ARCH ($MUSL_TARGET) v$VERSION ==="

# Step 1: Build binary
echo "--- Building binary ---"
cd "$REPO_DIR"
cargo build --release --target "$MUSL_TARGET"
BINARY="target/$MUSL_TARGET/release/tollgate-module-basic-rust"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY" >&2
    exit 1
fi

# Step 2: Create staging directory
STAGE=$(mktemp -d)
trap "rm -rf $STAGE" EXIT

echo "--- Staging files ---"
mkdir -p "$STAGE/data/usr/bin"
mkdir -p "$STAGE/data/etc/init.d"
mkdir -p "$STAGE/data/etc/nftables.d"
mkdir -p "$STAGE/data/etc/uci-defaults"
mkdir -p "$STAGE/data/etc/tollgate"
mkdir -p "$STAGE/data/lib/upgrade/keep.d"

# Binary
cp "$BINARY" "$STAGE/data/usr/bin/tollgate"
chmod 755 "$STAGE/data/usr/bin/tollgate"

# Init scripts
cp "$SCRIPT_DIR/files/etc/init.d/tollgate" "$STAGE/data/etc/init.d/"
cp "$SCRIPT_DIR/files/etc/init.d/tollgate-wrt" "$STAGE/data/etc/init.d/"
chmod 755 "$STAGE/data/etc/init.d/"*

# NDS enforcement
cp "$SCRIPT_DIR/files/etc/nftables.d/20-nds-enforce.nft" "$STAGE/data/etc/nftables.d/"

# UCI defaults
cp "$SCRIPT_DIR/files/etc/uci-defaults/99-tollgate-setup" "$STAGE/data/etc/uci-defaults/"
cp "$SCRIPT_DIR/files/etc/uci-defaults/90-tollgate-captive-portal-symlink" "$STAGE/data/etc/uci-defaults/"
chmod 755 "$STAGE/data/etc/uci-defaults/"*

# Emergency clear
cp "$SCRIPT_DIR/files/etc/tollgate/emergency-clear.nft" "$STAGE/data/etc/tollgate/"

# Upgrade keep
cp "$SCRIPT_DIR/files/lib/upgrade/keep.d/tollgate" "$STAGE/data/lib/upgrade/keep.d/"

# Captive portal site
cp -r "$SCRIPT_DIR/files/tollgate-captive-portal-site" "$STAGE/data/etc/tollgate/"

# License
[ -f "$REPO_DIR/LICENSE-MIT" ] && cp "$REPO_DIR/LICENSE-MIT" "$STAGE/data/etc/tollgate/"

# Step 3: Create control metadata
echo "--- Creating control ---"
mkdir -p "$STAGE/control"

cat > "$STAGE/control/control" << CTRL
Package: tollgate-rs
Version: $VERSION
Architecture: $ARCH
Maintainer: TollGate <tollgate@tollgate.me>
Section: net
Priority: optional
Depends: nodogsplash, jq
Provides: tollgate-wrt
Conflicts: tollgate-wrt
Replaces: tollgate-wrt
Description: TollGate payment gateway for OpenWrt (Rust implementation).
 Powered by Cashu ecash and CDK (Cashu Dev Kit).
CTRL

# Postinst
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

# Preinst
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

IPK_NAME="tollgate-rs_${VERSION}_${ARCH}.ipk"
IPK_PATH="$OUTPUT_DIR/$IPK_NAME"

ar rD "$IPK_PATH" \
    "$STAGE/debian-binary" \
    "$STAGE/control.tar.gz" \
    "$STAGE/data.tar.gz" 2>&1

echo ""
echo "=== IPK BUILD COMPLETE ==="
ls -lh "$IPK_PATH"
echo ""
echo "Package: tollgate-rs v$VERSION ($ARCH)"
echo "Files: $(tar tzf "$STAGE/data.tar.gz" | wc -l)"
echo ""
echo "Install on OpenWrt:"
echo "  scp $IPK_PATH root@192.168.1.1:/tmp/"
echo "  ssh root@192.168.1.1 'opkg install /tmp/$IPK_NAME'"
