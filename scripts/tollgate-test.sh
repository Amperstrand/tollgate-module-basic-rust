#!/usr/bin/env bash
# tollgate-test.sh — Start tollgate-rs for local testing.
# Kills any existing process on port 2121 first.
#
# Usage:
#   ./scripts/tollgate-test.sh [config-dir]
#
# If no config-dir provided, creates a temp dir with testnut config.

set -euo pipefail

PORT=2121
BINARY="${TOLLGATE_BINARY:-$(dirname "$0")/../target/release/tollgate-module-basic-rust}"

# Kill anything on the port
if fuser "$PORT/tcp" &>/dev/null; then
    PID=$(sudo ss -tlnp sport = :$PORT 2>/dev/null | grep -oP 'pid=\K[0-9]+' || true)
    echo "⚠️  Port $PORT in use by PID ${PID:-unknown}, killing..."
    sudo kill -9 "$PID" 2>/dev/null || fuser -k "$PORT/tcp" 2>/dev/null || true
    sleep 2
fi

# Setup config dir
CONFIG_DIR="${1:-$(mktemp -d)}"
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    cat > "$CONFIG_DIR/config.json" << 'EOF'
{"config_version":"v0.0.8","log_level":"info","metric":"milliseconds","step_size":5000,"margin":0.1,"accepted_mints":[{"url":"https://testnut.cashu.exchange","min_balance":0,"balance_tolerance_percent":100,"payout_interval_seconds":999999,"min_payout_amount":999999,"price_per_step":1,"price_unit":"sats","purchase_min_steps":0}],"profit_share":[{"factor":1.0,"identity":"owner"}],"show_setup":false,"reseller_mode":false}
EOF
fi

# Ensure dhcp.leases is writable for MAC resolution
if [ ! -f /tmp/dhcp.leases ]; then
    echo "9999999999 02:00:00:00:00:01 10.0.0.42 test-client *" > /tmp/dhcp.leases 2>/dev/null || \
    sudo bash -c 'echo "9999999999 02:00:00:00:00:01 10.0.0.42 test-client *" > /tmp/dhcp.leases && chmod 644 /tmp/dhcp.leases'
fi

echo "🚀 Starting tollgate-rs"
echo "   Binary: $BINARY"
echo "   Config: $CONFIG_DIR"
echo "   Port:   $PORT"
echo "   Test payment: curl -X POST http://127.0.0.1:$PORT/ -H 'Content-Type: text/plain' -H 'X-Forwarded-For: 10.0.0.42' -d '<cashu-token>'"
echo ""

exec env RUST_LOG="${RUST_LOG:-info}" TOLLGATE_TEST_CONFIG_DIR="$CONFIG_DIR" "$BINARY"
