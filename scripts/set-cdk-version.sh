#!/usr/bin/env bash
# Switch the cdk dependency version for A/B testing across CDK releases.
#
#   ./scripts/set-cdk-version.sh 0.18.0     # switch + update lockfile
#   ./scripts/set-cdk-version.sh 0.17.6     # fall back to the 0.17 line
#   ./scripts/set-cdk-version.sh --current  # print the version in Cargo.toml
#
# Only the version numbers of cdk / cdk-sqlite are rewritten; features and
# flags stay untouched. Run `cargo test --lib` afterwards to compare against
# the other version's baseline.
set -euo pipefail

cd "$(dirname "$0")/.."

current() {
  grep -m1 '^cdk = ' Cargo.toml | grep -oE '"[0-9]+\.[0-9]+(\.[0-9]+)?"' | tr -d '"'
}

if [ "${1:-}" = "--current" ] || [ -z "${1:-}" ]; then
  echo "cdk version in Cargo.toml: $(current)"
  [ -n "${1:-}" ] || echo "usage: $0 <version|--current>"
  exit 0
fi

target="$1"
if ! echo "$target" | grep -qE '^[0-9]+\.[0-9]+(\.[0-9]+)?$'; then
  echo "invalid version: $target" >&2
  exit 1
fi

sed -i "s|^cdk = { version = \"[^\"]*\"|cdk = { version = \"$target\"|" Cargo.toml
sed -i "s|^cdk-sqlite = \"[^\"]*\"|cdk-sqlite = \"$target\"|" Cargo.toml

echo "switched to cdk $target; updating Cargo.lock..."
cargo update --quiet
echo "done: $(current)"
echo "A/B hint: run 'cargo test --lib' now and against the other version to compare."
