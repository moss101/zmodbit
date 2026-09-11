#!/usr/bin/env bash
# Phase 4.4 (item 4, split 1/2): deterministic UNPACKAGED-BINARY bundle
# assembly + SBOM + provenance. Produces unsigned/development artifacts:
# production signing is the OPERATOR gate (see operator-signing.md,
# BLOCKED_EXTERNAL_CREDENTIAL / RELEASE_GATE).
#
# Usage: tools/release/package.sh [--dev]
#   --dev  bundle the debug binaries (fast local verification; default is
#          --release). Output: dist/modbit-<version>-<target>/ + SHA256SUMS
#          + provenance.json.
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo "$(dirname "$0")/../..")"

PROFILE=release
[[ "${1:-}" == "--dev" ]] && PROFILE=debug
TARGET="$(rustc -vV | awk '/host:/ {print $2}')"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
BIN_DIR="target/$PROFILE"
OUT="dist/modbit-$VERSION-$TARGET"
REPO_REV="$(git rev-parse HEAD)"

echo "== build (profile: $PROFILE) =="
if [ "$PROFILE" = "release" ]; then
  cargo build --release -p modbit-core-runtime --bin modbit --bin modbit-core
  cargo build --release -p modbit-execd
else
  cargo build -p modbit-core-runtime --bin modbit --bin modbit-core
  cargo build -p modbit-execd
fi

echo "== assemble bundle: $OUT =="
rm -rf "$OUT"
mkdir -p "$OUT/bin"
for bin in modbit modbit-core modbit-execd; do
  cp "$BIN_DIR/$bin" "$OUT/bin/$bin"
done

echo "== SBOM (Cargo.lock-derived, deterministic) =="
SBOM="$OUT/modbit-sbom.cyclonedx.json.gz"
if command -v cargo-cyclonedx >/dev/null 2>&1; then
  cargo cyclonedx --all-features --format json >/dev/null 2>&1
  find crates -name '*.cdx.json' -exec cp {} "$OUT/modbit-sbom.cyclonedx.json" \; 2>/dev/null || true
  rm -f "$SBOM"
else
  # Fallback: deterministic SBOM from Cargo.lock (name + version pairs),
  # generated with awk only (no jq/head -n -1 portability issues).
  awk '
    /^name = /    { n = $0; sub(/^name = /, "", n); gsub(/"/, "", n) }
    /^version = / { v = $0; sub(/^version = /, "", v); gsub(/"/, "", v) }
    /^$/ {
      if (n != "" && v != "") {
        sep = (count++ > 0) ? "," : ""
        printf "%s{\"name\":\"%s\",\"version\":\"%s\"}", sep, n, v
      }
      n = ""; v = ""
    }
    END { printf "\n" }
  ' Cargo.lock > /tmp/sbom-components.json
  {
    printf '{"bomFormat":"CycloneDX","specVersion":"1.4","components":['
    cat /tmp/sbom-components.json
    printf ']}'
  } > "$OUT/modbit-sbom.fallback.json"
fi

echo "== provenance =="
RUSTC_V="$(rustc -vV | awk '/commit-hash:/ {print $2}')"
cat > "$OUT/provenance.json" <<EOF2
{
  "product": "modbit",
  "version": "$VERSION",
  "git_rev": "$REPO_REV",
  "profile": "$PROFILE",
  "target": "$TARGET",
  "rustc_commit": "$RUSTC_V",
  "built_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "signed": false,
  "signing_state": "BLOCKED_EXTERNAL_CREDENTIAL / RELEASE_GATE"
}
EOF2

echo "== hashes =="
( cd "$OUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 shasum -a 256 > SHA256SUMS )
echo "bundle ready: $OUT"
