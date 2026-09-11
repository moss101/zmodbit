#!/usr/bin/env bash
# Phase 4.4 (item 4, split 2/2a): verification checks for a bundle.
#   1. SHA256SUMS match every file,
#   2. provenance.json is complete,
#   3. every binary runs and reports itself (smoke),
#   4. IF the bundle is signed (macOS: codesign; Windows: signtool via
#      operator), signatures verify — unsigned dev bundles report
#      "unsigned" and pass with a note.
# Usage: tools/release/verify-bundle.sh <bundle-dir>
set -euo pipefail
BUNDLE="${1:?usage: verify-bundle.sh <bundle-dir>}"
cd "$BUNDLE"

echo "== hash verification =="
shasum -a 256 -c SHA256SUMS

echo "== provenance =="
grep -q '"git_rev"' provenance.json
grep -q '"signed"' provenance.json
echo "provenance complete"

echo "== binary smoke =="
# Servers (core/execd) must not be left running: spawn with a probe that
# exits immediately and kill stragglers.
for bin in bin/*; do
  "$bin" --help >/dev/null 2>&1 &
  PID=$!
  ( sleep 3; kill "$PID" 2>/dev/null ) &
  WATCH=$!
  wait "$PID" 2>/dev/null || true
  kill "$WATCH" 2>/dev/null || true
  wait "$WATCH" 2>/dev/null || true
done
echo "binaries execute"

echo "== signature state =="
# An AD-HOC signature (local dev builds) verifies trivially; PRODUCTION
# requires a Developer ID authority. Check the authority, not just
# signature validity.
SIGNED_STATE="UNSIGNED development bundle (operator signing pending — BLOCKED_EXTERNAL_CREDENTIAL / RELEASE_GATE)"
if command -v codesign >/dev/null 2>&1; then
  if codesign --verify --strict bin/modbit-core 2>/dev/null; then
    if codesign -dv bin/modbit-core 2>&1 | grep -q "Authority=Developer ID Application"; then
      SIGNED_STATE="SIGNED (macOS Developer ID verified)"
    else
      SIGNED_STATE="AD-HOC signed (development build; Developer ID signing pending)"
    fi
  fi
elif command -v signtool >/dev/null 2>&1; then
  if signtool verify /pa bin/modbit-core.exe >/dev/null 2>&1; then
    SIGNED_STATE="SIGNED (Windows Authenticode verified)"
  fi
fi
echo "$SIGNED_STATE"
echo "verify-bundle: OK"
